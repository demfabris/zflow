use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
#[command(version, about = "zflow configuration window (does not capture input)")]
struct Args {
    /// Configuration file to edit. No file is written until you choose Save.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Start with the dark theme.
    #[arg(long)]
    dark: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
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
