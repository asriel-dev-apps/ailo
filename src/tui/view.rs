//! 画面の組み立て。状態を受け取って ratatui の Frame に描くだけ。
//!
//! 配置は Postman と同じ形（ユーザー指示 2026-09-08）。左に保存済みリクエスト、
//! 右は上から順にエンドポイント・タブ・レスポンス。

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::model::{
    display_url, split_vars, tab_count, tab_lines, App, Focus, Mode, Overlay, Pane, Piece, Tab,
};

/// フォーカス中のペインの枠。
///
/// **どこにキーが当たっているかを、常に 1 目で分かるようにする。**
/// 当たり先で `j` の意味が変わるので、分からないまま押すと意図しない場所が動く。
fn framed(title: &str, focused: bool) -> Block<'_> {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_string());
    if focused {
        block.border_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        block
    }
}

/// `{{名前}}` を色分けした Span 列にする。
///
/// **解決できない変数を別色にする。** 送ってから「変数が無い」と言われるより、
/// 書いている時点で分かるほうが早い。Postman が未解決を赤で出すのと同じ。
fn with_vars(text: &str, known: &[String]) -> Vec<Span<'static>> {
    split_vars(text, known)
        .into_iter()
        .map(|p| match p {
            Piece::Text(t) => Span::raw(t),
            Piece::Var { name, resolved } => Span::styled(
                format!("{{{{{name}}}}}"),
                if resolved {
                    Style::default().fg(Color::Magenta)
                } else {
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                },
            ),
        })
        .collect()
}

/// 中身の行数と見えている高さから、これ以上下げられない位置を出す。
fn max_top(lines: usize, height: u16) -> u16 {
    // 枠の上下 2 行は中身に使えない。
    let visible = height.saturating_sub(2);
    (lines as u16).saturating_sub(visible)
}

/// 左ペインの幅。これより狭い端末では一覧を畳む。
const SIDEBAR: u16 = 26;
/// 一覧を畳む閾値。
const NARROW: u16 = 60;

pub fn draw(f: &mut Frame, app: &mut App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(f.area());

    f.render_widget(header(app), root[0]);
    f.render_widget(footer(app, f.area().width), root[2]);

    // 狭い端末では一覧を出さない。潰れた 2 ペインより、片方が読めるほうがよい。
    if f.area().width < NARROW {
        // 一覧は出していないので、当たり判定も消す。残すと見えない場所が
        // クリックに反応する。
        app.areas.list = Rect::default();
        detail(f, app, root[1]);
        return;
    }

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(SIDEBAR), Constraint::Min(20)])
        .split(root[1]);

    app.areas.list = cols[0];
    sidebar(f, app, cols[0]);
    detail(f, app, cols[1]);
    overlay(f, app);
}

/// 中央にかぶせる矩形。画面の縦横の割合で決める。
fn centred(area: Rect, pct_w: u16, pct_h: u16) -> Rect {
    let w = (area.width * pct_w / 100).clamp(20, area.width);
    let h = (area.height * pct_h / 100).clamp(3, area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn overlay(f: &mut Frame, app: &mut App) {
    // 編集器が一番手前。開いている間はほかのかぶせものを出さない。
    if app.editing.is_some() {
        editor(f, app);
        return;
    }
    let Some(overlay) = app.overlay.as_mut() else {
        return;
    };
    let area = centred(f.area(), 60, 60);
    // **下を消してから描く。** 消さないと、後ろの文字が隙間から透ける。
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(overlay.title())
        .border_style(Style::default().fg(Color::Cyan));

    match overlay {
        Overlay::Workspace { items, cursor } | Overlay::Env { items, cursor } => {
            if items.is_empty() {
                f.render_widget(
                    Paragraph::new("ありません")
                        .style(Style::default().fg(Color::DarkGray))
                        .block(block),
                    area,
                );
                return;
            }
            let list = List::new(
                items
                    .iter()
                    .map(|n| ListItem::new(n.clone()))
                    .collect::<Vec<_>>(),
            )
            .block(block)
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
            let mut state = ListState::default();
            state.select(Some(*cursor));
            f.render_stateful_widget(list, area, &mut state);
        }
        Overlay::Vars { rows, scroll } => {
            let lines: Vec<Line> = if rows.is_empty() {
                vec![Line::from(Span::styled(
                    "この環境に変数はありません",
                    Style::default().fg(Color::DarkGray),
                ))]
            } else {
                rows.iter()
                    .map(|r| {
                        let mut spans = vec![
                            Span::styled(
                                format!("{:<20}", r.name),
                                Style::default().add_modifier(Modifier::BOLD),
                            ),
                            // 秘匿値は伏せ字が入っている。ここで生の値に触る経路は無い。
                            Span::styled(
                                format!("{:<24}", r.shown),
                                if r.secret {
                                    Style::default().fg(Color::Yellow)
                                } else {
                                    Style::default()
                                },
                            ),
                            Span::styled(
                                format!(" {}", r.source),
                                Style::default().fg(Color::DarkGray),
                            ),
                        ];
                        if !r.expires.is_empty() {
                            spans.push(Span::styled(
                                format!("  期限 {}", r.expires),
                                Style::default().fg(Color::DarkGray),
                            ));
                        }
                        Line::from(spans)
                    })
                    .collect()
            };
            let max = (lines.len() as u16).saturating_sub(area.height.saturating_sub(2));
            scroll.clamp(max);
            f.render_widget(
                Paragraph::new(lines).scroll((scroll.top(), 0)).block(block),
                area,
            );
        }
    }
}

fn header(app: &App) -> Paragraph<'_> {
    let env = app.env.clone().unwrap_or_else(|| "-".into());
    let mut spans = vec![
        Span::styled(" ailo ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("workspace: {}  env: {env}", app.workspace),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    // **切れていることだけを出す。** 既定は入りなので、入っているときに
    // 出しても場所を食うだけ。切れているのに気づかないほうが困る。
    // **切れているものだけを出す。** 既定のままなら出しても場所を食うだけで、
    // 既定から外れていることに気づかないほうが困る。
    if !app.dump {
        spans.push(Span::styled(
            "  ダンプ切",
            Style::default().fg(Color::Yellow),
        ));
    }
    if !app.mouse {
        spans.push(Span::styled(
            "  マウス切",
            Style::default().fg(Color::Yellow),
        ));
    }
    Paragraph::new(Line::from(spans))
}

/// 案内の候補。長いものから順に、**実際に収まるもの**を選ぶ。
///
/// 幅の閾値を定数で決めていたときは、60〜63 セルで最後の `q 終了` だけが
/// 切れていた。消えるのが**抜け方**なので、初見の利用者は raw モードの画面に
/// 取り残される。**途中で切れた案内は、無いより悪い。**
const NORMAL_HINTS: [&str; 4] = [
    " Tab ペイン  ↑↓ 移動  Enter 送信  / 絞込  w ws  E 環境  v 変数  d ダンプ  e 編集  q 終了",
    " Tab ペイン   ↑↓ 移動   Enter 送信   e 編集   q 終了",
    " Tab ペイン   Enter 送信   q 終了",
    " q 終了",
];

fn fits(text: &str, width: u16) -> bool {
    UnicodeWidthStr::width(text) <= width as usize
}

/// 収まる候補のうち一番長いもの。どれも収まらなければ一番短いもの。
fn best_hint(candidates: &[&'static str], width: u16) -> &'static str {
    candidates
        .iter()
        .copied()
        .find(|t| fits(t, width))
        .unwrap_or_else(|| candidates.last().copied().unwrap_or(""))
}

fn footer(app: &App, width: u16) -> Paragraph<'_> {
    let text = match app.mode {
        Mode::Filter => {
            // 絞り込み中に一番要るのは「戻り方」。検索語が長いと押し出されるので、
            // 収まらなければ検索語のほうを削る。
            let full = format!(" 絞り込み: {}   Enter 確定   Esc 取消", app.filter);
            if fits(&full, width) {
                full
            } else {
                " Enter 確定   Esc 取消".to_string()
            }
        }
        Mode::Normal => best_hint(&NORMAL_HINTS, width).to_string(),
    };
    Paragraph::new(Span::styled(text, Style::default().fg(Color::DarkGray)))
}

fn sidebar(f: &mut Frame, app: &App, area: Rect) {
    let visible = app.visible();
    let items: Vec<ListItem> = visible
        .iter()
        .map(|e| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<6}", e.req.method),
                    Style::default().fg(method_colour(&e.req.method)),
                ),
                Span::raw(e.name.clone()),
            ]))
        })
        .collect();

    let title = if app.filter.is_empty() {
        " 保存済み ".to_string()
    } else {
        format!(" 保存済み /{} ", app.filter)
    };
    let block = || framed(&title, app.focus == Focus::List);

    if items.is_empty() {
        let hint = if app.filter.is_empty() {
            "ありません\n\n`ailo new <名前>` で\n定義できます"
        } else {
            "一致しません"
        };
        f.render_widget(
            Paragraph::new(hint)
                .wrap(Wrap { trim: false })
                .block(block()),
            area,
        );
        return;
    }

    let mut state = ListState::default();
    state.select(Some(app.selected_index()));
    let list = List::new(items)
        .block(block())
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("");
    f.render_stateful_widget(list, area, &mut state);
}

fn detail(f: &mut Frame, app: &mut App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Percentage(45),
        ])
        .split(area);

    app.areas.endpoint = rows[0];
    app.areas.tabs = rows[1];
    app.areas.definition = rows[2];
    app.areas.response = rows[3];

    // **借りたまま `app` に書き戻せない**ので、必要な分だけ複製する。
    // 定義 1 件は小さく、1 フレームに 1 回しか作らない。
    let Some(entry) = app.selected().cloned() else {
        f.render_widget(
            Paragraph::new("リクエストを選んでください")
                .block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    };

    // 1. エンドポイント
    f.render_widget(
        Paragraph::new(Line::from(
            vec![Span::styled(
                format!("{} ", entry.req.method),
                Style::default()
                    .fg(method_colour(&entry.req.method))
                    .add_modifier(Modifier::BOLD),
            )]
            .into_iter()
            .chain(with_vars(&display_url(&entry.req.url), &app.known_vars))
            .collect::<Vec<_>>(),
        ))
        .wrap(Wrap { trim: true })
        .block(framed(" エンドポイント ", app.focus == Focus::Endpoint)),
        rows[0],
    );

    // 2. タブ
    let selected = Tab::ALL.iter().position(|t| *t == app.tab).unwrap_or(0);
    let tabs_focused = app.focus == Focus::Tabs;
    // **件数を出す。** 空のタブと中身のあるタブが同じ見た目だと、
    // 開いてみるまで何が入っているか分からない。
    let labels: Vec<String> = Tab::ALL
        .iter()
        .map(|t| match tab_count(&entry.req, *t) {
            0 => t.label().to_string(),
            n => format!("{} {n}", t.label()),
        })
        .collect();
    f.render_widget(
        Tabs::new(labels.clone())
            .select(selected)
            .divider(" ")
            // フォーカスが当たっているときだけ、選択中のタブを反転させる。
            // 反転しっぱなしだと、左右キーが効く状態かどうかが分からない。
            .highlight_style(if tabs_focused {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            }),
        rows[1],
    );
    app.areas.tab_items = tab_rects(rows[1], &labels);

    // 3. タブの中身
    let lines = tab_lines(&entry.req, app.tab);
    let body: Vec<Line> = if lines.is_empty() {
        vec![Line::from(Span::styled(
            "（なし）",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        lines
            .into_iter()
            .map(|l| Line::from(with_vars(&l, &app.known_vars)))
            .collect()
    };
    app.definition_max_top = max_top(body.len(), rows[2].height);
    app.definition_scroll.clamp(app.definition_max_top);
    f.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .scroll((app.definition_scroll.top(), 0))
            .block(framed(
                &scroll_title("", app.definition_scroll.top(), app.definition_max_top),
                app.focus == Focus::Definition,
            )),
        rows[2],
    );

    // 4. レスポンス
    let lines = response_lines(app);
    app.response_max_top = max_top(lines.len(), rows[3].height);
    app.response_scroll.clamp(app.response_max_top);
    let title = scroll_title(
        " レスポンス ",
        app.response_scroll.top(),
        app.response_max_top,
    );
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.response_scroll.top(), 0))
            .block(framed(&title, app.focus == Focus::Response)),
        rows[3],
    );
}

/// タブ 1 つずつの矩形。`Tabs` の並べ方（ラベル + `divider(" ")`）に合わせて数える。
///
/// ratatui は各タブの位置を教えてくれないので、同じ規則で数え直す。
/// **`Tabs` の組み立てを変えたらここも変える。** ずれると、隣のタブが選ばれる。
fn tab_rects(area: Rect, labels: &[String]) -> [Rect; Tab::ALL.len()] {
    let mut out = [Rect::default(); Tab::ALL.len()];
    // `Tabs` は先頭に 1 桁の余白を置く。
    let mut x = area.x + 1;
    for (i, label) in labels.iter().enumerate() {
        let w = UnicodeWidthStr::width(label.as_str()) as u16;
        out[i] = Rect {
            x,
            y: area.y,
            width: w.min(area.width.saturating_sub(x - area.x)),
            height: 1,
        };
        // ラベル + 区切り(" " の左右に 1 桁ずつ)。
        x += w + 3;
    }
    out
}

/// スクロールできる枠の見出し。**まだ下があることを見せる。**
/// 見せないと、最後まで読んだのか途中なのかが分からない。
fn scroll_title(name: &str, top: u16, max_top: u16) -> String {
    if max_top == 0 {
        return name.to_string();
    }
    format!("{name}[{top}/{max_top}] ")
}

/// レスポンス欄に描く行。**枠は呼び出し側が付ける**（スクロールの見出しを載せるため）。
fn response_lines(app: &App) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    match &app.pane {
        Pane::Idle => vec![Line::from(Span::styled("Enter で送信", dim))],
        Pane::Sending => vec![Line::from("送信中…（Esc で打ち切り）".to_string())],
        Pane::Failed(msg) => msg
            .lines()
            .map(|l| Line::from(Span::styled(l.to_string(), Style::default().fg(Color::Red))))
            .collect(),
        Pane::Done {
            status,
            status_text,
            ms,
            bytes,
            content_type,
            shape,
            body,
            dump,
            notes,
        } => {
            let mut lines = vec![Line::from(vec![
                Span::styled(
                    format!("{status} {status_text}"),
                    Style::default()
                        .fg(status_colour(*status))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "  {ms}ms  {}  {content_type}",
                        crate::output::human_size(*bytes)
                    ),
                    dim,
                ),
            ])];
            if let Some(path) = dump {
                lines.push(Line::from(Span::styled(format!("dump: {path}"), dim)));
            }
            for note in notes {
                lines.push(Line::from(Span::styled(
                    note.clone(),
                    Style::default().fg(Color::Yellow),
                )));
            }
            // 形があるならそれを先に。全文はこの下に続く。
            if let Some(shape) = shape {
                lines.extend(shape.lines().map(|l| Line::from(l.to_string())));
                lines.push(Line::from(Span::styled("── 本文 ──", dim)));
            }
            lines.extend(body.lines().map(|l| Line::from(l.to_string())));
            lines
        }
    }
}

fn method_colour(method: &str) -> Color {
    match method.to_ascii_uppercase().as_str() {
        "GET" => Color::Green,
        "POST" => Color::Yellow,
        "PUT" | "PATCH" => Color::Cyan,
        "DELETE" => Color::Red,
        _ => Color::Gray,
    }
}

fn status_colour(status: u16) -> Color {
    match status {
        200..=299 => Color::Green,
        300..=399 => Color::Cyan,
        400..=499 => Color::Yellow,
        _ => Color::Red,
    }
}

/// 編集器。画面のほとんどを占める。
fn editor(f: &mut Frame, app: &mut App) {
    let Some(editing) = app.editing.as_mut() else {
        return;
    };
    let area = centred(f.area(), 84, 80);
    f.render_widget(Clear, area);

    // 落ちた理由がある回は、その分の行を下に空ける。
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if editing.error.is_some() {
            [Constraint::Min(3), Constraint::Length(4)]
        } else {
            [Constraint::Min(3), Constraint::Length(0)]
        })
        .split(area);

    let hint = if editing.target.single_line() {
        " Enter 保存   Esc 破棄 "
    } else {
        " Ctrl-S 保存   Esc 破棄 "
    };
    editing.area.set_block(
        Block::default()
            .borders(Borders::ALL)
            .title(editing.target.title())
            .title_bottom(hint)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    f.render_widget(&editing.area, rows[0]);

    if let Some(err) = &editing.error {
        f.render_widget(
            Paragraph::new(err.clone())
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(Color::Red))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" 保存できません "),
                ),
            rows[1],
        );
    }
}
