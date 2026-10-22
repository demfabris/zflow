//! [`ColorPicker`]: a swatch that opens a saturation and brightness field
//! over a hue slider, and a hex field.

use zz_gpui::{
    Anchor, AnyElement, App, AppContext as _, Bounds, Context, Entity, EventEmitter, FocusHandle,
    Focusable, HitboxBehavior, Hsla, InteractiveElement as _, IntoElement, MouseButton,
    MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, Point, RenderOnce, SharedString,
    StatefulInteractiveElement as _, StyleRefinement, Styled as _, Subscription, TouchDragEvent,
    TouchPhase, Window, canvas, div, linear_color_stop, linear_gradient,
    prelude::FluentBuilder as _, px, relative,
};

use crate::{
    ActiveTheme as _, Colorize as _, Disableable, Kind, PaintState, Sizable, Size, StyledExt as _,
    Surface,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState, field_chrome, field_text},
    popover::Popover,
    sheet::{bottom_sheet, floating_sheet, sheet_close, sheet_inset},
    slider::{Slider, thumb},
    v_flex,
};

const TRIGGER_SWATCH_SIZE: Pixels = px(16.0);
const TOUCH_TRIGGER_SWATCH: Pixels = px(24.0);
const FIELD_SWATCH: Pixels = px(20.0);
const POPOVER_WIDTH: f32 = 248.0;
const FIELD_HEIGHT: f32 = 148.0;
const SHEET_FIELD_HEIGHT: f32 = 200.0;
const SHEET_PADDING_X: f32 = 16.0;
const HANDLE: f32 = 14.0;
const HUE_STOPS: u16 = 12;

pub enum ColorPickerEvent {
    /// The user committed a color, or cleared it back to inherited (`None`).
    Change(Option<Hsla>),
}

/// A color as its hue (0 to 1), saturation and brightness.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Hsv {
    hue: f32,
    saturation: f32,
    value: f32,
}

impl Hsv {
    fn of(color: Hsla) -> Self {
        let value = color.l + color.s * color.l.min(1.0 - color.l);
        let saturation = if value > 0.0 {
            2.0 * (1.0 - color.l / value)
        } else {
            0.0
        };
        Self {
            hue: color.h,
            saturation: saturation.clamp(0.0, 1.0),
            value: value.clamp(0.0, 1.0),
        }
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "the picker shows any color, not a theme color"
    )]
    fn color(self) -> Hsla {
        let lightness = self.value * (1.0 - self.saturation / 2.0);
        let saturation = if lightness > 0.0 && lightness < 1.0 {
            (self.value - lightness) / lightness.min(1.0 - lightness)
        } else {
            0.0
        };
        zz_gpui::hsla(self.hue, saturation, lightness, 1.0)
    }

    fn pure(self) -> Hsla {
        Self {
            saturation: 1.0,
            value: 1.0,
            ..self
        }
        .color()
    }
}

pub struct ColorPickerState {
    hex: Entity<InputState>,
    color: Option<Hsla>,
    picked: Option<Hsv>,
    field: Bounds<Pixels>,
    dragging: bool,
    touch: bool,
    sheet: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ColorPickerEvent> for ColorPickerState {}

impl Focusable for ColorPickerState {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.hex.focus_handle(cx)
    }
}

impl ColorPickerState {
    /// A picker starting on `color`, or inherited when it is `None`.
    pub fn new(color: Option<Hsla>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let hex = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("#rrggbb")
                .default_value(color.map(crate::to_hex).unwrap_or_default())
        });
        let subscriptions =
            vec![
                cx.subscribe_in(&hex, window, |picker, hex, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                        picker.commit_hex(hex, cx);
                    }
                }),
            ];
        Self {
            hex,
            color,
            picked: None,
            field: Bounds::default(),
            dragging: false,
            touch: false,
            sheet: false,
            _subscriptions: subscriptions,
        }
    }

    /// The committed color, or `None` while inherited.
    #[must_use]
    pub fn color(&self) -> Option<Hsla> {
        self.color
    }

    /// Push a color in without emitting, for echoing an external source back.
    pub fn set_color(&mut self, color: Option<Hsla>, window: &mut Window, cx: &mut Context<Self>) {
        if self.color == color {
            return;
        }
        self.color = color;
        let text = color.map(crate::to_hex).unwrap_or_default();
        self.hex
            .update(cx, |hex, cx| hex.set_value(text, window, cx));
        cx.notify();
    }

    /// The color on show as hue, saturation and brightness. The last one
    /// picked keeps its hue while it is gray or black.
    fn shown(&self, inherited: Hsla) -> Hsv {
        let shown = self.color.unwrap_or(inherited);
        self.picked
            .filter(|picked| crate::to_hex(picked.color()) == crate::to_hex(shown))
            .unwrap_or_else(|| Hsv::of(shown))
    }

    fn pick(&mut self, hsv: Hsv, window: &mut Window, cx: &mut Context<Self>) {
        self.picked = Some(hsv);
        let color = hsv.color();
        if self.color.map(crate::to_hex) == Some(crate::to_hex(color)) {
            cx.notify();
            return;
        }
        self.set_color(Some(color), window, cx);
        self.emit(Some(color), cx);
    }

    fn drag_to(&mut self, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(picked) = self.picked else {
            return;
        };
        let bounds = self.field;
        let along = |offset: Pixels, length: Pixels| (offset / length.max(px(1.0))).clamp(0.0, 1.0);
        self.pick(
            Hsv {
                saturation: along(position.x - bounds.left(), bounds.size.width),
                value: 1.0 - along(position.y - bounds.top(), bounds.size.height),
                ..picked
            },
            window,
            cx,
        );
    }

    fn commit_hex(&mut self, hex: &Entity<InputState>, cx: &mut Context<Self>) {
        let value = hex.read(cx).value();
        let color = if value.trim().is_empty() {
            None
        } else {
            match crate::parse_hex(&value) {
                Ok(color) => Some(color),
                Err(_) => return,
            }
        };
        self.emit(color, cx);
    }

    fn set_sheet(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.sheet != open {
            self.sheet = open;
            cx.notify();
        }
    }

    fn emit(&mut self, color: Option<Hsla>, cx: &mut Context<Self>) {
        self.color = color;
        cx.emit(ColorPickerEvent::Change(color));
        cx.notify();
    }
}

/// A swatch button that opens a saturation and brightness field, a hue
/// slider and a hex field.
#[derive(IntoElement)]
pub struct ColorPicker {
    state: Entity<ColorPickerState>,
    inherited: Hsla,
    label: Option<SharedString>,
    style: StyleRefinement,
    size: Size,
    swatch: Option<Pixels>,
    presets: Option<(SharedString, AnyElement)>,
    disabled: bool,
}

impl ColorPicker {
    #[must_use]
    pub fn new(state: &Entity<ColorPickerState>, inherited: Hsla) -> Self {
        Self {
            state: state.clone(),
            inherited,
            label: None,
            style: StyleRefinement::default(),
            size: Size::Medium,
            swatch: None,
            presets: None,
            disabled: false,
        }
    }

    /// Heading shown above the field, naming what is being colored.
    #[must_use]
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Draws the trigger as a bare swatch `size` across, to sit in a row of
    /// swatches.
    #[must_use]
    pub fn swatch(mut self, size: Pixels) -> Self {
        self.swatch = Some(size);
        self
    }

    #[must_use]
    pub fn presets(
        mut self,
        chosen: impl Into<SharedString>,
        color: Hsla,
        presets: impl IntoElement,
    ) -> Self {
        self.inherited = color;
        self.presets = Some((chosen.into(), presets.into_any_element()));
        self
    }

    fn clear(state: &Entity<ColorPickerState>, window: &mut Window, cx: &mut App) {
        state.update(cx, |picker, cx| {
            picker.picked = None;
            picker.set_color(None, window, cx);
            picker.emit(None, cx);
        });
    }

    /// The saturation and brightness of the picked hue, dragged with the
    /// handle.
    #[allow(
        clippy::disallowed_methods,
        reason = "the handle's white ring reads on every color under it"
    )]
    fn field(
        state: &Entity<ColorPickerState>,
        hsv: Hsv,
        height: Pixels,
        cx: &App,
    ) -> impl IntoElement {
        let radius = cx.theme().radius;
        let listeners = canvas(
            {
                let state = state.clone();
                move |bounds, window, cx| {
                    state.update(cx, |picker, _| picker.field = bounds);
                    window.insert_hitbox(bounds, HitboxBehavior::Normal)
                }
            },
            {
                let state = state.clone();
                move |_, hitbox, window, _| {
                    let touched = state.clone();
                    window.on_mouse_event(move |event: &TouchDragEvent, phase, window, cx| {
                        if !phase.bubble() {
                            return;
                        }
                        match event.phase {
                            TouchPhase::Started => {
                                if window.default_prevented()
                                    || !hitbox.is_hovered_at(event.start_position, window)
                                {
                                    return;
                                }
                                window.prevent_default();
                                touched.update(cx, |picker, cx| {
                                    picker.picked = Some(hsv);
                                    picker.touch = true;
                                    picker.drag_to(event.position, window, cx);
                                });
                            }
                            TouchPhase::Moved if touched.read(cx).touch => {
                                touched.update(cx, |picker, cx| {
                                    picker.drag_to(event.position, window, cx);
                                });
                            }
                            TouchPhase::Ended | TouchPhase::Cancelled if touched.read(cx).touch => {
                                touched.update(cx, |picker, _| picker.touch = false);
                            }
                            TouchPhase::Moved | TouchPhase::Ended | TouchPhase::Cancelled => {}
                        }
                    });
                    let moved = state.clone();
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                        if !phase.bubble() || !moved.read(cx).dragging {
                            return;
                        }
                        moved.update(cx, |picker, cx| {
                            if event.pressed_button == Some(MouseButton::Left) {
                                picker.drag_to(event.position, window, cx);
                            } else {
                                picker.dragging = false;
                            }
                        });
                    });
                    window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                        if phase.bubble() && state.read(cx).dragging {
                            state.update(cx, |picker, _| picker.dragging = false);
                        }
                    });
                }
            },
        )
        .absolute()
        .inset_0();
        let handle = div()
            .absolute()
            .left(relative(hsv.saturation))
            .top(relative(1.0 - hsv.value))
            .ml(px(-HANDLE / 2.0))
            .mt(px(-HANDLE / 2.0))
            .size(px(HANDLE))
            .rounded_full()
            .border_2()
            .border_color(zz_gpui::white())
            .bg(hsv.color())
            .shadow(crate::lifted_shadow(cx));
        let state = state.clone();
        div()
            .id(("zz-color-picker-field", state.entity_id()))
            .debug_selector(|| "zz-color-picker-field".to_owned())
            .relative()
            .w_full()
            .h(height)
            .flex_none()
            .rounded(radius)
            .cursor_crosshair()
            .bg(linear_gradient(
                90.0,
                linear_color_stop(zz_gpui::white(), 0.0),
                linear_color_stop(hsv.pure(), 1.0),
            ))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded(radius)
                    .bg(linear_gradient(
                        180.0,
                        linear_color_stop(zz_gpui::transparent_black(), 0.0),
                        linear_color_stop(zz_gpui::black(), 1.0),
                    )),
            )
            .child(handle)
            .child(listeners)
            .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                cx.stop_propagation();
                state.update(cx, |picker, cx| {
                    picker.picked = Some(hsv);
                    picker.dragging = true;
                    picker.drag_to(event.position, window, cx);
                });
            })
    }

    fn hue(state: &Entity<ColorPickerState>, hsv: Hsv, cx: &App) -> impl IntoElement {
        let stops = (0..=HUE_STOPS).map(|stop| {
            Hsv {
                hue: f32::from(stop) / f32::from(HUE_STOPS),
                ..hsv
            }
            .pure()
        });
        let state = state.clone();
        div().mx(px(-thumb(cx) / 2.0)).child(
            Slider::new(
                ("zz-color-picker-hue", state.entity_id()),
                "Hue",
                hsv.hue * 360.0,
                0.0..=360.0,
                move |degrees, window, cx| {
                    state.update(cx, |picker, cx| {
                        picker.pick(
                            Hsv {
                                hue: (degrees / 360.0).rem_euclid(1.0),
                                ..hsv
                            },
                            window,
                            cx,
                        );
                    });
                },
            )
            .step(1.0)
            .track(stops)
            .inline(),
        )
    }

    fn hex_row(state: &Entity<ColorPickerState>, cx: &App) -> impl IntoElement {
        let hex = state.read(cx).hex.clone();
        let state = state.clone();
        h_flex()
            .gap(px(6.0))
            .child(Input::new(&hex).flex_1().min_w_0())
            .child(
                Button::new(("zz-color-picker-clear", state.entity_id()))
                    .ghost()
                    .flat()
                    .compact()
                    .icon(crate::IconName::Undo2)
                    .tooltip("Clear the override")
                    .on_click(move |_, window, cx| Self::clear(&state, window, cx)),
            )
    }

    fn controls(
        state: &Entity<ColorPickerState>,
        inherited: Hsla,
        field_height: f32,
        cx: &App,
    ) -> impl IntoElement {
        let hsv = state.read(cx).shown(inherited);
        v_flex()
            .gap(px(8.0))
            .child(Self::field(state, hsv, px(field_height), cx))
            .child(Self::hue(state, hsv, cx))
            .child(Self::hex_row(state, cx))
    }

    fn trigger(&self, shown: Hsla, coarse: bool, cx: &App) -> Button {
        let id = ("zz-color-picker", self.state.entity_id());
        let swatch = |size: Pixels| {
            div()
                .size(size)
                .flex_none()
                .rounded(cx.theme().swatch_radius())
                .bg(shown)
                .surface_edge(cx)
        };
        let button = match self.swatch {
            Some(size) => Button::new(id)
                .ghost()
                .flat()
                .size(size)
                .p_0()
                .rounded(cx.theme().swatch_radius())
                .child(swatch(size)),
            None if coarse => Button::new(id).ghost().flat().with_size(self.size).child(
                h_flex()
                    .gap(px(10.0))
                    .child(
                        div()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_size(px(13.0))
                            .text_color(cx.theme().foreground.muted())
                            .child(crate::to_hex(shown).to_uppercase()),
                    )
                    .child(swatch(TOUCH_TRIGGER_SWATCH)),
            ),
            None => Button::new(id)
                .ghost()
                .flat()
                .with_size(self.size)
                .compact()
                .child(swatch(TRIGGER_SWATCH_SIZE)),
        };
        let state = self.state.clone();
        button.disabled(self.disabled).when(coarse, |button| {
            button.on_click(move |_, _, cx| {
                state.update(cx, |picker, cx| picker.set_sheet(true, cx));
            })
        })
    }

    fn field_trigger(&self, chosen: SharedString, shown: Hsla, cx: &App) -> impl IntoElement {
        let size = Size::Medium;
        let state = if self.state.read(cx).sheet {
            PaintState::Focused
        } else {
            PaintState::Rest
        };
        let opener = self.state.clone();
        field_chrome(
            field_text(
                h_flex()
                    .id(("zz-color-picker-field-trigger", self.state.entity_id()))
                    .w_full()
                    .h(size.control_h())
                    .px(size.input_px())
                    .gap(px(10.0))
                    .justify_between()
                    .cursor_pointer(),
                size,
            ),
            Surface::new(Kind::Field.role()),
            state,
            true,
            true,
            cx,
        )
        .child(div().min_w_0().truncate().child(chosen))
        .child(
            div()
                .flex_none()
                .size(FIELD_SWATCH)
                .rounded(cx.theme().swatch_radius())
                .bg(shown)
                .surface_edge(cx),
        )
        .when(!self.disabled, |this| {
            this.on_click(move |_, _, cx| {
                opener.update(cx, |picker, cx| picker.set_sheet(true, cx));
            })
        })
    }

    fn touch_sheet(
        &self,
        presets: Option<AnyElement>,
        window: &Window,
        cx: &App,
    ) -> zz_gpui::AnyElement {
        let close = self.state.clone();
        let dismiss = self.state.clone();
        floating_sheet(
            bottom_sheet(
                ("zz-color-picker-sheet", self.state.entity_id()),
                self.label.clone().unwrap_or_else(|| "Color".into()),
                [sheet_close("zz-color-picker-sheet-close")
                    .on_click(move |_, _, cx| {
                        close.update(cx, |picker, cx| picker.set_sheet(false, cx));
                    })
                    .into_any_element()],
                div()
                    .px(px(SHEET_PADDING_X))
                    .pb(px(12.0))
                    .children(presets.map(|presets| div().pb(px(12.0)).child(presets)))
                    .child(Self::controls(
                        &self.state,
                        self.inherited,
                        SHEET_FIELD_HEIGHT,
                        cx,
                    )),
                sheet_inset(window),
                move |_, cx| dismiss.update(cx, |picker, cx| picker.set_sheet(false, cx)),
            ),
            window,
        )
    }
}

impl Sizable for ColorPicker {
    fn with_size(mut self, size: impl Into<Size>) -> Self {
        self.size = size.into();
        self
    }
}

impl Disableable for ColorPicker {
    fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

impl zz_gpui::Styled for ColorPicker {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for ColorPicker {
    fn render(mut self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let shown = self.state.read(cx).color.unwrap_or(self.inherited);
        if crate::touch::CoarsePointer::get(cx) {
            let (chosen, presets) = self.presets.take().unzip();
            let sheet = self
                .state
                .read(cx)
                .sheet
                .then(|| self.touch_sheet(presets, window, cx));
            return match chosen {
                Some(chosen) => div()
                    .w_full()
                    .child(self.field_trigger(chosen, shown, cx))
                    .children(sheet),
                None => div()
                    .flex_none()
                    .child(self.trigger(shown, true, cx))
                    .children(sheet),
            }
            .into_any_element();
        }
        let state = self.state.clone();
        let inherited = self.inherited;
        let label = self.label.clone();
        Popover::new(("zz-color-picker-popover", self.state.entity_id()))
            .anchor(Anchor::TopRight)
            .trigger(self.trigger(shown, false, cx))
            .content(move |_, _, cx| {
                v_flex()
                    .gap(px(8.0))
                    .w(px(POPOVER_WIDTH))
                    .when_some(label.clone(), |this, label| {
                        this.child(
                            div()
                                .text_size(crate::rems_from_px(11.0))
                                .text_color(cx.theme().foreground.muted())
                                .child(label),
                        )
                    })
                    .child(Self::controls(&state, inherited, FIELD_HEIGHT, cx))
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};
    use zz_gpui::{Modifiers, MouseDownEvent, Render, TestAppContext, VisualTestContext, point};

    struct PickerTest {
        state: Entity<ColorPickerState>,
    }

    impl Render for PickerTest {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .p(px(10.0))
                .child(ColorPicker::new(&self.state, zz_gpui::black()))
        }
    }

    #[zz_gpui::test]
    fn dragging_the_field_picks_saturation_and_brightness(cx: &mut TestAppContext) {
        cx.update(crate::init);
        let changes = Rc::new(RefCell::new(Vec::new()));
        let (view, cx) = cx.add_window_view(|window, cx| PickerTest {
            state: cx.new(|cx| ColorPickerState::new(crate::parse_hex("#ff0000").ok(), window, cx)),
        });
        let cx: &mut VisualTestContext = cx;
        let state = view.read_with(cx, |test, _| test.state.clone());
        cx.update({
            let changes = Rc::clone(&changes);
            move |_, cx| {
                cx.subscribe(&state, move |_, event: &ColorPickerEvent, _| {
                    let ColorPickerEvent::Change(color) = event;
                    changes.borrow_mut().push(color.map(crate::to_hex));
                })
                .detach();
            }
        });
        let draw = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            cx.update(|window, cx| _ = window.draw(cx));
        };
        draw(cx);
        cx.simulate_click(point(px(20.0), px(20.0)), Modifiers::default());
        draw(cx);
        let field = cx
            .debug_bounds("zz-color-picker-field")
            .expect("open field");
        let at = |x: f32, y: f32| field.origin + point(field.size.width * x, field.size.height * y);
        cx.simulate_event(MouseDownEvent {
            position: at(0.5, 0.5),
            modifiers: Modifiers::default(),
            button: MouseButton::Left,
            click_count: 1,
            first_mouse: false,
        });
        draw(cx);
        cx.simulate_event(MouseMoveEvent {
            position: at(1.5, -0.5),
            modifiers: Modifiers::default(),
            pressed_button: Some(MouseButton::Left),
        });
        draw(cx);
        cx.simulate_event(MouseUpEvent {
            position: at(1.5, -0.5),
            modifiers: Modifiers::default(),
            button: MouseButton::Left,
            click_count: 1,
        });
        draw(cx);
        cx.simulate_event(MouseMoveEvent {
            position: at(0.0, 1.0),
            modifiers: Modifiers::default(),
            pressed_button: None,
        });
        draw(cx);
        assert_eq!(
            *changes.borrow(),
            [Some("#804040".to_owned()), Some("#ff0000".to_owned())]
        );
    }

    #[test]
    fn colors_survive_the_trip_through_hue_saturation_and_brightness() {
        for hex in [
            "#000000", "#ffffff", "#808080", "#146ae3", "#ff8800", "#3e6b2a",
        ] {
            let color = crate::parse_hex(hex).unwrap();
            assert_eq!(crate::to_hex(Hsv::of(color).color()), hex);
        }
        let red = Hsv::of(crate::parse_hex("#ff0000").unwrap());
        assert_eq!((red.saturation, red.value), (1.0, 1.0));
        let gray = Hsv::of(crate::parse_hex("#808080").unwrap());
        assert_eq!(gray.saturation, 0.0);
    }
}
