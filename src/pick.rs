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

    fn doc() -> Value {
        json!({
            "data": {
                "token": "abc",
                "items": [{"id": "1", "n": 1}, {"id": "2", "n": 2}]
            }
        })
    }

    #[test]
    fn jq_style_path_works() {
        assert_eq!(pick(&doc(), ".data.token").unwrap(), vec![json!("abc")]);
    }

    #[test]
    fn jq_style_array_expansion_works() {
        assert_eq!(
            pick(&doc(), ".data.items[].id").unwrap(),
            vec![json!("1"), json!("2")]
        );
    }

    #[test]
    fn a_top_level_array_can_be_expanded() {
        // 一覧系の API はトップレベルが配列。`.[]` が書けないと使い物にならない。
        let list = json!([{"title": "a"}, {"title": "b"}]);
        assert_eq!(
            pick(&list, ".[].title").unwrap(),
            vec![json!("a"), json!("b")]
        );
    }

    #[test]
    fn a_top_level_array_can_be_indexed() {
        let list = json!([{"title": "a"}, {"title": "b"}]);
        assert_eq!(pick(&list, ".[0].title").unwrap(), vec![json!("a")]);
        // `.` を省いた jq 以外の書き方も受ける。
        assert_eq!(pick(&list, "[1].title").unwrap(), vec![json!("b")]);
    }

    #[test]
    fn a_dot_before_an_index_is_accepted_mid_path() {
        // jq は `.data.items.[0]` とも書ける。JSONPath は `.` と `[` を続けられない。
        assert_eq!(
            pick(&doc(), ".data.items.[0].id").unwrap(),
            vec![json!("1")]
        );
    }

    #[test]
    fn a_key_with_a_hyphen_can_be_reached() {
        // HTTP クライアントなのでヘッダ名を引く場面が多い。ハイフンは普通に出る。
        let v = json!({"headers": {"X-Tenant": "acme", "Content-Type": "application/json"}});
        assert_eq!(pick(&v, ".headers.X-Tenant").unwrap(), vec![json!("acme")]);
        assert_eq!(
            pick(&v, ".headers.Content-Type").unwrap(),
            vec![json!("application/json")]
        );
    }

    #[test]
    fn keys_needing_quotes_work_inside_longer_paths() {
        let v = json!({"a": {"b-c": [{"d-e": 1}, {"d-e": 2}]}});
        assert_eq!(pick(&v, ".a.b-c[].d-e").unwrap(), vec![json!(1), json!(2)]);
    }

    #[test]
    fn a_key_that_starts_with_a_digit_is_reachable() {
        let v = json!({"2fa": {"enabled": true}});
        assert_eq!(pick(&v, ".2fa.enabled").unwrap(), vec![json!(true)]);
    }

    #[test]
    fn plain_names_keep_dot_notation() {
        // 包み直すのは必要なときだけ。無条件に括ると式が読みにくくなる。
        assert_eq!(normalize(".data.items[*].id"), "$.data.items[*].id");
    }

    #[test]
    fn jsonpath_is_accepted_as_is() {
        assert_eq!(
            pick(&doc(), "$.data.items[*].id").unwrap(),
            vec![json!("1"), json!("2")]
        );
    }

    #[test]
    fn leading_dot_is_optional() {
        assert_eq!(pick(&doc(), "data.token").unwrap(), vec![json!("abc")]);
    }

    #[test]
    fn a_missing_path_yields_nothing_rather_than_an_error() {
        // 「無い」はエラーではない。あるかどうかを調べる用途があるため。
        assert!(pick(&doc(), ".data.nope").unwrap().is_empty());
    }

    #[test]
    fn a_malformed_expression_is_an_error_naming_the_input() {
        let err = pick(&doc(), ".data[").unwrap_err().to_string();
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
