use clap::Parser;

use ailo::cli::Cli;
use ailo::run;

// 単一スレッドのランタイムで足りる。起動時間を削るため、ワーカースレッドは立てない。
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    match run::run(cli.command).await {
        Ok(outcome) => std::process::exit(outcome.code),
        Err(err) => {
            // 原因の連鎖をすべて出す。エージェントが次にやることを決められるように。
            eprintln!("エラー: {err}");
            for cause in err.chain().skip(1) {
                eprintln!("  原因: {cause}");
            }
            std::process::exit(2);
        }
    }
}
