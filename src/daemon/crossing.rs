//! Crossings this computer starts when its pointer pushes against an edge:
//! prepare the other computer's desktop, send input until the pointer comes
//! back through that computer's edge, then put the pointer back here.

use super::*;
use crate::app::handoff::{self, Handoff};
use crate::desktop::{DesktopRequest, DesktopResponse, Edge};
use anyhow::ensure;

/// How long activation may take to start arming before the crossing gives up.
const ARMING_START: Duration = Duration::from_millis(500);
/// Receivers from before the poll hold answer at once; this paces their polls.
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(50);

impl Shared {
    /// Runs one crossing toward the computer behind `edge`. Only one runs at
    /// a time; a push during a crossing is ignored.
    pub(super) async fn cross(self: &Arc<Self>, edge: Edge, position: u32) -> Result<()> {
        let Ok(_crossing) = self.crossing.try_lock() else {
            return Ok(());
        };
        self.require_not_controlled().await?;
        ensure!(
            self.runtime.status().ownership == OwnershipPhase::Idle,
            "local input ownership is not idle"
        );
        let geometry = match self.desktop.snapshot().await {
            DesktopResponse::Snapshot { geometry, .. } => geometry,
            DesktopResponse::Unavailable { reason } => bail!("{reason}"),
            _ => bail!("GNOME did not describe this desktop"),
        };
        let layout = self
            .local_layout()
            .await
            .context("no layout places another computer here")?;
        let handoff = handoff::from_edge(&layout, &geometry, edge, position)
            .context("the layout has no computer at that point of the edge")?;
        let record = self
            .config
            .read()
            .await
            .peers
            .get(&handoff.peer)
            .cloned()
            .with_context(|| format!("unknown peer {}", handoff.peer))?;
        require_outbound_permission(&*self.config.read().await, &handoff.peer, &record)?;
        let session = self.ensure_session(&handoff.peer, &record).await?;
        let token = handoff::token()?;
        let prepared = session.desktop_request(handoff.prepare(token)).await;
        let result = async {
            handoff.check_prepared(prepared?)?;
            self.activate(&handoff.peer).await?;
            self.until_returned(&session, &handoff, token).await
        }
        .await;
        let finished = handoff::check_finished(
            session
                .desktop_request(DesktopRequest::Finish { token })
                .await,
        );
        // A failure on the way in matters more than one while cleaning up.
        result.and(finished)
    }

    /// Keeps the other computer's handoff alive while input goes there. When
    /// the pointer comes back through its edge, puts the pointer at the
    /// matching point here and gives input back.
    async fn until_returned(
        &self,
        session: &SessionHandle,
        handoff: &Handoff,
        token: u64,
    ) -> Result<()> {
        let started = Instant::now();
        let mut armed = false;
        loop {
            match self.runtime.status().ownership {
                OwnershipPhase::Idle if armed => return Ok(()),
                OwnershipPhase::Idle => ensure!(
                    started.elapsed() < ARMING_START,
                    "the crossing did not start arming; check `zflow doctor`"
                ),
                _ => armed = true,
            }
            let polled = Instant::now();
            match session
                .desktop_request(DesktopRequest::Poll { token })
                .await?
            {
                DesktopResponse::Active => {
                    tokio::time::sleep_until((polled + MIN_POLL_INTERVAL).into()).await;
                }
                DesktopResponse::Returned { position } => {
                    let point = handoff.return_mapping.position(position)?;
                    if let DesktopResponse::Unavailable { reason } = self.desktop.warp(point).await
                    {
                        tracing::warn!(%reason, "pointer not put back after a crossing");
                    }
                    self.runtime
                        .send(RuntimeCommand::Release {
                            transport_live: true,
                        })
                        .map_err(|error| anyhow!(error))?;
                    return Ok(());
                }
                DesktopResponse::Unavailable { reason } => {
                    bail!("the other computer ended the crossing: {reason}")
                }
                _ => bail!("the other computer answered a poll unexpectedly"),
            }
        }
    }

    /// This computer's view of the shared layout, if it has one.
    pub(super) async fn local_layout(&self) -> Option<crate::app::layout_model::Layout> {
        let layout = self.layout.lock().await.clone()?;
        let keys = peer_keys(&*self.config.read().await);
        Some(crate::app::layout_model::Layout::from_shared(
            &layout,
            &self.identity_fingerprint,
            "This computer",
            &keys,
        ))
    }
}
