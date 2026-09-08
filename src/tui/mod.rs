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

pub use model::{App, Entry, Mode, Pane, Tab};

/// レスポンス欄に載せる本文の行数。全文はダンプで読む。
const BODY_LINES: usize = 200;

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
    let entries = load_entries()?;
    let mut app = App::new(entries, workspace::current().label(), env.clone());

    let mut terminal = enter()?;
    let result = event_loop(&mut terminal, &mut app, env.as_deref()).await;
    leave(&mut terminal)?;
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

fn enter() -> Result<Term> {
    enable_raw_mode().context("端末を raw モードにできません")?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen)?;
    Terminal::new(CrosstermBackend::new(out)).context("端末を初期化できません")
}

/// **必ず戻す。** 戻し損ねると、抜けたあとのシェルがエコーなしのまま残る。
fn leave(terminal: &mut Term) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
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
                app.pane = send(&name, env).await;
            }
            Action::Edit(name) => {
                // エディタは端末を占有する。**必ず画面を明け渡してから起動する。**
                // 明け渡さずに起動すると、vim が alternate screen の上に描いて
                // 何も見えないまま入力だけが通る。
                leave(terminal)?;
                let outcome = crate::run::edit_saved(&name);
                *terminal = enter()?;
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

    // Ctrl-C はどの状態からでも抜ける。
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.quit = true;
        return Action::Quit;
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => {
            app.quit = true;
            Action::Quit
        }
        KeyCode::Char('j') | KeyCode::Down => {
            app.move_down();
            Action::None
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.move_up();
            Action::None
        }
        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
            app.tab = app.tab.next();
            Action::None
        }
        KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
            app.tab = app.tab.prev();
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
        _ => Action::None,
    }
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
    match crate::run::send_saved(name, &common).await {
        Err(e) => {
            let mut msg = format!("{e}");
            for cause in e.chain().skip(1) {
                msg.push_str(&format!("\n  原因: {cause}"));
            }
            Pane::Failed(msg)
        }
        Ok(done) => fold(done),
    }
}

/// 送信結果を画面に載る形に畳む。**ここが唯一の変換点**なので、
/// マスクが効いているかはこの関数だけを見れば確かめられる。
pub fn fold(done: crate::run::Performed) -> Pane {
    // **表示するものは必ずマスクを通す。** 生の `response` は
    // capture のために残っているだけで、画面には出さない。
    let res = dump::redact_response(&done.response, &done.redactor);
    let shape = match &res.body {
        dump::BodyRecord::Json { value } => Some(shape::of(value).render()),
        _ => None,
    };
    let body = res
        .body
        .as_text()
        .map(|t| t.lines().take(BODY_LINES).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default();
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
        notes: done.notes,
    }
}
