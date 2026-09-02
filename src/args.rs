//! httpie 風の引数記法を解釈する。
//!
//! ```text
//! ailo post /users name=taro age:=30 limit==50 'X-Trace: abc' avatar@./a.png
//! ```
//!
//! | 記法        | 意味                                   |
//! | ----------- | -------------------------------------- |
//! | `key=value` | JSON ボディの文字列フィールド          |
//! | `key:=json` | JSON ボディの raw 値(数値/bool/配列/…) |
//! | `key==v`    | クエリパラメータ                       |
//! | `Name: v`   | ヘッダ                                 |
//! | `key@path`  | multipart のファイル                   |
//!
//! 区切り文字そのものを値に入れたいときは `\` で escape する(`key=a\=b`)。

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Header {
        name: String,
        value: String,
    },
    Query {
        name: String,
        value: String,
    },
    Field {
        name: String,
        value: String,
    },
    RawField {
        name: String,
        value: serde_json::Value,
    },
    FileField {
        name: String,
        path: PathBuf,
    },
}

/// 長いものから順に見る。`:=` を `:` より先に、`==` を `=` より先に判定する必要がある。
const SEPARATORS: &[&str] = &[":=", "==", "=", "@", ":"];

/// escape を解いた文字列と、最初に現れた区切りの (位置, 種類) を返す。
///
/// escape 中の区切り文字は区切りとして数えない。
fn split_on_separator(input: &str) -> Option<(String, &'static str, String)> {
    let bytes: Vec<char> = input.chars().collect();
    let mut head = String::new();
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == '\\' && i + 1 < bytes.len() && is_escapable(bytes[i + 1]) {
            head.push(bytes[i + 1]);
            i += 2;
            continue;
        }
        let rest: String = bytes[i..].iter().collect();
        if let Some(sep) = SEPARATORS.iter().find(|s| rest.starts_with(**s)) {
            let tail: String = bytes[i + sep.chars().count()..].iter().collect();
            return Some((head, sep, unescape(&tail)));
        }
        head.push(bytes[i]);
        i += 1;
    }
    None
}

/// `\` が escape として働く相手。
///
/// 何でも escape してしまうと、`file@C:\tmp\auth.txt` が `C:tmpauth.txt` になり、
/// **指定したのと違うファイルを送る**。escape の目的は区切り文字を値に入れることなので、
/// 相手は区切り文字と `\` 自身だけでよい。
fn is_escapable(c: char) -> bool {
    matches!(c, ':' | '=' | '@' | '\\')
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                if is_escapable(next) {
                    out.push(next);
                    chars.next();
                    continue;
                }
            }
        }
        out.push(c);
    }
    out
}

pub fn parse_item(input: &str) -> Result<Item> {
    let Some((name, sep, value)) = split_on_separator(input) else {
        bail!(
            "`{input}` を解釈できません。`key=value` / `key:=json` / `key==query` / `Name: value` / `key@path` のいずれかで指定してください"
        );
    };
    if name.is_empty() {
        bail!("`{input}` のキーが空です");
    }

    Ok(match sep {
        ":=" => {
            let parsed = serde_json::from_str(value.trim()).with_context(|| {
                format!("`{name}:=` の値が JSON として読めません: {value}。文字列を渡すなら `{name}=` を使ってください")
            })?;
            Item::RawField {
                name,
                value: parsed,
            }
        }
        "==" => Item::Query { name, value },
        "=" => Item::Field { name, value },
        "@" => Item::FileField {
            name,
            path: PathBuf::from(value),
        },
        ":" => {
            // `filter:status==open` は「`filter` というヘッダ」とも
            // 「`filter:status` というクエリ」とも読める。黙ってどちらかに決めると、
            // 意図と違うリクエストがエラーも出さずに飛ぶ。曖昧なら落とす。
            if value.contains("==") || value.contains(":=") {
                bail!(
                    "`{input}` はヘッダともクエリとも読めます。ヘッダなら `\\:` で区切りを escape し、キーに `:` を含むクエリなら `{}` のように書いてください",
                    input.replacen(':', "\\:", 1)
                );
            }
            Item::Header {
                name,
                // `Name: value` と `Name:value` の両方を受ける。
                value: value.trim_start().to_string(),
            }
        }
        _ => unreachable!("SEPARATORS に無い区切りが返された"),
    })
}

/// 引数列がそもそも item 記法かどうか。URL の位置引数と区別するために使う。
pub fn looks_like_item(input: &str) -> bool {
    split_on_separator(input).is_some_and(|(name, _, _)| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn field(name: &str, value: &str) -> Item {
        Item::Field {
            name: name.into(),
            value: value.into(),
        }
    }

    #[test]
    fn parses_string_field() {
        assert_eq!(parse_item("name=taro").unwrap(), field("name", "taro"));
    }

    #[test]
    fn parses_raw_field_as_json() {
        assert_eq!(
            parse_item("age:=30").unwrap(),
            Item::RawField {
                name: "age".into(),
                value: json!(30)
            }
        );
        assert_eq!(
            parse_item("tags:=[\"a\",\"b\"]").unwrap(),
            Item::RawField {
                name: "tags".into(),
                value: json!(["a", "b"])
            }
        );
    }

    #[test]
    fn raw_field_rejects_non_json_with_a_usable_message() {
        let err = parse_item("age:=notjson").unwrap_err().to_string();
        assert!(err.contains("age="), "次にやることを示していない: {err}");
    }

    #[test]
    fn parses_query_parameter() {
        assert_eq!(
            parse_item("limit==50").unwrap(),
            Item::Query {
                name: "limit".into(),
                value: "50".into()
            }
        );
    }

    #[test]
    fn parses_header_with_and_without_space() {
        let want = Item::Header {
            name: "X-Trace".into(),
            value: "abc".into(),
        };
        assert_eq!(parse_item("X-Trace: abc").unwrap(), want);
        assert_eq!(parse_item("X-Trace:abc").unwrap(), want);
    }

    #[test]
    fn header_value_may_contain_colons() {
        assert_eq!(
            parse_item("Authorization: Bearer a:b:c").unwrap(),
            Item::Header {
                name: "Authorization".into(),
                value: "Bearer a:b:c".into()
            }
        );
    }

    #[test]
    fn field_value_may_contain_equals_and_urls() {
        assert_eq!(
            parse_item("next=https://x/y?a=1").unwrap(),
            field("next", "https://x/y?a=1")
        );
    }

    #[test]
    fn parses_file_field() {
        assert_eq!(
            parse_item("avatar@./a.png").unwrap(),
            Item::FileField {
                name: "avatar".into(),
                path: PathBuf::from("./a.png")
            }
        );
    }

    #[test]
    fn an_item_that_reads_as_both_header_and_query_is_rejected_loudly() {
        // 黙ってヘッダにすると、意図したクエリが飛ばないままエラーも出ない。
        let err = parse_item("filter:status==open").unwrap_err().to_string();
        assert!(err.contains("ヘッダ"), "{err}");
        assert!(err.contains("escape"), "次にやることを示していない: {err}");
    }

    #[test]
    fn escaping_the_colon_resolves_the_ambiguity_towards_a_query() {
        assert_eq!(
            parse_item(r"filter\:status==open").unwrap(),
            Item::Query {
                name: "filter:status".into(),
                value: "open".into()
            }
        );
    }

    #[test]
    fn a_normal_header_whose_value_contains_a_single_equals_is_still_fine() {
        // `==` を含まない限り曖昧ではない。ここまで拒むとヘッダが書けなくなる。
        assert_eq!(
            parse_item("Cookie: sid=abc").unwrap(),
            Item::Header {
                name: "Cookie".into(),
                value: "sid=abc".into()
            }
        );
    }

    #[test]
    fn backslashes_that_are_not_escaping_a_separator_are_kept() {
        // 何でも escape すると、Windows のパスや正規表現が黙って壊れる。
        assert_eq!(
            parse_item(r"file@C:\tmp\auth.txt").unwrap(),
            Item::FileField {
                name: "file".into(),
                path: PathBuf::from(r"C:\tmp\auth.txt"),
            }
        );
        assert_eq!(
            parse_item(r"pattern=\d+\w+").unwrap(),
            field("pattern", r"\d+\w+")
        );
    }

    #[test]
    fn a_backslash_can_still_escape_itself() {
        assert_eq!(parse_item(r"a=b\\c").unwrap(), field("a", r"b\c"));
    }

    #[test]
    fn escaped_separator_stays_in_the_value() {
        assert_eq!(parse_item(r"note=a\=b").unwrap(), field("note", "a=b"));
        assert_eq!(parse_item(r"a\=b=v").unwrap(), field("a=b", "v"));
    }

    #[test]
    fn rejects_empty_key() {
        assert!(parse_item("=v").is_err());
    }

    #[test]
    fn a_bare_url_is_not_an_item() {
        // URL の位置引数を item と誤認すると、URL がヘッダとして解釈されてしまう。
        assert!(!looks_like_item("/users"));
        assert!(!looks_like_item("users"));
        // scheme 付きは `:` を含むが、キーが `https` になるだけで item として成立してしまう。
        // 呼び出し側は位置引数を先に URL として取り出すため、ここでは判定しない。
        assert!(looks_like_item("https://example.com"));
    }
}
