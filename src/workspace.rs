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
/// `init` を呼ぶ前に `current()` が呼ばれたときに返すもの。
///
/// **ここで `OnceLock` を埋めない。** 埋めてしまうと、あとから `init(Some("foo"))` を
/// 呼んでも黙って捨てられ、明示指定が効かないのに何も言わない状態になる。
const FALLBACK: Workspace = Workspace::Default;

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
    RESOLVED.get().unwrap_or(&FALLBACK)
}

fn resolve(explicit: Option<&str>, from: &Path) -> Result<Workspace> {
    if let Some(name) = explicit {
        return named(name, "--workspace");
    }
    if let Some(name) = std::env::var(ENV_VAR).ok().filter(|v| !v.trim().is_empty()) {
        return named(&name, ENV_VAR);
    }
    match find_marker(from)? {
        Some((path, name)) => named(&name, &path.display().to_string()),
        None => Ok(Workspace::Default),
    }
}

fn named(raw: &str, source: &str) -> Result<Workspace> {
    let name = raw.trim();
    validate(name).with_context(|| format!("{source} が指す workspace 名が使えません"))?;
    Ok(Workspace::Named(name.to_string()))
}

/// workspace 名として使える文字。**英数字と `-` だけ。**
///
/// ディレクトリ名とキーチェーンの account 名、そして環境変数名の一部になる。
/// 素通しにすると `../` で置き場所の外へ出られる。
///
/// **`_` と `.` を許さず、`-` を続けさせないのは、環境変数名で潰れないようにするため。**
/// 環境変数は英数字と `_` しか持てないので `-` は `_` に寄せることになる。
/// `_` も許すと `foo-bar` と `foo_bar` が同じ `FOO_BAR` になり、
/// **別の workspace の秘匿値を読めてしまう**。`--` を許さないのも、
/// 区切りに使う `__` と見分けが付かなくなるため。
pub fn validate(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if ok {
        Ok(())
    } else {
        bail!(
            "workspace 名 `{name}` は使えません。英数字と `-` だけで指定してください\n             (`_` と `.` を許さないのは、環境変数名で別の workspace と潰れないようにするためです)"
        )
    }
}

/// `.ailo` の 1 行目だけを読む上限。
///
/// 中身は名前 1 行なので、これで足りる。上限を置かないのは、壊れたリンクや
/// FIFO を指していたときに**起動が止まる**か、大きなファイルを丸ごと読むため。
const MARKER_READ_LIMIT: u64 = 4096;

/// `.ailo` を上へ辿って探す。git と同じ流儀。
///
/// **「無い」以外の失敗は握り潰さない。** 権限が無い、ディレクトリになっている、
/// リンクが壊れている——どれも設定の間違いなのに、黙って親や既定へ落ちると
/// **別プロジェクトの設定と認証情報で送ってしまう**。
fn find_marker(from: &Path) -> Result<Option<(PathBuf, String)>> {
    use std::io::Read as _;

    for dir in from.ancestors() {
        let candidate = dir.join(MARKER);
        let file = match std::fs::File::open(&candidate) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(e).with_context(|| format!("{} を読めません", candidate.display()))
            }
        };

        let mut text = String::new();
        file.take(MARKER_READ_LIMIT)
            .read_to_string(&mut text)
            .with_context(|| format!("{} を読めません", candidate.display()))?;

        // 1 行目だけを見る。将来行を足しても古い ailo が壊れない。
        let name = text.lines().next().unwrap_or("").trim().to_string();
        if name.is_empty() {
            bail!(
                "{} が空です。workspace 名を 1 行だけ書くか、ファイルごと消してください",
                candidate.display()
            );
        }
        return Ok(Some((candidate, name)));
    }
    Ok(None)
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
        assert!(validate("my-project-2").is_ok());
    }

    #[test]
    fn names_that_would_collide_as_environment_variables_are_refused() {
        // `foo-bar` と `foo_bar` はどちらも `FOO_BAR` になり、
        // 別の workspace の秘匿値を読めてしまう。
        assert!(validate("foo-bar").is_ok());
        assert!(validate("foo_bar").is_err());
        // `--` は区切りの `__` と見分けが付かない。
        assert!(validate("foo--bar").is_err());
        assert!(validate("-lead").is_err());
        assert!(validate("trail-").is_err());
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

        let (_, name) = find_marker(&deep).unwrap().expect("見つからない");
        assert_eq!(name, "my-project");
    }

    #[test]
    fn only_the_first_line_of_the_marker_is_used() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(MARKER), "proj\n# あとから足した行\n").unwrap();
        assert_eq!(find_marker(tmp.path()).unwrap().unwrap().1, "proj");
    }

    #[test]
    fn an_empty_marker_is_an_error_rather_than_a_silent_fallback() {
        // 黙って親や既定へ落ちると、別プロジェクトの設定で送ってしまう。
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(MARKER), "\n").unwrap();
        let err = find_marker(tmp.path()).unwrap_err().to_string();
        assert!(err.contains(MARKER), "{err}");
    }

    #[test]
    fn a_marker_that_cannot_be_read_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        // ディレクトリになっている場合。読めないので握り潰さない。
        std::fs::create_dir(tmp.path().join(MARKER)).unwrap();
        assert!(find_marker(tmp.path()).is_err());
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
