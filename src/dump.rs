//! 完全なリクエスト / レスポンスをファイルに落とし、索引に 1 行足す。
//!
//! ここが ailo の中核。本文を標準出力に出さずに済むのは、後から辿れる置き場所があるから。
//! 索引(`index.jsonl`)を grep すれば、全文をコンテキストに通さずに目的の 1 件へ辿り着ける。
//!
//! 置き場所はリポジトリの外(XDG のデータディレクトリ)に固定する。作業ディレクトリの中に
//! 置くと、`.gitignore` の入れ忘れ 1 回で認証情報が公開リポジトリへ流れる。

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::paths;
use crate::redact::Redactor;

/// 既定の保持件数。
pub const DEFAULT_KEEP_COUNT: usize = 200;
/// 既定の保持日数。
pub const DEFAULT_KEEP_DAYS: u64 = 7;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum BodyRecord {
    Json {
        value: Value,
    },
    Text {
        text: String,
    },
    /// 本文が JSON でもテキストでもない場合。中身は保存せず大きさだけ残す。
    Binary {
        bytes: usize,
    },
    Empty,
}

impl BodyRecord {
    /// 本文を人が読む 1 本のテキストにする。ダイジェストの先頭数行もここから取る。
    pub fn as_text(&self) -> Option<String> {
        match self {
            BodyRecord::Json { value } => serde_json::to_string_pretty(value).ok(),
            BodyRecord::Text { text } => Some(text.clone()),
            BodyRecord::Binary { .. } | BodyRecord::Empty => None,
        }
    }

    fn redacted(&self, r: &Redactor) -> BodyRecord {
        match self {
            BodyRecord::Json { value } => BodyRecord::Json {
                value: r.json(value),
            },
            BodyRecord::Text { text } => BodyRecord::Text { text: r.text(text) },
            other => other.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    pub method: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: BodyRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseRecord {
    pub status: u16,
    pub status_text: String,
    pub headers: BTreeMap<String, String>,
    pub body: BodyRecord,
    pub bytes: usize,
    pub ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dump {
    pub ts: String,
    /// 名前付きリクエストとして実行した場合のみ入る。
    pub name: Option<String>,
    /// 環境名。
    pub env: Option<String>,
    /// マスクを適用済みかどうか。false のダンプには生の認証情報が入っている。
    pub redacted: bool,
    pub request: RequestRecord,
    pub response: ResponseRecord,
}

/// 索引の 1 行。ダンプ本体を開かずに絞り込むための最小限だけを持つ。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    pub method: String,
    pub url: String,
    pub status: u16,
    pub ms: u64,
    pub bytes: usize,
    /// 索引からの相対ファイル名。
    pub dump: String,
}

pub struct Retention {
    pub keep_count: usize,
    pub keep_days: u64,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            keep_count: DEFAULT_KEEP_COUNT,
            keep_days: DEFAULT_KEEP_DAYS,
        }
    }
}

pub struct Written {
    pub path: PathBuf,
}

/// いま時刻の RFC3339 表現とファイル名向けの表現を返す。
fn timestamps() -> (String, String) {
    let now = OffsetDateTime::now_utc();
    let iso = now.format(&Rfc3339).unwrap_or_else(|_| "unknown".into());
    // `:` はファイル名に使いたくない(シェルでも Windows でも扱いが面倒)。
    let file = iso.replace(':', "-");
    (iso, file)
}

/// 同一秒に複数リクエストを投げてもファイル名が衝突しないようにする短い接尾辞。
///
/// `attempt` は衝突したときのやり直し回数。同じナノ秒・同じ pid で当たった場合でも
/// 別の綴りになるようにする。
fn suffix_for(attempt: u32) -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!(
        "{:04x}",
        (nanos ^ std::process::id() ^ attempt.wrapping_mul(0x9e37)) & 0xffff
    )
}

fn ensure_dir(dir: &Path) -> Result<()> {
    // すでにあるディレクトリのパーミッションは触らない。
    // `AILO_DUMP_DIR` が既存の共有ディレクトリを指していた場合、こちらの都合で
    // 0700 に書き換えてしまう。作るときだけ絞る。
    if dir.exists() {
        return Ok(());
    }
    fs::create_dir_all(dir).with_context(|| format!("{} を作成できません", dir.display()))?;
    // 認証情報が入りうるので、他ユーザーからは見せない。
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("{} のパーミッションを設定できません", dir.display()))?;
    Ok(())
}

/// ダンプ本体を書き、索引に 1 行足し、古いものを掃除する。
pub fn write(dump: &Dump, redactor: &Redactor, retention: &Retention) -> Result<Written> {
    let dir = paths::dumps_dir()?;
    ensure_dir(&dir)?;

    let mut redacted = dump.clone();
    apply(&mut redacted, redactor);
    let json = serde_json::to_string_pretty(&redacted)?;

    // 接尾辞は 16 ビットしかないので衝突しうる。`create_new` なので既存を壊しはしないが、
    // **リクエストは既に送り終えている**。ここで諦めると記録だけが失われるので、
    // 別の接尾辞で数回やり直す。
    let (filename, path, mut f) = {
        let (_, file_ts) = timestamps();
        let mut chosen = None;
        for attempt in 0..8 {
            let filename = format!("{file_ts}-{}.json", suffix_for(attempt));
            let path = dir.join(&filename);
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(f) => {
                    chosen = Some((filename, path, f));
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(e).with_context(|| format!("{} を作成できません", path.display()))
                }
            }
        }
        chosen.ok_or_else(|| anyhow::anyhow!("{} にダンプを作成できません", dir.display()))?
    };
    f.write_all(json.as_bytes())?;
    f.write_all(b"\n")?;

    append_index(
        &IndexEntry {
            ts: redacted.ts.clone(),
            name: redacted.name.clone(),
            env: redacted.env.clone(),
            method: redacted.request.method.clone(),
            url: redacted.request.url.clone(),
            status: redacted.response.status,
            ms: redacted.response.ms,
            bytes: redacted.response.bytes,
            dump: filename,
        },
        &dir,
    )?;

    prune(&dir, retention)?;

    Ok(Written { path })
}

/// ダンプ全体にマスクを適用する。書き出す直前に必ず通す。
fn apply(dump: &mut Dump, r: &Redactor) {
    dump.redacted = r.is_enabled();
    dump.request.url = r.url(&dump.request.url);
    dump.request.headers = dump
        .request
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), r.header_value(k, v)))
        .collect();
    dump.request.body = dump.request.body.redacted(r);
    dump.response.headers = dump
        .response
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), r.header_value(k, v)))
        .collect();
    dump.response.body = dump.response.body.redacted(r);
}

/// レスポンス 1 本にマスクを適用した複製を返す。
///
/// 画面に出す前にも通す。ダンプだけを守っても、反響された token がダイジェストに出れば
/// そのままエージェントのコンテキストに載る。
/// ただし `--pick` の結果には通さない。あれは値そのものを取りに行く明示的な操作。
pub fn redact_response(res: &ResponseRecord, r: &Redactor) -> ResponseRecord {
    ResponseRecord {
        headers: res
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), r.header_value(k, v)))
            .collect(),
        body: res.body.redacted(r),
        ..res.clone()
    }
}

fn append_index(entry: &IndexEntry, dir: &Path) -> Result<()> {
    let index = dir.join("index.jsonl");
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&index)
        .with_context(|| format!("{} を開けません", index.display()))?;
    writeln!(f, "{}", serde_json::to_string(entry)?)?;
    Ok(())
}

/// 掃除中であることを示すロック。これより古いロックは残骸とみなして奪う。
const PRUNE_LOCK_STALE_SECS: u64 = 60;

/// 掃除の排他を取る。取れなければ `None`。
///
/// 索引の追記は O_APPEND の 1 行書きなので競合しない。危ないのは掃除のほうで、
/// 「全部読む → 書き直す → rename」の途中に別プロセスが追記すると、その行が消える。
/// 掃除は後回しにしても困らないので、取れなければ黙って見送る。
fn acquire_prune_lock(dir: &Path) -> Option<PruneLock> {
    let path = dir.join("prune.lock");
    match OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(_) => Some(PruneLock { path }),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // 掃除の途中で落ちるとロックが残る。古すぎるものは奪って進む。
            let stale = fs::metadata(&path)
                .and_then(|m| m.modified())
                .and_then(|t| {
                    SystemTime::now()
                        .duration_since(t)
                        .map_err(|_| std::io::Error::other("時刻が巻き戻っている"))
                })
                .map(|age| age.as_secs() > PRUNE_LOCK_STALE_SECS)
                .unwrap_or(false);
            if stale {
                let _ = fs::remove_file(&path);
            }
            None
        }
        Err(_) => None,
    }
}

struct PruneLock {
    path: PathBuf,
}

impl Drop for PruneLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// 保持条件を超えたダンプを消し、索引からも該当行を落とす。
fn prune(dir: &Path, retention: &Retention) -> Result<()> {
    // 排他が取れなければ見送る。別プロセスが掃除中か、直後に掃除される。
    let Some(_lock) = acquire_prune_lock(dir) else {
        return Ok(());
    };

    let index = dir.join("index.jsonl");
    let Ok(content) = fs::read_to_string(&index) else {
        return Ok(());
    };

    let entries: Vec<IndexEntry> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    let cutoff = OffsetDateTime::now_utc() - time::Duration::days(retention.keep_days as i64);
    let too_old = |e: &IndexEntry| OffsetDateTime::parse(&e.ts, &Rfc3339).is_ok_and(|t| t < cutoff);

    // 新しいほうから keep_count 件だけ残す。
    let start = entries.len().saturating_sub(retention.keep_count);
    let (dropped, kept): (Vec<_>, Vec<_>) = entries
        .into_iter()
        .enumerate()
        .partition(|(i, e)| *i < start || too_old(e));

    if dropped.is_empty() {
        return Ok(());
    }

    // 先に索引を差し替え、そのあとで実体を消す。逆にすると、途中で落ちたときに
    // 「索引にあるのにファイルが無い」状態になり、`ailo show` が理由なく失敗する。
    let rewritten: String = kept
        .iter()
        .filter_map(|(_, e)| serde_json::to_string(e).ok())
        .map(|l| format!("{l}\n"))
        .collect();
    let tmp = index.with_extension(format!("jsonl.tmp.{}", std::process::id()));
    {
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(rewritten.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &index)?;

    for (_, e) in &dropped {
        let _ = fs::remove_file(dir.join(&e.dump));
    }

    let referenced: std::collections::HashSet<&str> =
        kept.iter().map(|(_, e)| e.dump.as_str()).collect();
    remove_orphans(dir, &referenced, retention)?;
    Ok(())
}

/// 索引から辿れなくなったダンプを消す。
///
/// 削除の対象を索引の行からしか辿らないと、何かの拍子に索引から落ちたダンプが
/// **保持期間を超えて永久に残る**。ダンプには認証情報が入りうるので、
/// 残留そのものがリスクになる。実体側からも掃く。
fn remove_orphans(
    dir: &Path,
    referenced: &std::collections::HashSet<&str>,
    retention: &Retention,
) -> Result<()> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(());
    };
    let max_age = std::time::Duration::from_secs(retention.keep_days * 24 * 60 * 60);
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.ends_with(".json") || referenced.contains(name) {
            continue;
        }
        // 書かれた直後のダンプを消さないよう、保持期間を過ぎたものだけにする。
        // 並行して走っている別プロセスが、まだ索引に載せていない可能性がある。
        let too_old = entry
            .metadata()
            .and_then(|m| m.modified())
            .and_then(|t| {
                SystemTime::now()
                    .duration_since(t)
                    .map_err(|_| std::io::Error::other("時刻が巻き戻っている"))
            })
            .map(|age| age > max_age)
            .unwrap_or(false);
        if too_old {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}

/// 索引を新しい順に読む。`ailo log` 用。
pub fn read_index(limit: usize) -> Result<Vec<IndexEntry>> {
    let index = paths::index_path()?;
    let Ok(content) = fs::read_to_string(&index) else {
        return Ok(Vec::new());
    };
    let mut entries: Vec<IndexEntry> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    entries.reverse();
    entries.truncate(limit);
    Ok(entries)
}

/// 索引の行に対応するダンプ本体のパス。
pub fn dump_path(entry: &IndexEntry) -> Result<PathBuf> {
    Ok(paths::dumps_dir()?.join(&entry.dump))
}

/// ダンプディレクトリが存在することだけ確かめる(`ailo log` などで使う)。
pub fn dumps_exist() -> bool {
    paths::index_path()
        .map(|p| File::open(p).is_ok())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Dump {
        Dump {
            ts: "2026-09-02T14:03:11Z".into(),
            name: Some("login".into()),
            env: Some("stg".into()),
            redacted: false,
            request: RequestRecord {
                method: "POST".into(),
                url: "https://api.example.com/auth/login".into(),
                headers: BTreeMap::from([
                    ("authorization".into(), "Bearer s3cr3t-token-value".into()),
                    ("content-type".into(), "application/json".into()),
                ]),
                body: BodyRecord::Json {
                    value: json!({"email": "a@example.com"}),
                },
            },
            response: ResponseRecord {
                status: 200,
                status_text: "OK".into(),
                headers: BTreeMap::from([("set-cookie".into(), "sid=deadbeefcafe".into())]),
                body: BodyRecord::Json {
                    value: json!({"token": "s3cr3t-token-value"}),
                },
                bytes: 42,
                ms: 312,
            },
        }
    }

    #[test]
    fn redaction_removes_sensitive_headers_from_the_dump() {
        let mut d = sample();
        apply(&mut d, &Redactor::new(true));
        assert_eq!(d.request.headers["authorization"], crate::redact::MASK);
        assert_eq!(d.response.headers["set-cookie"], crate::redact::MASK);
        assert_eq!(d.request.headers["content-type"], "application/json");
        assert!(d.redacted);
    }

    #[test]
    fn registered_literals_are_removed_from_the_response_body() {
        // ヘッダ名だけを見ていると、ボディに入った token を取りこぼす。
        let mut d = sample();
        let mut r = Redactor::new(true);
        r.add_literal("s3cr3t-token-value");
        apply(&mut d, &r);
        let serialized = serde_json::to_string(&d).unwrap();
        assert!(
            !serialized.contains("s3cr3t-token-value"),
            "ボディに token が残っている: {serialized}"
        );
    }

    #[test]
    fn control_case_confirms_the_check_can_actually_fail() {
        // 「0 件だから安全」の監査は、検査自体が壊れていても同じ見え方になる。
        // マスクを外せば必ずヒットすることを示し、上のテストが生きていることを担保する。
        let mut d = sample();
        apply(&mut d, &Redactor::disabled());
        let serialized = serde_json::to_string(&d).unwrap();
        assert!(
            serialized.contains("s3cr3t-token-value"),
            "マスクを外しても検出できない。検査が壊れている"
        );
        assert!(!d.redacted);
    }

    #[test]
    fn dump_filenames_do_not_contain_colons() {
        let (_, file_ts) = timestamps();
        assert!(!file_ts.contains(':'), "{file_ts}");
    }

    #[test]
    fn body_text_renders_json_for_the_digest() {
        let b = BodyRecord::Json {
            value: json!({"a": 1}),
        };
        assert!(b.as_text().unwrap().contains("\"a\""));
        assert!(BodyRecord::Binary { bytes: 3 }.as_text().is_none());
    }
}
