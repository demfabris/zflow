//! Spike C: does a uinput virtual multitouch touchpad make GNOME fire its
//! native 3-finger gestures from synthetic contacts?
//!
//! Creates a type-B MT touchpad (100x73mm at 30 units/mm), waits for udev and
//! the compositor to adopt it, then performs a 3-finger swipe UP (GNOME:
//! overview opens), pauses, and a 3-finger swipe DOWN (overview closes).
//! Verdict comes from udev classification plus what the compositor visibly
//! does. Device disappears on exit.

use std::thread::sleep;
use std::time::Duration;

use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{
    AbsInfo, AbsoluteAxisType, AttributeSet, EventType, InputEvent, Key, PropType, UinputAbsSetup,
};

const MAX_X: i32 = 2999; // 100mm at 30 units/mm
const MAX_Y: i32 = 2199; // 73mm
const RES: i32 = 30;
const FRAME: Duration = Duration::from_millis(8); // 125 Hz

fn abs(code: u16, v: i32) -> InputEvent {
    InputEvent::new(EventType::ABSOLUTE, code, v)
}

fn key(k: Key, v: i32) -> InputEvent {
    InputEvent::new(EventType::KEY, k.code(), v)
}

struct Pad {
    dev: VirtualDevice,
    next_id: i32,
}

impl Pad {
    /// Land three fingers in one frame: x-spread row centered at (cx, cy).
    fn touch_down(&mut self, cx: i32, cy: i32) -> std::io::Result<()> {
        let mut ev = Vec::new();
        for s in 0..3 {
            let x = cx + (s - 1) * 400;
            ev.push(abs(AbsoluteAxisType::ABS_MT_SLOT.0, s));
            ev.push(abs(AbsoluteAxisType::ABS_MT_TRACKING_ID.0, self.next_id));
            self.next_id += 1;
            ev.push(abs(AbsoluteAxisType::ABS_MT_POSITION_X.0, x));
            ev.push(abs(AbsoluteAxisType::ABS_MT_POSITION_Y.0, cy));
        }
        ev.push(key(Key::BTN_TOUCH, 1));
        ev.push(key(Key::BTN_TOOL_TRIPLETAP, 1));
        ev.push(abs(AbsoluteAxisType::ABS_X.0, cx - 400));
        ev.push(abs(AbsoluteAxisType::ABS_Y.0, cy));
        self.dev.emit(&ev)
    }

    fn move_to(&mut self, cx: i32, cy: i32) -> std::io::Result<()> {
        let mut ev = Vec::new();
        for s in 0..3 {
            let x = cx + (s - 1) * 400;
            ev.push(abs(AbsoluteAxisType::ABS_MT_SLOT.0, s));
            ev.push(abs(AbsoluteAxisType::ABS_MT_POSITION_X.0, x));
            ev.push(abs(AbsoluteAxisType::ABS_MT_POSITION_Y.0, cy));
        }
        ev.push(abs(AbsoluteAxisType::ABS_X.0, cx - 400));
        ev.push(abs(AbsoluteAxisType::ABS_Y.0, cy));
        self.dev.emit(&ev)
    }

    fn lift(&mut self) -> std::io::Result<()> {
        let mut ev = Vec::new();
        for s in 0..3 {
            ev.push(abs(AbsoluteAxisType::ABS_MT_SLOT.0, s));
            ev.push(abs(AbsoluteAxisType::ABS_MT_TRACKING_ID.0, -1));
        }
        ev.push(key(Key::BTN_TOUCH, 0));
        ev.push(key(Key::BTN_TOOL_TRIPLETAP, 0));
        self.dev.emit(&ev)
    }

    /// 3-finger vertical swipe, 45mm of travel. Slow mode takes ~1.3s with a
    /// 400ms hold at the midpoint to expose 1:1 progressive tracking.
    fn swipe_vertical(&mut self, upward: bool, slow: bool) -> std::io::Result<()> {
        let (y0, y1) = if upward { (1800, 450) } else { (450, 1800) };
        let cx = 1500;
        let steps = if slow { 80 } else { 30 };
        let frame = if slow { Duration::from_millis(16) } else { FRAME };
        self.touch_down(cx, y0)?;
        sleep(frame);
        for i in 1..=steps {
            let y = y0 + (y1 - y0) * i / steps;
            self.move_to(cx, y)?;
            sleep(frame);
            if slow && i == steps / 2 {
                sleep(Duration::from_millis(400)); // fingers hold mid-gesture
            }
        }
        self.lift()
    }
}

fn main() -> std::io::Result<()> {
    let mut keys = AttributeSet::<Key>::new();
    for k in [
        Key::BTN_TOUCH,
        Key::BTN_TOOL_FINGER,
        Key::BTN_TOOL_DOUBLETAP,
        Key::BTN_TOOL_TRIPLETAP,
        Key::BTN_TOOL_QUADTAP,
        Key::BTN_LEFT,
    ] {
        keys.insert(k);
    }
    let mut props = AttributeSet::<PropType>::new();
    props.insert(PropType::POINTER);
    props.insert(PropType::BUTTONPAD);

    let axis = |code, max| UinputAbsSetup::new(code, AbsInfo::new(0, 0, max, 0, 0, RES));
    let dev = VirtualDeviceBuilder::new()?
        .name("zflow-spike-touchpad")
        .with_properties(&props)?
        .with_keys(&keys)?
        .with_absolute_axis(&axis(AbsoluteAxisType::ABS_X, MAX_X))?
        .with_absolute_axis(&axis(AbsoluteAxisType::ABS_Y, MAX_Y))?
        .with_absolute_axis(&UinputAbsSetup::new(
            AbsoluteAxisType::ABS_MT_SLOT,
            AbsInfo::new(0, 0, 4, 0, 0, 0),
        ))?
        .with_absolute_axis(&UinputAbsSetup::new(
            AbsoluteAxisType::ABS_MT_TRACKING_ID,
            AbsInfo::new(0, 0, 65535, 0, 0, 0),
        ))?
        .with_absolute_axis(&axis(AbsoluteAxisType::ABS_MT_POSITION_X, MAX_X))?
        .with_absolute_axis(&axis(AbsoluteAxisType::ABS_MT_POSITION_Y, MAX_Y))?
        .build()?;

    let looping = std::env::args().any(|a| a == "--loop");
    let mut pad = Pad { dev, next_id: 1 };
    println!("virtual touchpad created; waiting 6s for udev + compositor adoption");
    println!("(check udevadm classification now)");
    sleep(Duration::from_secs(6));

    let slow = std::env::args().any(|a| a == "--slow");
    loop {
        println!("3-finger swipe UP (GNOME: overview should open)");
        pad.swipe_vertical(true, slow)?;
        sleep(Duration::from_millis(2500));

        println!("3-finger swipe DOWN (overview should close)");
        pad.swipe_vertical(false, slow)?;
        sleep(Duration::from_millis(2500));

        if !looping {
            break;
        }
    }
    println!("done; removing device");
    Ok(())
}
