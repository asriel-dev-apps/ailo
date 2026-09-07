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
    sb.write_config("[headers]\nAccept = \"application/json\"\nFrom = \"ailo\"\n");

    let run = sb.run(&[
        "get",
        &server.url("/reflect"),
        "Accept: text/csv",
        "--pick",
        ".headers.From",
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
Accept = "common"

[env.stg.headers]
Accept = "env"
"#,
    );

    let env_wins = sb.run(&[
        "get",
        &server.url("/reflect"),
        "-e",
        "stg",
        "--pick",
        ".headers.Accept",
    ]);
    assert_eq!(env_wins.ok(), "env");

    let cli_wins = sb.run(&[
        "get",
        &server.url("/reflect"),
        "-e",
        "stg",
        "Accept: cli",
        "--pick",
        ".headers.Accept",
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

    let run = sb.run(&["get", &server.url("/reflect"), "--pick", ".nope"]);
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

/// 隔離が「今のテストがその経路を踏まないから」ではなく**仕組みで**成り立っていること。
///
/// これが落ちたら、キーチェーンを触るテストを書いた瞬間に開発者本人の
/// login keychain へ書き込む状態に戻っている。
#[test]
fn the_sandbox_cannot_reach_the_real_keychain() {
    let sb = Sandbox::new();
    let run = sb.run(&["secret", "rm", "stg", "token"]);
    assert_ne!(run.code, 0, "キーチェーンに到達してしまっている");
    assert!(
        run.stderr.contains("AILO_NO_KEYCHAIN"),
        "止まった理由が違う: {}",
        run.stderr
    );
}

/// 圧縮されたレスポンスでも中身を扱えること。
///
/// ailo は自分で gzip を有効にしており、実 API の大半も圧縮して返す。
/// この経路を通らない fixture は、リクエスト側で直したのと同じ穴を
/// レスポンス側に残すことになる。
#[test]
fn a_gzip_encoded_response_is_decoded_before_picking() {
    let server = TestServer::start();
    let sb = Sandbox::new();

    // 式は最も単純な形にする。ここで見たいのは復号であって `--pick` ではない。
    let run = sb.run(&["get", &server.url("/gzip"), "--pick", ".path"]);
    assert_eq!(run.ok(), "/gzip");
}

/// `Content-Length` を出さず、接続を閉じることで本文の終わりを示す応答も扱えること。
#[test]
fn a_response_without_a_content_length_is_read_to_the_end() {
    let server = TestServer::start();
    let sb = Sandbox::new();

    let run = sb.run(&["get", &server.url("/no-length"), "--pick", ".path"]);
    assert_eq!(run.ok(), "/no-length");
}

/// 本文の無い応答(204)でも落ちず、状態が伝わること。
#[test]
fn an_empty_response_is_reported_rather_than_treated_as_a_failure() {
    let server = TestServer::start();
    let sb = Sandbox::new();

    let run = sb.run(&["get", &server.url("/empty")]);
    assert!(run.ok().contains("204"), "{}", run.stdout);
}

/// 同じテーブルに大文字小文字だけが違うヘッダを書いたら、黙って捨てずに知らせること。
///
/// 送られるのは片方だけで、しかもどちらが残るかは綴りのソート順で決まる。
/// 記述順と一致しないので、警告が無いと「書いたヘッダが理由なく消える」ように見える。
#[test]
fn two_spellings_of_one_header_in_the_same_table_are_reported() {
    let server = TestServer::start();
    let sb = Sandbox::new();
    sb.write_config("[headers]\nAccept = \"upper\"\naccept = \"lower\"\n");

    let run = sb.run(&[
        "get",
        &server.url("/reflect"),
        "--pick",
        ".header_values.accept",
    ]);
    // 実際に送られるのは 1 本だけ。
    assert_eq!(run.ok(), "[\"lower\"]");
    assert!(
        run.stderr.contains("大文字小文字だけが違うヘッダ"),
        "警告が出ていない: {}",
        run.stderr
    );
}

// ---------------------------------------------------------------- `--shape --depth`

/// `--shape` の出力は、幅も深さもある一覧だと 48 行になる。**最小コストで形を掴む**
/// という `--shape` の目的からは長い。`--depth` で切れることを、実際に送って測る。
///
/// 数字を直に書いているのは、「短くなった」ではなく「**どれだけになるか**」を
/// 固定するため。既定を変える変更が入れば、ここが落ちて気づける。
#[test]
fn shape_depth_cuts_the_output_and_the_default_is_pinned() {
    let server = TestServer::start();
    let sb = Sandbox::new();
    let url = server.url("/issues");
    let lines = |args: &[&str]| {
        let mut all = vec!["get", url.as_str(), "--shape", "--no-dump"];
        all.extend_from_slice(args);
        sb.run(&all).ok().lines().count()
    };

    // 1 行目は必ずステータス行。形そのものは行数から 1 を引いた分。
    assert_eq!(lines(&[]), 52, "既定の出力が変わった");
    assert_eq!(lines(&["--depth", "1"]), 2, "深さ 1 は配列 1 行だけのはず");
    assert_eq!(lines(&["--depth", "2"]), 26);
    assert_eq!(lines(&["--depth", "3"]), 40);
    assert_eq!(lines(&["--depth", "4"]), 48);
}

/// 既定が 6 段であること。
///
/// **行数では固定できない。** この fixture は 4 段目で打ち切っても 6 段まで開いても
/// 48 行に収まるので、既定を 4 に変えても行数のアサーションは通ってしまう。
/// 出力そのものを突き合わせる。
#[test]
fn the_default_depth_is_six() {
    let server = TestServer::start();
    let sb = Sandbox::new();
    let url = server.url("/issues");
    // 1 行目のステータス行には所要時間が入る。実行のたびに変わるので、形だけを比べる。
    let out = |args: &[&str]| {
        let mut all = vec!["get", url.as_str(), "--shape", "--no-dump"];
        all.extend_from_slice(args);
        sb.run(&all)
            .ok()
            .lines()
            .skip(1)
            .collect::<Vec<_>>()
            .join("\n")
    };

    assert_eq!(out(&[]), out(&["--depth", "6"]), "既定が 6 段でなくなった");
    // 入れ子の実際の深さを超えたら、それ以上は何も増えない。
    assert_eq!(out(&["--depth", "9"]), out(&["--depth", "6"]));
    // fixture が本当に 5 段目以降を持っていること。持っていなければ
    // 上の 2 つは「何を比べても同じ」で通ってしまう。
    assert_ne!(
        out(&["--depth", "4"]),
        out(&["--depth", "6"]),
        "fixture が浅すぎて、既定の深さを検出できていない"
    );
}

/// 打ち切った位置は `object` / `array` と書いて残す。消してしまうと
/// 「そのキーは無い」と読まれる。
#[test]
fn shape_depth_marks_what_it_elided_instead_of_dropping_it() {
    let server = TestServer::start();
    let sb = Sandbox::new();

    let run = sb.run(&[
        "get",
        &server.url("/issues"),
        "--shape",
        "--depth",
        "2",
        "--no-dump",
    ]);
    let out = run.ok();
    assert!(out.contains("user: object"), "{out}");
    // 配列は**要素数を残す**。0 件と 1 件が同じ `array` になると、
    // 「いま何件あるか」を知るのに深さを上げ直すことになる。
    assert!(out.contains("labels: [1 item] …"), "{out}");
    assert!(out.contains("assignees: [0 items]"), "{out}");
    // 深さ 2 で止めているのだから、3 段目のキーは出ていないこと。
    assert!(!out.contains("site_admin"), "打ち切れていない: {out}");
}

/// `--depth` は `--shape` 専用。単独で受け付けると「指定したのに何も変わらない」
/// という一番たちの悪い黙り方をする。
#[test]
fn depth_without_shape_is_rejected() {
    let server = TestServer::start();
    let sb = Sandbox::new();

    let run = sb.run(&["get", &server.url("/issues"), "--depth", "2", "--no-dump"]);
    assert_ne!(run.code, 0, "受け付けてしまっている: {}", run.stdout);
    assert!(run.stderr.contains("--shape"), "{}", run.stderr);
}

/// 深さ 0 は「配列」「オブジェクト」の一言しか出ず、形を掴む役に立たない。
#[test]
fn depth_zero_is_rejected() {
    let server = TestServer::start();
    let sb = Sandbox::new();

    let run = sb.run(&[
        "get",
        &server.url("/issues"),
        "--shape",
        "--depth",
        "0",
        "--no-dump",
    ]);
    assert_ne!(run.code, 0, "受け付けてしまっている: {}", run.stdout);
    // 「未知のフラグ」ではなく、範囲で落ちていること。
    assert!(run.stderr.contains("1..=128"), "{}", run.stderr);
}

/// 回帰: `--head` と `--full` は `--shape` に効かない。**黙って無視していた**ので、
/// 「短くしたつもりで長いまま」に気づけなかった。行数を切る指定は `--depth` に一本化し、
/// 効かない組み合わせは落とす。
#[test]
fn head_and_full_are_rejected_with_shape_instead_of_being_ignored() {
    let server = TestServer::start();
    let sb = Sandbox::new();

    let url = server.url("/issues");
    for extra in [vec!["--head", "5"], vec!["--full"]] {
        let mut args = vec!["get", url.as_str(), "--shape", "--no-dump"];
        args.extend_from_slice(&extra);
        let run = sb.run(&args);
        assert_ne!(run.code, 0, "{extra:?} が黙って通った: {}", run.stdout);
        assert!(run.stderr.contains("--shape"), "{}", run.stderr);
        assert!(run.stderr.contains(extra[0]), "{}", run.stderr);
    }
}

/// 上の 2 つを落とした結果、`--shape` 単独が壊れていないこと。
#[test]
fn shape_alone_still_works() {
    let server = TestServer::start();
    let sb = Sandbox::new();

    let run = sb.run(&["get", &server.url("/list"), "--shape", "--no-dump"]);
    assert!(run.ok().contains("[2 items]"), "{}", run.ok());
}
