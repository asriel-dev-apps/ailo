//! `ailo config` — 設定ファイルの読み書き。
//!
//! **`git config` と同じく、ファイルの構造をそのままパスで指す。** 変数は `[vars]`(共通)と
//! `[env.<名前>.vars]`(環境ごと)の 2 か所にあるので、`env set` のような専用の動詞を
//! 置くと前者に届かない。覚えることを増やさないために、書き換えの入口は 1 つにする。
//!
//! ```text
//! ailo config set -e stg base_url https://stg.example.com   # env.stg.vars.base_url
//! ailo config set api_version v1                            # vars.api_version
//! ailo config set env.stg.headers.Accept application/json    # フルパス
//! ```
//!
//! 読み書きは `toml_edit` を通す。`Config` に読み込んで書き戻すと、人が書いた
//! コメントと並びが値を 1 つ変えるたびに消える。

use anyhow::{bail, Context, Result};
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::config::{validate_env_name, Config};

/// 設定ファイルのトップレベルにある名前。
///
/// キーの先頭がこれなら**フルパスとして扱う**。そうでなければ変数の短い書き方だと見なす。
/// `vars` という名前の変数を作りたい場合は `vars.vars` と書く。
const TOP_LEVEL: &[&str] = &[
    "default_env",
    "vars",
    "headers",
    "env",
    "redact_headers",
    "retention",
];

/// 整数として書き込むパス。ここに無いものは全部文字列。
///
/// **推測で型を決めない。** `vars` の値は `BTreeMap<String, String>` なので、`123` を
/// 整数として書くと次の読み込みが型エラーで落ちる。「数字に見えるから整数」は
/// 設定ファイルを壊す方向にしか働かない。
const INTEGER_PATHS: &[&[&str]] = &[&["retention", "keep_count"], &["retention", "keep_days"]];

/// `-e` とキーから、設定ファイル内のパスを決める。
pub fn resolve_path(env: Option<&str>, key: &str) -> Result<Vec<String>> {
    if key.is_empty() {
        bail!("キーが空です");
    }
    let segments: Vec<String> = key.split('.').map(str::to_string).collect();
    if segments.iter().any(String::is_empty) {
        bail!("`{key}` はパスとして読めません。`.` が続いています");
    }

    if TOP_LEVEL.contains(&segments[0].as_str()) {
        if env.is_some() {
            // 黙って片方を選ばない。どちらの意味にも取れる。
            bail!("`{key}` はフルパスなので `-e` と同時には使えません。`-e` を外すか、キーを短い形で書いてください");
        }
        return Ok(segments);
    }

    match env {
        Some(e) => {
            validate_env_name(e)?;
            let mut path = vec!["env".to_string(), e.to_string(), "vars".to_string()];
            path.extend(segments);
            Ok(path)
        }
        None => {
            let mut path = vec!["vars".to_string()];
            path.extend(segments);
            Ok(path)
        }
    }
}

/// 平文の設定ファイルに書いてはいけない値かどうか。
///
/// `[env.prd.vars] token = "..."` を許すと、秘匿値が平文で残る。`ailo secret set` が
/// あるのにそちらへ行かないのは、単に `config set` のほうが手近だからなので、ここで止める。
///
/// **テンプレート(`{{...}}`)は通す。** `Authorization = "Bearer {{access_token}}"` は
/// 設定に書くのが正しい形で、値そのものはキーチェーンにある。
fn refuse_secret(path: &[String], value: &str) -> Result<()> {
    if value.contains("{{") {
        return Ok(());
    }
    let Some(last) = path.last() else {
        return Ok(());
    };

    let under = |name: &str| path.iter().any(|s| s == name);

    if under("vars") && crate::redact::is_sensitive_field(last) {
        bail!(
            "`{last}` は秘匿値の名前に見えます。平文の設定ファイルには書けません。\n\
             `ailo secret set <環境> {last}` を使ってください(秘匿値ではないなら `ailo config edit` で直接書けます)"
        );
    }
    if under("headers") && crate::redact::Redactor::new(true).is_sensitive_header(last) {
        bail!(
            "ヘッダ `{last}` の値を平文で書こうとしています。\n\
             `ailo secret set <環境> <キー>` で入れて、ここには `{{{{<キー>}}}}` と書いてください"
        );
    }
    Ok(())
}

/// パスの位置に値を書く。途中のテーブルは作る。
pub fn set(doc: &mut DocumentMut, path: &[String], value: &str) -> Result<()> {
    refuse_secret(path, value)?;

    let (last, parents) = path.split_last().expect("パスは空でない");
    let mut table = doc.as_table_mut();
    for name in parents {
        let entry = table
            .entry(name)
            .or_insert_with(|| Item::Table(Table::new()));
        table = entry.as_table_mut().ok_or_else(|| {
            anyhow::anyhow!("`{name}` はテーブルではないので、その下に値を置けません")
        })?;
    }

    let is_integer = INTEGER_PATHS
        .iter()
        .any(|p| p.len() == path.len() && p.iter().zip(path).all(|(a, b)| a == b));
    let new_value: Value = if is_integer {
        let n: i64 = value
            .parse()
            .with_context(|| format!("`{}` には整数が要ります: {value}", path.join(".")))?;
        n.into()
    } else {
        value.into()
    };
    table.insert(last, Item::Value(new_value));
    Ok(())
}

/// パスの位置の値を消す。消すものが無ければ false。
pub fn unset(doc: &mut DocumentMut, path: &[String]) -> Result<bool> {
    let (last, parents) = path.split_last().expect("パスは空でない");
    let mut table = doc.as_table_mut();
    for name in parents {
        let Some(next) = table.get_mut(name).and_then(Item::as_table_mut) else {
            return Ok(false);
        };
        table = next;
    }
    Ok(table.remove(last).is_some())
}

/// パスの位置を、`名前 = 値` の平らな並びにして返す。
///
/// テーブルなら中身を再帰的に並べる。**値そのものが 1 つなら 1 行**。
pub fn flatten(doc: &DocumentMut, path: &[String]) -> Vec<(String, String)> {
    let mut item: &Item = doc.as_item();
    for name in path {
        match item.get(name) {
            Some(next) => item = next,
            None => return Vec::new(),
        }
    }
    let mut out = Vec::new();
    walk(item, &path.join("."), &mut out);
    out
}

fn walk(item: &Item, prefix: &str, out: &mut Vec<(String, String)>) {
    match item {
        Item::Table(t) => {
            for (name, child) in t.iter() {
                let next = if prefix.is_empty() {
                    name.to_string()
                } else {
                    format!("{prefix}.{name}")
                };
                walk(child, &next, out);
            }
        }
        Item::Value(Value::InlineTable(t)) => {
            for (name, child) in t.iter() {
                let next = if prefix.is_empty() {
                    name.to_string()
                } else {
                    format!("{prefix}.{name}")
                };
                out.push((next, render(child)));
            }
        }
        Item::Value(v) => out.push((prefix.to_string(), render(v))),
        Item::ArrayOfTables(_) | Item::None => {}
    }
}

/// 表示用の値。文字列はクォートを外す。`--pick` と同じ流儀。
fn render(v: &Value) -> String {
    match v {
        Value::String(s) => s.value().clone(),
        other => other.to_string().trim().to_string(),
    }
}

/// 書き戻す前に、`Config` として読めることを確かめる。
///
/// **壊れた設定を書かない。** ここを通さないと、`config set` の 1 回で
/// 以降すべてのリクエストが「設定を読めません」で落ちる状態になる。
pub fn check(text: &str) -> Result<()> {
    toml::from_str::<Config>(text)
        .map(|_| ())
        .with_context(|| "設定として読めない形になります。書き込みを中止しました".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> DocumentMut {
        text.parse().unwrap()
    }

    #[test]
    fn a_short_key_becomes_a_common_variable() {
        assert_eq!(
            resolve_path(None, "api_version").unwrap(),
            ["vars", "api_version"]
        );
    }

    #[test]
    fn a_short_key_with_an_environment_becomes_that_environments_variable() {
        assert_eq!(
            resolve_path(Some("stg"), "base_url").unwrap(),
            ["env", "stg", "vars", "base_url"]
        );
    }

    #[test]
    fn a_full_path_is_taken_as_written() {
        assert_eq!(
            resolve_path(None, "env.stg.headers.Accept").unwrap(),
            ["env", "stg", "headers", "Accept"]
        );
        assert_eq!(resolve_path(None, "default_env").unwrap(), ["default_env"]);
    }

    #[test]
    fn a_full_path_together_with_an_environment_is_refused_rather_than_guessed() {
        // どちらの意味にも取れる。黙って片方を選ばない。
        let err = resolve_path(Some("stg"), "headers.Accept")
            .unwrap_err()
            .to_string();
        assert!(err.contains("-e"), "{err}");
    }

    #[test]
    fn a_malformed_path_is_refused() {
        assert!(resolve_path(None, "").is_err());
        assert!(resolve_path(None, "a..b").is_err());
    }

    #[test]
    fn setting_a_value_keeps_the_comments_and_the_order() {
        // 人が手で書くファイルなので、ここが消えるのは実質的な破壊。
        let mut d = doc("# 共通の設定\n[vars]\n# API の版\napi_version = \"v1\"\n");
        set(&mut d, &["vars".into(), "who".into()], "taro").unwrap();
        let out = d.to_string();
        assert!(out.contains("# 共通の設定"), "{out}");
        assert!(out.contains("# API の版"), "{out}");
        assert!(out.contains("who = \"taro\""), "{out}");
    }

    #[test]
    fn missing_tables_are_created_on_the_way() {
        let mut d = doc("");
        set(
            &mut d,
            &["env".into(), "stg".into(), "vars".into(), "base_url".into()],
            "http://x",
        )
        .unwrap();
        assert!(d.to_string().contains("base_url"));
        check(&d.to_string()).unwrap();
    }

    #[test]
    fn a_numeric_looking_variable_stays_a_string() {
        // `vars` は文字列の表。整数として書くと次の読み込みが型エラーで落ちる。
        let mut d = doc("");
        set(&mut d, &["vars".into(), "n".into()], "123").unwrap();
        assert!(d.to_string().contains("n = \"123\""), "{}", d.to_string());
        check(&d.to_string()).unwrap();
    }

    #[test]
    fn retention_is_written_as_an_integer() {
        let mut d = doc("");
        set(&mut d, &["retention".into(), "keep_count".into()], "50").unwrap();
        assert!(
            d.to_string().contains("keep_count = 50"),
            "{}",
            d.to_string()
        );
        check(&d.to_string()).unwrap();
    }

    #[test]
    fn retention_refuses_a_non_integer_instead_of_writing_a_string() {
        let mut d = doc("");
        let err = set(
            &mut d,
            &["retention".into(), "keep_count".into()],
            "たくさん",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("整数"), "{err}");
    }

    #[test]
    fn a_secret_looking_variable_is_refused_and_points_at_the_keychain() {
        let mut d = doc("");
        let err = set(
            &mut d,
            &["env".into(), "prd".into(), "vars".into(), "token".into()],
            "abc",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("ailo secret set"), "{err}");
    }

    #[test]
    fn a_template_referring_to_a_secret_is_allowed() {
        // 値そのものはキーチェーンにある。設定に書くのが正しい形。
        let mut d = doc("");
        set(
            &mut d,
            &[
                "env".into(),
                "prd".into(),
                "headers".into(),
                "Authorization".into(),
            ],
            "Bearer {{access_token}}",
        )
        .unwrap();
        assert!(d.to_string().contains("Bearer"));
    }

    #[test]
    fn a_literal_authorization_header_is_refused() {
        let mut d = doc("");
        let err = set(
            &mut d,
            &["headers".into(), "Authorization".into()],
            "Bearer abc",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("ailo secret set"), "{err}");
    }

    #[test]
    fn unset_removes_only_what_was_named() {
        let mut d = doc("[vars]\na = \"1\"\nb = \"2\"\n");
        assert!(unset(&mut d, &["vars".into(), "a".into()]).unwrap());
        let out = d.to_string();
        assert!(!out.contains("a = "), "{out}");
        assert!(out.contains("b = \"2\""), "{out}");
    }

    #[test]
    fn unset_says_when_there_was_nothing_to_remove() {
        let mut d = doc("[vars]\na = \"1\"\n");
        assert!(!unset(&mut d, &["vars".into(), "nope".into()]).unwrap());
        assert!(!unset(
            &mut d,
            &["env".into(), "stg".into(), "vars".into(), "x".into()]
        )
        .unwrap());
    }

    #[test]
    fn flatten_lists_a_subtree_as_dotted_keys() {
        let d = doc("default_env = \"stg\"\n[vars]\na = \"1\"\n[env.stg.vars]\nb = \"2\"\n");
        assert_eq!(
            flatten(&d, &[]),
            vec![
                ("default_env".to_string(), "stg".to_string()),
                ("vars.a".to_string(), "1".to_string()),
                ("env.stg.vars.b".to_string(), "2".to_string()),
            ]
        );
        assert_eq!(
            flatten(&d, &["env".into(), "stg".into()]),
            vec![("env.stg.vars.b".to_string(), "2".to_string())]
        );
    }

    #[test]
    fn flatten_returns_nothing_for_a_path_that_is_not_there() {
        let d = doc("[vars]\na = \"1\"\n");
        assert!(flatten(&d, &["env".into(), "nope".into()]).is_empty());
    }

    #[test]
    fn a_document_that_would_not_load_is_refused() {
        // 綴りの間違いを黙って受けない。`deny_unknown_fields` の網に掛ける。
        assert!(check("defaultenv = \"stg\"\n").is_err());
        assert!(check("default_env = \"stg\"\n").is_ok());
    }
}
