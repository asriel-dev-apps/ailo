//! コマンドライン定義。
//!
//! ヘルプは LLM が読むことを前提に短く保つ。長大なヘルプはそれ自体がコンテキストを食う。

use clap::{Args, Parser, Subcommand};

use crate::output::{Format, DEFAULT_HEAD_LINES};

#[derive(Debug, Parser)]
#[command(
    name = "ailo",
    version,
    about = "AI エージェント向けの HTTP クライアント。本文はファイルに落とし、要約だけを返す。",
    long_about = None,
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// GET を送る
    Get(RequestArgs),
    /// POST を送る
    Post(RequestArgs),
    /// PUT を送る
    Put(RequestArgs),
    /// PATCH を送る
    Patch(RequestArgs),
    /// DELETE を送る
    Delete(RequestArgs),
    /// HEAD を送る
    Head(RequestArgs),
    /// OPTIONS を送る
    Options(RequestArgs),
    /// 保存済みのリクエストを実行する
    Run(RunArgs),
    /// 直前のリクエストに名前を付けて保存する
    Save(SaveArgs),
    /// 保存済みのリクエストを一覧する
    Ls,
    /// 環境を一覧する
    Env,
    /// 秘匿値を出し入れする
    #[command(subcommand)]
    Secret(SecretCommand),
    /// 直近のリクエストを新しい順に並べる
    Log(LogArgs),
    /// ダンプ本体を表示する
    Show(ShowArgs),
}

impl Command {
    /// アドホックなリクエスト系サブコマンドなら (メソッド, 引数) を返す。
    pub fn as_request(&self) -> Option<(&'static str, &RequestArgs)> {
        match self {
            Command::Get(a) => Some(("GET", a)),
            Command::Post(a) => Some(("POST", a)),
            Command::Put(a) => Some(("PUT", a)),
            Command::Patch(a) => Some(("PATCH", a)),
            Command::Delete(a) => Some(("DELETE", a)),
            Command::Head(a) => Some(("HEAD", a)),
            Command::Options(a) => Some(("OPTIONS", a)),
            _ => None,
        }
    }
}

/// アドホックにも保存済み実行にも効く共通の指定。
#[derive(Debug, Args, Clone)]
pub struct CommonArgs {
    /// 使う環境 (省略時は config の default_env)
    #[arg(long, short)]
    pub env: Option<String>,

    /// 変数を上書きする (名前=値、繰り返し可)
    #[arg(long = "var", value_name = "名前=値")]
    pub vars: Vec<String>,

    /// 出力形式
    #[arg(long, value_enum, default_value = "auto")]
    pub format: Format,

    /// 指定した式の値だけを出す (例: '.data.items[].id')
    #[arg(long, conflicts_with = "shape")]
    pub pick: Option<String>,

    /// 値ではなく JSON の形だけを出す
    #[arg(long)]
    pub shape: bool,

    /// 本文の表示行数
    #[arg(long, default_value_t = DEFAULT_HEAD_LINES)]
    pub head: usize,

    /// 本文を全文表示する
    #[arg(long, conflicts_with = "head")]
    pub full: bool,

    /// 認証情報をマスクせずにダンプする
    #[arg(long)]
    pub no_redact: bool,

    /// タイムアウト秒数
    #[arg(long, default_value_t = 30)]
    pub timeout: u64,

    /// ダンプを書かない
    #[arg(long)]
    pub no_dump: bool,

    /// HTTP ステータスが 400 以上なら終了コードを 1 にする
    #[arg(long)]
    pub fail: bool,
}

impl CommonArgs {
    pub fn head_lines(&self) -> usize {
        if self.full {
            usize::MAX
        } else {
            self.head
        }
    }
}

#[derive(Debug, Args)]
pub struct RequestArgs {
    /// リクエスト先の URL
    pub url: String,

    /// key=値 / key:=JSON / key==クエリ / Name: 値 / key@パス
    ///
    /// `trailing_var_arg` は使わない。使うと item より後ろのフラグが item として
    /// 飲み込まれ、`ailo post <url> name=x --pick .id` が黙って壊れる。
    /// item は `-` で始まらないので、フラグとの取り違えは起きない。
    pub items: Vec<String>,

    /// ボディを application/x-www-form-urlencoded で送る
    #[arg(long)]
    pub form: bool,

    /// ボディを文字列として直接指定する
    #[arg(long)]
    pub raw: Option<String>,

    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// 保存済みリクエストの名前
    pub name: String,

    /// 追加の item (保存内容に上書きで足す)
    pub items: Vec<String>,

    #[command(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Args)]
pub struct SaveArgs {
    /// 付ける名前
    pub name: String,

    /// レスポンスから変数を取り出す指定 (名前=式、繰り返し可)
    #[arg(long = "capture", value_name = "名前=式")]
    pub captures: Vec<String>,

    /// capture のうちキーチェーンへ入れるもの (繰り返し可)
    #[arg(long = "secret", value_name = "名前")]
    pub secrets: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum SecretCommand {
    /// 値を保存する (値は標準入力から読む)
    Set {
        /// 環境名
        env: String,
        /// キー名
        key: String,
    },
    /// キー名を一覧する (値は表示しない)
    Ls {
        /// 環境名 (省略時は全環境)
        env: Option<String>,
    },
    /// 削除する
    Rm {
        /// 環境名
        env: String,
        /// キー名
        key: String,
    },
}

#[derive(Debug, Args)]
pub struct LogArgs {
    /// 表示する件数
    #[arg(long, short, default_value_t = 20)]
    pub limit: usize,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// 新しいものから数えた番号 (1 が直近)、またはダンプのファイル名
    pub target: String,
}
