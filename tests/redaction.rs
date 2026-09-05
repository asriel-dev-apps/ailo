//! マスクの結合テスト。
//!
//! ユニットテストが全部緑のまま、実網では 11 経路が漏れた。だからここでは
//! **実際に送ったあと、標準出力・標準エラー・保存された全ファイルをまとめて grep する**。
//! 個別の経路を 1 本ずつ見ていると、必ずどれかを見落とす。
//!
//! どのテストにも**コントロール**を添える。`--no-redact` で必ずヒットすることを
//! 示さないと、検査自体が壊れていても 0 件と同じ見え方になる。
//! 変形して残る形(percent-encode、短い値)も検査対象に含める。

mod support;

use support::{Run, Sandbox, TestServer};

/// 秘匿値がどこかに残っていないか。**変形した形も一緒に探す。**
///
/// 素の文字列だけを探す監査は、コントロールが付いていても素通りする。実際に
/// percent-encode 形を見ていなかったせいで 3 経路を見落とした。ここで見るのは
/// 素の値・percent-encode 形・JSON エスケープ形の 3 つで、いずれも**大文字小文字を
/// 無視して**照合する。
///
/// **ここが検査できる範囲がそのまま監査の守備範囲**であることは意識しておく。
/// 分割して保存された値、base64/hex に変換された値、値の一部だけが残った場合は
/// これでは捕まらない。
fn appears_anywhere(run: &Run, sb: &Sandbox, secret: &str) -> bool {
    let haystack = format!("{}{}{}", run.stdout, run.stderr, sb.all_stored_text()).to_lowercase();
    variants(secret)
        .iter()
        .any(|form| haystack.contains(&form.to_lowercase()))
}

/// 秘匿値が保存先で取りうる形。
fn variants(secret: &str) -> Vec<String> {
    let json = serde_json::to_string(secret).unwrap_or_default();
    vec![
        secret.to_string(),
        percent_encode(secret),
        // 前後のクォートを外した、エスケープ済みの中身。
        json.trim_matches('"').to_string(),
    ]
}

/// クエリに載ったときの形。
///
/// **バイト単位で回す。** `char` を `u8` に切り詰めると非 ASCII の秘匿値で
/// 実際の URL と食い違い、監査が静かに無意味になる。
fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            let c = b as char;
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                c.to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// 検査に使う変形が実物とずれていたら、監査は静かに無意味になる。
/// **変形のほうを、実物(`Url`)の出力と突き合わせて固定する。**
#[test]
fn the_percent_encoding_used_by_the_audit_matches_a_real_url() {
    for secret in ["a/b+c=d", "plain", "日本語のトークン", "sp ace"] {
        let url = reqwest::Url::parse_with_params("http://x/", [("k", secret)]).unwrap();
        let on_the_wire = url.query().unwrap().trim_start_matches("k=").to_string();
        // `Url` は空白を `+` にするなど流儀の違いがあるので、そこだけ揃えて比べる。
        assert_eq!(
            percent_encode(secret).replace("%20", "+"),
            on_the_wire,
            "`{secret}` の変形が実物とずれている"
        );
    }
}

/// JSON に保存されたときのエスケープ形も検査対象に入っていること。
#[test]
fn the_audit_also_looks_for_the_json_escaped_form() {
    let forms = variants(r#"a"b\c"#);
    assert!(
        forms.iter().any(|f| f.contains(r#"\""#)),
        "JSON エスケープ形が入っていない: {forms:?}"
    );
}

/// マスクありとコントロール(`--no-redact`)を必ず対で回す。
///
/// **コントロールを省いた検査は、検査器が壊れていても 0 件と同じ見え方になる。**
/// 「秘匿値が出ない」ことだけを見ているテストは、fixture が退行して秘匿値を
/// そもそも送らなくなった日から、何も検査していないのに緑を出し続ける。
struct Sending<'a> {
    /// 隔離ディレクトリに置く `config.toml`。
    config: &'a str,
    /// `ailo` に渡す引数(`--no-redact` は付けない)。
    args: Vec<String>,
    /// 追加で渡す環境変数。
    envs: Vec<(&'a str, &'a str)>,
}

impl<'a> Sending<'a> {
    fn new(args: &[&str]) -> Self {
        Self {
            config: "",
            args: args.iter().map(|s| s.to_string()).collect(),
            envs: Vec::new(),
        }
    }

    fn config(mut self, toml: &'a str) -> Self {
        self.config = toml;
        self
    }

    fn env(mut self, name: &'a str, value: &'a str) -> Self {
        self.envs.push((name, value));
        self
    }

    fn run(&self, no_redact: bool) -> (Run, Sandbox) {
        let sb = Sandbox::new();
        if !self.config.is_empty() {
            sb.write_config(self.config);
        }
        let mut cmd = sb.command();
        cmd.args(&self.args);
        if no_redact {
            cmd.arg("--no-redact");
        }
        for (k, v) in &self.envs {
            cmd.env(k, v);
        }
        let run = Run::of_command(&mut cmd);
        run.ok();
        (run, sb)
    }
}

/// 秘匿値が消えていること、かつコントロールでは必ず出ることを確かめる。
fn assert_masked_with_control(secret: &str, sending: Sending) {
    let (run, sb) = sending.run(false);
    assert!(
        !appears_anywhere(&run, &sb, secret),
        "マスクされていない:\nstdout:\n{}\nstderr:\n{}\n保存されたもの:\n{}",
        run.stdout,
        run.stderr,
        sb.all_stored_text()
    );

    let (run, sb) = sending.run(true);
    assert!(
        appears_anywhere(&run, &sb, secret),
        "コントロールがヒットしない。検査のほうが働いていない"
    );
}

/// Authorization は出力にもダンプにも残らないこと。
#[test]
fn an_authorization_header_never_reaches_stdout_or_disk() {
    let server = TestServer::start();
    let secret = "Bearer s3cret-token-value";
    assert_masked_with_control(
        secret,
        Sending::new(&[
            "get",
            &server.url("/reflect"),
            &format!("Authorization: {secret}"),
        ]),
    );
}

/// サーバが本文に反響して返した秘匿値も落ちること。
///
/// ヘッダ名だけでマスクしていると、**返ってきた本文**が素通りする。
/// リクエストを反響する API は珍しくない。
#[test]
fn a_secret_reflected_in_the_response_body_is_masked_too() {
    let server = TestServer::start();
    let secret = "Bearer reflected-token";
    assert_masked_with_control(
        "reflected-token",
        Sending::new(&[
            "get",
            &server.url("/reflect"),
            &format!("Authorization: {secret}"),
            "--full",
        ]),
    );
}

/// 環境変数から渡した秘匿値を URL に展開しても残らないこと。
#[test]
fn a_secret_expanded_into_the_url_is_masked_everywhere() {
    let server = TestServer::start();
    let secret = "k3y-abc-xyz";
    let url = format!("{}?key={{{{api_key}}}}", server.url("/reflect"));

    let sending = Sending::new(&["get", &url, "--full"])
        .config("default_env = \"stg\"\n")
        .env("AILO_SECRET_STG_API_KEY", secret);
    assert_masked_with_control(secret, sending);

    // 秘匿値を URL に載せたことは警告されること(サーバのアクセスログに残るため)。
    let sending = Sending::new(&["get", &url, "--full"])
        .config("default_env = \"stg\"\n")
        .env("AILO_SECRET_STG_API_KEY", secret);
    let (run, _) = sending.run(false);
    assert!(run.stderr.contains("URL に秘匿値"), "{}", run.stderr);
}

/// 短い値でも落ちること。長さで足切りしていると、実際の token が短いときに漏れる。
#[test]
fn a_short_secret_value_is_masked_as_well() {
    let server = TestServer::start();
    let secret = "abc123";
    assert_masked_with_control(
        secret,
        Sending::new(&[
            "get",
            &server.url("/reflect"),
            "X-Token: {{token}}",
            "--full",
        ])
        .config("default_env = \"stg\"\n")
        .env("AILO_SECRET_STG_TOKEN", secret),
    );
}

/// 記号を含む秘匿値をクエリに載せると percent-encode された形で送られる。
/// 素の形だけを探す監査はここで破れる。
#[test]
fn a_secret_is_masked_even_after_percent_encoding() {
    let server = TestServer::start();
    let secret = "a/b+c=d";
    assert_masked_with_control(
        secret,
        Sending::new(&[
            "get",
            &server.url("/reflect"),
            &format!("token=={secret}"),
            "--full",
        ])
        .config("default_env = \"stg\"\n")
        .env("AILO_SECRET_STG_TOKEN", secret),
    );
}

/// 非 ASCII の秘匿値でも落ちること。percent-encode の形が素の値と大きく変わる。
#[test]
fn a_non_ascii_secret_is_masked_as_well() {
    let server = TestServer::start();
    let secret = "とても秘密な値";
    assert_masked_with_control(
        secret,
        Sending::new(&[
            "get",
            &server.url("/reflect"),
            &format!("token=={secret}"),
            "--full",
        ])
        .config("default_env = \"stg\"\n")
        .env("AILO_SECRET_STG_TOKEN", secret),
    );
}

/// ボディに直書きした秘匿値も落ちること。名前で判断する経路。
#[test]
fn a_password_field_in_the_request_body_is_masked() {
    let server = TestServer::start();
    let secret = "hunter2-hunter2";
    assert_masked_with_control(
        secret,
        Sending::new(&[
            "post",
            &server.url("/reflect"),
            &format!("password={secret}"),
            "--full",
        ]),
    );
}

/// 設定で足したヘッダ名もマスク対象になること。
///
/// ここだけコントロールが `--no-redact` ではない。確かめたいのが「マスクが働くか」
/// ではなく「**設定が効いているか**」なので、設定を外したときに出ることを見る。
#[test]
fn redact_headers_in_the_config_are_honoured() {
    let server = TestServer::start();
    let secret = "tenant-internal-id";
    let args = [
        "get".to_string(),
        server.url("/reflect"),
        format!("X-Tenant: {secret}"),
        "--full".to_string(),
    ];
    let args: Vec<&str> = args.iter().map(String::as_str).collect();

    let (run, sb) = Sending::new(&args)
        .config("redact_headers = [\"X-Tenant\"]\n")
        .run(false);
    assert!(
        !appears_anywhere(&run, &sb, secret),
        "設定で指定したヘッダが落ちていない:\n{}\n{}",
        run.stdout,
        sb.all_stored_text()
    );

    let (run, sb) = Sending::new(&args).run(false);
    assert!(
        appears_anywhere(&run, &sb, secret),
        "コントロールがヒットしない。既定では出るはずの値が出ていない"
    );
}
