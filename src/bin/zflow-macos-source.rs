#[cfg(target_os = "macos")]
use std::{net::SocketAddr, path::PathBuf};

#[cfg(target_os = "macos")]
use clap::Parser;

#[cfg(target_os = "macos")]
#[derive(Debug, Parser)]
#[command(
    name = "zflow-macos-source",
    version,
    about = "Foreground macOS input source"
)]
struct Args {
    /// zflow configuration containing this Mac's identity and paired peer.
    #[arg(long)]
    config: PathBuf,

    /// Paired peer name from the configuration.
    #[arg(long, default_value = "ubuntu")]
    peer: String,

    /// Override the peer's stored input address.
    #[arg(long)]
    address: Option<SocketAddr>,

    /// Disable experimental raw Magic Trackpad forwarding.
    #[arg(long)]
    no_touch: bool,
}

#[cfg(target_os = "macos")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    zflow::macos::run(zflow::macos::SourceOptions {
        config_path: args.config,
        peer: args.peer,
        address: args.address,
        raw_touch: !args.no_touch,
    })
    .await
}

#[cfg(not(target_os = "macos"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("zflow-macos-source is supported only on macOS")
}
