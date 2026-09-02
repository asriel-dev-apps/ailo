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
    if body.starts_with('.') {
        format!("${body}")
    } else {
        format!("$.{body}")
    }
}

pub fn pick(value: &Value, expr: &str) -> Result<Vec<Value>> {
    let normalized = normalize(expr);
    let path = JsonPath::parse(&normalized).with_context(|| {
        format!("`{expr}` を式として解釈できません(JSONPath: `{normalized}`)")
    })?;
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
