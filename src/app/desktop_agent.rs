//! Login-session GNOME integration without a desktop window.
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
        let connection = super::gnome::connect(state.clone()).await?;
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
                        if !state.receiver.is_active() { state.receiver.start(&connection); }
                        state.receiver.status()
                    };
                    if status!=previous { tracing::info!(%status,"desktop agent"); previous=status; }
                    match crate::peer_view::status().await {
                        Ok(snapshot) => {
                            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                            if snapshot.discovery { state.nearby.start(); } else { state.nearby.stop(); }
                        },
                        Err(error) => {
                            state.lock().unwrap_or_else(|e| e.into_inner()).nearby.stop();
                            tracing::debug!(%error,"desktop service unavailable");
                        },
                    }
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
    println!(
        "Adding the zflow GNOME extension. GNOME may ask to download it from extensions.gnome.org."
    );
    let running = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let session = zbus::Connection::session().await.ok();
            let running = super::desktop::install_extension(session.as_ref()).await?;
            if let Some(connection) = &session {
                super::gnome::start_agent(connection).await?;
            }
            Ok::<_, anyhow::Error>(running)
        })?;
    if running {
        println!("GNOME integration is ready. Open zflow from Applications.");
    } else {
        println!("Log out and back in to finish setting up zflow in GNOME.");
    }
    Ok(())
}
