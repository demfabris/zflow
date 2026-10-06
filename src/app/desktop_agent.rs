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
    if let Err(error) = super::desktop::refresh_extension() {
        tracing::warn!(%error, "could not refresh the bundled GNOME extension");
    }
    let updated = tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
        let state = Arc::new(Mutex::new(super::gnome::State::default()));
        let connection = super::gnome::connect(state.clone()).await?;
        tokio::spawn(super::updates::remind(connection.clone()));
        let mut timer=tokio::time::interval(Duration::from_secs(2));
        let mut terminate=tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut interrupt=tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        let mut previous=String::new();
        let mut updated=None;
        loop {
            tokio::select! {
                _=terminate.recv() => break,
                _=interrupt.recv() => break,
                _=timer.tick() => {
                    if let Some(path) = std::fs::read_link("/proc/self/exe").ok().and_then(|exe| replaced(&exe)) {
                        tracing::info!("zflow was updated; restarting the desktop agent");
                        updated=Some(path);
                        break;
                    }
                    let status = {
                        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                        if !state.receiver.is_active() { state.receiver.start(&connection); }
                        state.receiver.status()
                    };
                    if status!=previous { tracing::info!(%status,"desktop agent"); previous=status; }
                }
            }
        }
        state.lock().unwrap_or_else(|e| e.into_inner()).receiver.stop();
        anyhow::Ok(updated)
    })?;
    // The service and this agent speak the version they were built from, so
    // after an update the agent runs the new binary in its place.
    match updated {
        Some(path) => {
            use std::os::unix::process::CommandExt;
            Err(std::process::Command::new(path)
                .args(std::env::args_os().skip(1))
                .exec()
                .into())
        }
        None => Ok(()),
    }
}

/// Where the new binary is, once an update replaced the file this agent runs
/// from. Linux names a running program's removed file "<path> (deleted)".
fn replaced(exe: &std::path::Path) -> Option<std::path::PathBuf> {
    let path = std::path::Path::new(exe.to_str()?.strip_suffix(" (deleted)")?);
    path.is_file().then(|| path.to_owned())
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
                super::gnome::start_agent(connection, true).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_update_restarts_the_agent_from_the_new_binary() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("zflow");
        let deleted = |path: &std::path::Path| format!("{} (deleted)", path.display());
        assert_eq!(
            replaced(deleted(&binary).as_ref()),
            None,
            "removed, not replaced"
        );
        std::fs::write(&binary, b"new").unwrap();
        assert_eq!(replaced(&binary), None, "still running from it");
        assert_eq!(replaced(deleted(&binary).as_ref()), Some(binary));
    }
}
