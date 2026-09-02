#[cfg(target_os = "linux")]
use clap::Parser;

#[cfg(target_os = "linux")]
#[derive(Debug, Parser)]
#[command(name = "zflowd", version, about = "zflow privileged input daemon")]
struct Args {
    #[arg(long, default_value = "/etc/zflow/zflow.toml")]
    config: std::path::PathBuf,
}

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    zflow::daemon::run(args.config)
}

#[cfg(not(target_os = "linux"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("zflowd is currently supported only on Linux")
}
