//! Login-session GNOME integration without a desktop window.
use super::{
    desktop::DesktopReceiver,
    displays::{DesktopDetector, DisplayDiscovery},
};
use anyhow::{Context, Result, ensure};
use std::{path::PathBuf, time::Duration};

pub fn run(install: bool) -> Result<()> {
    if install {
        return install_agent();
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,zflow=info".into()),
        )
        .try_init();
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
        let mut receiver=DesktopReceiver::default();
        let mut detector=DesktopDetector::default();
        let mut discovery=DisplayDiscovery::default();
        let mut timer=tokio::time::interval(Duration::from_secs(2));
        let mut terminate=tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut interrupt=tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        let mut previous=String::new();
        loop {
            tokio::select! {
                _=terminate.recv() => break,
                _=interrupt.recv() => break,
                _=timer.tick() => {
                    if !receiver.is_active() { receiver.start(); }
                    let status=receiver.status();
                    if status!=previous { tracing::info!(%status,"desktop agent"); previous=status; }
                    detector.refresh();
                    match crate::peer_view::fetch().await {
                        Ok(snapshot) => discovery.update(detector.snapshot().0,snapshot.discovery),
                        Err(error) => { discovery.stop(); tracing::debug!(%error,"desktop service unavailable"); },
                    }
                    if let Some(error)=discovery.error() { tracing::warn!(%error,"desktop discovery unavailable"); }
                }
            }
        }
        receiver.stop();
        Ok(())
    })
}

fn install_agent() -> Result<()> {
    ensure!(
        !nix::unistd::Uid::effective().is_root(),
        "Run desktop-agent --install as your desktop user, without sudo"
    );
    let directory = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("HOME is not set")?
        .join("autostart");
    let executable = std::env::current_exe()?;
    let executable = executable
        .to_str()
        .context("Executable path must be UTF-8")?;
    ensure!(
        !executable.chars().any(char::is_control),
        "Invalid executable path"
    );
    let executable = executable
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('`', "\\`")
        .replace('$', "\\$")
        .replace('%', "%%");
    std::fs::create_dir_all(&directory)?;
    std::fs::write(
        directory.join("io.zflow.desktop-agent.desktop"),
        format!(
            "[Desktop Entry]\nType=Application\nName=zflow desktop agent\nExec=\"{executable}\" desktop-agent\nOnlyShowIn=GNOME;\nTerminal=false\n"
        ),
    )?;
    super::desktop::install_extension()?;
    println!(
        "Desktop agent installed for your next login. Run zflow desktop-agent to start it now."
    );
    Ok(())
}
