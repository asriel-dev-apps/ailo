//! TUI の中で定義を書き換える。
//!
//! **保存の経路は `ailo new` と同じ**（`config::Requests` へ書くところまで
//! `run::save_edited` に寄せる）。ここで独自に書き込むと、秘匿値のガードと
//! 編集中の衝突検出が片方だけ古くなる。
//!
//! 編集器そのものは `tui-textarea` に任せる。行の挿入・削除・やり直し・検索を
//! 自前で持つと、それだけで TUI 本体より大きくなる。

use anyhow::{Context, Result};
use tui_textarea::TextArea;

use crate::config::SavedRequest;

use super::model::Tab;

/// 何を編集しているか。**保存の仕方がここで決まる**ので、対象を型で持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// メソッドと URL。1 行。
    Endpoint,
    /// そのタブの item を 1 行ずつ。
    Items(Tab),
    /// `--raw` の本文。複数行。
    Raw,
    /// 定義まるごと（TOML）。
    Whole,
}

impl Target {
    pub fn title(self) -> String {
        match self {
            Target::Endpoint => " エンドポイントを編集 ".into(),
            Target::Items(tab) => format!(" {} を編集 ", tab.label()),
            Target::Raw => " 本文を編集 ".into(),
            Target::Whole => " 定義まるごとを編集（TOML）".into(),
        }
    }

    /// 1 行しか受け付けないか。
    pub fn single_line(self) -> bool {
        self == Target::Endpoint
    }
}

/// 編集中の状態。
#[derive(Clone)]
pub struct Editing {
    /// 検索中なら、打ちかけの語。**編集器の中の小さなモード**で、
    /// `Esc` で閉じる（外側の `Esc` は編集の破棄なので、先にこちらが拾う）。
    pub search: Option<String>,
    pub name: String,
    pub target: Target,
    pub area: TextArea<'static>,
    /// 開いたときの定義。**保存する直前に、これと今の中身を比べる。**
    /// 変わっていたら上書きしない（`ailo new` と同じ扱い）。
    pub opened_from: SavedRequest,
    /// 直前の保存で落ちた理由。画面に出す。
    pub error: Option<String>,
}

impl Editing {
    pub fn open(name: &str, req: &SavedRequest, target: Target) -> Self {
        let mut area = TextArea::new(initial_text(req, target));
        area.set_line_number_style(
            ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray),
        );
        Self {
            search: None,
            name: name.to_string(),
            target,
            area,
            opened_from: req.clone(),
            error: None,
        }
    }

    /// 検索語を反映し、次の一致へ飛ぶ。
    ///
    /// **語が空なら強調を消す。** 消さないと、閉じたあとも当たった場所が
    /// 光ったまま残る。
    pub fn apply_search(&mut self, forward: bool) {
        let Some(pattern) = self.search.clone() else {
            let _ = self.area.set_search_pattern("");
            return;
        };
        if pattern.is_empty() {
            let _ = self.area.set_search_pattern("");
            return;
        }
        // 正規表現として読めない途中の入力（`(` を打った瞬間など）で
        // 落とさない。打ち終わるまで一致が無いだけ。
        if self.area.set_search_pattern(&pattern).is_err() {
            return;
        }
        if forward {
            self.area.search_forward(false);
        } else {
            self.area.search_back(false);
        }
    }

    pub fn lines(&self) -> Vec<String> {
        self.area.lines().to_vec()
    }

    /// 編集した中身を定義に畳み込む。**書き込みはしない。**
    pub fn apply(&self) -> Result<SavedRequest> {
        apply(&self.opened_from, self.target, &self.lines())
    }
}

fn initial_text(req: &SavedRequest, target: Target) -> Vec<String> {
    match target {
        Target::Endpoint => vec![format!("{} {}", req.method, req.url)],
        Target::Items(tab) => items_of(req, tab),
        Target::Raw => req
            .raw
            .clone()
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect(),
        Target::Whole => toml::to_string_pretty(req)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect(),
    }
}

/// そのタブに属する item だけを取り出す。
///
/// **展開もマスクもしない。** 編集器に伏せ字を渡すと、保存した瞬間に
/// `***` が本物の値として書き込まれる。
fn items_of(req: &SavedRequest, tab: Tab) -> Vec<String> {
    req.items
        .iter()
        .filter(|raw| belongs_to(raw, tab))
        .cloned()
        .collect()
}

/// **どのタブに属するかは `model::tab_of` が 1 つだけ決める。**
/// ここで数え直すと、画面に出ていない行が編集器には出る、というずれになる。
fn belongs_to(raw: &str, tab: Tab) -> bool {
    super::model::tab_of(raw) == tab
}

/// 編集結果を定義へ畳む。
pub fn apply(base: &SavedRequest, target: Target, lines: &[String]) -> Result<SavedRequest> {
    let mut out = base.clone();
    match target {
        Target::Endpoint => {
            let line = lines.join(" ");
            let line = line.trim();
            let (method, url) = line
                .split_once(char::is_whitespace)
                .context("`メソッド URL` の形で書いてください（例: GET https://example.com/x）")?;
            if url.trim().is_empty() {
                anyhow::bail!("URL が空です");
            }
            out.method = method.to_ascii_uppercase();
            out.url = url.trim().to_string();
        }
        Target::Items(tab) => {
            // そのタブの分だけ入れ替える。**他のタブの item は触らない。**
            let kept: Vec<String> = base
                .items
                .iter()
                .filter(|raw| !belongs_to(raw, tab))
                .cloned()
                .collect();
            let mut edited = Vec::new();
            for line in lines {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                // 保存する前に読めることを確かめる。読めない行を書き込むと、
                // 次に送ったときに落ちる。
                crate::args::parse_item(line)
                    .with_context(|| format!("`{line}` を解釈できません"))?;
                // **そのタブに属する種類だけを受ける。** 課さないと、Headers の
                // 画面に `limit==50` と書くだけで Headers が消えて Query が増える
                // （下の `kept` が「そのタブでないもの」を残すため）。
                if !belongs_to(line, tab) {
                    anyhow::bail!(
                        "`{line}` は {} の書き方ではありません。ほかのタブか `T`（定義まるごと）で編集してください",
                        tab.label()
                    );
                }
                edited.push(line.to_string());
            }
            out.items = kept;
            out.items.extend(edited);
        }
        Target::Raw => {
            let text = lines.join("\n");
            out.raw = if text.trim().is_empty() {
                None
            } else {
                Some(text)
            };
        }
        Target::Whole => {
            out = toml::from_str(&lines.join("\n")).context("定義として読めません")?;
        }
    }
    Ok(out)
}
