use clap::Parser;

use ailo::cli::Cli;
use ailo::run;

// 単一スレッドのランタイムで足りる。起動時間を削るため、ワーカースレッドは立てない。
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    // 置き場所より先に決める。1 回のプロセスの途中で変わると、読んだ場所と
    // 書いた場所が食い違う。
    if let Err(err) = ailo::workspace::init(cli.workspace.as_deref()) {
        eprintln!("エラー: {err}");
        for cause in err.chain().skip(1) {
            eprintln!("  原因: {cause}");
        }
        std::process::exit(2);
    }
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
