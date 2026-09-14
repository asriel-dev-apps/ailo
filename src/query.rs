//! `ailo query "select ..."` — 履歴 DB への読み取り専用の SQL。
//!
//! **この口は次の 5 つが揃って初めて出す。**
//! 1 つでも欠けたら出さない。`open_hardened` がどれかを課せなかったら、問い合わせを走らせずに落とす。
//!
//! 1. `SQLITE_LIMIT_ATTACHED` を 0。読み取り専用の接続からでも `ATTACH` で別 DB を読める(実測)。
//!    塞がないと「任意の SQLite ファイルを標準出力に流す口」になり、`show` のパス検査を迂回できる
//! 2. `prepare()` の単文のみ。`execute_batch` は複文を弾かない
//! 3. authorizer。**既定は Deny**。許すのは `history` 表の見える列の `Read` と `SELECT`・関数・
//!    再帰 CTE だけ。テンプレート列は `Ignore`(＝全経路で NULL)。**列も既定で隠す**:
//!    `VISIBLE` に無い列は、後から足された列も含めて NULL になる
//! 4. `PRAGMA query_only=1` と `SQLITE_DBCONFIG_DEFENSIVE`(接続も読み取り専用で開く)
//! 5. 行数・出力バイト数・実行時間・列数・1 行の大きさの上限。行数とバイト数で超えたら
//!    全体をファイルへ落とし、標準出力は先頭だけ
//!
//! エラーはここの境界を通す。SQL 全文・列の値・絶対パスを反響させない。

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rusqlite::config::DbConfig;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::limits::Limit;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, ErrorCode, OpenFlags};

use crate::paths;

/// query から値が見える列。**ここに無い列は NULL になる。**
///
/// 並びは `--schema` の出力順。説明はエージェントが読む前提で短く。
pub const VISIBLE: &[(&str, &str)] = &[
    ("id", "INTEGER  追加順の連番。大きいほど新しい"),
    ("ts", "TEXT     送信時刻 (RFC 3339, UTC)"),
    (
        "name",
        "TEXT     保存済みリクエストの名前。アドホックなら NULL",
    ),
    ("env", "TEXT     環境名。無ければ NULL"),
    ("method", "TEXT     GET / POST ..."),
    ("url", "TEXT     送った URL (秘匿値はマスク済み)"),
    ("status", "INTEGER  HTTP ステータス"),
    ("ms", "INTEGER  所要ミリ秒"),
    ("bytes", "INTEGER  レスポンス本文のバイト数"),
    ("dump", "TEXT     ダンプのファイル名 (`ailo show <dump>`)"),
    (
        "body_deleted",
        "INTEGER  1 なら本文は保持期間切れで消えている",
    ),
    ("form", "INTEGER  1 なら form で送った"),
    (
        "redacted",
        "TEXT     直書きの秘匿値として落とした項目名の JSON 配列",
    ),
];

/// 標準出力に載せる上限。超えたら全体をファイルへ落とす。
const STDOUT_ROWS: usize = 100;
const STDOUT_BYTES: usize = 16 * 1024;
/// ファイルに落とす分の上限。ここも超えたら打ち切る。
const FILE_BYTES: u64 = 64 * 1024 * 1024;
/// 1 問い合わせの実行時間。
const TIME_LIMIT: Duration = Duration::from_secs(5);
/// 1 つの値の長さ。`randomblob(1e9)` のような 1 式での巨大な確保を塞ぐ。
const VALUE_BYTES: i32 = 1024 * 1024;
/// 結果の列数。値の上限と掛け合わせたものが 1 行の確保の上限になる。
const MAX_COLUMNS: i32 = 100;
/// 1 行の本文の合計。列を並べて値の上限を掛け算させない。
const ROW_BYTES: usize = 8 * 1024 * 1024;

/// 結果を出し切れなかった。行数・バイト数・時間のどれでも同じコード。
pub const TRUNCATED: i32 = 4;

/// `ailo query --schema`。定数から出す(`PRAGMA` は authorizer が通さない)。
pub fn schema() -> String {
    let mut out = String::from("history\n");
    for (name, desc) in VISIBLE {
        out.push_str(&format!("  {name:<13} {desc}\n"));
    }
    out
}

/// 問い合わせを走らせ、終了コードを返す。エラー文はここで作り、呼ぶ側では飾らない。
///
/// **どの失敗も `?` で main へ流さない。** main は原因の連鎖を全部出すので、
/// パスを含む文脈がそのまま標準エラーに出る。
pub fn run(sql: &str) -> i32 {
    // 取り込みとスキーマ作成は書き込みの接続でやる。`ailo log` と同じ答えにするため
    // (まだ取り込んでいない `index.jsonl` の分が query にだけ無い、を作らない)。
    // **SQL からは何も書けない**: 書き込むのはこの初期化だけで、`ailo log` と同じもの。
    // **開いたままにしておく。** 最後の接続が閉じると `-shm` が消え、
    // 読み取り専用の接続は WAL の DB を開けなくなる。
    let opened = crate::history::db_path().and_then(|p| Ok((p, crate::history::Db::open()?)));
    let (path, db) = match opened {
        Ok(v) => v,
        Err(_) => {
            eprintln!("エラー: 履歴 DB を開けません。`ailo log` で原因を確認してください");
            return 2;
        }
    };
    for w in &db.warnings {
        eprintln!("{w}");
    }

    let conn = match open_hardened(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "エラー: 履歴 DB を問い合わせ用に開けません: {}",
                explain(&e)
            );
            return 2;
        }
    };

    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("エラー: {}", explain(&e));
            return 2;
        }
    };
    // authorizer で足りているはずだが、書き換える文なら走らせない。
    if !stmt.readonly() {
        eprintln!("エラー: {DENIED}");
        return 2;
    }
    // 空文字やコメントだけの SQL は、列の無い「文」として prepare を通ってしまう。
    if stmt.column_count() == 0 {
        eprintln!("エラー: 結果を返す SELECT を 1 文渡してください");
        return 2;
    }

    let columns: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
    let mut rows = match stmt.query([]) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("エラー: {}", explain(&e));
            return 2;
        }
    };
    let mut out = Output::new(&columns);
    let stopped = loop {
        match rows.next() {
            Ok(Some(row)) => {
                let values: Vec<ValueRef<'_>> = (0..columns.len())
                    .map(|i| row.get_ref(i).unwrap_or(ValueRef::Null))
                    .collect();
                // **JSON に組み立てる前に測る。** 1 つの値は `VALUE_BYTES` までだが、
                // 列を並べれば 1 行は列数倍になる。組み立ててから測ると、その前に確保が済む。
                let raw: usize = values
                    .iter()
                    .map(|v| match v {
                        ValueRef::Text(t) => t.len(),
                        _ => 0,
                    })
                    .sum();
                if raw > ROW_BYTES {
                    break Some(format!(
                        "1 行の大きさの上限({} MiB)を超えたので打ち切りました",
                        ROW_BYTES / 1024 / 1024
                    ));
                }
                let line = serde_json::Value::Array(values.into_iter().map(to_json).collect());
                if !out.push(line.to_string()) {
                    break Some(format!(
                        "ファイルの上限({} MiB)に達したので打ち切りました",
                        FILE_BYTES / 1024 / 1024
                    ));
                }
            }
            Ok(None) => break None,
            Err(e) if e.sqlite_error_code() == Some(ErrorCode::OperationInterrupted) => {
                break Some(format!(
                    "実行時間の上限({} 秒)に達したので打ち切りました",
                    TIME_LIMIT.as_secs()
                ));
            }
            // **途中まで出した行を残さない。** 標準出力へはまだ何も書いていない。
            // 先頭だけの結果を「全部」と読み違えさせないため。
            Err(e) => {
                eprintln!("エラー: {}", explain(&e));
                return 2;
            }
        }
    };
    drop(rows);
    drop(db);
    out.finish(stopped)
}

/// 5 つの守りを課した読み取り専用の接続。**どれか 1 つでも課せなければ Err。**
fn open_hardened(path: &std::path::Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
    conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, VALUE_BYTES)?;
    conn.set_limit(Limit::SQLITE_LIMIT_COLUMN, MAX_COLUMNS)?;
    let defensive = conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    // PRAGMA と sqlite_master は authorizer が通さないので、その前に済ませる。
    conn.pragma_update(None, "query_only", true)?;
    let query_only: bool = conn.pragma_query_value(None, "query_only", |r| r.get(0))?;
    // **課したつもりで課せていない、を黙って通さない。**
    if !defensive || !query_only || conn.limit(Limit::SQLITE_LIMIT_ATTACHED)? != 0 {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let schema_names: Vec<String> = conn
        .prepare("SELECT lower(name) FROM sqlite_master")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let deadline = Instant::now() + TIME_LIMIT;
    conn.progress_handler(1000, Some(move || Instant::now() > deadline))?;
    conn.authorizer(Some(move |ctx: AuthContext<'_>| {
        authorize(&schema_names, ctx)
    }))?;
    Ok(conn)
}

/// `schema_names` は DB に実在する表・索引の名前(小文字)。
fn authorize(schema_names: &[String], ctx: AuthContext<'_>) -> Authorization {
    match ctx.action {
        AuthAction::Select | AuthAction::Recursive => Authorization::Allow,
        // ファイルに触る唯一の組み込み関数。実行時は既定で無効だが、その前提に依存しない。
        AuthAction::Function { function_name } => {
            if function_name.eq_ignore_ascii_case("load_extension") {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }
        AuthAction::Read {
            table_name: "history",
            column_name,
        } => {
            if VISIBLE.iter().any(|(c, _)| *c == column_name) {
                Authorization::Allow
            } else {
                Authorization::Ignore
            }
        }
        // 引数の JSON を展開するだけの表値関数。DB の中身には触らない。
        AuthAction::Read {
            table_name: "json_each" | "json_tree",
            ..
        } => Authorization::Allow,
        // 列を 1 つも読まない参照(`count(*)`)。CTE を数えるときに表名 = CTE 名で来る。
        // authorizer からは CTE と本物の表を見分けられない(どちらも database 名が無い)ので、
        // **本物の表と組み込みの仮想表の名前なら拒む**。行数だけでも、他の表の中身の
        // 手がかりになる(`count(*) from sqlite_master` や `dbstat`)。
        AuthAction::Read {
            table_name,
            column_name: "",
        } => {
            let t = table_name.to_ascii_lowercase();
            let builtin = t.starts_with("sqlite_") || t.starts_with("pragma_") || t == "dbstat";
            if builtin || schema_names.contains(&t) {
                Authorization::Deny
            } else {
                Authorization::Ignore
            }
        }
        // 他の表(sqlite_master・dbstat・pragma 表など)、書き込み、ATTACH、PRAGMA、トランザクション。
        _ => Authorization::Deny,
    }
}

const DENIED: &str =
    "許可されていない操作です。読めるのは history 表への SELECT だけです(書き込み・ATTACH・PRAGMA・他の表は不可)";

/// SQLite のエラーを、SQL 全文・値・パスを含まない文に写す。
///
/// rusqlite の `SqlInputError` は Display に **SQL 全文**を含むので、素で出さない。
/// SQLite の文言のうち、SQL から識別子を反響するものは**識別子が素直な綴りのときだけ**
/// 残す(`"/private/x"` のように引用符で何でも識別子にできる)。
/// 生の SQL を選んだ利点「列名を間違えればエラーになる」には `no such column: x` が要る。
fn explain(e: &rusqlite::Error) -> String {
    use rusqlite::Error as E;
    let msg = match e {
        E::MultipleStatement => return "1 回に実行できるのは 1 文だけです".into(),
        E::SqlInputError { msg, .. } => Some(msg.as_str()),
        E::SqliteFailure(_, msg) => msg.as_deref(),
        _ => None,
    };
    match e.sqlite_error_code() {
        Some(ErrorCode::AuthorizationForStatementDenied) | Some(ErrorCode::ReadOnly) => {
            return DENIED.into()
        }
        Some(ErrorCode::OperationInterrupted) => {
            return format!("実行時間の上限({} 秒)に達しました", TIME_LIMIT.as_secs())
        }
        _ => {}
    }
    let plain = |s: &str| {
        !s.is_empty()
            && s.len() <= 64
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    };
    let m = msg.unwrap_or_default();
    if let Some(rest) = m.strip_prefix("near \"") {
        return match rest.split_once("\": syntax error") {
            Some((tok, "")) if plain(tok) => format!("near \"{tok}\": syntax error"),
            _ => "syntax error".into(),
        };
    }
    for p in [
        "no such column",
        "no such table",
        "no such function",
        "ambiguous column name",
        "unrecognized token",
    ] {
        if let Some(rest) = m.strip_prefix(p).and_then(|r| r.strip_prefix(": ")) {
            return if plain(rest) {
                format!("{p}: {rest}")
            } else {
                p.into()
            };
        }
    }
    // 識別子を含みうる残りは、決まった頭だけを出す。
    for p in [
        "incomplete input",
        "wrong number of arguments to function",
        "misuse of aggregate",
        "too many columns",
        "string or blob too big",
    ] {
        if m.starts_with(p) {
            return p.into();
        }
    }
    match e.sqlite_error_code() {
        Some(code) => format!("SQL を実行できません({code:?})"),
        None => "SQL を実行できません".into(),
    }
}

fn to_json(v: ValueRef<'_>) -> serde_json::Value {
    match v {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(i) => i.into(),
        ValueRef::Real(f) => f.into(),
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned().into(),
        ValueRef::Blob(b) => format!("<blob {} bytes>", b.len()).into(),
    }
}

/// 標準出力は先頭の行だけ。上限を超えた時点で全体をファイルへ切り替える。
///
/// **標準出力へは最後にまとめて書く。** 途中で SQL が失敗したとき、
/// 先頭だけの結果を完全な答えに見せないため。溜めるのは上限の範囲だけなので小さい。
///
/// 1 行目は列名の JSON 配列、以降は 1 行 1 JSON 配列。ファイルは
/// `{"columns":[...],"rows":[...]}` で、`.json` なので本文と同じ保持期間で掃除される。
struct Output {
    header: String,
    head: Vec<String>,
    head_bytes: usize,
    rows: usize,
    file: Option<(PathBuf, BufWriter<File>, u64)>,
    file_failed: bool,
}

impl Output {
    fn new(columns: &[String]) -> Self {
        let header = serde_json::to_string(columns).unwrap_or_else(|_| "[]".into());
        Output {
            head_bytes: header.len() + 1,
            header,
            head: Vec::new(),
            rows: 0,
            file: None,
            file_failed: false,
        }
    }

    /// 1 行足す。これ以上続けられないなら false。
    fn push(&mut self, line: String) -> bool {
        self.rows += 1;
        if self.file.is_none() {
            if self.rows <= STDOUT_ROWS && self.head_bytes + line.len() < STDOUT_BYTES {
                self.head_bytes += line.len() + 1;
                self.head.push(line);
                return true;
            }
            if self.spill().is_err() {
                // 全体を残せない。数えるだけ続けても答えは出せないので止める。
                self.file_failed = true;
                return false;
            }
        }
        let Some((_, w, written)) = &mut self.file else {
            return false;
        };
        let sep = if self.rows == 1 { "\n" } else { ",\n" };
        let n = (sep.len() + line.len()) as u64;
        if *written + n > FILE_BYTES {
            return false;
        }
        *written += n;
        w.write_all(sep.as_bytes()).is_ok() && w.write_all(line.as_bytes()).is_ok()
    }

    /// ここまでに溜めた行ごと、ファイルへ移る。
    fn spill(&mut self) -> std::io::Result<()> {
        let dir = paths::dumps_dir().map_err(std::io::Error::other)?;
        let stamp = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
        let path = dir.join(format!("query-{stamp}-{}.json", std::process::id()));
        let f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)?;
        let mut w = BufWriter::new(f);
        let head = format!("{{\"columns\":{},\"rows\":[", self.header);
        w.write_all(head.as_bytes())?;
        let mut written = head.len() as u64;
        for (i, line) in self.head.iter().enumerate() {
            let sep = if i == 0 { "\n" } else { ",\n" };
            w.write_all(sep.as_bytes())?;
            w.write_all(line.as_bytes())?;
            written += (sep.len() + line.len()) as u64;
        }
        self.file = Some((path, w, written));
        Ok(())
    }

    fn finish(self, stopped: Option<String>) -> i32 {
        println!("{}", self.header);
        for line in &self.head {
            println!("{line}");
        }
        let shown = self.head.len();
        let limits = format!("{STDOUT_ROWS} 行 / {} KiB", STDOUT_BYTES / 1024);
        if self.file_failed {
            eprintln!(
                "結果が上限({limits})を超えました。先頭 {shown} 行だけ出しています。全体を保存するファイルを作れません"
            );
            return TRUNCATED;
        }
        let Some((path, mut w, _)) = self.file else {
            if let Some(why) = stopped {
                eprintln!("{why}。出したのは {shown} 行です");
                return TRUNCATED;
            }
            return 0;
        };
        let tail = if stopped.is_some() {
            "\n],\"truncated\":true}\n"
        } else {
            "\n]}\n"
        };
        // ファイル名だけを出す。場所は `ailo show` が知っている。
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if w.write_all(tail.as_bytes())
            .and_then(|_| w.flush())
            .is_err()
        {
            eprintln!(
                "結果が上限({limits})を超えました。先頭 {shown} 行だけ出しています。全体を {name} に書き切れませんでした"
            );
            return TRUNCATED;
        }
        let saved = match &stopped {
            Some(why) => format!("{why}。それまでの {} 行", self.rows),
            None => format!("全 {} 行", self.rows),
        };
        eprintln!(
            "結果が上限({limits})を超えたので、先頭 {shown} 行だけ出しました。{saved}を保存しました(`ailo show {name}`)"
        );
        TRUNCATED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本物のスキーマで行を 3 つ持つ DB に、守りを課した接続を開く。
    fn fixture() -> (tempfile::TempDir, Connection) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.db");
        let w = Connection::open(&path).unwrap();
        w.execute_batch(crate::history::SCHEMA).unwrap();
        for (i, status) in [200, 404, 500].iter().enumerate() {
            w.execute(
                "INSERT INTO history (ts, method, url, status, ms, bytes, dump,
                 url_template, items_template, raw_template)
                 VALUES ('2026-09-14T00:00:00Z', 'GET', 'https://x/?k=***', ?1, 10, 20, ?2,
                 'https://x/?k={{secret_tpl}}', '[\"A: {{secret_tpl}}\"]', 'raw-secret_tpl')",
                rusqlite::params![status, format!("d{i}.json")],
            )
            .unwrap();
        }
        let conn = open_hardened(&path).unwrap();
        (tmp, conn)
    }

    fn all(conn: &Connection, sql: &str) -> rusqlite::Result<Vec<Vec<serde_json::Value>>> {
        let mut stmt = conn.prepare(sql)?;
        let n = stmt.column_count();
        let rows = stmt
            .query_map([], |r| {
                Ok((0..n).map(|i| to_json(r.get_ref(i).unwrap())).collect())
            })?
            .collect();
        rows
    }

    /// テンプレートがどの読み方をしても出てこないこと。値そのものも、盲目オラクルとしても。
    #[test]
    fn template_columns_read_as_null_on_every_path() {
        let (_t, conn) = fixture();
        for sql in [
            "select url_template, items_template, raw_template from history",
            "select * from history",
            "select group_concat(url_template) from history",
            "select length(raw_template), typeof(url_template) from history",
            "select (select url_template from history limit 1)",
            "select count(*) from history where url_template like '%secret_tpl%'",
            "select count(*) from history where instr(items_template, 'secret_tpl') > 0",
            "select url_template from history order by url_template",
            "with t as (select * from history) select raw_template from t",
            "select h.url_template from history as h",
            "select URL_TEMPLATE from HISTORY",
            "select main.history.url_template from main.history",
            "select \"url_template\" from \"history\"",
            "select value from json_each((select items_template from history limit 1))",
        ] {
            let text = serde_json::to_string(&all(&conn, sql).unwrap()).unwrap();
            assert!(!text.contains("secret_tpl"), "{sql} → {text}");
            // 盲目オラクル: 条件がテンプレートに当たっているなら件数は 3。
            assert!(!text.contains("[3]"), "{sql} が件数で中身を漏らす → {text}");
        }
        // コントロール: 同じ DB に確かに書いてある。
        let raw = Connection::open(conn.path().unwrap()).unwrap();
        let t: String = raw
            .query_row("select url_template from history limit 1", [], |r| r.get(0))
            .unwrap();
        assert!(t.contains("secret_tpl"));
    }

    /// 通す側。普通の集計が壊れていないこと。
    #[test]
    fn ordinary_analysis_still_works() {
        let (_t, conn) = fixture();
        let cases: &[(&str, &str)] = &[
            ("select count(*) from history", "[[3]]"),
            (
                "select status, count(*) from history group by status order by status",
                "[[200,1],[404,1],[500,1]]",
            ),
            (
                "select round(avg(status >= 400) * 100) from history",
                "[[67.0]]",
            ),
            ("select max(id) - min(id) from history", "[[2]]"),
            ("select dump from history where status = 404", "[[\"d1.json\"]]"),
            (
                "with recursive n(x) as (select 1 union all select x + 1 from n where x < 3) select sum(x) from n",
                "[[6]]",
            ),
            (
                "select substr(ts, 1, 10), count(*) from history group by 1",
                "[[\"2026-09-14\",3]]",
            ),
            ("select rowid from history where id = 1", "[[1]]"),
            ("select STATUS from HISTORY where ID = 1", "[[200]]"),
            ("select main.history.status from main.history where id = 1", "[[200]]"),
            (
                "select count(*) from history a join history b on a.id = b.id",
                "[[3]]",
            ),
            (
                "select json_array_length(coalesce(redacted, '[]')) from history limit 1",
                "[[0]]",
            ),
            (
                "with recursive n(x) as (select 1 union all select x + 1 from n where x < 5) select count(*) from n",
                "[[5]]",
            ),
            ("select count(*) from json_each('[1,2]')", "[[2]]"),
            (
                "with t as materialized (select id from history) select count(*) from t",
                "[[3]]",
            ),
            ("select sum(value) from json_each('[1,2]')", "[[3]]"),
        ];
        for (sql, want) in cases {
            let got = serde_json::to_string(&all(&conn, sql).unwrap()).unwrap();
            assert_eq!(&got, want, "{sql}");
        }
    }

    /// 塞ぐ側。どれも authorizer か上限か単文の検査で失敗すること。
    #[test]
    fn everything_but_reading_history_is_refused() {
        let (tmp, conn) = fixture();
        let other = tmp.path().join("other.db");
        Connection::open(&other)
            .unwrap()
            .execute_batch("create table t(x); insert into t values ('other-db-content');")
            .unwrap();
        let attach = format!("attach '{}' as x", other.display());
        for sql in [
            attach.as_str(),
            "delete from history",
            "update history set status = 1",
            "insert into history (ts) values ('x')",
            "drop table history",
            "create temp table t(x)",
            "pragma query_only = 0",
            "pragma writable_schema = 1",
            "select * from sqlite_master",
            "select * from pragma_table_info('history')",
            "select * from dbstat",
            "select count(*) from sqlite_master",
            "select count(*) from sqlite_schema",
            "select count(*) from SQLITE_MASTER",
            "select count(*) from dbstat",
            "select count(*) from pragma_table_info('history')",
            "select count(*) from history_ts",
            "select load_extension('/tmp/x')",
            "begin",
            "vacuum",
            "vacuum into '/tmp/ailo-vacuum-probe.db'",
        ] {
            let result = all(&conn, sql);
            assert!(result.is_err(), "{sql} が通った: {result:?}");
        }
        assert!(
            !std::path::Path::new("/tmp/ailo-vacuum-probe.db").exists(),
            "vacuum into がファイルを作った"
        );
        // 複文は prepare が弾く。
        assert!(matches!(
            conn.prepare("select 1; select 2"),
            Err(rusqlite::Error::MultipleStatement)
        ));
        // コントロール: 同じ ATTACH は守りの無い接続なら通る(＝上の失敗は守りのおかげ)。
        let bare =
            Connection::open_with_flags(conn.path().unwrap(), OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        bare.execute_batch(&attach).unwrap();
        let leaked: String = bare
            .query_row("select x from x.t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(leaked, "other-db-content");
    }

    /// 1 式で巨大な値を作らせない。
    #[test]
    fn a_single_huge_value_is_refused() {
        let (_t, conn) = fixture();
        assert!(all(&conn, "select length(randomblob(100000000))").is_err());
        assert!(all(&conn, "select length(randomblob(1000))").is_ok());
    }

    /// 1 つずつは上限内の値を列に並べても、列数で止まること。
    #[test]
    fn many_legal_values_cannot_multiply_into_a_huge_row() {
        let (_t, conn) = fixture();
        let wide = vec!["1"; MAX_COLUMNS as usize + 1].join(", ");
        assert!(all(&conn, &format!("select {wide}")).is_err());
        let ok = vec!["1"; MAX_COLUMNS as usize].join(", ");
        assert!(all(&conn, &format!("select {ok}")).is_ok());
    }

    #[test]
    fn errors_do_not_echo_the_sql_or_paths() {
        let (_t, conn) = fixture();
        for sql in [
            "select nope_col from history where url = 'literal-value-xyz'",
            "selec 'literal-value-xyz'",
            "select * from history where url = 'literal-value-xyz' and",
            "attach '/private/abs/path/literal-value-xyz.db' as x",
            "select * from \"/private/literal-value-xyz\"",
            "select history.\"/private/literal-value-xyz\" from history",
            "select \"/private/literal-value-xyz\"() from history",
            "select 1 from history \"/private/literal-value-xyz\" \"x\"",
            "select count(*) from history where url = 'literal-value-xyz",
        ] {
            let err = conn.prepare(sql).map(|_| ()).unwrap_err();
            let shown = explain(&err);
            assert!(!shown.contains("literal-value-xyz"), "{sql} → {shown}");
            assert!(!shown.contains("/private"), "{sql} → {shown}");
        }
        // 列名の誤りは、列名つきで分かる。
        let err = conn.prepare("select nope_col from history").unwrap_err();
        assert_eq!(explain(&err), "no such column: nope_col");
    }

    /// 走り続ける問い合わせが時間の上限で止まること。
    #[test]
    fn a_runaway_query_is_interrupted() {
        let (_t, conn) = fixture();
        let deadline = Instant::now() + Duration::from_millis(200);
        conn.progress_handler(1000, Some(move || Instant::now() > deadline))
            .unwrap();
        let started = Instant::now();
        let err = all(
            &conn,
            "with recursive n(x) as (select 1 union all select x + 1 from n) select count(*) from n",
        )
        .unwrap_err();
        assert_eq!(
            err.sqlite_error_code(),
            Some(ErrorCode::OperationInterrupted)
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    /// `VISIBLE` と実際の表が食い違っていないこと。列を足したら、見せるかどうかを
    /// ここで決めることになる(足しただけなら NULL で隠れる)。
    #[test]
    fn every_column_is_either_visible_or_deliberately_hidden() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::history::SCHEMA).unwrap();
        let mut stmt = conn
            .prepare("select name from pragma_table_info('history')")
            .unwrap();
        let actual: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let hidden = ["url_template", "items_template", "raw_template"];
        for c in &actual {
            let visible = VISIBLE.iter().any(|(v, _)| v == c);
            assert!(
                visible ^ hidden.contains(&c.as_str()),
                "{c} を見せるか隠すか決めていない"
            );
        }
        assert_eq!(actual.len(), VISIBLE.len() + hidden.len());
    }
}
