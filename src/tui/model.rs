//! TUI が持つ状態と、そこにかかる規則。**描画も端末も触らない。**
//!
//! 端末を握る部分と分けてあるのは、テストのため。ratatui のイベントループを
//! 動かさないと確かめられない作りにすると、キー操作の規則は結局テストされない。

use std::collections::BTreeMap;

use ratatui::layout::Rect;

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

/// いまキーが効く場所。**ペインごとに「選ぶ」「スクロールする」の意味が変わる**ので、
/// どこに当たっているかを常に画面に出す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// 左の一覧。
    List,
    /// 右上のエンドポイント。
    Endpoint,
    /// タブの切り替え。
    Tabs,
    /// タブの中身（Body / Headers / Query / Capture）。
    Definition,
    /// レスポンス。
    Response,
}

impl Focus {
    /// 右回り。左の一覧から始めて、上から下へ辿る。
    pub const RING: [Focus; 5] = [
        Focus::List,
        Focus::Endpoint,
        Focus::Tabs,
        Focus::Definition,
        Focus::Response,
    ];

    pub fn next(self) -> Focus {
        let i = Self::RING.iter().position(|f| *f == self).unwrap_or(0);
        Self::RING[(i + 1) % Self::RING.len()]
    }

    pub fn prev(self) -> Focus {
        let i = Self::RING.iter().position(|f| *f == self).unwrap_or(0);
        Self::RING[(i + Self::RING.len() - 1) % Self::RING.len()]
    }
}

/// 縦スクロールの位置。**行数を持たせない。**
///
/// 中身の行数は描くときにしか分からない（折り返しがある）。位置だけを持ち、
/// 上限は描画側が知っている行数で毎回畳む。持たせると、内容が変わったのに
/// 上限が古いまま残り、「スクロールできない」「空白まで進む」になる。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Scroll {
    top: u16,
}

impl Scroll {
    pub fn top(self) -> u16 {
        self.top
    }

    pub fn down(&mut self, lines: u16, max_top: u16) {
        self.top = (self.top + lines).min(max_top);
    }

    pub fn up(&mut self, lines: u16) {
        self.top = self.top.saturating_sub(lines);
    }

    pub fn to_start(&mut self) {
        self.top = 0;
    }

    pub fn to_end(&mut self, max_top: u16) {
        self.top = max_top;
    }

    /// 内容が変わったら先頭へ戻す。前の位置を残すと、短い中身で空白が出る。
    pub fn reset(&mut self) {
        self.top = 0;
    }

    /// 描画時に、実際の行数で畳む。
    pub fn clamp(&mut self, max_top: u16) {
        self.top = self.top.min(max_top);
    }
}

/// 画面の上にかぶせるもの。**同時に 1 つだけ**。
///
/// 重ねられるようにすると、閉じ方が状態ごとに変わって迷子になる。
/// どれが出ていても `Esc` で閉じる。
#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    /// workspace を選ぶ。
    Workspace { items: Vec<String>, cursor: usize },
    /// 環境を選ぶ。
    Env { items: Vec<String>, cursor: usize },
    /// 変数の一覧。選ぶものではないのでカーソルは持たず、スクロールだけ。
    Vars { rows: Vec<VarRow>, scroll: Scroll },
}

impl Overlay {
    pub fn title(&self) -> &'static str {
        match self {
            Overlay::Workspace { .. } => " workspace を選ぶ ",
            Overlay::Env { .. } => " 環境を選ぶ ",
            Overlay::Vars { .. } => " 変数 ",
        }
    }

    pub fn move_cursor(&mut self, down: bool) {
        let (items, cursor) = match self {
            Overlay::Workspace { items, cursor } | Overlay::Env { items, cursor } => {
                (items.len(), cursor)
            }
            Overlay::Vars { .. } => return,
        };
        if items == 0 {
            return;
        }
        *cursor = if down {
            (*cursor + 1) % items
        } else {
            (*cursor + items - 1) % items
        };
    }

    /// いま当たっている項目。変数一覧には無い。
    pub fn chosen(&self) -> Option<&str> {
        match self {
            Overlay::Workspace { items, cursor } | Overlay::Env { items, cursor } => {
                items.get(*cursor).map(String::as_str)
            }
            Overlay::Vars { .. } => None,
        }
    }
}

/// 変数 1 つ。**値は「見せてよいものだけ」を入れて渡す。**
///
/// 秘匿かどうかの判断をここでやらない。作る側が済ませたものだけを持つので、
/// 描画側が誤って生の値を出す経路が無い。
#[derive(Debug, Clone, PartialEq)]
pub struct VarRow {
    pub name: String,
    /// 表示してよい値。秘匿値は既に伏せてある。
    pub shown: String,
    /// どこから来たか（`config` / `capture` / 環境変数 など）。
    pub source: String,
    /// 失効時刻。無ければ空。
    pub expires: String,
    pub secret: bool,
}

/// テンプレートの断片。`{{名前}}` を色分けして描くために切り出す。
#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    Text(String),
    /// `{{名前}}`。`resolved` は、いまの環境で値が付くか。
    Var {
        name: String,
        resolved: bool,
    },
}

/// `{{名前}}` で切り分ける。
///
/// **解決できるかどうかを見せるのが目的。** Postman が未解決を赤で出すのと同じ。
/// 送ってから「変数が無い」と言われるより、書いている時点で分かるほうが早い。
pub fn split_vars(text: &str, known: &[String]) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let Some(len) = rest[start..].find("}}") else {
            break;
        };
        let end = start + len;
        if start > 0 {
            out.push(Piece::Text(rest[..start].to_string()));
        }
        let name = rest[start + 2..end].trim().to_string();
        let resolved = known.contains(&name);
        out.push(Piece::Var { name, resolved });
        rest = &rest[end + 2..];
    }
    if !rest.is_empty() {
        out.push(Piece::Text(rest.to_string()));
    }
    out
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
        /// マスク済みの本文。**全文**を持つ。
        ///
        /// 先頭だけを持っていたときは、画面で追える量がそこで頭打ちだった。
        /// スクロールできる以上、切る理由が無い。全文はどのみちダンプにある。
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

#[derive(Clone)]
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
    pub focus: Focus,
    /// 定義（タブの中身）のスクロール位置。
    pub definition_scroll: Scroll,
    /// レスポンスのスクロール位置。
    pub response_scroll: Scroll,
    /// **描画側が毎回書き込む**、これ以上は下げられない位置。
    ///
    /// 何行あるかは、幅が決まって折り返してみるまで分からない。だから状態には
    /// 持たず、描いた側が実測を書き戻す。キー操作はその値で畳む。
    /// 描く前に押されたキーは 0 で畳まれるが、1 フレーム目だけの話で害が無い。
    pub definition_max_top: u16,
    pub response_max_top: u16,
    /// **描画側が毎回書き込む**、各ペインが占めている画面上の矩形。
    /// マウスのクリックをペインに対応づけるのに要る。
    pub areas: Areas,
    /// かぶせて出ているもの。
    pub overlay: Option<Overlay>,
    /// 編集中なら、その状態。**編集中はほかのキーを全部そちらへ渡す。**
    pub editing: Option<crate::tui::editor::Editing>,
    /// ダンプを書くか。**画面から切れるようにする**（ユーザー要望 2026-09-09）。
    pub dump: bool,
    /// いまの環境で値の付く変数の名前。**値は持たない**（画面に出すのは名前だけ）。
    /// 未解決の `{{名前}}` を別色にするのに使う。
    pub known_vars: Vec<String>,
    /// マウスの捕捉が有効か。
    ///
    /// **切れるようにしておく。** 捕捉したままだと端末側のテキスト選択・コピーが
    /// できなくなる。TUI でこれを塞ぐと、出力を貼りたいだけの人が詰む。
    pub mouse: bool,
}

/// 各ペインの矩形。クリックの当たり判定に使う。
#[derive(Debug, Clone, Copy, Default)]
pub struct Areas {
    pub list: Rect,
    pub endpoint: Rect,
    pub tabs: Rect,
    /// タブ 1 つずつの矩形。クリックでそのタブへ切り替えるため。
    pub tab_items: [Rect; Tab::ALL.len()],
    pub definition: Rect,
    pub response: Rect,
    /// 一覧が実際にスクロールしていた量。**描画側が書き戻す。**
    /// `ListState` の offset は描いてみるまで決まらない（`max_top` と同じ理由）。
    pub list_offset: usize,
}

impl Areas {
    /// その点がどのペインか。どれでもなければ `None`。
    pub fn hit(&self, x: u16, y: u16) -> Option<Focus> {
        let inside = |r: Rect| {
            r.width > 0
                && r.height > 0
                && x >= r.x
                && x < r.x + r.width
                && y >= r.y
                && y < r.y + r.height
        };
        // 上から順に見る。重なりは無いが、順序を決めておかないと
        // 実装を変えたときに当たり先が黙って入れ替わる。
        if inside(self.list) {
            Some(Focus::List)
        } else if inside(self.endpoint) {
            Some(Focus::Endpoint)
        } else if inside(self.tabs) {
            Some(Focus::Tabs)
        } else if inside(self.definition) {
            Some(Focus::Definition)
        } else if inside(self.response) {
            Some(Focus::Response)
        } else {
            None
        }
    }

    /// その点にあるタブの番号。
    pub fn tab_at(&self, x: u16, y: u16) -> Option<usize> {
        self.tab_items.iter().position(|r| {
            r.width > 0 && x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
        })
    }

    /// 一覧の何行目をクリックしたか。枠の 1 行を差し引き、**スクロール量を足す。**
    ///
    /// 足さないと、下までスクロールした一覧で最上行を押したときに先頭が選ばれる。
    /// 押した覚えのないリクエストが、その場の `Enter` で飛ぶ。
    pub fn list_row_at(&self, y: u16) -> Option<usize> {
        let inner_top = self.list.y + 1;
        let inner_bottom = self.list.y + self.list.height.saturating_sub(1);
        (y >= inner_top && y < inner_bottom).then(|| (y - inner_top) as usize + self.list_offset)
    }
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
            focus: Focus::List,
            definition_scroll: Scroll::default(),
            response_scroll: Scroll::default(),
            definition_max_top: 0,
            response_max_top: 0,
            areas: Areas::default(),
            overlay: None,
            editing: None,
            dump: true,
            known_vars: Vec::new(),
            mouse: true,
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
            self.on_request_changed();
        }
    }

    pub fn move_up(&mut self) {
        let n = self.visible().len();
        if n > 0 {
            self.selected = (self.selected + n - 1) % n;
            self.on_request_changed();
        }
    }

    /// 一覧の n 番目を選ぶ。範囲外は無視する。
    pub fn select_visible(&mut self, index: usize) {
        if index < self.visible().len() && index != self.selected {
            self.selected = index;
            self.on_request_changed();
        }
    }

    /// workspace のピッカーを開く。いまの workspace に当たりを合わせる。
    pub fn open_workspace_picker(&mut self, items: Vec<String>) {
        let cursor = items.iter().position(|n| *n == self.workspace).unwrap_or(0);
        self.overlay = Some(Overlay::Workspace { items, cursor });
    }

    /// 環境のピッカーを開く。
    pub fn open_env_picker(&mut self, items: Vec<String>) {
        let cursor = self
            .env
            .as_deref()
            .and_then(|e| items.iter().position(|n| n == e))
            .unwrap_or(0);
        self.overlay = Some(Overlay::Env { items, cursor });
    }

    pub fn open_vars(&mut self, rows: Vec<VarRow>) {
        self.overlay = Some(Overlay::Vars {
            rows,
            scroll: Scroll::default(),
        });
    }

    /// 環境を変える。**変数もレスポンスも作り直しになる**ので、
    /// 表示中のレスポンスは捨てる（別の環境の結果が残るのは誤読のもと）。
    pub fn set_env(&mut self, name: String) {
        if self.env.as_deref() != Some(name.as_str()) {
            self.env = Some(name);
            self.pane = Pane::Idle;
            self.response_scroll.reset();
        }
    }

    /// 見ているリクエストが変わったときの後始末。
    ///
    /// **スクロール位置を持ち越さない。** 持ち越すと、短い定義に切り替えた瞬間に
    /// 空白だけが見え、「壊れた」と読める。レスポンスも前のリクエストのものなので捨てる。
    pub fn on_request_changed(&mut self) {
        self.definition_scroll.reset();
        self.response_scroll.reset();
        self.pane = Pane::Idle;
    }

    pub fn set_tab(&mut self, tab: Tab) {
        if self.tab != tab {
            self.tab = tab;
            self.definition_scroll.reset();
        }
    }

    /// 絞り込みを変えたら選択位置を畳み直す。
    ///
    /// **これを忘れると、絞り込みで一覧が短くなったときに選択が範囲外へ残り、
    /// 「何も選ばれていないのに Enter が効かない」**という無言の壊れ方をする。
    ///
    /// **指しているリクエストが変わったら、前のレスポンスも捨てる。**
    /// ここでやる（呼び出し側 3 か所に足すのではなく）。絞り込みだけこの不変条件を
    /// 破っていて、`login` の結果が出たまま `users` が選ばれている状態になっていた。
    fn clamp(&mut self) {
        let before = self.selected().map(|e| e.name.clone());
        let n = self.visible().len();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
        if self.selected().map(|e| e.name.clone()) != before {
            self.on_request_changed();
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

/// そのタブに何件入っているか。**空のタブと中身のあるタブを見分ける**ために出す。
/// Postman が `Headers 11` や `Body ●` を出しているのと同じ役目。
pub fn tab_count(req: &SavedRequest, tab: Tab) -> usize {
    tab_lines_raw(req, tab).len()
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
        _ => {
            let r = crate::redact::Redactor::new(true);
            lines
                .iter()
                .map(|l| crate::redact::mask_item(l, &r).text)
                .collect()
        }
    }
}

/// URL を画面に出せる形にする。判定は `redact` 側に 1 つだけ置いてある。
pub fn display_url(url: &str) -> String {
    crate::redact::mask_url(url).text
}

fn tab_lines_raw(req: &SavedRequest, tab: Tab) -> Vec<String> {
    match tab {
        Tab::Body => {
            let mut out: Vec<String> = req
                .items
                .iter()
                .filter(|i| tab_of(i) == Tab::Body)
                .cloned()
                .collect();
            if let Some(raw) = &req.raw {
                out.push(crate::redact::mask_body(raw).text);
            }
            if req.form {
                out.push("(form-urlencoded で送る)".into());
            }
            out
        }
        Tab::Headers | Tab::Query => req
            .items
            .iter()
            .filter(|i| tab_of(i) == tab)
            .cloned()
            .collect(),
        Tab::Capture => capture_lines(&req.capture, &req.secret),
    }
}

fn parsed(raw: &str) -> Option<crate::args::Item> {
    crate::args::parse_item(raw).ok()
}

/// その行がどのタブに出るか。**表示と編集器で同じ関数を使う。**
///
/// 別々に持っていたとき、`parse_item` が読めない行は表示のどのタブにも出ないのに
/// 編集器の Body には出ていた。**画面に出ないものが編集器には出る**ので、
/// そこから秘匿値が漏れた。どちらか一方を直しても、ずれの原因は残る。
///
/// 読めない行も落とさない。どこにも出さないと、そのタブを編集した時点で消える。
pub fn tab_of(raw: &str) -> Tab {
    match parsed(raw) {
        Some(crate::args::Item::Header { .. }) => Tab::Headers,
        Some(crate::args::Item::Query { .. }) => Tab::Query,
        _ => Tab::Body,
    }
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
