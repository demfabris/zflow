//! The clipboard goes along with the pointer: the computer the pointer
//! leaves sends what it has copied to the one it enters. Nothing reads the
//! clipboard while the pointer stays put, and a crossing never waits for it.

use super::*;
use crate::clipboard::{Clip, Echo};
use crate::peer_view::ClipboardContents;

/// What the clipboard this computer read means for the peer it would go to.
#[derive(Debug, PartialEq)]
enum Outgoing {
    Send(Clip),
    /// Over the limit, so the person hears why nothing went.
    TooLarge(usize),
    Nothing,
}

/// Empty clipboards and clips the peer already has stay here.
fn outgoing(contents: ClipboardContents, echo: &mut Echo) -> Result<Outgoing> {
    let clip = match contents {
        ClipboardContents::Clip { kind, data } => Clip::new(kind, data.0)?,
        ClipboardContents::Empty => return Ok(Outgoing::Nothing),
        ClipboardContents::TooLarge { bytes } => return Ok(Outgoing::TooLarge(bytes)),
    };
    Ok(if echo.should_send(&clip) {
        Outgoing::Send(clip)
    } else {
        Outgoing::Nothing
    })
}

impl Shared {
    /// Sends this computer's clipboard to `peer`, which the pointer just
    /// went to, if this computer shares its clipboard.
    pub(super) fn share_clipboard(self: &Arc<Self>, peer: &str) {
        let (shared, peer) = (self.clone(), peer.to_owned());
        tokio::spawn(async move {
            if !shared.config.read().await.clipboard.share {
                return;
            }
            let session = shared.sessions.lock().await.get(&peer).cloned();
            let Some(session) = session.filter(|session| !session.is_closed()) else {
                return;
            };
            let result = match shared.desktop.read_clipboard().await {
                Ok(contents) => {
                    let mut echoes = shared.clipboard_echo.lock().await;
                    outgoing(contents, echoes.entry(peer.clone()).or_default())
                }
                Err(error) => Err(error),
            };
            match result {
                Ok(Outgoing::Send(clip)) => {
                    tracing::debug!(%peer, kind = ?clip.kind(), bytes = clip.data().len(), "clipboard sent");
                    session.send_clipboard(clip);
                }
                Ok(Outgoing::TooLarge(bytes)) => {
                    tracing::info!(%peer, bytes, "clipboard too large to share");
                    shared
                        .desktop
                        .notify(crate::clipboard::too_large(bytes))
                        .await;
                }
                Ok(Outgoing::Nothing) => {}
                Err(error) => {
                    tracing::info!(%peer, error = %format_args!("{error:#}"), "clipboard not shared");
                }
            }
        });
    }

    /// Puts a clip `peer` sent on this computer's clipboard, if this
    /// computer shares its clipboard.
    pub(super) fn keep_clipboard(self: &Arc<Self>, peer: String, clip: Clip) {
        let shared = self.clone();
        tokio::spawn(async move {
            let (kind, bytes) = (clip.kind(), clip.data().len());
            if !shared.config.read().await.clipboard.share {
                tracing::debug!(%peer, ?kind, bytes, "clipboard from peer dropped: sharing is off");
                return;
            }
            // The peer has this one, so it never needs to go back, even if
            // GNOME does not take it.
            shared
                .clipboard_echo
                .lock()
                .await
                .entry(peer.clone())
                .or_default()
                .written(&clip);
            match shared.desktop.write_clipboard(clip).await {
                Ok(()) => tracing::debug!(%peer, ?kind, bytes, "clipboard kept"),
                Err(error) => {
                    tracing::info!(%peer, ?kind, bytes, error = %format_args!("{error:#}"), "clipboard not kept");
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clipboard::{ClipKind, MAX_CLIP_BYTES};
    use crate::peer_view::ClipData;

    fn contents(text: &str) -> ClipboardContents {
        ClipboardContents::Clip {
            kind: ClipKind::Text,
            data: ClipData(text.into()),
        }
    }

    #[test]
    fn only_a_new_clip_goes_and_a_large_one_is_explained() {
        let mut echo = Echo::default();
        let clip = |text: &str| Clip::new(ClipKind::Text, text.into()).unwrap();
        assert_eq!(
            outgoing(contents("one"), &mut echo).unwrap(),
            Outgoing::Send(clip("one"))
        );
        assert_eq!(
            outgoing(contents("one"), &mut echo).unwrap(),
            Outgoing::Nothing,
            "the peer has it already"
        );
        // A clip the peer gave this computer does not bounce back.
        echo.written(&clip("two"));
        assert_eq!(
            outgoing(contents("two"), &mut echo).unwrap(),
            Outgoing::Nothing
        );
        assert_eq!(
            outgoing(ClipboardContents::Empty, &mut echo).unwrap(),
            Outgoing::Nothing
        );
        assert_eq!(
            outgoing(
                ClipboardContents::TooLarge {
                    bytes: MAX_CLIP_BYTES + 1
                },
                &mut echo
            )
            .unwrap(),
            Outgoing::TooLarge(MAX_CLIP_BYTES + 1)
        );
        // Bytes that are not what GNOME called them never go.
        let fake_png = ClipboardContents::Clip {
            kind: ClipKind::Png,
            data: ClipData(b"GIF89a".to_vec()),
        };
        assert!(outgoing(fake_png, &mut echo).is_err());
        assert_eq!(
            outgoing(contents("three"), &mut echo).unwrap(),
            Outgoing::Send(clip("three"))
        );
    }
}
