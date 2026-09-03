//! リクエストの組み立てと送信。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use reqwest::{Method, Url};
use serde_json::{Map, Value};

use crate::args::Item;
use crate::dump::{BodyRecord, RequestRecord, ResponseRecord};

/// ボディの送り方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyMode {
    /// 既定。フィールドを JSON オブジェクトにまとめる。
    Json,
    /// `--form`。`application/x-www-form-urlencoded`。
    Form,
    /// ファイルフィールドがあるとき。`multipart/form-data`。
    Multipart,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub method: Method,
    pub url: Url,
    pub headers: Vec<(String, String)>,
    pub mode: BodyMode,
    pub fields: Vec<(String, Value)>,
    pub files: Vec<(String, PathBuf)>,
    /// `--raw` で本文を直接指定した場合。フィールドより優先する。
    pub raw: Option<String>,
}

/// item 列と URL から送信計画を組む。
pub fn plan(
    method: Method,
    url: &str,
    items: &[Item],
    force_form: bool,
    raw: Option<String>,
) -> Result<Plan> {
    let mut parsed = Url::parse(url)
        .with_context(|| format!("URL として読めません: {url}。scheme(https://)は必要です"))?;

    let mut headers = Vec::new();
    let mut fields = Vec::new();
    let mut files = Vec::new();

    {
        let mut query = parsed.query_pairs_mut();
        for item in items {
            match item {
                Item::Query { name, value } => {
                    query.append_pair(name, value);
                }
                Item::Header { name, value } => {
                    // 同じ名前が既にあれば差し替える。設定の `[headers]` を
                    // コマンドラインで上書きしたいのが普通の意図なのに、そのまま
                    // 積むと**両方が送られる**。受け取り側の挙動はサーバ次第で、
                    // どちらが効いたのか分からないまま話が進む。
                    headers.retain(|(n, _): &(String, String)| !n.eq_ignore_ascii_case(name));
                    headers.push((name.clone(), value.clone()));
                }
                Item::Field { name, value } => {
                    fields.push((name.clone(), Value::String(value.clone())))
                }
                Item::RawField { name, value } => fields.push((name.clone(), value.clone())),
                Item::FileField { name, path } => files.push((name.clone(), path.clone())),
            }
        }
    }
    // append_pair は query が空でも `?` を残すことがあるので畳んでおく。
    if parsed.query() == Some("") {
        parsed.set_query(None);
    }

    let mode = if !files.is_empty() {
        multipart_mode(force_form)?
    } else if force_form {
        BodyMode::Form
    } else {
        BodyMode::Json
    };

    Ok(Plan {
        method,
        url: parsed,
        headers,
        mode,
        fields,
        files,
        raw,
    })
}

/// ファイルフィールドがあるときのモード決定。`--form` との併用は意味が定まらないので拒む。
fn multipart_mode(force_form: bool) -> Result<BodyMode> {
    if force_form {
        // どちらの意味にも取れるので、黙って片方を選ばない。
        bail!("`--form` とファイルフィールド(`key@path`)は同時に指定できません。ファイルを送るなら `--form` を外してください(multipart になります)");
    }
    Ok(BodyMode::Multipart)
}

fn fields_to_json(fields: &[(String, Value)]) -> Value {
    let mut map = Map::new();
    for (k, v) in fields {
        map.insert(k.clone(), v.clone());
    }
    Value::Object(map)
}

fn header_map(headers: &[(String, String)]) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let n = HeaderName::try_from(name.as_str())
            .with_context(|| format!("ヘッダ名として使えません: {name}"))?;
        let v = HeaderValue::from_str(value)
            .with_context(|| format!("ヘッダ `{name}` の値に使えない文字が含まれています"))?;
        map.append(n, v);
    }
    Ok(map)
}

pub struct Sent {
    pub request: RequestRecord,
    pub response: ResponseRecord,
}

/// https から http へ落とすリダイレクトを拒む。
///
/// reqwest は host か実効ポートが変われば `Authorization` を落とすが、
/// **scheme だけが変わる場合**は同一 origin とみなして落とさない。
/// `https://host:443/` → `http://host:443/` はホストもポートも同じなので、
/// 認証ヘッダが平文で流れる。既定のリダイレクト上限は保ったまま、これだけ止める。
fn no_downgrade_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        let previous_was_https = attempt
            .previous()
            .last()
            .is_some_and(|u| u.scheme() == "https");
        if previous_was_https && attempt.url().scheme() == "http" {
            return attempt.error("https から http へのリダイレクトを拒否しました");
        }
        if attempt.previous().len() >= 10 {
            return attempt.error("リダイレクトが 10 回を超えました");
        }
        attempt.follow()
    })
}

pub async fn send(plan: &Plan, timeout_secs: u64) -> Result<Sent> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .user_agent(concat!("ailo/", env!("CARGO_PKG_VERSION")))
        .redirect(no_downgrade_policy())
        .build()
        .context("HTTP クライアントを初期化できません")?;

    let mut req = client.request(plan.method.clone(), plan.url.clone());
    let mut recorded_body = BodyRecord::Empty;

    if let Some(raw) = &plan.raw {
        recorded_body = match serde_json::from_str::<Value>(raw) {
            Ok(v) => BodyRecord::Json { value: v },
            Err(_) => BodyRecord::Text { text: raw.clone() },
        };
        req = req.body(raw.clone());
    } else {
        match plan.mode {
            BodyMode::Json if !plan.fields.is_empty() => {
                let body = fields_to_json(&plan.fields);
                recorded_body = BodyRecord::Json {
                    value: body.clone(),
                };
                req = req.json(&body);
            }
            BodyMode::Form if !plan.fields.is_empty() => {
                let pairs: Vec<(String, String)> = plan
                    .fields
                    .iter()
                    .map(|(k, v)| (k.clone(), scalar_to_string(v)))
                    .collect();
                recorded_body = BodyRecord::Json {
                    value: fields_to_json(&plan.fields),
                };
                req = req.form(&pairs);
            }
            BodyMode::Multipart => {
                let mut form = reqwest::multipart::Form::new();
                for (k, v) in &plan.fields {
                    form = form.text(k.clone(), scalar_to_string(v));
                }
                for (k, path) in &plan.files {
                    let bytes = tokio::fs::read(path)
                        .await
                        .with_context(|| format!("{} を読めません", path.display()))?;
                    let filename = path
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| k.clone());
                    form = form.part(
                        k.clone(),
                        reqwest::multipart::Part::bytes(bytes).file_name(filename),
                    );
                }
                recorded_body = BodyRecord::Text {
                    text: format!(
                        "multipart: {} field(s), {} file(s)",
                        plan.fields.len(),
                        plan.files.len()
                    ),
                };
                req = req.multipart(form);
            }
            _ => {}
        }
    }

    req = req.headers(header_map(&plan.headers)?);

    let started = Instant::now();
    // エラー文には展開後の URL を載せない。クエリや userinfo に秘匿値が入っていると、
    // 失敗のたびに標準エラーへ出る。原因の切り分けに要るのは宛先だけ。
    let res = req.send().await.with_context(|| {
        format!(
            "{} へのリクエストが失敗しました",
            plan.url.host_str().unwrap_or("宛先")
        )
    })?;
    let status = res.status();

    let res_headers: BTreeMap<String, String> = res
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                v.to_str().unwrap_or("<非UTF-8>").to_string(),
            )
        })
        .collect();

    let content_type = res
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();

    let bytes = res.bytes().await.context("レスポンス本文を読めません")?;
    let ms = started.elapsed().as_millis() as u64;
    let len = bytes.len();

    let body = decode_body(&bytes, &content_type);

    let mut req_headers: BTreeMap<String, String> = plan
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
        .collect();
    if matches!(recorded_body, BodyRecord::Json { .. }) && plan.mode == BodyMode::Json {
        req_headers
            .entry("content-type".into())
            .or_insert_with(|| "application/json".into());
    }

    Ok(Sent {
        request: RequestRecord {
            method: plan.method.to_string(),
            url: plan.url.to_string(),
            headers: req_headers,
            body: recorded_body,
        },
        response: ResponseRecord {
            status: status.as_u16(),
            status_text: status.canonical_reason().unwrap_or("").to_string(),
            headers: res_headers,
            body,
            bytes: len,
            ms,
        },
    })
}

fn decode_body(bytes: &[u8], content_type: &str) -> BodyRecord {
    if bytes.is_empty() {
        return BodyRecord::Empty;
    }
    // Content-Type を信じきらない。JSON を名乗らないが JSON を返す API は珍しくない。
    if let Ok(text) = std::str::from_utf8(bytes) {
        if let Ok(value) = serde_json::from_str::<Value>(text) {
            return BodyRecord::Json { value };
        }
        return BodyRecord::Text {
            text: text.to_string(),
        };
    }
    let _ = content_type;
    BodyRecord::Binary { bytes: bytes.len() }
}

/// フォームや multipart のテキスト値。文字列はクォート無しで送る。
fn scalar_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::parse_item;

    fn items(raw: &[&str]) -> Vec<Item> {
        raw.iter().map(|s| parse_item(s).unwrap()).collect()
    }

    #[test]
    fn query_items_are_appended_to_the_url() {
        let p = plan(
            Method::GET,
            "https://example.com/users",
            &items(&["limit==50", "q==a b"]),
            false,
            None,
        )
        .unwrap();
        let q = p.url.query().unwrap();
        assert!(q.contains("limit=50"), "{q}");
        // 空白はエンコードされること。
        assert!(q.contains("q=a+b") || q.contains("q=a%20b"), "{q}");
    }

    #[test]
    fn a_url_without_query_items_keeps_no_trailing_question_mark() {
        let p = plan(Method::GET, "https://example.com/u", &[], false, None).unwrap();
        assert_eq!(p.url.as_str(), "https://example.com/u");
    }

    #[test]
    fn fields_become_a_json_object_by_default() {
        let p = plan(
            Method::POST,
            "https://example.com/u",
            &items(&["name=taro", "age:=30"]),
            false,
            None,
        )
        .unwrap();
        assert_eq!(p.mode, BodyMode::Json);
        let body = fields_to_json(&p.fields);
        assert_eq!(body["name"], "taro");
        assert_eq!(body["age"], 30);
    }

    #[test]
    fn a_file_field_switches_to_multipart() {
        let p = plan(
            Method::POST,
            "https://example.com/u",
            &items(&["avatar@./a.png"]),
            false,
            None,
        )
        .unwrap();
        assert_eq!(p.mode, BodyMode::Multipart);
    }

    #[test]
    fn form_and_file_fields_together_are_rejected_rather_than_guessed() {
        let err = plan(
            Method::POST,
            "https://example.com/u",
            &items(&["avatar@./a.png"]),
            true,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("--form"), "{err}");
    }

    #[test]
    fn a_url_without_a_scheme_is_rejected_with_a_usable_message() {
        let err = plan(Method::GET, "example.com", &[], false, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("scheme"), "{err}");
    }

    #[test]
    fn json_response_is_decoded_even_without_a_json_content_type() {
        let b = decode_body(br#"{"a":1}"#, "text/plain");
        assert!(matches!(b, BodyRecord::Json { .. }));
    }

    #[test]
    fn non_utf8_response_is_recorded_by_size_only() {
        let b = decode_body(&[0xff, 0xfe, 0x00], "application/octet-stream");
        assert_eq!(b, BodyRecord::Binary { bytes: 3 });
    }

    #[test]
    fn empty_response_is_distinguishable_from_an_empty_string() {
        assert_eq!(decode_body(b"", "application/json"), BodyRecord::Empty);
    }

    #[test]
    fn a_later_header_replaces_an_earlier_one_of_the_same_name() {
        // 設定の [headers] を後ろのコマンドライン指定で上書きするのが普通の意図。
        // 積むと両方送られ、どちらが効いたのか分からないまま話が進む。
        let p = plan(
            Method::GET,
            "https://example.com/u",
            &items(&["Accept: application/json", "Accept: text/csv"]),
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            p.headers,
            vec![("Accept".to_string(), "text/csv".to_string())]
        );
    }

    #[test]
    fn header_replacement_ignores_case_in_the_name() {
        let p = plan(
            Method::GET,
            "https://example.com/u",
            &items(&["accept: application/json", "Accept: text/csv"]),
            false,
            None,
        )
        .unwrap();
        assert_eq!(p.headers.len(), 1, "{:?}", p.headers);
        assert_eq!(p.headers[0].1, "text/csv");
    }

    #[test]
    fn different_header_names_are_all_kept() {
        let p = plan(
            Method::GET,
            "https://example.com/u",
            &items(&["Accept: application/json", "X-Trace: abc"]),
            false,
            None,
        )
        .unwrap();
        assert_eq!(p.headers.len(), 2);
    }

    #[test]
    fn header_names_are_validated_before_sending() {
        assert!(header_map(&[("Bad Header".into(), "v".into())]).is_err());
        assert!(header_map(&[("X-Ok".into(), "v".into())]).is_ok());
    }
}
