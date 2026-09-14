use std::{io::IsTerminal, path::PathBuf};

use clap::Parser;

#[derive(Parser)]
#[command(version, about = "zflow setup and input sharing")]
struct Args {
    /// Configuration file to edit. No file is written until you choose Save.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Start with the dark theme.
    #[arg(long)]
    dark: bool,

    /// Log crossing stages and timings. RUST_LOG can override the filter.
    #[arg(long)]
    debug: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let default_filter = if args.debug {
        "warn,zflow=debug,zflow_gui=debug"
    } else {
        "warn,zflow=info,zflow_gui=info"
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default_filter.into()),
        )
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr)
        .init();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        pid = std::process::id(),
        debug = args.debug,
        "zflow GUI started"
    );
    let mut app = match args.config {
        Some(path) => zflow::gui::SettingsApp::open(path, args.dark),
        None => zflow::gui::SettingsApp::open_default(args.dark)?,
    };
    eframe::run_native(
        "zflow",
        eframe::NativeOptions {
            renderer: eframe::Renderer::Glow,
            viewport: eguicn::egui::ViewportBuilder::default()
                .with_inner_size([1080.0, 800.0])
                .with_min_inner_size([760.0, 600.0]),
            ..Default::default()
        },
        Box::new(move |cc| {
            app.install_theme(&cc.egui_ctx);
            app.enable_discovery(cc.egui_ctx.clone());
            Ok(Box::new(app))
        }),
    )
    .map_err(|error| anyhow::anyhow!("Could not open the settings window: {error}"))
}
