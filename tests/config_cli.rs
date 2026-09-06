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
    run.ok();
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
