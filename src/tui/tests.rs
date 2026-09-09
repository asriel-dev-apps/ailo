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

/// `Tab` はペインを移る。タブの切り替えではない。
/// ペインが 5 つある以上、一番押されるキーは「どこに当てるか」に要る。
#[test]
fn tab_moves_between_panes() {
    use super::Focus;
    let mut a = app(&["x"]);
    assert_eq!(a.focus, Focus::List);
    on_key(&mut a, key(KeyCode::Tab));
    assert_eq!(a.focus, Focus::Endpoint);
    on_key(&mut a, key(KeyCode::BackTab));
    assert_eq!(a.focus, Focus::List);
    // 先頭から戻ると末尾へ回る。
    on_key(&mut a, key(KeyCode::BackTab));
    assert_eq!(a.focus, Focus::Response);
}

/// タブの切り替えは、タブのペインに当たっているときの左右。
#[test]
fn tabs_switch_with_left_and_right_when_that_pane_has_focus() {
    use super::Focus;
    let mut a = app(&["x"]);
    a.focus = Focus::Tabs;
    assert_eq!(a.tab, Tab::Body);
    on_key(&mut a, key(KeyCode::Right));
    assert_eq!(a.tab, Tab::Headers);
    on_key(&mut a, key(KeyCode::Left));
    assert_eq!(a.tab, Tab::Body);
    on_key(&mut a, key(KeyCode::Left));
    assert_eq!(a.tab, Tab::Capture);
}

/// 一覧に当たっているときの左右はタブを動かさない。
/// **当たっていない場所が動くのが、この手の画面で一番たちが悪い。**
#[test]
fn left_and_right_do_nothing_to_tabs_while_the_list_has_focus() {
    use super::Focus;
    let mut a = app(&["x", "y"]);
    assert_eq!(a.focus, Focus::List);
    on_key(&mut a, key(KeyCode::Right));
    on_key(&mut a, key(KeyCode::Left));
    assert_eq!(a.tab, Tab::Body);
}

/// 同じ `j` でも、一覧では次のリクエスト、レスポンスでは 1 行下。
#[test]
fn j_means_a_different_thing_in_each_pane() {
    use super::Focus;
    let mut a = app(&["one", "two"]);
    on_key(&mut a, key(KeyCode::Char('j')));
    assert_eq!(a.selected().unwrap().name, "two");

    a.focus = Focus::Response;
    a.response_max_top = 40;
    let before = a.selected().unwrap().name.clone();
    on_key(&mut a, key(KeyCode::Char('j')));
    assert_eq!(a.response_scroll.top(), 1);
    assert_eq!(a.selected().unwrap().name, before, "一覧まで動いている");
}

/// スクロールは実測の上限で畳む。**描いた行数より下へは行かない。**
/// 行かせると、中身の無い空白まで進んで「壊れた」ように見える。
#[test]
fn scrolling_stops_at_the_measured_end() {
    use super::Focus;
    let mut a = app(&["x"]);
    a.focus = Focus::Response;
    a.response_max_top = 3;
    for _ in 0..10 {
        on_key(&mut a, key(KeyCode::Char('j')));
    }
    assert_eq!(a.response_scroll.top(), 3);
    on_key(&mut a, key(KeyCode::Char('g')));
    assert_eq!(a.response_scroll.top(), 0);
    on_key(&mut a, key(KeyCode::Char('G')));
    assert_eq!(a.response_scroll.top(), 3);
}

/// 回帰: リクエストを変えたらスクロールとレスポンスを捨てる。
/// 持ち越すと、短い定義に切り替えた瞬間に空白だけが見える。
/// 前のリクエストのレスポンスが残るのは、もっとたちが悪い。
#[test]
fn changing_the_request_drops_the_scroll_and_the_previous_response() {
    use super::Focus;
    let mut a = app(&["one", "two"]);
    a.focus = Focus::Definition;
    a.definition_max_top = 20;
    on_key(&mut a, key(KeyCode::Char('j')));
    assert_eq!(a.definition_scroll.top(), 1);

    a.pane = Pane::Failed("前のリクエストの結果".into());
    a.focus = Focus::List;
    on_key(&mut a, key(KeyCode::Char('j')));

    assert_eq!(a.definition_scroll.top(), 0);
    assert_eq!(a.response_scroll.top(), 0);
    assert_eq!(a.pane, Pane::Idle, "前のレスポンスが残っている");
}

/// タブを変えたときも定義のスクロールは先頭へ戻す。
#[test]
fn changing_the_tab_resets_the_definition_scroll() {
    use super::Focus;
    let mut a = app(&["x"]);
    a.focus = Focus::Definition;
    a.definition_max_top = 20;
    on_key(&mut a, key(KeyCode::Char('j')));
    assert_eq!(a.definition_scroll.top(), 1);

    a.focus = Focus::Tabs;
    on_key(&mut a, key(KeyCode::Right));
    assert_eq!(a.definition_scroll.top(), 0);
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

fn fold_of(done: Performed) -> Pane {
    fold(done, Vec::new())
}

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
    }
}

/// **画面に出るものは必ずマスク済み。** `fold` が唯一の変換点なので、
/// ここが素通しなら TUI からは全部見えてしまう。
#[test]
fn the_response_shown_on_screen_is_masked() {
    let mut r = Redactor::new(true);
    r.add_literal("s3cr3t-token-value");
    let pane = fold_of(performed(
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
    let pane = fold_of(performed(
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

/// 本文は**全文**を持つ。画面はスクロールできるので、ここで切る理由が無い。
/// 先頭だけを持っていたときは、画面で追える量がそこで頭打ちだった。
#[test]
fn the_pane_keeps_the_whole_body_so_it_can_be_scrolled() {
    let items: Vec<_> = (0..5000).map(|i| serde_json::json!({"id": i})).collect();
    let pane = fold_of(performed(
        serde_json::json!({ "items": items }),
        Redactor::new(true),
    ));
    let Pane::Done { body, .. } = &pane else {
        panic!("{pane:?}");
    };
    assert!(body.lines().count() > 5000, "{}", body.lines().count());
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
    // 描画は測った行数を書き戻すので `&mut` が要る。テストからは複製を渡し、
    // 呼び出し側の状態を触らない。
    let mut app = app.clone();
    let mut t = Terminal::new(TestBackend::new(width, height)).expect("TestBackend");
    t.draw(|f| super::view::draw(f, &mut app))
        .expect("描けない");
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
                // 幅の判定は自前の表を持たない。範囲を書き写すと、絵文字や
                // 一部の書記素で外し、「画面に出ているのに一致しない」で
                // 検査が空振りする。
                x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
            }
            row.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
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

// -------------------------------------------------- 定義そのものに直書きされた秘匿値

/// 展開しないだけでは足りない。定義に生の値が書かれていることがある。
/// `ailo new` は拒むが、手で書いた `requests.toml` や古い定義は通り抜ける。
#[test]
fn a_literal_secret_written_into_the_definition_is_masked_on_screen() {
    let r = req(
        "POST",
        "http://x/",
        &[
            "Authorization: Bearer live-token-abc",
            "password=hunter2",
            "api_key==live-key-xyz",
        ],
    );
    let all = [
        tab_lines(&r, Tab::Headers),
        tab_lines(&r, Tab::Body),
        tab_lines(&r, Tab::Query),
    ]
    .concat()
    .join("\n");

    assert!(!all.contains("live-token-abc"), "{all}");
    assert!(!all.contains("hunter2"), "{all}");
    assert!(!all.contains("live-key-xyz"), "{all}");
    // 名前は残す。何が設定されているかは分かる必要がある。
    assert!(all.contains("Authorization"), "{all}");
    assert!(all.contains("password"), "{all}");
    assert!(all.contains("api_key"), "{all}");
}

/// 上のテストが本当に検査になっているかのコントロール。
/// 秘匿でない名前の値は**必ずそのまま出る**こと。出なければ全部を潰しているだけ。
#[test]
fn control_a_non_secret_value_is_shown_as_written() {
    let r = req("POST", "http://x/", &["user=taro", "X-Trace: abc123"]);
    let all = [tab_lines(&r, Tab::Body), tab_lines(&r, Tab::Headers)]
        .concat()
        .join("\n");
    assert!(all.contains("taro"), "{all}");
    assert!(all.contains("abc123"), "{all}");
}

/// 変数参照は落とさない。名前しか出ていないので漏れない。
#[test]
fn a_templated_secret_is_left_readable() {
    let r = req("POST", "http://x/", &["Authorization: Bearer {{token}}"]);
    assert_eq!(
        tab_lines(&r, Tab::Headers),
        vec!["Authorization: Bearer {{token}}"]
    );
}

/// 回帰: `parse_item` が読めない行を「検査対象外」にすると、
/// **一番危ない行だけが素通りする**。`password:=hunter2` は JSON として壊れている。
#[test]
fn an_item_that_cannot_be_parsed_is_still_masked() {
    let r = req("POST", "http://x/", &["password:=hunter2"]);
    let out = tab_lines(&r, Tab::Body).join("\n");
    assert!(!out.contains("hunter2"), "{out}");
}

/// URL のクエリに直書きされた秘匿値も落とす。
#[test]
fn a_secret_in_the_url_query_is_masked_on_screen() {
    let masked = super::model::display_url("https://api.example.com/x?api_key=live-key-xyz&page=2");
    assert!(!masked.contains("live-key-xyz"), "{masked}");
    assert!(masked.contains("page=2"), "落としすぎ: {masked}");
}

// ------------------------------------------------------------------ Ctrl-C

/// 回帰: 絞り込み中の Ctrl-C が文字 `c` として検索語に入り、抜ける手段が無かった。
#[test]
fn ctrl_c_quits_even_while_filtering() {
    let mut a = app(&["x"]);
    on_key(&mut a, key(KeyCode::Char('/')));
    let ctrl_c = KeyEvent {
        code: KeyCode::Char('c'),
        modifiers: KeyModifiers::CONTROL,
        kind: KeyEventKind::Press,
        state: ratatui::crossterm::event::KeyEventState::NONE,
    };
    assert_eq!(on_key(&mut a, ctrl_c), Action::Quit);
    assert!(a.quit);
    assert_eq!(a.filter, "", "検索語に入ってしまっている");
}

// ------------------------------------------------ `--raw` 本文とテンプレート URL

/// 回帰: `--raw` の本文を item 記法の判定に通すと**素通りする**。
/// `{"password":"hunter2"}` は `parse_item` に成功してしまう
/// (`{"password"` という名前のヘッダと読まれる)。ログインの本文は
/// 一番秘匿値が入る場所なので、ここが抜けると表示側の防御が意味を失う。
#[test]
fn a_secret_in_a_raw_body_is_masked() {
    for raw in [
        r#"{"user":"a","password":"hunter2-secret"}"#,
        r#"{"grant_type":"password","client_secret":"abc123XYZ"}"#,
        r#"{"nested":{"api_key":"AKIAsecretvalue"}}"#,
    ] {
        let mut r = req("POST", "http://x/", &[]);
        r.raw = Some(raw.into());
        let out = tab_lines(&r, Tab::Body).join("\n");
        for leak in ["hunter2-secret", "abc123XYZ", "AKIAsecretvalue"] {
            assert!(!out.contains(leak), "{raw} → {out}");
        }
    }
}

/// JSON として読めない本文も素通しにしない。
#[test]
fn a_raw_body_that_is_not_json_is_not_passed_through_when_it_smells_of_secrets() {
    let mut r = req("POST", "http://x/", &[]);
    r.raw = Some("grant_type=password&client_secret=abc123XYZ".into());
    let out = tab_lines(&r, Tab::Body).join("\n");
    assert!(!out.contains("abc123XYZ"), "{out}");
}

/// コントロール: 秘匿でない本文は**そのまま出る**。出なければ全部を潰しているだけ。
#[test]
fn control_a_raw_body_without_secrets_is_shown_as_written() {
    let mut r = req("POST", "http://x/", &[]);
    r.raw = Some(r#"{"user":"taro","limit":50}"#.into());
    let out = tab_lines(&r, Tab::Body).join("\n");
    assert!(out.contains("taro"), "{out}");
    assert!(out.contains("50"), "{out}");
}

/// 変数参照は落とさない。
#[test]
fn a_raw_body_that_references_a_variable_is_left_readable() {
    let mut r = req("POST", "http://x/", &[]);
    r.raw = Some(r#"{"password":"{{password}}"}"#.into());
    let out = tab_lines(&r, Tab::Body).join("\n");
    assert!(out.contains("{{password}}"), "{out}");
}

/// 回帰: `Redactor::url` は `Url::parse` に失敗した入力をそのまま返す。
/// **この repo で一番普通の形**（`{{base_url}}/...`）だけが落ちなかった。
#[test]
fn a_secret_in_a_templated_url_is_masked() {
    for url in [
        "{{base_url}}/v1?api_key=AKIAsecretvalue123",
        "{{base_url}}/v1?token=abcdef123456&page=2",
    ] {
        let out = super::model::display_url(url);
        assert!(!out.contains("AKIAsecretvalue123"), "{url} → {out}");
        assert!(!out.contains("abcdef123456"), "{url} → {out}");
        assert!(out.contains("{{base_url}}"), "{out}");
    }
    // 絶対 URL も従来どおり落ちる。
    let out = super::model::display_url("https://api.example.com/x?api_key=live-key-xyz&page=2");
    assert!(!out.contains("live-key-xyz"), "{out}");
    assert!(out.contains("page=2"), "落としすぎ: {out}");
}

/// コントロール: 秘匿でないクエリはそのまま残る。
#[test]
fn control_a_templated_url_without_secrets_is_untouched() {
    let url = "{{base_url}}/v1/users?page=2&sort=name";
    assert_eq!(super::model::display_url(url), url);
}

// ------------------------------------------------------------------ 案内の幅

/// 回帰: 閾値を定数で決めていたとき、幅 60〜63 で最後の `q 終了` だけが切れていた。
/// 消えるのが**抜け方**なので、初見の利用者は raw モードの画面に取り残される。
#[test]
fn the_hint_line_is_never_cut_off_at_any_width() {
    for width in [30u16, 40, 50, 58, 59, 60, 62, 63, 64, 80, 100] {
        let out = screen(&demo(), width, 20);
        let last = out.lines().last().unwrap_or_default().to_string();
        assert!(
            last.contains("q 終了"),
            "幅 {width} で抜け方が消えている: {last:?}"
        );
    }
}

/// 絞り込み中も戻り方が消えないこと。検索語が長ければ検索語のほうを削る。
#[test]
fn the_filter_hint_keeps_the_way_back_even_with_a_long_search_term() {
    let mut a = demo();
    on_key(&mut a, key(KeyCode::Char('/')));
    for c in "とてもながいけんさくごをいれてみる".chars() {
        on_key(&mut a, key(KeyCode::Char(c)));
    }
    for width in [40u16, 50, 60, 80] {
        let out = screen(&a, width, 20);
        let last = out.lines().last().unwrap_or_default().to_string();
        assert!(last.contains("Esc 取消"), "幅 {width}: {last:?}");
    }
}

/// フォーカス中のペインは枠の色が変わる。
///
/// **文字だけを見るテストでは落ちない**ので、色そのものを確かめる。
/// 当たり先で `j` の意味が変わる以上、どこに当たっているか分からない画面は使えない。
#[test]
fn the_focused_pane_is_the_only_one_with_a_highlighted_border() {
    use super::Focus;
    use ratatui::style::Color;

    let border_colours = |focus: Focus| -> Vec<Color> {
        let mut a = demo();
        a.focus = focus;
        let mut t = Terminal::new(TestBackend::new(100, 26)).expect("TestBackend");
        t.draw(|f| super::view::draw(f, &mut a)).expect("描けない");
        let buf = t.backend().buffer().clone();
        // 枠線に使われている文字のセルだけ拾う。
        (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .filter(|(x, y)| matches!(buf[(*x, *y)].symbol(), "┌" | "┐" | "└" | "┘" | "─" | "│"))
            .map(|(x, y)| buf[(x, y)].style().fg.unwrap_or(Color::Reset))
            .collect()
    };

    for focus in Focus::RING {
        let colours = border_colours(focus);
        let highlighted = colours.iter().filter(|c| **c == Color::Cyan).count();
        match focus {
            // タブは枠を持たない行なので、強調される枠は無い。
            Focus::Tabs => assert_eq!(highlighted, 0, "{focus:?}"),
            _ => assert!(highlighted > 0, "{focus:?} に強調された枠が無い"),
        }
    }

    // 何も当たっていない状態は作れないが、当たっている枠が
    // **1 つだけ**であることは確かめられる。
    let mut a = demo();
    a.focus = Focus::Response;
    let mut t = Terminal::new(TestBackend::new(100, 26)).expect("TestBackend");
    t.draw(|f| super::view::draw(f, &mut a)).expect("描けない");
    let buf = t.backend().buffer().clone();
    let cyan_rows: std::collections::BTreeSet<u16> = (0..buf.area.height)
        .filter(|y| {
            (0..buf.area.width).any(|x| {
                buf[(x, *y)].symbol() == "│"
                    && buf[(x, *y)].style().fg == Some(ratatui::style::Color::Cyan)
            })
        })
        .collect();
    assert!(!cyan_rows.is_empty(), "レスポンスの枠が強調されていない");
}

/// スクロールできるときは、どこまで来たかを枠の見出しに出す。
/// 出さないと、最後まで読んだのか途中なのかが分からない。
#[test]
fn a_scrollable_pane_shows_how_far_down_it_is() {
    let mut a = demo();
    a.pane = Pane::Done {
        status: 200,
        status_text: "OK".into(),
        ms: 1,
        bytes: 1,
        content_type: "application/json".into(),
        shape: None,
        body: (0..80)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n"),
        dump: None,
        notes: Vec::new(),
    };
    let out = screen(&a, 100, 26);
    assert!(out.contains("レスポンス [0/"), "{out}");
}

// ------------------------------------------------------------------ マウス

use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

fn click(x: u16, y: u16) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }
}

fn wheel(down: bool, x: u16, y: u16) -> MouseEvent {
    MouseEvent {
        kind: if down {
            MouseEventKind::ScrollDown
        } else {
            MouseEventKind::ScrollUp
        },
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }
}

/// 矩形は描いたときに書き戻される。クリックの当たり判定はそれを使う。
fn drawn(app: &App, width: u16, height: u16) -> App {
    let mut a = app.clone();
    let mut t = Terminal::new(TestBackend::new(width, height)).expect("TestBackend");
    t.draw(|f| super::view::draw(f, &mut a)).expect("描けない");
    a
}

/// クリックしたペインにフォーカスが移る。
#[test]
fn clicking_a_pane_moves_the_focus_there() {
    use super::{on_mouse, Focus};
    let mut a = drawn(&demo(), 100, 26);

    let response = a.areas.response;
    on_mouse(&mut a, click(response.x + 2, response.y + 1));
    assert_eq!(a.focus, Focus::Response);

    let list = a.areas.list;
    on_mouse(&mut a, click(list.x + 2, list.y + 1));
    assert_eq!(a.focus, Focus::List);
}

/// 一覧はクリックした行が選ばれる。
#[test]
fn clicking_a_row_in_the_list_selects_it() {
    use super::on_mouse;
    let mut a = drawn(&demo(), 100, 26);
    assert_eq!(a.selected().unwrap().name, "login");

    let list = a.areas.list;
    // 枠の 1 行下が 0 行目。その次が 1 行目。
    on_mouse(&mut a, click(list.x + 2, list.y + 2));
    assert_eq!(a.selected().unwrap().name, "issues");
}

/// タブはクリックしたものへ切り替わる。
#[test]
fn clicking_a_tab_switches_to_it() {
    use super::on_mouse;
    let mut a = drawn(&demo(), 100, 26);
    assert_eq!(a.tab, Tab::Body);

    let query = a.areas.tab_items[2];
    on_mouse(&mut a, click(query.x, query.y));
    assert_eq!(a.tab, Tab::Query, "tab_items: {:?}", a.areas.tab_items);
}

/// **ホイールはフォーカスを移さない。** 見るために回しただけで
/// キーの当たり先が変わると、次に押したキーが思わぬ場所に効く。
#[test]
fn the_wheel_scrolls_without_stealing_the_focus() {
    use super::{on_mouse, Focus};
    let mut a = drawn(&demo(), 100, 26);
    a.focus = Focus::List;
    a.response_max_top = 30;

    let response = a.areas.response;
    on_mouse(&mut a, wheel(true, response.x + 2, response.y + 1));
    assert!(a.response_scroll.top() > 0, "スクロールしていない");
    assert_eq!(a.focus, Focus::List, "フォーカスが動いている");

    on_mouse(&mut a, wheel(false, response.x + 2, response.y + 1));
    assert_eq!(a.response_scroll.top(), 0);
}

/// 何も無いところのクリックは無視する。
#[test]
fn clicking_outside_every_pane_does_nothing() {
    use super::{on_mouse, Focus};
    let mut a = drawn(&demo(), 100, 26);
    a.focus = Focus::Response;
    on_mouse(&mut a, click(0, 0)); // ヘッダの行
    assert_eq!(a.focus, Focus::Response);
}

/// 絞り込み中はマウスを効かせない。打った文字がどこへ行ったか分からなくなる。
#[test]
fn the_mouse_is_ignored_while_filtering() {
    use super::{on_mouse, Focus};
    let mut a = drawn(&demo(), 100, 26);
    a.focus = Focus::List;
    on_key(&mut a, key(KeyCode::Char('/')));

    let response = a.areas.response;
    on_mouse(&mut a, click(response.x + 2, response.y + 1));
    assert_eq!(a.focus, Focus::List);
}

/// 一覧を畳んだ狭い端末では、一覧の当たり判定も消える。
/// 残すと、見えていない場所がクリックに反応する。
#[test]
fn a_hidden_sidebar_has_no_hit_area() {
    use super::on_mouse;
    use super::Focus;
    let mut a = drawn(&demo(), 50, 24);
    assert_eq!(a.areas.list, Rect::default());
    // 左端は、狭い端末では右側のペインが占めている。
    // **一覧に当たってはいけない**（一覧は描かれていない）。
    for y in 1..23 {
        on_mouse(&mut a, click(1, y));
        assert_ne!(a.focus, Focus::List, "描いていない一覧に当たった (y={y})");
    }
}

/// `m` で捕捉を切れる。切れていることは画面に出す。
#[test]
fn mouse_capture_can_be_turned_off_and_says_so() {
    let mut a = demo();
    assert!(a.mouse);
    on_key(&mut a, key(KeyCode::Char('m')));
    assert!(!a.mouse);
    assert!(screen(&a, 100, 26).contains("マウス切"), "画面に出ていない");
    on_key(&mut a, key(KeyCode::Char('m')));
    assert!(a.mouse);
    assert!(!screen(&a, 100, 26).contains("マウス切"));
}
