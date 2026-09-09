//! `{{変数}}` の解決と展開。
//!
//! 値は複数の層から来る。先に見つかったほうが勝つ。
//!
//! 1. コマンドラインの `--var k=v`
//! 2. プロセスの環境変数 `AILO_VAR_<KEY>`
//! 3. キーチェーンの秘匿値
//! 4. 環境ファイルの環境固有セクション
//! 5. 環境ファイルの共通セクション
//!
//! 未解決の変数が残ったらリクエストを送らずに落とす。空文字で送ってしまうと、
//! 返ってきた 401 の原因が「token が空だった」ことだと分からない。

use std::collections::BTreeMap;

use anyhow::{bail, Result};

/// 環境変数から変数を拾うときの接頭辞。
///
/// 裸の環境変数名を見ると `PATH` や `HOME` を変数として拾ってしまう。接頭辞で隔離する。
pub const ENV_PREFIX: &str = "AILO_VAR_";

#[derive(Debug, Clone)]
pub struct Layer {
    pub name: &'static str,
    pub values: BTreeMap<String, String>,
    /// この層の値を秘匿として扱うか。秘匿値はマスク対象になる。
    pub secret: bool,
}

impl Layer {
    pub fn new(name: &'static str, values: BTreeMap<String, String>, secret: bool) -> Self {
        Self {
            name,
            values,
            secret,
        }
    }
}

/// 変数 1 つの、**外へ出してよい**説明。
#[derive(Debug, Clone, PartialEq)]
pub struct VarDescription {
    pub name: String,
    /// 秘匿値なら伏せ字になっている。
    pub shown: String,
    pub from: &'static str,
    pub secret: bool,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub value: String,
    pub secret: bool,
    /// どの層から来たか。診断用。
    pub from: &'static str,
}

#[derive(Debug, Clone, Default)]
pub struct Vars {
    resolved: BTreeMap<String, Resolved>,
}

/// プロセスの環境変数から `AILO_VAR_*` を集める。
///
/// 秘匿かどうかは**名前で判断する**。層ごと秘匿にすると `AILO_VAR_base_url` の値まで
/// マスク対象になり、ダンプの索引が `{"url":"***/users"}` になる。
/// 索引を grep して 1 件に辿り着けることは、このツールの中核機能なので壊せない。
/// 秘匿として渡したい値は名前でそう分かるようにするか、`AILO_SECRET_*` を使う。
pub fn layer_from_process_env() -> Vec<Layer> {
    let mut plain = BTreeMap::new();
    let mut secret = BTreeMap::new();
    for (k, v) in std::env::vars() {
        let Some(name) = k.strip_prefix(ENV_PREFIX) else {
            continue;
        };
        if crate::redact::is_sensitive_field(name) {
            secret.insert(name.to_string(), v);
        } else {
            plain.insert(name.to_string(), v);
        }
    }
    vec![
        Layer::new("env", secret, true),
        Layer::new("env", plain, false),
    ]
}

impl Vars {
    /// 層を優先順に渡す。先頭が最優先。
    pub fn from_layers(layers: Vec<Layer>) -> Self {
        let mut resolved: BTreeMap<String, Resolved> = BTreeMap::new();
        for layer in layers {
            for (k, v) in layer.values {
                resolved.entry(k).or_insert(Resolved {
                    value: v,
                    secret: layer.secret,
                    from: layer.name,
                });
            }
        }
        Self { resolved }
    }

    pub fn get(&self, name: &str) -> Option<&Resolved> {
        self.resolved.get(name)
    }

    pub fn is_empty(&self) -> bool {
        self.resolved.is_empty()
    }

    /// 一覧に出すための説明。**秘匿値は既に伏せてある。**
    ///
    /// 生の値を外へ出す口を作らないのがこの関数の目的。呼び出し側が
    /// 「秘匿なら伏せる」を書く形にすると、書き忘れた場所から漏れる。
    pub fn describe(&self) -> Vec<VarDescription> {
        self.resolved
            .iter()
            .map(|(name, r)| {
                // **画面に出す側は厳しいほうに倒す。** 層の `secret` 旗だけを見ていた
                // ときは、`capture` で取ったトークンが（`secret = [...]` の書き忘れで）
                // `state` 層に入り、変数一覧に全文で出ていた。
                // プロセス環境変数は名前で判定しているのに capture は出す、という不整合。
                let secret = r.secret || crate::redact::is_sensitive_field(name);
                VarDescription {
                    name: name.clone(),
                    shown: if secret {
                        crate::redact::MASK.to_string()
                    } else {
                        r.value.clone()
                    },
                    from: r.from,
                    secret,
                }
            })
            .collect()
    }

    /// 秘匿として解決された値。マスクの literal に登録するために使う。
    pub fn secret_values(&self) -> Vec<&str> {
        self.resolved
            .values()
            .filter(|r| r.secret)
            .map(|r| r.value.as_str())
            .collect()
    }

    /// 変数の値が変数を含む場合に、何回まで展開し直すか。
    ///
    /// `A = "{{B}}"` のような設定は普通に書かれる。1 回しか展開しないと `{{B}}` が
    /// そのまま送信され、未解決を送信前に止めるという約束が破れる。
    /// 循環参照で止まらなくならないよう上限を置く。
    const MAX_PASSES: usize = 8;

    /// テンプレートを展開する。未解決があれば送信前に落とす。
    ///
    /// 展開結果にまだ変数が残っていれば、解決できなくなるまで繰り返す。
    pub fn expand(&self, template: &str) -> Result<Expanded> {
        let mut current = self.expand_once(template)?;
        for _ in 1..Self::MAX_PASSES {
            if !current.text.contains("{{") {
                return Ok(current);
            }
            let next = self.expand_once(&current.text)?;
            if next.text == current.text {
                // これ以上変わらない。閉じていない `{{` などが残っているだけ。
                return Ok(current);
            }
            current = Expanded {
                text: next.text,
                used_secret: current.used_secret || next.used_secret,
            };
        }
        if Self::referenced_names(&current.text)
            .iter()
            .any(|n| self.resolved.contains_key(n))
        {
            bail!(
                "変数の展開が {} 回で終わりませんでした。変数どうしが循環参照している可能性があります",
                Self::MAX_PASSES
            );
        }
        Ok(current)
    }

    fn expand_once(&self, template: &str) -> Result<Expanded> {
        let mut out = String::with_capacity(template.len());
        let mut missing: Vec<String> = Vec::new();
        let mut used_secret = false;
        let mut rest = template;

        while let Some(start) = rest.find("{{") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else {
                // 閉じていないものは変数ではない。そのまま残す。
                out.push_str(&rest[start..]);
                rest = "";
                break;
            };
            let name = after[..end].trim();
            match self.resolved.get(name) {
                Some(r) => {
                    out.push_str(&r.value);
                    used_secret |= r.secret;
                }
                None => {
                    missing.push(name.to_string());
                    // 落とす前提なので、出力の中身は使われない。
                    out.push_str("{{");
                    out.push_str(name);
                    out.push_str("}}");
                }
            }
            rest = &after[end + 2..];
        }
        out.push_str(rest);

        if !missing.is_empty() {
            missing.dedup();
            bail!(
                "変数が解決できません: {}。`ailo secret set <env> <名前>` か `--var <名前>=<値>` で与えてください",
                missing.join(", ")
            );
        }

        Ok(Expanded {
            text: out,
            used_secret,
        })
    }

    /// テンプレートに現れる変数名を、解決の可否を問わず列挙する。
    pub fn referenced_names(template: &str) -> Vec<String> {
        let mut names = Vec::new();
        let mut rest = template;
        while let Some(start) = rest.find("{{") {
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else { break };
            names.push(after[..end].trim().to_string());
            rest = &after[end + 2..];
        }
        names
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Expanded {
    pub text: String,
    /// 展開した値に秘匿値が含まれていたか。URL に載ったときの警告に使う。
    pub used_secret: bool,
}

/// `--var k=v` を解釈する。
pub fn parse_assignment(input: &str) -> Result<(String, String)> {
    match input.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
        _ => bail!("`{input}` は `名前=値` の形で指定してください"),
    }
}

pub fn map(pairs: impl IntoIterator<Item = (String, String)>) -> BTreeMap<String, String> {
    pairs.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vars {
        Vars::from_layers(vec![
            Layer::new(
                "cli",
                map([("base_url".into(), "https://cli.example".into())]),
                false,
            ),
            Layer::new(
                "keychain",
                map([("token".into(), "s3cr3t-token-value".into())]),
                true,
            ),
            Layer::new(
                "env-file",
                map([
                    ("base_url".into(), "https://file.example".into()),
                    ("region".into(), "jp".into()),
                ]),
                false,
            ),
        ])
    }

    #[test]
    fn the_earlier_layer_wins() {
        assert_eq!(vars().get("base_url").unwrap().value, "https://cli.example");
        assert_eq!(vars().get("base_url").unwrap().from, "cli");
    }

    #[test]
    fn a_value_only_in_a_later_layer_is_still_found() {
        assert_eq!(vars().get("region").unwrap().value, "jp");
    }

    #[test]
    fn expands_variables_in_a_template() {
        let e = vars().expand("{{base_url}}/users/{{region}}").unwrap();
        assert_eq!(e.text, "https://cli.example/users/jp");
        assert!(!e.used_secret);
    }

    #[test]
    fn whitespace_inside_the_braces_is_allowed() {
        assert_eq!(vars().expand("{{ region }}").unwrap().text, "jp");
    }

    #[test]
    fn expanding_a_secret_is_reported_so_the_caller_can_react() {
        let e = vars().expand("Bearer {{token}}").unwrap();
        assert_eq!(e.text, "Bearer s3cr3t-token-value");
        assert!(e.used_secret, "秘匿値を使ったことが伝わっていない");
    }

    #[test]
    fn an_unresolved_variable_stops_the_request_and_names_it() {
        // 空文字で送ってしまうと、返ってきた 401 の原因が分からなくなる。
        let err = vars()
            .expand("{{base_url}}/{{nope}}")
            .unwrap_err()
            .to_string();
        assert!(err.contains("nope"), "{err}");
        assert!(err.contains("--var"), "次にやることを示していない: {err}");
    }

    #[test]
    fn all_missing_names_are_reported_at_once() {
        let err = vars().expand("{{a}}{{b}}").unwrap_err().to_string();
        assert!(err.contains('a') && err.contains('b'), "{err}");
    }

    #[test]
    fn an_unclosed_brace_is_left_alone_rather_than_treated_as_a_variable() {
        // JSON の本文などに `{{` が現れることはある。
        let e = vars().expand("{{ not closed").unwrap();
        assert_eq!(e.text, "{{ not closed");
    }

    #[test]
    fn a_variable_whose_value_is_itself_a_variable_is_fully_expanded() {
        // 1 回しか展開しないと `{{token}}` がそのまま送信され、
        // 「未解決は送信前に止める」という約束が破れる。
        let v = Vars::from_layers(vec![
            Layer::new(
                "cli",
                map([("auth".into(), "Bearer {{token}}".into())]),
                false,
            ),
            Layer::new(
                "keychain",
                map([("token".into(), "s3cr3t-token-value".into())]),
                true,
            ),
        ]);
        let e = v.expand("{{auth}}").unwrap();
        assert_eq!(e.text, "Bearer s3cr3t-token-value");
        // 間接的に秘匿値を使ったことも伝わること。伝わらないとマスクから漏れる。
        assert!(e.used_secret, "間接参照で秘匿の印が落ちている");
    }

    #[test]
    fn an_unresolvable_variable_introduced_by_expansion_is_reported() {
        let v = Vars::from_layers(vec![Layer::new(
            "cli",
            map([("auth".into(), "Bearer {{nope}}".into())]),
            false,
        )]);
        let err = v.expand("{{auth}}").unwrap_err().to_string();
        assert!(err.contains("nope"), "{err}");
    }

    #[test]
    fn a_reference_cycle_stops_instead_of_looping_forever() {
        let v = Vars::from_layers(vec![Layer::new(
            "cli",
            map([("a".into(), "{{b}}".into()), ("b".into(), "{{a}}".into())]),
            false,
        )]);
        let err = v.expand("{{a}}").unwrap_err().to_string();
        assert!(err.contains("循環"), "{err}");
    }

    #[test]
    fn text_without_variables_passes_through_unchanged() {
        assert_eq!(vars().expand("plain").unwrap().text, "plain");
    }

    #[test]
    fn secret_values_are_listed_for_masking() {
        assert_eq!(vars().secret_values(), vec!["s3cr3t-token-value"]);
    }

    #[test]
    fn referenced_names_are_found_even_when_unresolvable() {
        assert_eq!(
            Vars::referenced_names("{{a}}/{{ b }}"),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn assignments_split_on_the_first_equals_only() {
        assert_eq!(
            parse_assignment("url=https://x/?a=1").unwrap(),
            ("url".into(), "https://x/?a=1".into())
        );
        assert!(parse_assignment("=v").is_err());
        assert!(parse_assignment("novalue").is_err());
    }

    #[test]
    fn process_env_variables_need_the_prefix() {
        // 裸の環境変数名を拾うと PATH や HOME まで変数になる。
        std::env::set_var("AILO_VAR_from_env", "yes");
        std::env::set_var("NOT_AN_AILO_VAR", "no");
        let layers = layer_from_process_env();
        let found = layers
            .iter()
            .find_map(|l| l.values.get("from_env"))
            .map(String::as_str);
        assert_eq!(found, Some("yes"));
        assert!(layers
            .iter()
            .all(|l| !l.values.contains_key("NOT_AN_AILO_VAR")));
        std::env::remove_var("AILO_VAR_from_env");
        std::env::remove_var("NOT_AN_AILO_VAR");
    }

    #[test]
    fn a_plain_env_variable_is_not_treated_as_a_secret() {
        // 層ごと秘匿にすると base_url の値までマスク対象になり、
        // ダンプの索引が `{"url":"***/users"}` になって grep が壊れる。
        std::env::set_var("AILO_VAR_base_url", "https://example.com");
        std::env::set_var("AILO_VAR_api_token", "s3cr3t-token-value");
        let v = Vars::from_layers(layer_from_process_env());
        assert!(
            !v.get("base_url").unwrap().secret,
            "base_url が秘匿扱いになっている"
        );
        assert!(
            v.get("api_token").unwrap().secret,
            "token が秘匿扱いになっていない"
        );
        assert_eq!(v.secret_values(), vec!["s3cr3t-token-value"]);
        std::env::remove_var("AILO_VAR_base_url");
        std::env::remove_var("AILO_VAR_api_token");
    }
}
