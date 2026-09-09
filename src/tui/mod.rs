//! `ailo tui` — 保存済みリクエストを人が読んで、選んで、送る画面。
//!
//! エージェントは CLI を使う。TUI は**人が副の利用者として**、保存済みの定義を
//! 見渡し、その場で叩いて結果を確かめるためのもの。定義そのものの編集は
//! `ailo new` と同じ `$EDITOR` に投げる（画面の中に編集器を作らない）。

mod model;
pub(crate) mod view;

#[cfg(test)]
mod tests;

use std::io;
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
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

pub use model::{App, Entry, Focus, Mode, Pane, Scroll, Tab};

/// 押されたキーに対して何をするか。**端末を触らずに決める**ので、テストできる。
#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    Quit,
    Send,
    Edit(String),
}

pub async fn run(env: Option<String>) -> Result<Outcome> {
    let cfg = Config::load()?;
    let env = cfg.resolve_env(env.as_deref());
    require_terminal()?;
    install_panic_restore();
    let entries = load_entries()?;
    let mut app = App::new(entries, workspace::current().label(), env.clone());

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
    let result = event_loop(&mut screen.terminal, &mut app, env.as_deref()).await;
    // `screen` の Drop がここで端末を戻す。エラーで抜けても同じ。
    drop(screen);
    result?;
    Ok(Outcome { code: 0 })
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
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
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
        execute!(out, EnterAlternateScreen).context("画面を切り替えられません")?;
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
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
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

async fn event_loop(terminal: &mut Term, app: &mut App, env: Option<&str>) -> Result<()> {
    loop {
        terminal.draw(|f| view::draw(f, app))?;

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        // **押した瞬間だけを見る。** Windows は離したときにも同じキーを送るので、
        // これを見ないと 1 回の入力が 2 回効く。
        if key.kind != KeyEventKind::Press {
            continue;
        }

        match on_key(app, key) {
            Action::None => {}
            Action::Quit => return Ok(()),
            Action::Send => {
                let Some(name) = app.selected().map(|e| e.name.clone()) else {
                    continue;
                };
                app.pane = Pane::Sending;
                terminal.draw(|f| view::draw(f, app))?;
                app.pane = send_watching_for_cancel(&name, env).await?;
            }
            Action::Edit(name) => {
                // エディタは端末を占有する。**必ず画面を明け渡してから起動する。**
                // 明け渡さずに起動すると、vim が alternate screen の上に描いて
                // 何も見えないまま入力だけが通る。
                let outcome = with_terminal_released(terminal, || crate::run::edit_saved(&name))?;
                terminal.clear()?;
                match outcome {
                    Ok(()) => {
                        app.reload(load_entries()?);
                        app.pane = Pane::Idle;
                    }
                    Err(e) => app.pane = Pane::Failed(format!("{e}")),
                }
            }
        }
        if app.quit {
            return Ok(());
        }
    }
}

/// キー 1 つに対する判断。端末にも通信にも触らない。
pub fn on_key(app: &mut App, key: KeyEvent) -> Action {
    // **Ctrl-C はどの状態からでも抜ける。** 絞り込みの分岐より後ろに置いていたときは、
    // 絞り込み中の Ctrl-C が文字 `c` として検索語に入り、抜ける手段が無くなっていた。
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.quit = true;
        return Action::Quit;
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
        KeyCode::Enter => Action::Send,
        KeyCode::Char('e') => match app.selected() {
            Some(e) => Action::Edit(e.name.clone()),
            None => Action::None,
        },
        _ => on_focused_key(app, key),
    }
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

/// 端末を明け渡してから `f` を動かし、戻ってきたら握り直す。
///
/// **明け渡さずにエディタを起動すると、vim が alternate screen の上に描いて
/// 何も見えないまま入力だけが通る。** 握り直しに失敗したときは、そのまま
/// 上へ返す（画面を持たないまま描き続けるより、落ちるほうがよい）。
fn with_terminal_released<T>(terminal: &mut Term, f: impl FnOnce() -> T) -> Result<T> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    let out = f();

    enable_raw_mode().context("端末を raw モードに戻せません")?;
    execute!(terminal.backend_mut(), EnterAlternateScreen).context("画面を戻せません")?;
    terminal.hide_cursor()?;
    Ok(out)
}

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
async fn send_watching_for_cancel(name: &str, env: Option<&str>) -> Result<Pane> {
    let sending = send(name, env);
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
async fn send(name: &str, env: Option<&str>) -> Pane {
    let common = CommonArgs {
        env: env.map(str::to_string),
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
