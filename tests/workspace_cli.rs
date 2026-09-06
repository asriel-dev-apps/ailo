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

    // alpha の名前で渡した秘匿値は、alpha でだけ解決する。
    // 区切りが `__` なのは、`a` + `b_c` と `a_b` + `c` を別物にするため。
    let in_alpha = Run::of_command(
        sb.command()
            .current_dir(&a)
            .args(["get", &url, "-e", "stg", "--pick", ".query"])
            .env("AILO_SECRET_ALPHA__STG_TOKEN", secret),
    );
    assert_eq!(in_alpha.ok(), format!("key={secret}"));

    // 同じ環境変数のまま beta へ移ると、解決しないので送信前に落ちる。
    let in_beta = Run::of_command(
        sb.command()
            .current_dir(&b)
            .args(["get", &url, "-e", "stg", "--pick", ".query"])
            .env("AILO_SECRET_ALPHA__STG_TOKEN", secret),
    );
    assert_ne!(in_beta.code, 0, "beta 側で解決してしまっている");
    assert!(
        !in_beta.stdout.contains(secret) && !in_beta.stderr.contains(secret),
        "秘匿値が出力に出ている"
    );
}

/// **秘匿値の索引も workspace をまたがないこと。**
///
/// 索引は「どのキーを預けたか」の一覧で、値は持たないが、
/// 別プロジェクトのキー名が見えること自体が分離の破れ。
#[test]
fn the_secret_index_does_not_leak_between_workspaces() {
    let sb = Sandbox::new();
    let a = sb.dir("project-a");
    let b = sb.dir("project-b");
    sb.mark(&a, "alpha");
    sb.mark(&b, "beta");

    // alpha の索引にだけキー名を入れる(値はキーチェーン側なのでここには無い)。
    std::fs::write(
        sb.workspace_config_dir("alpha").join("secret-index.toml"),
        "[envs]\nstg = [\"alpha-key\"]\n",
    )
    .unwrap();

    assert!(sb.run_in(&a, &["secret", "ls"]).ok().contains("alpha-key"));
    let in_beta = sb.run_in(&b, &["secret", "ls"]).ok().to_string();
    assert!(!in_beta.contains("alpha-key"), "{in_beta}");
    let in_default = sb.run(&["secret", "ls"]).ok().to_string();
    assert!(!in_default.contains("alpha-key"), "{in_default}");
}

/// ダンプの索引も workspace をまたがないこと。`AILO_DUMP_DIR` で上書きしても同じ。
#[test]
fn dumps_do_not_leak_between_workspaces_even_with_an_override() {
    let server = TestServer::start();
    let sb = Sandbox::new();
    let a = sb.dir("project-a");
    let b = sb.dir("project-b");
    sb.mark(&a, "alpha");
    sb.mark(&b, "beta");
    let shared = sb.dir("shared-dumps");

    Run::of_command(
        sb.command()
            .current_dir(&a)
            .args(["get", &server.url("/alpha-only")])
            .env("AILO_DUMP_DIR", &shared),
    )
    .ok();

    let log_in_beta = Run::of_command(
        sb.command()
            .current_dir(&b)
            .args(["log"])
            .env("AILO_DUMP_DIR", &shared),
    );
    log_in_beta.ok();
    assert!(
        !log_in_beta.stdout.contains("/alpha-only"),
        "別 workspace のダンプが見えている:\n{}",
        log_in_beta.stdout
    );
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

/// **`ailo new` も `ailo save` と同じ秘匿値のガードを通ること。**
///
/// 新しい入口だけが素通しだと、既存の安全境界を迂回できてしまう。
#[test]
fn new_refuses_a_definition_with_a_literal_secret() {
    let sb = Sandbox::new();
    let editor = sb.editor(
        "printf 'method = \"GET\"\nurl = \"https://example.com\"\nitems = [\"Authorization: Bearer real-secret\"]\n' > \"$1\"\n",
    );
    let run = Run::of_command(sb.command().args(["new", "leaky"]).env("EDITOR", &editor));

    assert_ne!(run.code, 0, "平文の秘匿値が登録できてしまった");
    assert!(run.stderr.contains("ailo secret set"), "{}", run.stderr);
    assert!(sb.run(&["ls"]).ok().contains("ありません"));
}

/// **既定の workspace が、名前付きの秘匿値を読まないこと。**
///
/// 既定の環境変数の接頭辞は短いので、環境名を workspace 名と同じにするだけで
/// `AILO_SECRET_<WS>__<ENV>_<KEY>` を丸ごと飲み込めてしまう（実際に読めていた）。
#[test]
fn the_default_workspace_cannot_read_a_named_workspaces_secret() {
    let server = TestServer::start();
    let sb = Sandbox::new();
    let plain = sb.dir("no-marker");

    let run = Run::of_command(
        sb.command()
            .current_dir(&plain)
            .args([
                "get",
                &format!("{}?k={{{{_stg_token}}}}", server.url("/reflect")),
                "-e",
                "alpha",
                "--pick",
                ".query",
            ])
            .env("AILO_SECRET_ALPHA__STG_TOKEN", "leaked-secret"),
    );
    assert_ne!(run.code, 0, "既定 workspace が名前付きの秘匿値を読んでいる");
    assert!(
        !run.stdout.contains("leaked-secret") && !run.stderr.contains("leaked-secret"),
        "秘匿値が出力に出ている"
    );
}

/// 接頭辞が重なる名前どうしでも混ざらないこと。
///
/// 区切りが 1 文字だと、workspace `a` + 環境 `b-c` と workspace `a-b` + 環境 `c` が
/// 同じ環境変数名になる。
#[test]
fn workspaces_with_overlapping_prefixes_do_not_share_secrets() {
    let server = TestServer::start();
    let sb = Sandbox::new();
    let short = sb.dir("short");
    let long = sb.dir("long");
    sb.mark(&short, "a");
    sb.mark(&long, "a-b");

    let url = format!("{}?k={{{{token}}}}", server.url("/reflect"));

    // workspace `a` / 環境 `b-c` に置いた値。
    let owner = Run::of_command(
        sb.command()
            .current_dir(&short)
            .args(["get", &url, "-e", "b-c", "--pick", ".query"])
            .env("AILO_SECRET_A__B_C_TOKEN", "owned-by-a"),
    );
    assert_eq!(owner.ok(), "k=owned-by-a");

    // workspace `a-b` / 環境 `c` からは見えないこと。
    let other = Run::of_command(
        sb.command()
            .current_dir(&long)
            .args(["get", &url, "-e", "c", "--pick", ".query"])
            .env("AILO_SECRET_A__B_C_TOKEN", "owned-by-a"),
    );
    assert_ne!(other.code, 0, "別 workspace が同じ環境変数を読んでいる");
    assert!(!other.stdout.contains("owned-by-a"));
}

/// `AILO_WORKSPACE` が `.ailo` より強く、`--workspace` より弱いこと。
#[test]
fn the_environment_variable_sits_between_the_flag_and_the_marker() {
    let sb = Sandbox::new();
    let dir = sb.dir("project");
    sb.mark(&dir, "from-marker");

    sb.run_in(&dir, &["config", "set", "who", "marker"]).ok();
    Run::of_command(
        sb.command()
            .current_dir(&dir)
            .args(["config", "set", "who", "envvar"])
            .env("AILO_WORKSPACE", "from-env"),
    )
    .ok();

    // 環境変数が `.ailo` に勝つ。
    let from_env = Run::of_command(
        sb.command()
            .current_dir(&dir)
            .args(["config", "get", "who"])
            .env("AILO_WORKSPACE", "from-env"),
    );
    assert_eq!(from_env.ok(), "envvar");

    // フラグが環境変数に勝つ。
    let from_flag = Run::of_command(
        sb.command()
            .current_dir(&dir)
            .args(["-w", "from-marker", "config", "get", "who"])
            .env("AILO_WORKSPACE", "from-env"),
    );
    assert_eq!(from_flag.ok(), "marker");
}

/// 読めない `.ailo` を「無い」として既定へ落とさないこと。
///
/// 黙って落ちると、マーカーがあるのに既定へ書き、以後の秘匿値も別の名前空間になる。
#[test]
fn a_marker_that_cannot_be_read_stops_rather_than_falling_back() {
    let sb = Sandbox::new();
    let dir = sb.dir("bad-perm");
    // ディレクトリにして「あるが読めない」を作る。
    std::fs::create_dir(dir.join(".ailo")).unwrap();

    let run = sb.run_in(&dir, &["config", "set", "who", "silent-default"]);
    assert_ne!(run.code, 0, "黙って既定 workspace に書いている");
    assert!(run.stderr.contains(".ailo"), "{}", run.stderr);
}

/// いまどの workspace を見ているかが、空の一覧に出ること。
#[test]
fn an_empty_listing_says_which_workspace_it_looked_in() {
    let sb = Sandbox::new();
    let dir = sb.dir("project");
    sb.mark(&dir, "alpha");

    assert!(sb.run_in(&dir, &["ls"]).ok().contains("alpha"));
    // 既定は括弧を二重にしない。
    let default = sb.run(&["ls"]).ok().to_string();
    assert!(default.contains("既定"), "{default}");
    assert!(!default.contains("((既定))"), "{default}");
}
