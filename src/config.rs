//! 設定・保存済みリクエスト・キャプチャした状態の読み書き。
//!
//! 3 つのファイルに分かれている。分けている理由は「誰が書き換えるか」が違うから。
//!
//! | ファイル | 場所 | 書き換える主体 |
//! | --- | --- | --- |
//! | `config.toml` | 設定ディレクトリ | 人間と `ailo config` |
//! | `requests.toml` | 設定ディレクトリ | `ailo save` |
//! | `state/<env>.toml` | データディレクトリ | キャプチャ |
//!
//! **どのファイルにも秘匿値は書かない。** キャプチャした値のうち秘匿指定されたものは
//! キーチェーンへ行き、ここには残らない。

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

/// 一時ファイルへ書いてから rename する。
///
/// 書き込み先を直接 truncate すると、途中で落ちたときに壊れたファイルが残る。
/// `state/<env>.toml` が壊れると期限の記録ごと失われるので、ここは不可分にする。
/// 一時ファイル名に pid を混ぜるのは、同時に走った ailo どうしが同じ一時ファイルを
/// 奪い合わないようにするため。
fn write_private(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("{} を書けません", paths::tildify(&tmp)))?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("{} を更新できません", paths::tildify(path)))?;
    Ok(())
}

// ---------------------------------------------------------------- config.toml

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `--env` を省いたときに使う環境。
    pub default_env: Option<String>,
    /// 全環境共通の変数。
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    /// 全環境共通で足すヘッダ。
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// 環境ごとの設定。
    #[serde(default)]
    pub env: BTreeMap<String, EnvConfig>,
    /// 既定に加えてマスクするヘッダ名。
    #[serde(default)]
    pub redact_headers: Vec<String>,
    #[serde(default)]
    pub retention: RetentionConfig,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvConfig {
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// **片方だけ書けるようにしておく。** 両方必須だと、`keep_count` を変えたい人が
/// `keep_days` も書かされ、書き忘れると設定全体が読めなくなる。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionConfig {
    #[serde(default = "default_keep_count")]
    pub keep_count: usize,
    #[serde(default = "default_keep_days")]
    pub keep_days: u64,
}

fn default_keep_count() -> usize {
    crate::dump::DEFAULT_KEEP_COUNT
}

fn default_keep_days() -> u64 {
    crate::dump::DEFAULT_KEEP_DAYS
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            keep_count: default_keep_count(),
            keep_days: default_keep_days(),
        }
    }
}

/// 置き去りのロックを奪ってよいまでの秒数。
const LOCK_STALE_SECS: u64 = 60;
/// ロックを待つ上限。人が待てる範囲で、かつ 1 回の書き換えには十分長い。
const LOCK_WAIT_MS: u64 = 3000;

/// 設定ファイルの読み書きを直列化する。
///
/// **`write_private` が保証するのは 1 回の書き込みの原子性だけで、
/// 「読む → 変える → 書き戻す」の原子性ではない。** 排他が無いと、同時に走った
/// `ailo config set` は互いの結果を捨て合い、しかも全部が成功として終わる。
/// 主利用者はエージェントで、セットアップを並列に流すのは普通の使い方なので、
/// ここは「たまたま起きない」に頼れない。
pub struct ConfigLock {
    path: PathBuf,
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// 設定ファイルのロックを取る。取れるまで少し待ち、駄目なら失敗させる。
///
/// 掃除のロック(`dump.rs`)と違い、**取れなければ見送るのではなく落とす**。
/// 掃除は後回しにできるが、設定の書き換えを黙って見送ると値が消えたのと同じになる。
pub fn lock_config() -> Result<ConfigLock> {
    let dir = paths::config_dir()?;
    fs::create_dir_all(&dir)?;
    let path = dir.join("config.lock");

    let started = std::time::Instant::now();
    loop {
        match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(_) => return Ok(ConfigLock { path }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // 書き換えの途中で落ちるとロックが残る。古すぎるものは奪って進む。
                let stale = fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())
                    .is_some_and(|age| age.as_secs() > LOCK_STALE_SECS);
                if stale {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                if started.elapsed().as_millis() as u64 >= LOCK_WAIT_MS {
                    anyhow::bail!(
                        "他の ailo が設定を書き換えています。少し待ってやり直してください({} が残り続ける場合は消してください)",
                        paths::tildify(&path)
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => {
                return Err(e).with_context(|| format!("{} を作れません", paths::tildify(&path)))
            }
        }
    }
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        Ok(paths::config_dir()?.join("config.toml"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        let Ok(text) = fs::read_to_string(&path) else {
            return Ok(Self::default());
        };
        toml::from_str(&text).with_context(|| format!("{} を読めません", paths::tildify(&path)))
    }

    /// 設定ファイルの中身をそのまま読む。無ければ空文字。
    ///
    /// `ailo config` は `Config` に読み込んで書き戻すのではなく、本文を直接扱う。
    /// 読み込んで書き戻すと、人が書いたコメントと並びが消える。
    ///
    /// **「無い」以外の読み取り失敗は握り潰さない。** 権限や I/O の失敗まで
    /// 空文字にすると、`config set` がその空文字を土台にして**既存の設定を
    /// 丸ごと置き換える**。読めなかったことは、消えたことより先に伝える。
    pub fn read_text() -> Result<String> {
        let path = Self::path()?;
        match fs::read_to_string(&path) {
            Ok(text) => Ok(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e).with_context(|| format!("{} を読めません", paths::tildify(&path))),
        }
    }

    /// 設定ファイルを丸ごと置き換える。一時ファイル + rename、0600。
    pub fn write_text(text: &str) -> Result<()> {
        write_private(&Self::path()?, text)
    }

    /// 環境名を解決する。`--env` > `default_env`。
    pub fn resolve_env(&self, requested: Option<&str>) -> Option<String> {
        requested
            .map(str::to_string)
            .or_else(|| self.default_env.clone())
    }

    pub fn env_config(&self, env: Option<&str>) -> EnvConfig {
        env.and_then(|e| self.env.get(e))
            .cloned()
            .unwrap_or_default()
    }

    /// 環境名が設定に存在するか。打ち間違いを送信前に捕まえるために使う。
    pub fn knows_env(&self, env: &str) -> bool {
        self.env.contains_key(env)
    }

    pub fn environments(&self) -> Vec<&str> {
        self.env.keys().map(String::as_str).collect()
    }
}

// -------------------------------------------------------------- requests.toml

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Requests {
    #[serde(default)]
    pub requests: BTreeMap<String, SavedRequest>,
}

#[derive(PartialEq, Debug, Default, Clone, Serialize, Deserialize)]
pub struct SavedRequest {
    pub method: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub form: bool,
    /// レスポンスから変数へ束縛する式。`{ access_token = ".data.token" }`
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub capture: BTreeMap<String, String>,
    /// `capture` のうちキーチェーンへ入れるもの。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secret: Vec<String>,
}

impl Requests {
    pub fn path() -> Result<PathBuf> {
        Ok(paths::config_dir()?.join("requests.toml"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        let Ok(text) = fs::read_to_string(&path) else {
            return Ok(Self::default());
        };
        toml::from_str(&text).with_context(|| format!("{} を読めません", paths::tildify(&path)))
    }

    pub fn save(&self) -> Result<()> {
        write_private(&Self::path()?, &toml::to_string_pretty(self)?)
    }

    pub fn get(&self, name: &str) -> Option<&SavedRequest> {
        self.requests.get(name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.requests.keys().map(String::as_str).collect()
    }

    pub fn put(&mut self, name: &str, req: SavedRequest) {
        self.requests.insert(name.to_string(), req);
    }
}

// ------------------------------------------------------------ 直前のリクエスト

/// `ailo save` の材料。**展開前のテンプレート**を持つ。
///
/// ダンプから復元しないのは、ダンプがマスク済みだから。`***` を保存してしまう。
/// かといって展開後の値を残せば平文の token をファイルに書くことになる。
/// 展開前なら `{{token}}` のまま残る。
///
/// ただし展開前でも、`Authorization: Bearer <生の値>` や `password=<生の値>` のように
/// **人が直接打ち込んだ秘匿値**はテンプレートに含まれてしまう。これはここに書かず、
/// 落とした項目の名前だけを `redacted` に残す。`ailo save` はそれを見て保存を拒む。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastInvocation {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub items: Vec<String>,
    #[serde(default)]
    pub raw: Option<String>,
    #[serde(default)]
    pub form: bool,
    /// 記録時に値を落とした項目名。空でなければ `save` できない。
    #[serde(default)]
    pub redacted: Vec<String>,
}

impl LastInvocation {
    fn path() -> Result<PathBuf> {
        Ok(paths::data_dir()?.join("last.toml"))
    }

    pub fn record(&self) -> Result<()> {
        write_private(&Self::path()?, &toml::to_string_pretty(self)?)
    }

    pub fn load() -> Result<Option<Self>> {
        let path = Self::path()?;
        let Ok(text) = fs::read_to_string(&path) else {
            return Ok(None);
        };
        Ok(toml::from_str(&text).ok())
    }
}

// ------------------------------------------------------------- state/<env>.toml

/// キャプチャした非秘匿の値と、token の期限。
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    /// 変数名 → RFC3339 の失効時刻。
    #[serde(default)]
    pub expires_at: BTreeMap<String, String>,
}

/// 環境名として使える文字。
///
/// 環境名はファイル名とキーチェーンの account 名の一部になる。素通しにすると
/// `--env ../../../../tmp/x` で state ファイルをデータディレクトリの外へ書き出せる
/// (実際に書けることを確認した)。名前は英数と `-` `_` `.` に限る。
/// `.` は許すが、`.` だけ・`..` だけの名前は弾く。
pub fn validate_env_name(env: &str) -> Result<()> {
    let ok = !env.is_empty()
        && env != "."
        && env != ".."
        && env
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        anyhow::bail!("環境名 `{env}` は使えません。英数字と `-` `_` `.` だけで指定してください")
    }
}

impl State {
    fn path(env: &str) -> Result<PathBuf> {
        validate_env_name(env)?;
        Ok(paths::data_dir()?.join("state").join(format!("{env}.toml")))
    }

    pub fn load(env: &str) -> Result<Self> {
        let path = Self::path(env)?;
        let Ok(text) = fs::read_to_string(&path) else {
            // 未作成は普通の状態。エラーにしない。
            return Ok(Self::default());
        };
        // 壊れていたら既定値で続けない。既定値には期限が入っていないので、
        // 期限切れの token を無期限に送り続けることになる。破損は必ず知らせる。
        toml::from_str(&text).with_context(|| {
            format!(
                "{} が壊れています。消せば再取得からやり直せます",
                paths::tildify(&path)
            )
        })
    }

    pub fn save(&self, env: &str) -> Result<()> {
        write_private(&Self::path(env)?, &toml::to_string_pretty(self)?)
    }

    /// 期限切れの変数名を返す。
    pub fn expired(&self, now: time::OffsetDateTime) -> Vec<&str> {
        use time::format_description::well_known::Rfc3339;
        self.expires_at
            .iter()
            .filter(|(_, at)| time::OffsetDateTime::parse(at, &Rfc3339).is_ok_and(|t| t <= now))
            .map(|(name, _)| name.as_str())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_config_file_is_not_an_error() {
        // 設定を書く前でもアドホックに叩けること。
        assert!(Config::default().vars.is_empty());
    }

    #[test]
    fn config_parses_environments_and_common_values() {
        let cfg: Config = toml::from_str(
            r#"
            default_env = "stg"
            [vars]
            api_version = "v1"
            [env.stg.vars]
            base_url = "https://stg.example.com"
            [env.prd.vars]
            base_url = "https://api.example.com"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.resolve_env(None).as_deref(), Some("stg"));
        assert_eq!(cfg.resolve_env(Some("prd")).as_deref(), Some("prd"));
        assert_eq!(cfg.vars["api_version"], "v1");
        assert_eq!(
            cfg.env_config(Some("prd")).vars["base_url"],
            "https://api.example.com"
        );
        assert_eq!(cfg.environments(), vec!["prd", "stg"]);
    }

    #[test]
    fn an_unknown_environment_yields_empty_settings_not_a_panic() {
        let cfg = Config::default();
        assert!(cfg.env_config(Some("nope")).vars.is_empty());
        assert!(!cfg.knows_env("nope"));
    }

    #[test]
    fn a_typo_in_a_config_key_is_rejected_rather_than_ignored() {
        // 黙って無視すると「設定したのに効かない」を延々と追うことになる。
        let err = toml::from_str::<Config>("defualt_env = \"stg\"").unwrap_err();
        assert!(err.to_string().contains("defualt_env"), "{err}");
    }

    #[test]
    fn saved_requests_round_trip_through_toml() {
        let mut reqs = Requests::default();
        reqs.put(
            "login",
            SavedRequest {
                method: "POST".into(),
                url: "{{base_url}}/auth/login".into(),
                items: vec!["email={{email}}".into()],
                capture: BTreeMap::from([("access_token".into(), ".data.token".into())]),
                secret: vec!["access_token".into()],
                ..Default::default()
            },
        );
        let text = toml::to_string_pretty(&reqs).unwrap();
        let back: Requests = toml::from_str(&text).unwrap();
        let saved = back.get("login").unwrap();
        assert_eq!(saved.url, "{{base_url}}/auth/login");
        assert_eq!(saved.capture["access_token"], ".data.token");
        assert_eq!(saved.secret, vec!["access_token".to_string()]);
    }

    #[test]
    fn saved_requests_keep_templates_not_expanded_values() {
        // 展開後を保存すると平文の token がファイルに残る。
        let text = toml::to_string_pretty(&Requests {
            requests: BTreeMap::from([(
                "x".into(),
                SavedRequest {
                    method: "GET".into(),
                    url: "{{base_url}}/u".into(),
                    items: vec!["Authorization: Bearer {{token}}".into()],
                    ..Default::default()
                },
            )]),
        })
        .unwrap();
        assert!(text.contains("{{token}}"), "{text}");
        assert!(!text.contains("Bearer s3cr3t"), "{text}");
    }

    #[test]
    fn an_environment_name_cannot_escape_the_data_directory() {
        // 素通しにすると `--env ../../../../tmp/x` で state ファイルを
        // データディレクトリの外へ書き出せた。実際に書けることを確認済み。
        for bad in ["../x", "a/b", "..", ".", "", "a\0b", "x/../../y"] {
            assert!(validate_env_name(bad).is_err(), "通してしまった: {bad:?}");
        }
        assert!(State::path("../evil").is_err());
    }

    #[test]
    fn ordinary_environment_names_are_accepted() {
        for good in ["stg", "prd", "dev-1", "my_env", "v1.2"] {
            assert!(validate_env_name(good).is_ok(), "弾いてしまった: {good}");
        }
    }

    #[test]
    fn expiry_is_reported_only_for_times_already_past() {
        let now = time::OffsetDateTime::now_utc();
        let state = State {
            vars: BTreeMap::new(),
            expires_at: BTreeMap::from([
                ("old".into(), "2020-01-01T00:00:00Z".into()),
                ("future".into(), "2999-01-01T00:00:00Z".into()),
                ("garbage".into(), "not a time".into()),
            ]),
        };
        let expired = state.expired(now);
        assert_eq!(expired, vec!["old"]);
    }
}
