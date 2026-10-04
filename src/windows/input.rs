use crate::{
    capture::{CaptureFrame, CaptureTransition, KeyState},
    core::{HidUsage, KeyRemap, KeyboardMode, MotionDelta, PointerButton, ReceiverEffect},
    desktop::{Geometry, Point, Rect},
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
    fn zflow_input_stop(thread: u32);
    fn zflow_input_clean() -> i32;
    fn zflow_desktop_available() -> i32;
    fn zflow_input_post(kind: i32, code: i32, value: i32, extra: i32) -> i32;
    fn zflow_monitors(
        callback: extern "C" fn(i32, i32, i32, i32, *mut c_void),
        context: *mut c_void,
    ) -> i32;
    fn zflow_cursor(x: *mut i32, y: *mut i32, move_to: i32) -> i32;
}

pub enum Event {
    Ready(u32),
    Frame(CaptureFrame),
    Escape,
    Activate,
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
pub fn clean() -> bool {
    unsafe { zflow_input_clean() != 0 }
}
pub fn available() -> bool {
    unsafe { zflow_desktop_available() != 0 }
}
pub fn geometry() -> Result<Geometry> {
    extern "C" fn monitor(x: i32, y: i32, width: i32, height: i32, context: *mut c_void) {
        // SAFETY: zflow_monitors calls synchronously with the live Vec below.
        unsafe { &mut *context.cast::<Vec<Rect>>() }.push(Rect {
            x,
            y,
            width: width as u32,
            height: height as u32,
        });
    }
    let mut monitors: Vec<Rect> = Vec::new();
    ensure!(
        unsafe { zflow_monitors(monitor, (&mut monitors as *mut Vec<Rect>).cast()) } != 0,
        "Could not enumerate Windows monitors"
    );
    let geometry = Geometry { monitors };
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
        "Windows rejected input. Elevated apps require zflow at the same integrity level; secure desktops cannot be controlled"
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
        i.release();
        assert!(i.keys.is_empty());
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
