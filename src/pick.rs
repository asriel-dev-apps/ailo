//! レスポンスから必要なフィールドだけを取り出す。
//!
//! 本文がコンテキストに一度も入らないようにするための道具。
//! 式は JSONPath(RFC 9535)。加えて、手に馴染んだ jq 風の書き方も受ける。
//!
//! ```text
//! --pick '.data.items[].id'     # jq 風。内部で $.data.items[*].id に直す
//! --pick '$.data.items[*].id'   # JSONPath をそのまま
//! ```

use anyhow::{Context, Result};
use serde_json::Value;
use serde_json_path::JsonPath;

/// jq 風の式を JSONPath に寄せる。すでに JSONPath なら触らない。
fn normalize(expr: &str) -> String {
    let trimmed = expr.trim();
    if trimmed.starts_with('$') {
        return trimmed.to_string();
    }
    // `.items[]` は「配列を展開する」の意。JSONPath では `[*]`。
    let body = trimmed.replace("[]", "[*]");
    // jq は添字の前にも `.` を書ける(`.[]`、`.[0]`、`.items.[0]`)が、JSONPath では
    // `.` と `[` を続けられない。これを許さないと、**トップレベルが配列のレスポンス**
    // (一覧系の API はたいていこれ)に対して `.[].title` が書けなくなる。
    let body = body.replace(".[", "[");
    format!("${}", quote_awkward_names(&body))
}

/// ドット記法で書けない名前を `['...']` に直す。
///
/// JSONPath のドット記法は英数字と `_` しか使えない。`.headers.X-Tenant` や
/// `.data.user-id` はそのままでは構文エラーになる。**HTTP クライアントなのに
/// ヘッダ名が引けない**のは実用にならないので、ここで包み直す。
fn quote_awkward_names(body: &str) -> String {
    let is_plain_name = |s: &str| {
        !s.is_empty()
            && !s.starts_with(|c: char| c.is_ascii_digit())
            && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };

    let chars: Vec<char> = body.chars().collect();
    let mut out = String::with_capacity(body.len() + 8);
    let mut i = 0usize;

    while i < chars.len() {
        match chars[i] {
            // 角括弧の中は JSONPath の構文そのものなので触らない。
            '[' => {
                let start = i;
                let mut depth = 0usize;
                while i < chars.len() {
                    if chars[i] == '[' {
                        depth += 1;
                    } else if chars[i] == ']' {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    i += 1;
                }
                out.extend(&chars[start..i]);
            }
            _ => {
                if chars[i] == '.' {
                    i += 1;
                }
                let start = i;
                while i < chars.len() && chars[i] != '.' && chars[i] != '[' {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                if name.is_empty() {
                    continue;
                }
                if is_plain_name(&name) {
                    out.push('.');
                    out.push_str(&name);
                } else {
                    out.push_str(&format!("['{}']", name.replace('\'', "\\'")));
                }
            }
        }
    }
    out
}

pub fn pick(value: &Value, expr: &str) -> Result<Vec<Value>> {
    let normalized = normalize(expr);
    let path = JsonPath::parse(&normalized)
        .with_context(|| format!("`{expr}` を式として解釈できません(JSONPath: `{normalized}`)"))?;
    Ok(path.query(value).into_iter().cloned().collect())
}

/// 抽出結果を出力用の文字列にする。
///
/// 文字列はクォートを外して出す。`--pick '.token'` の結果をそのままシェル変数に
/// 入れられるようにするため。それ以外は JSON のまま 1 件 1 行。
pub fn render(results: &[Value]) -> String {
    results
        .iter()
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 式ひとつぶんの期待。**正規化後の JSONPath と結果を対で書く。**
    ///
    /// 結果だけを見ていると、たまたま同じ値に行き着く別解釈を見逃す。
    /// 逆に JSONPath だけを見ていると、正規化は正しいのに問い合わせが空という
    /// 組み合わせを見逃す。両方を並べて初めて「解釈」を固定できる。
    struct Case {
        /// 何を模しているか。落ちたときに読む。
        about: &'static str,
        expr: &'static str,
        /// `normalize` が返すべき JSONPath。
        path: &'static str,
        /// 取り出せるべき値。
        want: Vec<Value>,
    }

    /// 実物に寄せた入力。
    ///
    /// 以前の fixture は**オブジェクト始まり・英数字だけのキー**しか持っておらず、
    /// 一覧系 API(トップレベルが配列)もヘッダ名(ハイフン)も表現できていなかった。
    /// 実使用で最初に当たった 2 つがどちらもここだったので、形のほうを直す。
    fn object_doc() -> Value {
        json!({
            "data": {
                "token": "abc",
                "items": [{"id": "1", "n": 1}, {"id": "2", "n": 2}]
            },
            "headers": {"X-Tenant": "acme", "Content-Type": "application/json"},
            "2fa": {"enabled": true},
            "a": {"b-c": [{"d-e": 1}, {"d-e": 2}]}
        })
    }

    fn array_doc() -> Value {
        json!([{"title": "a", "author-name": "taro"}, {"title": "b", "author-name": "hanako"}])
    }

    fn object_cases() -> Vec<Case> {
        vec![
            Case {
                about: "jq 風のドット記法",
                expr: ".data.token",
                path: "$.data.token",
                want: vec![json!("abc")],
            },
            Case {
                about: "先頭の `.` は省ける",
                expr: "data.token",
                path: "$.data.token",
                want: vec![json!("abc")],
            },
            Case {
                about: "JSONPath はそのまま通す",
                expr: "$.data.items[*].id",
                path: "$.data.items[*].id",
                want: vec![json!("1"), json!("2")],
            },
            Case {
                about: "jq 風の配列展開",
                expr: ".data.items[].id",
                path: "$.data.items[*].id",
                want: vec![json!("1"), json!("2")],
            },
            Case {
                about: "jq は添字の前にも `.` を書ける",
                expr: ".data.items.[0].id",
                path: "$.data.items[0].id",
                want: vec![json!("1")],
            },
            Case {
                about: "ハイフンを含むキー(ヘッダ名で必ず出る)",
                expr: ".headers.X-Tenant",
                path: "$.headers['X-Tenant']",
                want: vec![json!("acme")],
            },
            Case {
                about: "ハイフンを含むキーが 2 つ続く",
                expr: ".headers.Content-Type",
                path: "$.headers['Content-Type']",
                want: vec![json!("application/json")],
            },
            Case {
                about: "ハイフンと配列展開の組み合わせ",
                expr: ".a.b-c[].d-e",
                path: "$.a['b-c'][*]['d-e']",
                want: vec![json!(1), json!(2)],
            },
            Case {
                about: "数字で始まるキー",
                expr: ".2fa.enabled",
                path: "$['2fa'].enabled",
                want: vec![json!(true)],
            },
            Case {
                about: "無い経路は空。エラーにしない(存在確認に使うため)",
                expr: ".data.nope",
                path: "$.data.nope",
                want: vec![],
            },
        ]
    }

    fn array_cases() -> Vec<Case> {
        vec![
            Case {
                about: "トップレベルが配列。一覧系 API はこの形",
                expr: ".[].title",
                path: "$[*].title",
                want: vec![json!("a"), json!("b")],
            },
            Case {
                about: "トップレベル配列の添字",
                expr: ".[0].title",
                path: "$[0].title",
                want: vec![json!("a")],
            },
            Case {
                about: "`.` を省いた書き方も受ける",
                expr: "[1].title",
                path: "$[1].title",
                want: vec![json!("b")],
            },
            Case {
                about: "トップレベル配列 × ハイフン",
                expr: ".[].author-name",
                path: "$[*]['author-name']",
                want: vec![json!("taro"), json!("hanako")],
            },
        ]
    }

    fn check(cases: Vec<Case>, doc: &Value) {
        for c in cases {
            assert_eq!(
                normalize(c.expr),
                c.path,
                "{}: `{}` の正規化がずれている",
                c.about,
                c.expr
            );
            assert_eq!(
                pick(doc, c.expr).unwrap(),
                c.want,
                "{}: `{}` の結果がずれている",
                c.about,
                c.expr
            );
        }
    }

    #[test]
    fn expressions_over_an_object_document() {
        check(object_cases(), &object_doc());
    }

    #[test]
    fn expressions_over_a_top_level_array() {
        check(array_cases(), &array_doc());
    }

    #[test]
    fn plain_names_keep_dot_notation() {
        // 包み直すのは必要なときだけ。無条件に括ると式が読みにくくなる。
        assert_eq!(normalize(".data.items[*].id"), "$.data.items[*].id");
    }

    #[test]
    fn a_malformed_expression_is_an_error_naming_the_input() {
        let err = pick(&object_doc(), ".data[").unwrap_err().to_string();
        assert!(err.contains(".data["), "入力を示していない: {err}");
    }

    #[test]
    fn strings_render_without_quotes_so_they_can_be_piped() {
        assert_eq!(render(&[json!("abc")]), "abc");
    }

    #[test]
    fn non_strings_render_as_json_one_per_line() {
        assert_eq!(render(&[json!(1), json!({"a": 1})]), "1\n{\"a\":1}");
    }
}
