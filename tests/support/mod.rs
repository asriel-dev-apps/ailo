//! 結合テストの土台。
//!
//! ユニットテストの fixture が「都合のよい形」しか持っていなかったせいで、実使用で
//! すぐ出るバグ(トップレベルが配列、ハイフン入りのキー、同名ヘッダの重複送信)が
//! 全部素通りした。そこで**実際に送って、サーバが受け取ったものを見る**土台を置く。
//!
//! httpbin を使わないのは、ネットワークに依存したテストは落ちる理由が増えるから。
//! 代わりに同じ性質(リクエストの反響)を持つ最小のサーバをその場で立てる。
//! wiremock / httpmock を入れないのは、ここで要るのが「宣言的なモック」ではなく
//! **受け取ったヘッダをそのまま本文に返すこと**だけで、依存を増やす釣り合いが
//! 取れないため。接続は 1 リクエストごとに閉じる(`Connection: close`)ので、
//! keep-alive とチャンクの面倒を持ち込まずに済む。

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Map, Value};

/// テスト用の HTTP サーバ。`drop` で止まる。
pub struct TestServer {
    addr: String,
    stop: Arc<AtomicBool>,
}

impl TestServer {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("ポートを開けない");
        let addr = listener.local_addr().unwrap().to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();

        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if flag.load(Ordering::Relaxed) {
                    break;
                }
                match conn {
                    Ok(stream) => {
                        std::thread::spawn(move || {
                            let _ = handle(stream);
                        });
                    }
                    Err(_) => break,
                }
            }
        });

        Self { addr, stop }
    }

    /// `path` は `/reflect` のように先頭スラッシュ込みで渡す。
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // accept を起こすためだけの接続。
        let _ = TcpStream::connect(&self.addr);
    }
}

struct Request {
    method: String,
    target: String,
    /// 受け取った順の (名前, 値)。名前は伝送されたまま(HTTP/1.1 では小文字)。
    headers: Vec<(String, String)>,
    body: String,
}

fn handle(mut stream: TcpStream) -> std::io::Result<()> {
    // **必ず時間で切る。** クライアント側の送り方が変わって(chunked など)
    // 宣言された本文が来なくなったとき、待ち続けると失敗ではなく**ハング**になる。
    // CI が無期限に止まるのは、テストが赤くなるより遥かにたちが悪い。
    let limit = std::time::Duration::from_secs(5);
    stream.set_read_timeout(Some(limit))?;
    stream.set_write_timeout(Some(limit))?;

    let Some(req) = read_request(&mut stream)? else {
        return Ok(());
    };

    let (path, query) = match req.target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (req.target.as_str(), ""),
    };

    let (status, content_type, body) = route(&req, path, query);

    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    let _ = stream.shutdown(Shutdown::Write);
    Ok(())
}

fn route(req: &Request, path: &str, query: &str) -> (&'static str, &'static str, String) {
    match path {
        // 一覧系の API。**トップレベルが配列**。`--pick '.[].title'` が要る形。
        "/list" => (
            "200 OK",
            "application/json",
            json!([
                {"id": 1, "title": "1 つめ", "author-name": "taro"},
                {"id": 2, "title": "2 つめ", "author-name": "hanako"}
            ])
            .to_string(),
        ),
        "/text" => ("200 OK", "text/plain; charset=utf-8", "ただの文章".into()),
        "/empty" => ("204 No Content", "text/plain", String::new()),
        "/notfound" => (
            "404 Not Found",
            "application/json",
            json!({"error": "no such thing"}).to_string(),
        ),
        // httpbin と同じく、受け取ったものをそのまま返す。マスク漏れと
        // ヘッダの重複はここでしか見えない。
        _ => (
            "200 OK",
            "application/json",
            reflection(req, path, query).to_string(),
        ),
    }
}

/// 受け取ったリクエストを JSON にして返す。
///
/// `headers` は httpbin に合わせて名前を `X-Tenant` の形に直し、値を `, ` で連結する。
/// 同名ヘッダを積んで送ってしまうと、ここが `application/json, text/csv` になる。
/// `header_values` は連結せずに配列で返すので、本数まで確かめられる。
fn reflection(req: &Request, path: &str, query: &str) -> Value {
    let mut joined: Map<String, Value> = Map::new();
    let mut values: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for (name, value) in &req.headers {
        values
            .entry(name.to_ascii_lowercase())
            .or_default()
            .push(value.clone());
    }
    for (name, list) in &values {
        joined.insert(title_case(name), json!(list.join(", ")));
    }

    json!({
        "method": req.method,
        "path": path,
        "query": query,
        "headers": Value::Object(joined),
        "header_values": values,
        "body": req.body,
    })
}

/// `x-tenant` → `X-Tenant`。httpbin が返す形に寄せる。
fn title_case(name: &str) -> String {
    name.split('-')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    if method.is_empty() {
        return Ok(None);
    }

    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 {
            break;
        }
        let h = h.trim_end_matches(['\r', '\n']);
        if h.is_empty() {
            break;
        }
        if let Some((name, value)) = h.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }

    // chunked は扱わない。**黙って空の本文として扱わず、ここで落とす。**
    // 対応していない送り方を「本文なし」と解釈すると、テストは緑のまま
    // 検査対象が消える。
    if headers
        .iter()
        .any(|(n, v)| n.eq_ignore_ascii_case("transfer-encoding") && v.contains("chunked"))
    {
        return Err(std::io::Error::other(
            "テストサーバは chunked を扱えない。送り方が変わったので土台を直すこと",
        ));
    }

    let len: usize = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut body)?;
    }

    Ok(Some(Request {
        method,
        target,
        headers,
        body: String::from_utf8_lossy(&body).to_string(),
    }))
}

// ------------------------------------------------------------------ 実行の隔離

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// 本物の `~/.config/ailo` を触らずに `ailo` を動かすための隔離ディレクトリ。
///
/// 環境変数は**全部落としてから**必要なものだけ入れる。親のシェルに
/// `AILO_VAR_*` や `AILO_SECRET_*` が残っていると、テストの結果がマシンごとに変わる。
/// 秘匿値の索引も空になるので、キーチェーンへは一度も触らない。
pub struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("一時ディレクトリを作れない");
        std::fs::create_dir_all(dir.path().join("config/ailo")).unwrap();
        std::fs::create_dir_all(dir.path().join("data/ailo")).unwrap();
        Self { dir }
    }

    pub fn config_dir(&self) -> PathBuf {
        self.dir.path().join("config/ailo")
    }

    pub fn data_dir(&self) -> PathBuf {
        self.dir.path().join("data/ailo")
    }

    pub fn write_config(&self, toml: &str) {
        std::fs::write(self.config_dir().join("config.toml"), toml).unwrap();
    }

    pub fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_ailo"));
        cmd.env_clear()
            .env("HOME", self.dir.path())
            .env("PATH", "/usr/bin:/bin")
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("XDG_DATA_HOME", self.dir.path().join("data"));
        cmd
    }

    pub fn run(&self, args: &[&str]) -> Run {
        Run::of(
            self.command()
                .args(args)
                .output()
                .expect("ailo を起動できない"),
        )
    }

    /// 保存された全ファイルを 1 本のテキストにする。
    ///
    /// 漏洩の監査は「どこに出たか」ではなく「どこかに出たか」を見る必要がある。
    /// ダンプ本体・索引・state を個別に見ていると、必ずどれかを見落とす。
    ///
    /// **UTF-8 として読めないファイルも捨てない。** 読めたものだけを対象にすると、
    /// 「全ファイルを見た」と言いながら一部を素通りさせることになる。
    pub fn all_stored_text(&self) -> String {
        let mut out = String::new();
        collect(&self.data_dir(), &mut out);
        collect(&self.config_dir(), &mut out);
        out
    }
}

fn collect(dir: &Path, out: &mut String) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if let Ok(bytes) = std::fs::read(&path) {
            out.push_str(&format!(
                "--- {}\n{}\n",
                path.display(),
                String::from_utf8_lossy(&bytes)
            ));
        }
    }
}

pub struct Run {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

impl Run {
    /// 環境変数を足すなど、`Sandbox::run` では足りないときに使う。
    pub fn of_command(cmd: &mut Command) -> Self {
        Self::of(cmd.output().expect("ailo を起動できない"))
    }

    fn of(o: Output) -> Self {
        Self {
            stdout: String::from_utf8_lossy(&o.stdout).to_string(),
            stderr: String::from_utf8_lossy(&o.stderr).to_string(),
            code: o.status.code().unwrap_or(-1),
        }
    }

    /// 成功を確かめたうえで標準出力を返す。落ちたときは標準エラーごと見せる。
    pub fn ok(&self) -> &str {
        assert_eq!(
            self.code, 0,
            "終了コード {}\nstdout:\n{}\nstderr:\n{}",
            self.code, self.stdout, self.stderr
        );
        self.stdout.trim_end()
    }
}
