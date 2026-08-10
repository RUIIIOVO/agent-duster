//! duster CLI 入口。命令结构为「名词 + 动词」，外壳零业务逻辑。

use clap::{Parser, Subcommand};

mod output;

#[derive(Parser)]
#[command(name = "duster", version, about = "AI Agent 的资源管理器")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 发现 agent 并建立索引（默认增量）
    Scan {
        /// 全量重扫
        #[arg(long)]
        full: bool,
    },
    /// 总览：agent / 资源 / 体积 / 问题数
    Status,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan { full } => {
            anyhow::bail!("scan (full={full}) 尚未实现 — 见里程碑 M0");
        }
        Command::Status => {
            anyhow::bail!("status 尚未实现 — 见里程碑 M0");
        }
    }
}
