//! 出力の作り分け。
//!
//! 既定は実行環境で切り替える。端末なら人が読む形、パイプやエージェント経由なら
//! ダイジェスト。エージェントに `--digest` を付け忘れさせないための既定値であって、
//! `--format` でいつでも上書きできる。

use std::io::IsTerminal;

use serde::Serialize;

use crate::dump::{BodyRecord, ResponseRecord};
use crate::paths;

/// ダイジェストで見せる本文の既定行数。
pub const DEFAULT_HEAD_LINES: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// 端末なら pretty、それ以外は digest。
    Auto,
    /// 色付き整形。本文は全文。
    Pretty,
    /// 1 行の要約 + ダンプの場所 + 本文の先頭数行。
    Digest,
    /// 機械可読の 1 行 JSON。本文は含めない。
    Json,
}

impl Format {
    pub fn resolve(self) -> Format {
        match self {
            Format::Auto => {
                if std::io::stdout().is_terminal() {
                    Format::Pretty
                } else {
                    Format::Digest
                }
            }
            other => other,
        }
    }
}

/// 端末のときだけ色を付ける。パイプ先に ANSI を流し込むと、
/// エージェントが読む文字列がエスケープで汚れる。
#[derive(Clone, Copy)]
pub struct Palette {
    on: bool,
}

impl Palette {
    pub fn detect() -> Self {
        let disabled = std::env::var_os("NO_COLOR").is_some();
        Self {
            on: !disabled && std::io::stdout().is_terminal(),
        }
    }

    pub fn plain() -> Self {
        Self { on: false }
    }

    fn wrap(&self, code: &str, s: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    pub fn status(&self, status: u16, s: &str) -> String {
        let code = match status {
            200..=299 => "32",
            300..=399 => "36",
            400..=499 => "33",
            _ => "31",
        };
        self.wrap(code, s)
    }

    pub fn dim(&self, s: &str) -> String {
        self.wrap("2", s)
    }

    pub fn key(&self, s: &str) -> String {
        self.wrap("34", s)
    }
}

pub fn human_size(bytes: usize) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

fn content_type(res: &ResponseRecord) -> &str {
    res.headers
        .get("content-type")
        .map(|s| s.split(';').next().unwrap_or(s).trim())
        .unwrap_or("-")
}

/// `200 OK  312ms  4.2 KB  application/json`
pub fn status_line(res: &ResponseRecord, p: &Palette) -> String {
    let head = format!("{} {}", res.status, res.status_text)
        .trim_end()
        .to_string();
    format!(
        "{}  {}  {}  {}",
        p.status(res.status, &head),
        p.dim(&format!("{}ms", res.ms)),
        p.dim(&human_size(res.bytes)),
        p.dim(content_type(res)),
    )
}

pub fn digest(
    res: &ResponseRecord,
    dump_path: Option<&std::path::Path>,
    head_lines: usize,
    p: &Palette,
) -> String {
    let mut out = vec![status_line(res, p)];

    if let Some(path) = dump_path {
        out.push(format!("{} {}", p.key("dump:"), paths::tildify(path)));
    }

    match res.body.as_text() {
        Some(text) => {
            let total = text.lines().count();
            if head_lines == 0 {
                out.push(p.dim(&format!("body: {total} lines (--full で全文)")));
            } else if total <= head_lines {
                out.push(p.key(&format!("body ({total} lines):")));
                out.push(text);
            } else {
                out.push(p.key(&format!("body (first {head_lines} lines of {total}):")));
                let head: Vec<&str> = text.lines().take(head_lines).collect();
                out.push(head.join("\n"));
                out.push(p.dim(&format!(
                    "… 残り {} 行。全文はダンプか --full で",
                    total - head_lines
                )));
            }
        }
        None => match &res.body {
            BodyRecord::Binary { bytes } => {
                out.push(p.dim(&format!("body: バイナリ {}", human_size(*bytes))))
            }
            _ => out.push(p.dim("body: なし")),
        },
    }

    out.join("\n")
}

pub fn pretty(res: &ResponseRecord, dump_path: Option<&std::path::Path>, p: &Palette) -> String {
    let mut out = vec![status_line(res, p)];
    if let Some(path) = dump_path {
        out.push(p.dim(&format!("dump: {}", paths::tildify(path))));
    }
    match res.body.as_text() {
        Some(text) => out.push(text),
        None => {
            if let BodyRecord::Binary { bytes } = &res.body {
                out.push(p.dim(&format!("(バイナリ {})", human_size(*bytes))));
            }
        }
    }
    out.join("\n")
}

#[derive(Serialize)]
struct MachineLine<'a> {
    status: u16,
    ms: u64,
    bytes: usize,
    content_type: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    dump: Option<String>,
}

/// 本文を含まない 1 行 JSON。パイプで受けて条件分岐したいとき用。
pub fn machine(res: &ResponseRecord, dump_path: Option<&std::path::Path>) -> String {
    let line = MachineLine {
        status: res.status,
        ms: res.ms,
        bytes: res.bytes,
        content_type: content_type(res),
        dump: dump_path.map(paths::tildify),
    };
    serde_json::to_string(&line).unwrap_or_else(|_| "{}".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn res(body: BodyRecord, bytes: usize) -> ResponseRecord {
        ResponseRecord {
            status: 200,
            status_text: "OK".into(),
            headers: BTreeMap::from([(
                "content-type".into(),
                "application/json; charset=utf-8".into(),
            )]),
            body,
            bytes,
            ms: 312,
        }
    }

    fn long_json() -> BodyRecord {
        let items: Vec<_> = (0..100).map(|i| serde_json::json!({"id": i})).collect();
        BodyRecord::Json {
            value: serde_json::json!({ "items": items }),
        }
    }

    #[test]
    fn human_size_switches_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(4300), "4.2 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn status_line_has_no_ansi_when_colour_is_off() {
        let line = status_line(&res(BodyRecord::Empty, 0), &Palette::plain());
        assert!(!line.contains('\x1b'), "{line}");
        assert!(line.starts_with("200 OK"), "{line}");
    }

    #[test]
    fn content_type_parameters_are_trimmed() {
        let line = status_line(&res(BodyRecord::Empty, 0), &Palette::plain());
        assert!(line.contains("application/json"), "{line}");
        assert!(!line.contains("charset"), "{line}");
    }

    #[test]
    fn digest_truncates_a_long_body_and_says_how_much_was_cut() {
        let out = digest(&res(long_json(), 4300), None, 20, &Palette::plain());
        assert!(out.contains("first 20 lines of"), "{out}");
        assert!(out.contains("残り"), "{out}");
        // 打ち切りが効いていること。効いていなければ 300 行以上になる。
        assert!(
            out.lines().count() < 30,
            "打ち切れていない: {}",
            out.lines().count()
        );
    }

    #[test]
    fn a_short_body_is_shown_whole_without_a_truncation_notice() {
        let body = BodyRecord::Json {
            value: serde_json::json!({"a": 1}),
        };
        let out = digest(&res(body, 8), None, 20, &Palette::plain());
        assert!(!out.contains("残り"), "{out}");
        assert!(out.contains("\"a\""), "{out}");
    }

    #[test]
    fn digest_names_the_dump_so_the_full_body_can_be_found_later() {
        let out = digest(
            &res(long_json(), 4300),
            Some(std::path::Path::new("/tmp/x.json")),
            20,
            &Palette::plain(),
        );
        assert!(out.contains("/tmp/x.json"), "{out}");
    }

    #[test]
    fn binary_bodies_report_size_instead_of_contents() {
        let out = digest(
            &res(BodyRecord::Binary { bytes: 2048 }, 2048),
            None,
            20,
            &Palette::plain(),
        );
        assert!(out.contains("2.0 KB"), "{out}");
    }

    #[test]
    fn machine_output_is_one_line_and_carries_no_body() {
        let out = machine(
            &res(long_json(), 4300),
            Some(std::path::Path::new("/tmp/x.json")),
        );
        assert!(!out.contains('\n'), "{out}");
        assert!(!out.contains("items"), "本文が漏れている: {out}");
        assert!(out.contains("\"status\":200"), "{out}");
    }
}
