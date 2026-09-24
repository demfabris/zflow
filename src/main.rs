use clap::Parser;

fn main() -> anyhow::Result<()> {
    zflow::cli::run(zflow::cli::Cli::parse())
}
