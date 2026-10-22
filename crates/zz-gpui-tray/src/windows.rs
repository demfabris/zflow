//! `Shell_NotifyIcon` on a real window: `TaskbarCreated` skips message-only
//! windows.

use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use async_channel::Sender;
use parking_lot::Mutex;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::CreateBitmap;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CW_USEDEFAULT, CreateIconIndirect, CreatePopupMenu, CreateWindowExW,
    DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos,
    GetMessageW, GetWindowLongPtrW, HICON, ICONINFO, MF_CHECKED, MF_GRAYED, MF_SEPARATOR,
    MF_STRING, MSG, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
    SetForegroundWindow, SetWindowLongPtrW, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_DESTROY, WM_LBUTTONUP,
    WM_NCCREATE, WM_NCDESTROY, WM_RBUTTONUP, WNDCLASSW, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use super::{Menu, MenuEntry, TrayEvent, TrayIcon, TrayOptions, decode, send};

// `WM_APP`, not `WM_USER`: that range is private to controls.
const WM_TRAY: u32 = WM_APP + 1;
const WM_LOOK: u32 = WM_APP + 2;
const MENU_ACTION_BASE: usize = 1;
const TRAY_ICON_ID: u32 = 1;

/// A live notification icon. Dropping it removes the icon and ends the tray
/// thread.
pub(super) struct NotifyIcon {
    hwnd: isize,
    /// The next look, picked up by the tray thread on `WM_LOOK`.
    look: std::sync::Arc<Mutex<Option<(String, TrayIcon)>>>,
    thread: Option<JoinHandle<()>>,
}

// SAFETY: the handle is only ever turned back into an `HWND` for
// `PostMessageW`, which is documented cross-thread.
#[allow(
    unsafe_code,
    reason = "a Send assertion for a raw handle has no safe form"
)]
unsafe impl Send for NotifyIcon {}

impl NotifyIcon {
    #[allow(unsafe_code, reason = "posting a look change to the owning thread")]
    pub(super) fn set_look(&self, tooltip: &str, icon: &TrayIcon) {
        *self.look.lock() = Some((tooltip.to_owned(), icon.clone()));
        // SAFETY: the window lives until Drop and PostMessageW supports other threads.
        unsafe {
            let _ = PostMessageW(Some(HWND(self.hwnd as _)), WM_LOOK, WPARAM(0), LPARAM(0));
        }
    }
}

impl Drop for NotifyIcon {
    fn drop(&mut self) {
        #[allow(
            unsafe_code,
            reason = "no safe binding posts to a foreign thread's window"
        )]
        // SAFETY: the window outlives every drop path: it is destroyed only by
        // this message, and the join below outwaits the teardown.
        unsafe {
            let _ = PostMessageW(Some(HWND(self.hwnd as _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct TrayState {
    events: Sender<TrayEvent>,
    taskbar_created: u32,
    icon: HICON,
    tooltip: String,
    menu: Menu,
    menu_on_click: bool,
    look: std::sync::Arc<Mutex<Option<(String, TrayIcon)>>>,
}

impl TrayState {
    fn notify_icon_data(&self, hwnd: HWND) -> NOTIFYICONDATAW {
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: TRAY_ICON_ID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: self.icon,
            ..Default::default()
        };
        for (slot, value) in data
            .szTip
            .iter_mut()
            .take(127)
            .zip(self.tooltip.encode_utf16())
        {
            *slot = value;
        }
        data
    }
}

pub(super) fn spawn(
    options: TrayOptions,
    menu: Menu,
    events: Sender<TrayEvent>,
) -> Option<NotifyIcon> {
    let (ready_sender, ready) = mpsc::channel();
    let look = std::sync::Arc::new(Mutex::new(None));
    let shared = look.clone();
    let thread = thread::Builder::new()
        .name("zz-tray".into())
        .spawn(move || pump(options, menu, events, shared, &ready_sender))
        .ok()?;
    if let Ok(Some(hwnd)) = ready.recv() {
        Some(NotifyIcon {
            hwnd,
            look,
            thread: Some(thread),
        })
    } else {
        let _ = thread.join();
        None
    }
}

#[allow(
    unsafe_code,
    reason = "win32 window and shell-icon calls have no safe binding"
)]
fn pump(
    options: TrayOptions,
    menu: Menu,
    events: Sender<TrayEvent>,
    look: std::sync::Arc<Mutex<Option<(String, TrayIcon)>>>,
    ready: &mpsc::Sender<Option<isize>>,
) {
    // SAFETY: plain win32 setup on this thread's own windows; the state Box
    // handed to `WM_NCCREATE` is reclaimed in `WM_NCDESTROY` below.
    let created = unsafe {
        let instance = match GetModuleHandleW(None) {
            Ok(instance) => instance,
            Err(error) => {
                log::warn!(target: "zz::tray", "no module handle for the tray window: {error}");
                let _ = ready.send(None);
                return;
            }
        };
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_procedure),
            hInstance: instance.into(),
            lpszClassName: w!("zz-tray"),
            ..Default::default()
        };
        // A zero atom means the class already exists, which is not fatal.
        RegisterClassW(&raw const class);

        let state = Box::new(TrayState {
            events,
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
            icon: tray_icon(&options.icon),
            tooltip: options.tooltip,
            menu,
            menu_on_click: options.menu_on_click,
            look,
        });
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("zz-tray"),
            PCWSTR::null(),
            WS_POPUP,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            Some(Box::into_raw(state) as *const _),
        )
    };
    let hwnd = match created {
        Ok(hwnd) => hwnd,
        Err(error) => {
            log::warn!(target: "zz::tray", "could not create the tray window: {error}");
            let _ = ready.send(None);
            return;
        }
    };

    // SAFETY: `hwnd` was created just above on this thread and `state` was
    // stashed by `WM_NCCREATE`.
    unsafe {
        let state =
            &*(GetWindowLongPtrW(hwnd, windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA)
                as *const TrayState);
        let added = Shell_NotifyIconW(NIM_ADD, &state.notify_icon_data(hwnd)).as_bool();
        send(&state.events, TrayEvent::Available(added));
        if !added {
            // A busy shell at sign-in can refuse it. The window stays, so the
            // icon comes back when the shell sends `TaskbarCreated`.
            log::warn!(target: "zz::tray", "the shell rejected the notification icon");
        }
    }
    let _ = ready.send(Some(hwnd.0 as isize));

    // SAFETY: the canonical pump, on the thread that owns the window.
    unsafe {
        let mut message = MSG::default();
        while GetMessageW(&raw mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&raw const message);
            DispatchMessageW(&raw const message);
        }
    }
}

#[allow(unsafe_code, reason = "a window procedure is unsafe by construction")]
unsafe extern "system" fn window_procedure(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{CREATESTRUCTW, GWLP_USERDATA};

    // SAFETY: `GWLP_USERDATA` only ever holds the `TrayState` box installed at
    // `WM_NCCREATE` and reclaimed at `WM_NCDESTROY`; between the two it is
    // this thread's alone.
    unsafe {
        if message == WM_NCCREATE {
            let create = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayState;
        if state.is_null() {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        if message == WM_NCDESTROY {
            let state = Box::from_raw(state);
            if !state.icon.is_invalid() {
                let _ = DestroyIcon(state.icon);
            }
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        if message == WM_LOOK {
            let next = (*state).look.lock().take();
            if let Some((tooltip, icon)) = next {
                let old = (*state).icon;
                (*state).icon = tray_icon(&icon);
                (*state).tooltip = tooltip;
                let _ = Shell_NotifyIconW(NIM_MODIFY, &(*state).notify_icon_data(hwnd));
                if !old.is_invalid() {
                    let _ = DestroyIcon(old);
                }
            }
            return LRESULT(0);
        }
        let state = &*state;

        match message {
            WM_TRAY => match lparam.0 as u32 {
                WM_LBUTTONUP if state.menu_on_click => show_menu(hwnd, state),
                WM_LBUTTONUP => send(&state.events, TrayEvent::Activate),
                WM_RBUTTONUP => show_menu(hwnd, state),
                _ => {}
            },
            WM_CLOSE => {
                let _ = Shell_NotifyIconW(NIM_DELETE, &state.notify_icon_data(hwnd));
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                return LRESULT(0);
            }
            message if message == state.taskbar_created => {
                let available = Shell_NotifyIconW(NIM_ADD, &state.notify_icon_data(hwnd)).as_bool();
                send(&state.events, TrayEvent::Available(available));
                if !available {
                    log::warn!(target: "zz::tray", "could not restore the icon after a shell restart");
                }
            }
            _ => {}
        }
        DefWindowProcW(hwnd, message, wparam, lparam)
    }
}

#[allow(unsafe_code, reason = "menus have no safe binding")]
fn show_menu(hwnd: HWND, state: &TrayState) {
    // SAFETY: menu creation and tracking on the window's own thread.
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        let mut actions = Vec::new();
        for entry in state.menu.entries() {
            match entry {
                MenuEntry::Separator => {
                    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                }
                MenuEntry::Item {
                    label,
                    action,
                    checked,
                } => {
                    let label: Vec<u16> = label
                        .replace('&', "&&")
                        .encode_utf16()
                        .chain(Some(0))
                        .collect();
                    let mut flags = MF_STRING;
                    if action.is_none() {
                        flags |= MF_GRAYED;
                    }
                    if checked == Some(true) {
                        flags |= MF_CHECKED;
                    }
                    let _ = AppendMenuW(
                        menu,
                        flags,
                        MENU_ACTION_BASE + actions.len(),
                        PCWSTR(label.as_ptr()),
                    );
                    actions.push(action);
                }
            }
        }

        let mut cursor = windows::Win32::Foundation::POINT::default();
        let _ = GetCursorPos(&raw mut cursor);
        // Without this the notification-icon menu cannot be dismissed by
        // clicking away.
        let _ = SetForegroundWindow(hwnd);
        let picked = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
            cursor.x,
            cursor.y,
            None,
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
        if let Some(index) = (picked.0 as usize).checked_sub(MENU_ACTION_BASE)
            && let Some(Some(action)) = actions.get(index)
        {
            send(&state.events, TrayEvent::Action(action.clone()));
        }
    }
}

// `CreateIconIndirect` requires a mask bitmap but ignores it at 32bpp.
#[allow(unsafe_code, reason = "GDI bitmaps have no safe binding")]
fn tray_icon(icon: &TrayIcon) -> HICON {
    let Some(rgba) = decode(icon) else {
        return HICON::default();
    };
    let (width, height) = rgba.dimensions();
    let bgra: Vec<u8> = rgba
        .pixels()
        .flat_map(|pixel| {
            let [r, g, b, a] = pixel.0;
            [b, g, r, a]
        })
        .collect();
    let mask = vec![0xffu8; (width as usize).div_ceil(8) * height as usize];

    // SAFETY: both bitmaps are built from exactly-sized local buffers and
    // released after `CreateIconIndirect` copies them.
    unsafe {
        let color = CreateBitmap(
            width as i32,
            height as i32,
            1,
            32,
            Some(bgra.as_ptr().cast()),
        );
        let monochrome = CreateBitmap(
            width as i32,
            height as i32,
            1,
            1,
            Some(mask.as_ptr().cast()),
        );
        let info = ICONINFO {
            fIcon: true.into(),
            hbmMask: monochrome,
            hbmColor: color,
            ..Default::default()
        };
        let icon = CreateIconIndirect(&raw const info).unwrap_or_default();
        let _ = windows::Win32::Graphics::Gdi::DeleteObject(color.into());
        let _ = windows::Win32::Graphics::Gdi::DeleteObject(monochrome.into());
        icon
    }
}
