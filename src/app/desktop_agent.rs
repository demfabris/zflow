//! Login-session GNOME integration without a desktop window.
use super::displays::{DesktopDetector, DisplayDiscovery};
use anyhow::{Result, ensure};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

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
        let state = Arc::new(Mutex::new(super::gnome::State::default()));
        let _connection = super::gnome::connect(state.clone()).await?;
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
                    let status = {
                        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                        if !state.receiver.is_active() { state.receiver.start(); }
                        state.receiver.status()
                    };
                    if status!=previous { tracing::info!(%status,"desktop agent"); previous=status; }
                    detector.refresh();
                    match crate::peer_view::fetch().await {
                        Ok(snapshot) => {
                            discovery.update(detector.snapshot().0,snapshot.discovery);
                            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                            if snapshot.discovery { state.nearby.start(); } else { state.nearby.stop(); }
                        },
                        Err(error) => {
                            discovery.stop();
                            state.lock().unwrap_or_else(|e| e.into_inner()).nearby.stop();
                            tracing::debug!(%error,"desktop service unavailable");
                        },
                    }
                    if let Some(error)=discovery.error() { tracing::warn!(%error,"desktop discovery unavailable"); }
                }
            }
        }
        state.lock().unwrap_or_else(|e| e.into_inner()).receiver.stop();
        Ok(())
    })
}

fn install_agent() -> Result<()> {
    ensure!(
        !nix::unistd::Uid::effective().is_root(),
        "Run desktop-agent --install as your desktop user, without sudo"
    );
    super::gnome::install()?;
    super::desktop::install_extension()?;
    println!("GNOME integration installed. Open zflow from Applications or run zflow settings.");
    Ok(())
}
