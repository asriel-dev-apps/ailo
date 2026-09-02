//! コマンドライン解釈のテスト。
//!
//! ここが壊れると、指定したはずのフラグが黙って無視される。実際に一度壊した箇所なので
//! 「フラグを item の後ろに書ける」ことを回帰テストとして固定しておく。

use clap::Parser;

use ailo::cli::{Cli, Command};

fn parse(argv: &[&str]) -> Cli {
    Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("解釈できない: {argv:?}\n{e}"))
}

#[test]
fn flags_may_appear_after_items() {
    let cli = parse(&[
        "ailo",
        "post",
        "https://example.com/u",
        "name=taro",
        "--pick",
        ".id",
    ]);
    let (method, args) = cli.command.as_request().expect("リクエスト系");
    assert_eq!(method, "POST");
    assert_eq!(args.items, vec!["name=taro"]);
    assert_eq!(args.common.pick.as_deref(), Some(".id"));
}

#[test]
fn flags_may_appear_before_items() {
    let cli = parse(&[
        "ailo",
        "post",
        "https://example.com/u",
        "--form",
        "name=taro",
    ]);
    let (_, args) = cli.command.as_request().unwrap();
    assert!(args.form);
    assert_eq!(args.items, vec!["name=taro"]);
}

#[test]
fn header_items_survive_argument_parsing() {
    let cli = parse(&[
        "ailo",
        "get",
        "https://example.com/u",
        "Authorization: Bearer x",
        "--no-redact",
    ]);
    let (_, args) = cli.command.as_request().unwrap();
    assert_eq!(args.items, vec!["Authorization: Bearer x"]);
    assert!(args.common.no_redact);
}

#[test]
fn every_http_method_maps_to_its_verb() {
    for (sub, verb) in [
        ("get", "GET"),
        ("post", "POST"),
        ("put", "PUT"),
        ("patch", "PATCH"),
        ("delete", "DELETE"),
        ("head", "HEAD"),
        ("options", "OPTIONS"),
    ] {
        let cli = parse(&["ailo", sub, "https://example.com"]);
        assert_eq!(cli.command.as_request().unwrap().0, verb);
    }
}

#[test]
fn pick_and_shape_cannot_be_combined() {
    // どちらを優先するかを黙って決めない。
    let result = Cli::try_parse_from([
        "ailo",
        "get",
        "https://example.com",
        "--pick",
        ".a",
        "--shape",
    ]);
    assert!(result.is_err());
}

#[test]
fn full_overrides_the_head_line_limit() {
    let cli = parse(&["ailo", "get", "https://example.com", "--full"]);
    let (_, args) = cli.command.as_request().unwrap();
    assert_eq!(args.common.head_lines(), usize::MAX);
}

#[test]
fn log_and_show_are_not_request_commands() {
    assert!(parse(&["ailo", "log"]).command.as_request().is_none());
    assert!(parse(&["ailo", "show", "1"]).command.as_request().is_none());
    assert!(matches!(parse(&["ailo", "log"]).command, Command::Log(_)));
}
