//! ailo から見た秘匿値の出し入れ。
//!
//! 実際の保管は [`crate::keychain`]（macOS は Keychain、Linux は Secret Service）。
//! ここが決めているのは「環境ごとに名前空間を切ること」と「値がどこから来るか」だけ。
//!
//! 平文ファイルには値を書かない。書くのは**キー名の索引だけ**で、これは
//! `ailo secret ls` のためにある。macOS の `security` も Secret Service も
//! 「この service に属する account を列挙する」を素直に提供しないため、
//! 名前だけを別に持つ。名前は秘匿ではない。

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

/// 環境変数から秘匿値を渡すときの接頭辞。headless や CI 用のフォールバック。
pub const ENV_PREFIX: &str = "AILO_SECRET_";

/// キーチェーン上の account 名。環境ごとに名前空間を分ける。
fn account(env: &str, key: &str) -> String {
    format!("{env}/{key}")
}

/// `AILO_SECRET_<ENV>_<KEY>`。どちらも大文字にし、`-` は `_` に寄せる。
pub fn env_var_name(env: &str, key: &str) -> String {
    let norm = |s: &str| s.to_ascii_uppercase().replace(['-', '.'], "_");
    format!("{ENV_PREFIX}{}_{}", norm(env), norm(key))
}

/// 1 件読む。環境変数が最優先。
///
/// キーチェーンの無い環境(CI、コンテナ)でも動かせる経路を必ず残しておく。
/// ここが無いと、キーチェーンが使えない場所で平文ファイルに書く誘惑が生まれる。
pub fn get(env: &str, key: &str) -> Result<Option<String>> {
    if let Ok(v) = std::env::var(env_var_name(env, key)) {
        if !v.is_empty() {
            return Ok(Some(v));
        }
    }
    crate::keychain::load(&account(env, key))
        .with_context(|| format!("`{env}` の `{key}` を読み出せません"))
}

pub fn set(env: &str, key: &str, value: &str) -> Result<()> {
    crate::keychain::save(&account(env, key), value)
        .with_context(|| format!("`{env}` の `{key}` を保存できません"))?;
    Index::load()?.add(env, key)?;
    Ok(())
}

pub fn remove(env: &str, key: &str) -> Result<()> {
    crate::keychain::delete(&account(env, key))
        .with_context(|| format!("`{env}` の `{key}` を削除できません"))?;
    Index::load()?.remove(env, key)?;
    Ok(())
}

/// ある環境の秘匿値をまとめて読む。変数解決の 1 層になる。
///
/// 索引にあるのに読めないキーは黙って飛ばす。別のマシンで登録した索引を
/// 同期した場合など、値だけが無い状態は普通に起こる。
pub fn load_env(env: &str) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for key in Index::load()?.keys(env) {
        if let Ok(Some(v)) = get(env, &key) {
            out.insert(key, v);
        }
    }
    // 索引に無くても環境変数で渡されたものは拾う。
    let prefix = format!("{ENV_PREFIX}{}_", env.to_ascii_uppercase().replace(['-', '.'], "_"));
    for (k, v) in std::env::vars() {
        if let Some(name) = k.strip_prefix(&prefix) {
            if !v.is_empty() {
                out.insert(name.to_ascii_lowercase(), v);
            }
        }
    }
    Ok(out)
}

/// キー名の索引。値は持たない。
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Index {
    #[serde(default)]
    envs: BTreeMap<String, Vec<String>>,
}

impl Index {
    fn path() -> Result<std::path::PathBuf> {
        Ok(paths::config_dir()?.join("secret-index.toml"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        let Ok(text) = fs::read_to_string(&path) else {
            return Ok(Self::default());
        };
        toml::from_str(&text).with_context(|| format!("{} を読めません", paths::tildify(&path)))
    }

    pub fn keys(&self, env: &str) -> Vec<String> {
        self.envs.get(env).cloned().unwrap_or_default()
    }

    pub fn environments(&self) -> Vec<&str> {
        self.envs.keys().map(String::as_str).collect()
    }

    fn add(&mut self, env: &str, key: &str) -> Result<()> {
        let list = self.envs.entry(env.to_string()).or_default();
        if !list.iter().any(|k| k == key) {
            list.push(key.to_string());
            list.sort();
        }
        self.save()
    }

    fn remove(&mut self, env: &str, key: &str) -> Result<()> {
        if let Some(list) = self.envs.get_mut(env) {
            list.retain(|k| k != key);
            if list.is_empty() {
                self.envs.remove(env);
            }
        }
        self.save()
    }

    fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self)?;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("{} を書けません", paths::tildify(&path)))?;
        f.write_all(text.as_bytes())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounts_are_namespaced_by_environment() {
        // stg と prd で同名のキーが衝突してはいけない。
        assert_eq!(account("stg", "token"), "stg/token");
        assert_ne!(account("stg", "token"), account("prd", "token"));
    }

    #[test]
    fn env_var_names_are_upper_case_and_underscored() {
        assert_eq!(env_var_name("stg", "access_token"), "AILO_SECRET_STG_ACCESS_TOKEN");
        assert_eq!(env_var_name("my-env", "api.key"), "AILO_SECRET_MY_ENV_API_KEY");
    }

    #[test]
    fn the_index_holds_names_only() {
        // 索引は平文ファイル。値が入ると、キーチェーンに仕舞う意味がなくなる。
        let mut idx = Index::default();
        idx.envs.insert("stg".into(), vec!["access_token".into()]);
        let text = toml::to_string_pretty(&idx).unwrap();
        assert!(text.contains("access_token"));
        assert!(!text.contains("Bearer"), "値が索引に入っている: {text}");
    }

    #[test]
    fn keys_are_reported_per_environment() {
        let mut idx = Index::default();
        idx.envs.insert("stg".into(), vec!["a".into()]);
        idx.envs.insert("prd".into(), vec!["b".into()]);
        assert_eq!(idx.keys("stg"), vec!["a".to_string()]);
        assert!(idx.keys("nope").is_empty());
        assert_eq!(idx.environments(), vec!["prd", "stg"]);
    }

    #[test]
    fn the_environment_variable_fallback_wins_over_the_keychain() {
        // キーチェーンの無い環境でも動く経路。ここが無いと平文ファイルへの誘惑が生まれる。
        std::env::set_var("AILO_SECRET_UNITTEST_TOKEN", "from-env");
        let got = get("unittest", "token").unwrap();
        std::env::remove_var("AILO_SECRET_UNITTEST_TOKEN");
        assert_eq!(got.as_deref(), Some("from-env"));
    }

    #[test]
    fn an_empty_environment_variable_is_not_treated_as_a_value() {
        // 空文字を値として通すと、原因の分からない 401 になる。
        std::env::set_var("AILO_SECRET_UNITTEST2_TOKEN", "");
        let got = get("unittest2", "token");
        std::env::remove_var("AILO_SECRET_UNITTEST2_TOKEN");
        // キーチェーンに無ければ None。エラーではない。
        assert!(matches!(got, Ok(None) | Err(_)));
    }
}
