use crate::windows::{self, engine, ipc};
use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "zflow", version, about = "zflow Windows input sharing")]
pub struct Cli {
    #[arg(long,global=true,default_value_os_t=windows::config_path())]
    config: PathBuf,
    #[command(subcommand)]
    command: Commands,
}
#[derive(Subcommand)]
enum Commands {
    /// Run the engine in this interactive Windows session.
    Run,
    /// Create per-user configuration and identity.
    Setup,
    /// Show running engine state as JSON.
    Status,
    /// Check the native desktop, identity and network addresses.
    Doctor,
    /// Discover a computer by IP, Tailscale name, or host:port.
    Nearby { address: String },
    /// Trust a discovered computer by its full fingerprint or displayed mark.
    Trust { key: String },
    /// Return all input to Windows.
    Local,
    /// Begin controlling a paired computer.
    Switch { peer: String },
    /// Send one JSON control request (also used by automated checks).
    Request { json: String },
    /// Stop the engine and release input.
    Stop,
}
pub fn run(cli: Cli) -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "zflow=info".into()),
        )
        .try_init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let value=runtime.block_on(async {
        let command=match cli.command {
            Commands::Run=>{engine::run(cli.config).await?;return Ok(serde_json::json!({"stopped":true}));},
            Commands::Setup=>{let config=engine::setup(&cli.config)?;let identity=crate::identity::Identity::load_or_create(&config.daemon.state_dir)?;
                return Ok(serde_json::json!({"config":cli.config,"name":crate::hello::local_name(),"fingerprint":identity.fingerprint_hex(),"mark":crate::neighbors::mark(identity.spki())}));},
            Commands::Doctor=>return Ok(serde_json::json!({"desktop_available":windows::input::available(),"geometry":windows::input::geometry()?,"addresses":crate::discovery::this_host_addresses(),"config":cli.config,"pipe":ipc::pipe_name(&cli.config)?})),
            Commands::Status=>ipc::Command::Status,
            Commands::Nearby{address}=>ipc::Command::Nearby{address},
            Commands::Trust{key}=>ipc::Command::Trust{key},
            Commands::Local=>ipc::Command::Local,
            Commands::Switch{peer}=>ipc::Command::Activate{peer},
            Commands::Request{json}=>serde_json::from_str(&json)?,
            Commands::Stop=>ipc::Command::Quit,
        };
        ipc::request(&cli.config,command).await
    })?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
