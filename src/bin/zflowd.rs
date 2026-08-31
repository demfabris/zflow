use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "zflowd", version, about = "zflow privileged input daemon")]
struct Args {
    #[arg(long, default_value = "/etc/zflow/zflow.toml")]
    config: std::path::PathBuf,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    zflow::daemon::run(args.config)
}
