//! The clipboard goes along with the pointer, as on Linux (see
//! src/daemon/clipboard.rs): the computer the pointer leaves sends what it
//! has copied to the one it enters. The pasteboard is read only then, on a
//! blocking thread, so a crossing never waits for it.

use std::{
    collections::BTreeMap,
    ffi::c_char,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result, bail};

use crate::{
    clipboard::{Clip, ClipKind, Echo, MAX_CLIP_BYTES},
    session::SessionHandle,
};

/// What the pasteboard held when it was read.
#[derive(Debug, PartialEq)]
pub(crate) enum Contents {
    /// Still what this Mac last wrote there, so it was not read again.
    Unchanged,
    Empty,
    Clip(ClipKind, Vec<u8>),
    /// Over [`MAX_CLIP_BYTES`], so only its size was read.
    TooLarge(usize),
    /// macOS would ask the person first, or refuses, so it was not read.
    NotAllowed,
}

/// This Mac's pasteboard. Tests use `FakePasteboard`.
pub(crate) trait Pasteboard: Send + Sync + 'static {
    /// What the pasteboard holds, or `Unchanged` while its change count is
    /// still `ours`.
    fn read(&self, ours: Option<i64>) -> Result<Contents>;
    /// Puts `clip` on the pasteboard beside zflow's marker, and returns the
    /// change count after it.
    fn write(&self, clip: &Clip) -> Result<i64>;
    /// Reads it once now if macOS has never asked the person about zflow
    /// reading it, so that alert shows while they are here. Does not wait.
    fn ask(&self);
}

/// The general pasteboard, reached on the main thread by pasteboard.m.
pub(crate) struct MacPasteboard;

const KIND_EMPTY: u32 = 0;
const KIND_UNCHANGED: u32 = 3;
const KIND_NOT_ALLOWED: u32 = 4;

/// What Checks shows while macOS keeps zflow from reading the pasteboard.
const NOT_ALLOWED: &str = "Clipboard not shared: allow zflow in System Settings > \
    Privacy & Security > Paste from Other Apps";

impl Pasteboard for MacPasteboard {
    fn read(&self, ours: Option<i64>) -> Result<Contents> {
        let (mut kind, mut data, mut length, mut count) = (0, std::ptr::null_mut(), 0, 0);
        let unchanged_at = ours.as_ref().map_or(std::ptr::null(), |count| count);
        // SAFETY: every output points to writable storage, and `unchanged_at`
        // is null or points to a live i64.
        let status = unsafe {
            zflow_mac_pasteboard_read(
                std::ptr::null(),
                unchanged_at,
                MAX_CLIP_BYTES,
                &mut kind,
                &mut data,
                &mut length,
                &mut count,
            )
        };
        if status != 0 {
            bail!("Could not read the Mac clipboard");
        }
        let bytes = (!data.is_null()).then(|| {
            // SAFETY: the bridge allocated `length` bytes at `data` and hands them over.
            let bytes = unsafe { std::slice::from_raw_parts(data, length) }.to_vec();
            // SAFETY: `data` came from the bridge's malloc and is freed once.
            unsafe { zflow_mac_pasteboard_free(data) };
            bytes
        });
        Ok(match (kind, bytes) {
            (KIND_UNCHANGED, _) => Contents::Unchanged,
            (KIND_NOT_ALLOWED, _) => Contents::NotAllowed,
            (KIND_EMPTY, _) => Contents::Empty,
            (_, None) if length > MAX_CLIP_BYTES => Contents::TooLarge(length),
            (_, None) => Contents::Empty,
            (code, Some(bytes)) => {
                let kind = u8::try_from(code).ok().and_then(ClipKind::from_code);
                Contents::Clip(kind.context("Unknown clipboard kind")?, bytes)
            }
        })
    }

    fn write(&self, clip: &Clip) -> Result<i64> {
        let mut count = 0;
        let data = clip.data();
        // SAFETY: `data` is live for the call, which copies it, and `count`
        // is writable.
        let status = unsafe {
            zflow_mac_pasteboard_write(
                std::ptr::null(),
                u32::from(clip.kind().code()),
                data.as_ptr(),
                data.len(),
                &mut count,
            )
        };
        if status != 0 {
            bail!("Could not write the Mac clipboard");
        }
        Ok(count)
    }

    fn ask(&self) {
        // The app's tests turn sharing on with this pasteboard, and must
        // leave the person's clipboard alone.
        if cfg!(test) {
            return;
        }
        // SAFETY: a null name means the general pasteboard.
        unsafe { zflow_mac_pasteboard_ask(std::ptr::null()) }
    }
}

/// What a read means for the peer the pointer went to.
#[derive(Debug, PartialEq)]
enum Outgoing {
    Send(Clip),
    /// Over the limit, so the person hears why nothing went.
    TooLarge(usize),
    /// Not read, so the person hears how to allow it.
    NotAllowed,
    Nothing,
}

#[derive(Default)]
struct State {
    /// What each peer has from this Mac, or gave it.
    echoes: BTreeMap<String, Echo>,
    /// The change count this Mac's last write left, and the clip it wrote.
    written: Option<(i64, Clip)>,
    /// Why the last clipboard read went nowhere, while it was too large or
    /// not allowed.
    notice: Option<String>,
}

impl State {
    /// Empty pasteboards and clips the peer already has stay here. A
    /// pasteboard still holding this Mac's last write stands for the clip it
    /// wrote, so that clip never goes back to the peer that sent it.
    fn outgoing(&mut self, peer: &str, contents: Contents) -> Result<Outgoing> {
        let clip = match contents {
            Contents::Unchanged => match &self.written {
                Some((_, clip)) => clip.clone(),
                None => return Ok(Outgoing::Nothing),
            },
            Contents::Empty => return Ok(Outgoing::Nothing),
            Contents::TooLarge(bytes) => return Ok(Outgoing::TooLarge(bytes)),
            Contents::NotAllowed => return Ok(Outgoing::NotAllowed),
            Contents::Clip(_, data) if data.is_empty() => return Ok(Outgoing::Nothing),
            Contents::Clip(_, data) if data.len() > MAX_CLIP_BYTES => {
                return Ok(Outgoing::TooLarge(data.len()));
            }
            Contents::Clip(kind, data) => Clip::new(kind, data)?,
        };
        let echo = self.echoes.entry(peer.to_owned()).or_default();
        Ok(if echo.should_send(&clip) {
            Outgoing::Send(clip)
        } else {
            Outgoing::Nothing
        })
    }
}

/// Shares this Mac's pasteboard with paired computers. Every link uses the
/// same one, through `Receiving`.
pub(crate) struct Clipboard {
    pasteboard: Arc<dyn Pasteboard>,
    /// `[clipboard] share`, as the app last saw it.
    share: AtomicBool,
    state: Mutex<State>,
}

impl Clipboard {
    pub fn new(pasteboard: Arc<dyn Pasteboard>) -> Arc<Self> {
        Arc::new(Self {
            pasteboard,
            share: AtomicBool::new(false),
            state: Mutex::default(),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn set_share(&self, share: bool) {
        let shared = self.share.swap(share, Ordering::Relaxed);
        if share && !shared {
            // The person is at this Mac now, unlike at a crossing.
            self.pasteboard.ask();
        }
        if !share {
            self.state().notice = None;
        }
    }

    /// Why the last clipboard went nowhere, while that still holds.
    pub fn notice(&self) -> Option<String> {
        self.state().notice.clone()
    }

    /// Sends this Mac's clipboard to `session`'s peer, which the pointer
    /// just went to, if this Mac shares its clipboard. Only the blocking
    /// thread that reads the pasteboard waits for it.
    pub fn share(self: &Arc<Self>, session: &SessionHandle) {
        if !self.share.load(Ordering::Relaxed) {
            return;
        }
        let (clipboard, session) = (self.clone(), session.clone());
        tokio::task::spawn_blocking(move || {
            if session.is_closed() {
                return;
            }
            clipboard.send_to(session.peer(), |clip| session.send_clipboard(clip));
        });
    }

    /// Reads the pasteboard and gives `send` what `peer` does not have yet.
    fn send_to(&self, peer: &str, send: impl FnOnce(Clip)) {
        let ours = self.state().written.as_ref().map(|(count, _)| *count);
        let outgoing = self
            .pasteboard
            .read(ours)
            .and_then(|contents| self.state().outgoing(peer, contents));
        match outgoing {
            Ok(Outgoing::Send(clip)) => {
                self.state().notice = None;
                tracing::debug!(%peer, kind = ?clip.kind(), bytes = clip.data().len(), "clipboard sent");
                send(clip);
            }
            Ok(Outgoing::TooLarge(bytes)) => {
                tracing::info!(%peer, bytes, "clipboard too large to share");
                self.state().notice = Some(crate::clipboard::too_large(bytes));
            }
            Ok(Outgoing::NotAllowed) => {
                tracing::info!(%peer, "clipboard not shared: macOS does not let zflow read it");
                self.state().notice = Some(NOT_ALLOWED.into());
            }
            Ok(Outgoing::Nothing) => self.state().notice = None,
            Err(error) => {
                tracing::info!(%peer, error = %format_args!("{error:#}"), "clipboard not shared");
            }
        }
    }

    /// Puts a clip `peer` sent on this Mac's pasteboard, if this Mac shares
    /// its clipboard.
    pub fn keep(self: &Arc<Self>, peer: &str, clip: Clip) {
        let (kind, bytes) = (clip.kind(), clip.data().len());
        if !self.share.load(Ordering::Relaxed) {
            tracing::debug!(%peer, ?kind, bytes, "clipboard from peer dropped: sharing is off");
            return;
        }
        // The peer has this one, so it never needs to go back, even if the
        // pasteboard does not take it.
        self.state()
            .echoes
            .entry(peer.to_owned())
            .or_default()
            .written(&clip);
        let (clipboard, peer) = (self.clone(), peer.to_owned());
        tokio::task::spawn_blocking(move || match clipboard.pasteboard.write(&clip) {
            Ok(count) => {
                tracing::debug!(%peer, ?kind, bytes, "clipboard kept");
                clipboard.state().written = Some((count, clip));
            }
            Err(error) => {
                tracing::info!(%peer, ?kind, bytes, error = %format_args!("{error:#}"), "clipboard not kept");
            }
        });
    }
}

unsafe extern "C" {
    fn zflow_mac_pasteboard_read(
        name: *const c_char,
        unchanged_at: *const i64,
        limit: usize,
        kind: *mut u32,
        data: *mut *mut u8,
        length: *mut usize,
        change_count: *mut i64,
    ) -> i32;
    fn zflow_mac_pasteboard_free(data: *mut u8);
    fn zflow_mac_pasteboard_ask(name: *const c_char);
    fn zflow_mac_pasteboard_write(
        name: *const c_char,
        kind: u32,
        data: *const u8,
        length: usize,
        change_count: *mut i64,
    ) -> i32;
}

/// A pasteboard in memory with a change count, as macOS keeps one.
/// Clones share it, so a test keeps one and hands one to the links.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct FakePasteboard(Arc<Mutex<FakeBoard>>);

#[cfg(test)]
#[derive(Default)]
pub(crate) struct FakeBoard {
    pub contents: Option<(ClipKind, Vec<u8>)>,
    pub count: i64,
    pub reads: usize,
    pub writes: Vec<Clip>,
    /// macOS keeps zflow from reading it.
    pub not_allowed: bool,
    pub asks: usize,
}

#[cfg(test)]
impl FakePasteboard {
    pub fn board(&self) -> MutexGuard<'_, FakeBoard> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What a person copying `data` would leave.
    pub fn copy(&self, kind: ClipKind, data: &[u8]) {
        let mut board = self.board();
        board.contents = Some((kind, data.to_vec()));
        board.count += 1;
    }
}

#[cfg(test)]
impl Pasteboard for FakePasteboard {
    fn read(&self, ours: Option<i64>) -> Result<Contents> {
        let mut board = self.board();
        board.reads += 1;
        if ours == Some(board.count) {
            return Ok(Contents::Unchanged);
        }
        if board.not_allowed {
            return Ok(Contents::NotAllowed);
        }
        Ok(match &board.contents {
            None => Contents::Empty,
            Some((_, data)) if data.len() > MAX_CLIP_BYTES => Contents::TooLarge(data.len()),
            Some((kind, data)) => Contents::Clip(*kind, data.clone()),
        })
    }

    fn write(&self, clip: &Clip) -> Result<i64> {
        let mut board = self.board();
        board.contents = Some((clip.kind(), clip.data().to_vec()));
        board.count += 1;
        board.writes.push(clip.clone());
        Ok(board.count)
    }

    fn ask(&self) {
        self.board().asks += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(text: &str) -> Clip {
        Clip::new(ClipKind::Text, text.into()).unwrap()
    }

    #[test]
    fn only_a_new_clip_goes_and_a_large_one_is_explained() {
        let mut state = State::default();
        let clip = |data: &str| Contents::Clip(ClipKind::Text, data.into());
        let mut outgoing = |contents| state.outgoing("linux", contents).unwrap();
        assert_eq!(outgoing(clip("one")), Outgoing::Send(text("one")));
        assert_eq!(outgoing(clip("one")), Outgoing::Nothing, "linux has it");
        assert_eq!(outgoing(Contents::Empty), Outgoing::Nothing);
        assert_eq!(outgoing(clip("")), Outgoing::Nothing, "empty text");
        assert_eq!(outgoing(Contents::Unchanged), Outgoing::Nothing);
        assert_eq!(outgoing(Contents::NotAllowed), Outgoing::NotAllowed);
        let over = MAX_CLIP_BYTES + 1;
        assert_eq!(outgoing(Contents::TooLarge(over)), Outgoing::TooLarge(over));
        let large = Contents::Clip(ClipKind::Text, vec![b'a'; over]);
        assert_eq!(outgoing(large), Outgoing::TooLarge(over));
        // Bytes that are not what the pasteboard called them never go.
        let fake_png = Contents::Clip(ClipKind::Png, b"GIF89a".to_vec());
        assert!(state.outgoing("linux", fake_png).is_err());
        // Each peer has its own record.
        assert_eq!(
            state.outgoing("desk", clip("one")).unwrap(),
            Outgoing::Send(text("one"))
        );
    }

    #[test]
    fn this_macs_own_write_stands_for_what_it_wrote() {
        let mut state = State::default();
        let two = text("two");
        state
            .echoes
            .entry("linux".into())
            .or_default()
            .written(&two);
        state.written = Some((7, two.clone()));
        // The pasteboard still holds what linux sent, so linux gets nothing
        // back, while another computer the pointer goes to gets it.
        let unchanged = state.outgoing("linux", Contents::Unchanged).unwrap();
        assert_eq!(unchanged, Outgoing::Nothing);
        let forwarded = state.outgoing("desk", Contents::Unchanged).unwrap();
        assert_eq!(forwarded, Outgoing::Send(two));
    }

    #[test]
    fn a_clip_from_a_peer_is_dropped_while_sharing_is_off() {
        let fake = FakePasteboard::default();
        fake.copy(ClipKind::Text, b"copied here");
        let clipboard = Clipboard::new(Arc::new(fake.clone()));
        // Outside a runtime, so a write it started would panic.
        clipboard.keep("linux", text("from linux"));
        let board = fake.board();
        assert_eq!((board.reads, board.writes.len()), (0, 0));
        assert_eq!(
            board.contents,
            Some((ClipKind::Text, b"copied here".to_vec()))
        );
        drop(board);
        assert!(clipboard.state().echoes.is_empty());
    }

    /// Runs `work` inside a runtime, then waits for the blocking tasks it
    /// started.
    fn blocking(work: impl FnOnce()) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        {
            let _entered = runtime.enter();
            work();
        }
        runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    }

    #[test]
    fn a_kept_clip_goes_on_the_pasteboard_and_never_back() {
        let fake = FakePasteboard::default();
        let clipboard = Clipboard::new(Arc::new(fake.clone()));
        clipboard.set_share(true);
        blocking(|| clipboard.keep("linux", text("from linux")));
        assert_eq!(fake.board().writes, [text("from linux")]);
        let written = clipboard.state().written.clone();
        assert_eq!(written, Some((1, text("from linux"))));

        let mut sent = Vec::new();
        clipboard.send_to("linux", |clip| sent.push(clip));
        assert!(sent.is_empty(), "linux sent it");
        // The unchanged pasteboard was not read back.
        assert_eq!(fake.board().reads, 1);
        clipboard.send_to("desk", |clip| sent.push(clip));
        assert_eq!(sent, [text("from linux")]);

        // Something copied here since then goes to linux, once.
        fake.copy(ClipKind::Text, b"copied here");
        clipboard.send_to("linux", |clip| sent.push(clip));
        clipboard.send_to("linux", |clip| sent.push(clip));
        assert_eq!(sent[1..], [text("copied here")]);
    }

    #[test]
    fn turning_sharing_on_lets_macos_ask_and_a_refusal_is_explained() {
        let fake = FakePasteboard::default();
        let clipboard = Clipboard::new(Arc::new(fake.clone()));
        // Only turning it on asks: at startup, on resume, or from the switch.
        clipboard.set_share(true);
        clipboard.set_share(true);
        assert_eq!(fake.board().asks, 1);
        clipboard.set_share(false);
        clipboard.set_share(true);
        assert_eq!(fake.board().asks, 2);

        fake.copy(ClipKind::Text, b"copied here");
        fake.board().not_allowed = true;
        let mut sent = Vec::new();
        clipboard.send_to("linux", |clip| sent.push(clip));
        assert!(sent.is_empty());
        assert_eq!(
            clipboard.notice().as_deref(),
            Some(
                "Clipboard not shared: allow zflow in System Settings > \
                 Privacy & Security > Paste from Other Apps"
            )
        );
        // Once allowed, the next crossing sends it and the warning goes.
        fake.board().not_allowed = false;
        clipboard.send_to("linux", |clip| sent.push(clip));
        assert_eq!(sent, [text("copied here")]);
        assert_eq!(clipboard.notice(), None);

        // A clip this Mac wrote itself still goes on, since it needs no read.
        fake.board().not_allowed = true;
        blocking(|| clipboard.keep("linux", text("from linux")));
        clipboard.send_to("desk", |clip| sent.push(clip));
        assert_eq!(sent[1..], [text("from linux")]);
    }

    #[test]
    fn a_large_clipboard_is_explained_until_the_next_one_goes() {
        let fake = FakePasteboard::default();
        let clipboard = Clipboard::new(Arc::new(fake.clone()));
        clipboard.set_share(true);
        let png = |bytes: usize| {
            let mut data = b"\x89PNG\r\n\x1a\n".to_vec();
            data.resize(bytes, 0);
            data
        };
        fake.copy(ClipKind::Png, &png(5 * 1024 * 1024));
        let mut sent = Vec::new();
        clipboard.send_to("linux", |clip| sent.push(clip));
        assert!(sent.is_empty());
        assert_eq!(
            clipboard.notice().as_deref(),
            Some("Clipboard not shared: 5.0 MB is over the 3 MB limit")
        );
        fake.copy(ClipKind::Png, &png(1024));
        clipboard.send_to("linux", |clip| sent.push(clip));
        assert_eq!(sent.len(), 1);
        assert_eq!(clipboard.notice(), None);
        // Turning sharing off clears it too.
        fake.copy(ClipKind::Png, &png(MAX_CLIP_BYTES + 1));
        clipboard.send_to("linux", |clip| sent.push(clip));
        assert!(clipboard.notice().is_some());
        clipboard.set_share(false);
        assert_eq!(clipboard.notice(), None);
    }
}
