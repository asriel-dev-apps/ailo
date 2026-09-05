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

/// 秘匿値がどこかに素のまま残っていないか。変形した形も見る。
fn appears_anywhere(run: &Run, sb: &Sandbox, secret: &str) -> bool {
    let encoded = percent_encode(secret);
    let haystack = format!("{}{}{}", run.stdout, run.stderr, sb.all_stored_text());
    haystack.contains(secret) || haystack.contains(&encoded)
}

/// クエリに載ったときの形。`urlencoding` を足さずに済む範囲で十分。
fn percent_encode(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                c.to_string()
            } else {
                format!("%{:02X}", c as u8)
            }
        })
        .collect()
}

#[test]
fn percent_encoding_helper_matches_what_a_url_does() {
    // 検査に使う変形が実物とずれていたら、監査は静かに無意味になる。
    assert_eq!(percent_encode("a/b+c"), "a%2Fb%2Bc");
    assert_eq!(percent_encode("plain"), "plain");
}

/// Authorization は出力にもダンプにも残らないこと。コントロール付き。
#[test]
fn an_authorization_header_never_reaches_stdout_or_disk() {
    let server = TestServer::start();
    let secret = "Bearer s3cret-token-value";

    let sb = Sandbox::new();
    let run = sb.run(&[
        "get",
        &server.url("/reflect"),
        &format!("Authorization: {secret}"),
    ]);
    run.ok();
    assert!(
        !appears_anywhere(&run, &sb, secret),
        "マスクされていない:\n{}\n{}",
        run.stdout,
        sb.all_stored_text()
    );

    // コントロール: 外せば必ず出る。出なければ検査のほうが壊れている。
    let control = Sandbox::new();
    let run = control.run(&[
        "get",
        &server.url("/reflect"),
        &format!("Authorization: {secret}"),
        "--no-redact",
    ]);
    run.ok();
    assert!(
        appears_anywhere(&run, &control, secret),
        "コントロールがヒットしない。検査が働いていない"
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

    let sb = Sandbox::new();
    let run = sb.run(&[
        "get",
        &server.url("/reflect"),
        &format!("Authorization: {secret}"),
        "--full",
    ]);
    run.ok();
    assert!(
        !appears_anywhere(&run, &sb, "reflected-token"),
        "反響した本文が素通りしている:\n{}",
        run.stdout
    );
}

/// 環境変数から渡した秘匿値を URL に展開しても残らないこと。
#[test]
fn a_secret_expanded_into_the_url_is_masked_everywhere() {
    let server = TestServer::start();
    let secret = "k3y-abc-xyz";

    let sb = Sandbox::new();
    sb.write_config("default_env = \"stg\"\n");
    let url = format!("{}?key={{{{api_key}}}}", server.url("/reflect"));
    let run = Run::of_command(
        sb.command()
            .args(["get", &url, "--full"])
            .env("AILO_SECRET_STG_API_KEY", secret),
    );
    run.ok();
    assert!(
        !appears_anywhere(&run, &sb, secret),
        "URL に展開した秘匿値が残っている:\n{}\n{}",
        run.stdout,
        sb.all_stored_text()
    );
    // 秘匿値を URL に載せたことは警告されること(サーバのログに残るため)。
    assert!(run.stderr.contains("URL に秘匿値"), "{}", run.stderr);

    let control = Sandbox::new();
    control.write_config("default_env = \"stg\"\n");
    let url = format!("{}?key={{{{api_key}}}}", server.url("/reflect"));
    let run = Run::of_command(
        control
            .command()
            .args(["get", &url, "--full", "--no-redact"])
            .env("AILO_SECRET_STG_API_KEY", secret),
    );
    run.ok();
    assert!(
        appears_anywhere(&run, &control, secret),
        "コントロールがヒットしない。検査が働いていない"
    );
}

/// 短い値でも落ちること。長さで足切りしていると、実際の token が短いときに漏れる。
#[test]
fn a_short_secret_value_is_masked_as_well() {
    let server = TestServer::start();
    let secret = "abc123";

    let sb = Sandbox::new();
    sb.write_config("default_env = \"stg\"\n");
    let run = Run::of_command(
        sb.command()
            .args(["get", &server.url("/reflect"), "--full"])
            .env("AILO_SECRET_STG_TOKEN", secret)
            .args(["X-Token: {{token}}"]),
    );
    run.ok();
    assert!(
        !appears_anywhere(&run, &sb, secret),
        "短い秘匿値が素通りしている:\n{}\n{}",
        run.stdout,
        sb.all_stored_text()
    );
}

/// 記号を含む秘匿値をクエリに載せると percent-encode された形で送られる。
/// 素の形だけを探す監査はここで破れる。
#[test]
fn a_secret_is_masked_even_after_percent_encoding() {
    let server = TestServer::start();
    let secret = "a/b+c=d";

    let sb = Sandbox::new();
    sb.write_config("default_env = \"stg\"\n");
    let run = Run::of_command(
        sb.command()
            .args(["get", &server.url("/reflect"), "--full"])
            .args(["token==".to_string() + secret])
            .env("AILO_SECRET_STG_TOKEN", secret),
    );
    run.ok();
    assert!(
        !appears_anywhere(&run, &sb, secret),
        "percent-encode された秘匿値が残っている:\n{}\n{}",
        run.stdout,
        sb.all_stored_text()
    );
}

/// ボディに直書きした秘匿値も落ちること。名前で判断する経路。
#[test]
fn a_password_field_in_the_request_body_is_masked() {
    let server = TestServer::start();
    let secret = "hunter2-hunter2";

    let sb = Sandbox::new();
    let run = sb.run(&[
        "post",
        &server.url("/reflect"),
        &format!("password={secret}"),
        "--full",
    ]);
    run.ok();
    assert!(
        !appears_anywhere(&run, &sb, secret),
        "本文の password が残っている:\n{}\n{}",
        run.stdout,
        sb.all_stored_text()
    );
}

/// 設定で足したヘッダ名もマスク対象になること。
#[test]
fn redact_headers_in_the_config_are_honoured() {
    let server = TestServer::start();
    let secret = "tenant-internal-id";

    let sb = Sandbox::new();
    sb.write_config("redact_headers = [\"X-Tenant\"]\n");
    let run = sb.run(&[
        "get",
        &server.url("/reflect"),
        &format!("X-Tenant: {secret}"),
        "--full",
    ]);
    run.ok();
    assert!(
        !appears_anywhere(&run, &sb, secret),
        "設定で指定したヘッダが落ちていない:\n{}\n{}",
        run.stdout,
        sb.all_stored_text()
    );

    // コントロール: 設定しなければ出る。設定が効いていることの裏取り。
    let control = Sandbox::new();
    let run = control.run(&[
        "get",
        &server.url("/reflect"),
        &format!("X-Tenant: {secret}"),
        "--full",
    ]);
    run.ok();
    assert!(
        appears_anywhere(&run, &control, secret),
        "コントロールがヒットしない。検査が働いていない"
    );
}
