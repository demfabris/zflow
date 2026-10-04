//! Windows text and registered-PNG clipboard formats, bounded before allocation.
use crate::clipboard::{Clip, ClipKind, MAX_CLIP_BYTES};
use anyhow::{Result, ensure};
use windows_sys::Win32::{
    Foundation::GlobalFree,
    System::{DataExchange::*, Memory::*},
};

const UNICODE_TEXT: u32 = 13;
unsafe extern "C" {
    fn zflow_clipboard_window() -> *mut std::ffi::c_void;
}
struct Open;
impl Drop for Open {
    fn drop(&mut self) {
        unsafe {
            CloseClipboard();
        }
    }
}
fn open() -> Result<Open> {
    // The capture window lives for the entire engine lifetime and gives
    // SetClipboardData a non-null owner. Never claim a user's foreground window.
    ensure!(
        unsafe { OpenClipboard(zflow_clipboard_window()) } != 0,
        "Clipboard is busy; it stayed on this computer"
    );
    Ok(Open)
}
fn format(name: &str) -> u32 {
    let wide: Vec<_> = name.encode_utf16().chain([0]).collect();
    unsafe { RegisterClipboardFormatW(wide.as_ptr()) }
}
fn read_format(kind: u32, limit: usize) -> Result<Option<Vec<u8>>> {
    unsafe {
        let handle = GetClipboardData(kind);
        if handle.is_null() {
            return Ok(None);
        }
        let length = GlobalSize(handle);
        ensure!(length <= limit, "{}", crate::clipboard::too_large(length));
        if length == 0 {
            return Ok(None);
        }
        let bytes = GlobalLock(handle);
        ensure!(!bytes.is_null(), "Could not read clipboard memory");
        let value = std::slice::from_raw_parts(bytes.cast::<u8>(), length).to_vec();
        GlobalUnlock(handle);
        Ok(Some(value))
    }
}
pub fn read() -> Result<Option<Clip>> {
    let _open = open()?;
    // Password managers use this convention to mark content that should not
    // enter clipboard history or another device.
    if unsafe { IsClipboardFormatAvailable(format("ExcludeClipboardContentFromMonitorProcessing")) }
        != 0
    {
        return Ok(None);
    }
    if let Some(png) = read_format(format("PNG"), MAX_CLIP_BYTES)? {
        return Clip::new(ClipKind::Png, png).map(Some);
    }
    let Some(bytes) = read_format(UNICODE_TEXT, MAX_CLIP_BYTES * 2 + 2)? else {
        return Ok(None);
    };
    let text = decode_text(&bytes)?;
    if text.is_empty() {
        return Ok(None);
    }
    Clip::new(ClipKind::Text, text.into_bytes()).map(Some)
}
fn decode_text(bytes: &[u8]) -> Result<String> {
    ensure!(
        bytes.len().is_multiple_of(2),
        "Windows clipboard text has an incomplete UTF-16 character"
    );
    let words: Vec<_> = bytes
        .chunks_exact(2)
        .map(|p| u16::from_le_bytes([p[0], p[1]]))
        .take_while(|&w| w != 0)
        .collect();
    Ok(String::from_utf16(&words)?)
}
pub fn write(clip: &Clip) -> Result<()> {
    let (kind, bytes) = match clip.kind() {
        ClipKind::Text => (
            UNICODE_TEXT,
            std::str::from_utf8(clip.data())?
                .encode_utf16()
                .chain([0])
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>(),
        ),
        ClipKind::Png => (format("PNG"), clip.data().to_vec()),
    };
    let _open = open()?;
    unsafe {
        let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
        ensure!(!handle.is_null(), "Could not allocate clipboard memory");
        let destination = GlobalLock(handle);
        if destination.is_null() {
            GlobalFree(handle);
            anyhow::bail!("Could not lock clipboard memory");
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), destination.cast::<u8>(), bytes.len());
        GlobalUnlock(handle);
        if EmptyClipboard() == 0 || SetClipboardData(kind, handle).is_null() {
            GlobalFree(handle);
            anyhow::bail!("Could not write the Windows clipboard");
        }
        // Successful SetClipboardData transfers ownership to Windows.
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn utf16_is_bounded_terminated_and_checked() {
        let bytes: Vec<_> = "Olá 👋\0ignored"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(decode_text(&bytes).unwrap(), "Olá 👋");
        assert!(decode_text(&[0]).is_err());
        assert!(decode_text(&[0, 0xd8]).is_err());
    }
}
