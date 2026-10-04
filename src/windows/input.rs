use crate::{
    capture::{CaptureFrame, CaptureTransition, KeyState},
    core::{HidUsage, KeyRemap, KeyboardMode, MotionDelta, PointerButton, ReceiverEffect},
    desktop::{Display, Edge, FRACTION_MAX, Geometry, Point, Rect},
};
use anyhow::{Result, bail, ensure};
use std::{
    collections::BTreeSet,
    ffi::c_void,
    sync::OnceLock,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

unsafe extern "C" {
    fn zflow_input_run(callback: extern "C" fn(i32, i32, i32, i32) -> i32) -> i32;
    fn zflow_input_remote(enabled: i32);
    fn zflow_input_is_remote() -> i32;
    fn zflow_input_grab() -> i32;
    fn zflow_input_pulse();
    fn zflow_input_boundaries(items: *const Boundary, count: i32);
    fn zflow_input_stop(thread: u32);
    fn zflow_input_clean() -> i32;
    fn zflow_input_elevated() -> i32;
    fn zflow_desktop_available() -> i32;
    fn zflow_input_post(kind: i32, code: i32, value: i32, extra: i32) -> i32;
    fn zflow_monitors(
        callback: extern "C" fn(
            i32,
            i32,
            i32,
            i32,
            *const std::ffi::c_char,
            *const std::ffi::c_char,
            u32,
            u32,
            *mut c_void,
        ),
        context: *mut c_void,
    ) -> i32;
    fn zflow_cursor(x: *mut i32, y: *mut i32, move_to: i32) -> i32;
}

pub enum Event {
    Ready(u32),
    Frame(CaptureFrame),
    Escape,
    Activate,
    EdgeHit(Point, Edge),
    Stopped,
}
static EVENTS: OnceLock<mpsc::Sender<Event>> = OnceLock::new();
extern "C" fn event(kind: i32, code: i32, value: i32, extra: i32) -> i32 {
    let state = if value != 0 {
        KeyState::Pressed
    } else {
        KeyState::Released
    };
    let mut frame = CaptureFrame {
        event_count: 1,
        ..Default::default()
    };
    let event = match kind {
        0 => Event::Ready(code as u32),
        1 => {
            let Some(usage) =
                super::keys::captured(code as u16, extra & 1 != 0, (extra >> 8) as u16)
            else {
                return 1;
            };
            frame
                .transitions
                .push(CaptureTransition::Key { usage, state });
            Event::Frame(frame)
        }
        2 => {
            frame.transitions.push(CaptureTransition::Button {
                button: PointerButton(code as u16),
                state,
            });
            Event::Frame(frame)
        }
        3 => {
            frame.motion = MotionDelta {
                dx: code.into(),
                dy: value.into(),
                ..Default::default()
            };
            Event::Frame(frame)
        }
        4 => {
            frame.motion = MotionDelta {
                scroll_x: code.into(),
                scroll_y: value.into(),
                ..Default::default()
            };
            Event::Frame(frame)
        }
        5 => Event::Escape,
        6 => Event::Activate,
        7 => Event::EdgeHit(
            Point { x: code, y: value },
            match extra {
                0 => Edge::Left,
                1 => Edge::Right,
                2 => Edge::Top,
                3 => Edge::Bottom,
                _ => return 1,
            },
        ),
        _ => return 1,
    };
    i32::from(EVENTS.get().is_some_and(|tx| tx.try_send(event).is_ok()))
}

pub struct Capture {
    thread: u32,
    join: Option<std::thread::JoinHandle<()>>,
}
impl Capture {
    pub async fn start() -> Result<(Self, mpsc::Receiver<Event>)> {
        let (tx, mut rx) = mpsc::channel(2048);
        EVENTS
            .set(tx.clone())
            .map_err(|_| anyhow::anyhow!("Input capture already started"))?;
        let join = std::thread::spawn(move || {
            // SAFETY: callback is a static function; the adapter owns all hooks.
            unsafe {
                zflow_input_run(event);
            }
            let _ = tx.blocking_send(Event::Stopped);
        });
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await? {
            Some(Event::Ready(thread)) => Ok((
                Self {
                    thread,
                    join: Some(join),
                },
                rx,
            )),
            _ => bail!("Windows could not install the input hooks or Raw Input window"),
        }
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        remote(false);
        // SAFETY: thread identifies the message loop owned by this guard.
        unsafe {
            zflow_input_stop(self.thread);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
pub fn remote(enabled: bool) {
    unsafe {
        zflow_input_remote(i32::from(enabled));
    }
}
pub fn is_remote() -> bool {
    unsafe { zflow_input_is_remote() != 0 }
}
pub fn grab() -> Result<()> {
    ensure!(
        unsafe { zflow_input_grab() } != 0,
        "Release all keys and mouse buttons before switching"
    );
    Ok(())
}
pub fn pulse() {
    unsafe {
        zflow_input_pulse();
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Boundary {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    edge: i32,
    start: i32,
    end: i32,
    returning: i32,
}
impl Boundary {
    pub fn new(bounds: Rect, edge: Edge, start: u32, end: u32, returning: bool) -> Option<Self> {
        if start >= end || end > FRACTION_MAX {
            return None;
        }
        let (origin, span) = match edge {
            Edge::Left | Edge::Right => (bounds.y, bounds.height),
            _ => (bounds.x, bounds.width),
        };
        let start = (u64::from(start) * u64::from(span)).div_ceil(u64::from(FRACTION_MAX)) as i32;
        let end = if returning {
            (u64::from(end) * u64::from(span) / u64::from(FRACTION_MAX)) as i32 + 1
        } else {
            (u64::from(end) * u64::from(span)).div_ceil(u64::from(FRACTION_MAX)) as i32
        };
        let margin = if returning { 0 } else { 8 };
        let start = start.max(margin);
        let end = end.min(span as i32 - margin);
        (start < end).then_some(Self {
            left: bounds.x,
            top: bounds.y,
            right: bounds.x + bounds.width as i32,
            bottom: bounds.y + bounds.height as i32,
            edge: match edge {
                Edge::Left => 0,
                Edge::Right => 1,
                Edge::Top => 2,
                Edge::Bottom => 3,
            },
            start: origin + start,
            end: origin + end,
            returning: i32::from(returning),
        })
    }
}
pub fn boundaries(items: &[Boundary]) {
    // SAFETY: C copies these bounded descriptors before returning.
    unsafe {
        zflow_input_boundaries(items.as_ptr(), items.len() as i32);
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    unsafe extern "C" {
        fn zflow_boundary_hit(
            boundary: *const Boundary,
            x: i32,
            y: i32,
            nx: i32,
            ny: i32,
            hx: *mut i32,
            hy: *mut i32,
        ) -> i32;
    }
    fn hit(b: &Boundary, from: (i32, i32), to: (i32, i32)) -> Option<(i32, i32)> {
        let (mut x, mut y) = (0, 0);
        (unsafe { zflow_boundary_hit(b, from.0, from.1, to.0, to.1, &mut x, &mut y) } != 0)
            .then_some((x, y))
    }
    #[test]
    fn native_guard_catches_fast_internal_crossing_and_leaves_other_motion_alone() {
        let b = Boundary::new(
            Rect {
                x: 0,
                y: 0,
                width: 3840,
                height: 2160,
            },
            Edge::Right,
            250_000,
            750_000,
            false,
        )
        .unwrap();
        assert_eq!(hit(&b, (3800, 1000), (4100, 1300)), Some((3839, 1039)));
        assert_eq!(
            hit(&b, (3839, 1000), (3920, 1100)),
            Some((3839, 1100)),
            "a held edge still allows sliding"
        );
        assert_eq!(
            hit(&b, (3839, 1000), (3810, 1000)),
            None,
            "reversing stays local"
        );
        assert_eq!(
            hit(&b, (3800, 200), (4100, 200)),
            None,
            "outside the configured edge remains native"
        );
        assert_eq!(
            hit(&b, (4100, 1000), (4200, 1000)),
            None,
            "other monitor cannot trigger this edge"
        );
        let full = Boundary::new(
            Rect {
                x: 0,
                y: 0,
                width: 3840,
                height: 2160,
            },
            Edge::Right,
            0,
            FRACTION_MAX,
            false,
        )
        .unwrap();
        assert_eq!(
            hit(&full, (3830, 4), (4000, 4)),
            None,
            "dead corner stays local"
        );
    }
    #[test]
    fn native_guards_cover_all_directions_and_negative_origins() {
        let rect = Rect {
            x: -1920,
            y: -1080,
            width: 1920,
            height: 1080,
        };
        for (edge, from, to, expected) in [
            (Edge::Left, (-1800, -540), (-2100, -540), (-1920, -540)),
            (Edge::Right, (-100, -540), (200, -540), (-1, -540)),
            (Edge::Top, (-960, -1000), (-960, -1300), (-960, -1080)),
            (Edge::Bottom, (-960, -100), (-960, 200), (-960, -1)),
        ] {
            let b = Boundary::new(rect, edge, 0, FRACTION_MAX, false).unwrap();
            assert_eq!(hit(&b, from, to), Some(expected));
        }
    }
    #[test]
    fn incoming_guard_stops_at_selected_monitor_including_partial_range_end() {
        let b = Boundary::new(
            Rect {
                x: 3840,
                y: 0,
                width: 3840,
                height: 2160,
            },
            Edge::Left,
            250_000,
            750_000,
            true,
        )
        .unwrap();
        assert_eq!(hit(&b, (3900, 1620), (3700, 1620)), Some((3840, 1620)));
        assert_eq!(hit(&b, (3900, 1621), (3700, 1621)), None);
        assert_eq!(hit(&b, (3900, 539), (3700, 539)), None);
        assert!(
            Boundary::new(
                Rect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080
                },
                Edge::Left,
                800_000,
                500_000,
                false
            )
            .is_none()
        );
    }

    #[tokio::test]
    #[ignore = "moves and restores the real pointer; pause other zflow instances first"]
    async fn native_return_guard_stops_injected_motion_at_an_internal_edge() {
        let geometry = geometry().unwrap();
        // Place the boundary inside a real screen so OS desktop clamping cannot
        // hide a missed interception. This also works with one active monitor.
        let mut source = geometry.monitors[0];
        source.width /= 2;
        let original = cursor().unwrap();
        let (_capture, _events) = Capture::start().await.unwrap();
        struct Restore(Point);
        impl Drop for Restore {
            fn drop(&mut self) {
                boundaries(&[]);
                let _ = move_to(self.0);
            }
        }
        let _restore = Restore(original);
        let start = Point {
            x: source.x + source.width as i32 - 40,
            y: source.y + source.height as i32 / 2,
        };
        move_to(start).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(cursor().unwrap(), start);
        assert!(
            clean(),
            "release all keys and buttons before running the probe"
        );
        boundaries(&[Boundary::new(source, Edge::Right, 0, FRACTION_MAX, true).unwrap()]);
        pulse();
        post(4, 200, 0, 0).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let actual = cursor().unwrap();
        eprintln!(
            "native return: from={start:?}, actual={actual:?}, expected_x={}",
            source.x + source.width as i32 - 1
        );
        assert_eq!(actual.x, source.x + source.width as i32 - 1);
        assert_eq!(actual.y, start.y);
        post(4, 200, 0, 0).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            cursor().unwrap(),
            actual,
            "a continued push stays at the edge"
        );
        post(4, -100, 0, 0).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            cursor().unwrap().x < actual.x,
            "reversing stays on this screen"
        );
    }
}
pub fn clean() -> bool {
    unsafe { zflow_input_clean() != 0 }
}
pub fn available() -> bool {
    unsafe { zflow_desktop_available() != 0 }
}
pub fn elevated() -> bool {
    unsafe { zflow_input_elevated() != 0 }
}
pub fn geometry() -> Result<Geometry> {
    extern "C" fn monitor(
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        id: *const std::ffi::c_char,
        name: *const std::ffi::c_char,
        width_mm: u32,
        height_mm: u32,
        context: *mut c_void,
    ) {
        use sha2::{Digest, Sha256};
        use std::ffi::CStr;
        // SAFETY: the bridge calls synchronously with live, terminated strings.
        let geometry = unsafe { &mut *context.cast::<Geometry>() };
        let bounds = Rect {
            x,
            y,
            width: width as u32,
            height: height as u32,
        };
        if geometry.monitors.contains(&bounds) {
            return;
        } // Mirrored surfaces share one cursor.
        geometry.monitors.push(bounds);
        geometry.displays.push(Display {
            id: format!(
                "{:x}",
                Sha256::digest(unsafe { CStr::from_ptr(id) }.to_bytes())
            ),
            name: unsafe { CStr::from_ptr(name) }
                .to_string_lossy()
                .chars()
                .filter(|c| !c.is_control())
                .take(32)
                .collect(),
            bounds,
            width_mm,
            height_mm,
            active: true,
        });
    }
    let mut geometry = Geometry {
        monitors: Vec::new(),
        displays: Vec::new(),
    };
    ensure!(
        unsafe { zflow_monitors(monitor, (&mut geometry as *mut Geometry).cast()) } != 0,
        "Could not enumerate Windows monitors"
    );
    geometry
        .monitors
        .sort_by_key(|r| (r.x, r.y, r.width, r.height));
    geometry.displays.sort_by(|a, b| a.id.cmp(&b.id));
    geometry.validate()?;
    Ok(geometry)
}
pub fn cursor() -> Result<Point> {
    let mut p = Point { x: 0, y: 0 };
    ensure!(
        unsafe { zflow_cursor(&mut p.x, &mut p.y, 0) } != 0,
        "Could not read the Windows pointer"
    );
    Ok(p)
}
pub fn move_to(mut p: Point) -> Result<()> {
    ensure!(
        unsafe { zflow_cursor(&mut p.x, &mut p.y, 1) } != 0,
        "Could not place the Windows pointer"
    );
    Ok(())
}

fn post(kind: i32, code: i32, value: i32, extra: i32) -> Result<()> {
    ensure!(
        unsafe { zflow_input_post(kind, code, value, extra) } != 0,
        "Windows blocked remote input. To control administrator windows, open zflow Settings and choose Restart as administrator. Windows sign-in and UAC prompts require local input."
    );
    Ok(())
}

pub struct Injector {
    post: fn(i32, i32, i32, i32) -> Result<()>,
    keys: BTreeSet<HidUsage>,
    buttons: BTreeSet<PointerButton>,
    remap: KeyRemap,
    repeat: Option<(HidUsage, Instant)>,
    repeat_delay: Duration,
    repeat_interval: Duration,
    reverse_scroll: bool,
    releasing: bool,
}
impl Injector {
    pub fn new() -> Self {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SPI_GETKEYBOARDDELAY, SPI_GETKEYBOARDSPEED, SystemParametersInfoW,
        };
        let (mut delay, mut speed) = (1u32, 31u32);
        unsafe {
            SystemParametersInfoW(SPI_GETKEYBOARDDELAY, 0, (&mut delay as *mut u32).cast(), 0);
            SystemParametersInfoW(SPI_GETKEYBOARDSPEED, 0, (&mut speed as *mut u32).cast(), 0);
        }
        Self {
            post,
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            remap: KeyRemap::new(KeyboardMode::Standard),
            repeat: None,
            repeat_delay: Duration::from_millis(250 * (u64::from(delay.min(3)) + 1)),
            repeat_interval: Duration::from_secs_f64(
                1.0 / (2.5 + 27.5 * f64::from(speed.min(31)) / 31.0),
            ),
            reverse_scroll: false,
            releasing: false,
        }
    }
    pub fn configure(&mut self, keyboard: KeyboardMode, reverse_scroll: bool) {
        self.release();
        self.remap.reset(keyboard);
        self.reverse_scroll = reverse_scroll;
    }
    fn key(&mut self, key: HidUsage, pressed: bool) -> Result<()> {
        if let Some((kind, code, extra)) = super::keys::posted(key) {
            (self.post)(kind, code, i32::from(pressed), extra)?;
            if pressed {
                self.keys.insert(key);
            } else {
                self.keys.remove(&key);
            }
            if pressed
                && key.page == crate::core::HidUsagePage::KEYBOARD_KEYPAD
                && (4..224).contains(&key.usage.0)
                && ![57, 71, 72, 83, 101].contains(&key.usage.0)
            {
                self.repeat = Some((key, Instant::now() + self.repeat_delay));
            } else if self.repeat.is_some_and(|(held, _)| held == key) {
                self.repeat = None;
            }
        }
        Ok(())
    }
    pub fn apply(&mut self, effects: Vec<ReceiverEffect>) -> Result<()> {
        if self.releasing {
            self.release();
            ensure!(
                !self.releasing,
                "Windows has not released the previous input. Focus a normal window locally, or restart zflow as administrator in Settings."
            );
        }
        let result = (|| {
            for effect in effects {
                match effect {
                    ReceiverEffect::Key { key, pressed, .. } => {
                        for (key, down) in self.remap.key(key, pressed) {
                            self.key(key, down)?;
                        }
                    }
                    ReceiverEffect::Button {
                        button, pressed, ..
                    } => {
                        for (key, down) in self.remap.pointer(pressed || !self.buttons.is_empty()) {
                            self.key(key, down)?;
                        }
                        (self.post)(3, i32::from(button.0), i32::from(pressed), 0)?;
                        if pressed {
                            self.buttons.insert(button);
                        } else {
                            self.buttons.remove(&button);
                        }
                        self.remap.pointer_released(!self.buttons.is_empty());
                    }
                    ReceiverEffect::Motion { delta, .. } => {
                        if delta.scroll_x != 0 || delta.scroll_y != 0 {
                            for (key, down) in self.remap.pointer(!self.buttons.is_empty()) {
                                self.key(key, down)?;
                            }
                        }
                        let bounded =
                            |n: i64| n.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
                        if delta.dx != 0 || delta.dy != 0 {
                            (self.post)(4, bounded(delta.dx), bounded(delta.dy), 0)?;
                        }
                        let flip = |n: i64| {
                            if self.reverse_scroll {
                                n.saturating_neg()
                            } else {
                                n
                            }
                        };
                        if delta.scroll_y != 0 {
                            (self.post)(5, 0, bounded(flip(delta.scroll_y)), 0)?;
                        }
                        if delta.scroll_x != 0 {
                            (self.post)(5, 0, bounded(flip(delta.scroll_x)), 1)?;
                        }
                    }
                    ReceiverEffect::ActivationClosed { .. } => self.release(),
                    ReceiverEffect::TouchReplaced { state, .. } => {
                        ensure!(state.is_empty(), "Raw touch is not supported on Windows")
                    }
                    _ => {}
                }
            }
            Ok(())
        })();
        if result.is_err() {
            self.release();
        }
        result
    }
    pub fn tick(&mut self) -> Result<()> {
        if self.releasing {
            // UIPI can reject the mouse-up immediately after a click focuses an
            // elevated window. Keep retrying even after that session closes.
            self.release();
            return Ok(());
        }
        if let Some((key, at)) = self.repeat
            && Instant::now() >= at
        {
            if let Some((kind, code, extra)) = super::keys::posted(key) {
                (self.post)(kind, code, 1, extra)?;
            }
            self.repeat = Some((key, Instant::now() + self.repeat_interval));
        }
        Ok(())
    }
    pub fn release(&mut self) {
        self.repeat = None;
        for key in self.keys.clone() {
            if let Some((kind, code, extra)) = super::keys::posted(key)
                && (self.post)(kind, code, 0, extra).is_ok()
            {
                self.keys.remove(&key);
            }
        }
        for button in self.buttons.clone() {
            if (self.post)(3, i32::from(button.0), 0, 0).is_ok() {
                self.buttons.remove(&button);
            }
        }
        self.remap.reset(self.remap.mode());
        self.releasing = !self.keys.is_empty() || !self.buttons.is_empty();
    }
}
impl Drop for Injector {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    type NativeEvent = (i32, i32, i32, i32);
    thread_local! {
        static POSTS:RefCell<Vec<NativeEvent>>=const{RefCell::new(Vec::new())};
        static FAIL:Cell<bool>=const{Cell::new(false)};
    }
    fn record(kind: i32, code: i32, value: i32, extra: i32) -> Result<()> {
        ensure!(!FAIL.get(), "simulated SendInput failure");
        POSTS.with_borrow_mut(|events| events.push((kind, code, value, extra)));
        Ok(())
    }
    fn fake() -> Injector {
        POSTS.with_borrow_mut(Vec::clear);
        FAIL.set(false);
        let mut injector = Injector::new();
        injector.post = record;
        injector
    }
    fn key(usage: u16, pressed: bool) -> ReceiverEffect {
        ReceiverEffect::Key {
            key: HidUsage::keyboard(usage),
            pressed,
            synthetic: false,
        }
    }
    #[test]
    fn release_and_drop_free_every_key_and_button() {
        let mut i = fake();
        i.apply(vec![
            key(224, true),
            key(4, true),
            ReceiverEffect::Button {
                button: PointerButton(4),
                pressed: true,
                synthetic: false,
            },
        ])
        .unwrap();
        drop(i);
        POSTS.with_borrow(|p| {
            assert!(p.contains(&(1, 0x1d, 0, 0)));
            assert!(p.contains(&(1, 0x1e, 0, 0)));
            assert!(p.contains(&(3, 4, 0, 0)));
        });
    }
    #[test]
    fn failed_release_is_retried_when_desktop_becomes_available() {
        let mut i = fake();
        i.apply(vec![key(4, true)]).unwrap();
        FAIL.set(true);
        i.release();
        assert!(i.keys.contains(&HidUsage::keyboard(4)));
        FAIL.set(false);
        i.tick().unwrap();
        assert!(i.keys.is_empty());
    }
    #[test]
    fn blocked_click_cancels_repeat_retries_mouse_up_and_blocks_new_activation() {
        let mut i = fake();
        let button = |pressed| ReceiverEffect::Button {
            button: PointerButton(1),
            pressed,
            synthetic: false,
        };
        i.apply(vec![key(4, true), button(true)]).unwrap();
        FAIL.set(true);
        assert!(i.apply(vec![button(false)]).is_err());
        assert!(i.repeat.is_none());
        assert!(i.releasing);
        assert!(i.apply(vec![key(5, true)]).is_err());
        i.tick().unwrap();
        assert!(i.releasing);
        FAIL.set(false);
        i.tick().unwrap();
        assert!(!i.releasing);
        assert!(i.keys.is_empty() && i.buttons.is_empty());
        POSTS.with_borrow(|p| {
            assert!(p.contains(&(1, 0x1e, 0, 0)));
            assert!(p.contains(&(3, 1, 0, 0)));
            assert!(!p.contains(&(1, 0x30, 1, 0)));
        });
        i.apply(vec![key(5, true), key(5, false)]).unwrap();
    }
    #[test]
    fn arrow_repeat_and_pc_modifier_positions() {
        let mut i = fake();
        i.configure(KeyboardMode::PcPositions, false);
        i.apply(vec![key(227, true), key(79, true)]).unwrap();
        assert_eq!(i.repeat.unwrap().0, HidUsage::keyboard(79));
        i.repeat = Some((HidUsage::keyboard(79), Instant::now()));
        i.tick().unwrap();
        POSTS.with_borrow(|p| {
            assert!(p.contains(&(1, 0x38, 1, 0)));
            assert_eq!(p.iter().filter(|e| **e == (1, 0x4d, 1, 1)).count(), 2);
        });
        i.apply(vec![key(79, false)]).unwrap();
        assert!(i.repeat.is_none());
    }
    #[test]
    fn wheel_units_and_direction_are_preserved() {
        let mut i = fake();
        i.configure(KeyboardMode::Standard, true);
        i.apply(vec![ReceiverEffect::Motion {
            delta: MotionDelta {
                dx: 7,
                dy: -9,
                scroll_x: 30,
                scroll_y: -120,
            },
            through_sequence: crate::core::MotionSequence(1),
        }])
        .unwrap();
        POSTS.with_borrow(|p| assert_eq!(p, &[(4, 7, -9, 0), (5, 0, 120, 0), (5, 0, -30, 1)]));
    }
}
