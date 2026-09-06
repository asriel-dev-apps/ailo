//! workspace の結合テスト。
//!
//! 確かめるのは 4 つ。**同名のリクエストを別々に持てること**、
//! **ディレクトリを移動するだけで切り替わること**、**明示指定でも切り替えられること**、
//! そして **workspace をまたいで秘匿値が漏れないこと**。

mod support;

use support::{Run, Sandbox, TestServer};

/// 2 つの workspace が同名のリクエストを別々に持てること。
#[test]
fn two_workspaces_hold_their_own_request_of_the_same_name() {
    let server = TestServer::start();
    let sb = Sandbox::new();

    let a = sb.dir("project-a");
    let b = sb.dir("project-b");
    sb.mark(&a, "alpha");
    sb.mark(&b, "beta");

    // 同じ名前 `login` を、別々の URL で登録する。
    for (dir, path) in [(&a, "/alpha"), (&b, "/beta")] {
        let editor = sb.editor(&format!(
            "printf 'method = \"GET\"\\nurl = \"{}\"\\n' > \"$1\"\n",
            server.url(path)
        ));
        Run::of_command(
            sb.command()
                .current_dir(dir)
                .args(["new", "login"])
                .env("EDITOR", &editor),
        )
        .ok();
    }

    assert_eq!(
        sb.run_in(&a, &["run", "login", "--pick", ".path"]).ok(),
        "/alpha"
    );
    assert_eq!(
        sb.run_in(&b, &["run", "login", "--pick", ".path"]).ok(),
        "/beta"
    );

    // 一覧も混ざらない。
    assert!(sb.run_in(&a, &["ls"]).ok().contains("/alpha"));
    assert!(!sb.run_in(&a, &["ls"]).ok().contains("/beta"));
}

/// ディレクトリを移動するだけで設定が切り替わること。
#[test]
fn moving_between_directories_switches_the_workspace() {
    let sb = Sandbox::new();
    let a = sb.dir("project-a");
    let b = sb.dir("project-b");
    sb.mark(&a, "alpha");
    sb.mark(&b, "beta");

    sb.run_in(&a, &["config", "set", "who", "alpha-side"]).ok();
    sb.run_in(&b, &["config", "set", "who", "beta-side"]).ok();

    assert_eq!(sb.run_in(&a, &["config", "get", "who"]).ok(), "alpha-side");
    assert_eq!(sb.run_in(&b, &["config", "get", "who"]).ok(), "beta-side");
}

/// サブディレクトリでも効くこと。`.ailo` は git と同じく上へ辿って探す。
#[test]
fn the_marker_is_found_from_a_subdirectory() {
    let sb = Sandbox::new();
    let root = sb.dir("project-a");
    sb.mark(&root, "alpha");
    let deep = sb.dir("project-a/src/inner");

    sb.run_in(&root, &["config", "set", "who", "alpha-side"])
        .ok();
    assert_eq!(
        sb.run_in(&deep, &["config", "get", "who"]).ok(),
        "alpha-side"
    );
}

/// 明示指定が `.ailo` より強いこと。`--env` と同じ関係。
#[test]
fn an_explicit_flag_wins_over_the_marker() {
    let sb = Sandbox::new();
    let a = sb.dir("project-a");
    sb.mark(&a, "alpha");

    sb.run_in(&a, &["config", "set", "who", "alpha-side"]).ok();
    sb.run_in(&a, &["-w", "beta", "config", "set", "who", "beta-side"])
        .ok();

    assert_eq!(sb.run_in(&a, &["config", "get", "who"]).ok(), "alpha-side");
    assert_eq!(
        sb.run_in(&a, &["-w", "beta", "config", "get", "who"]).ok(),
        "beta-side"
    );
}

/// `.ailo` の無いところは、これまでどおりの置き場所で動くこと（破壊的変更を入れない）。
#[test]
fn a_directory_without_a_marker_keeps_using_the_default_place() {
    let sb = Sandbox::new();
    let plain = sb.dir("no-marker");

    sb.run(&["config", "set", "who", "default-side"]).ok();
    assert_eq!(
        sb.run_in(&plain, &["config", "get", "who"]).ok(),
        "default-side"
    );
    // 既定の設定ファイルは今までの場所にある。
    assert!(sb.config_dir().join("config.toml").exists());
}

/// **秘匿値が workspace をまたいで見えないこと。**
///
/// 索引は workspace ごとの置き場所にあり、環境変数の名前も workspace で分かれる。
#[test]
fn secrets_do_not_leak_between_workspaces() {
    let server = TestServer::start();
    let sb = Sandbox::new();
    let a = sb.dir("project-a");
    let b = sb.dir("project-b");
    sb.mark(&a, "alpha");
    sb.mark(&b, "beta");

    let secret = "alpha-only-token";
    let url = format!("{}?key={{{{token}}}}", server.url("/reflect"));

    // alpha 側の名前で渡した秘匿値は、alpha でだけ解決する。
    let in_alpha = Run::of_command(
        sb.command()
            .current_dir(&a)
            .args(["get", &url, "-e", "stg", "--pick", ".query"])
            .env("AILO_SECRET_ALPHA_STG_TOKEN", secret),
    );
    assert_eq!(in_alpha.ok(), format!("key={secret}"));

    // 同じ環境変数のまま beta へ移ると、解決しないので送信前に落ちる。
    let in_beta = Run::of_command(
        sb.command()
            .current_dir(&b)
            .args(["get", &url, "-e", "stg", "--pick", ".query"])
            .env("AILO_SECRET_ALPHA_STG_TOKEN", secret),
    );
    assert_ne!(in_beta.code, 0, "beta 側で解決してしまっている");
    assert!(
        !in_beta.stdout.contains(secret) && !in_beta.stderr.contains(secret),
        "秘匿値が出力に出ている"
    );

    // 秘匿値の索引も混ざらない。
    let ls_in_beta = sb.run_in(&b, &["secret", "ls"]).ok().to_string();
    assert!(!ls_in_beta.contains("token"), "{ls_in_beta}");
}

/// 使えない workspace 名は、どこから来たかを添えて拒むこと。
#[test]
fn an_unusable_workspace_name_is_refused_with_its_source() {
    let sb = Sandbox::new();
    let dir = sb.dir("bad");
    sb.mark(&dir, "../escape");

    let run = sb.run_in(&dir, &["config", "list"]);
    assert_ne!(run.code, 0);
    assert!(run.stderr.contains(".ailo"), "{}", run.stderr);

    let flag = sb.run(&["-w", "../escape", "config", "list"]);
    assert_ne!(flag.code, 0);
    assert!(flag.stderr.contains("--workspace"), "{}", flag.stderr);
}

/// `ailo new` が送らずに登録できること。既にある名前ならその定義を開くこと。
#[test]
fn new_registers_a_request_without_sending_it() {
    let server = TestServer::start();
    let sb = Sandbox::new();

    let editor = sb.editor(&format!(
        "printf 'method = \"GET\"\\nurl = \"{}\"\\n' > \"$1\"\n",
        server.url("/list")
    ));
    Run::of_command(sb.command().args(["new", "listing"]).env("EDITOR", &editor)).ok();

    assert_eq!(
        sb.run(&["run", "listing", "--pick", ".[].title"]).ok(),
        "1 つめ\n2 つめ"
    );

    // 既にある名前なら、いまの定義が開かれる。
    let show = sb.editor("cp \"$1\" \"$(dirname \"$1\")/seen.toml\"\n");
    Run::of_command(sb.command().args(["new", "listing"]).env("EDITOR", &show)).ok();
    let seen = std::fs::read_to_string(sb.config_dir().join("seen.toml")).unwrap();
    assert!(seen.contains("/list"), "既存の定義が開かれていない: {seen}");
}

/// 定義として読めないものは登録せず、書いたものを捨てないこと。
#[test]
fn new_refuses_a_broken_definition_and_keeps_the_work() {
    let sb = Sandbox::new();
    let editor = sb.editor("printf 'これは TOML ではない\\n' > \"$1\"\n");
    let run = Run::of_command(sb.command().args(["new", "broken"]).env("EDITOR", &editor));

    assert_ne!(run.code, 0);
    assert!(run.stderr.contains("残してあります"), "{}", run.stderr);
    assert!(sb.run(&["ls"]).ok().contains("ありません"));
}
