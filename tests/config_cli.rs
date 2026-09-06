//! `ailo config` と `ailo env use` の結合テスト。
//!
//! ここで確かめたいのは 1 つ。**変数の登録から使用までが、設定ファイルを一度も
//! 開かずにコマンドだけで完結すること。** 秘匿値には `ailo secret set` があるのに
//! 普通の変数はファイルを手で書くしかない、という非対称が実際に人を詰まらせた。

mod support;

use support::{Run, Sandbox, TestServer};

/// 登録 → 既定の環境の切り替え → 実際のリクエストまで、コマンドだけで通ること。
#[test]
fn a_variable_can_be_registered_and_used_without_opening_the_file() {
    let server = TestServer::start();
    let sb = Sandbox::new();

    sb.run(&["config", "set", "-e", "stg", "base_url", &server.url("")])
        .ok();
    sb.run(&["config", "set", "api_version", "v1"]).ok();
    sb.run(&["env", "use", "stg"]).ok();

    let run = sb.run(&[
        "get",
        "{{base_url}}/reflect?v={{api_version}}",
        "--pick",
        ".query",
    ]);
    assert_eq!(run.ok(), "v=v1");
}

/// 短いキーは `-e` の有無で置き場所が変わること。
#[test]
fn a_short_key_lands_in_the_common_or_the_environment_table() {
    let sb = Sandbox::new();
    sb.run(&["config", "set", "who", "common"]).ok();
    sb.run(&["config", "set", "-e", "stg", "who", "env"]).ok();

    assert_eq!(sb.run(&["config", "get", "who"]).ok(), "common");
    assert_eq!(sb.run(&["config", "get", "-e", "stg", "who"]).ok(), "env");
    // フルパスでも同じ場所を指せる。エージェント向けの曖昧さのない書き方。
    assert_eq!(sb.run(&["config", "get", "env.stg.vars.who"]).ok(), "env");
}

/// フルパスなら `vars` 以外にも書けること。
#[test]
fn a_full_path_can_reach_tables_other_than_vars() {
    let sb = Sandbox::new();
    sb.run(&["config", "set", "env.stg.headers.Accept", "text/csv"])
        .ok();
    assert_eq!(
        sb.run(&["config", "get", "env.stg.headers.Accept"]).ok(),
        "text/csv"
    );
}

/// 一覧は `名前=値` の平らな並びで、環境で絞れること。
#[test]
fn the_listing_is_flat_and_can_be_narrowed_to_one_environment() {
    let sb = Sandbox::new();
    sb.run(&["config", "set", "who", "common"]).ok();
    sb.run(&["config", "set", "-e", "stg", "base_url", "http://x"])
        .ok();

    let all = sb.run(&["config", "list"]).ok().to_string();
    assert!(all.contains("vars.who=common"), "{all}");
    assert!(all.contains("env.stg.vars.base_url=http://x"), "{all}");

    let only_stg = sb.run(&["config", "list", "-e", "stg"]).ok().to_string();
    assert_eq!(only_stg, "env.stg.vars.base_url=http://x");
}

/// 消せること。無いものを消したときは、それと分かること。
#[test]
fn a_value_can_be_removed_and_absence_is_reported() {
    let sb = Sandbox::new();
    sb.run(&["config", "set", "who", "taro"]).ok();
    sb.run(&["config", "unset", "who"]).ok();
    assert_eq!(sb.run(&["config", "list"]).ok(), "");

    let run = sb.run(&["config", "unset", "who"]);
    assert_eq!(run.code, 1, "無いものを消したことが終了コードに出ていない");
    assert!(run.stderr.contains("ありません"), "{}", run.stderr);
}

/// **秘匿らしいキー名は弾き、キーチェーンへ誘導すること。**
///
/// 平文の設定ファイルに token を書けてしまうと、`ailo secret set` がある意味がない。
#[test]
fn a_secret_looking_key_is_refused_and_points_at_the_keychain() {
    let sb = Sandbox::new();
    let run = sb.run(&["config", "set", "-e", "prd", "token", "abc123"]);
    assert_ne!(run.code, 0);
    assert!(run.stderr.contains("ailo secret set"), "{}", run.stderr);
    // 弾いたのだから、ファイルには何も残っていないこと。
    assert_eq!(sb.run(&["config", "list"]).ok(), "");
}

/// テンプレートは通ること。値そのものはキーチェーンにあるので平文ではない。
#[test]
fn a_header_template_referring_to_a_secret_is_allowed() {
    let sb = Sandbox::new();
    sb.run(&[
        "config",
        "set",
        "env.prd.headers.Authorization",
        "Bearer {{access_token}}",
    ])
    .ok();
    assert_eq!(
        sb.run(&["config", "get", "env.prd.headers.Authorization"])
            .ok(),
        "Bearer {{access_token}}"
    );
}

/// フルパスと `-e` の併用は、黙って片方を選ばずに拒むこと。
#[test]
fn a_full_path_together_with_an_environment_is_refused() {
    let sb = Sandbox::new();
    let run = sb.run(&["config", "set", "-e", "stg", "headers.Accept", "text/csv"]);
    assert_ne!(run.code, 0);
    assert!(run.stderr.contains("-e"), "{}", run.stderr);
}

/// 打ち間違えた環境名で切り替わらないこと。
///
/// 通してしまうと、以降のリクエストが全部 `base_url` 未解決で落ちる。
/// 原因が切り替えにあるとは気づきにくい。
#[test]
fn switching_to_an_unknown_environment_is_refused_and_lists_what_exists() {
    let sb = Sandbox::new();
    sb.run(&["config", "set", "-e", "stg", "base_url", "http://x"])
        .ok();

    let run = sb.run(&["env", "use", "prod"]);
    assert_ne!(run.code, 0);
    assert!(run.stderr.contains("stg"), "{}", run.stderr);
    // 既定は変わっていないこと。
    assert!(!sb.run(&["config", "list"]).ok().contains("default_env"));
}

/// 設定に手で書いたコメントと並びが、`config set` で消えないこと。
#[test]
fn hand_written_comments_survive_a_config_set() {
    let sb = Sandbox::new();
    sb.write_config("# 手で書いたメモ\n[vars]\n# API の版\napi_version = \"v1\"\n");
    sb.run(&["config", "set", "who", "taro"]).ok();

    let text = std::fs::read_to_string(sb.config_dir().join("config.toml")).unwrap();
    assert!(text.contains("# 手で書いたメモ"), "{text}");
    assert!(text.contains("# API の版"), "{text}");
    assert!(text.contains("who = \"taro\""), "{text}");
}

/// `config edit` が保存内容を検証し、正しければ本体に書くこと。
#[test]
fn config_edit_saves_what_the_editor_wrote() {
    let sb = Sandbox::new();
    let editor = sb.editor("printf '[vars]\\nwho = \"taro\"\\n' > \"$1\"\n");

    Run::of_command(sb.command().args(["config", "edit"]).env("EDITOR", &editor)).ok();
    assert_eq!(sb.run(&["config", "get", "who"]).ok(), "taro");
}

/// **壊れた内容は本体に書かず、書いたものも捨てないこと。**
#[test]
fn config_edit_refuses_a_broken_document_and_keeps_the_original() {
    let sb = Sandbox::new();
    sb.run(&["config", "set", "who", "taro"]).ok();

    let editor = sb.editor("printf 'これは TOML ではない\\n' > \"$1\"\n");
    let run = Run::of_command(sb.command().args(["config", "edit"]).env("EDITOR", &editor));
    assert_ne!(run.code, 0);
    // 元の設定は無事。
    assert_eq!(sb.run(&["config", "get", "who"]).ok(), "taro");
    // 書いたものの居場所が伝わること。
    assert!(run.stderr.contains("config.edit"), "{}", run.stderr);
}

/// 綴りを間違えた設定も、書き込む前に弾くこと。
#[test]
fn config_edit_refuses_a_document_that_would_not_load() {
    let sb = Sandbox::new();
    let editor = sb.editor("printf 'defaultenv = \"stg\"\\n' > \"$1\"\n");
    let run = Run::of_command(sb.command().args(["config", "edit"]).env("EDITOR", &editor));
    assert_ne!(run.code, 0);
    assert!(run.stderr.contains("設定として読めない"), "{}", run.stderr);
}

/// 編集用の一時ファイルも 0600 で作られること。
///
/// 中身は設定ファイルの複製。本体を 0600 で書きながらここが 0644 では、
/// 編集している間だけ同じ内容が誰にでも読める状態になる。
#[test]
fn the_file_handed_to_the_editor_is_not_world_readable() {
    let sb = Sandbox::new();
    // エディタ自身に権限を測らせ、その結果を設定として書き戻させる。
    let editor = sb.editor(
        "m=$(stat -f %Lp \"$1\" 2>/dev/null || stat -c %a \"$1\")\n\
         printf '[vars]\\nmode = \"%s\"\\n' \"$m\" > \"$1\"\n",
    );

    Run::of_command(sb.command().args(["config", "edit"]).env("EDITOR", &editor)).ok();
    assert_eq!(sb.run(&["config", "get", "mode"]).ok(), "600");
}

/// 同時に走った `config set` が互いの結果を捨て合わないこと。
///
/// **主利用者はエージェントで、セットアップを並列に流すのは普通の使い方。**
/// `write_private` が保証するのは 1 回の書き込みの原子性だけで、
/// 「読む → 変える → 書き戻す」の原子性ではない。
#[test]
fn concurrent_writes_do_not_lose_each_other() {
    let sb = Sandbox::new();
    let children: Vec<_> = (0..8)
        .map(|i| {
            sb.command()
                .args(["config", "set", &format!("k{i}"), &format!("v{i}")])
                .spawn()
                .expect("ailo を起動できない")
        })
        .collect();
    for mut child in children {
        let status = child.wait().unwrap();
        assert!(status.success(), "{status}");
    }

    let listed = sb.run(&["config", "list"]).ok().to_string();
    for i in 0..8 {
        assert!(
            listed.contains(&format!("vars.k{i}=v{i}")),
            "k{i} が消えている:\n{listed}"
        );
    }
}

/// `config set default_env` も、`env use` と同じく存在しない環境名を弾くこと。
///
/// エージェントにはフルパスを勧めているので、検証がこちらに無いと
/// **勧めたほうの経路にだけガードが無い**ことになる。
#[test]
fn setting_default_env_directly_is_validated_too() {
    let sb = Sandbox::new();
    sb.run(&["config", "set", "-e", "stg", "base_url", "http://x"])
        .ok();

    let run = sb.run(&["config", "set", "default_env", "prod"]);
    assert_ne!(run.code, 0);
    assert!(run.stderr.contains("stg"), "{}", run.stderr);
    assert!(!sb.run(&["config", "list"]).ok().contains("default_env"));

    sb.run(&["config", "set", "default_env", "stg"]).ok();
}

/// 「無い」と「空文字が入っている」を終了コードで区別できること。
#[test]
fn a_missing_value_and_an_empty_value_are_told_apart_by_the_exit_code() {
    let sb = Sandbox::new();
    assert_eq!(sb.run(&["config", "get", "nope"]).code, 1);

    sb.run(&["config", "set", "empty", ""]).ok();
    let found = sb.run(&["config", "get", "empty"]);
    assert_eq!(found.code, 0);
    assert_eq!(found.stdout, "\n");

    // 消すときも同じ。
    assert_eq!(sb.run(&["config", "unset", "nope"]).code, 1);
    assert_eq!(sb.run(&["config", "unset", "empty"]).code, 0);
}

/// テンプレートを混ぜただけの秘匿値は通さないこと。
#[test]
fn a_secret_with_a_template_glued_on_does_not_slip_through() {
    let sb = Sandbox::new();
    let run = sb.run(&["config", "set", "-e", "prd", "token", "sk-live-abcdef{{}}"]);
    assert_ne!(run.code, 0, "平文の秘匿値が通ってしまった");
    assert!(run.stderr.contains("ailo secret set"), "{}", run.stderr);
    assert_eq!(sb.run(&["config", "list"]).ok(), "");
}

/// 改行を含む値は拒むこと。`config list` の 1 行 1 件が壊れる。
#[test]
fn a_value_with_a_newline_is_refused() {
    let sb = Sandbox::new();
    let run = sb.run(&["config", "set", "note", "1 行目\n2 行目=x"]);
    assert_ne!(run.code, 0);
    assert!(run.stderr.contains("制御文字"), "{}", run.stderr);
}

/// エディタが保存後に異常終了しても、書いたものを捨てないこと。
///
/// `vim` の `:cq`、クラッシュ、接続断はどれも「保存済み・非ゼロ終了」になる。
#[test]
fn config_edit_keeps_the_work_when_the_editor_exits_with_a_failure() {
    let sb = Sandbox::new();
    sb.run(&["config", "set", "who", "taro"]).ok();

    let editor = sb.editor("printf '[vars]\\nwho = \"hanako\"\\n' > \"$1\"\nexit 1\n");
    let run = Run::of_command(sb.command().args(["config", "edit"]).env("EDITOR", &editor));
    assert_ne!(run.code, 0);
    // 本体は変わっていない。
    assert_eq!(sb.run(&["config", "get", "who"]).ok(), "taro");
    // 書いたものの居場所が伝わる。
    assert!(run.stderr.contains("config.edit"), "{}", run.stderr);
}

/// 何も書き換えずにエディタが失敗したときは、残骸を置いていかないこと。
#[test]
fn config_edit_leaves_nothing_behind_when_the_editor_changed_nothing() {
    let sb = Sandbox::new();
    let editor = sb.editor("exit 1\n");
    let run = Run::of_command(sb.command().args(["config", "edit"]).env("EDITOR", &editor));
    assert_ne!(run.code, 0);

    let leftovers: Vec<_> = std::fs::read_dir(sb.config_dir())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("config.edit"))
        .collect();
    assert!(leftovers.is_empty(), "残骸がある: {leftovers:?}");
}
