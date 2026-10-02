//! Tells computers on the local network where this Mac takes input and
//! what it is called, as the Linux daemon does, so a paired computer finds
//! it after its address changes and others can show it before they trust
//! it. The record carries no Touch: the Mac cannot post contacts.

use std::time::Duration;

use tokio::sync::oneshot;

use crate::{
    core::InputCapability,
    discovery::{Advertisement, Discovery, DiscoveryError},
    hello::local_name,
};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// Advertises one port on its own thread until dropped.
pub struct Advertiser {
    port: u16,
    /// The record's instance, which this Mac's own browser finds too.
    instance: Option<String>,
    stop: Option<oneshot::Sender<()>>,
}

impl Advertiser {
    pub fn start(port: u16) -> Self {
        let (stop, stopped) = oneshot::channel();
        let registered = (|| {
            let mut discovery = Discovery::new()?;
            discovery.register(advertisement(port)?)?;
            Ok::<_, DiscoveryError>(discovery)
        })();
        let discovery = match registered {
            Ok(discovery) => discovery,
            Err(error) => {
                tracing::warn!(%error, "could not advertise this Mac on the local network");
                return Self {
                    port,
                    instance: None,
                    stop: None,
                };
            }
        };
        let instance = Some(discovery.instance_id().to_string());
        let spawned = std::thread::Builder::new()
            .name("zflow-advertise".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                match runtime {
                    Ok(runtime) => runtime.block_on(advertise(port, discovery, stopped)),
                    Err(error) => tracing::warn!(%error, "could not advertise this Mac"),
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "could not advertise this Mac");
        }
        Self {
            port,
            instance,
            stop: Some(stop),
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn instance(&self) -> Option<&str> {
        self.instance.as_deref()
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

fn advertisement(port: u16) -> Result<Advertisement, DiscoveryError> {
    let capabilities = [
        InputCapability::Keyboard,
        InputCapability::Pointer,
        InputCapability::Scroll,
    ];
    Ok(Advertisement::new(port, capabilities)?.with_name(&local_name()))
}

async fn advertise(port: u16, discovery: Discovery, stop: oneshot::Receiver<()>) {
    tracing::info!(port, "advertising this Mac on the local network");
    tokio::select! {
        _ = stop => {}
        error = discovery.next_daemon_error() => match error {
            Ok(error) => tracing::warn!(%error, "mDNS daemon error; this Mac is no longer advertised"),
            Err(error) => tracing::warn!(%error, "mDNS monitoring stopped; this Mac is no longer advertised"),
        },
    }
    match tokio::time::timeout(SHUTDOWN_TIMEOUT, discovery.shutdown()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "mDNS shutdown failed"),
        Err(_) => tracing::warn!("mDNS shutdown timed out"),
    }
}
