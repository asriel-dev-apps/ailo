//! レスポンスから変数を取り出して束縛する。
//!
//! Postman の「ログイン → token を環境変数に入れる → 以降のヘッダに自動で乗る」に相当する。
//! スクリプトは持ち込まない。抽出の仕組みは `--pick` と同じ JSONPath をそのまま使う。
//!
//! ```toml
//! [requests.login]
//! capture = { access_token = ".data.token", expires_in = ".data.expiresIn" }
//! secret  = ["access_token"]
//! ```

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// 有効期限として特別扱いする変数名。
///
/// `expires_in` は秒数、`expires_at` は RFC3339 の絶対時刻として読む。
/// どちらも、その名前を持つリクエストが `secret` に挙げた変数すべての期限になる。
/// ログインが token 1 本と期限 1 つを返す形が圧倒的に多いため、対応付けは持たない。
pub const EXPIRES_IN: &str = "expires_in";
pub const EXPIRES_AT: &str = "expires_at";

/// 期限に対して手前に取るマージン。
///
/// ちょうど期限までを有効と見なすと、送信中に切れて 401 を受ける。
const SKEW_SECONDS: i64 = 30;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Captured {
    /// 平文で state に保存してよい値。
    pub vars: BTreeMap<String, String>,
    /// キーチェーンへ入れる値。
    pub secrets: BTreeMap<String, String>,
    /// 変数名 → RFC3339 の失効時刻。
    pub expires_at: BTreeMap<String, String>,
}

impl Captured {
    pub fn is_empty(&self) -> bool {
        self.vars.is_empty() && self.secrets.is_empty()
    }

    /// マスクに登録すべき値。キャプチャした秘匿値が以降のダンプに素通りしないように。
    pub fn secret_values(&self) -> Vec<&str> {
        self.secrets.values().map(String::as_str).collect()
    }
}

/// 抽出結果を 1 つの文字列にする。複数ヒットは変数として扱えないので拒む。
fn single(name: &str, found: &[Value]) -> Result<String> {
    match found {
        [] => bail!("`{name}` の抽出式が何にも一致しませんでした"),
        [one] => Ok(match one {
            Value::String(s) => s.clone(),
            Value::Null => bail!("`{name}` の抽出結果が null です"),
            other => other.to_string(),
        }),
        many => bail!(
            "`{name}` の抽出式が {} 件に一致しました。変数には 1 件だけを指定してください",
            many.len()
        ),
    }
}

pub fn capture(
    body: &Value,
    spec: &BTreeMap<String, String>,
    secret_names: &[String],
    now: OffsetDateTime,
) -> Result<Captured> {
    let mut out = Captured::default();
    let mut expiry: Option<OffsetDateTime> = None;

    for (name, expr) in spec {
        let found = crate::pick::pick(body, expr)?;
        let value = single(name, &found)?;

        if name == EXPIRES_IN {
            let secs: i64 = value.trim().parse().map_err(|_| {
                anyhow::anyhow!("`{EXPIRES_IN}` は秒数のはずですが `{value}` でした")
            })?;
            expiry = Some(now + time::Duration::seconds(secs - SKEW_SECONDS));
            continue;
        }
        if name == EXPIRES_AT {
            let at = OffsetDateTime::parse(value.trim(), &Rfc3339).map_err(|_| {
                anyhow::anyhow!("`{EXPIRES_AT}` は RFC3339 のはずですが `{value}` でした")
            })?;
            expiry = Some(at - time::Duration::seconds(SKEW_SECONDS));
            continue;
        }

        if secret_names.iter().any(|s| s == name) {
            out.secrets.insert(name.clone(), value);
        } else {
            out.vars.insert(name.clone(), value);
        }
    }

    if let Some(at) = expiry {
        let stamp = at.format(&Rfc3339)?;
        // 期限は秘匿値に付ける。平文の値は期限で無効にする意味がない。
        for name in out.secrets.keys() {
            out.expires_at.insert(name.clone(), stamp.clone());
        }
        // secret 指定が無ければ、捕まえた変数すべてに付ける。
        if out.secrets.is_empty() {
            for name in out.vars.keys() {
                out.expires_at.insert(name.clone(), stamp.clone());
            }
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
    }

    fn spec(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_secret_goes_to_the_keychain_side_and_a_plain_value_does_not() {
        let body = json!({"data": {"token": "abc", "userId": 7}});
        let got = capture(
            &body,
            &spec(&[("access_token", ".data.token"), ("user_id", ".data.userId")]),
            &["access_token".into()],
            now(),
        )
        .unwrap();
        assert_eq!(got.secrets["access_token"], "abc");
        assert_eq!(got.vars["user_id"], "7");
        assert!(
            !got.vars.contains_key("access_token"),
            "秘匿値が平文側にいる"
        );
    }

    #[test]
    fn expires_in_becomes_an_absolute_time_on_the_secret() {
        let body = json!({"token": "abc", "expiresIn": 3600});
        let got = capture(
            &body,
            &spec(&[("access_token", ".token"), ("expires_in", ".expiresIn")]),
            &["access_token".into()],
            now(),
        )
        .unwrap();
        let at = OffsetDateTime::parse(&got.expires_at["access_token"], &Rfc3339).unwrap();
        // 手前にマージンを取ること。ちょうど期限だと送信中に切れる。
        assert_eq!(at, now() + time::Duration::seconds(3600 - SKEW_SECONDS));
        // 期限そのものは変数として残さない。
        assert!(!got.vars.contains_key("expires_in"));
        assert!(!got.secrets.contains_key("expires_in"));
    }

    #[test]
    fn an_absolute_expiry_is_accepted_too() {
        let body = json!({"token": "abc", "exp": "2030-01-01T00:00:00Z"});
        let got = capture(
            &body,
            &spec(&[("access_token", ".token"), ("expires_at", ".exp")]),
            &["access_token".into()],
            now(),
        )
        .unwrap();
        assert!(got.expires_at["access_token"].starts_with("2029-12-31T23:59:30"));
    }

    #[test]
    fn a_non_numeric_expires_in_is_an_error_naming_the_value() {
        let body = json!({"token": "a", "e": "soon"});
        let err = capture(
            &body,
            &spec(&[("access_token", ".token"), ("expires_in", ".e")]),
            &[],
            now(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("soon"), "{err}");
    }

    #[test]
    fn an_expression_matching_nothing_is_an_error_rather_than_an_empty_binding() {
        // 空文字を束縛すると、次のリクエストが理由不明の 401 で落ちる。
        let err = capture(&json!({}), &spec(&[("t", ".nope")]), &[], now())
            .unwrap_err()
            .to_string();
        assert!(err.contains('t'), "{err}");
    }

    #[test]
    fn a_null_value_is_an_error_rather_than_the_string_null() {
        let err = capture(&json!({"t": null}), &spec(&[("t", ".t")]), &[], now())
            .unwrap_err()
            .to_string();
        assert!(err.contains("null"), "{err}");
    }

    #[test]
    fn matching_several_values_is_rejected_rather_than_picking_one() {
        let body = json!({"items": [{"id": "1"}, {"id": "2"}]});
        let err = capture(&body, &spec(&[("id", ".items[].id")]), &[], now())
            .unwrap_err()
            .to_string();
        assert!(err.contains('2'), "件数を伝えていない: {err}");
    }

    #[test]
    fn captured_secret_values_are_offered_for_masking() {
        let body = json!({"token": "s3cr3t-token-value"});
        let got = capture(&body, &spec(&[("t", ".token")]), &["t".into()], now()).unwrap();
        assert_eq!(got.secret_values(), vec!["s3cr3t-token-value"]);
    }

    #[test]
    fn nothing_captured_is_reported_as_empty() {
        assert!(capture(&json!({}), &BTreeMap::new(), &[], now())
            .unwrap()
            .is_empty());
    }
}
