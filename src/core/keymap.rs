//! Pure per-peer keyboard modes: physical key edges in, injected edges out.
//!
//! The wire, the core receiver and its snapshots stay physical. This stage
//! runs on the receiver after admission and before the input backend. It
//! records which physical keys keep each injected key down, so a physical
//! release only ever releases, and once every physical key is up nothing it
//! injected is left down.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::HidUsage;

/// How keys from one peer are interpreted on this machine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyboardMode {
    /// Keys as sent: a Mac's Cmd is Super and Option is Alt.
    #[default]
    Standard,
    /// Option and Cmd trade places on both sides, so each key does what the
    /// PC key in that spot does. The file also takes the CLI's spelling.
    #[serde(alias = "pc-positions")]
    PcPositions,
    /// Cmd is Ctrl, plus a small table of macOS shortcuts.
    Mac,
}

impl KeyboardMode {
    pub fn is_standard(&self) -> bool {
        *self == Self::Standard
    }
}

/// Remaps one peer's physical key edges to the edges to inject.
///
/// Every injected key has a set of physical owners and goes up when the last
/// one is released. Presses happen only while handling a physical press or a
/// pointer event.
#[derive(Debug, Clone)]
pub struct KeyRemap {
    mode: KeyboardMode,
    terminal: bool,
    pointer_busy: bool,
    physical: BTreeSet<HidUsage>,
    /// Option keys held back until the next key or click, so a lone tap never
    /// reaches the desktop as Alt and opens a menu bar.
    deferred: BTreeSet<HidUsage>,
    /// Injected key to the physical keys keeping it down. A key is injected
    /// exactly when it has an entry.
    owners: BTreeMap<HidUsage, BTreeSet<HidUsage>>,
    /// Outputs of an active `Hold` rule. While set, keys skip the rules so the
    /// app switcher stays open.
    bound: Option<BTreeSet<HidUsage>>,
}

impl KeyRemap {
    pub fn new(mode: KeyboardMode) -> Self {
        Self {
            mode,
            terminal: false,
            pointer_busy: false,
            physical: BTreeSet::new(),
            deferred: BTreeSet::new(),
            owners: BTreeMap::new(),
            bound: None,
        }
    }

    pub fn mode(&self) -> KeyboardMode {
        self.mode
    }

    /// Forgets every held key and switches mode. The caller has already
    /// released every injected key. The terminal bit describes the desktop,
    /// not this peer's keys, so it survives.
    pub fn reset(&mut self, mode: KeyboardMode) {
        *self = Self {
            terminal: self.terminal,
            ..Self::new(mode)
        };
    }

    /// Whether the focused app is a terminal. It is read only when a key is
    /// pressed; keys already down keep what they injected.
    pub fn set_terminal(&mut self, terminal: bool) {
        self.terminal = terminal;
    }

    /// Maps one physical key edge to the edges to inject, in order. A repeated
    /// press or a release of a key that is not held maps to nothing.
    pub fn key(&mut self, usage: HidUsage, pressed: bool) -> Vec<(HidUsage, bool)> {
        let mut edges = Vec::new();
        if !pressed {
            if self.physical.remove(&usage) {
                self.release(usage, &mut edges);
            }
        } else if self.physical.insert(usage) {
            match modifier_class(usage) {
                Some(class) => self.press_modifier(usage, class, &mut edges),
                None => self.press_key(usage, &mut edges),
            }
        }
        edges
    }

    /// Call before a pointer button edge, a scroll, or a touch replacement.
    /// `busy` says whether a button or touch contact is still held after that
    /// event. Held-back Option keys are pressed now, so Option+click works.
    pub fn pointer(&mut self, busy: bool) -> Vec<(HidUsage, bool)> {
        let mut edges = Vec::new();
        for modifier in std::mem::take(&mut self.deferred) {
            self.own(default_output(self.mode, modifier), modifier, &mut edges);
        }
        self.pointer_busy = busy;
        edges
    }

    /// Call after a button or touch release in place of [`Self::pointer`].
    /// A release presses nothing: its press may never have been delivered,
    /// as when the seat is on hold, so held-back Option keys stay back.
    pub fn pointer_released(&mut self, busy: bool) {
        self.pointer_busy = busy;
    }

    /// Keys currently injected.
    pub fn injected(&self) -> impl Iterator<Item = HidUsage> + '_ {
        self.owners.keys().copied()
    }

    fn press_modifier(&mut self, modifier: HidUsage, class: u8, edges: &mut Vec<(HidUsage, bool)>) {
        // With another key or a button already down, Option is part of a game
        // or a drag and goes out at once.
        let hold_back = self.mode == KeyboardMode::Mac
            && class == OPT
            && !self.pointer_busy
            && self.physical.iter().all(|&held| is_modifier(held));
        if hold_back {
            self.deferred.insert(modifier);
        } else {
            self.own(default_output(self.mode, modifier), modifier, edges);
        }
    }

    fn press_key(&mut self, key: HidUsage, edges: &mut Vec<(HidUsage, bool)>) {
        if self.bound.is_some() {
            // The app switcher is open. Keep its Alt and send the key as is,
            // so Tab, Esc and Q reach the switcher instead of a rule.
            let idle: Vec<_> = self
                .physical
                .iter()
                .copied()
                .filter(|&held| is_modifier(held) && !self.owns_any(held))
                .collect();
            for modifier in idle {
                self.own(default_output(self.mode, modifier), modifier, edges);
            }
            self.deferred.clear();
            self.own(key, key, edges);
            return;
        }
        let Some(rule) = self.lookup(key) else {
            self.set_modifiers(0, &[], &[], key, edges);
            self.own(key, key, edges);
            return;
        };
        match rule.action {
            Action::Block => {}
            Action::Tap(output) => {
                self.set_modifiers(rule.held, &[], &[], key, edges);
                if !self.owners.contains_key(&output) {
                    edges.extend([(output, true), (output, false)]);
                }
            }
            Action::Press(mods, output) => {
                self.set_modifiers(rule.held, mods, &[], key, edges);
                self.own(output.resolve(key), key, edges);
            }
            Action::PressWith(mods, key_mods, output) => {
                self.set_modifiers(rule.held, mods, key_mods, key, edges);
                self.own(output.resolve(key), key, edges);
            }
            Action::Hold(mods, output) => {
                self.set_modifiers(rule.held, mods, &[], key, edges);
                self.own(output.resolve(key), key, edges);
                let bound: BTreeSet<_> = mods
                    .iter()
                    .copied()
                    .filter(|output| self.owners.contains_key(output))
                    .collect();
                self.bound = (!bound.is_empty()).then_some(bound);
            }
        }
    }

    /// Makes the injected modifiers exactly the wanted set: `mods` owned by the
    /// held modifiers of the `consumed` classes, `key_mods` owned by `key`, and
    /// every other held modifier's default owned by itself.
    fn set_modifiers(
        &mut self,
        consumed: u8,
        mods: &[HidUsage],
        key_mods: &[HidUsage],
        key: HidUsage,
        edges: &mut Vec<(HidUsage, bool)>,
    ) {
        let mut wanted: BTreeMap<HidUsage, BTreeSet<HidUsage>> = BTreeMap::new();
        for &held in &self.physical {
            let Some(class) = modifier_class(held) else {
                continue;
            };
            if class & consumed == 0 {
                wanted
                    .entry(default_output(self.mode, held))
                    .or_default()
                    .insert(held);
            } else {
                for &output in mods {
                    wanted.entry(output).or_default().insert(held);
                }
            }
        }
        for &output in key_mods {
            wanted.entry(output).or_default().insert(key);
        }
        self.deferred.clear();

        let unwanted: Vec<_> = self
            .owners
            .keys()
            .copied()
            .filter(|&output| is_modifier(output) && !wanted.contains_key(&output))
            .collect();
        for output in unwanted {
            self.owners.remove(&output);
            edges.push((output, false));
        }
        for (output, owners) in wanted {
            if self.owners.insert(output, owners).is_none() {
                edges.push((output, true));
            }
        }
    }

    fn release(&mut self, key: HidUsage, edges: &mut Vec<(HidUsage, bool)>) {
        self.deferred.remove(&key);
        let mut freed = Vec::new();
        self.owners.retain(|&output, owners| {
            owners.remove(&key);
            if owners.is_empty() {
                freed.push(output);
            }
            !owners.is_empty()
        });
        // Keys before modifiers, so a released chord never leaves its bare
        // key down for the client to repeat.
        freed.sort_by_key(|&output| is_modifier(output));
        for output in freed {
            if self
                .bound
                .as_ref()
                .is_some_and(|bound| bound.contains(&output))
            {
                self.bound = None;
            }
            edges.push((output, false));
        }
    }

    fn own(&mut self, output: HidUsage, owner: HidUsage, edges: &mut Vec<(HidUsage, bool)>) {
        self.owners
            .entry(output)
            .or_insert_with(|| {
                edges.push((output, true));
                BTreeSet::new()
            })
            .insert(owner);
    }

    fn owns_any(&self, key: HidUsage) -> bool {
        self.owners.values().any(|owners| owners.contains(&key))
    }

    fn lookup(&self, key: HidUsage) -> Option<&'static Rule> {
        if self.mode != KeyboardMode::Mac {
            return None;
        }
        let held = self
            .physical
            .iter()
            .filter_map(|&held| modifier_class(held))
            .fold(0, |classes, class| classes | class);
        RULES
            .iter()
            .find(|rule| rule.matches(self.terminal, held, key))
    }
}

// Physical modifier classes. Each names the left and right key.
const CMD: u8 = 1 << 0;
const OPT: u8 = 1 << 1;
const CTRL: u8 = 1 << 2;
const SHIFT: u8 = 1 << 3;

fn modifier_class(usage: HidUsage) -> Option<u8> {
    match usage {
        LCTRL | RCTRL => Some(CTRL),
        LSHIFT | RSHIFT => Some(SHIFT),
        LALT | RALT => Some(OPT),
        LGUI | RGUI => Some(CMD),
        _ => None,
    }
}

fn is_modifier(usage: HidUsage) -> bool {
    modifier_class(usage).is_some()
}

/// What a held physical modifier injects when no rule consumes it.
fn default_output(mode: KeyboardMode, modifier: HidUsage) -> HidUsage {
    match (mode, modifier) {
        (KeyboardMode::PcPositions, LALT) => LGUI,
        (KeyboardMode::PcPositions, LGUI) => LALT,
        (KeyboardMode::PcPositions, RALT) => RGUI,
        (KeyboardMode::PcPositions, RGUI) => RALT,
        (KeyboardMode::Mac, LGUI) => LCTRL,
        (KeyboardMode::Mac, RGUI) => RCTRL,
        _ => modifier,
    }
}

#[derive(Debug, Clone, Copy)]
enum Context {
    Terminal,
    Any,
    Gui,
}

#[derive(Debug, Clone, Copy)]
enum Out {
    /// The physical key itself.
    Same,
    Key(HidUsage),
}

impl Out {
    fn resolve(self, key: HidUsage) -> HidUsage {
        match self {
            Self::Same => key,
            Self::Key(output) => output,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Action {
    /// Press the modifiers, owned by the consumed physical modifiers, then
    /// the output key, owned by the physical key.
    Press(&'static [HidUsage], Out),
    /// `Press`, plus the second set of modifiers owned by the physical key, so
    /// they lift with it.
    PressWith(&'static [HidUsage], &'static [HidUsage], Out),
    /// `Press`, and keep the modifiers down for the keys that follow until
    /// they are released.
    Hold(&'static [HidUsage], Out),
    /// Press and release one key with no other modifier down.
    Tap(HidUsage),
    /// Inject nothing.
    Block,
}

/// One row of the Mac shortcut table.
#[derive(Debug, Clone, Copy)]
struct Rule {
    context: Context,
    /// Classes that must be held. Cmd, Option and Ctrl match exactly. Shift
    /// passes through unless named here; then it must be held and the rule
    /// consumes it.
    held: u8,
    /// Physical keys the row matches. [`ANY_KEY`] matches every key that is
    /// not a modifier.
    keys: &'static [HidUsage],
    action: Action,
}

impl Rule {
    fn matches(&self, terminal: bool, held: u8, key: HidUsage) -> bool {
        let context = match self.context {
            Context::Terminal => terminal,
            Context::Any => true,
            Context::Gui => !terminal,
        };
        let exact = CMD | OPT | CTRL;
        context
            && held & exact == self.held & exact
            && (self.held & SHIFT == 0 || held & SHIFT != 0)
            && (self.keys.is_empty() || self.keys.contains(&key))
    }
}

const fn rule(context: Context, held: u8, keys: &'static [HidUsage], action: Action) -> Rule {
    Rule {
        context,
        held,
        keys,
        action,
    }
}

const ANY_KEY: &[HidUsage] = &[];

/// Mac mode shortcuts. The first matching row wins, so the row order is the
/// lookup order: rows that need Shift, terminal rows, rows for every app, GUI
/// rows, then the terminal catch-all.
#[rustfmt::skip]
const RULES: &[Rule] = {
    use Action::*;
    use Context::*;
    use Out::*;
    &[
        // Rows that need Shift go first. They never shadow a terminal row, and
        // the terminal Cmd+3 row would turn Shift+Cmd+3 into Alt+#, which
        // comments out the shell's line and runs it.
        rule(Any, CMD | SHIFT, &[LBRACKET],                    Press(&[LCTRL], Key(PAGE_UP))),
        rule(Any, CMD | SHIFT, &[RBRACKET],                    Press(&[LCTRL], Key(PAGE_DOWN))),
        rule(Any, CMD | SHIFT, &[N3],                          Press(&[LSHIFT], Key(PRINT_SCREEN))),
        rule(Any, CMD | SHIFT, &[N4, N5],                      Press(&[], Key(PRINT_SCREEN))),
        // Terminal copy, paste and tabs add Shift, so Cmd never sends a control byte.
        // Not Cmd+A: GNOME Terminal binds nothing to Ctrl+Shift+A, so it would be ^A.
        rule(Terminal, CMD, &[C, V, T, N, W, F, Q],    PressWith(&[LCTRL], &[LSHIFT], Same)),
        rule(Terminal, CMD, &[K],                      Press(&[LCTRL], Key(L))),
        // Zoom in is Ctrl+plus, and GTK only sees plus with Shift down.
        rule(Terminal, CMD, &[EQUAL],                  PressWith(&[LCTRL], &[LSHIFT], Same)),
        rule(Terminal, CMD, &[MINUS, N0],              Press(&[LCTRL], Same)),
        rule(Terminal, CMD, &[N1, N2, N3, N4, N5],     Press(&[LALT], Same)),
        rule(Terminal, CMD, &[N6, N7, N8, N9],         Press(&[LALT], Same)),
        rule(Terminal, OPT, &[LEFT],                   Press(&[LALT], Key(B))),
        rule(Terminal, OPT, &[RIGHT],                  Press(&[LALT], Key(F))),
        rule(Terminal, OPT, &[BACKSPACE],              Press(&[LCTRL], Key(W))),
        rule(Terminal, CMD, &[BACKSPACE],              Press(&[LCTRL], Key(U))),
        // Every app.
        rule(Any, CMD,         &[TAB],                         Hold(&[LALT], Same)),
        rule(Any, CMD,         &[GRAVE, NON_US_BACKSLASH],     Hold(&[LALT], Key(GRAVE))),
        rule(Any, CMD,         &[SPACE],                       Tap(LGUI)),
        rule(Any, CMD,         &[LEFT],                        Press(&[], Key(HOME))),
        rule(Any, CMD,         &[RIGHT],                       Press(&[], Key(END))),
        rule(Any, CMD,         &[UP],                          Press(&[LCTRL], Key(HOME))),
        rule(Any, CMD,         &[DOWN],                        Press(&[LCTRL], Key(END))),
        rule(Any, CMD,         &[H, M],                        Press(&[LGUI], Key(H))),
        rule(Any, CTRL | CMD,  &[Q],                           Press(&[LGUI], Key(L))),
        rule(Any, CTRL | CMD,  &[F],                           Press(&[], Key(F11))),
        rule(Any, CTRL | CMD,  ANY_KEY,                        Block),
        rule(Any, CTRL | CMD | OPT, ANY_KEY,                   Block),
        // Ctrl+Alt+Fn switches VT, Ctrl+Alt+Delete logs out and Ctrl+Alt+arrows
        // switch workspace. Cmd+Option+Left and Right change tabs instead.
        rule(Any, CMD | OPT,   &[LEFT],                        Press(&[LCTRL], Key(PAGE_UP))),
        rule(Any, CMD | OPT,   &[RIGHT],                       Press(&[LCTRL], Key(PAGE_DOWN))),
        rule(Any, CMD | OPT,   &[F1, F2, F3, F4, F5, F6],      Block),
        rule(Any, CMD | OPT,   &[F7, F8, F9, F10, F11, F12],   Block),
        rule(Any, CMD | OPT,   &[BACKSPACE, DELETE, UP, DOWN], Block),
        rule(Any, CTRL,        &[LEFT],                        Press(&[LGUI], Key(PAGE_UP))),
        rule(Any, CTRL,        &[RIGHT],                       Press(&[LGUI], Key(PAGE_DOWN))),
        rule(Any, CTRL,        &[UP],                          Tap(LGUI)),
        // GUI apps. Physical Ctrl stays Ctrl in terminals.
        rule(Gui, OPT,  &[LEFT, RIGHT, BACKSPACE, DELETE], Press(&[LCTRL], Same)),
        rule(Gui, CTRL, &[A],                              Press(&[], Key(HOME))),
        rule(Gui, CTRL, &[E],                              Press(&[], Key(END))),
        rule(Gui, CTRL, &[B],                              Press(&[], Key(LEFT))),
        rule(Gui, CTRL, &[F],                              Press(&[], Key(RIGHT))),
        rule(Gui, CTRL, &[N],                              Press(&[], Key(DOWN))),
        rule(Gui, CTRL, &[P],                              Press(&[], Key(UP))),
        rule(Gui, CTRL, &[D],                              Press(&[], Key(DELETE))),
        rule(Gui, CTRL, &[H],                              Press(&[], Key(BACKSPACE))),
        // Any other Cmd chord never reaches a terminal, so Cmd+D is not EOF and
        // Cmd+Z does not suspend. Ctrl+Cmd is already blocked above.
        rule(Terminal, CMD,       ANY_KEY, Block),
        rule(Terminal, CMD | OPT, ANY_KEY, Block),
    ]
};

/// Every usage a rule can inject, so a backend can check it types them all.
pub fn output_usages() -> BTreeSet<HidUsage> {
    let mut outputs = BTreeSet::new();
    for rule in RULES {
        let (mods, key_mods, out): (&[HidUsage], &[HidUsage], Out) = match rule.action {
            Action::Press(mods, out) | Action::Hold(mods, out) => (mods, &[], out),
            Action::PressWith(mods, key_mods, out) => (mods, key_mods, out),
            Action::Tap(output) => (&[], &[], Out::Key(output)),
            Action::Block => continue,
        };
        outputs.extend(mods.iter().chain(key_mods));
        match out {
            Out::Same => outputs.extend(rule.keys),
            Out::Key(output) => {
                outputs.insert(output);
            }
        }
    }
    outputs
}

// HID keyboard page usages. Mac Option is Alt and Cmd is GUI.
const LCTRL: HidUsage = HidUsage::keyboard(0xe0);
const LSHIFT: HidUsage = HidUsage::keyboard(0xe1);
const LALT: HidUsage = HidUsage::keyboard(0xe2);
const LGUI: HidUsage = HidUsage::keyboard(0xe3);
const RCTRL: HidUsage = HidUsage::keyboard(0xe4);
const RSHIFT: HidUsage = HidUsage::keyboard(0xe5);
const RALT: HidUsage = HidUsage::keyboard(0xe6);
const RGUI: HidUsage = HidUsage::keyboard(0xe7);

const A: HidUsage = HidUsage::keyboard(0x04);
const B: HidUsage = HidUsage::keyboard(0x05);
const C: HidUsage = HidUsage::keyboard(0x06);
const D: HidUsage = HidUsage::keyboard(0x07);
const E: HidUsage = HidUsage::keyboard(0x08);
const F: HidUsage = HidUsage::keyboard(0x09);
const H: HidUsage = HidUsage::keyboard(0x0b);
const K: HidUsage = HidUsage::keyboard(0x0e);
const L: HidUsage = HidUsage::keyboard(0x0f);
const M: HidUsage = HidUsage::keyboard(0x10);
const N: HidUsage = HidUsage::keyboard(0x11);
const P: HidUsage = HidUsage::keyboard(0x13);
const Q: HidUsage = HidUsage::keyboard(0x14);
const T: HidUsage = HidUsage::keyboard(0x17);
const U: HidUsage = HidUsage::keyboard(0x18);
const V: HidUsage = HidUsage::keyboard(0x19);
const W: HidUsage = HidUsage::keyboard(0x1a);

// Number row.
const N1: HidUsage = HidUsage::keyboard(0x1e);
const N2: HidUsage = HidUsage::keyboard(0x1f);
const N3: HidUsage = HidUsage::keyboard(0x20);
const N4: HidUsage = HidUsage::keyboard(0x21);
const N5: HidUsage = HidUsage::keyboard(0x22);
const N6: HidUsage = HidUsage::keyboard(0x23);
const N7: HidUsage = HidUsage::keyboard(0x24);
const N8: HidUsage = HidUsage::keyboard(0x25);
const N9: HidUsage = HidUsage::keyboard(0x26);
const N0: HidUsage = HidUsage::keyboard(0x27);

const BACKSPACE: HidUsage = HidUsage::keyboard(0x2a);
const TAB: HidUsage = HidUsage::keyboard(0x2b);
const SPACE: HidUsage = HidUsage::keyboard(0x2c);
const MINUS: HidUsage = HidUsage::keyboard(0x2d);
const EQUAL: HidUsage = HidUsage::keyboard(0x2e);
const LBRACKET: HidUsage = HidUsage::keyboard(0x2f);
const RBRACKET: HidUsage = HidUsage::keyboard(0x30);
const GRAVE: HidUsage = HidUsage::keyboard(0x35);
const NON_US_BACKSLASH: HidUsage = HidUsage::keyboard(0x64);

const F1: HidUsage = HidUsage::keyboard(0x3a);
const F2: HidUsage = HidUsage::keyboard(0x3b);
const F3: HidUsage = HidUsage::keyboard(0x3c);
const F4: HidUsage = HidUsage::keyboard(0x3d);
const F5: HidUsage = HidUsage::keyboard(0x3e);
const F6: HidUsage = HidUsage::keyboard(0x3f);
const F7: HidUsage = HidUsage::keyboard(0x40);
const F8: HidUsage = HidUsage::keyboard(0x41);
const F9: HidUsage = HidUsage::keyboard(0x42);
const F10: HidUsage = HidUsage::keyboard(0x43);
const F11: HidUsage = HidUsage::keyboard(0x44);
const F12: HidUsage = HidUsage::keyboard(0x45);

const PRINT_SCREEN: HidUsage = HidUsage::keyboard(0x46);
const HOME: HidUsage = HidUsage::keyboard(0x4a);
const PAGE_UP: HidUsage = HidUsage::keyboard(0x4b);
const DELETE: HidUsage = HidUsage::keyboard(0x4c);
const END: HidUsage = HidUsage::keyboard(0x4d);
const PAGE_DOWN: HidUsage = HidUsage::keyboard(0x4e);
const RIGHT: HidUsage = HidUsage::keyboard(0x4f);
const LEFT: HidUsage = HidUsage::keyboard(0x50);
const DOWN: HidUsage = HidUsage::keyboard(0x51);
const UP: HidUsage = HidUsage::keyboard(0x52);

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use Step::*;

    // Physical Mac keys. Ctrl and Shift keep their own names.
    const LCMD: HidUsage = LGUI;
    const RCMD: HidUsage = RGUI;
    const LOPT: HidUsage = LALT;
    const ROPT: HidUsage = RALT;

    const X: HidUsage = HidUsage::keyboard(0x1b);
    const Z: HidUsage = HidUsage::keyboard(0x1d);
    const ESCAPE: HidUsage = HidUsage::keyboard(0x29);
    const VOLUME_UP: HidUsage = HidUsage::consumer(0xe9);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Step {
        Down(HidUsage),
        Up(HidUsage),
        Terminal(bool),
        /// A click, a scroll or a touch, with whether the pointer is busy after.
        Pointer(bool),
        /// A button or touch release, with whether the pointer is busy after.
        PointerUp(bool),
    }

    type Case = (&'static str, &'static [Step], &'static [Step]);

    fn replay(mode: KeyboardMode, steps: &[Step]) -> Vec<Step> {
        let mut remap = KeyRemap::new(mode);
        let mut injected = Vec::new();
        for &step in steps {
            let edges = match step {
                Down(key) => remap.key(key, true),
                Up(key) => remap.key(key, false),
                Terminal(terminal) => {
                    remap.set_terminal(terminal);
                    Vec::new()
                }
                Pointer(busy) => remap.pointer(busy),
                PointerUp(busy) => {
                    remap.pointer_released(busy);
                    Vec::new()
                }
            };
            injected.extend(edges.into_iter().map(
                |(key, pressed)| {
                    if pressed { Down(key) } else { Up(key) }
                },
            ));
        }
        injected
    }

    fn check(mode: KeyboardMode, cases: &[Case]) {
        for (name, physical, injected) in cases {
            assert_eq!(replay(mode, physical), *injected, "{name}");
        }
    }

    const STANDARD: &[Case] = &[
        (
            "cmd+c",
            &[Down(LCMD), Down(C), Up(C), Up(LCMD)],
            &[Down(LCMD), Down(C), Up(C), Up(LCMD)],
        ),
        (
            "option tap",
            &[Down(LOPT), Up(LOPT)],
            &[Down(LOPT), Up(LOPT)],
        ),
        (
            "terminal bit and pointer change nothing",
            &[
                Terminal(true),
                Down(LCMD),
                Down(D),
                Pointer(true),
                Up(D),
                Up(LCMD),
            ],
            &[Down(LCMD), Down(D), Up(D), Up(LCMD)],
        ),
        (
            "duplicate press and stray release",
            &[Down(C), Down(C), Up(D), Up(C), Up(C)],
            &[Down(C), Up(C)],
        ),
    ];

    const PC_POSITIONS: &[Case] = &[
        (
            "left option and cmd swap",
            &[Down(LOPT), Down(LCMD), Up(LOPT), Up(LCMD)],
            &[Down(LGUI), Down(LALT), Up(LGUI), Up(LALT)],
        ),
        (
            "right option and cmd swap",
            &[Down(ROPT), Down(RCMD), Up(RCMD), Up(ROPT)],
            &[Down(RGUI), Down(RALT), Up(RALT), Up(RGUI)],
        ),
        (
            "ctrl and shift stay",
            &[Down(LCTRL), Down(RSHIFT), Down(C)],
            &[Down(LCTRL), Down(RSHIFT), Down(C)],
        ),
        (
            "cmd+tab is alt+tab",
            &[Down(LCMD), Down(TAB), Up(TAB), Up(LCMD)],
            &[Down(LALT), Down(TAB), Up(TAB), Up(LALT)],
        ),
    ];

    const MAC: &[Case] = &[
        (
            "cmd+c is ctrl+c",
            &[Down(LCMD), Down(C), Up(C), Up(LCMD)],
            &[Down(LCTRL), Down(C), Up(C), Up(LCTRL)],
        ),
        (
            "right cmd is right ctrl",
            &[Down(RCMD), Down(C), Up(C), Up(RCMD)],
            &[Down(RCTRL), Down(C), Up(C), Up(RCTRL)],
        ),
        (
            "lone cmd tap is a ctrl tap",
            &[Down(LCMD), Up(LCMD)],
            &[Down(LCTRL), Up(LCTRL)],
        ),
        (
            "option+left jumps a word without alt",
            &[Down(LOPT), Down(LEFT), Up(LEFT), Up(LOPT)],
            &[Down(LCTRL), Down(LEFT), Up(LEFT), Up(LCTRL)],
        ),
        (
            "right option+left jumps a word too",
            &[Down(ROPT), Down(LEFT), Up(LEFT), Up(ROPT)],
            &[Down(LCTRL), Down(LEFT), Up(LEFT), Up(LCTRL)],
        ),
        (
            "option+backspace deletes a word",
            &[Down(LOPT), Down(BACKSPACE)],
            &[Down(LCTRL), Down(BACKSPACE)],
        ),
        (
            "lone option tap injects nothing",
            &[Down(LOPT), Up(LOPT)],
            &[],
        ),
        (
            "option+key is alt+key",
            &[Down(LOPT), Down(X)],
            &[Down(LALT), Down(X)],
        ),
        (
            "right option types through altgr",
            &[Down(ROPT), Down(E), Up(E), Up(ROPT)],
            &[Down(RALT), Down(E), Up(E), Up(RALT)],
        ),
        (
            "cmd+tab keeps alt down",
            &[Down(LCMD), Down(TAB), Up(TAB), Down(TAB), Up(TAB), Up(LCMD)],
            &[
                Down(LCTRL),
                Up(LCTRL),
                Down(LALT),
                Down(TAB),
                Up(TAB),
                Down(TAB),
                Up(TAB),
                Up(LALT),
            ],
        ),
        (
            "escape goes to the open switcher",
            &[
                Down(LCMD),
                Down(TAB),
                Up(TAB),
                Down(ESCAPE),
                Up(ESCAPE),
                Up(LCMD),
            ],
            &[
                Down(LCTRL),
                Up(LCTRL),
                Down(LALT),
                Down(TAB),
                Up(TAB),
                Down(ESCAPE),
                Up(ESCAPE),
                Up(LALT),
            ],
        ),
        (
            "shift+cmd+tab keeps shift",
            &[Down(LSHIFT), Down(LCMD), Down(TAB)],
            &[Down(LSHIFT), Down(LCTRL), Up(LCTRL), Down(LALT), Down(TAB)],
        ),
        (
            "cmd+grave switches windows of the app",
            &[Down(LCMD), Down(NON_US_BACKSLASH)],
            &[Down(LCTRL), Up(LCTRL), Down(LALT), Down(GRAVE)],
        ),
        (
            "cmd+space taps super alone",
            &[Down(LCMD), Down(SPACE), Up(SPACE), Up(LCMD)],
            &[Down(LCTRL), Up(LCTRL), Down(LGUI), Up(LGUI)],
        ),
        (
            "cmd+left after a copy drops ctrl",
            &[Down(LCMD), Down(C), Up(C), Down(LEFT)],
            &[Down(LCTRL), Down(C), Up(C), Up(LCTRL), Down(HOME)],
        ),
        (
            "cmd+shift+left selects to line start",
            &[Down(LSHIFT), Down(LCMD), Down(LEFT)],
            &[Down(LSHIFT), Down(LCTRL), Up(LCTRL), Down(HOME)],
        ),
        (
            "cmd+up is ctrl+home",
            &[Down(LCMD), Down(UP), Up(UP), Up(LCMD)],
            &[Down(LCTRL), Down(HOME), Up(HOME), Up(LCTRL)],
        ),
        (
            "shift+cmd+bracket consumes shift",
            &[Down(LSHIFT), Down(LCMD), Down(RBRACKET)],
            &[Down(LSHIFT), Down(LCTRL), Up(LSHIFT), Down(PAGE_DOWN)],
        ),
        (
            "shift+cmd+4 is print alone",
            &[Down(LSHIFT), Down(LCMD), Down(N4)],
            &[
                Down(LSHIFT),
                Down(LCTRL),
                Up(LCTRL),
                Up(LSHIFT),
                Down(PRINT_SCREEN),
            ],
        ),
        (
            "shift+cmd+3 is shift+print",
            &[Down(LSHIFT), Down(LCMD), Down(N3)],
            &[Down(LSHIFT), Down(LCTRL), Up(LCTRL), Down(PRINT_SCREEN)],
        ),
        (
            "cmd+m minimizes",
            &[Down(LCMD), Down(M)],
            &[Down(LCTRL), Up(LCTRL), Down(LGUI), Down(H)],
        ),
        (
            "ctrl+cmd+c is blocked and ctrl waits for cmd",
            &[Down(LCTRL), Down(LCMD), Down(C), Up(C), Up(LCTRL)],
            &[Down(LCTRL)],
        ),
        (
            "ctrl lifts with its last owner",
            &[Down(LCTRL), Down(LCMD), Down(C), Up(LCTRL), Up(C), Up(LCMD)],
            &[Down(LCTRL), Up(LCTRL)],
        ),
        (
            "ctrl+cmd+q locks the screen",
            &[Down(LCTRL), Down(LCMD), Down(Q)],
            &[Down(LCTRL), Up(LCTRL), Down(LGUI), Down(L)],
        ),
        (
            "ctrl+cmd+f is full screen",
            &[Down(LCTRL), Down(LCMD), Down(F)],
            &[Down(LCTRL), Up(LCTRL), Down(F11)],
        ),
        (
            "cmd+option+f2 never switches vt",
            &[Down(LCMD), Down(LOPT), Down(F2), Up(F2), Up(LOPT), Up(LCMD)],
            &[Down(LCTRL), Up(LCTRL)],
        ),
        (
            "ctrl+cmd+option+f2 never switches vt",
            &[
                Down(LCTRL),
                Down(LCMD),
                Down(LOPT),
                Down(F2),
                Down(DELETE),
                Up(F2),
                Up(DELETE),
                Up(LOPT),
                Up(LCMD),
                Up(LCTRL),
            ],
            &[Down(LCTRL), Up(LCTRL)],
        ),
        (
            "cmd+option+left is the previous tab",
            &[
                Down(LCMD),
                Down(LOPT),
                Down(LEFT),
                Up(LEFT),
                Up(LOPT),
                Up(LCMD),
            ],
            &[Down(LCTRL), Down(PAGE_UP), Up(PAGE_UP), Up(LCTRL)],
        ),
        (
            "cmd+option+up never switches workspace",
            &[Down(LCMD), Down(LOPT), Down(UP), Up(UP), Up(LOPT), Up(LCMD)],
            &[Down(LCTRL), Up(LCTRL)],
        ),
        (
            "ctrl+left switches workspace",
            &[Down(LCTRL), Down(LEFT), Up(LEFT), Up(LCTRL)],
            &[
                Down(LCTRL),
                Up(LCTRL),
                Down(LGUI),
                Down(PAGE_UP),
                Up(PAGE_UP),
                Up(LGUI),
            ],
        ),
        (
            "ctrl+up taps super",
            &[Down(LCTRL), Down(UP)],
            &[Down(LCTRL), Up(LCTRL), Down(LGUI), Up(LGUI)],
        ),
        (
            "ctrl+a is home outside terminals",
            &[Down(LCTRL), Down(A), Up(A), Up(LCTRL)],
            &[Down(LCTRL), Up(LCTRL), Down(HOME), Up(HOME)],
        ),
        (
            "pointer presses a held-back option",
            &[Down(LOPT), Pointer(true), Up(LOPT)],
            &[Down(LALT), Up(LALT)],
        ),
        (
            "option while a button is down",
            &[Pointer(true), Down(LOPT)],
            &[Down(LALT)],
        ),
        (
            "option waits again once the button is up",
            &[Pointer(true), PointerUp(false), Down(LOPT)],
            &[],
        ),
        (
            "a release whose click was dropped leaves option held back",
            &[Down(LOPT), PointerUp(false), Up(LOPT)],
            &[],
        ),
        (
            "holding w then option",
            &[Down(W), Down(LOPT)],
            &[Down(W), Down(LALT)],
        ),
        (
            "duplicate press and stray release",
            &[Down(C), Down(C), Up(D), Up(C), Up(C)],
            &[Down(C), Up(C)],
        ),
        (
            "consumer key passes and presses a held-back option",
            &[Down(LOPT), Down(VOLUME_UP), Up(VOLUME_UP), Up(LOPT)],
            &[Down(LALT), Down(VOLUME_UP), Up(VOLUME_UP), Up(LALT)],
        ),
    ];

    const MAC_TERMINAL: &[Case] = &[
        (
            "cmd+c copies with shift",
            &[Terminal(true), Down(LCMD), Down(C), Up(C), Up(LCMD)],
            &[
                Down(LCTRL),
                Down(LSHIFT),
                Down(C),
                Up(C),
                Up(LSHIFT),
                Up(LCTRL),
            ],
        ),
        (
            "shift lifts with the key",
            &[Terminal(true), Down(LCMD), Down(C), Up(C)],
            &[Down(LCTRL), Down(LSHIFT), Down(C), Up(C), Up(LSHIFT)],
        ),
        (
            "cmd+d is blocked",
            &[Terminal(true), Down(LCMD), Down(D), Up(D), Up(LCMD)],
            &[Down(LCTRL), Up(LCTRL)],
        ),
        (
            "cmd+option+z is blocked",
            &[Terminal(true), Down(LCMD), Down(LOPT), Down(Z)],
            &[Down(LCTRL)],
        ),
        (
            "cmd+a is blocked, not ^a",
            &[Terminal(true), Down(LCMD), Down(A), Up(A), Up(LCMD)],
            &[Down(LCTRL), Up(LCTRL)],
        ),
        (
            "shift+cmd+3 is a screenshot, not alt+#",
            &[Terminal(true), Down(LSHIFT), Down(LCMD), Down(N3)],
            &[Down(LSHIFT), Down(LCTRL), Up(LCTRL), Down(PRINT_SCREEN)],
        ),
        (
            "shift+cmd+4 is a screenshot, not alt+$",
            &[Terminal(true), Down(LSHIFT), Down(LCMD), Down(N4)],
            &[
                Down(LSHIFT),
                Down(LCTRL),
                Up(LCTRL),
                Up(LSHIFT),
                Down(PRINT_SCREEN),
            ],
        ),
        (
            "cmd+equal zooms in as ctrl+plus",
            &[Terminal(true), Down(LCMD), Down(EQUAL), Up(EQUAL), Up(LCMD)],
            &[
                Down(LCTRL),
                Down(LSHIFT),
                Down(EQUAL),
                Up(EQUAL),
                Up(LSHIFT),
                Up(LCTRL),
            ],
        ),
        (
            "cmd+minus zooms out as ctrl+minus",
            &[Terminal(true), Down(LCMD), Down(MINUS)],
            &[Down(LCTRL), Down(MINUS)],
        ),
        (
            "cmd+option+right is the next tab",
            &[Terminal(true), Down(LCMD), Down(LOPT), Down(RIGHT)],
            &[Down(LCTRL), Down(PAGE_DOWN)],
        ),
        (
            "cmd+tab still switches apps",
            &[Terminal(true), Down(LCMD), Down(TAB)],
            &[Down(LCTRL), Up(LCTRL), Down(LALT), Down(TAB)],
        ),
        (
            "cmd+tab then q quits the app in the switcher",
            &[Terminal(true), Down(LCMD), Down(TAB), Up(TAB), Down(Q)],
            &[
                Down(LCTRL),
                Up(LCTRL),
                Down(LALT),
                Down(TAB),
                Up(TAB),
                Down(Q),
            ],
        ),
        (
            "cmd+k clears the screen",
            &[Terminal(true), Down(LCMD), Down(K)],
            &[Down(LCTRL), Down(L)],
        ),
        (
            "cmd+2 picks tab two",
            &[Terminal(true), Down(LCMD), Down(N2)],
            &[Down(LCTRL), Up(LCTRL), Down(LALT), Down(N2)],
        ),
        (
            "option+left is alt+b",
            &[Terminal(true), Down(LOPT), Down(LEFT)],
            &[Down(LALT), Down(B)],
        ),
        (
            "ctrl+a stays ctrl",
            &[Terminal(true), Down(LCTRL), Down(A)],
            &[Down(LCTRL), Down(A)],
        ),
        (
            "leaving the terminal mid-hold releases by owner",
            &[
                Terminal(true),
                Down(LCMD),
                Down(C),
                Terminal(false),
                Up(C),
                Up(LCMD),
            ],
            &[
                Down(LCTRL),
                Down(LSHIFT),
                Down(C),
                Up(C),
                Up(LSHIFT),
                Up(LCTRL),
            ],
        ),
        (
            "entering the terminal mid-hold releases by owner",
            &[Down(LOPT), Down(LEFT), Terminal(true), Up(LEFT), Up(LOPT)],
            &[Down(LCTRL), Down(LEFT), Up(LEFT), Up(LCTRL)],
        ),
    ];

    #[test]
    fn standard_changes_nothing() {
        check(KeyboardMode::Standard, STANDARD);
    }

    #[test]
    fn pc_positions_swaps_option_and_cmd() {
        check(KeyboardMode::PcPositions, PC_POSITIONS);
    }

    #[test]
    fn mac_maps_shortcuts() {
        check(KeyboardMode::Mac, MAC);
    }

    #[test]
    fn mac_terminal_never_sends_cmd_as_a_control_byte() {
        check(KeyboardMode::Mac, MAC_TERMINAL);
    }

    #[test]
    fn reset_forgets_keys_and_keeps_the_terminal_bit() {
        let mut remap = KeyRemap::new(KeyboardMode::Mac);
        remap.set_terminal(true);
        remap.key(LCMD, true);
        remap.key(C, true);
        remap.reset(KeyboardMode::Mac);

        assert_eq!(remap.injected().count(), 0);
        assert!(remap.key(C, false).is_empty());
        assert!(remap.key(LCMD, false).is_empty());
        assert_eq!(remap.key(LCMD, true), [(LCTRL, true)]);
        assert_eq!(remap.key(C, true), [(LSHIFT, true), (C, true)]);

        remap.reset(KeyboardMode::Standard);
        assert_eq!(remap.mode(), KeyboardMode::Standard);
        assert_eq!(remap.key(LCMD, true), [(LCMD, true)]);
    }

    #[test]
    fn keyboard_mode_uses_config_names() {
        for (mode, name) in [
            (KeyboardMode::Standard, "\"standard\""),
            (KeyboardMode::PcPositions, "\"pc_positions\""),
            (KeyboardMode::Mac, "\"mac\""),
        ] {
            assert_eq!(serde_json::to_string(&mode).unwrap(), name);
            assert_eq!(serde_json::from_str::<KeyboardMode>(name).unwrap(), mode);
        }
        assert!(KeyboardMode::default().is_standard());
        assert!(!KeyboardMode::Mac.is_standard());
    }

    /// Every modifier, keys the rules name, keys they don't, and a Consumer key.
    const POOL: &[HidUsage] = &[
        LCTRL, LSHIFT, LALT, LGUI, RCTRL, RSHIFT, RALT, RGUI, A, C, D, K, Q, F, H, X, N2, N4, TAB,
        GRAVE, SPACE, LEFT, UP, BACKSPACE, F2, LBRACKET, ESCAPE, VOLUME_UP,
    ];

    /// Letters a terminal rule sends as Ctrl+letter on purpose: Cmd+K is
    /// Ctrl+L, Option+Backspace is Ctrl+W, Cmd+Backspace is Ctrl+U.
    const CTRL_LETTERS: &[HidUsage] = &[L, W, U];

    #[derive(Debug, Clone, Copy)]
    enum Op {
        Key(usize, bool),
        Terminal(bool),
        Pointer(bool),
        PointerUp(bool),
    }

    fn ops() -> impl Strategy<Value = Vec<Op>> {
        prop::collection::vec(
            prop_oneof![
                8 => (0..POOL.len(), any::<bool>()).prop_map(|(key, down)| Op::Key(key, down)),
                1 => any::<bool>().prop_map(Op::Terminal),
                1 => any::<bool>().prop_map(Op::Pointer),
                1 => any::<bool>().prop_map(Op::PointerUp),
            ],
            0..96,
        )
    }

    fn mode() -> impl Strategy<Value = KeyboardMode> {
        prop_oneof![
            Just(KeyboardMode::Standard),
            Just(KeyboardMode::PcPositions),
            Just(KeyboardMode::Mac),
        ]
    }

    proptest! {
        /// Required property: injected edges are well formed, a physical
        /// release only releases, and once every physical key is up nothing
        /// is injected. In a terminal, a letter never goes down under Ctrl
        /// without Shift while Cmd is held, unless a rule sends it that way.
        #[test]
        fn injected_keys_follow_physical_keys(
            mode in mode(),
            ops in ops(),
            release_order in Just((0..POOL.len()).collect::<Vec<_>>()).prop_shuffle(),
        ) {
            let mut remap = KeyRemap::new(mode);
            let mut physical = BTreeSet::new();
            let mut down = BTreeSet::new();
            let mut terminal = false;
            let releases = release_order.into_iter().map(|key| Op::Key(key, false));
            for op in ops.into_iter().chain(releases) {
                let (edges, released, check_ctrl) = match op {
                    Op::Key(key, pressed) => {
                        let usage = POOL[key];
                        // While a Hold rule keeps Alt down the switcher has
                        // the keyboard, so keys then never reach the terminal.
                        let check_ctrl = mode == KeyboardMode::Mac
                            && terminal
                            && (physical.contains(&LCMD) || physical.contains(&RCMD))
                            && remap.bound.is_none();
                        if pressed {
                            physical.insert(usage);
                        } else {
                            physical.remove(&usage);
                        }
                        (remap.key(usage, pressed), !pressed, check_ctrl)
                    }
                    Op::Terminal(value) => {
                        terminal = value;
                        remap.set_terminal(value);
                        continue;
                    }
                    Op::Pointer(busy) => (remap.pointer(busy), false, false),
                    Op::PointerUp(busy) => {
                        remap.pointer_released(busy);
                        continue;
                    }
                };
                for (usage, pressed) in edges {
                    if pressed {
                        prop_assert!(!released, "release injected a press of {usage:?}");
                        prop_assert!(down.insert(usage), "double press of {usage:?}");
                        let letter = (A..=Z).contains(&usage) && !CTRL_LETTERS.contains(&usage);
                        if check_ctrl && letter {
                            let ctrl = down.contains(&LCTRL) || down.contains(&RCTRL);
                            let shift = down.contains(&LSHIFT) || down.contains(&RSHIFT);
                            prop_assert!(!ctrl || shift, "Cmd sent {usage:?} as a control byte");
                        }
                    } else {
                        prop_assert!(down.remove(&usage), "release of {usage:?} which is up");
                    }
                }
                prop_assert_eq!(&remap.injected().collect::<BTreeSet<_>>(), &down);
            }
            prop_assert!(physical.is_empty());
            prop_assert!(down.is_empty());
        }

        /// Required property: standard maps every edge to itself, and
        /// PC positions applied twice changes nothing.
        #[test]
        fn standard_is_identity_and_pc_positions_undoes_itself(ops in ops()) {
            let mut standard = KeyRemap::new(KeyboardMode::Standard);
            let mut pc = KeyRemap::new(KeyboardMode::PcPositions);
            let mut pc_again = KeyRemap::new(KeyboardMode::PcPositions);
            let mut physical = BTreeSet::new();
            for op in ops {
                let (usage, pressed) = match op {
                    Op::Key(key, pressed) => (POOL[key], pressed),
                    Op::Terminal(terminal) => {
                        for remap in [&mut standard, &mut pc, &mut pc_again] {
                            remap.set_terminal(terminal);
                        }
                        continue;
                    }
                    Op::Pointer(busy) => {
                        for remap in [&mut standard, &mut pc, &mut pc_again] {
                            prop_assert!(remap.pointer(busy).is_empty());
                        }
                        continue;
                    }
                    Op::PointerUp(busy) => {
                        for remap in [&mut standard, &mut pc, &mut pc_again] {
                            remap.pointer_released(busy);
                        }
                        continue;
                    }
                };
                let changed = if pressed {
                    physical.insert(usage)
                } else {
                    physical.remove(&usage)
                };
                let expected = if changed { vec![(usage, pressed)] } else { Vec::new() };
                prop_assert_eq!(&standard.key(usage, pressed), &expected);
                let twice: Vec<_> = pc
                    .key(usage, pressed)
                    .into_iter()
                    .flat_map(|(usage, pressed)| pc_again.key(usage, pressed))
                    .collect();
                prop_assert_eq!(&twice, &expected);
            }
        }
    }
}
