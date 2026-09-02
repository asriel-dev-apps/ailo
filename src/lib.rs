//! ailo — AI エージェントを主利用者とする HTTP クライアント。
//!
//! 存在理由はひとつ。**レスポンス全文をコンテキストに入れずに API を叩けること**。
//! そのために本文はファイルへ落とし、標準出力には要約・抽出・形だけを返す。

pub mod args;
pub mod cli;
pub mod dump;
pub mod http;
pub mod output;
pub mod paths;
pub mod pick;
pub mod redact;
pub mod run;
pub mod shape;
