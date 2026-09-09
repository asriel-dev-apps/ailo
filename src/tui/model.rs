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
    /// ダンプを書くか。**画面から切れるようにする**（ユーザー要望 2026-09-09）。
    pub dump: bool,
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

    /// 一覧の何行目をクリックしたか。枠の 1 行を差し引く。
    pub fn list_row_at(&self, y: u16) -> Option<usize> {
        let inner_top = self.list.y + 1;
        let inner_bottom = self.list.y + self.list.height.saturating_sub(1);
        (y >= inner_top && y < inner_bottom).then(|| (y - inner_top) as usize)
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
            dump: true,
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

/// `--raw` の本文を画面に出せる形にする。
///
/// **item 記法の判定に通してはいけない。** `{"password":"hunter2"}` は
/// `parse_item` に**成功する**（`{"password"` という名前のヘッダと読まれる）ので、
/// item として検査すると素通りする。ログインの本文は一番秘匿値が入る場所なので、
/// ここが素通りすると「表示側でも落とす」という主張が成り立たない。
fn mask_raw_body(raw: &str) -> String {
    if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(raw) {
        mask_json_in_place(&mut value);
        return serde_json::to_string(&value).unwrap_or_else(|_| MASK.to_string());
    }
    // JSON として読めない本文は構造で判断できない。秘匿らしい綴りがあれば
    // 行ごと落とす。**「読めなかったから素通し」にはしない。**
    let lower = raw.to_ascii_lowercase();
    let suspicious = ["password", "passwd", "token", "secret", "api_key", "apikey"]
        .iter()
        .any(|w| lower.contains(*w));
    if suspicious && !raw.contains("{{") {
        format!("(本文は画面に出しません。`ailo show` で確かめてください) {MASK}")
    } else {
        raw.to_string()
    }
}

fn mask_json_in_place(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                let literal = match v {
                    serde_json::Value::String(s) => !s.contains("{{"),
                    _ => true,
                };
                if is_secret_name(key) && literal {
                    *v = serde_json::Value::String(MASK.to_string());
                } else {
                    mask_json_in_place(v);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(mask_json_in_place),
        _ => {}
    }
}

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
/// **`Redactor::url` に投げるだけでは足りない。** あれは `Url::parse` に失敗した入力を
/// そのまま返すので、`{{base_url}}/x?api_key=...` という**この repo で一番普通の形**だけが
/// 落ちない。クエリは自分で分解して落とし、そのうえで parse できるものは
/// `Redactor::url` にも通す（userinfo のパスワードなど、クエリ以外の経路のため）。
pub fn display_url(url: &str) -> String {
    let masked = mask_query(url);
    if reqwest::Url::parse(&masked).is_ok() {
        crate::redact::Redactor::new(true).url(&masked)
    } else {
        masked
    }
}

fn mask_query(url: &str) -> String {
    let Some((head, query)) = url.split_once('?') else {
        return url.to_string();
    };
    // fragment は落とさない。秘匿値の置き場所ではないうえ、`#` の後ろまで
    // クエリとして扱うと、素の断片まで書き換えてしまう。
    let (query, fragment) = match query.split_once('#') {
        Some((q, f)) => (q, Some(f)),
        None => (query, None),
    };
    let masked: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) if is_secret_name(name) && !value.contains("{{") => {
                format!("{name}={MASK}")
            }
            _ => pair.to_string(),
        })
        .collect();
    let mut out = format!("{head}?{}", masked.join("&"));
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    out
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
                out.push(mask_raw_body(raw));
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
