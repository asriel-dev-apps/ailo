//! TUI の規則を、端末を立てずに確かめる。
//!
//! ratatui のイベントループを回さないと確かめられない作りにすると、キー操作も
//! マスクも結局テストされないまま残る。判断は `on_key` と `fold` に寄せてある。

use std::collections::BTreeMap;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::model::{tab_lines, App, Entry, Mode, Pane, Tab};
use super::{fold, on_key, Action};
use crate::config::{Requests, SavedRequest};
use crate::dump::{BodyRecord, ResponseRecord};
use crate::redact::Redactor;
use crate::run::Performed;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: ratatui::crossterm::event::KeyEventState::NONE,
    }
}

fn req(method: &str, url: &str, items: &[&str]) -> SavedRequest {
    SavedRequest {
        method: method.into(),
        url: url.into(),
        items: items.iter().map(|s| (*s).to_string()).collect(),
        ..SavedRequest::default()
    }
}

fn app(names: &[&str]) -> App {
    let entries = names
        .iter()
        .map(|n| Entry {
            name: (*n).to_string(),
            req: req("GET", "http://x/", &[]),
        })
        .collect();
    App::new(entries, "既定", Some("stg".into()))
}

// ------------------------------------------------------------------ 選択と絞り込み

#[test]
fn moving_down_wraps_at_the_end() {
    let mut a = app(&["one", "two"]);
    assert_eq!(a.selected().unwrap().name, "one");
    on_key(&mut a, key(KeyCode::Down));
    assert_eq!(a.selected().unwrap().name, "two");
    on_key(&mut a, key(KeyCode::Down));
    assert_eq!(a.selected().unwrap().name, "one");
}

#[test]
fn moving_up_from_the_top_wraps_to_the_end() {
    let mut a = app(&["one", "two", "three"]);
    on_key(&mut a, key(KeyCode::Up));
    assert_eq!(a.selected().unwrap().name, "three");
}

/// 回帰: 絞り込みで一覧が短くなったとき、選択位置が範囲外に残ると
/// **何も選ばれていないのに Enter が黙って効かない**。畳み直すこと。
#[test]
fn narrowing_the_filter_pulls_the_selection_back_into_range() {
    let mut a = app(&["alpha", "beta", "gamma"]);
    on_key(&mut a, key(KeyCode::Down));
    on_key(&mut a, key(KeyCode::Down));
    assert_eq!(a.selected().unwrap().name, "gamma");

    on_key(&mut a, key(KeyCode::Char('/')));
    for c in "alp".chars() {
        on_key(&mut a, key(KeyCode::Char(c)));
    }
    assert_eq!(a.visible().len(), 1);
    assert_eq!(
        a.selected().map(|e| e.name.clone()),
        Some("alpha".to_string()),
        "選択が範囲外に残っている"
    );
}

#[test]
fn filter_ignores_letter_case() {
    let mut a = app(&["Login", "logout"]);
    on_key(&mut a, key(KeyCode::Char('/')));
    for c in "LOG".chars() {
        on_key(&mut a, key(KeyCode::Char(c)));
    }
    assert_eq!(a.visible().len(), 2);
}

#[test]
fn escape_cancels_the_filter_and_restores_the_whole_list() {
    let mut a = app(&["alpha", "beta"]);
    on_key(&mut a, key(KeyCode::Char('/')));
    on_key(&mut a, key(KeyCode::Char('z')));
    assert_eq!(a.visible().len(), 0);
    on_key(&mut a, key(KeyCode::Esc));
    assert_eq!(a.mode, Mode::Normal);
    assert_eq!(a.visible().len(), 2);
}

/// 絞り込み中の `q` は文字であって終了ではない。
#[test]
fn typing_q_while_filtering_does_not_quit() {
    let mut a = app(&["query"]);
    on_key(&mut a, key(KeyCode::Char('/')));
    let action = on_key(&mut a, key(KeyCode::Char('q')));
    assert_eq!(action, Action::None);
    assert!(!a.quit);
    assert_eq!(a.filter, "q");
}

#[test]
fn enter_on_an_empty_list_asks_to_send_but_finds_nothing() {
    let mut a = app(&[]);
    assert_eq!(on_key(&mut a, key(KeyCode::Enter)), Action::Send);
    assert!(a.selected().is_none(), "空なのに選択がある");
}

#[test]
fn reloading_after_an_edit_keeps_the_selection_in_range() {
    let mut a = app(&["one", "two", "three"]);
    on_key(&mut a, key(KeyCode::Down));
    on_key(&mut a, key(KeyCode::Down));
    a.reload(vec![Entry {
        name: "one".into(),
        req: req("GET", "http://x/", &[]),
    }]);
    assert_eq!(a.selected().unwrap().name, "one");
}

// ---------------------------------------------------------------------- タブ

#[test]
fn tab_cycles_forward_and_backward() {
    let mut a = app(&["x"]);
    assert_eq!(a.tab, Tab::Body);
    on_key(&mut a, key(KeyCode::Tab));
    assert_eq!(a.tab, Tab::Headers);
    on_key(&mut a, key(KeyCode::BackTab));
    assert_eq!(a.tab, Tab::Body);
    // 先頭から戻ると末尾へ回る。
    on_key(&mut a, key(KeyCode::BackTab));
    assert_eq!(a.tab, Tab::Capture);
}

/// 項目の振り分け。ヘッダが Body に出たり、クエリが消えたりすると、
/// 「送っていないつもりのものが送られている」に気づけない。
#[test]
fn items_are_sorted_into_the_tab_they_belong_to() {
    let r = req(
        "POST",
        "http://x/",
        &["name=taro", "age:=30", "X-Trace: abc", "limit==50"],
    );
    let body = tab_lines(&r, Tab::Body);
    assert!(body.contains(&"name=taro".to_string()), "{body:?}");
    assert!(body.contains(&"age:=30".to_string()), "{body:?}");
    assert!(!body.contains(&"X-Trace: abc".to_string()), "{body:?}");
    assert!(!body.contains(&"limit==50".to_string()), "{body:?}");

    assert_eq!(tab_lines(&r, Tab::Headers), vec!["X-Trace: abc"]);
    assert_eq!(tab_lines(&r, Tab::Query), vec!["limit==50"]);
}

#[test]
fn a_raw_body_shows_up_under_body() {
    let mut r = req("POST", "http://x/", &[]);
    r.raw = Some("{\"a\":1}".into());
    assert_eq!(tab_lines(&r, Tab::Body), vec!["{\"a\":1}"]);
}

/// **テンプレートのまま出す。** 展開して見せると画面に秘匿値が出る。
#[test]
fn variables_are_shown_unexpanded() {
    let r = req(
        "GET",
        "{{base_url}}/me",
        &["Authorization: Bearer {{token}}"],
    );
    assert_eq!(
        tab_lines(&r, Tab::Headers),
        vec!["Authorization: Bearer {{token}}"]
    );
    assert_eq!(r.url, "{{base_url}}/me");
}

/// capture 欄は「どこへ入るか」まで出すが、値そのものは持たない。
#[test]
fn capture_shows_names_and_marks_the_ones_that_go_to_the_keychain() {
    let mut r = req("POST", "http://x/", &[]);
    r.capture = BTreeMap::from([
        ("access_token".into(), ".data.token".into()),
        ("user_id".into(), ".data.id".into()),
    ]);
    r.secret = vec!["access_token".into()];

    let lines = tab_lines(&r, Tab::Capture);
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("access_token = .data.token"), "{lines:?}");
    assert!(lines[0].contains("キーチェーン"), "{lines:?}");
    assert!(!lines[1].contains("キーチェーン"), "{lines:?}");
}

// -------------------------------------------------------------- 一覧の読み口

/// 一覧は `Requests` そのもの。TUI が別の読み口を持たないこと。
/// workspace ごとの分離は `Requests::load()` 側にあり、`tests/workspace_cli.rs`
/// で確かめてある。ここで別経路を作ると、その分離が片方だけ破れる。
#[test]
fn the_list_is_exactly_what_requests_holds() {
    let mut reqs = Requests::default();
    reqs.put("login", req("POST", "http://x/token", &[]));
    reqs.put("me", req("GET", "http://x/me", &[]));

    let entries = super::entries_of(&reqs);
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["login", "me"]);
    assert_eq!(entries[0].req.method, "POST");
}

// ------------------------------------------------------------------ マスク

fn performed(body: serde_json::Value, redactor: Redactor) -> Performed {
    Performed {
        response: ResponseRecord {
            status: 200,
            status_text: "OK".into(),
            headers: BTreeMap::from([(
                "content-type".into(),
                "application/json; charset=utf-8".into(),
            )]),
            body: BodyRecord::Json { value: body },
            bytes: 42,
            ms: 12,
        },
        dump_path: None,
        redactor,
        notes: Vec::new(),
    }
}

/// **画面に出るものは必ずマスク済み。** `fold` が唯一の変換点なので、
/// ここが素通しなら TUI からは全部見えてしまう。
#[test]
fn the_response_shown_on_screen_is_masked() {
    let mut r = Redactor::new(true);
    r.add_literal("s3cr3t-token-value");
    let pane = fold(performed(
        serde_json::json!({"access_token": "s3cr3t-token-value", "id": 7}),
        r,
    ));

    let Pane::Done { body, shape, .. } = &pane else {
        panic!("送信できていない: {pane:?}");
    };
    assert!(!body.contains("s3cr3t"), "秘匿値が画面に出ている: {body}");
    // 形のほうにも値は出ない(形はキー名と型だけ)。
    let shape = shape.as_deref().unwrap_or_default();
    assert!(!shape.contains("s3cr3t"), "{shape}");
    assert!(shape.contains("access_token"), "キー名は出るはず: {shape}");
}

/// 上のテストが「マスクが効いた」ことを本当に見ているかのコントロール。
/// マスクを外せば**必ず**素の値が出ること。出なければ検査自体が死んでいる。
#[test]
fn control_without_masking_the_secret_would_have_been_visible() {
    let pane = fold(performed(
        serde_json::json!({"access_token": "s3cr3t-token-value"}),
        Redactor::disabled(),
    ));
    let Pane::Done { body, .. } = &pane else {
        panic!("{pane:?}");
    };
    assert!(
        body.contains("s3cr3t-token-value"),
        "コントロールが素通りしていない。検査が働いていない: {body}"
    );
}

/// 本文は打ち切る。全文はダンプにある。
#[test]
fn a_long_body_is_cut_so_the_pane_does_not_hold_the_whole_response() {
    let items: Vec<_> = (0..5000).map(|i| serde_json::json!({"id": i})).collect();
    let pane = fold(performed(
        serde_json::json!({ "items": items }),
        Redactor::new(true),
    ));
    let Pane::Done { body, .. } = &pane else {
        panic!("{pane:?}");
    };
    assert!(
        body.lines().count() <= super::BODY_LINES,
        "{}",
        body.lines().count()
    );
}

// ------------------------------------------------------------------ 終了

#[test]
fn q_and_ctrl_c_both_quit() {
    let mut a = app(&["x"]);
    assert_eq!(on_key(&mut a, key(KeyCode::Char('q'))), Action::Quit);
    assert!(a.quit);

    let mut a = app(&["x"]);
    let ctrl_c = KeyEvent {
        code: KeyCode::Char('c'),
        modifiers: KeyModifiers::CONTROL,
        kind: KeyEventKind::Press,
        state: ratatui::crossterm::event::KeyEventState::NONE,
    };
    assert_eq!(on_key(&mut a, ctrl_c), Action::Quit);
    assert!(a.quit);
}

#[test]
fn e_opens_the_editor_for_the_selected_request() {
    let mut a = app(&["one", "two"]);
    on_key(&mut a, key(KeyCode::Down));
    assert_eq!(
        on_key(&mut a, key(KeyCode::Char('e'))),
        Action::Edit("two".into())
    );
}

// ------------------------------------------------------------------ 描画

use ratatui::backend::TestBackend;
use ratatui::Terminal;

/// 描いた画面を文字列にする。TestBackend なので端末は要らない。
fn screen(app: &App, width: u16, height: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(width, height)).expect("TestBackend");
    t.draw(|f| super::view::draw(f, app)).expect("描けない");
    let buf = t.backend().buffer().clone();
    // **全角は 2 セルを占め、2 セル目には空白が入る。** そのまま連結すると
    // `保存済み` が `保 存 済 み` になり、「画面に出ているのに一致しない」で
    // 検査が空振りする。全角を書いたセルの次は読み飛ばす。
    (0..buf.area.height)
        .map(|y| {
            let mut row = String::new();
            let mut x = 0;
            while x < buf.area.width {
                let sym = buf[(x, y)].symbol();
                row.push_str(sym);
                x += if sym.chars().next().is_some_and(is_wide) {
                    2
                } else {
                    1
                };
            }
            row.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 端末で 2 セル分の幅を取る文字か。罫線(U+2500 台)は 1 セルなので入れない。
fn is_wide(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x3FFFD)
}

fn demo() -> App {
    let mut login = req(
        "POST",
        "{{base_url}}/tokens",
        &[
            "user=taro",
            "pass={{password}}",
            "Content-Type: application/json",
        ],
    );
    login.capture = BTreeMap::from([("access_token".into(), ".data.token".into())]);
    login.secret = vec!["access_token".into()];

    App::new(
        vec![
            Entry {
                name: "login".into(),
                req: login,
            },
            Entry {
                name: "issues".into(),
                req: req(
                    "GET",
                    "https://api.github.com/repos/cli/cli/issues",
                    &["Accept: application/vnd.github+json", "per_page==5"],
                ),
            },
        ],
        "vecta",
        Some("stg".into()),
    )
}

/// 4 つの領域（エンドポイント・タブ・定義・レスポンス）が全部出ていること。
#[test]
fn the_screen_has_the_four_areas_it_promises() {
    let out = screen(&demo(), 100, 28);
    assert!(out.contains("workspace: vecta"), "{out}");
    assert!(out.contains("env: stg"), "{out}");
    assert!(out.contains("保存済み"), "{out}");
    assert!(out.contains("エンドポイント"), "{out}");
    assert!(out.contains("{{base_url}}/tokens"), "{out}");
    assert!(out.contains("Body"), "{out}");
    assert!(out.contains("Headers"), "{out}");
    assert!(out.contains("レスポンス"), "{out}");
    assert!(out.contains("Enter 送信"), "{out}");
}

/// 変数は展開せずに出す。展開すると画面に秘匿値が出る。
#[test]
fn the_screen_never_expands_a_variable() {
    let out = screen(&demo(), 100, 28);
    assert!(out.contains("{{password}}"), "{out}");
}

/// 狭い端末では一覧を畳む。潰れた 2 ペインより、片方が読めるほうがよい。
#[test]
fn a_narrow_terminal_drops_the_sidebar_instead_of_squashing_it() {
    let wide = screen(&demo(), 100, 28);
    assert!(wide.contains("保存済み"), "{wide}");

    let narrow = screen(&demo(), 50, 24);
    assert!(!narrow.contains("保存済み"), "畳めていない:\n{narrow}");
    assert!(narrow.contains("エンドポイント"), "{narrow}");
    // 案内は削って全部出す。途中で切れると、消えた項目に気づけない。
    assert!(narrow.contains("q 終了"), "案内が切れている:\n{narrow}");
}

/// 保存済みが 1 件も無いときに、次にやることを出す。
#[test]
fn an_empty_list_says_what_to_do_next() {
    let out = screen(&App::new(Vec::new(), "既定", None), 100, 24);
    assert!(out.contains("ailo new"), "{out}");
}

#[test]
#[ignore = "目視用。`cargo test -- --ignored --nocapture scratch_screen` で見る"]
fn scratch_screen() {
    eprintln!("=== 100x28 ===\n{}", screen(&demo(), 100, 28));
    let mut a = demo();
    a.tab = Tab::Capture;
    eprintln!("=== Capture タブ ===\n{}", screen(&a, 100, 20));
    eprintln!("=== 50x24（狭い）===\n{}", screen(&demo(), 50, 24));
}
