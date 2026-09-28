//! Posting input on the Mac for a peer that controls it.

use std::{
    collections::{BTreeMap, VecDeque},
    ffi::{CStr, c_char},
    sync::mpsc,
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use tokio::sync::oneshot;

use super::{
    CursorPosition, DesktopRect, keys,
    pointer::{Acceleration, Profile, Scroll, ScrollSplit, clamp_to_displays},
};
use crate::core::{HidUsage, KeyRemap, KeyboardMode, PointerButton, ReceiverEffect};

/// "zflow" in ASCII. Every posted event carries it.
const POSTED_MARK: i64 = 0x7A_666C_6F77;
const DEFAULT_REPEAT: KeyRepeat = KeyRepeat {
    delay: Duration::from_millis(250),
    interval: Duration::from_millis(33),
};
const DEFAULT_DOUBLE_CLICK: Duration = Duration::from_millis(500);
const MIN_INTERVAL: Duration = Duration::from_millis(1);
const LOCK_CHECK_INTERVAL: Duration = Duration::from_millis(100);
/// Presses of one button further apart than this start a new click count.
const CLICK_SLOP: f64 = 4.0;
/// A cursor further than this from where zflow put it was moved on the Mac.
const CURSOR_SLOP: f64 = 0.01;
/// How many recent moves the real cursor may still be behind.
const UNSEEN_MOVES: usize = 16;
const CAPS_LOCK: HidUsage = HidUsage::keyboard(0x39);
const LCTRL: HidUsage = HidUsage::keyboard(0xe0);
const LGUI: HidUsage = HidUsage::keyboard(0xe3);
const RCTRL: HidUsage = HidUsage::keyboard(0xe4);
const RGUI: HidUsage = HidUsage::keyboard(0xe7);

/// A button held during a move, as a CG button number, with the click state
/// of its press.
#[derive(Clone, Copy)]
pub(crate) struct Drag {
    pub button: u16,
    pub click_state: i64,
}

/// One event to post. No `Debug`, so typed keys never reach a log.
#[derive(Clone, Copy)]
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
                // Up is positive for both, but the wire counts right as
                // positive, as Linux does, and CoreGraphics counts left.
                (native.wheel_x, native.wheel_y, native.pixel) = match scroll {
                    Scroll::Lines { x, y } => (x.saturating_neg(), y, 0),
                    Scroll::Pixels { x, y } => (x.saturating_neg(), y, 1),
                };
                native.flags = flags;
            }
            Self::Media { .. } => {}
        }
        native
    }
}

/// What an activation reads from this Mac when it opens.
#[derive(Clone, Copy)]
pub(crate) struct Environment {
    /// The keyboard last typed on is ISO.
    pub iso: bool,
    pub repeat: KeyRepeat,
    pub double_click: Duration,
}

impl Default for Environment {
    fn default() -> Self {
        Self {
            iso: false,
            repeat: DEFAULT_REPEAT,
            double_click: DEFAULT_DOUBLE_CLICK,
        }
    }
}

/// Sees each of the peer's moves before it is posted, from the cursor to
/// where the move would put it before the clamp to the displays. True drops
/// the move. A desktop handoff uses it to notice the pointer leaving.
pub(crate) type Watch = Box<dyn FnMut(CursorPosition, CursorPosition) -> bool + Send>;

/// Everything the injector does to the Mac. Tests use `FakeBackend`.
pub(crate) trait Backend: Send + 'static {
    fn post(&mut self, event: &Posted) -> Result<()>;
    /// Flips the real Caps Lock.
    fn toggle_caps_lock(&mut self) -> Result<()>;
    /// Releases whatever the native side still holds, after a failed post.
    fn release_all(&mut self);
    fn environment(&mut self) -> Environment;
    fn cursor(&mut self) -> Option<CursorPosition>;
    fn display_generation(&mut self) -> u32;
    fn displays(&mut self) -> Vec<DesktopRect>;
    fn frontmost_is_terminal(&mut self) -> bool;
    fn session_locked(&mut self) -> bool;
}

struct Repeat {
    code: u16,
    at: Instant,
}

#[derive(Clone, Copy)]
struct Press {
    button: u16,
    at: Instant,
    position: CursorPosition,
    count: i64,
}

/// Turns receiver effects into Mac events and remembers what they hold, so
/// every way out can release it. No `Debug`: it holds typed keys.
pub(crate) struct InjectorCore<B> {
    backend: B,
    environment: Environment,
    /// The mode for the next activation, sent with the batch that opens it.
    next_keyboard: KeyboardMode,
    keyboard: KeyboardMode,
    remap: KeyRemap,
    /// In the mac mode, what each physical Ctrl or Cmd key became when it
    /// was pressed, so its release matches.
    chosen: BTreeMap<HidUsage, HidUsage>,
    /// Keyboard usages down, with the keycode each went down as.
    keys: BTreeMap<HidUsage, u16>,
    /// How many usages hold each keycode. PrintScreen and F13 share one.
    codes: BTreeMap<u16, u32>,
    media: BTreeMap<HidUsage, u32>,
    repeat: Option<Repeat>,
    /// CG buttons down, with the click state of their press.
    buttons: BTreeMap<u16, i64>,
    last_press: Option<Press>,
    position: Option<CursorPosition>,
    /// Where the cursor was before each move macOS may not show yet, oldest
    /// first. A real cursor at one of them is behind, not moved on the Mac.
    unseen: VecDeque<CursorPosition>,
    acceleration: Acceleration,
    scroll: ScrollSplit,
    displays: Vec<DesktopRect>,
    display_generation: Option<u32>,
    locked: Option<(Instant, bool)>,
    watch: Option<Watch>,
}

impl<B: Backend> InjectorCore<B> {
    pub fn new(backend: B, profile: Profile) -> Self {
        Self {
            backend,
            environment: Environment::default(),
            next_keyboard: KeyboardMode::Standard,
            keyboard: KeyboardMode::Standard,
            remap: KeyRemap::new(KeyboardMode::Standard),
            chosen: BTreeMap::new(),
            keys: BTreeMap::new(),
            codes: BTreeMap::new(),
            media: BTreeMap::new(),
            repeat: None,
            buttons: BTreeMap::new(),
            last_press: None,
            position: None,
            unseen: VecDeque::new(),
            acceleration: Acceleration::new(profile),
            scroll: ScrollSplit::default(),
            displays: Vec::new(),
            display_generation: None,
            locked: None,
            watch: None,
        }
    }

    pub fn watch(&mut self, watch: Watch) {
        self.watch = Some(watch);
    }

    /// Applies one batch in order. A failure or a locked screen releases
    /// everything and fails the batch, which closes the session.
    pub fn apply(
        &mut self,
        effects: Vec<ReceiverEffect>,
        keyboard: Option<KeyboardMode>,
        now: Instant,
    ) -> Result<()> {
        if let Some(keyboard) = keyboard {
            self.next_keyboard = keyboard;
        }
        let result = if effects.iter().any(ReceiverEffect::is_injection) && self.locked(now) {
            Err(anyhow!("the Mac is locked"))
        } else {
            effects
                .into_iter()
                .try_for_each(|effect| self.effect(effect, now))
        };
        if result.is_err() {
            self.release_all();
        }
        result
    }

    fn effect(&mut self, effect: ReceiverEffect, now: Instant) -> Result<()> {
        // Keys the keyboard held back go down before a click or a scroll, as
        // on Linux. `busy` is whether a button is still down after it.
        let pointer = match &effect {
            ReceiverEffect::Button {
                button, pressed, ..
            } => Some((
                *pressed,
                *pressed || self.buttons.keys().any(|&held| held + 1 != button.0),
            )),
            ReceiverEffect::Motion { delta, .. } if delta.scroll_x != 0 || delta.scroll_y != 0 => {
                Some((true, !self.buttons.is_empty()))
            }
            _ => None,
        };
        match pointer {
            Some((true, busy)) => {
                for (usage, pressed) in self.remap.pointer(busy) {
                    self.output(usage, pressed, now)?;
                }
            }
            Some((false, busy)) => self.remap.pointer_released(busy),
            None => {}
        }
        match effect {
            ReceiverEffect::ActivationOpened(_) => {
                self.begin();
                Ok(())
            }
            ReceiverEffect::ActivationClosed { .. } => {
                self.release_all();
                Ok(())
            }
            ReceiverEffect::Motion { delta, .. } => {
                self.motion(delta.dx, delta.dy, now)?;
                self.scroll(delta.scroll_x, delta.scroll_y)
            }
            ReceiverEffect::Key { key, pressed, .. } => self.key(key, pressed, now),
            ReceiverEffect::Button {
                button, pressed, ..
            } => self.button(button, pressed, now),
            // The session drops touch before it reaches a Mac.
            ReceiverEffect::TouchReplaced { .. }
            | ReceiverEffect::SnapshotAck { .. }
            | ReceiverEffect::Rejected { .. } => Ok(()),
        }
    }

    /// Starts an activation with nothing down, in the mode its first batch
    /// named, and rereads this Mac's keyboard, repeat and click settings.
    fn begin(&mut self) {
        self.release_all();
        self.keyboard = self.next_keyboard;
        // The mac mode is Standard plus the Ctrl and Cmd swap in `choose`.
        self.remap.reset(match self.keyboard {
            KeyboardMode::Mac => KeyboardMode::Standard,
            mode => mode,
        });
        self.environment = self.backend.environment();
        self.acceleration.reset();
        self.scroll.reset();
        self.last_press = None;
    }

    /// Releases keys, then modifiers, then buttons, and stops the repeat.
    /// Caps Lock stays as it is.
    pub fn release_all(&mut self) {
        self.repeat = None;
        let mut failed = false;
        let (modifiers, plain): (Vec<u16>, Vec<u16>) = self
            .codes
            .keys()
            .partition(|&&code| keys::is_modifier(code));
        for code in plain {
            self.codes.remove(&code);
            failed |= self
                .backend
                .post(&self.key_event(code, false, false))
                .is_err();
        }
        for (_, key) in std::mem::take(&mut self.media) {
            failed |= self
                .backend
                .post(&Posted::Media { key, down: false })
                .is_err();
        }
        for code in modifiers {
            self.codes.remove(&code);
            let flags = self.flags();
            failed |= self
                .backend
                .post(&Posted::Modifier {
                    code,
                    down: false,
                    flags,
                })
                .is_err();
        }
        if !self.buttons.is_empty() {
            let at = self.cursor();
            for (button, click_state) in std::mem::take(&mut self.buttons) {
                let flags = self.flags();
                failed |= self
                    .backend
                    .post(&Posted::Button {
                        button,
                        down: false,
                        at,
                        click_state,
                        flags,
                    })
                    .is_err();
            }
        }
        self.keys.clear();
        self.chosen.clear();
        self.remap.reset(self.remap.mode());
        self.backend.release_all();
        if failed {
            tracing::warn!("some held Mac input could not be released");
        }
    }

    /// When the repeating key next repeats.
    pub fn deadline(&self) -> Option<Instant> {
        self.repeat.as_ref().map(|repeat| repeat.at)
    }

    /// Repeats the newest held key when it is due. macOS does not repeat
    /// posted keys.
    pub fn tick(&mut self, now: Instant) -> Result<()> {
        let (code, due) = match &self.repeat {
            Some(repeat) if repeat.at <= now => (repeat.code, repeat.at),
            _ => return Ok(()),
        };
        if self.locked(now) {
            self.release_all();
            bail!("the Mac is locked");
        }
        // A late tick does not catch up with a burst.
        let interval = self.environment.repeat.interval;
        let next = due + interval;
        let at = if next > now { next } else { now + interval };
        self.repeat = Some(Repeat { code, at });
        let result = self.backend.post(&self.key_event(code, true, true));
        if result.is_err() {
            self.release_all();
        }
        result
    }

    fn key(&mut self, physical: HidUsage, pressed: bool, now: Instant) -> Result<()> {
        let usage = self.choose(physical, pressed);
        for (output, pressed) in self.remap.key(usage, pressed) {
            self.output(output, pressed, now)?;
        }
        Ok(())
    }

    /// In the mac mode, Ctrl and Cmd trade places outside terminals, so
    /// Ctrl+C copies in an editor and still interrupts in a shell. The front
    /// app is read on the press, and the release goes where the press went.
    fn choose(&mut self, physical: HidUsage, pressed: bool) -> HidUsage {
        let swapped = match physical {
            LCTRL => LGUI,
            LGUI => LCTRL,
            RCTRL => RGUI,
            RGUI => RCTRL,
            _ => return physical,
        };
        if self.keyboard != KeyboardMode::Mac {
            return physical;
        }
        if !pressed {
            return self.chosen.remove(&physical).unwrap_or(physical);
        }
        if let Some(&chosen) = self.chosen.get(&physical) {
            return chosen;
        }
        let chosen = if self.backend.frontmost_is_terminal() {
            physical
        } else {
            swapped
        };
        self.chosen.insert(physical, chosen);
        chosen
    }

    /// Posts one key edge that came out of the remap.
    fn output(&mut self, usage: HidUsage, pressed: bool, now: Instant) -> Result<()> {
        if let Some(key) = keys::hid_to_media_key(usage) {
            let changed = if pressed {
                self.media.insert(usage, key).is_none()
            } else {
                self.media.remove(&usage).is_some()
            };
            return match changed {
                true => self.backend.post(&Posted::Media { key, down: pressed }),
                false => Ok(()),
            };
        }
        if usage == CAPS_LOCK {
            // The press flips the real lock; its release does nothing.
            return match pressed {
                true => self.backend.toggle_caps_lock(),
                false => Ok(()),
            };
        }
        if pressed {
            self.press(usage, now)
        } else {
            self.release(usage)
        }
    }

    fn press(&mut self, usage: HidUsage, now: Instant) -> Result<()> {
        let Some(code) = keys::hid_to_mac_keycode(usage, self.environment.iso) else {
            return Ok(());
        };
        if self.keys.contains_key(&usage) {
            return Ok(());
        }
        self.keys.insert(usage, code);
        let count = self.codes.entry(code).or_default();
        *count += 1;
        if *count > 1 {
            return Ok(());
        }
        if keys::is_modifier(code) {
            let flags = self.flags();
            return self.backend.post(&Posted::Modifier {
                code,
                down: true,
                flags,
            });
        }
        // Only the newest key repeats, as on a real keyboard.
        self.repeat = Some(Repeat {
            code,
            at: now + self.environment.repeat.delay,
        });
        self.backend.post(&self.key_event(code, true, false))
    }

    fn release(&mut self, usage: HidUsage) -> Result<()> {
        let Some(code) = self.keys.remove(&usage) else {
            return Ok(());
        };
        let Some(count) = self.codes.get_mut(&code) else {
            return Ok(());
        };
        *count -= 1;
        if *count > 0 {
            return Ok(());
        }
        self.codes.remove(&code);
        if keys::is_modifier(code) {
            let flags = self.flags();
            return self.backend.post(&Posted::Modifier {
                code,
                down: false,
                flags,
            });
        }
        if self
            .repeat
            .as_ref()
            .is_some_and(|repeat| repeat.code == code)
        {
            self.repeat = None;
        }
        self.backend.post(&self.key_event(code, false, false))
    }

    fn key_event(&self, code: u16, down: bool, autorepeat: bool) -> Posted {
        Posted::Key {
            code,
            down,
            autorepeat,
            flags: self.flags() | keys::key_flags(code),
        }
    }

    /// Held modifiers with their side bits. Every event carries them, and
    /// inject.c adds Caps Lock from the Mac's own lock, which the Mac's
    /// keyboard can flip at any time.
    fn flags(&self) -> u64 {
        keys::modifier_flags(self.codes.keys().copied())
    }

    fn button(&mut self, button: PointerButton, pressed: bool, now: Instant) -> Result<()> {
        // Wire button 1 is CG's left (0), 2 right (1), 3 middle (2), and on.
        let Some(button) = button.0.checked_sub(1).filter(|&button| button < 32) else {
            return Ok(());
        };
        if self.buttons.contains_key(&button) == pressed {
            return Ok(());
        }
        let at = self.cursor();
        let click_state = if pressed {
            let count = self.click_count(button, at, now);
            self.buttons.insert(button, count);
            count
        } else {
            self.buttons.remove(&button).unwrap_or(1)
        };
        let flags = self.flags();
        self.backend.post(&Posted::Button {
            button,
            down: pressed,
            at,
            click_state,
            flags,
        })
    }

    /// The same button pressed again soon and close by counts up, so apps
    /// see a double or triple click.
    fn click_count(&mut self, button: u16, at: CursorPosition, now: Instant) -> i64 {
        let count = match self.last_press {
            Some(last)
                if last.button == button
                    && now.saturating_duration_since(last.at) <= self.environment.double_click
                    && distance(last.position, at) <= CLICK_SLOP =>
            {
                last.count + 1
            }
            _ => 1,
        };
        self.last_press = Some(Press {
            button,
            at: now,
            position: at,
            count,
        });
        count
    }

    /// Where zflow last put the cursor, unless something on the Mac moved it
    /// since. Read right after a post, the real cursor can still be where an
    /// earlier move left it, which is not a move on the Mac.
    fn cursor(&mut self) -> CursorPosition {
        let near = |a: CursorPosition, b: CursorPosition| distance(a, b) <= CURSOR_SLOP;
        let position = match (self.position, self.backend.cursor()) {
            (Some(ours), Some(real)) if near(ours, real) => {
                self.unseen.clear();
                ours
            }
            (Some(ours), Some(real)) if self.unseen.iter().any(|&at| near(at, real)) => ours,
            (_, Some(real)) => {
                self.unseen.clear();
                real
            }
            (Some(ours), None) => ours,
            (None, None) => CursorPosition::default(),
        };
        self.position = Some(position);
        position
    }

    fn motion(&mut self, dx: i64, dy: i64, now: Instant) -> Result<()> {
        let (dx, dy) = self.acceleration.apply(dx, dy, now);
        if dx == 0 && dy == 0 {
            return Ok(());
        }
        let from = self.cursor();
        let target = CursorPosition {
            x: from.x + dx as f64,
            y: from.y + dy as f64,
        };
        if self.watch.as_mut().is_some_and(|watch| watch(from, target)) {
            return Ok(());
        }
        self.post_move(from, target, dx, dy)
    }

    /// Puts the cursor at `point` with a posted move, which unlike a warp
    /// does not hold off the Mac's own input.
    pub fn move_to(&mut self, point: CursorPosition) -> Result<()> {
        let from = self.cursor();
        let (dx, dy) = ((point.x - from.x).round(), (point.y - from.y).round());
        self.post_move(from, point, dx as i64, dy as i64)
    }

    fn post_move(
        &mut self,
        from: CursorPosition,
        target: CursorPosition,
        dx: i64,
        dy: i64,
    ) -> Result<()> {
        let to = clamp_to_displays(target, self.displays());
        if self.unseen.len() == UNSEEN_MOVES {
            self.unseen.pop_front();
        }
        self.unseen.push_back(from);
        self.position = Some(to);
        // A drag names one button: left, then right, then the lowest other.
        let drag = self
            .buttons
            .first_key_value()
            .map(|(&button, &click_state)| Drag {
                button,
                click_state,
            });
        let flags = self.flags();
        self.backend.post(&Posted::Move {
            to,
            dx,
            dy,
            drag,
            flags,
        })
    }

    /// The active displays, read again after macOS reconfigures them.
    fn displays(&mut self) -> &[DesktopRect] {
        let generation = self.backend.display_generation();
        if self.display_generation != Some(generation) {
            self.displays = self.backend.displays();
            self.display_generation = (!self.displays.is_empty()).then_some(generation);
        }
        &self.displays
    }

    fn scroll(&mut self, x: i64, y: i64) -> Result<()> {
        let Some(scroll) = self.scroll.split(x, y) else {
            return Ok(());
        };
        let flags = self.flags();
        self.backend.post(&Posted::Scroll { scroll, flags })
    }

    /// Whether the screen is locked, asked at most every 100 ms.
    fn locked(&mut self, now: Instant) -> bool {
        if let Some((at, locked)) = self.locked
            && now.saturating_duration_since(at) < LOCK_CHECK_INTERVAL
        {
            return locked;
        }
        let locked = self.backend.session_locked();
        self.locked = Some((now, locked));
        locked
    }
}

fn distance(a: CursorPosition, b: CursorPosition) -> f64 {
    (a.x - b.x).hypot(a.y - b.y)
}

/// Whether this Mac can post a key. The session drops the others.
pub(crate) fn supports_key(usage: HidUsage) -> bool {
    keys::hid_to_mac_keycode(usage, false).is_some() || keys::hid_to_media_key(usage).is_some()
}

/// Work for the injector thread. No `Debug`, so effects never reach a log.
enum InjectCommand {
    Apply {
        effects: Vec<ReceiverEffect>,
        keyboard: Option<KeyboardMode>,
        applied: oneshot::Sender<Result<Instant>>,
    },
    MoveTo(CursorPosition),
    ReleaseAll,
    Watch(Watch),
}

/// Posts on its own thread, `zflow-inject`, which also times key repeat.
/// Dropping it releases everything and joins the thread.
pub(crate) struct Injector {
    commands: Option<mpsc::Sender<InjectCommand>>,
    thread: Option<JoinHandle<()>>,
}

impl Injector {
    /// Posts on this Mac.
    pub fn mac() -> Result<Self> {
        Self::start(MacBackend::open()?, pointer_profile())
    }

    pub fn start<B: Backend>(backend: B, profile: Profile) -> Result<Self> {
        let (commands, receiver) = mpsc::channel();
        let core = InjectorCore::new(backend, profile);
        let thread = std::thread::Builder::new()
            .name("zflow-inject".to_owned())
            .spawn(move || run(core, receiver))
            .context("could not start the Mac input thread")?;
        Ok(Self {
            commands: Some(commands),
            thread: Some(thread),
        })
    }

    /// Applies a batch in order. The answer says when its last event went
    /// out; an error means everything was released.
    pub fn apply(
        &self,
        effects: Vec<ReceiverEffect>,
        keyboard: Option<KeyboardMode>,
    ) -> oneshot::Receiver<Result<Instant>> {
        let (applied, answer) = oneshot::channel();
        self.send(InjectCommand::Apply {
            effects,
            keyboard,
            applied,
        });
        answer
    }

    /// Puts the cursor at `point`, after anything sent before.
    pub fn move_to(&self, point: CursorPosition) {
        self.send(InjectCommand::MoveTo(point));
    }

    pub fn release_all(&self) {
        self.send(InjectCommand::ReleaseAll);
    }

    /// Shows `watch` every move from now on.
    pub fn watch(&self, watch: Watch) {
        self.send(InjectCommand::Watch(watch));
    }

    fn send(&self, command: InjectCommand) {
        // A thread that is gone drops `applied`, which answers the caller.
        if let Some(commands) = &self.commands {
            let _ = commands.send(command);
        }
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        // The thread releases everything once its channel closes.
        self.commands.take();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::warn!("the Mac input thread panicked");
        }
    }
}

fn run<B: Backend>(mut core: InjectorCore<B>, commands: mpsc::Receiver<InjectCommand>) {
    loop {
        let command = match core.deadline() {
            Some(deadline) => {
                match commands.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(command) => Some(command),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match commands.recv() {
                Ok(command) => Some(command),
                Err(mpsc::RecvError) => break,
            },
        };
        match command {
            Some(InjectCommand::Apply {
                effects,
                keyboard,
                applied,
            }) => {
                let result = core
                    .apply(effects, keyboard, Instant::now())
                    .map(|()| Instant::now());
                let _ = applied.send(result);
            }
            Some(InjectCommand::MoveTo(point)) => {
                if let Err(error) = core.move_to(point) {
                    tracing::warn!(error = %format!("{error:#}"), "could not place the Mac cursor");
                }
            }
            Some(InjectCommand::ReleaseAll) => core.release_all(),
            Some(InjectCommand::Watch(watch)) => core.watch(watch),
            None => {}
        }
        // Steady input can keep the timeout from firing, so the repeat is
        // checked after every command too.
        if let Err(error) = core.tick(Instant::now()) {
            tracing::warn!(error = %format!("{error:#}"), "key repeat stopped");
        }
    }
    core.release_all();
}

/// `ZFLOW_MAC_POINTER`, `adaptive:<speed>` or `flat:<speed>`, tunes the
/// pointer until there is a setting for it.
fn pointer_profile() -> Profile {
    let profile = match std::env::var("ZFLOW_MAC_POINTER") {
        Ok(value) => value.parse().unwrap_or_else(|error: anyhow::Error| {
            tracing::warn!(value, error = %format!("{error:#}"), "ignoring ZFLOW_MAC_POINTER");
            Profile::default()
        }),
        Err(_) => Profile::default(),
    };
    tracing::info!(?profile, "Mac pointer profile");
    profile
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

    fn caps_lock(&mut self) -> Result<bool> {
        let mut on = 0;
        // SAFETY: the output is a plain int.
        ensure!(
            unsafe { zflow_mac_caps_lock(&mut on) } == 0,
            "could not read Caps Lock"
        );
        Ok(on != 0)
    }
}

impl Backend for MacBackend {
    fn post(&mut self, event: &Posted) -> Result<()> {
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

    /// Flips the real lock, light included.
    fn toggle_caps_lock(&mut self) -> Result<()> {
        let on = !self.caps_lock()?;
        // SAFETY: this only calls IOHIDSystem.
        ensure!(
            unsafe { zflow_mac_set_caps_lock(i32::from(on)) } == 0,
            "could not set Caps Lock"
        );
        Ok(())
    }

    fn release_all(&mut self) {
        // SAFETY: this posts releases from inject.c's own table.
        unsafe { zflow_mac_inject_release_all() }
    }

    fn environment(&mut self) -> Environment {
        Environment {
            iso: keyboard_is_iso(),
            repeat: key_repeat(),
            double_click: double_click_interval(),
        }
    }

    fn cursor(&mut self) -> Option<CursorPosition> {
        super::cursor_position().ok()
    }

    fn display_generation(&mut self) -> u32 {
        super::display_generation()
    }

    fn displays(&mut self) -> Vec<DesktopRect> {
        super::active_desktop_rectangles().unwrap_or_default()
    }

    fn frontmost_is_terminal(&mut self) -> bool {
        frontmost_bundle_id().is_some_and(|bundle| keys::is_terminal(&bundle))
    }

    fn session_locked(&mut self) -> bool {
        session_locked()
    }
}

impl Drop for MacBackend {
    fn drop(&mut self) {
        // SAFETY: close releases everything held, then frees the source.
        unsafe { zflow_mac_inject_close() }
    }
}

#[derive(Clone, Copy)]
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

/// Records what would be posted and answers queries from its own state.
/// Clones share that state, so a test keeps one and hands one to the core.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct FakeBackend(std::sync::Arc<std::sync::Mutex<Fake>>);

#[cfg(test)]
pub(crate) struct Fake {
    /// What was posted, in order, as text tests compare.
    pub log: Vec<String>,
    pub environment: Environment,
    /// The Mac's own Caps Lock.
    pub caps_lock: bool,
    pub cursor: Option<CursorPosition>,
    /// How many posted moves the cursor shows late, as macOS may.
    pub lag: usize,
    pub moves: VecDeque<CursorPosition>,
    pub displays: Vec<DesktopRect>,
    pub generation: u32,
    pub terminal: bool,
    pub locked: bool,
    pub fail: bool,
    pub focus_reads: usize,
    pub lock_reads: usize,
    /// How often the display was woken.
    pub wakes: usize,
}

#[cfg(test)]
impl Default for Fake {
    fn default() -> Self {
        Self {
            log: Vec::new(),
            environment: Environment::default(),
            caps_lock: false,
            cursor: Some(CursorPosition { x: 960.0, y: 540.0 }),
            lag: 0,
            moves: VecDeque::new(),
            displays: vec![DesktopRect {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
            }],
            generation: 0,
            terminal: false,
            locked: false,
            fail: false,
            focus_reads: 0,
            lock_reads: 0,
            wakes: 0,
        }
    }
}

#[cfg(test)]
impl FakeBackend {
    pub fn state(&self) -> std::sync::MutexGuard<'_, Fake> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn take_log(&self) -> Vec<String> {
        std::mem::take(&mut self.state().log)
    }
}

#[cfg(test)]
impl Backend for FakeBackend {
    fn post(&mut self, event: &Posted) -> Result<()> {
        let mut fake = self.state();
        ensure!(!fake.fail, "posting failed");
        if let Posted::Move { to, .. } = *event {
            fake.moves.push_back(to);
            while fake.moves.len() > fake.lag {
                fake.cursor = fake.moves.pop_front();
            }
        }
        fake.log.push(describe(event));
        Ok(())
    }

    fn toggle_caps_lock(&mut self) -> Result<()> {
        let mut fake = self.state();
        ensure!(!fake.fail, "Caps Lock failed");
        fake.caps_lock = !fake.caps_lock;
        let on = fake.caps_lock;
        fake.log
            .push(format!("caps {}", if on { "on" } else { "off" }));
        Ok(())
    }

    fn release_all(&mut self) {
        self.state().log.push("release all".to_owned());
    }

    fn environment(&mut self) -> Environment {
        self.state().environment
    }

    fn cursor(&mut self) -> Option<CursorPosition> {
        self.state().cursor
    }

    fn display_generation(&mut self) -> u32 {
        self.state().generation
    }

    fn displays(&mut self) -> Vec<DesktopRect> {
        self.state().displays.clone()
    }

    fn frontmost_is_terminal(&mut self) -> bool {
        let mut fake = self.state();
        fake.focus_reads += 1;
        fake.terminal
    }

    fn session_locked(&mut self) -> bool {
        let mut fake = self.state();
        fake.lock_reads += 1;
        fake.locked
    }
}

#[cfg(test)]
fn describe(event: &Posted) -> String {
    let edge = |down: bool| if down { "down" } else { "up" };
    let flags = |flags: u64| match flags {
        0 => String::new(),
        flags => format!(" flags {flags:#x}"),
    };
    match *event {
        Posted::Move {
            to,
            dx,
            dy,
            drag,
            flags: bits,
        } => {
            let drag = drag.map_or(String::new(), |drag| {
                format!(" drag {} click {}", drag.button, drag.click_state)
            });
            format!("move {},{} by {dx},{dy}{drag}{}", to.x, to.y, flags(bits))
        }
        Posted::Button {
            button,
            down,
            at,
            click_state,
            flags: bits,
        } => format!(
            "button {button} {} at {},{} click {click_state}{}",
            edge(down),
            at.x,
            at.y,
            flags(bits)
        ),
        Posted::Key {
            code,
            down,
            autorepeat,
            flags: bits,
        } => format!(
            "key {code} {}{}{}",
            edge(down),
            if autorepeat { " repeat" } else { "" },
            flags(bits)
        ),
        Posted::Modifier {
            code,
            down,
            flags: bits,
        } => format!("modifier {code} {}{}", edge(down), flags(bits)),
        Posted::Scroll {
            scroll: Scroll::Lines { x, y },
            flags: bits,
        } => format!("scroll lines {x},{y}{}", flags(bits)),
        Posted::Scroll {
            scroll: Scroll::Pixels { x, y },
            flags: bits,
        } => format!("scroll pixels {x},{y}{}", flags(bits)),
        Posted::Media { key, down } => format!("media {key} {}", edge(down)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        ActivationId, MotionDelta, MotionSequence, SessionCloseReason, SessionContext,
        SessionEpoch, TransportGeneration,
    };

    const AT: CursorPosition = CursorPosition { x: 10.5, y: -20.0 };

    fn context() -> SessionContext {
        SessionContext {
            session_epoch: SessionEpoch([7; 16]),
            transport_generation: TransportGeneration(1),
            activation_id: ActivationId(1),
        }
    }

    fn opened() -> ReceiverEffect {
        ReceiverEffect::ActivationOpened(context())
    }

    fn closed() -> ReceiverEffect {
        ReceiverEffect::ActivationClosed {
            session: context(),
            reason: SessionCloseReason::LocalRelease,
        }
    }

    fn key(usage: u16, pressed: bool) -> ReceiverEffect {
        ReceiverEffect::Key {
            key: HidUsage::keyboard(usage),
            pressed,
            synthetic: false,
        }
    }

    fn media(usage: u16, pressed: bool) -> ReceiverEffect {
        ReceiverEffect::Key {
            key: HidUsage::consumer(usage),
            pressed,
            synthetic: false,
        }
    }

    fn button(button: u16, pressed: bool) -> ReceiverEffect {
        ReceiverEffect::Button {
            button: PointerButton(button),
            pressed,
            synthetic: false,
        }
    }

    fn motion(dx: i64, dy: i64, scroll_x: i64, scroll_y: i64) -> ReceiverEffect {
        ReceiverEffect::Motion {
            delta: MotionDelta {
                dx,
                dy,
                scroll_x,
                scroll_y,
            },
            through_sequence: MotionSequence(0),
        }
    }

    fn after(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    /// A core in `mode` with its activation open and the log empty. Motion
    /// is not accelerated, so moves are easy to read.
    fn open(mode: KeyboardMode, now: Instant) -> (InjectorCore<FakeBackend>, FakeBackend) {
        let fake = FakeBackend::default();
        let mut core = InjectorCore::new(fake.clone(), Profile::Flat { speed: 0.0 });
        core.apply(vec![opened()], Some(mode), now).unwrap();
        fake.take_log();
        (core, fake)
    }

    fn apply(
        core: &mut InjectorCore<FakeBackend>,
        fake: &FakeBackend,
        effects: Vec<ReceiverEffect>,
        now: Instant,
    ) -> Vec<String> {
        core.apply(effects, None, now).unwrap();
        fake.take_log()
    }

    #[test]
    fn a_drag_names_left_then_right_then_the_lowest_other_button() {
        let start = Instant::now();
        let (mut core, fake) = open(KeyboardMode::Standard, start);
        let mut drag = |effects: Vec<ReceiverEffect>| {
            core.apply(effects, None, start).unwrap();
            core.apply(vec![motion(10, 0, 0, 0)], None, start).unwrap();
            let log = fake.take_log();
            let last = log.last().unwrap();
            last.split_once(" drag ")
                .map_or("none".to_owned(), |(_, drag)| drag.to_owned())
        };
        assert_eq!(drag(vec![]), "none");
        assert_eq!(drag(vec![button(1, true)]), "0 click 1");
        assert_eq!(drag(vec![button(2, true)]), "0 click 1");
        assert_eq!(drag(vec![button(1, false)]), "1 click 1");
        assert_eq!(drag(vec![button(5, true), button(4, true)]), "1 click 1");
        assert_eq!(drag(vec![button(2, false)]), "3 click 1");
        assert_eq!(drag(vec![button(3, true)]), "2 click 1");
        assert_eq!(
            drag(vec![button(3, false), button(4, false), button(5, false)]),
            "none"
        );
    }

    #[test]
    fn buttons_post_where_the_cursor_is_with_their_click_state() {
        let start = Instant::now();
        let (mut core, fake) = open(KeyboardMode::Standard, start);
        let click = |core: &mut InjectorCore<FakeBackend>, button_number: u16, ms: u64| {
            apply(
                core,
                &fake,
                vec![button(button_number, true), button(button_number, false)],
                after(start, ms),
            )
        };
        assert_eq!(
            click(&mut core, 1, 0),
            [
                "button 0 down at 960,540 click 1",
                "button 0 up at 960,540 click 1"
            ]
        );
        assert_eq!(
            click(&mut core, 1, 300)[1],
            "button 0 up at 960,540 click 2"
        );
        assert_eq!(
            click(&mut core, 1, 600)[0],
            "button 0 down at 960,540 click 3"
        );
        // Too slow, another button, or too far away starts over.
        assert_eq!(
            click(&mut core, 1, 1101)[0],
            "button 0 down at 960,540 click 1"
        );
        assert_eq!(
            click(&mut core, 2, 1200)[0],
            "button 1 down at 960,540 click 1"
        );
        assert_eq!(
            click(&mut core, 1, 1300)[0],
            "button 0 down at 960,540 click 1"
        );
        apply(
            &mut core,
            &fake,
            vec![motion(3, 0, 0, 0)],
            after(start, 1350),
        );
        assert_eq!(
            click(&mut core, 1, 1400)[0],
            "button 0 down at 963,540 click 2"
        );
        apply(
            &mut core,
            &fake,
            vec![motion(5, 0, 0, 0)],
            after(start, 1450),
        );
        assert_eq!(
            click(&mut core, 1, 1500)[0],
            "button 0 down at 968,540 click 1"
        );

        // A drag carries its press's count.
        apply(&mut core, &fake, vec![button(1, true)], after(start, 1600));
        assert_eq!(
            apply(
                &mut core,
                &fake,
                vec![motion(1, 0, 0, 0)],
                after(start, 1650)
            ),
            ["move 969,540 by 1,0 drag 0 click 2"]
        );
        // Button 0 and anything past 32 do not exist on a Mac.
        assert!(
            apply(
                &mut core,
                &fake,
                vec![button(0, true), button(33, true)],
                start
            )
            .is_empty()
        );
        // A new activation forgets the last click.
        core.apply(vec![opened()], None, after(start, 1700))
            .unwrap();
        fake.take_log();
        assert_eq!(
            click(&mut core, 1, 1750)[0],
            "button 0 down at 969,540 click 1"
        );
    }

    #[test]
    fn every_event_carries_the_held_modifiers() {
        let start = Instant::now();
        let (mut core, fake) = open(KeyboardMode::Standard, start);
        assert_eq!(
            apply(
                &mut core,
                &fake,
                vec![
                    key(0xe1, true),
                    key(0x04, true),
                    button(1, true),
                    motion(1, 0, 0, 120),
                ],
                start,
            ),
            [
                "modifier 56 down flags 0x20002",
                "key 0 down flags 0x20002",
                "button 0 down at 960,540 click 1 flags 0x20002",
                "move 961,540 by 1,0 drag 0 click 1 flags 0x20002",
                "scroll lines 0,1 flags 0x20002",
            ]
        );
        assert_eq!(
            apply(
                &mut core,
                &fake,
                vec![key(0xe7, true), key(0xe1, false)],
                start
            ),
            [
                "modifier 54 down flags 0x120012",
                "modifier 56 up flags 0x100010"
            ]
        );
        // Arrows keep the numeric pad and Fn flags a real arrow has.
        assert_eq!(
            apply(&mut core, &fake, vec![key(0x50, true)], start),
            ["key 123 down flags 0xb00010"]
        );
    }

    #[test]
    fn caps_lock_flips_the_real_lock_on_press_only() {
        let start = Instant::now();
        let (mut core, fake) = open(KeyboardMode::Standard, start);
        assert_eq!(
            apply(
                &mut core,
                &fake,
                vec![key(0x39, true), key(0x39, false), key(0x04, true)],
                start
            ),
            ["caps on", "key 0 down"]
        );
        assert_eq!(core.deadline(), Some(after(start, 250)));
        assert_eq!(
            apply(&mut core, &fake, vec![key(0x39, true)], start),
            ["caps off"]
        );
        // Releasing everything never flips it.
        apply(
            &mut core,
            &fake,
            vec![key(0x39, false), key(0x39, true)],
            start,
        );
        assert_eq!(
            apply(&mut core, &fake, vec![closed()], start),
            ["key 0 up", "release all"]
        );
        assert!(fake.state().caps_lock);

        // The Mac's own keyboard turned it off; the next press turns it on.
        fake.state().caps_lock = false;
        core.apply(vec![opened()], None, start).unwrap();
        fake.take_log();
        assert_eq!(
            apply(&mut core, &fake, vec![key(0x39, true)], start),
            ["caps on"]
        );
    }

    #[test]
    fn the_newest_key_repeats_after_the_delay_then_at_the_interval() {
        let start = Instant::now();
        let (mut core, fake) = open(KeyboardMode::Standard, start);
        apply(&mut core, &fake, vec![key(0x0e, true)], start);
        assert_eq!(core.deadline(), Some(after(start, 250)));
        core.tick(after(start, 249)).unwrap();
        assert!(fake.take_log().is_empty());
        core.tick(after(start, 250)).unwrap();
        assert_eq!(fake.take_log(), ["key 40 down repeat"]);
        assert_eq!(core.deadline(), Some(after(start, 283)));
        core.tick(after(start, 283)).unwrap();
        assert_eq!(fake.take_log(), ["key 40 down repeat"]);
        // A late tick repeats once, not in a burst.
        core.tick(after(start, 400)).unwrap();
        assert_eq!(fake.take_log(), ["key 40 down repeat"]);
        assert_eq!(core.deadline(), Some(after(start, 433)));

        // A newer key takes over the repeat, and releasing the older one
        // does not stop it.
        apply(&mut core, &fake, vec![key(0x05, true)], after(start, 410));
        assert_eq!(core.deadline(), Some(after(start, 660)));
        apply(&mut core, &fake, vec![key(0x0e, false)], after(start, 420));
        core.tick(after(start, 660)).unwrap();
        assert_eq!(fake.take_log(), ["key 11 down repeat"]);
        // Modifiers carry into the repeats but never repeat themselves.
        apply(&mut core, &fake, vec![key(0xe1, true)], after(start, 670));
        assert_eq!(core.deadline(), Some(after(start, 693)));
        core.tick(after(start, 693)).unwrap();
        assert_eq!(fake.take_log(), ["key 11 down repeat flags 0x20002"]);
        apply(&mut core, &fake, vec![key(0x05, false)], after(start, 700));
        assert_eq!(core.deadline(), None);

        // Caps Lock and media keys do not repeat either.
        apply(
            &mut core,
            &fake,
            vec![key(0x39, true), media(0xe9, true)],
            after(start, 710),
        );
        assert_eq!(core.deadline(), None);
    }

    #[test]
    fn repeat_follows_this_macs_rate() {
        let start = Instant::now();
        let fake = FakeBackend::default();
        fake.state().environment.repeat = KeyRepeat {
            delay: Duration::from_millis(500),
            interval: Duration::from_millis(80),
        };
        let mut core = InjectorCore::new(fake.clone(), Profile::default());
        core.apply(vec![opened(), key(0x04, true)], None, start)
            .unwrap();
        assert_eq!(core.deadline(), Some(after(start, 500)));
        core.tick(after(start, 500)).unwrap();
        assert_eq!(core.deadline(), Some(after(start, 580)));
    }

    #[test]
    fn release_all_lets_go_of_keys_then_modifiers_then_buttons() {
        let start = Instant::now();
        let (mut core, fake) = open(KeyboardMode::Standard, start);
        apply(
            &mut core,
            &fake,
            vec![
                key(0xe0, true),
                button(3, true),
                key(0x04, true),
                media(0xe9, true),
                key(0xe5, true),
                button(1, true),
            ],
            start,
        );
        assert!(core.deadline().is_some());
        assert_eq!(
            apply(&mut core, &fake, vec![closed()], start),
            [
                "key 0 up flags 0x60005",
                "media 0 up",
                "modifier 59 up flags 0x20004",
                "modifier 60 up",
                "button 0 up at 960,540 click 1",
                "button 2 up at 960,540 click 1",
                "release all",
            ]
        );
        assert_eq!(core.deadline(), None);
        // Nothing is left to release, and the remap forgot the held keys.
        assert!(
            apply(
                &mut core,
                &fake,
                vec![key(0x04, false), key(0xe0, false), button(1, false)],
                start
            )
            .is_empty()
        );
    }

    #[test]
    fn opening_an_activation_resets_the_remap_and_rereads_the_mac() {
        let start = Instant::now();
        let (mut core, fake) = open(KeyboardMode::Standard, start);
        apply(&mut core, &fake, vec![key(0x04, true)], start);
        fake.state().environment.iso = true;
        core.apply(vec![opened()], Some(KeyboardMode::PcPositions), start)
            .unwrap();
        assert_eq!(fake.take_log(), ["key 0 up", "release all"]);
        // PC positions: the key beside Space is Cmd.
        assert_eq!(
            apply(
                &mut core,
                &fake,
                vec![key(0x04, false), key(0xe2, true)],
                start
            ),
            ["modifier 55 down flags 0x100008"]
        );
        // The key left of 1 and the key left of Z swap on an ISO Mac.
        assert_eq!(
            apply(
                &mut core,
                &fake,
                vec![key(0x35, true), key(0x64, true)],
                start
            ),
            ["key 10 down flags 0x100008", "key 50 down flags 0x100008"]
        );
        fake.state().environment.iso = false;
        core.apply(vec![opened()], Some(KeyboardMode::Standard), start)
            .unwrap();
        fake.take_log();
        assert_eq!(
            apply(
                &mut core,
                &fake,
                vec![key(0xe2, true), key(0x35, true)],
                start
            ),
            [
                "modifier 58 down flags 0x80020",
                "key 50 down flags 0x80020"
            ]
        );
    }

    #[test]
    fn the_keyboard_mode_waits_for_the_next_activation() {
        let start = Instant::now();
        let (mut core, fake) = open(KeyboardMode::Standard, start);
        core.apply(
            vec![key(0xe2, true)],
            Some(KeyboardMode::PcPositions),
            start,
        )
        .unwrap();
        assert_eq!(fake.take_log(), ["modifier 58 down flags 0x80020"]);
    }

    #[test]
    fn the_mac_mode_swaps_ctrl_and_cmd_outside_terminals() {
        let start = Instant::now();
        let (mut core, fake) = open(KeyboardMode::Mac, start);
        assert_eq!(
            apply(&mut core, &fake, vec![key(0xe0, true)], start),
            ["modifier 55 down flags 0x100008"]
        );
        // The release matches the press after the focus moves.
        fake.state().terminal = true;
        assert_eq!(
            apply(&mut core, &fake, vec![key(0xe0, false)], start),
            ["modifier 55 up"]
        );
        assert_eq!(
            apply(&mut core, &fake, vec![key(0xe0, true)], start),
            ["modifier 59 down flags 0x40001"]
        );
        fake.state().terminal = false;
        assert_eq!(
            apply(&mut core, &fake, vec![key(0xe0, false)], start),
            ["modifier 59 up"]
        );
        // A repeated press keeps the first choice.
        apply(&mut core, &fake, vec![key(0xe0, true)], start);
        fake.state().terminal = true;
        assert!(apply(&mut core, &fake, vec![key(0xe0, true)], start).is_empty());
        assert_eq!(
            apply(&mut core, &fake, vec![key(0xe0, false)], start),
            ["modifier 55 up"]
        );
        fake.state().terminal = false;
        // Super is Ctrl in an app and Cmd in a terminal, on both sides.
        assert_eq!(
            apply(
                &mut core,
                &fake,
                vec![key(0xe3, true), key(0xe7, true)],
                start
            ),
            [
                "modifier 59 down flags 0x40001",
                "modifier 62 down flags 0x42001"
            ]
        );
        fake.state().terminal = true;
        apply(
            &mut core,
            &fake,
            vec![key(0xe3, false), key(0xe7, false)],
            start,
        );
        assert_eq!(
            apply(
                &mut core,
                &fake,
                vec![key(0xe3, true), key(0x06, true)],
                start
            ),
            [
                "modifier 55 down flags 0x100008",
                "key 8 down flags 0x100008"
            ]
        );
        // Only a new Ctrl or Cmd press asks which app is in front.
        assert_eq!(fake.state().focus_reads, 6);

        let (mut core, fake) = open(KeyboardMode::Standard, start);
        assert_eq!(
            apply(&mut core, &fake, vec![key(0xe0, true)], start),
            ["modifier 59 down flags 0x40001"]
        );
        assert_eq!(fake.state().focus_reads, 0);
    }

    #[test]
    fn media_keys_press_and_release() {
        let start = Instant::now();
        let (mut core, fake) = open(KeyboardMode::Standard, start);
        assert_eq!(
            apply(
                &mut core,
                &fake,
                vec![media(0xcd, true), media(0xcd, false), media(0xe9, true)],
                start
            ),
            ["media 16 down", "media 16 up", "media 0 down"]
        );
        // Power is not posted.
        assert!(apply(&mut core, &fake, vec![media(0x30, true)], start).is_empty());
        assert_eq!(
            apply(&mut core, &fake, vec![media(0xe9, false)], start),
            ["media 0 up"]
        );
    }

    #[test]
    fn print_screen_and_f13_share_a_keycode() {
        let start = Instant::now();
        let (mut core, fake) = open(KeyboardMode::Standard, start);
        assert_eq!(
            apply(
                &mut core,
                &fake,
                vec![key(0x46, true), key(0x68, true)],
                start
            ),
            ["key 105 down flags 0x800000"]
        );
        assert!(apply(&mut core, &fake, vec![key(0x46, false)], start).is_empty());
        assert!(core.deadline().is_some());
        assert_eq!(
            apply(&mut core, &fake, vec![key(0x68, false)], start),
            ["key 105 up flags 0x800000"]
        );
        assert_eq!(core.deadline(), None);
        // Keys a Mac lacks post nothing.
        assert!(
            apply(
                &mut core,
                &fake,
                vec![key(0x47, true), key(0x48, true)],
                start
            )
            .is_empty()
        );
    }

    #[test]
    fn motion_stays_on_the_displays_and_follows_the_real_cursor() {
        let start = Instant::now();
        let (mut core, fake) = open(KeyboardMode::Standard, start);
        assert_eq!(
            apply(&mut core, &fake, vec![motion(100, -40, 0, 0)], start),
            ["move 1060,500 by 100,-40"]
        );
        assert_eq!(
            apply(&mut core, &fake, vec![motion(2000, 0, 0, 0)], start),
            ["move 1919,500 by 2000,0"]
        );
        // The Mac's own trackpad moved the cursor.
        fake.state().cursor = Some(CursorPosition { x: 10.5, y: 10.0 });
        assert_eq!(
            apply(&mut core, &fake, vec![motion(-20, 5, 0, 0)], start),
            ["move 0,15 by -20,5"]
        );
        // A display change is picked up.
        {
            let mut state = fake.state();
            state.displays = vec![DesktopRect {
                x: 0.0,
                y: 0.0,
                width: 800.0,
                height: 600.0,
            }];
            state.generation = 1;
        }
        assert_eq!(
            apply(&mut core, &fake, vec![motion(1000, 1000, 0, 0)], start),
            ["move 799,599 by 1000,1000"]
        );
        assert!(apply(&mut core, &fake, vec![motion(0, 0, 0, 0)], start).is_empty());
    }

    #[test]
    fn a_handoff_places_the_cursor_and_sees_each_move_before_the_clamp() {
        let start = Instant::now();
        let (mut core, fake) = open(KeyboardMode::Standard, start);
        core.move_to(CursorPosition { x: 3.0, y: 540.0 }).unwrap();
        assert_eq!(fake.take_log(), ["move 3,540 by -957,0"]);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        core.watch(Box::new({
            let seen = seen.clone();
            move |from: CursorPosition, to: CursorPosition| {
                seen.lock().unwrap().push((from.x, to.x));
                to.x < 0.0
            }
        }));
        assert_eq!(
            apply(&mut core, &fake, vec![motion(-2, 0, 0, 0)], start),
            ["move 1,540 by -2,0"]
        );
        assert!(
            apply(&mut core, &fake, vec![motion(-5, 0, 0, 0)], start).is_empty(),
            "a move out through the edge is dropped"
        );
        assert_eq!(*seen.lock().unwrap(), [(3.0, 1.0), (1.0, -4.0)]);
    }

    #[test]
    fn clicks_land_where_the_last_move_put_the_cursor_before_macos_shows_it() {
        let start = Instant::now();
        let (mut core, fake) = open(KeyboardMode::Standard, start);
        fake.state().lag = 2;
        assert_eq!(
            apply(
                &mut core,
                &fake,
                vec![
                    motion(10, 0, 0, 0),
                    motion(0, 5, 0, 0),
                    button(1, true),
                    motion(1, 0, 0, 0),
                    button(1, false),
                ],
                start
            ),
            [
                "move 970,540 by 10,0",
                "move 970,545 by 0,5",
                "button 0 down at 970,545 click 1",
                "move 971,545 by 1,0 drag 0 click 1",
                "button 0 up at 971,545 click 1",
            ]
        );
        // A cursor anywhere else was still moved on the Mac.
        {
            let mut state = fake.state();
            state.moves.clear();
            state.cursor = Some(CursorPosition { x: 100.0, y: 100.0 });
        }
        assert_eq!(
            apply(
                &mut core,
                &fake,
                vec![motion(5, 0, 0, 0), button(2, true)],
                start
            ),
            ["move 105,100 by 5,0", "button 1 down at 105,100 click 1"]
        );
    }

    #[test]
    fn motion_is_accelerated() {
        let start = Instant::now();
        let fake = FakeBackend::default();
        let mut core = InjectorCore::new(fake.clone(), Profile::default());
        core.apply(vec![opened()], None, start).unwrap();
        fake.take_log();
        for step in 0..3 {
            core.apply(vec![motion(40, 0, 0, 0)], None, after(start, step * 8))
                .unwrap();
        }
        // 38, 77, then twice the motion at full speed.
        assert_eq!(
            fake.take_log(),
            [
                "move 998,540 by 38,0",
                "move 1075,540 by 77,0",
                "move 1155,540 by 80,0"
            ]
        );
    }

    #[test]
    fn scroll_posts_lines_for_detents_and_pixels_otherwise() {
        let start = Instant::now();
        let (mut core, fake) = open(KeyboardMode::Standard, start);
        assert_eq!(
            apply(
                &mut core,
                &fake,
                vec![
                    motion(0, 0, 0, -240),
                    motion(0, 0, 15, 0),
                    motion(2, 0, 0, 120)
                ],
                start
            ),
            [
                "scroll lines 0,-2",
                "scroll pixels 1,0",
                "move 962,540 by 2,0",
                "scroll pixels 0,10"
            ]
        );
    }

    #[test]
    fn a_locked_mac_refuses_input_and_releases_everything() {
        let start = Instant::now();
        let (mut core, fake) = open(KeyboardMode::Standard, start);
        apply(&mut core, &fake, vec![key(0x04, true)], start);
        // The lock is read at most every 100 ms.
        fake.state().locked = true;
        apply(&mut core, &fake, vec![key(0x05, true)], after(start, 50));
        assert!(
            core.apply(vec![key(0x06, true)], None, after(start, 100))
                .is_err()
        );
        assert_eq!(fake.take_log(), ["key 0 up", "key 11 up", "release all"]);
        // Effects that post nothing still go through.
        core.apply(vec![opened()], None, after(start, 110)).unwrap();
        fake.take_log();

        // A repeat that comes due on a locked screen stops.
        fake.state().locked = false;
        apply(&mut core, &fake, vec![key(0x04, true)], after(start, 300));
        fake.state().locked = true;
        assert!(core.tick(after(start, 550)).is_err());
        assert_eq!(fake.take_log(), ["key 0 up", "release all"]);
        assert_eq!(core.deadline(), None);
        assert_eq!(fake.state().lock_reads, 4);
    }

    #[test]
    fn a_failed_post_releases_everything() {
        let start = Instant::now();
        let (mut core, fake) = open(KeyboardMode::Standard, start);
        apply(
            &mut core,
            &fake,
            vec![key(0x04, true), button(1, true)],
            start,
        );
        fake.state().fail = true;
        assert!(core.apply(vec![key(0x05, true)], None, start).is_err());
        // The posts failed too; the C table is the last word.
        assert_eq!(fake.take_log(), ["release all"]);
        fake.state().fail = false;
        assert!(
            apply(
                &mut core,
                &fake,
                vec![key(0x04, false), button(1, false)],
                start
            )
            .is_empty()
        );
        assert_eq!(core.deadline(), None);
    }

    #[test]
    fn supports_the_keys_it_can_post() {
        assert!(supports_key(HidUsage::keyboard(0x04)));
        assert!(supports_key(HidUsage::keyboard(0x46)));
        assert!(supports_key(HidUsage::consumer(0xe9)));
        assert!(!supports_key(HidUsage::keyboard(0x47)));
        assert!(!supports_key(HidUsage::consumer(0x30)));
    }

    #[test]
    fn the_thread_repeats_held_keys_and_releases_them_when_dropped() {
        let fake = FakeBackend::default();
        fake.state().environment.repeat = KeyRepeat {
            delay: Duration::from_millis(20),
            interval: Duration::from_millis(10),
        };
        let injector = Injector::start(fake.clone(), Profile::default()).unwrap();
        let applied = injector
            .apply(
                vec![opened(), key(0x0e, true)],
                Some(KeyboardMode::Standard),
            )
            .blocking_recv()
            .unwrap();
        assert!(applied.is_ok());
        std::thread::sleep(Duration::from_millis(150));
        let log = fake.take_log();
        assert_eq!(log[..2], ["release all", "key 40 down"]);
        assert!(log[2..].len() >= 3, "{log:?}");
        assert!(log[2..].iter().all(|line| line == "key 40 down repeat"));

        injector.release_all();
        let applied = injector.apply(vec![], None).blocking_recv().unwrap();
        assert!(applied.is_ok());
        // One more repeat may have gone out first.
        let log = fake.take_log();
        let (repeats, released) = log.split_at(log.len() - 2);
        assert_eq!(released, ["key 40 up", "release all"]);
        assert!(repeats.iter().all(|line| line == "key 40 down repeat"));

        injector
            .apply(vec![key(0x04, true)], None)
            .blocking_recv()
            .unwrap()
            .unwrap();
        drop(injector);
        let log: Vec<_> = fake
            .take_log()
            .into_iter()
            .filter(|line| line != "key 0 down repeat")
            .collect();
        assert_eq!(log, ["key 0 down", "key 0 up", "release all"]);
    }

    #[test]
    fn a_failed_batch_answers_with_an_error() {
        let fake = FakeBackend::default();
        let injector = Injector::start(fake.clone(), Profile::default()).unwrap();
        fake.state().fail = true;
        let applied = injector
            .apply(vec![key(0x04, true)], None)
            .blocking_recv()
            .unwrap();
        assert!(applied.is_err());
    }

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
    fn scroll_keeps_its_axes_and_units_and_turns_right_into_cg_left() {
        let native = Posted::Scroll {
            scroll: Scroll::Lines { x: -1, y: 2 },
            flags: 0,
        }
        .native();
        assert_eq!(
            (native.kind, native.wheel_x, native.wheel_y, native.pixel),
            (5, 1, 2, 0)
        );
        let native = Posted::Scroll {
            scroll: Scroll::Pixels { x: 7, y: -9 },
            flags: 0,
        }
        .native();
        assert_eq!((native.wheel_x, native.wheel_y, native.pixel), (-7, -9, 1));
        let native = Posted::Scroll {
            scroll: Scroll::Pixels { x: i32::MIN, y: 0 },
            flags: 0,
        }
        .native();
        assert_eq!(native.wheel_x, i32::MAX);
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
