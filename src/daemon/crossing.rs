//! Crossings this computer starts when its pointer pushes against an edge:
//! prepare the other computer's desktop, send input until the pointer leaves
//! it again, then put the pointer back here. A pointer that leaves toward a
//! third computer moves on to it, with this computer routing.

use super::*;
use crate::app::handoff::{self, Handoff, Next};
use crate::desktop::{DesktopRequest, DesktopResponse, Edge};
use anyhow::ensure;

/// How long activation may take to start arming before the crossing gives up.
const ARMING_START: Duration = Duration::from_millis(500);
/// Arming waits for every key and button to be up. A push made while one is
/// held, such as dragging a window to the edge, is not a crossing, so give up
/// rather than cross when it is dropped later somewhere else.
const ARMING_LIMIT: Duration = Duration::from_millis(400);
/// Receivers from before the poll hold answer at once; this paces their polls.
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// The computer a crossing's input goes to now: its session, its handoff,
/// and the token that holds its desktop.
struct Leg {
    session: SessionHandle,
    handoff: Handoff,
    token: u64,
}

impl Shared {
    /// Runs one crossing toward the computer behind `edge`. Only one runs at
    /// a time; a push during a crossing is ignored.
    pub(super) async fn cross(
        self: &Arc<Self>,
        monitor: Option<&str>,
        edge: Edge,
        position: u32,
    ) -> Result<()> {
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
        self.fit_own_tile(&geometry).await;
        let layout = self
            .local_layout()
            .await
            .context("no layout places another computer here")?;
        let mut handoff = handoff::from_edge(&layout, &geometry, monitor, edge, position)
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
        self.keep_reachable(&mut handoff).await;
        let token = handoff::token()?;
        let prepared = session.desktop_request(handoff.prepare(token)).await;
        let mut leg = Leg {
            session,
            handoff,
            token,
        };
        let entered = async {
            leg.handoff.check_prepared(prepared?)?;
            self.activate(&leg.handoff.peer).await
        }
        .await;
        let result = match entered {
            Ok(()) => {
                let result = self.until_returned(&mut leg).await;
                // However it failed, input must not stay armed or grabbed for
                // a crossing that is over. Finish below ends the other side.
                if result.is_err() {
                    self.release_own_input().await;
                }
                result
            }
            Err(error) => Err(error),
        };
        let finished = handoff::check_finished(
            leg.session
                .desktop_request(DesktopRequest::Finish { token: leg.token })
                .await,
        );
        // A failure on the way in matters more than one while cleaning up.
        result.and(finished)
    }

    /// Keeps the handoff's routes on to computers this one may send to and
    /// has a session with now. A hop never waits for a dial, which could
    /// outlast the lease of the computer the pointer is on.
    async fn keep_reachable(&self, handoff: &mut Handoff) {
        let config = self.config.read().await;
        let sessions = self.sessions.lock().await;
        handoff.keep_routes(|peer| {
            config
                .peers
                .get(peer)
                .is_some_and(|record| require_outbound_permission(&config, peer, record).is_ok())
                && sessions
                    .get(peer)
                    .is_some_and(|session| !session.is_closed())
        });
    }

    /// Keeps the handoff of the computer input goes to alive. When the
    /// pointer leaves it toward home, puts the pointer at the matching point
    /// here and gives input back; toward a third computer with nothing
    /// held, moves input on to that one and keeps going.
    async fn until_returned(self: &Arc<Self>, leg: &mut Leg) -> Result<()> {
        let started = Instant::now();
        let mut armed = false;
        loop {
            match self.runtime.status().ownership {
                OwnershipPhase::Idle if armed => return Ok(()),
                OwnershipPhase::Idle => ensure!(
                    started.elapsed() < ARMING_START,
                    "the crossing did not start arming; check `zflow doctor`"
                ),
                OwnershipPhase::Arming => {
                    armed = true;
                    ensure!(
                        started.elapsed() < ARMING_LIMIT,
                        "a key or button stayed down, so the pointer stays here"
                    );
                }
                _ => armed = true,
            }
            let polled = Instant::now();
            match leg
                .session
                .desktop_request(DesktopRequest::Poll { token: leg.token })
                .await?
            {
                DesktopResponse::Active => {
                    tokio::time::sleep_until((polled + MIN_POLL_INTERVAL).into()).await;
                }
                DesktopResponse::Exited { exit, position } => {
                    match leg.handoff.next(exit, position)? {
                        Next::Home(point) => {
                            if let DesktopResponse::Unavailable { reason } =
                                self.desktop.warp(point).await
                            {
                                tracing::warn!(%reason, "pointer not put back after a crossing");
                            }
                            self.release_own_input().await;
                            return Ok(());
                        }
                        // Polling on tells that computer the pointer stays.
                        Next::Hop(next) if !self.runtime.status().neutral => {
                            tracing::debug!(peer = %next.peer, "a key or button is held; the pointer stays");
                        }
                        Next::Hop(next) => {
                            let peer = next.peer.clone();
                            match self.hop(leg, *next).await {
                                Ok(left) => {
                                    tracing::info!(from = %left.handoff.peer, to = %peer, "input moved on");
                                    let finished = handoff::check_finished(
                                        left.session
                                            .desktop_request(DesktopRequest::Finish {
                                                token: left.token,
                                            })
                                            .await,
                                    );
                                    if let Err(error) = finished {
                                        tracing::warn!(error = %format_args!("{error:#}"), peer = %left.handoff.peer, "desktop not let go after a hop");
                                    }
                                }
                                Err(error) => {
                                    tracing::info!(error = %format_args!("{error:#}"), %peer, "input stays where it is");
                                }
                            }
                        }
                    }
                }
                DesktopResponse::Unavailable { reason } => {
                    bail!("the other computer ended the crossing: {reason}")
                }
                _ => bail!("the other computer answered a poll unexpectedly"),
            }
        }
    }

    /// Moves input on to the computer `next` names, for a pointer that left
    /// the current one toward it: prepares that computer's desktop at the
    /// matching entry, then sends input there. Returns the leg it left,
    /// whose desktop the caller lets go of. On failure input stays.
    async fn hop(self: &Arc<Self>, leg: &mut Leg, mut next: Handoff) -> Result<Leg> {
        let session = self
            .sessions
            .lock()
            .await
            .get(&next.peer)
            .filter(|session| !session.is_closed())
            .cloned()
            .with_context(|| format!("{} is not connected", next.peer))?;
        self.keep_reachable(&mut next).await;
        let token = handoff::token()?;
        let prepared = session.desktop_request(next.prepare(token)).await;
        let switched = async {
            next.check_prepared(prepared?)?;
            self.switch_outbound(&next.peer).await
        }
        .await;
        if let Err(error) = switched {
            // Whatever Prepare took on that computer goes back.
            let _ = session
                .desktop_request(DesktopRequest::Finish { token })
                .await;
            return Err(error);
        }
        // The pointer left for the next computer, so the clipboard goes along.
        self.share_clipboard(&next.peer);
        Ok(std::mem::replace(
            leg,
            Leg {
                session,
                handoff: next,
                token,
            },
        ))
    }

    /// Gives this computer's input back to it. Releasing while idle does
    /// nothing, so this is safe whatever state the crossing reached.
    async fn release_own_input(&self) {
        let released = self
            .runtime
            .send_critical(
                RuntimeCommand::Release {
                    transport_live: true,
                },
                TERMINAL_SEND_TIMEOUT,
            )
            .await;
        if let Err(error) = released {
            tracing::warn!(%error, "input release after a crossing not delivered");
        }
    }

    /// This computer's view of the shared layout, if it has one.
    pub(super) async fn local_layout(&self) -> Option<crate::app::layout_model::Layout> {
        let layout = self.layout.lock().await.clone()?;
        let keys = peer_keys(&*self.config.read().await);
        Some(layout_view(&layout, &self.identity_fingerprint, &keys))
    }
}
