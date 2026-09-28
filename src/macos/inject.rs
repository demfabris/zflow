//! Posting input on the Mac for a peer that controls it.

// Nothing posts yet, and tests never reach the real poster. Remove once the
// injector is wired in.
#![allow(dead_code)]

use std::{
    ffi::{CStr, c_char},
    time::Duration,
};

use anyhow::{Result, bail, ensure};

use super::{CursorPosition, pointer::Scroll};

/// "zflow" in ASCII. Every posted event carries it.
const POSTED_MARK: i64 = 0x7A_666C_6F77;
const DEFAULT_REPEAT: KeyRepeat = KeyRepeat {
    delay: Duration::from_millis(250),
    interval: Duration::from_millis(33),
};
const DEFAULT_DOUBLE_CLICK: Duration = Duration::from_millis(500);
const MIN_INTERVAL: Duration = Duration::from_millis(1);

/// A button held during a move, as a CG button number, with the click state
/// of its press.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Drag {
    pub button: u16,
    pub click_state: i64,
}

/// One event to post. No `Debug`, so typed keys never reach a log.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Posted {
    Move {
        to: CursorPosition,
        dx: i64,
        dy: i64,
        drag: Option<Drag>,
        flags: u64,
    },
    /// `button` is a CG button: 0 left, 1 right, 2 and up other.
    Button {
        button: u16,
        down: bool,
        at: CursorPosition,
        click_state: i64,
        flags: u64,
    },
    Key {
        code: u16,
        down: bool,
        autorepeat: bool,
        flags: u64,
    },
    /// A modifier going down or up. `flags` already include the change.
    Modifier {
        code: u16,
        down: bool,
        flags: u64,
    },
    Scroll {
        scroll: Scroll,
        flags: u64,
    },
    /// An `NX_KEYTYPE`, such as volume up.
    Media {
        key: u32,
        down: bool,
    },
}

impl Posted {
    /// The event for inject.c. Media keys go through AppKit instead, so
    /// theirs is empty and inject.c refuses it.
    fn native(&self) -> NativePosted {
        let mut native = NativePosted::default();
        match *self {
            Self::Move {
                to,
                dx,
                dy,
                drag,
                flags,
            } => {
                native.kind = NativePostKind::Move as u32;
                (native.x, native.y) = (to.x, to.y);
                (native.dx, native.dy) = (dx, dy);
                if let Some(drag) = drag {
                    native.drag = 1;
                    native.code = drag.button;
                    native.click_state = drag.click_state;
                }
                native.flags = flags;
            }
            Self::Button {
                button,
                down,
                at,
                click_state,
                flags,
            } => {
                native.kind = NativePostKind::Button as u32;
                native.code = button;
                native.down = u8::from(down);
                (native.x, native.y) = (at.x, at.y);
                native.click_state = click_state;
                native.flags = flags;
            }
            Self::Key {
                code,
                down,
                autorepeat,
                flags,
            } => {
                native.kind = NativePostKind::Key as u32;
                native.code = code;
                native.down = u8::from(down);
                native.autorepeat = u8::from(autorepeat);
                native.flags = flags;
            }
            Self::Modifier { code, down, flags } => {
                native.kind = NativePostKind::Modifier as u32;
                native.code = code;
                native.down = u8::from(down);
                native.flags = flags;
            }
            Self::Scroll { scroll, flags } => {
                native.kind = NativePostKind::Scroll as u32;
                (native.wheel_x, native.wheel_y, native.pixel) = match scroll {
                    Scroll::Lines { x, y } => (x, y, 0),
                    Scroll::Pixels { x, y } => (x, y, 1),
                };
                native.flags = flags;
            }
            Self::Media { .. } => {}
        }
        native
    }
}

/// Posts through inject.c, which keeps one event source and one table of
/// held input for the whole process. Only one may exist.
pub(crate) struct MacBackend(());

impl MacBackend {
    pub fn open() -> Result<Self> {
        // SAFETY: open creates the source once and is safe to call again.
        if unsafe { zflow_mac_inject_open() } != 0 {
            bail!("could not create a Mac event source");
        }
        Ok(Self(()))
    }

    pub fn post(&mut self, event: &Posted) -> Result<()> {
        let status = match *event {
            // SAFETY: AppKit builds and posts its own event.
            Posted::Media { key, down } => unsafe {
                zflow_mac_post_media_key(key, i32::from(down), POSTED_MARK)
            },
            // SAFETY: the event has inject.c's C layout and outlives the call.
            _ => unsafe { zflow_mac_inject_post(&event.native()) },
        };
        ensure!(status == 0, "could not post a Mac input event");
        Ok(())
    }

    pub fn caps_lock(&mut self) -> Result<bool> {
        let mut on = 0;
        // SAFETY: the output is a plain int.
        ensure!(
            unsafe { zflow_mac_caps_lock(&mut on) } == 0,
            "could not read Caps Lock"
        );
        Ok(on != 0)
    }

    /// Flips the real lock, light included, and returns whether it is on.
    pub fn toggle_caps_lock(&mut self) -> Result<bool> {
        let on = !self.caps_lock()?;
        // SAFETY: this only calls IOHIDSystem.
        ensure!(
            unsafe { zflow_mac_set_caps_lock(i32::from(on)) } == 0,
            "could not set Caps Lock"
        );
        Ok(on)
    }

    /// Releases whatever inject.c still holds.
    pub fn release_all(&mut self) {
        // SAFETY: this posts releases from inject.c's own table.
        unsafe { zflow_mac_inject_release_all() }
    }
}

impl Drop for MacBackend {
    fn drop(&mut self) {
        // SAFETY: close releases everything held, then frees the source.
        unsafe { zflow_mac_inject_close() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KeyRepeat {
    pub delay: Duration,
    pub interval: Duration,
}

impl Default for KeyRepeat {
    fn default() -> Self {
        DEFAULT_REPEAT
    }
}

/// This Mac's key repeat: IOHIDSystem's, then the preferences, then AppKit's.
pub(crate) fn key_repeat() -> KeyRepeat {
    let (mut initial, mut interval) = (0, 0);
    // SAFETY: both outputs are plain integers.
    if unsafe { zflow_mac_key_repeat_ns(&mut initial, &mut interval) } == 0 {
        return KeyRepeat {
            delay: Duration::from_nanos(initial).max(MIN_INTERVAL),
            interval: Duration::from_nanos(interval).max(MIN_INTERVAL),
        };
    }
    let (mut delay, mut every) = (0.0, 0.0);
    // SAFETY: both outputs are plain doubles.
    if unsafe { zflow_mac_appkit_key_repeat(&mut delay, &mut every) } == 0 {
        return KeyRepeat {
            delay: seconds(delay).unwrap_or(DEFAULT_REPEAT.delay),
            interval: seconds(every).unwrap_or(DEFAULT_REPEAT.interval),
        };
    }
    DEFAULT_REPEAT
}

pub(crate) fn double_click_interval() -> Duration {
    // SAFETY: this reads an AppKit setting.
    seconds(unsafe { zflow_mac_double_click_interval() }).unwrap_or(DEFAULT_DOUBLE_CLICK)
}

/// A positive, finite, sane number of seconds.
fn seconds(value: f64) -> Option<Duration> {
    (value.is_finite() && value > 0.0 && value < 60.0)
        .then(|| Duration::from_secs_f64(value).max(MIN_INTERVAL))
}

/// The ISO keyboard swaps the keys left of 1 and left of Z.
pub(crate) fn keyboard_is_iso() -> bool {
    // SAFETY: this reads the last keyboard's type.
    unsafe { zflow_mac_keyboard_is_iso() == 1 }
}

pub(crate) fn frontmost_bundle_id() -> Option<String> {
    let mut buffer = [0u8; 256];
    // SAFETY: the bridge writes a NUL-terminated string within the capacity.
    if unsafe { zflow_mac_frontmost_bundle_id(buffer.as_mut_ptr().cast(), buffer.len()) } < 0 {
        return None;
    }
    let bundle = CStr::from_bytes_until_nul(&buffer).ok()?;
    bundle.to_str().ok().map(str::to_owned)
}

/// The screen lock is up, or another user has the console.
pub(crate) fn session_locked() -> bool {
    // SAFETY: this reads the window server's session dictionary.
    unsafe { zflow_mac_session_locked() != 0 }
}

/// Accessibility lets this process post events.
pub(crate) fn post_allowed() -> bool {
    // SAFETY: this checks a permission without prompting.
    unsafe { zflow_mac_post_allowed() == 1 }
}

/// Wakes the display as local input would.
pub(crate) fn declare_user_activity() -> Result<()> {
    // SAFETY: this only declares an IOKit power assertion.
    ensure!(
        unsafe { zflow_mac_declare_user_activity() } == 0,
        "could not wake the Mac display"
    );
    Ok(())
}

/// Releases held input on SIGINT, SIGTERM and SIGHUP before the app dies.
pub(crate) fn install_exit_handlers() {
    // SAFETY: installs once; later calls do nothing.
    unsafe { zflow_mac_inject_install_exit_handlers() }
}

#[repr(u32)]
enum NativePostKind {
    Move = 1,
    Button = 2,
    Key = 3,
    Modifier = 4,
    Scroll = 5,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NativePosted {
    kind: u32,
    code: u16,
    down: u8,
    autorepeat: u8,
    x: f64,
    y: f64,
    dx: i64,
    dy: i64,
    wheel_x: i32,
    wheel_y: i32,
    pixel: u8,
    drag: u8,
    padding: [u8; 6],
    click_state: i64,
    flags: u64,
}

const _: () = assert!(std::mem::size_of::<NativePosted>() == 72);

unsafe extern "C" {
    fn zflow_mac_inject_open() -> i32;
    fn zflow_mac_inject_post(posted: *const NativePosted) -> i32;
    fn zflow_mac_inject_release_all();
    fn zflow_mac_inject_close();
    fn zflow_mac_caps_lock(on: *mut i32) -> i32;
    fn zflow_mac_set_caps_lock(on: i32) -> i32;
    fn zflow_mac_keyboard_is_iso() -> i32;
    fn zflow_mac_key_repeat_ns(initial: *mut u64, interval: *mut u64) -> i32;
    fn zflow_mac_session_locked() -> i32;
    fn zflow_mac_post_allowed() -> i32;
    fn zflow_mac_declare_user_activity() -> i32;
    fn zflow_mac_inject_install_exit_handlers();
    fn zflow_mac_post_media_key(key: u32, down: i32, mark: i64) -> i32;
    fn zflow_mac_frontmost_bundle_id(buffer: *mut c_char, capacity: usize) -> i32;
    fn zflow_mac_double_click_interval() -> f64;
    fn zflow_mac_appkit_key_repeat(delay: *mut f64, interval: *mut f64) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: CursorPosition = CursorPosition { x: 10.5, y: -20.0 };

    #[test]
    fn moves_and_drags_carry_location_deltas_and_button() {
        let native = Posted::Move {
            to: AT,
            dx: 3,
            dy: -4,
            drag: None,
            flags: 0x10_0008,
        }
        .native();
        assert_eq!(native.kind, 1);
        assert_eq!(
            (native.x, native.y, native.dx, native.dy),
            (10.5, -20.0, 3, -4)
        );
        assert_eq!((native.drag, native.code, native.flags), (0, 0, 0x10_0008));

        let native = Posted::Move {
            to: AT,
            dx: 0,
            dy: 1,
            drag: Some(Drag {
                button: 2,
                click_state: 3,
            }),
            flags: 0,
        }
        .native();
        assert_eq!((native.drag, native.code, native.click_state), (1, 2, 3));
    }

    #[test]
    fn buttons_keys_and_modifiers_carry_their_state() {
        let native = Posted::Button {
            button: 1,
            down: true,
            at: AT,
            click_state: 2,
            flags: 0x2_0002,
        }
        .native();
        assert_eq!((native.kind, native.code, native.down), (2, 1, 1));
        assert_eq!(
            (native.x, native.click_state, native.flags),
            (10.5, 2, 0x2_0002)
        );

        let native = Posted::Key {
            code: 40,
            down: true,
            autorepeat: true,
            flags: 0,
        }
        .native();
        assert_eq!(
            (native.kind, native.code, native.down, native.autorepeat),
            (3, 40, 1, 1)
        );

        let native = Posted::Modifier {
            code: 55,
            down: false,
            flags: 0,
        }
        .native();
        assert_eq!((native.kind, native.code, native.down), (4, 55, 0));
    }

    #[test]
    fn scroll_keeps_its_axes_and_units() {
        let native = Posted::Scroll {
            scroll: Scroll::Lines { x: -1, y: 2 },
            flags: 0,
        }
        .native();
        assert_eq!(
            (native.kind, native.wheel_x, native.wheel_y, native.pixel),
            (5, -1, 2, 0)
        );
        let native = Posted::Scroll {
            scroll: Scroll::Pixels { x: 7, y: -9 },
            flags: 0,
        }
        .native();
        assert_eq!((native.wheel_x, native.wheel_y, native.pixel), (7, -9, 1));
    }

    #[test]
    fn media_keys_do_not_go_through_the_c_table() {
        assert_eq!(Posted::Media { key: 0, down: true }.native().kind, 0);
    }

    #[test]
    fn only_sane_seconds_count() {
        assert_eq!(seconds(0.25), Some(Duration::from_millis(250)));
        assert_eq!(seconds(0.0001), Some(MIN_INTERVAL));
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, 120.0] {
            assert_eq!(seconds(bad), None, "{bad}");
        }
    }
}
