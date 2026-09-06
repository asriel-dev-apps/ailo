//! workspace — リクエストと設定を、プロジェクト単位で分ける。
//!
//! `requests.toml` が 1 ファイルだと、複数プロジェクトのリクエストが同じ場所に混ざる。
//! そこで **`~/.config/ailo/workspaces/<名前>/` に丸ごと分ける**。
//!
//! **カレントディレクトリで自動的に切り替わる。明示指定があればそちらが勝つ**
//! （`--env` と同じ関係）。明示切替だけにしないのは、kubectl の context や AWS の
//! profile で「切替忘れによる誤操作」が定番の事故になっている実績があるため。
//! 主利用者のエージェントはリポジトリの中で動くので、ディレクトリ由来と相性が良い。
//!
//! 対応はリポジトリ直下の `.ailo` に持つ。中身は workspace 名 1 行だけで、
//! **定義本体も秘匿値もここには入らない**ので、git に入っても安全。
//! 親ディレクトリを遡って探すため、サブディレクトリで叩いても効く。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};

/// ディレクトリに置く目印のファイル名。
pub const MARKER: &str = ".ailo";

/// 明示指定に使う環境変数。フラグの次に強い。
pub const ENV_VAR: &str = "AILO_WORKSPACE";

/// いまどの workspace を見ているか。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Workspace {
    /// 名前の付いていない、これまでどおりの置き場所。
    ///
    /// `.ailo` が見つからないディレクトリでは今までどおり動く。アドホックに叩く人は
    /// workspace を知らずに済み、移行作業も起きない。
    Default,
    Named(String),
}

impl Workspace {
    pub fn name(&self) -> Option<&str> {
        match self {
            Workspace::Default => None,
            Workspace::Named(n) => Some(n),
        }
    }

    /// 表示用。既定は名前が無いので `(既定)`。
    pub fn label(&self) -> &str {
        self.name().unwrap_or("(既定)")
    }
}

static RESOLVED: OnceLock<Workspace> = OnceLock::new();

/// 起動時に 1 度だけ決める。
///
/// 優先順位は **`--workspace` > `AILO_WORKSPACE` > `.ailo` > 既定**。
/// 決め直せないようにしてあるのは、1 回のプロセスの途中で置き場所が変わると、
/// 読んだ場所と書いた場所が食い違うため。
pub fn init(explicit: Option<&str>) -> Result<&'static Workspace> {
    let resolved = resolve(explicit, &std::env::current_dir()?)?;
    Ok(RESOLVED.get_or_init(|| resolved))
}

/// 決まっているものを返す。`init` を呼んでいなければ既定。
pub fn current() -> &'static Workspace {
    RESOLVED.get_or_init(|| Workspace::Default)
}

fn resolve(explicit: Option<&str>, from: &Path) -> Result<Workspace> {
    if let Some(name) = explicit {
        return named(name, "--workspace");
    }
    if let Some(name) = std::env::var(ENV_VAR).ok().filter(|v| !v.trim().is_empty()) {
        return named(&name, ENV_VAR);
    }
    match find_marker(from) {
        Some((path, name)) => named(&name, &path.display().to_string()),
        None => Ok(Workspace::Default),
    }
}

fn named(raw: &str, source: &str) -> Result<Workspace> {
    let name = raw.trim();
    validate(name).with_context(|| format!("{source} が指す workspace 名が使えません"))?;
    Ok(Workspace::Named(name.to_string()))
}

/// workspace 名として使える文字。
///
/// ディレクトリ名とキーチェーンの account 名の一部になる。素通しにすると
/// `../` で置き場所の外へ出られる。`.` を許さないのは、キーチェーンの名前空間を
/// `/` で切っている以上、見分けの付かない名前を増やしたくないため。
pub fn validate(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if ok {
        Ok(())
    } else {
        bail!("workspace 名 `{name}` は使えません。英数字と `-` `_` だけで指定してください")
    }
}

/// `.ailo` を上へ辿って探す。git と同じ流儀。
fn find_marker(from: &Path) -> Option<(PathBuf, String)> {
    for dir in from.ancestors() {
        let candidate = dir.join(MARKER);
        let Ok(text) = std::fs::read_to_string(&candidate) else {
            continue;
        };
        // 1 行目だけを見る。将来行を足しても古い ailo が壊れない。
        let name = text.lines().next().unwrap_or("").trim().to_string();
        if name.is_empty() {
            continue;
        }
        return Some((candidate, name));
    }
    None
}

/// この workspace の置き場所を、与えられた土台の下に作る。
///
/// 既定は土台そのもの。名前付きは `workspaces/<名前>/` の下。
pub fn scoped(base: PathBuf, ws: &Workspace) -> PathBuf {
    match ws {
        Workspace::Default => base,
        Workspace::Named(name) => base.join("workspaces").join(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_may_not_escape_its_directory() {
        assert!(validate("..").is_err());
        assert!(validate("../other").is_err());
        assert!(validate("a/b").is_err());
        // `.` は名前空間の区切りと紛らわしいので許さない。
        assert!(validate("a.b").is_err());
        assert!(validate("").is_err());
        assert!(validate("my-project_2").is_ok());
    }

    #[test]
    fn the_default_workspace_keeps_the_old_places() {
        let base = PathBuf::from("/x/.config/ailo");
        assert_eq!(scoped(base.clone(), &Workspace::Default), base);
    }

    #[test]
    fn a_named_workspace_lives_under_workspaces() {
        assert_eq!(
            scoped(
                PathBuf::from("/x/.config/ailo"),
                &Workspace::Named("proj".into())
            ),
            PathBuf::from("/x/.config/ailo/workspaces/proj")
        );
    }

    #[test]
    fn the_marker_is_found_from_a_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(MARKER), "my-project\n").unwrap();
        let deep = tmp.path().join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();

        let (_, name) = find_marker(&deep).expect("見つからない");
        assert_eq!(name, "my-project");
    }

    #[test]
    fn only_the_first_line_of_the_marker_is_used() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(MARKER), "proj\n# あとから足した行\n").unwrap();
        assert_eq!(find_marker(tmp.path()).unwrap().1, "proj");
    }

    #[test]
    fn an_empty_marker_is_ignored_rather_than_treated_as_a_name() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(MARKER), "\n").unwrap();
        assert!(find_marker(tmp.path()).is_none());
    }

    #[test]
    fn an_explicit_name_wins_over_the_marker() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(MARKER), "from-marker\n").unwrap();
        assert_eq!(
            resolve(Some("from-flag"), tmp.path()).unwrap(),
            Workspace::Named("from-flag".into())
        );
    }

    #[test]
    fn a_bad_name_in_the_marker_names_the_file_in_the_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(MARKER), "../escape\n").unwrap();
        let err = resolve(None, tmp.path()).unwrap_err().to_string();
        assert!(
            err.contains(MARKER),
            "どのファイルが悪いか分からない: {err}"
        );
    }
}
