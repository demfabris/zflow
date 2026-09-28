//! What the clipboard carries between computers: one piece of text or one
//! PNG image per transfer, sent when the pointer leaves a computer.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The most a clip may hold. The over-limit notice and the stream check both
/// use this, so they always agree.
pub const MAX_CLIP_BYTES: usize = 3 * 1024 * 1024;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipKind {
    /// UTF-8 text.
    Text,
    /// A PNG image.
    Png,
}

impl ClipKind {
    /// The byte that names this kind on the wire.
    pub fn code(self) -> u8 {
        match self {
            Self::Text => 1,
            Self::Png => 2,
        }
    }

    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Text),
            2 => Some(Self::Png),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Clip {
    kind: ClipKind,
    data: Vec<u8>,
}

impl std::fmt::Debug for Clip {
    // Logs must never hold clipboard content.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Clip")
            .field("kind", &self.kind)
            .field("bytes", &self.data.len())
            .finish()
    }
}

impl Clip {
    /// Checks the size and that the bytes are what `kind` says.
    pub fn new(kind: ClipKind, data: Vec<u8>) -> Result<Self> {
        ensure!(!data.is_empty(), "The clipboard is empty");
        ensure!(data.len() <= MAX_CLIP_BYTES, "{}", too_large(data.len()));
        match kind {
            ClipKind::Text => ensure!(
                std::str::from_utf8(&data).is_ok(),
                "Clipboard text is not UTF-8"
            ),
            ClipKind::Png => ensure!(
                data.starts_with(PNG_SIGNATURE),
                "Clipboard image is not a PNG"
            ),
        }
        Ok(Self { kind, data })
    }

    pub fn kind(&self) -> ClipKind {
        self.kind
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn into_data(self) -> Vec<u8> {
        self.data
    }

    fn digest(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update([self.kind.code()]);
        hash.update(&self.data);
        hash.finalize().into()
    }
}

/// What the person sees when a clip is too large to share.
pub fn too_large(bytes: usize) -> String {
    let megabytes = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);
    format!(
        "Clipboard not shared: {:.1} MB is over the {:.0} MB limit",
        megabytes(bytes),
        megabytes(MAX_CLIP_BYTES)
    )
}

/// Keeps a clip from going back and forth: a computer does not send what it
/// just sent, nor what a peer just gave it.
#[derive(Debug, Default)]
pub struct Echo {
    sent: Option<[u8; 32]>,
    written: Option<[u8; 32]>,
}

impl Echo {
    /// Whether `clip` is news to the peer, and if so remembers sending it.
    pub fn should_send(&mut self, clip: &Clip) -> bool {
        let digest = clip.digest();
        if self.sent == Some(digest) || self.written == Some(digest) {
            return false;
        }
        self.sent = Some(digest);
        true
    }

    /// Remembers a clip a peer gave this computer.
    pub fn written(&mut self, clip: &Clip) {
        self.written = Some(clip.digest());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_are_checked_and_never_logged() {
        let text = Clip::new(ClipKind::Text, b"secret".to_vec()).unwrap();
        assert!(!format!("{text:?}").contains("secret"));
        assert!(Clip::new(ClipKind::Text, vec![0xff, 0xfe]).is_err());
        assert!(Clip::new(ClipKind::Text, Vec::new()).is_err());
        assert!(Clip::new(ClipKind::Png, b"GIF89a".to_vec()).is_err());
        let mut png = PNG_SIGNATURE.to_vec();
        png.push(0);
        assert_eq!(Clip::new(ClipKind::Png, png).unwrap().kind(), ClipKind::Png);
        let error = Clip::new(ClipKind::Text, vec![b'a'; MAX_CLIP_BYTES + 1]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Clipboard not shared: 3.0 MB is over the 3 MB limit"
        );
        assert!(Clip::new(ClipKind::Text, vec![b'a'; MAX_CLIP_BYTES]).is_ok());
        for kind in [ClipKind::Text, ClipKind::Png] {
            assert_eq!(ClipKind::from_code(kind.code()), Some(kind));
        }
        assert_eq!(ClipKind::from_code(3), None);
    }

    #[test]
    fn a_clip_does_not_bounce_back() {
        let clip = |text: &str| Clip::new(ClipKind::Text, text.as_bytes().to_vec()).unwrap();
        let mut echo = Echo::default();
        assert!(echo.should_send(&clip("one")));
        assert!(!echo.should_send(&clip("one")), "already sent");
        echo.written(&clip("two"));
        assert!(!echo.should_send(&clip("two")), "the peer gave it to us");
        assert!(echo.should_send(&clip("three")));
    }
}
