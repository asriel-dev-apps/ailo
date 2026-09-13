//! 送信履歴の索引。SQLite に置く。本文はファイルのまま。
//!
//! 設計は ADR 0001。要点だけ写すと:
//!
//! - **行は無期限、本文は 200 件 7 日。** 行は数百バイトなので何万件持っても
//!   `ailo log` は索引が効いて一定時間で返る。重いのは本文だけなのでそちらだけ消す。
//! - **本文が消えた行には `body_deleted` が立つ。** これが無いと、実体側からの掃除
//!   (`remove_orphans`)の参照集合が全行になり、恒久的に空振りする。
//! - **並び順は `rowid` の降順。** `ts` で並べない。並列書き込みの同秒衝突と
//!   時計のずれで順序が入れ替わり、`log` の番号と `show` の番号がずれる。
//! - **テンプレート列(`*_template`)は `ailo query` から見えてはいけない。**
//!   `query.rs` の authorizer が消す。ここの `recent()` は守りの無い接続で読むので、
//!   **列名を明示して `select *` しない**ことは引き続き守る。

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use rusqlite::Connection;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::dump::{IndexEntry, Retention};
use crate::paths;

/// 索引が待つ時間。別プロセスが書いている間の `SQLITE_BUSY` を握りつぶさない。
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// 取り込みで 1 つのトランザクションに入れる件数。
///
/// 書き込みロックを掴む時間を短く保つためだけの値。大きくすると、取り込みの間に
/// 走った本物のリクエストの索引書き込みが待たされる。
const MIGRATE_CHUNK: usize = 200;

/// 取り込みのために `index.jsonl` から切り離した断面の名前の頭。
///
/// この頭で始まるファイルは「切り離したが、まだ取り込み終えていない」もの。
/// 起動のたびに拾い直すので、途中で落ちても置き去りにならない。
const MIGRATING_PREFIX: &str = "index.jsonl.migrating.";

/// 断面と退避先に付ける、他プロセスと衝突しない接尾辞。
fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos:x}", std::process::id())
}

/// 切り離し済みでまだ取り込んでいない断面。古い順に返す。
fn pending_snapshots(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(MIGRATING_PREFIX))
        })
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

/// 0600 で書く。隔離した行にも認証情報が入りうる。
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(text.as_bytes())
}

/// 履歴 1 行を作るための材料。マスク済みの索引項目と、展開前のテンプレート。
pub struct NewRow<'a> {
    pub entry: &'a IndexEntry,
    /// 展開前の URL。`entry.url` はマスク済みで、これだけでは送り直せない。
    pub url_template: &'a str,
    pub items_template: &'a [String],
    pub raw_template: Option<&'a str>,
    pub form: bool,
    /// 直書きの秘匿値として値ごと落とした項目の**名前**。値は入らない。
    pub redacted: &'a [String],
}

/// 画面と `ailo log` が読む 1 行。**テンプレート列は入らない。**
#[derive(Debug, Clone)]
pub struct Row {
    pub id: i64,
    pub ts: String,
    pub name: Option<String>,
    pub env: Option<String>,
    pub method: String,
    pub url: String,
    pub status: u16,
    pub ms: u64,
    pub bytes: usize,
    pub dump: String,
    /// 本文が保持期間を過ぎて消されている。
    pub body_deleted: bool,
}

pub struct Db {
    conn: Connection,
    /// 開くときに気づいたこと。呼ぶ側が `notes` へ流す。
    pub warnings: Vec<String>,
}

/// 履歴 DB のファイル名。ダンプと同じディレクトリに置く(workspace の分けが付いてくる)。
pub(crate) fn db_path() -> Result<PathBuf> {
    Ok(paths::dumps_dir()?.join("history.db"))
}

/// `history.db` に対する `-wal` / `-shm`。拡張子ではなく**名前の後ろに足す**。
fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut name = db.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    db.with_file_name(name)
}

/// DB 本体と、SQLite が作るサイドカー全部に 0600 を課す。
///
/// **SQLite はどれも 0644 で作る**(実測)。ダンプと旧 `index.jsonl` は明示的に
/// 0600 で、`ensure_dir` は既存ディレクトリの権限を意図的に触らない。
/// `AILO_DUMP_DIR` が共有ディレクトリを指していると、SQLite 化した瞬間に守りが下がる。
///
/// **本体を先に締めれば、サイドカーは付いてくる。** SQLite の unix VFS は
/// `-wal` / `-shm` / `-journal` を作るとき**本体 DB の mode を引き継ぐ**
/// (実測: 本体 600 → journal 600、644 → 644、640 → 640)。だから
/// `Connection::open` の直後に本体を締めておけば、そのあとの `append` や
/// `prune_bodies` が作るジャーナルも自動的に 0600 になる。
///
/// 残る窓は**いちばん最初の open だけ**。SQLite が本体を 0644 で作り、
/// こちらが締めるまでの間に書き込みが走るとそのジャーナルは 0644 になる。
/// だから本体を締めるのは**スキーマを流す前**。
///
/// `-journal` も一覧に入れておく。WAL が取れない置き場所ではこれが使われる。
/// 無いファイルは黙って飛ばすので、並べておいても害はない。
fn lock_down(db: &Path) -> Result<()> {
    for p in [
        db.to_path_buf(),
        sidecar(db, "-wal"),
        sidecar(db, "-shm"),
        sidecar(db, "-journal"),
    ] {
        if p.exists() {
            fs::set_permissions(&p, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("{} のパーミッションを設定できません", p.display()))?;
        }
    }
    Ok(())
}

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS history (
  -- `INTEGER PRIMARY KEY` は rowid の別名。**この別名であることが、
  -- 「並び順は追加順」を成立させている。** `WITHOUT ROWID` にしたり id を
  -- 明示的に振ったりすると、`log` と `show` の番号が黙ってずれる。
  id            INTEGER PRIMARY KEY,
  ts            TEXT    NOT NULL,
  name          TEXT,
  env           TEXT,
  -- `method` は展開前も展開後も同じ(変数を展開しても masking しても変わらない)ので、
  -- テンプレート列を別に持たない。`ailo query` から method 別に集計したいので、
  -- こちらは見える側に置く。
  method        TEXT    NOT NULL,
  url           TEXT    NOT NULL,
  status        INTEGER NOT NULL,
  ms            INTEGER NOT NULL,
  bytes         INTEGER NOT NULL,
  dump          TEXT    NOT NULL UNIQUE,
  body_deleted  INTEGER NOT NULL DEFAULT 0,
  url_template  TEXT,
  items_template TEXT,
  raw_template  TEXT,
  form          INTEGER NOT NULL DEFAULT 0,
  redacted      TEXT
);
CREATE INDEX IF NOT EXISTS history_ts ON history(ts);
CREATE INDEX IF NOT EXISTS history_live_body ON history(body_deleted, id);
"#;

/// `journal_mode` を WAL にする。WAL にできたら `None`、できなければ実際のモードを返す。
///
/// **`busy_timeout` はここには効かない。** ロールバックジャーナルから WAL への変換は
/// DB 全体の排他ロックを要求し、取れないと SQLite は busy handler を呼ばずに
/// `SQLITE_BUSY` を即座に返す。素で `?` すると、**まっさらな状態から 2 プロセスが
/// 同時に立ち上がっただけで `ailo log` が exit 2 で死ぬ**(実測 13%)。変換が要るのは
/// 初回だけで、既に WAL の DB に対しては no-op なので、定常状態では起きない。
///
/// 並んだ相手が変換を終えれば、こちらは no-op で通る。だから短く待って試し直す。
/// それでも駄目なら**いま何になっているかを読んで返す**。開くこと自体は諦めない:
/// 履歴は補助であって、これでコマンドを落としてはいけない。
fn set_wal(conn: &Connection) -> Result<Option<String>> {
    for attempt in 0..5 {
        match conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0)) {
            Ok(mode) if mode.eq_ignore_ascii_case("wal") => return Ok(None),
            Ok(mode) => return Ok(Some(mode)),
            Err(e) if is_busy(&e) => {
                std::thread::sleep(Duration::from_millis(20 * (attempt + 1)));
            }
            Err(e) => return Err(e).context("履歴 DB の journal_mode を設定できません"),
        }
    }
    // 変換はできなかったが、相手が終えていれば既に WAL になっている。
    let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
    Ok((!mode.eq_ignore_ascii_case("wal")).then_some(mode))
}

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy) | Some(rusqlite::ErrorCode::DatabaseLocked)
    )
}

impl Db {
    /// 履歴 DB を開く。無ければ作り、`index.jsonl` があれば取り込む。
    pub fn open() -> Result<Db> {
        let dir = paths::dumps_dir()?;
        crate::dump::ensure_dir(&dir)?;
        let path = db_path()?;
        let conn = Connection::open(&path)
            .with_context(|| format!("{} を開けません", paths::tildify(&path)))?;
        conn.busy_timeout(BUSY_TIMEOUT)?;

        let mut warnings = Vec::new();
        // **スキーマを流す前に本体を締める。** ここより後ろの書き込みが作る
        // ジャーナルは本体の mode を引き継ぐので、これで全部 0600 になる。
        if let Err(e) = lock_down(&path) {
            warnings.push(format!("{e:#}"));
        }
        if let Some(mode) = set_wal(&conn)? {
            // **取れなかったことを検知する。** rusqlite は要求した journal_mode が
            // 取れなくても失敗しない。`AILO_DUMP_DIR` が同期フォルダやネットワーク FS を
            // 指していると WAL が取れず、ロックが壊れて全履歴が一度に壊れる。
            // いまの最悪は「JSONL の 1 行が混線する」で、本文ファイルという冗長系が残る。
            // SQLite と無期限履歴では、その冗長系を捨てたうえで全部が壊れる。
            warnings.push(format!(
                "履歴 DB の journal_mode が WAL になりません({mode})。{} が同期フォルダや\
                 ネットワーク FS を指していないか確認してください。\
                 この状態ではロックが壊れ、履歴が一度に失われることがあります",
                paths::tildify(&dir)
            ));
        }

        conn.execute_batch(SCHEMA)
            .context("履歴 DB のスキーマを作成できません")?;
        // スキーマの作成が最初の書き込みなので、この時点でサイドカーがある。
        // **権限を締められないことで履歴を読めなくしない。** chmod が効かない FS や
        // 他人所有の `history.db` で `?` を返すと、`ailo log` が恒久的に死ぬ。
        // 読めないことのほうが、締められないことより重い。
        if let Err(e) = lock_down(&path) {
            warnings.push(format!("{e:#}"));
        }

        let mut db = Db { conn, warnings };
        db.migrate_jsonl(&dir);
        Ok(db)
    }

    /// `index.jsonl` があれば取り込み、取り込めたら退避する。
    ///
    /// **1 回だけにしない。** 旧バイナリが同時に走ると移行後に JSONL へ追記されうる。
    /// `dump` に UNIQUE を張って `INSERT OR IGNORE` にしてあるので、何度走らせても
    /// 同じ結果になる。
    ///
    /// **壊れた行があれば取り込みを失敗させる。** 現行の reader と prune は不正行を
    /// `filter_map(...ok())` で握りつぶしている。そのまま移すと欠損が検出できない。
    ///
    /// 設計では「一時 DB に入れてから rename」だが、**採っていない**。生きている DB へ
    /// 直接入れ、代わりに**小分けにして**書く。一時 DB の利点は「取り込みの間、
    /// 生きている DB を一度も掴まない」ことにある。SQLite の書き込みは WAL でも
    /// 直列化されるので、取り込みを 1 つの大きなトランザクションにすると、その間に
    /// 走った本物のリクエストの索引書き込みが待たされ、待ち時間を超えると落ちる。
    ///
    /// **一時 DB と同じにはならない。** 小分けにすると、コミットした分が途中で
    /// `recent()` から見える。一度きり・数秒で、行の順序も壊れないので受け入れている。
    /// 一方、既に行がある DB に一時 DB を rename で被せると、その間に入った行を
    /// 捨てることになる。そちらのほうが重い。
    ///
    /// **`Db::open` をここで失敗させない。** 取り込みが失敗したときに `?` を返すと、
    /// `ailo log` と `ailo show` が丸ごと使えなくなるうえ、`prune_bodies` は
    /// 索引に行が入る経路の中にしか無いので、**保持期間の掃除ごと止まる**。
    /// 認証情報を含むダンプが無期限に残り続けることになる。
    fn migrate_jsonl(&mut self, dir: &Path) {
        let Some(_lock) = crate::dump::acquire_lock(&dir.join("migrate.lock")) else {
            return;
        };

        // **読む前に切り離す。** 旧バイナリは `migrate.lock` を知らず `O_APPEND` で
        // 追記する。読んでから rename すると、その間に入った行が取り込まれないまま
        // 退避され、次回以降は `index.jsonl` しか見ないので**二度と拾われない**。
        // 先に rename しておけば、以後の追記は新しい `index.jsonl` に載る。
        let index = dir.join("index.jsonl");
        if index.exists() {
            let snap = dir.join(format!("{MIGRATING_PREFIX}{}", unique_suffix()));
            if let Err(e) = fs::rename(&index, &snap) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    self.warnings
                        .push(format!("{} を切り離せません: {e}", paths::tildify(&index)));
                    return;
                }
            }
        }

        // 切り離し済みの断面を拾う。**前回 rename したあとに落ちた分もここで回収される。**
        // 拾い漏らすと、そのファイルは誰も読まないまま残る。
        for snap in pending_snapshots(dir) {
            if let Err(e) = self.import_snapshot(&snap) {
                // 次回また拾える。落とさない。
                self.warnings.push(format!(
                    "{} を取り込めません（次回やり直します）: {e:#}",
                    paths::tildify(&snap)
                ));
            }
        }
    }

    /// 切り離した断面 1 本を取り込み、退避する。
    ///
    /// **壊れた行は握りつぶさないが、取り込み全体も止めない。** 読めない行だけを
    /// 別ファイルへ隔離し、行番号を警告に出す。止めてしまうと、1 行の破損で
    /// 履歴と掃除が恒久的に死ぬ（旧 prune の「全部読む→書き直す→rename」と
    /// `O_APPEND` の競合で、千切れた行は実際に起こりうると設計側も認めている）。
    fn import_snapshot(&mut self, snap: &Path) -> Result<()> {
        let content = fs::read_to_string(snap)
            .with_context(|| format!("{} を読めません", paths::tildify(snap)))?;

        let mut entries = Vec::new();
        let mut broken: Vec<(usize, &str)> = Vec::new();
        for (i, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<IndexEntry>(line) {
                Ok(e) => entries.push(e),
                Err(_) => broken.push((i + 1, line)),
            }
        }

        for chunk in entries.chunks(MIGRATE_CHUNK) {
            let tx = self.conn.transaction()?;
            {
                // 移行された行はテンプレートを持たない。**この行は再送できない。**
                // 画面まで先送りせず、ここで NULL と決めておく。
                let mut stmt = tx.prepare(
                    "INSERT OR IGNORE INTO history
                     (ts, name, env, method, url, status, ms, bytes, dump, body_deleted)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0)",
                )?;
                for e in chunk {
                    stmt.execute(rusqlite::params![
                        e.ts,
                        e.name,
                        e.env,
                        e.method,
                        e.url,
                        e.status,
                        e.ms as i64,
                        e.bytes as i64,
                        e.dump,
                    ])?;
                }
            }
            tx.commit()?;
        }

        let dir = snap.parent().unwrap_or(Path::new("."));
        let suffix = snap
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix(MIGRATING_PREFIX))
            .unwrap_or("unknown")
            .to_string();

        if !broken.is_empty() {
            let lines: Vec<usize> = broken.iter().map(|(n, _)| *n).collect();
            let kept = dir.join(format!("index.jsonl.broken.{suffix}"));
            let text: String = broken.iter().map(|(_, l)| format!("{l}\n")).collect();
            let where_to = match write_private(&kept, &text) {
                Ok(()) => paths::tildify(&kept),
                Err(e) => format!("(隔離にも失敗: {e})"),
            };
            self.warnings.push(format!(
                "{} の {} 行目が読めません。その {} 行を {} へ隔離し、残りを取り込みました",
                paths::tildify(snap),
                lines
                    .iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                lines.len(),
                where_to
            ));
        }

        // **退避先も一意にする。** 固定名だと 2 回目の移行が前回の退避を黙って
        // 上書きし、1 回目に取りこぼした行がディスクからも消える（実測）。
        let retired = dir.join(format!("index.jsonl.migrated.{suffix}"));
        match fs::rename(snap, &retired) {
            Ok(()) => self.warnings.push(format!(
                "{} 件を履歴 DB に取り込みました({} に退避)",
                entries.len(),
                paths::tildify(&retired)
            )),
            Err(e) => self.warnings.push(format!(
                "{} 件を履歴 DB に取り込みましたが、{} を退避できません: {e}",
                entries.len(),
                paths::tildify(snap)
            )),
        }
        Ok(())
    }

    /// 1 行足す。
    pub fn append(&self, row: &NewRow<'_>) -> Result<()> {
        let e = row.entry;
        self.conn.execute(
            "INSERT OR IGNORE INTO history
             (ts, name, env, method, url, status, ms, bytes, dump, body_deleted,
              url_template, items_template, raw_template, form, redacted)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10, ?11, ?12, ?13, ?14)",
            rusqlite::params![
                e.ts,
                e.name,
                e.env,
                e.method,
                e.url,
                e.status,
                e.ms as i64,
                e.bytes as i64,
                e.dump,
                row.url_template,
                serde_json::to_string(row.items_template)?,
                row.raw_template,
                row.form as i64,
                serde_json::to_string(row.redacted)?,
            ],
        )?;
        Ok(())
    }

    /// 新しい順に `limit` 件。**`log` も `show` もこの 1 つから番号を作る。**
    ///
    /// 番号が 2 か所で作られると、1 件増えただけで `show 1` が別のものを指す。
    pub fn recent(&self, limit: usize) -> Result<Vec<Row>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ts, name, env, method, url, status, ms, bytes, dump, body_deleted
             FROM history ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit as i64], |r| {
                Ok(Row {
                    id: r.get(0)?,
                    ts: r.get(1)?,
                    name: r.get(2)?,
                    env: r.get(3)?,
                    method: r.get(4)?,
                    url: r.get(5)?,
                    status: r.get::<_, i64>(6)? as u16,
                    ms: r.get::<_, i64>(7)? as u64,
                    bytes: r.get::<_, i64>(8)? as usize,
                    dump: r.get(9)?,
                    body_deleted: r.get::<_, i64>(10)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// 保持条件を超えた**本文**を消す。行は残す。
    ///
    /// 順序は **UPDATE → unlink**。逆にすると「行は生きていると言うのにファイルが無い」が
    /// 残る。途中で落ちた分は `remove_orphans` の mtime を見る掃除が次回拾う。
    pub fn prune_bodies(&self, retention: &Retention) -> Result<()> {
        let dir = paths::dumps_dir()?;
        // 掃除は後回しにしても困らない。取れなければ見送る。
        let Some(_lock) = crate::dump::acquire_lock(&dir.join("prune.lock")) else {
            return Ok(());
        };

        // **本文がまだ生きている行だけ**を見る。全行を見ると、行が無期限に残るぶん
        // 参照集合が際限なく膨らみ、実体側からの掃除が永久に空振りする。
        let mut stmt = self
            .conn
            .prepare("SELECT id, ts, dump FROM history WHERE body_deleted = 0 ORDER BY id DESC")?;
        let live: Vec<(i64, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let cutoff = OffsetDateTime::now_utc() - time::Duration::days(retention.keep_days as i64);
        let too_old = |ts: &str| OffsetDateTime::parse(ts, &Rfc3339).is_ok_and(|t| t < cutoff);
        let (dropped, kept): (Vec<_>, Vec<_>) = live
            .iter()
            .enumerate()
            .partition(|(i, (_, ts, _))| *i >= retention.keep_count || too_old(ts));

        if !dropped.is_empty() {
            // **1 行 1 トランザクションにしない。** auto-commit のまま回すと
            // 1 行ごとに fsync が走る。12 万行の履歴を初めて掃除する `ailo get` 1 回で
            // 10.4 秒かかっていた。取り込み側をわざわざ小分けにして掴む時間を
            // 短くしているのに、こちらが正反対をやっていた。
            let tx = self.conn.unchecked_transaction()?;
            {
                let mut mark = tx.prepare("UPDATE history SET body_deleted = 1 WHERE id = ?1")?;
                for (_, (id, _, _)) in &dropped {
                    mark.execute([id])?;
                }
            }
            tx.commit()?;
            // 印を付け終えてから消す。
            for (_, (_, _, dump)) in &dropped {
                let _ = fs::remove_file(dir.join(dump));
            }
        }

        let referenced: HashSet<&str> = kept.iter().map(|(_, (_, _, d))| d.as_str()).collect();
        remove_orphans(&dir, &referenced, retention)
    }
}

/// 索引から辿れなくなったダンプを消す。
///
/// 削除の対象を行からしか辿らないと、何かの拍子に行から落ちたダンプが
/// **保持期間を超えて永久に残る**。ダンプには認証情報が入りうるので、
/// 残留そのものがリスクになる。実体側からも掃く。
fn remove_orphans(dir: &Path, referenced: &HashSet<&str>, retention: &Retention) -> Result<()> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(());
    };
    let max_age = Duration::from_secs(retention.keep_days * 24 * 60 * 60);
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.ends_with(".json") || referenced.contains(name) {
            continue;
        }
        // 書かれた直後のダンプを消さない。並行して走っている別プロセスが、
        // まだ行を足していない可能性がある。
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

#[cfg(test)]
mod tests {
    use super::*;

    /// ロールバックジャーナルに落ちた世界でも、書き込み中のジャーナルが 0600 であること。
    ///
    /// `AILO_DUMP_DIR` が同期フォルダやネットワーク FS のときに実際に通る経路。
    /// `-wal` だけ守っていると、**一番守りが下がっている環境でだけ**穴が開く。
    /// ここは `Db::open` を通さず、その世界を直接作って確かめる。
    #[test]
    fn the_rollback_journal_is_private_too() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("history.db");
        let conn = Connection::open(&db).unwrap();
        conn.pragma_update(None, "journal_mode", "DELETE").unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO history (ts, method, url, status, ms, bytes, dump, url_template)
             VALUES ('t', 'GET', 'u', 200, 1, 2, 'd.json', 'https://x/{{api_key}}')",
        )
        .unwrap();
        // 書き込みトランザクションを開いたままにすると `-journal` が生きる。
        // **実際にページを書き換える UPDATE にすること。** 0 行に当たる UPDATE では
        // ジャーナルが作られず、テストが「無い」を「守れている」と読み違える。
        conn.execute_batch("BEGIN IMMEDIATE; UPDATE history SET status = 201;")
            .unwrap();
        let journal = sidecar(&db, "-journal");
        assert!(journal.exists(), "この経路で -journal が作られていない");
        lock_down(&db).unwrap();
        let mode = fs::metadata(&journal).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "-journal が {mode:o}");
    }

    /// 隔離した `AILO_DUMP_DIR` で走らせる。
    ///
    /// 環境変数はプロセス共有なので、この module のテストは直列でしか走れない。
    /// `cargo test` は同一プロセス内で並列に走るため、ロックで直列化する。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_dir<T>(f: impl FnOnce(&Path) -> T) -> T {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("AILO_DUMP_DIR", tmp.path());
        let out = f(tmp.path());
        std::env::remove_var("AILO_DUMP_DIR");
        out
    }

    /// 退避済みのファイル。名前は一意なので、数えるには走査が要る。
    fn retired_files(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("migrated"))
            .map(|e| e.path())
            .collect()
    }

    fn entry(dump: &str, ts: &str) -> IndexEntry {
        IndexEntry {
            ts: ts.into(),
            name: Some("login".into()),
            env: Some("stg".into()),
            method: "POST".into(),
            url: "https://api.example.com/auth?key=***".into(),
            status: 200,
            ms: 12,
            bytes: 34,
            dump: dump.into(),
        }
    }

    fn append(db: &Db, dump: &str, ts: &str) {
        db.append(&NewRow {
            entry: &entry(dump, ts),
            url_template: "https://api.example.com/auth?key={{api_key}}",
            items_template: &["Authorization: Bearer {{token}}".to_string()],
            raw_template: None,
            form: false,
            redacted: &[],
        })
        .unwrap();
    }

    #[test]
    fn db_and_wal_files_exist_and_are_private() {
        with_dir(|dir| {
            let db = Db::open().unwrap();
            append(&db, "a.json", "2026-09-13T00:00:00Z");
            let main = dir.join("history.db");
            // **存在も確かめる。** 無いファイルへの chmod は黙って通るので、
            // 権限だけ見ていると「塞いだ」と「一度も作られていない」を見分けられない。
            for p in [main.clone(), sidecar(&main, "-wal"), sidecar(&main, "-shm")] {
                assert!(p.exists(), "{} が無い", p.display());
                let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{} が {:o}", p.display(), mode);
            }
        });
    }

    /// 取り込みが小分けになっていること。1 つの大きなトランザクションにすると、
    /// その間に走った本物のリクエストの索引書き込みが待たされて落ちる。
    #[test]
    fn migration_commits_in_chunks_so_the_write_lock_is_never_held_long() {
        with_dir(|dir| {
            let lines: String = (0..MIGRATE_CHUNK * 2 + 7)
                .map(|i| {
                    serde_json::to_string(&entry(&format!("d{i}.json"), "2026-09-13T00:00:00Z"))
                        .unwrap()
                        + "\n"
                })
                .collect();
            fs::write(dir.join("index.jsonl"), &lines).unwrap();
            let db = Db::open().unwrap();
            // 端数を落とさない(`chunks` の最後の 1 枚を取りこぼす書き方をしていないか)。
            assert_eq!(db.recent(9999).unwrap().len(), MIGRATE_CHUNK * 2 + 7);
        });
    }

    /// 取り込みに失敗しても `Db::open` は通り、次回また拾えること。
    ///
    /// ここで `?` を返していた頃は、1 本の断面が読めないだけで `ailo log` /
    /// `ailo show` が丸ごと使えなくなり、`prune_bodies` は索引に行が入る経路の
    /// 中にしか無いので**保持期間の掃除まで止まっていた**。
    #[test]
    fn a_failed_import_does_not_take_the_history_down_with_it() {
        with_dir(|dir| {
            let snap = dir.join(format!("{MIGRATING_PREFIX}9999-dead"));
            fs::write(&snap, "{}\n").unwrap();
            // 読めない断面にする。
            fs::set_permissions(&snap, fs::Permissions::from_mode(0o000)).unwrap();

            let db = Db::open().expect("取り込みの失敗で履歴ごと開けなくなっている");
            assert!(
                db.warnings.iter().any(|w| w.contains("取り込めません")),
                "黙って失敗している: {:?}",
                db.warnings
            );
            // 断面は残っている＝次回やり直せる。
            assert!(snap.exists(), "取り込めなかった断面が消えている");
            fs::set_permissions(&snap, fs::Permissions::from_mode(0o600)).unwrap();
        });
    }

    #[test]
    fn order_is_by_rowid_not_timestamp() {
        with_dir(|_| {
            let db = Db::open().unwrap();
            // 同じ秒に 2 件。`ts` で並べると順序が決まらず、`log` の番号と
            // `show` の番号がずれて、Enter で違うものが送られる。
            append(&db, "first.json", "2026-09-13T00:00:00Z");
            append(&db, "second.json", "2026-09-13T00:00:00Z");
            let rows = db.recent(10).unwrap();
            assert_eq!(rows[0].dump, "second.json");
            assert_eq!(rows[1].dump, "first.json");
        });
    }

    #[test]
    fn migration_is_repeatable_and_retires_the_jsonl() {
        with_dir(|dir| {
            let lines: String = ["a.json", "b.json"]
                .iter()
                .map(|d| serde_json::to_string(&entry(d, "2026-09-13T00:00:00Z")).unwrap() + "\n")
                .collect();
            fs::write(dir.join("index.jsonl"), &lines).unwrap();

            let db = Db::open().unwrap();
            assert_eq!(db.recent(99).unwrap().len(), 2);
            assert!(!dir.join("index.jsonl").exists());
            assert_eq!(retired_files(dir).len(), 1);

            // 旧バイナリが走って JSONL が生え直しても、もう一度取り込める。
            // 重複は `dump` の UNIQUE と `INSERT OR IGNORE` が吸う。
            fs::write(dir.join("index.jsonl"), &lines).unwrap();
            drop(db);
            let db = Db::open().unwrap();
            assert_eq!(db.recent(99).unwrap().len(), 2, "重複して取り込まれた");
        });
    }

    /// 壊れた行は隔離して残す。**握りつぶさないが、履歴ごと殺しもしない。**
    ///
    /// 落としていた頃は、1 行の破損で `ailo log` / `show` が恒久的に exit 2 になり、
    /// さらに `prune_bodies` は索引に行が入る経路の中にしか無いので、
    /// **保持期間の掃除まで止まって認証情報入りのダンプが無期限に残っていた。**
    #[test]
    fn a_broken_line_is_quarantined_without_bricking_the_history() {
        with_dir(|dir| {
            let good = serde_json::to_string(&entry("a.json", "2026-09-13T00:00:00Z")).unwrap();
            fs::write(dir.join("index.jsonl"), format!("{good}\n{{\"ts\":\n")).unwrap();

            let db = Db::open().expect("壊れた行 1 つで履歴が開けなくなっている");
            // 読めた行は入っている。
            assert_eq!(db.recent(99).unwrap().len(), 1);
            // 壊れた行は捨てず、隔離して残す。
            let quarantined: Vec<_> = fs::read_dir(dir)
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().contains("broken"))
                .collect();
            assert_eq!(quarantined.len(), 1, "壊れた行が隔離されていない");
            assert!(fs::read_to_string(quarantined[0].path())
                .unwrap()
                .contains("{\"ts\":"));
            // 何行目が読めなかったかを言う。
            assert!(
                db.warnings.iter().any(|w| w.contains("2 行目")),
                "行番号が出ていない: {:?}",
                db.warnings
            );
        });
    }

    /// 取り込みの最中に**古い版のバイナリ**が追記した行を失わない。
    ///
    /// 旧版は `migrate.lock` を知らず `O_APPEND` で書く。読んでから rename すると
    /// その間の追記が退避ファイルへ流れ、二度と拾われない。先に切り離せば、
    /// 以後の追記は新しい `index.jsonl` に載り、次回そのまま取り込まれる。
    #[test]
    fn a_row_appended_by_an_old_binary_during_migration_is_not_lost() {
        with_dir(|dir| {
            let index = dir.join("index.jsonl");
            fs::write(
                &index,
                serde_json::to_string(&entry("first.json", "2026-09-13T00:00:00Z")).unwrap() + "\n",
            )
            .unwrap();
            let db = Db::open().unwrap();
            assert_eq!(db.recent(99).unwrap().len(), 1);
            drop(db);

            // 旧版が、移行後に追記した（＝新しい `index.jsonl` ができる）。
            fs::write(
                &index,
                serde_json::to_string(&entry("late.json", "2026-09-13T00:00:01Z")).unwrap() + "\n",
            )
            .unwrap();
            let db = Db::open().unwrap();
            let dumps: Vec<String> = db.recent(99).unwrap().into_iter().map(|r| r.dump).collect();
            assert!(dumps.contains(&"late.json".to_string()), "{dumps:?}");
            assert_eq!(dumps.len(), 2);
        });
    }

    /// 2 回目の移行が、1 回目の退避ファイルを黙って上書きしない。
    ///
    /// 固定名 `index.jsonl.migrated` だった頃は上書きしていた。取りこぼした行が
    /// そこにしか残っていなかった場合、ディスクからも消えていた。
    #[test]
    fn retiring_never_overwrites_an_earlier_retired_file() {
        with_dir(|dir| {
            for dump in ["one.json", "two.json"] {
                fs::write(
                    dir.join("index.jsonl"),
                    serde_json::to_string(&entry(dump, "2026-09-13T00:00:00Z")).unwrap() + "\n",
                )
                .unwrap();
                drop(Db::open().unwrap());
            }
            assert_eq!(
                retired_files(dir).len(),
                2,
                "退避ファイルが上書きされている"
            );
        });
    }

    /// 切り離したあとに落ちた断面を、次の起動が拾う。
    #[test]
    fn a_snapshot_left_behind_by_a_crash_is_picked_up_next_time() {
        with_dir(|dir| {
            // 「rename は済んだが取り込み前に落ちた」状態を直接作る。
            fs::write(
                dir.join(format!("{MIGRATING_PREFIX}9999-abc")),
                serde_json::to_string(&entry("orphaned.json", "2026-09-13T00:00:00Z")).unwrap()
                    + "\n",
            )
            .unwrap();
            let db = Db::open().unwrap();
            assert_eq!(
                db.recent(99).unwrap()[0].dump,
                "orphaned.json",
                "置き去りの断面が拾われていない"
            );
        });
    }

    #[test]
    fn pruning_marks_the_row_and_removes_only_the_body() {
        with_dir(|dir| {
            let db = Db::open().unwrap();
            for i in 0..5 {
                let name = format!("d{i}.json");
                fs::write(dir.join(&name), "{}").unwrap();
                append(&db, &name, "2026-09-13T00:00:00Z");
            }
            db.prune_bodies(&Retention {
                keep_count: 2,
                keep_days: 3650,
            })
            .unwrap();

            let rows = db.recent(99).unwrap();
            assert_eq!(rows.len(), 5, "行は消さない");
            let gone: Vec<&str> = rows
                .iter()
                .filter(|r| r.body_deleted)
                .map(|r| r.dump.as_str())
                .collect();
            assert_eq!(gone, ["d2.json", "d1.json", "d0.json"]);
            assert!(!dir.join("d0.json").exists());
            assert!(dir.join("d4.json").exists());
        });
    }

    #[test]
    fn orphan_sweeping_does_not_go_blind_once_rows_outlive_their_bodies() {
        with_dir(|dir| {
            let db = Db::open().unwrap();
            // 本文が既に消えている行。参照集合に入れてしまうと、同名の
            // 迷子ファイルが保持期間を超えて永久に残る。
            fs::write(dir.join("orphan.json"), "{}").unwrap();
            append(&db, "orphan.json", "2026-09-13T00:00:00Z");
            db.conn
                .execute("UPDATE history SET body_deleted = 1", [])
                .unwrap();
            // mtime を保持期間より古くする。
            let old = SystemTime::now() - Duration::from_secs(30 * 24 * 3600);
            fs::File::open(dir.join("orphan.json"))
                .unwrap()
                .set_modified(old)
                .unwrap();

            db.prune_bodies(&Retention::default()).unwrap();
            assert!(
                !dir.join("orphan.json").exists(),
                "本文が消えた行を参照集合に入れており、実体側の掃除が空振りしている"
            );
        });
    }

    #[test]
    fn templates_are_stored_but_never_selected_by_the_read_path() {
        with_dir(|_| {
            let db = Db::open().unwrap();
            append(&db, "a.json", "2026-09-13T00:00:00Z");
            // 書かれてはいる(再送と「定義として保存」がこれを要求する)。
            let t: String = db
                .conn
                .query_row("SELECT url_template FROM history", [], |r| r.get(0))
                .unwrap();
            assert!(t.contains("{{api_key}}"));
            // `recent` の返す型にテンプレートは無い。`query` の外では、
            // 読み出し側が列名を明示していることが守りになる。
            let row = &db.recent(1).unwrap()[0];
            assert_eq!(row.url, "https://api.example.com/auth?key=***");
        });
    }

    #[test]
    fn index_entry_fields_survive_the_round_trip() {
        with_dir(|_| {
            let db = Db::open().unwrap();
            let e = IndexEntry {
                name: None,
                env: None,
                bytes: 700_000,
                ..entry("x.json", "2026-09-13T01:02:03Z")
            };
            db.append(&NewRow {
                entry: &e,
                url_template: "u",
                items_template: &[],
                raw_template: Some("{\"a\":1}"),
                form: true,
                redacted: &["password".into()],
            })
            .unwrap();
            let r = &db.recent(1).unwrap()[0];
            assert_eq!((r.name.as_deref(), r.env.as_deref()), (None, None));
            assert_eq!(r.bytes, 700_000);
            assert_eq!(r.ts, "2026-09-13T01:02:03Z");
            assert!(!r.body_deleted);
        });
    }
}
