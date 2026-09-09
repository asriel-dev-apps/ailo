//! `ailo tui` — 保存済みリクエストを人が読んで、選んで、送る画面。
//!
//! エージェントは CLI を使う。TUI は**人が副の利用者として**、保存済みの定義を
//! 見渡し、その場で叩いて結果を確かめるためのもの。定義そのものの編集は
//! `ailo new` と同じ `$EDITOR` に投げる（画面の中に編集器を作らない）。

mod editor;
mod model;
pub(crate) mod view;

#[cfg(test)]
mod tests;

use std::io;
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::{CrosstermBackend, Terminal};

use crate::cli::CommonArgs;
use crate::config::{Config, Requests};
use crate::dump;
use crate::run::Outcome;
use crate::shape;
use crate::workspace;
use editor::{Editing, Target};

pub use model::{App, Areas, Entry, Focus, Mode, Overlay, Pane, Scroll, Tab, VarRow};

/// 押されたキーに対して何をするか。**端末を触らずに決める**ので、テストできる。
#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    /// 編集した定義を保存する。
    Save,
    Quit,
    Send,
    /// workspace を切り替える。**同じプロセスでは切り替えられない**ので、
    /// 自分自身を `-w <名前>` で起動し直す（下の `switch_workspace` を見よ）。
    SwitchWorkspace(String),
}

pub async fn run(env: Option<String>) -> Result<Outcome> {
    let cfg = Config::load()?;
    let env = cfg.resolve_env(env.as_deref());
    require_terminal()?;
    install_panic_restore();
    let entries = load_entries()?;
    let mut app = App::new(entries, workspace::current().label(), env.clone());
    app.known_vars = known_var_names(env.as_deref());

    // **未知の環境は起動時に言う。** 送るまで黙っていると、ヘッダに
    // `env: typo` と平然と出たまま、変数が 1 つも解決しない画面を見ることになる。
    if let (Some(name), false) = (env.as_deref(), cfg.environments().is_empty()) {
        if !cfg.knows_env(name) {
            app.pane = Pane::Failed(format!(
                "環境 `{name}` は設定にありません。あるのは: {}",
                cfg.environments().join(", ")
            ));
        }
    }

    let mut screen = Screen::enter()?;
    let result = event_loop(&mut screen.terminal, &mut app).await;
    // `screen` の Drop がここで端末を戻す。エラーで抜けても同じ。
    drop(screen);

    match result? {
        Exit::Quit => Ok(Outcome { code: 0 }),
        Exit::Switch(name) => switch_workspace(&name),
    }
}

/// workspace を変えて起動し直す。
///
/// **同じプロセスでは切り替えられない。** 置き場所は起動時に 1 度だけ決める作りで
/// (`workspace::init` の `OnceLock`)、途中で変えると読んだ場所と書いた場所が
/// 食い違う。その不変条件を崩すより、自分自身を `-w <名前>` で起動し直すほうが安い。
///
/// **端末は既に戻してから呼ぶこと。** `exec` は戻ってこないので、後始末の機会が無い。
fn switch_workspace(name: &str) -> Result<Outcome> {
    use std::os::unix::process::CommandExt;

    let exe = std::env::current_exe().context("自分自身の場所が分かりません")?;
    let mut cmd = std::process::Command::new(exe);
    // 既定は名前を持たないので、`-w` を付けずに起動する。
    if name != crate::workspace::Workspace::Default.label() {
        cmd.arg("-w").arg(name);
    }
    cmd.arg("tui");
    // `AILO_WORKSPACE` が残っていると `-w` の無い既定側で効いてしまう。
    cmd.env_remove(crate::workspace::ENV_VAR);

    // 戻ってきたということは起動できなかったということ。
    let err = cmd.exec();
    Err(anyhow::Error::new(err).context("起動し直せませんでした"))
}

fn load_entries() -> Result<Vec<Entry>> {
    Ok(entries_of(&Requests::load()?))
}

/// 一覧の中身は `Requests` そのもの。**別の読み口を作らない。**
///
/// `Requests::load()` は workspace ごとに置き場所が分かれており、その分離は
/// `tests/workspace_cli.rs` で確かめてある。TUI が独自にファイルを読みに行くと、
/// その分離をもう一度書くことになり、片方だけ破れる。
pub fn entries_of(reqs: &Requests) -> Vec<Entry> {
    reqs.requests
        .iter()
        .map(|(name, req)| Entry {
            name: name.clone(),
            req: req.clone(),
        })
        .collect()
}

type Term = Terminal<CrosstermBackend<io::Stdout>>;

/// 端末でないところで開かない。
///
/// **主利用者はエージェントで、エージェントには端末が無い。** 気づかず起動すると、
/// 環境によっては入力待ちのまま止まる。止まるのは失敗より遥かにたちが悪いので、
/// 先に落として**次にやること**を書く。
fn require_terminal() -> Result<()> {
    use std::io::IsTerminal;
    if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
        return Ok(());
    }
    anyhow::bail!(
        "`ailo tui` は端末でだけ使えます。一覧は `ailo ls`、実行は `ailo run <名前>` を使ってください"
    )
}

/// パニックしても端末を戻す。
///
/// **戻さないと、抜けたあとのシェルがエコーなしの raw モードで残る。**
/// 利用者から見ると「ターミナルが壊れた」で、`reset` を知らないと直せない。
/// パニックの本文は素の端末に出したいので、戻してから既定のハンドラへ渡す。
fn install_panic_restore() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
        previous(info);
    }));
}

/// 端末を握っている間だけ生きる。**`Drop` で必ず戻す。**
///
/// 手で `leave()` を呼ぶ形にしていたときは、`enable_raw_mode()` が成功したあとに
/// `EnterAlternateScreen` や `Terminal::new` が失敗すると raw モードのまま抜けていた。
/// 早期 return が 1 本増えるたびに同じ穴が開くので、戻す責任を型に持たせる。
struct Screen {
    terminal: Term,
}

impl Screen {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("端末を raw モードにできません")?;
        // ここから先で失敗しても raw モードを戻す。
        let guard = RawGuard;
        let mut out = io::stdout();
        execute!(out, EnterAlternateScreen, EnableMouseCapture)
            .context("画面を切り替えられません")?;
        let terminal = Terminal::new(CrosstermBackend::new(out)).context("端末を初期化できません");
        match terminal {
            Ok(terminal) => {
                std::mem::forget(guard);
                Ok(Self { terminal })
            }
            Err(e) => {
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                Err(e)
            }
        }
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        // **マウス捕捉も必ず解く。** 解かないと、抜けたあとの端末で
        // 選択もスクロールもできないまま残る。
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = self.terminal.show_cursor();
    }
}

/// `enable_raw_mode()` だけが成功した状態を戻すための番人。
struct RawGuard;

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

/// イベントループの抜け方。
pub enum Exit {
    Quit,
    /// workspace を変えて起動し直す。
    Switch(String),
}

async fn event_loop(terminal: &mut Term, app: &mut App) -> Result<Exit> {
    loop {
        terminal.draw(|f| view::draw(f, app))?;

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let action = match event::read()? {
            Event::Key(key) => {
                // **押した瞬間だけを見る。** Windows は離したときにも同じキーを
                // 送るので、これを見ないと 1 回の入力が 2 回効く。
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let was_capturing = app.mouse;
                let action = on_key(app, key);
                if app.mouse != was_capturing {
                    set_mouse_capture(terminal, app.mouse)?;
                }
                action
            }
            Event::Mouse(m) => on_mouse(app, m),
            _ => continue,
        };

        match action {
            Action::None => {}
            Action::Quit => return Ok(Exit::Quit),
            Action::Send => {
                let Some(name) = app.selected().map(|e| e.name.clone()) else {
                    continue;
                };
                app.pane = Pane::Sending;
                terminal.draw(|f| view::draw(f, app))?;
                app.pane = send_watching_for_cancel(&name, app.env.as_deref(), app.dump).await?;
                // capture で変数が増えることがある。取り直さないと、
                // いま取ったばかりの値が未解決の色で出る。
                app.known_vars = known_var_names(app.env.as_deref());
            }
            Action::Save => save_editing(app)?,
            Action::SwitchWorkspace(name) => return Ok(Exit::Switch(name)),
        }
        if app.quit {
            return Ok(Exit::Quit);
        }
    }
}

/// キー 1 つに対する判断。端末にも通信にも触らない。
pub fn on_key(app: &mut App, key: KeyEvent) -> Action {
    // **Ctrl-C はどの状態からでも抜ける。** 絞り込みの分岐より後ろに置いていたときは、
    // 絞り込み中の Ctrl-C が文字 `c` として検索語に入り、抜ける手段が無くなっていた。
    //
    // 編集中だけは例外。書きかけを Ctrl-C で消し飛ばすのは、どの editor でもしない。
    // 編集中は `Esc` で破棄、`Ctrl-S` で保存。
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && key.code == KeyCode::Char('c')
        && app.editing.is_none()
    {
        app.quit = true;
        return Action::Quit;
    }

    if app.editing.is_some() {
        return on_editor_key(app, key);
    }

    if app.overlay.is_some() {
        return on_overlay_key(app, key);
    }

    if app.mode == Mode::Filter {
        match key.code {
            KeyCode::Esc => {
                app.clear_filter();
                app.mode = Mode::Normal;
            }
            KeyCode::Enter => app.mode = Mode::Normal,
            KeyCode::Backspace => app.pop_filter(),
            KeyCode::Char(c) => app.push_filter(c),
            _ => {}
        }
        return Action::None;
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            app.quit = true;
            Action::Quit
        }
        // **`Tab` はペインを移る。** タブの切り替えではない。
        // ペインが増えた以上、一番押されるキーは「どこに当てるか」に要る。
        // タブは、タブのペインに当たっているときの左右で切り替える。
        KeyCode::Tab => {
            app.focus = app.focus.next();
            Action::None
        }
        KeyCode::BackTab => {
            app.focus = app.focus.prev();
            Action::None
        }
        KeyCode::Char('/') => {
            app.mode = Mode::Filter;
            Action::None
        }
        // マウス捕捉の入り切り。切ると端末のテキスト選択が戻る。
        KeyCode::Char('m') => {
            app.mouse = !app.mouse;
            Action::None
        }
        // 一覧を作るのに設定を読む。読めなければ開かず、理由をレスポンス欄に出す。
        KeyCode::Char('w') => {
            match workspace_names() {
                Ok(items) => app.open_workspace_picker(items),
                Err(e) => app.pane = Pane::Failed(format!("{e}")),
            }
            Action::None
        }
        KeyCode::Char('E') => {
            match crate::run::environment_names() {
                Ok(items) => app.open_env_picker(items),
                Err(e) => app.pane = Pane::Failed(format!("{e}")),
            }
            Action::None
        }
        KeyCode::Char('v') => {
            match var_rows(app.env.as_deref()) {
                Ok(rows) => app.open_vars(rows),
                Err(e) => app.pane = Pane::Failed(format!("{e}")),
            }
            Action::None
        }
        KeyCode::Char('d') => {
            app.dump = !app.dump;
            Action::None
        }
        // **フォーカス中のペインを編集する。** どこを編集するかは、
        // いま当たっている場所で決まる（ユーザー要望 2026-09-09）。
        KeyCode::Char('e') => {
            open_editor_for_focus(app);
            Action::None
        }
        // 定義まるごとを TOML で編集する。capture や form など、
        // ペインに出ていないものを触るときに要る。
        KeyCode::Char('T') => {
            open_editor(app, Target::Whole);
            Action::None
        }
        KeyCode::Enter => Action::Send,
        _ => on_focused_key(app, key),
    }
}

/// マウスの捕捉を切り替える。
///
/// 捕捉している間は端末側のテキスト選択ができない。**出力を貼りたいだけの人が
/// 詰まないよう、いつでも切れるようにする。**
fn set_mouse_capture(terminal: &mut Term, on: bool) -> Result<()> {
    if on {
        execute!(terminal.backend_mut(), EnableMouseCapture)?;
    } else {
        execute!(terminal.backend_mut(), DisableMouseCapture)?;
    }
    Ok(())
}

/// マウス 1 つに対する判断。端末にも通信にも触らない。
pub fn on_mouse(app: &mut App, m: MouseEvent) -> Action {
    // 絞り込み中はキーボードに専念させる。ここでフォーカスが飛ぶと、
    // 打った文字がどこへ行ったか分からなくなる。
    if app.mode == Mode::Filter {
        return Action::None;
    }
    let (x, y) = (m.column, m.row);
    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(focus) = app.areas.hit(x, y) else {
                return Action::None;
            };
            app.focus = focus;
            match focus {
                Focus::List => {
                    if let Some(row) = app.areas.list_row_at(y) {
                        app.select_visible(row);
                    }
                }
                Focus::Tabs => {
                    if let Some(i) = app.areas.tab_at(x, y) {
                        app.set_tab(Tab::ALL[i]);
                    }
                }
                _ => {}
            }
        }
        // **ホイールはカーソルの下のペインを動かす。** フォーカスを移さないのは、
        // 「見るために回しただけ」でキーの当たり先が変わると事故になるため。
        MouseEventKind::ScrollDown => match app.areas.hit(x, y) {
            Some(Focus::Definition) => {
                let max = app.definition_max_top;
                app.definition_scroll.down(WHEEL, max);
            }
            Some(Focus::Response) => {
                let max = app.response_max_top;
                app.response_scroll.down(WHEEL, max);
            }
            Some(Focus::List) => app.move_down(),
            _ => {}
        },
        MouseEventKind::ScrollUp => match app.areas.hit(x, y) {
            Some(Focus::Definition) => app.definition_scroll.up(WHEEL),
            Some(Focus::Response) => app.response_scroll.up(WHEEL),
            Some(Focus::List) => app.move_up(),
            _ => {}
        },
        _ => {}
    }
    Action::None
}

/// ホイール 1 刻みで動く行数。
const WHEEL: u16 = 3;

/// 編集中の内容を保存する。**落ちても編集器は閉じない。**
///
/// 閉じてしまうと、直せば通る内容を書いた人が書いたものごと失う。
/// 理由を編集器の中に出して、直せるようにする。
fn save_editing(app: &mut App) -> Result<()> {
    let Some(editing) = app.editing.as_mut() else {
        return Ok(());
    };
    let outcome = editing
        .apply()
        .and_then(|req| crate::run::save_edited(&editing.name, &editing.opened_from, req));

    match outcome {
        Ok(()) => {
            app.editing = None;
            // 書き換わったので読み直す。読み直さないと、画面が古い定義のまま。
            app.reload(load_entries()?);
            app.pane = Pane::Idle;
        }
        Err(e) => {
            let mut msg = format!("{e}");
            for cause in e.chain().skip(1) {
                msg.push_str(&format!("\n  原因: {cause}"));
            }
            editing.error = Some(msg);
        }
    }
    Ok(())
}

/// 選べる workspace の名前。既定は表示名で出す。
fn workspace_names() -> Result<Vec<String>> {
    let base = crate::paths::config_base()?;
    Ok(workspace::list(&base)
        .into_iter()
        .map(|w| w.label().to_string())
        .collect())
}

/// いまの環境で値の付く変数の名前。**値は持ち出さない。**
///
/// 読めなければ空で返す。ここで落として画面が開かないほうが困る
/// (色が付かないだけで、送るのに支障は無い)。
fn known_var_names(env: Option<&str>) -> Vec<String> {
    crate::run::variables(env)
        .map(|rows| rows.into_iter().map(|(d, _)| d.name).collect())
        .unwrap_or_default()
}

/// 変数一覧の行。**値は `run::variables` の時点で伏せてある。**
fn var_rows(env: Option<&str>) -> Result<Vec<VarRow>> {
    Ok(crate::run::variables(env)?
        .into_iter()
        .map(|(d, expires)| VarRow {
            name: d.name,
            shown: d.shown,
            source: d.from.to_string(),
            expires: expires.unwrap_or_default(),
            secret: d.secret,
        })
        .collect())
}

/// フォーカスから、編集する対象を決める。
fn open_editor_for_focus(app: &mut App) {
    let target = match app.focus {
        Focus::Endpoint => Target::Endpoint,
        Focus::Definition | Focus::Tabs => match app.tab {
            // capture は「名前 = 式」で item 記法ではない。行単位で編集させると
            // 保存の形が別物になるので、まるごと TOML に回す。
            Tab::Capture => Target::Whole,
            tab => Target::Items(tab),
        },
        // 一覧とレスポンスに当たっているときは、まるごと開く。
        // 「どこを編集するか」が決まらないので、決め打ちで一部を開かない。
        Focus::List | Focus::Response => Target::Whole,
    };
    open_editor(app, target);
}

fn open_editor(app: &mut App, target: Target) {
    let Some(entry) = app.selected() else {
        return;
    };
    // 本文を持つリクエストで Body を編集するなら、item ではなく本文を開く。
    // 両方を 1 つの画面に混ぜると、保存の形が決まらない。
    let target = match (target, entry.req.raw.is_some()) {
        (Target::Items(Tab::Body), true) => Target::Raw,
        (t, _) => t,
    };
    app.editing = Some(Editing::open(&entry.name, &entry.req, target));
}

/// 編集中のキー。**ここで拾わないものは全部 `tui-textarea` に渡す。**
fn on_editor_key(app: &mut App, key: KeyEvent) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // 検索中は編集器の中の小さなモード。**外側の `Esc`（編集の破棄）より先に拾う。**
    if app.editing.as_ref().is_some_and(|e| e.search.is_some()) {
        let Some(editing) = app.editing.as_mut() else {
            return Action::None;
        };
        match key.code {
            KeyCode::Esc => {
                editing.search = None;
                editing.apply_search(true);
            }
            KeyCode::Enter => editing.apply_search(true),
            KeyCode::Backspace => {
                if let Some(s) = editing.search.as_mut() {
                    s.pop();
                }
                editing.apply_search(true);
            }
            KeyCode::Char(c) => {
                if let Some(s) = editing.search.as_mut() {
                    s.push(c);
                }
                editing.apply_search(true);
            }
            _ => {}
        }
        return Action::None;
    }

    match key.code {
        KeyCode::Esc => {
            app.editing = None;
            return Action::None;
        }
        KeyCode::Char('s') if ctrl => return Action::Save,
        KeyCode::Char('f') if ctrl => {
            if let Some(editing) = app.editing.as_mut() {
                editing.search = Some(String::new());
            }
            return Action::None;
        }
        // 1 行しか受け付けない対象では改行を入れさせない。
        // 入れさせると、保存時に黙って連結されて意図と違う値になる。
        KeyCode::Enter => {
            let single = app.editing.as_ref().is_some_and(|e| e.target.single_line());
            if single {
                return Action::Save;
            }
        }
        _ => {}
    }
    if let Some(editing) = app.editing.as_mut() {
        editing.error = None;
        editing.area.input(key);
    }
    Action::None
}

/// かぶせて出ているものへのキー。**`Esc` はどれでも閉じる。**
fn on_overlay_key(app: &mut App, key: KeyEvent) -> Action {
    let Some(overlay) = app.overlay.as_mut() else {
        return Action::None;
    };
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.overlay = None;
        }
        KeyCode::Char('j') | KeyCode::Down => match overlay {
            Overlay::Vars { rows, scroll } => {
                let max = rows.len().saturating_sub(1) as u16;
                scroll.down(1, max);
            }
            other => other.move_cursor(true),
        },
        KeyCode::Char('k') | KeyCode::Up => match overlay {
            Overlay::Vars { scroll, .. } => scroll.up(1),
            other => other.move_cursor(false),
        },
        KeyCode::Enter => {
            let chosen = overlay.chosen().map(str::to_string);
            let kind = app.overlay.take();
            match (kind, chosen) {
                (Some(Overlay::Workspace { .. }), Some(name)) => {
                    return Action::SwitchWorkspace(name);
                }
                (Some(Overlay::Env { .. }), Some(name)) => {
                    app.set_env(name);
                    // 環境が変われば解決できる変数も変わる。取り直さないと
                    // 色分けが前の環境のまま残る。
                    app.known_vars = known_var_names(app.env.as_deref());
                }
                _ => {}
            }
        }
        _ => {}
    }
    Action::None
}

/// フォーカス中のペインに配るキー。
///
/// **同じ `j` でも、一覧では「次のリクエスト」、レスポンスでは「1 行下へ」。**
/// ペインごとに意味が変わるので、ここで振り分ける。
fn on_focused_key(app: &mut App, key: KeyEvent) -> Action {
    let down = matches!(key.code, KeyCode::Char('j') | KeyCode::Down);
    let up = matches!(key.code, KeyCode::Char('k') | KeyCode::Up);
    let right = matches!(key.code, KeyCode::Char('l') | KeyCode::Right);
    let left = matches!(key.code, KeyCode::Char('h') | KeyCode::Left);

    match app.focus {
        Focus::List => {
            if down {
                app.move_down();
            } else if up {
                app.move_up();
            }
        }
        // タブに当たっているときは左右で切り替える。上下は当たり先を移す
        // (タブは 1 行しか無いので、上下にスクロールするものが無い)。
        Focus::Tabs => {
            if right {
                let next = app.tab.next();
                app.set_tab(next);
            } else if left {
                let prev = app.tab.prev();
                app.set_tab(prev);
            } else if down {
                app.focus = Focus::Definition;
            } else if up {
                app.focus = Focus::Endpoint;
            }
        }
        Focus::Definition => scroll_keys(&mut app.definition_scroll, key, app.definition_max_top),
        Focus::Response => scroll_keys(&mut app.response_scroll, key, app.response_max_top),
        // エンドポイントは 1〜2 行。スクロールするものが無いので、上下で隣のペインへ。
        Focus::Endpoint => {
            if down {
                app.focus = Focus::Tabs;
            } else if up {
                app.focus = Focus::List;
            }
        }
    }
    Action::None
}

/// スクロールできるペインの共通のキー。
fn scroll_keys(scroll: &mut Scroll, key: KeyEvent, max_top: u16) {
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => scroll.down(1, max_top),
        KeyCode::Char('k') | KeyCode::Up => scroll.up(1),
        KeyCode::PageDown | KeyCode::Char('f') => scroll.down(PAGE, max_top),
        KeyCode::PageUp | KeyCode::Char('b') => scroll.up(PAGE),
        KeyCode::Home | KeyCode::Char('g') => scroll.to_start(),
        KeyCode::End | KeyCode::Char('G') => scroll.to_end(max_top),
        _ => {}
    }
}

/// 1 ページ分の行数。端末の高さは描くまで分からないので、控えめな固定値にする。
const PAGE: u16 = 10;

/// 送りながら、中断のキーだけを拾い続ける。
///
/// **`await` の間もイベントを読む。** 読まないと、タイムアウト（既定 30 秒）まで
/// 画面が固まる。raw モードでは Ctrl-C が SIGINT にならないので、固まっている間は
/// 端末を叩いても何も起きない。**止められない画面は、失敗するより悪い。**
///
/// 中断は送信の future を捨てることで行う。**「送っていない」ことは保証できない。**
/// TCP に出したあとに捨てても、相手ではもう実行されているかもしれない。
/// 文言でそう言う。
///
/// `biased;` を付けて送信のほうを先に見る。既定のランダム順だと、応答を受け取って
/// ダンプも capture も書き終えた結果を捨てて「中断しました」と出すことがある
/// （応答受信後の処理に await が無いので、完了＝全部書き終えている）。
async fn send_watching_for_cancel(name: &str, env: Option<&str>, dump: bool) -> Result<Pane> {
    let sending = send(name, env, dump);
    tokio::pin!(sending);
    loop {
        tokio::select! {
            biased;
            pane = &mut sending => return Ok(pane),
            _ = tokio::time::sleep(Duration::from_millis(80)) => {
                if wants_cancel()? {
                    return Ok(Pane::Failed(
                        "送信を打ち切りました（サーバには届いている可能性があります）".into(),
                    ));
                }
            }
        }
    }
}

/// 溜まっているキーのうち、中断を意味するものがあるか。
///
/// **溜まっている分は全部読み切る。** 途中で抜けると、送信中に押したキーが
/// 送信後の操作として遅れて効く。
fn wants_cancel() -> Result<bool> {
    let mut cancel = false;
    while event::poll(Duration::from_millis(0))? {
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let ctrl_c =
                key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c');
            if ctrl_c || key.code == KeyCode::Esc {
                cancel = true;
            }
        }
    }
    Ok(cancel)
}

/// 送って、画面に載る形に畳む。
///
/// **CLI と同じ `run::send_saved` を通す。** ここで独自に組み立てると、
/// マスク・ダンプ・capture のどれかが TUI 経由でだけ効かなくなる。
async fn send(name: &str, env: Option<&str>, dump: bool) -> Pane {
    let common = CommonArgs {
        env: env.map(str::to_string),
        no_dump: !dump,
        ..CommonArgs::for_tui()
    };
    let mut notes = Vec::new();
    match crate::run::send_saved(name, &common, &mut notes).await {
        Err(e) => {
            let mut msg = format!("{e}");
            for cause in e.chain().skip(1) {
                msg.push_str(&format!("\n  原因: {cause}"));
            }
            // **送信前に出ていた警告も一緒に見せる。** 失敗したときこそ、
            // 「未知の環境を指していた」「URL に秘匿値を展開した」が要る。
            for note in notes {
                msg.push_str(&format!("\n{note}"));
            }
            Pane::Failed(msg)
        }
        Ok(done) => fold(done, notes),
    }
}

/// 送信結果を画面に載る形に畳む。**ここが唯一の変換点**なので、
/// マスクが効いているかはこの関数だけを見れば確かめられる。
pub fn fold(done: crate::run::Performed, notes: Vec<String>) -> Pane {
    // **表示するものは必ずマスクを通す。** 生の `response` は
    // capture のために残っているだけで、画面には出さない。
    let res = dump::redact_response(&done.response, &done.redactor);
    let shape = match &res.body {
        dump::BodyRecord::Json { value } => Some(shape::of(value).render()),
        _ => None,
    };
    // **全文を持つ。** 画面はスクロールできるので、ここで切る理由が無い。
    let body = res.body.as_text().unwrap_or_default();
    Pane::Done {
        status: res.status,
        status_text: res.status_text.clone(),
        ms: res.ms,
        bytes: res.bytes,
        content_type: res
            .headers
            .get("content-type")
            .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
            .unwrap_or_else(|| "-".into()),
        shape,
        body,
        dump: done.dump_path.as_deref().map(crate::paths::tildify),
        notes,
    }
}
