//! 実際に送って、サーバが受け取ったものを見る結合テスト。
//!
//! ここにある回帰テストは、いずれも**ユニットテストが全部緑のまま実使用で出たバグ**を
//! 模している。共通していたのは、fixture が都合のよい形(オブジェクト始まり、英数字
//! だけのキー、ヘッダ 1 本)しか持っていなかったこと。だから形のほうを実物に寄せる。

mod support;

use support::{Sandbox, TestServer};

/// 回帰: 一覧系の API はトップレベルが配列。`.[].title` が書けないと最初の実用で詰まる。
#[test]
fn a_top_level_array_response_can_be_picked() {
    let server = TestServer::start();
    let sb = Sandbox::new();

    let run = sb.run(&["get", &server.url("/list"), "--pick", ".[].title"]);
    assert_eq!(run.ok(), "1 つめ\n2 つめ");
}

/// 回帰: ハイフンを含むキー。**HTTP クライアントなのにヘッダ名が引けない**のは
/// 実用にならない。サーバに反響させた値を引く形で、経路ごと確かめる。
#[test]
fn a_hyphenated_header_name_can_be_picked_from_the_response() {
    let server = TestServer::start();
    let sb = Sandbox::new();

    let run = sb.run(&[
        "get",
        &server.url("/reflect"),
        "X-Tenant: acme",
        "--pick",
        ".headers.X-Tenant",
    ]);
    assert_eq!(run.ok(), "acme");
}

/// ハイフンは本文側のキーにも普通に出る。配列展開と組み合わせても壊れないこと。
#[test]
fn a_hyphenated_body_key_can_be_picked_through_an_array() {
    let server = TestServer::start();
    let sb = Sandbox::new();

    let run = sb.run(&["get", &server.url("/list"), "--pick", ".[].author-name"]);
    assert_eq!(run.ok(), "taro\nhanako");
}

/// 回帰: 設定の `[headers]` と同名をコマンドラインで指定したら**後勝ち**。
/// 積むと両方送られ、どちらが効いたのか分からないまま話が進む。
#[test]
fn a_command_line_header_replaces_the_configured_one_on_the_wire() {
    let server = TestServer::start();
    let sb = Sandbox::new();
    sb.write_config("[headers]\nAccept = \"application/json\"\n");

    let run = sb.run(&[
        "get",
        &server.url("/reflect"),
        "Accept: text/csv",
        "--pick",
        ".header_values.accept",
    ]);
    // 本数まで見る。連結された文字列を見ているだけだと、1 本に見えるが実は
    // サーバ側で結合されていた、という取り違えが起きる。
    assert_eq!(run.ok(), "[\"text/csv\"]");
}

/// 上書きしていない設定のヘッダはそのまま送られること。上書きの修正で
/// 「設定のヘッダが消える」方向へ倒れていないかを見る。
#[test]
fn configured_headers_that_are_not_overridden_still_reach_the_server() {
    let server = TestServer::start();
    let sb = Sandbox::new();
    sb.write_config("[headers]\nAccept = \"application/json\"\nX-App = \"ailo\"\n");

    let run = sb.run(&[
        "get",
        &server.url("/reflect"),
        "Accept: text/csv",
        "--pick",
        ".headers.X-App",
    ]);
    assert_eq!(run.ok(), "ailo");
}

/// 環境ごとのヘッダは共通ヘッダより優先し、コマンドラインはそのさらに上。
#[test]
fn header_layers_resolve_common_then_env_then_command_line() {
    let server = TestServer::start();
    let sb = Sandbox::new();
    sb.write_config(
        r#"
[headers]
X-Layer = "common"
X-From-Common = "yes"

[env.stg.headers]
X-Layer = "env"
"#,
    );

    let env_wins = sb.run(&[
        "get",
        &server.url("/reflect"),
        "-e",
        "stg",
        "--pick",
        ".headers.X-Layer",
    ]);
    assert_eq!(env_wins.ok(), "env");

    let cli_wins = sb.run(&[
        "get",
        &server.url("/reflect"),
        "-e",
        "stg",
        "X-Layer: cli",
        "--pick",
        ".headers.X-Layer",
    ]);
    assert_eq!(cli_wins.ok(), "cli");
}

/// 変数の層。config < env-config < プロセス環境 < `--var`。
/// クエリに載せてサーバに反響させ、実際に届いた値で判定する。
#[test]
fn variable_layers_resolve_in_priority_order() {
    let server = TestServer::start();
    let sb = Sandbox::new();
    sb.write_config(
        r#"
default_env = "stg"

[vars]
who = "config"

[env.stg.vars]
who = "env-config"
"#,
    );

    let url = server.url("/reflect");
    let pick = ".query";

    let base = sb.run(&["get", &format!("{url}?who={{{{who}}}}"), "--pick", pick]);
    assert_eq!(base.ok(), "who=env-config");

    let from_process = support::Run::of_command(
        sb.command()
            .args(["get", &format!("{url}?who={{{{who}}}}"), "--pick", pick])
            .env("AILO_VAR_who", "process"),
    );
    assert_eq!(from_process.ok(), "who=process");

    let from_flag = support::Run::of_command(
        sb.command()
            .args([
                "get",
                &format!("{url}?who={{{{who}}}}"),
                "--pick",
                pick,
                "--var",
                "who=flag",
            ])
            .env("AILO_VAR_who", "process"),
    );
    assert_eq!(from_flag.ok(), "who=flag");
}

/// 共通変数しか無い環境では config の値が使われること(上の層が空でも落ちない)。
#[test]
fn a_common_variable_is_used_when_no_environment_overrides_it() {
    let server = TestServer::start();
    let sb = Sandbox::new();
    sb.write_config("[vars]\nwho = \"config\"\n");

    let run = sb.run(&[
        "get",
        &format!("{}?who={{{{who}}}}", server.url("/reflect")),
        "--pick",
        ".query",
    ]);
    assert_eq!(run.ok(), "who=config");
}

/// JSON でない応答に `--pick` を当てたら、理由の分かるエラーで落ちること。
#[test]
fn picking_a_non_json_response_fails_with_a_usable_message() {
    let server = TestServer::start();
    let sb = Sandbox::new();

    let run = sb.run(&["get", &server.url("/text"), "--pick", ".a"]);
    assert!(run.stderr.contains("JSON"), "{}", run.stderr);
}

/// 一致しなかったことは、空文字が返ったことと区別できる形で伝わること。
#[test]
fn a_pick_that_matches_nothing_says_so_on_stderr() {
    let server = TestServer::start();
    let sb = Sandbox::new();

    let run = sb.run(&["get", &server.url("/list"), "--pick", ".[].nope"]);
    assert_eq!(run.ok(), "");
    assert!(
        run.stderr.contains("一致する値はありません"),
        "{}",
        run.stderr
    );
}

/// `--shape` は値ではなく形を返す。トップレベルが配列でも形が出ること。
#[test]
fn shape_describes_a_top_level_array() {
    let server = TestServer::start();
    let sb = Sandbox::new();

    let out = sb
        .run(&["get", &server.url("/list"), "--shape"])
        .ok()
        .to_string();
    assert!(out.contains("title"), "{out}");
    assert!(!out.contains("1 つめ"), "値が出ている: {out}");
}

/// `--fail` を付けたときだけ、4xx が終了コードに出ること。
#[test]
fn only_fail_turns_a_4xx_into_a_nonzero_exit_code() {
    let server = TestServer::start();
    let sb = Sandbox::new();

    assert_eq!(sb.run(&["get", &server.url("/notfound")]).code, 0);
    assert_eq!(sb.run(&["get", &server.url("/notfound"), "--fail"]).code, 1);
}

/// ヘッダ名の大文字小文字は**層をまたいでも**同じものとして扱われること。
///
/// 設定は名前をそのままキーにした表なので、`accept` と `Accept` を別物として
/// 持ててしまう。層ごとの優先順位が名前の綴りで入れ替わると、上書きしたつもりの
/// 指定が黙って無視される。
#[test]
fn header_layers_merge_regardless_of_letter_case() {
    let server = TestServer::start();

    for (common, env) in [("Accept", "accept"), ("accept", "Accept")] {
        let sb = Sandbox::new();
        sb.write_config(&format!(
            "[headers]\n{common} = \"common\"\n\n[env.stg.headers]\n{env} = \"env\"\n"
        ));

        let run = sb.run(&[
            "get",
            &server.url("/reflect"),
            "-e",
            "stg",
            "--pick",
            ".header_values.accept",
        ]);
        assert_eq!(
            run.ok(),
            "[\"env\"]",
            "共通 `{common}` / 環境 `{env}` の組み合わせで環境側が勝っていない"
        );
    }
}
