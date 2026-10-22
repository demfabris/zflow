#![allow(
    unsafe_code,
    reason = "the hot key is Carbon's and the key monitor AppKit's; neither has a safe binding"
)]

use std::{
    collections::HashMap,
    ffi::c_void,
    ptr::NonNull,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use block2::RcBlock;
use objc2::{MainThreadMarker, rc::Retained, runtime::AnyObject};
use objc2_app_kit::{NSBeep, NSEvent, NSEventMask, NSEventModifierFlags, NSEventType, NSScreen};
use zflow::{
    app::api::Request,
    lens::viewer::{CHUNK, Input, State, Viewer},
};
use zz_gpui::{
    App, AppContext as _, Bounds, Context, Entity, InteractiveElement as _, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, Point,
    Render, RenderImage, ScrollDelta, ScrollWheelEvent, SharedString, Styled as _, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions, div, img,
    point, px, size,
};

use super::geometry::{Fit, Grid, KeyAction, button, key_down, modifier_down, scroll};
use crate::{copy, model::Model};

pub const HOTKEY_LABEL: &str = "⌃⌥L";
/// The circle's radius, in points.
const RADIUS: f64 = 200.0;
/// How long a lens may wait for its first answer.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a failure stays up before the lens closes.
const FAILURE_SHOWN: Duration = Duration::from_secs(2);

struct Lens {
    model: Entity<Model>,
    /// The computer looked at last, which the hot key opens again.
    last: Option<String>,
    window: Option<WindowHandle<LensView>>,
}

impl zz_gpui::Global for Lens {}

/// The computer the hot key opens: the last one looked at while it is still
/// linked, else the first linked one.
pub fn target(cx: &App) -> Option<String> {
    let lens = cx.try_global::<Lens>()?;
    let snapshot = lens.model.read(cx).snapshot.as_ref()?;
    let linked: Vec<&str> = snapshot
        .peers
        .iter()
        .filter(|peer| copy::linked(peer.state))
        .map(|peer| peer.name.as_str())
        .collect();
    lens.last
        .as_deref()
        .filter(|last| linked.contains(last))
        .or_else(|| linked.first().copied())
        .map(str::to_owned)
}

pub fn init(model: &Entity<Model>, cx: &mut App) {
    cx.set_global(Lens {
        model: model.clone(),
        last: None,
        window: None,
    });
    let (pressed, hits) = async_channel::unbounded();
    if register_hot_key(pressed) {
        cx.spawn(async move |cx| {
            while hits.recv().await.is_ok() {
                let () = cx.update(toggle);
            }
        })
        .detach();
    } else {
        log::warn!("the lens hot key {HOTKEY_LABEL} is taken");
    }
}

fn toggle(cx: &mut App) {
    if close(cx) {
        return;
    }
    match target(cx) {
        Some(name) => look(&name, cx),
        None => NSBeep(),
    }
}

/// Closes an open lens. Returns whether one was open.
fn close(cx: &mut App) -> bool {
    let Some(window) = cx.try_global::<Lens>().and_then(|lens| lens.window) else {
        return false;
    };
    cx.global_mut::<Lens>().window = None;
    window
        .update(cx, |view, window, _| {
            view.end();
            window.remove_window();
        })
        .is_ok()
}

/// Opens a lens onto computer `name` on the screen under the pointer.
pub fn look(name: &str, cx: &mut App) {
    close(cx);
    let Some(lens) = cx.try_global::<Lens>() else {
        return;
    };
    let model = lens.model.clone();
    let Some(viewer) = model.read(cx).lens.clone() else {
        NSBeep();
        return;
    };
    let Some(screen) = screen_under_pointer() else {
        return;
    };
    let answer = model.read(cx).attempt(Request::Lens {
        name: name.to_owned(),
    });
    let name = name.to_owned();
    cx.global_mut::<Lens>().last = Some(name.clone());
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(screen)),
            titlebar: None,
            kind: WindowKind::PopUp,
            focus: true,
            show: true,
            is_movable: false,
            window_background: WindowBackgroundAppearance::Transparent,
            ..Default::default()
        },
        |window, cx| {
            let view = cx.new(|cx| LensView::new(name, viewer, screen, window, cx));
            let pending = view.downgrade();
            cx.spawn(async move |cx| {
                let failure = answer.recv().await.ok().and_then(Result::err);
                let _ = pending.update(cx, |view, cx| view.answered(failure, cx));
            })
            .detach();
            view
        },
    );
    match opened {
        Ok(window) => cx.global_mut::<Lens>().window = Some(window),
        Err(error) => log::warn!("could not open the lens: {error:#}"),
    }
}

/// The frame of the screen under the pointer, in the window system's
/// top-left coordinates.
fn screen_under_pointer() -> Option<Bounds<Pixels>> {
    let mtm = MainThreadMarker::new()?;
    let mouse = NSEvent::mouseLocation();
    let screens = NSScreen::screens(mtm);
    let primary = screens.firstObject()?.frame();
    let frame = screens
        .iter()
        .map(|screen| screen.frame())
        .find(|frame| {
            mouse.x >= frame.origin.x
                && mouse.x < frame.origin.x + frame.size.width
                && mouse.y >= frame.origin.y
                && mouse.y < frame.origin.y + frame.size.height
        })
        .unwrap_or(primary);
    let top = primary.size.height - (frame.origin.y + frame.size.height);
    Some(Bounds::new(
        point(px(frame.origin.x as f32), px(top as f32)),
        size(px(frame.size.width as f32), px(frame.size.height as f32)),
    ))
}

/// What the key monitor tells the view.
enum KeyNote {
    Close,
}

struct LensView {
    name: String,
    viewer: Arc<Viewer>,
    screen: (f64, f64),
    opened: Instant,
    /// The engine answered the lens request.
    answered: bool,
    /// Why it failed, shown before it closes.
    failure: Option<(String, Instant)>,
    fit: Option<Fit>,
    grid: Option<Grid>,
    chunks: HashMap<u32, Arc<RenderImage>>,
    pointer: (f64, f64),
    shown: (u64, (f64, f64)),
    monitor: Option<Retained<AnyObject>>,
    ended: bool,
}

impl LensView {
    fn new(
        name: String,
        viewer: Arc<Viewer>,
        screen: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (notes, inbox) = async_channel::unbounded();
        let monitor = key_monitor(viewer.clone(), notes);
        cx.spawn(async move |this, cx| {
            while let Ok(KeyNote::Close) = inbox.recv().await {
                let _ = this.update(cx, |_, cx| {
                    cx.defer(|cx| {
                        close(cx);
                    });
                });
            }
        })
        .detach();
        // Another app in front, or zflow's own window, ends the lens.
        cx.observe_window_activation(window, |view, window, cx| {
            if !window.is_window_active() && view.answered && !view.ended {
                cx.defer(|cx| {
                    close(cx);
                });
            }
        })
        .detach();
        Self {
            name,
            viewer,
            screen: (
                f64::from(f32::from(screen.size.width)),
                f64::from(f32::from(screen.size.height)),
            ),
            opened: Instant::now(),
            answered: false,
            failure: None,
            fit: None,
            grid: None,
            chunks: HashMap::new(),
            pointer: (
                f64::from(f32::from(screen.size.width)) / 2.0,
                f64::from(f32::from(screen.size.height)) / 2.0,
            ),
            shown: (u64::MAX, (0.0, 0.0)),
            monitor,
            ended: false,
        }
    }

    fn answered(&mut self, failure: Option<String>, cx: &mut Context<Self>) {
        self.answered = true;
        if let Some(failure) = failure {
            self.fail(failure);
        }
        cx.notify();
    }

    fn fail(&mut self, message: String) {
        if self.failure.is_none() {
            self.failure = Some((message, Instant::now()));
        }
    }

    /// Tells the other computer to let go of everything the lens pressed,
    /// and stops reading keys.
    fn end(&mut self) {
        if self.ended {
            return;
        }
        self.ended = true;
        self.viewer.send(Input::Close);
        if let Some(monitor) = self.monitor.take() {
            // SAFETY: the monitor `addLocalMonitorForEventsMatchingMask:` returned.
            unsafe { NSEvent::removeMonitor(&monitor) };
        }
    }

    /// Follows the engine each frame: opening, open with new pixels, or
    /// failed.
    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((_, since)) = &self.failure {
            if since.elapsed() >= FAILURE_SHOWN {
                cx.defer(|cx| {
                    close(cx);
                });
            }
            return;
        }
        let status = self.viewer.status();
        match status.state {
            State::Opening => {}
            State::Open => self.pull(status.width, status.height, status.frames, window),
            State::Failed => {
                let message = self
                    .viewer
                    .error()
                    .unwrap_or_else(|| format!("The lens onto {} closed", self.name));
                self.fail(message);
            }
            State::Closed => {
                if self.fit.is_some() {
                    cx.defer(|cx| {
                        close(cx);
                    });
                } else if self.opened.elapsed() > ANSWER_TIMEOUT {
                    self.fail(format!("{} did not answer", self.name));
                }
            }
        }
    }

    /// Copies the chunks that changed under the circle.
    fn pull(&mut self, width: u32, height: u32, frames: u64, window: &mut Window) {
        if self.fit.is_none_or(|fit| fit.desktop != (width, height)) {
            self.fit = Fit::new((width, height), self.screen);
            self.grid = Some(Grid::new((width, height), CHUNK as u32));
            for (_, image) in self.chunks.drain() {
                let _ = window.drop_image(image);
            }
            self.send_move();
        }
        let Some(fit) = self.fit else { return };
        let center = fit.pixel(self.pointer.0, self.pointer.1);
        if self.shown == (frames, center) {
            return;
        }
        self.shown = (frames, center);
        let radius = fit.pixels(RADIUS);
        let mut ids = vec![0u32; 256];
        let count = self.viewer.dirty(center.0, center.1, radius, &mut ids);
        let mut buffer = vec![0u8; CHUNK * CHUNK * 4];
        for &id in &ids[..count] {
            let Some((w, h)) = self.viewer.chunk(id as usize, &mut buffer, CHUNK * 4) else {
                continue;
            };
            let mut pixels = Vec::with_capacity(w * h * 4);
            for row in 0..h {
                pixels.extend_from_slice(&buffer[row * CHUNK * 4..row * CHUNK * 4 + w * 4]);
            }
            // BGRx: the padding byte becomes opaque alpha.
            for pixel in pixels.chunks_exact_mut(4) {
                pixel[3] = 255;
            }
            let Some(image) = image::RgbaImage::from_raw(w as u32, h as u32, pixels) else {
                continue;
            };
            let next = Arc::new(RenderImage::new([image::Frame::new(image)]));
            if let Some(old) = self.chunks.insert(id, next) {
                let _ = window.drop_image(old);
            }
        }
    }

    fn send_move(&self) {
        let Some(fit) = self.fit else { return };
        let (x, y) = fit.pixel(self.pointer.0, self.pointer.1);
        self.viewer.send(Input::Move {
            x,
            y,
            radius: fit.pixels(RADIUS),
        });
    }

    fn inside(&self, x: f64, y: f64) -> bool {
        (x - self.pointer.0).hypot(y - self.pointer.1) <= RADIUS
    }

    fn moved(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.pointer = (
            f64::from(f32::from(position.x)),
            f64::from(f32::from(position.y)),
        );
        self.send_move();
        cx.notify();
    }

    /// A left click outside the circle, or any click while it shows a
    /// failure, closes the lens. Other clicks go over.
    fn pressed(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let (x, y) = (
            f64::from(f32::from(event.position.x)),
            f64::from(f32::from(event.position.y)),
        );
        if self.failure.is_some() || (event.button == MouseButton::Left && !self.inside(x, y)) {
            cx.defer(|cx| {
                close(cx);
            });
            return;
        }
        if let Some(button) = button(mouse_number(event.button)) {
            self.viewer.send(Input::Button {
                button,
                pressed: true,
            });
        }
    }

    fn released(&mut self, event: &MouseUpEvent) {
        if let Some(button) = button(mouse_number(event.button)) {
            self.viewer.send(Input::Button {
                button,
                pressed: false,
            });
        }
    }

    fn scrolled(&mut self, event: &ScrollWheelEvent) {
        let (dx, dy, precise) = match event.delta {
            ScrollDelta::Pixels(delta) => (
                f64::from(f32::from(delta.x)),
                f64::from(f32::from(delta.y)),
                true,
            ),
            ScrollDelta::Lines(delta) => (f64::from(delta.x), f64::from(delta.y), false),
        };
        let (dx, dy) = scroll(dx, dy, precise);
        self.viewer.send(Input::Scroll { dx, dy });
    }
}

fn mouse_number(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::Navigate(zz_gpui::NavigationDirection::Back) => 3,
        MouseButton::Navigate(zz_gpui::NavigationDirection::Forward) => 4,
    }
}

impl Render for LensView {
    #[allow(
        clippy::disallowed_methods,
        reason = "the lens shows another computer's screen inside a fixed dark well with a white ring, the same in every theme"
    )]
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.tick(window, cx);
        if !self.ended {
            window.request_animation_frame();
        }
        let (x, y) = self.pointer;
        let diameter = (RADIUS * 2.0) as f32;
        let left = (x - RADIUS) as f32;
        let top = (y - RADIUS) as f32;
        let message: Option<SharedString> = match (&self.failure, self.fit) {
            (Some((failure, _)), _) => Some(failure.clone().into()),
            (None, None) => Some(format!("Opening {}…", self.name).into()),
            (None, Some(_)) => None,
        };
        let desktop = self.fit.zip(self.grid).map(|(fit, grid)| {
            div()
                .absolute()
                .left(px((fit.x as f32) - left))
                .top(px((fit.y as f32) - top))
                .w(px(fit.width as f32))
                .h(px(fit.height as f32))
                .children(self.chunks.iter().map(|(id, image)| {
                    let (cx_, cy, w, h) = grid.rect(*id);
                    img(image.clone())
                        .absolute()
                        .left(px((f64::from(cx_) * fit.scale) as f32))
                        .top(px((f64::from(cy) * fit.scale) as f32))
                        .w(px((f64::from(w) * fit.scale) as f32))
                        .h(px((f64::from(h) * fit.scale) as f32))
                }))
        });
        div()
            .id("lens")
            .size_full()
            .on_mouse_move(cx.listener(|view, event: &MouseMoveEvent, _, cx| {
                view.moved(event.position, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, event, _, cx| view.pressed(event, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|view, event, _, cx| view.pressed(event, cx)),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|view, event, _, cx| view.pressed(event, cx)),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, event, _, _| view.released(event)),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|view, event, _, _| view.released(event)),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|view, event, _, _| view.released(event)),
            )
            .on_scroll_wheel(cx.listener(|view, event, _, _| view.scrolled(event)))
            .child(
                div()
                    .absolute()
                    .left(px(left))
                    .top(px(top))
                    .size(px(diameter))
                    .rounded_full()
                    .overflow_hidden()
                    .bg(zz_gpui::rgba(0x1414_14ff))
                    .border(px(1.5))
                    .border_color(zz_gpui::rgba(0xffff_ffd9))
                    .children(desktop)
                    .children(message.map(|message| {
                        div()
                            .absolute()
                            .left(px(RADIUS as f32 * 0.25))
                            .w(px(RADIUS as f32 * 1.5))
                            .top(px(RADIUS as f32 * 2.0 - 64.0))
                            .flex()
                            .justify_center()
                            .child(
                                div()
                                    .px(px(10.0))
                                    .py(px(6.0))
                                    .rounded(px(6.0))
                                    .bg(zz_gpui::rgba(0x0000_00b3))
                                    .text_color(zz_gpui::rgba(0xffff_ffff))
                                    .text_size(px(13.0))
                                    .font_weight(zz_gpui::FontWeight::MEDIUM)
                                    .text_center()
                                    .child(message),
                            )
                    })),
            )
    }
}

impl Drop for LensView {
    fn drop(&mut self) {
        self.end();
    }
}

/// Reads keys while the lens is up, before the rest of the app sees them:
/// Escape closes, Command-C copies the other computer's selection, and every
/// other key, modifiers by side, goes over.
fn key_monitor(
    viewer: Arc<Viewer>,
    notes: async_channel::Sender<KeyNote>,
) -> Option<Retained<AnyObject>> {
    let swallowed = std::cell::Cell::new(None::<u16>);
    let block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: AppKit hands the monitor a live event for this call.
        let event = unsafe { event.as_ref() };
        let code = event.keyCode();
        let flags = event.modifierFlags();
        let send = |pressed: bool| {
            if let Some(usage) = zflow::macos::mac_keycode_to_hid(code) {
                viewer.send(Input::Key { usage, pressed });
            }
        };
        match event.r#type() {
            NSEventType::KeyDown => {
                let command_only = flags.intersection(
                    NSEventModifierFlags::Command
                        | NSEventModifierFlags::Shift
                        | NSEventModifierFlags::Option
                        | NSEventModifierFlags::Control,
                ) == NSEventModifierFlags::Command;
                let character = event
                    .charactersIgnoringModifiers()
                    .map(|text| text.to_string());
                match key_down(code, command_only, character.as_deref()) {
                    KeyAction::Close => {
                        let _ = notes.try_send(KeyNote::Close);
                    }
                    KeyAction::Copy => {
                        swallowed.set(Some(code));
                        viewer.send(Input::Copy);
                    }
                    KeyAction::Send => send(true),
                }
            }
            NSEventType::KeyUp => {
                if swallowed.get() == Some(code) {
                    swallowed.set(None);
                } else if code != 53 {
                    send(false);
                }
            }
            NSEventType::FlagsChanged => {
                const CAPS_LOCK: u16 = 57;
                if code == CAPS_LOCK {
                    send(true);
                    send(false);
                } else if let Some(down) = modifier_down(code, flags.0 as u64) {
                    send(down);
                }
            }
            _ => return std::ptr::from_ref(event).cast_mut(),
        }
        std::ptr::null_mut()
    });
    // SAFETY: the block lives as long as AppKit keeps it, and returns nil
    // for every key it takes.
    unsafe {
        NSEvent::addLocalMonitorForEventsMatchingMask_handler(
            NSEventMask::KeyDown | NSEventMask::KeyUp | NSEventMask::FlagsChanged,
            &block,
        )
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EventHotKeyId {
    signature: u32,
    id: u32,
}

#[repr(C)]
struct EventTypeSpec {
    class: u32,
    kind: u32,
}

type Handler = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> *mut c_void;
    fn InstallEventHandler(
        target: *mut c_void,
        handler: Handler,
        count: u32,
        types: *const EventTypeSpec,
        user_data: *mut c_void,
        out: *mut *mut c_void,
    ) -> i32;
    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        id: EventHotKeyId,
        target: *mut c_void,
        options: u32,
        out: *mut *mut c_void,
    ) -> i32;
}

const fn four(code: [u8; 4]) -> u32 {
    u32::from_be_bytes(code)
}

static PRESSED: OnceLock<async_channel::Sender<()>> = OnceLock::new();

extern "C" fn hot_key_pressed(_: *mut c_void, _: *mut c_void, _: *mut c_void) -> i32 {
    if let Some(pressed) = PRESSED.get() {
        let _ = pressed.try_send(());
    }
    0
}

/// ⌃⌥L, through Carbon's hot keys, which need no Accessibility permission.
fn register_hot_key(pressed: async_channel::Sender<()>) -> bool {
    const KEY_L: u32 = 0x25;
    const CONTROL: u32 = 1 << 12;
    const OPTION: u32 = 1 << 11;
    const KEYBOARD: u32 = four(*b"keyb");
    const HOT_KEY_PRESSED: u32 = 5;
    if PRESSED.set(pressed).is_err() {
        return false;
    }
    let spec = EventTypeSpec {
        class: KEYBOARD,
        kind: HOT_KEY_PRESSED,
    };
    let mut handler = std::ptr::null_mut();
    let mut hot_key = std::ptr::null_mut();
    // SAFETY: a static handler on the application target, and a hot key
    // registered for the life of the app.
    unsafe {
        let target = GetApplicationEventTarget();
        InstallEventHandler(
            target,
            hot_key_pressed,
            1,
            &raw const spec,
            std::ptr::null_mut(),
            &raw mut handler,
        ) == 0
            && RegisterEventHotKey(
                KEY_L,
                CONTROL | OPTION,
                EventHotKeyId {
                    signature: four(*b"zflw"),
                    id: 1,
                },
                target,
                0,
                &raw mut hot_key,
            ) == 0
    }
}
