//! TUI が持つ状態と、そこにかかる規則。**描画も端末も触らない。**
//!
//! 端末を握る部分と分けてあるのは、テストのため。ratatui のイベントループを
//! 動かさないと確かめられない作りにすると、キー操作の規則は結局テストされない。

use std::collections::BTreeMap;

use crate::config::SavedRequest;

/// 右側の切り替えタブ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Body,
    Headers,
    Query,
    Capture,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Body, Tab::Headers, Tab::Query, Tab::Capture];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Body => "Body",
            Tab::Headers => "Headers",
            Tab::Query => "Query",
            Tab::Capture => "Capture",
        }
    }

    pub fn next(self) -> Tab {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Tab {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// レスポンス欄の状態。
#[derive(Debug, Clone, PartialEq)]
pub enum Pane {
    /// まだ送っていない。
    Idle,
    Sending,
    /// 送れた。本文は**マスク済み**のものだけを持つ。
    Done {
        status: u16,
        status_text: String,
        ms: u64,
        bytes: usize,
        content_type: String,
        /// 形の要約。JSON でなければ `None`。
        shape: Option<String>,
        /// マスク済みの本文（先頭のみ）。
        body: String,
        dump: Option<String>,
        notes: Vec<String>,
    },
    Failed(String),
}

/// 一覧に出す 1 件。
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub req: SavedRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// 一覧の絞り込み中。キー入力は検索語に入る。
    Filter,
}

pub struct App {
    all: Vec<Entry>,
    pub filter: String,
    pub mode: Mode,
    /// 絞り込み後の一覧における選択位置。
    selected: usize,
    pub tab: Tab,
    pub pane: Pane,
    pub workspace: String,
    pub env: Option<String>,
    pub quit: bool,
}

impl App {
    pub fn new(entries: Vec<Entry>, workspace: impl Into<String>, env: Option<String>) -> Self {
        Self {
            all: entries,
            filter: String::new(),
            mode: Mode::Normal,
            selected: 0,
            tab: Tab::Body,
            pane: Pane::Idle,
            workspace: workspace.into(),
            env,
            quit: false,
        }
    }

    /// 絞り込みを通した一覧。大文字小文字は無視する。
    pub fn visible(&self) -> Vec<&Entry> {
        if self.filter.is_empty() {
            return self.all.iter().collect();
        }
        let needle = self.filter.to_lowercase();
        self.all
            .iter()
            .filter(|e| e.name.to_lowercase().contains(&needle))
            .collect()
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn selected(&self) -> Option<&Entry> {
        self.visible().get(self.selected).copied()
    }

    pub fn move_down(&mut self) {
        let n = self.visible().len();
        if n > 0 {
            self.selected = (self.selected + 1) % n;
        }
    }

    pub fn move_up(&mut self) {
        let n = self.visible().len();
        if n > 0 {
            self.selected = (self.selected + n - 1) % n;
        }
    }

    /// 絞り込みを変えたら選択位置を畳み直す。
    ///
    /// **これを忘れると、絞り込みで一覧が短くなったときに選択が範囲外へ残り、
    /// 「何も選ばれていないのに Enter が効かない」**という無言の壊れ方をする。
    fn clamp(&mut self) {
        let n = self.visible().len();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
    }

    pub fn push_filter(&mut self, c: char) {
        self.filter.push(c);
        self.clamp();
    }

    pub fn pop_filter(&mut self) {
        self.filter.pop();
        self.clamp();
    }

    /// 一覧を読み直す。編集で名前が消えても選択が範囲外に残らないようにする。
    pub fn reload(&mut self, entries: Vec<Entry>) {
        self.all = entries;
        self.clamp();
    }

    pub fn clear_filter(&mut self) {
        self.filter.clear();
        self.clamp();
    }
}

/// タブごとに見せる中身を、保存済みの定義から組む。
///
/// **展開しない。**`{{token}}` を展開して見せると画面に秘匿値が出る。
/// 展開後の姿はダンプで確かめる。
///
/// **展開しないだけでは足りない。** 定義そのものに `Authorization: Bearer <生の値>` が
/// 直書きされていることがある。`ailo new` は既知の秘匿名を拒むが、手で書いた
/// `requests.toml`・古い版で作った定義・`parse_item` が読めない綴りは通り抜ける。
/// 表示の側でも落とす。
pub fn tab_lines(req: &SavedRequest, tab: Tab) -> Vec<String> {
    let lines = tab_lines_raw(req, tab);
    match tab {
        // Capture は item 記法ではなく「名前 = 式」。値を持たないので落とすものが無く、
        // item として読ませると式のほうが値だと解釈されて潰れる。
        Tab::Capture => lines,
        _ => lines.iter().map(|l| mask_literal_secret(l)).collect(),
    }
}

/// 名前が秘匿らしく、値がテンプレートでないなら値を落とす。
///
/// `{{token}}` はそのまま残す。名前しか出ていないので、それ自体は漏れない。
/// 区切りは正規の綴りで書き直す（escape を含む名前を復元しようとして
/// 間違えるより、`名前 = ***` と分かる形のほうがよい）。
fn mask_literal_secret(line: &str) -> String {
    use crate::args::Item;

    let Ok(item) = crate::args::parse_item(line) else {
        // **読めなかった行も素通しにしない。** `password:=hunter2` のように
        // JSON として壊れた item は `parse_item` が落ちる。「読めたものだけ検査する」に
        // すると、一番危ない行だけが素通りする。
        return mask_unparsed(line);
    };

    let literal = |value: &str| !value.contains("{{");
    match &item {
        Item::Header { name, value } if is_sensitive_header_name(name) && literal(value) => {
            format!("{name}: {MASK}")
        }
        Item::Field { name, value } if is_secret_name(name) && literal(value) => {
            format!("{name}={MASK}")
        }
        Item::Query { name, value } if is_secret_name(name) && literal(value) => {
            format!("{name}=={MASK}")
        }
        Item::RawField { name, value } if is_secret_name(name) && literal(&value.to_string()) => {
            format!("{name}:={MASK}")
        }
        _ => line.to_string(),
    }
}

/// 画面に出すマスク。ダンプ側と同じ綴りにする。
const MASK: &str = "***";

fn is_secret_name(name: &str) -> bool {
    crate::redact::is_sensitive_field(name)
}

/// `parse_item` が読めなかった行。名前らしき先頭だけ残して、後ろを落とす。
fn mask_unparsed(line: &str) -> String {
    let head: String = line
        .chars()
        .take_while(|c| !matches!(c, ':' | '=' | '@'))
        .collect();
    if is_secret_name(&head) && !line.contains("{{") {
        format!("{head} {MASK}")
    } else {
        line.to_string()
    }
}

/// 既定の秘匿ヘッダ名。`Redactor` と同じ判定を使う。
fn is_sensitive_header_name(name: &str) -> bool {
    crate::redact::Redactor::new(true).is_sensitive_header(name)
}

/// URL も同じ理由で落とす。`?api_key=<生の値>` は画面にも残したくない。
///
/// 展開していないので、生の秘匿値が入るのは `{{...}}` でない部分だけ。
pub fn display_url(url: &str) -> String {
    crate::redact::Redactor::new(true).url(url)
}

fn tab_lines_raw(req: &SavedRequest, tab: Tab) -> Vec<String> {
    match tab {
        Tab::Body => {
            let mut out: Vec<String> = req
                .items
                .iter()
                .filter(|i| is_body_item(i))
                .cloned()
                .collect();
            if let Some(raw) = &req.raw {
                out.push(raw.clone());
            }
            if req.form {
                out.push("(form-urlencoded で送る)".into());
            }
            out
        }
        Tab::Headers => req
            .items
            .iter()
            .filter(|i| matches!(parsed(i), Some(crate::args::Item::Header { .. })))
            .cloned()
            .collect(),
        Tab::Query => req
            .items
            .iter()
            .filter(|i| matches!(parsed(i), Some(crate::args::Item::Query { .. })))
            .cloned()
            .collect(),
        Tab::Capture => capture_lines(&req.capture, &req.secret),
    }
}

fn parsed(raw: &str) -> Option<crate::args::Item> {
    crate::args::parse_item(raw).ok()
}

fn is_body_item(raw: &str) -> bool {
    matches!(
        parsed(raw),
        Some(
            crate::args::Item::Field { .. }
                | crate::args::Item::RawField { .. }
                | crate::args::Item::FileField { .. }
        )
    )
}

fn capture_lines(capture: &BTreeMap<String, String>, secret: &[String]) -> Vec<String> {
    capture
        .iter()
        .map(|(name, expr)| {
            // 「キーチェーンへ入る」ことは画面に出す。値は出さない。
            let mark = if secret.iter().any(|s| s == name) {
                "  → キーチェーン"
            } else {
                ""
            };
            format!("{name} = {expr}{mark}")
        })
        .collect()
}
