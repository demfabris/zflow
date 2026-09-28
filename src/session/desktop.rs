//! Desktop handoff requests carried on the input control stream.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::sync::oneshot;

use super::{
    Reporter, SessionCommand, SessionEventKind, SessionHandle, desktop_operation,
    desktop_response_kind,
};
use crate::{
    core::SessionCloseReason,
    desktop::{DesktopMessage, DesktopRequest, DesktopResponse},
    transport::InputChannels,
};

impl SessionHandle {
    /// Sends a clip to the peer in the background, cancelling one still on
    /// its way. Input never waits for it.
    pub fn send_clipboard(&self, clip: crate::clipboard::Clip) {
        let clipboard = self.clipboard.clone();
        let (peer, id) = (self.peer.clone(), self.id);
        let task = tokio::spawn(async move {
            if let Err(error) = clipboard.send(&clip).await {
                tracing::info!(%peer, session_id = id, %error, "clip not sent");
            }
        });
        let previous = self
            .sending_clip
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(task.abort_handle());
        if let Some(previous) = previous {
            previous.abort();
        }
    }

    /// Sends this computer's layout to the peer. Nothing answers it.
    pub fn send_layout(&self, layout: crate::desktop::SharedLayout) -> Result<()> {
        layout.validate()?;
        self.commands
            .try_send(SessionCommand::Layout(layout))
            .map_err(|error| anyhow::anyhow!("layout not queued: {error}"))
    }

    /// One scoped desktop operation. A timeout closes transport so a late warp cannot
    /// leave the source believing that a cancelled handoff completed.
    pub async fn desktop_request(
        &self,
        request: crate::desktop::DesktopRequest,
    ) -> Result<crate::desktop::DesktopResponse> {
        request.validate()?;
        let operation = desktop_operation(&request);
        let started = Instant::now();
        let id = self
            .desktop_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        struct CancelOnDrop {
            handle: SessionHandle,
            completed: bool,
            id: u64,
            operation: &'static str,
            started: Instant,
        }
        impl Drop for CancelOnDrop {
            fn drop(&mut self) {
                if !self.completed {
                    tracing::debug!(peer = %self.handle.peer, session_id = self.handle.id, request_id = self.id, operation = self.operation, elapsed_ms = self.started.elapsed().as_millis() as u64, "desktop request wait canceled; closing transport");
                    self.handle.close(SessionCloseReason::BackendUnavailable);
                }
            }
        }
        let mut guard = CancelOnDrop {
            handle: self.clone(),
            completed: false,
            id,
            operation,
            started,
        };
        anyhow::ensure!(id != 0, "Desktop request ID exhausted");
        let (reply, receiver) = oneshot::channel();
        if let Err(error) = self
            .commands
            .try_send(SessionCommand::Desktop { id, request, reply })
        {
            guard.completed = true;
            self.close(SessionCloseReason::BackendUnavailable);
            tracing::warn!(peer = %self.peer, session_id = self.id, request_id = id, operation, %error, "desktop request queue unavailable");
            bail!("Desktop {operation} request queue unavailable: {error}");
        }
        if operation != "poll" {
            tracing::debug!(peer = %self.peer, session_id = self.id, request_id = id, operation, "desktop request queued");
        } else {
            tracing::trace!(peer = %self.peer, session_id = self.id, request_id = id, operation, "desktop request queued");
        }
        match tokio::time::timeout(
            Duration::from_millis(crate::desktop::REQUEST_TIMEOUT_MS),
            receiver,
        )
        .await
        {
            Ok(Ok(response)) => {
                guard.completed = true;
                let elapsed_ms = started.elapsed().as_millis() as u64;
                let outcome = desktop_response_kind(&response);
                if operation != "poll"
                    || elapsed_ms >= crate::desktop::POLL_HOLD_MS + 150
                    || outcome != "active"
                {
                    tracing::debug!(peer = %self.peer, session_id = self.id, request_id = id, operation, outcome, elapsed_ms, "desktop request completed");
                } else {
                    tracing::trace!(peer = %self.peer, session_id = self.id, request_id = id, operation, outcome, elapsed_ms, "desktop request completed");
                }
                if let crate::desktop::DesktopResponse::Unavailable { reason } = &response {
                    tracing::warn!(peer = %self.peer, session_id = self.id, request_id = id, operation, %reason, "desktop receiver unavailable");
                }
                Ok(response)
            }
            Ok(Err(_)) => {
                guard.completed = true;
                self.close(SessionCloseReason::BackendUnavailable);
                tracing::warn!(peer = %self.peer, session_id = self.id, request_id = id, operation, elapsed_ms = started.elapsed().as_millis() as u64, "desktop response channel closed before reply");
                bail!(
                    "Desktop {operation} response channel closed before reply; input session ended"
                )
            }
            Err(_) => {
                guard.completed = true;
                self.close(SessionCloseReason::BackendUnavailable);
                tracing::warn!(peer = %self.peer, session_id = self.id, request_id = id, operation, elapsed_ms = started.elapsed().as_millis() as u64, timeout_ms = crate::desktop::REQUEST_TIMEOUT_MS, "desktop request timed out");
                bail!(
                    "Desktop {operation} request timed out after {} ms",
                    crate::desktop::REQUEST_TIMEOUT_MS
                )
            }
        }
    }
}

/// At most one request in each direction: ours waiting on the peer, and the
/// peer's waiting to reach or hear back from the local desktop.
#[derive(Default)]
pub(super) struct DesktopRelay {
    waiter: Option<(u64, oneshot::Sender<DesktopResponse>)>,
    incoming: Option<(u64, DesktopRequest, Instant)>,
    reply: Option<(u64, oneshot::Receiver<DesktopResponse>, Instant)>,
}

impl DesktopRelay {
    /// Sends a local request to the peer.
    pub(super) async fn request(
        &mut self,
        reporter: &Reporter,
        channels: &mut InputChannels,
        id: u64,
        request: DesktopRequest,
        reply: oneshot::Sender<DesktopResponse>,
    ) -> Result<()> {
        if self.waiter.is_some() {
            tracing::debug!(peer = %reporter.peer, session_id = reporter.session_id, request_id = id, operation = desktop_operation(&request), "desktop request rejected while previous reply pending");
            let _ = reply.send(DesktopResponse::unavailable(
                "A desktop request is already pending",
            ));
        } else {
            tracing::trace!(peer = %reporter.peer, session_id = reporter.session_id, request_id = id, operation = desktop_operation(&request), "desktop request sending to peer");
            channels
                .control_send
                .send_desktop(DesktopMessage::Request { id, request })
                .await?;
            self.waiter = Some((id, reply));
        }
        Ok(())
    }

    pub(super) fn receive(&mut self, reporter: &Reporter, message: DesktopMessage) -> Result<()> {
        match message {
            DesktopMessage::Request { id, request } => {
                tracing::trace!(peer = %reporter.peer, session_id = reporter.session_id, request_id = id, operation = desktop_operation(&request), "desktop request received from peer");
                anyhow::ensure!(
                    self.incoming.is_none() && self.reply.is_none(),
                    "Overlapping desktop requests"
                );
                self.incoming = Some((id, request, Instant::now()));
            }
            DesktopMessage::Response { id, response } => {
                tracing::trace!(peer = %reporter.peer, session_id = reporter.session_id, request_id = id, outcome = desktop_response_kind(&response), "desktop response received from peer");
                let (expected, reply) =
                    self.waiter.take().context("Unexpected desktop response")?;
                anyhow::ensure!(id == expected, "Desktop response ID does not match request");
                let _ = reply.send(response);
            }
            DesktopMessage::Layout { layout } => {
                reporter.emit(SessionEventKind::Layout { layout })?;
            }
        }
        Ok(())
    }

    /// Hands the peer's request to the local desktop once it may run.
    pub(super) fn dispatch(
        &mut self,
        reporter: &Reporter,
        controls_pending: bool,
        activation_open: bool,
    ) -> Result<()> {
        let Some((_, request, started)) = &self.incoming else {
            return Ok(());
        };
        anyhow::ensure!(
            started.elapsed() < Duration::from_millis(crate::desktop::REQUEST_TIMEOUT_MS),
            "Desktop operation ordering timed out"
        );
        // Finish follows the sender's SessionClose on this stream. Wait through
        // playout and the daemon's backend receipt before acknowledging desktop
        // cleanup.
        let ready = !controls_pending
            && (!matches!(request, DesktopRequest::Finish { .. }) || !activation_open);
        if ready && self.reply.is_none() {
            let (id, request, started) = self.incoming.take().expect("checked above");
            tracing::trace!(peer = %reporter.peer, session_id = reporter.session_id, request_id = id, operation = desktop_operation(&request), elapsed_ms = started.elapsed().as_millis() as u64, "desktop request dispatching to receiver");
            let (reply, receipt) = oneshot::channel();
            reporter.emit(SessionEventKind::Desktop { request, reply })?;
            self.reply = Some((id, receipt, started));
        }
        Ok(())
    }

    /// Waits for the local desktop's answer to the peer's request.
    pub(super) async fn next_reply(&mut self) -> Result<(u64, DesktopResponse)> {
        let Some((id, receipt, started)) = self.reply.as_mut() else {
            return std::future::pending().await;
        };
        let deadline = tokio::time::Instant::from_std(
            *started + Duration::from_millis(crate::desktop::REQUEST_TIMEOUT_MS),
        );
        let response = tokio::time::timeout_at(deadline, receipt)
            .await
            .context("Desktop receiver timed out")?
            .context("Desktop receiver stopped")?;
        Ok((*id, response))
    }

    pub(super) async fn send_reply(
        &mut self,
        reporter: &Reporter,
        channels: &mut InputChannels,
        id: u64,
        response: DesktopResponse,
    ) -> Result<()> {
        let (_, _, started) = self.reply.take().expect("completed desktop reply");
        tracing::trace!(peer = %reporter.peer, session_id = reporter.session_id, request_id = id, outcome = desktop_response_kind(&response), elapsed_ms = started.elapsed().as_millis() as u64, "desktop reply sending to peer");
        channels
            .control_send
            .send_desktop(DesktopMessage::Response { id, response })
            .await?;
        Ok(())
    }
}
