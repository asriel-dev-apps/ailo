//! ailo が使うディレクトリ。
//!
//! macOS でも `~/Library/Application Support` ではなく XDG 規約(`~/.local/share`)に寄せている。
//! エージェントがパスを推測できること、Linux と同じ場所を grep できることを優先したため。
//! `XDG_DATA_HOME` / `XDG_CONFIG_HOME` が設定されていればそちらを尊重する。

use std::path::PathBuf;

use anyhow::{anyhow, Result};

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| anyhow!("HOME が設定されていません"))
}

fn xdg(var: &str, fallback: &str) -> Result<PathBuf> {
    // XDG 仕様上、相対パスの値は「未設定」として扱う。
    if let Some(v) = std::env::var_os(var) {
        let p = PathBuf::from(v);
        if p.is_absolute() {
            return Ok(p);
        }
    }
    Ok(home()?.join(fallback))
}

/// `~/.local/share/ailo`（名前付き workspace なら `.../workspaces/<名前>`）
pub fn data_dir() -> Result<PathBuf> {
    Ok(crate::workspace::scoped(
        xdg("XDG_DATA_HOME", ".local/share")?.join("ailo"),
        crate::workspace::current(),
    ))
}

/// `~/.config/ailo`（名前付き workspace なら `.../workspaces/<名前>`）
///
/// **workspace は置き場所ごと分ける。** 設定・保存済みリクエスト・秘匿値の索引・
/// ダンプが同じ木の下に揃うので、「いまどの workspace か」を 1 か所で決めれば
/// 残りは自動的に付いてくる。
pub fn config_dir() -> Result<PathBuf> {
    Ok(crate::workspace::scoped(
        xdg("XDG_CONFIG_HOME", ".config")?.join("ailo"),
        crate::workspace::current(),
    ))
}

/// ダンプの置き場所。`AILO_DUMP_DIR` で上書きできる。
///
/// **上書きしても workspace の分けは残す。** 素通しにすると、同じ `AILO_DUMP_DIR` を
/// 設定した別 workspace どうしで `log` と `show` が互いのダンプを開き、
/// `--no-redact` で保存した本文にも届いてしまう。
pub fn dumps_dir() -> Result<PathBuf> {
    if let Some(v) = std::env::var_os("AILO_DUMP_DIR") {
        let p = PathBuf::from(v);
        if !p.as_os_str().is_empty() {
            return Ok(crate::workspace::scoped(p, crate::workspace::current()));
        }
    }
    Ok(data_dir()?.join("dumps"))
}

/// ダンプ索引。1 リクエスト 1 行の JSONL。
pub fn index_path() -> Result<PathBuf> {
    Ok(dumps_dir()?.join("index.jsonl"))
}

/// ホームディレクトリ配下のパスを `~/...` に畳む。出力にフルパスを出さないため。
pub fn tildify(path: &std::path::Path) -> String {
    if let Ok(h) = home() {
        if let Ok(rest) = path.strip_prefix(&h) {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tildify_folds_home_and_leaves_others_alone() {
        let h = PathBuf::from(std::env::var("HOME").unwrap());
        assert_eq!(tildify(&h.join("a/b")), "~/a/b");
        assert_eq!(tildify(std::path::Path::new("/etc/hosts")), "/etc/hosts");
    }
}
