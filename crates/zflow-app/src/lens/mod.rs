//! The lens: a Mac looks at another computer's screen in a circle around its
//! pointer, and works there. The remote desktop is fitted whole onto the
//! screen under the pointer, and only the circle shows it. Clicks, scrolls
//! and keys go to the other computer. ⌃⌥L opens and closes it.

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub mod geometry;

#[cfg(target_os = "macos")]
mod mac;

#[cfg(target_os = "macos")]
pub use mac::{HOTKEY_LABEL, init, look, target};
