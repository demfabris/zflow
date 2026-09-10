use eguicn::{Badge, Button, ButtonVariant, Card, Select, Theme, egui};

use crate::config::Config;

use super::{
    field, heading,
    layout_model::{Edge, Layout, LayoutDocument, Monitor},
    muted,
};

#[derive(Clone, Copy)]
struct View {
    scale: f32,
    offset: egui::Vec2,
}

impl View {
    fn fit(layout: &Layout, canvas: egui::Rect) -> Self {
        let bounds = layout
            .monitors
            .iter()
            .map(world_rect)
            .reduce(egui::Rect::union)
            .unwrap_or(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1920.0, 1080.0),
            ));
        let scale = ((canvas.width() - 64.0) / bounds.width().max(1.0))
            .min((canvas.height() - 64.0) / bounds.height().max(1.0))
            .clamp(0.0001, 0.3);
        Self {
            scale,
            offset: canvas.center().to_vec2() - bounds.center().to_vec2() * scale,
        }
    }

    fn rect(self, monitor: &Monitor) -> egui::Rect {
        let rect = world_rect(monitor);
        egui::Rect::from_min_max(
            (rect.min.to_vec2() * self.scale + self.offset).to_pos2(),
            (rect.max.to_vec2() * self.scale + self.offset).to_pos2(),
        )
    }
}

struct Drag {
    index: usize,
    original: Layout,
    pointer: egui::Pos2,
    view: View,
    position: Option<(i32, i32)>,
}

pub(super) struct LayoutEditor {
    document: Option<LayoutDocument>,
    config_path: std::path::PathBuf,
    error: Option<String>,
    notice: String,
    selected: usize,
    add_owner: usize,
    drag: Option<Drag>,
    edited: bool,
}

impl LayoutEditor {
    pub fn open(config_path: &std::path::Path) -> Self {
        let mut editor = Self {
            document: None,
            config_path: config_path.into(),
            error: None,
            notice: String::new(),
            selected: 0,
            add_owner: 0,
            drag: None,
            edited: false,
        };
        editor.reload();
        editor
    }

    pub fn reload(&mut self) {
        let result = if let Some(document) = &mut self.document {
            document.reload().map(|()| None)
        } else {
            LayoutDocument::open(&self.config_path).map(Some)
        };
        match result {
            Ok(replacement) => {
                if let Some(document) = replacement {
                    self.document = Some(document);
                }
                self.error = None;
                self.notice.clear();
                self.selected = 0;
                self.drag = None;
                self.edited = false;
            }
            Err(error) => self.error = Some(format!("{error:#}")),
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.document
            .as_ref()
            .is_some_and(|document| document.is_dirty() || (self.edited && document.is_new()))
    }

    fn suggested(&self) -> bool {
        !self.edited
            && self
                .document
                .as_ref()
                .is_some_and(|document| document.is_new() && document.draft.monitors.is_empty())
    }

    fn effective_layout(&self, config: &Config) -> Layout {
        if self.suggested() {
            suggested_layout(config)
        } else {
            self.document
                .as_ref()
                .map(|document| document.draft.clone())
                .unwrap_or_default()
        }
    }

    pub fn can_save(&self, config: &Config) -> bool {
        self.error.is_none()
            && self
                .document
                .as_ref()
                .is_some_and(|document| document.is_new() || self.is_dirty())
            && self.effective_layout(config).validate().is_ok()
    }

    pub fn save(&mut self, config: &Config) {
        if self.error.is_some() {
            return;
        }
        let layout = self.effective_layout(config);
        let Some(document) = &mut self.document else {
            return;
        };
        document.draft = layout;
        self.notice = match document.save() {
            Ok(()) => {
                self.edited = false;
                "Layout saved. It does not enable automatic input switching.".into()
            }
            Err(error) => format!("Could not save layout: {error:#}"),
        };
    }

    pub fn show(&mut self, ui: &mut egui::Ui, config: &Config) {
        heading(
            ui,
            "Arrange your displays",
            "Drag displays together. Their shared edge defines the crossing zone.",
        );
        if let Some(error) = &self.error {
            ui.colored_label(Theme::from_ui(ui).destructive, error);
            muted(
                ui,
                "The layout file has not been replaced. Fix it, then use Reload from disk.",
            );
            return;
        }
        let Some(_) = &self.document else {
            return;
        };
        let suggested = self.suggested();
        let mut layout = self.effective_layout(config);
        let before = layout.clone();
        let theme = Theme::from_ui(ui);
        ui.horizontal_wrapped(|ui| {
            ui.add(Badge::new("Layout preview").variant(ButtonVariant::Secondary));
            muted(ui, "Cursor handoff is not active yet.");
        });
        if suggested {
            muted(
                ui,
                "Suggested positions, using 1920 × 1080 logical pixels per computer. Adjust the sizes to match your display settings, then save the layout.",
            );
        }
        ui.add_space(12.0);
        self.canvas(ui, &mut layout);
        ui.add_space(10.0);
        muted(
            ui,
            "Drag to snap edges. Use arrow keys on a focused display for 10-pixel steps; hold Shift for 1 pixel. Overlapping drops return to the previous position.",
        );
        ui.add_space(14.0);

        let mut owners: Vec<Option<String>> = vec![None];
        owners.extend(config.peers.keys().cloned().map(Some));
        self.add_owner = self.add_owner.min(owners.len() - 1);
        let owner_labels: Vec<_> = owners
            .iter()
            .map(|peer| peer.as_deref().unwrap_or("This computer"))
            .collect();
        ui.horizontal(|ui| {
            Select::new("add-display-owner", &mut self.add_owner, &owner_labels)
                .width(190.0)
                .show(ui);
            if ui
                .add_enabled(
                    layout.monitors.len() < 32,
                    Button::new("Add display").variant(ButtonVariant::Outline),
                )
                .clicked()
            {
                add_monitor(&mut layout, owners[self.add_owner].clone());
                self.selected = layout.monitors.len().saturating_sub(1);
            }
        });
        ui.add_space(16.0);
        self.selected = self.selected.min(layout.monitors.len().saturating_sub(1));
        let mut remove = false;
        if let Some(monitor) = layout.monitors.get_mut(self.selected) {
            Card::new().padding(18).show(ui, |ui| {
                ui.set_width(ui.available_width());
                Card::header(ui, "Selected display", "Dimensions and positions use logical pixels, not physical panel pixels.");
                field(ui, "Display name", &mut monitor.label);
                if let Some(peer) = &monitor.peer && !config.peers.contains_key(peer) {
                    ui.colored_label(theme.destructive, format!("{peer} is not paired in this configuration. This display cannot authorize a connection."));
                }
                let mut selected_owners = owners.clone();
                if !selected_owners.contains(&monitor.peer) { selected_owners.push(monitor.peer.clone()); }
                let mut owner = selected_owners.iter().position(|value| value == &monitor.peer).unwrap_or(0);
                let labels: Vec<_> = selected_owners.iter().map(|peer| peer.as_deref().unwrap_or("This computer")).collect();
                ui.label("Connected to");
                if Select::new("selected-display-owner", &mut owner, &labels).show(ui).changed() {
                    monitor.peer = selected_owners[owner].clone();
                }
                ui.add_space(8.0);
                egui::Grid::new("display-dimensions").num_columns(4).spacing([12.0, 8.0]).show(ui, |ui| {
                    for (label, value) in [("Width", &mut monitor.width), ("Height", &mut monitor.height)] {
                        let text = ui.label(label);
                        ui.add(egui::DragValue::new(value).range(1..=16384).suffix(" px")).labelled_by(text.id);
                    }
                    ui.end_row();
                    for (label, value) in [("X", &mut monitor.x), ("Y", &mut monitor.y)] {
                        let text = ui.label(label);
                        ui.add(egui::DragValue::new(value).range(-100000..=100000).suffix(" px")).labelled_by(text.id);
                    }
                });
                ui.add_space(10.0);
                remove = ui.add(Button::new("Remove display").variant(ButtonVariant::Ghost)).clicked();
            });
        }
        if remove {
            layout.monitors.remove(self.selected);
            self.selected = 0;
        }
        ui.add_space(16.0);
        let validation = layout.validate().err();
        if let Some(error) = &validation {
            ui.colored_label(theme.destructive, format!("{error:#}"));
        } else {
            transitions(ui, &layout);
        }

        if layout != before {
            self.document.as_mut().expect("loaded layout").draft = layout.clone();
            self.edited = true;
            self.notice.clear();
        }
        ui.add_space(12.0);
        let mut save = false;
        ui.horizontal(|ui| {
            save = ui
                .add_enabled(
                    validation.is_none()
                        && (suggested
                            || self.is_dirty()
                            || self.document.as_ref().is_some_and(LayoutDocument::is_new)),
                    Button::new("Save layout"),
                )
                .clicked();
            muted(
                ui,
                if self.is_dirty() {
                    "Unsaved layout changes"
                } else if suggested {
                    "Suggested layout, not saved"
                } else {
                    "Layout saved"
                },
            );
        });
        if save {
            self.save(config);
        }
        if !self.notice.is_empty() {
            ui.label(&self.notice);
        }
        if let Some(document) = &self.document {
            muted(ui, &document.path.display().to_string());
        }
        muted(
            ui,
            "The layout file is separate from daemon settings. Discovery does not report physical displays; add and size each display here.",
        );
    }

    fn canvas(&mut self, ui: &mut egui::Ui, layout: &mut Layout) {
        let theme = Theme::from_ui(ui);
        let (canvas, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), 300.0),
            egui::Sense::hover(),
        );
        let painter = ui.painter().with_clip_rect(canvas);
        painter.rect_filled(canvas, 12, theme.muted);
        for x in (0..canvas.width() as usize).step_by(24) {
            for y in (0..300).step_by(24) {
                painter.circle_filled(
                    canvas.min + egui::vec2(x as f32, y as f32),
                    1.0,
                    theme.border,
                );
            }
        }
        let view = self
            .drag
            .as_ref()
            .map(|drag| drag.view)
            .unwrap_or_else(|| View::fit(layout, canvas));
        let mut display = layout.clone();
        if let Some(drag) = &mut self.drag
            && let Some(pointer) = ui.input(|input| input.pointer.interact_pos())
        {
            let origin = &drag.original.monitors[drag.index];
            let delta = (pointer - drag.pointer) / drag.view.scale;
            let desired = (
                origin.x.saturating_add(delta.x.round() as i32),
                origin.y.saturating_add(delta.y.round() as i32),
            );
            drag.position = drag.original.snap_move(
                drag.index,
                desired.0,
                desired.1,
                (12.0 / drag.view.scale).round() as i32,
            );
            let position = drag.position.unwrap_or(desired);
            display.monitors[drag.index].x = position.0;
            display.monitors[drag.index].y = position.1;
        }
        for (index, monitor) in display.monitors.iter().enumerate() {
            let rect = view.rect(monitor);
            let response = ui.interact(
                rect.intersect(canvas),
                ui.id().with(("display", &monitor.id)),
                egui::Sense::click_and_drag(),
            );
            response.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::WidgetType::Button,
                    true,
                    self.selected == index,
                    &monitor.label,
                )
            });
            if response.clicked() || response.drag_started() {
                self.selected = index;
                response.request_focus();
            }
            if response.drag_started() {
                self.drag = Some(Drag {
                    index,
                    original: layout.clone(),
                    pointer: ui
                        .input(|input| input.pointer.press_origin())
                        .unwrap_or(rect.center()),
                    view,
                    position: Some((monitor.x, monitor.y)),
                });
            }
            if response.has_focus() && self.drag.is_none() {
                self.selected = index;
                ui.memory_mut(|memory| {
                    memory.set_focus_lock_filter(
                        response.id,
                        egui::EventFilter {
                            horizontal_arrows: true,
                            vertical_arrows: true,
                            ..Default::default()
                        },
                    )
                });
                let delta = ui.input(|input| {
                    let step = if input.modifiers.shift { 1 } else { 10 };
                    (
                        step * (i32::from(input.key_pressed(egui::Key::ArrowRight))
                            - i32::from(input.key_pressed(egui::Key::ArrowLeft))),
                        step * (i32::from(input.key_pressed(egui::Key::ArrowDown))
                            - i32::from(input.key_pressed(egui::Key::ArrowUp))),
                    )
                });
                if delta != (0, 0)
                    && let Some((x, y)) =
                        layout.snap_move(index, monitor.x + delta.0, monitor.y + delta.1, 0)
                {
                    layout.monitors[index].x = x;
                    layout.monitors[index].y = y;
                }
            }
            let invalid = self
                .drag
                .as_ref()
                .is_some_and(|drag| drag.index == index && drag.position.is_none());
            let selected = self.selected == index;
            let stroke = egui::Stroke::new(
                if selected { 2.0 } else { 1.0 },
                if invalid {
                    theme.destructive
                } else if selected {
                    theme.primary
                } else {
                    theme.border
                },
            );
            painter.rect(rect, 7, theme.card, stroke, egui::StrokeKind::Inside);
            let text_painter = painter.with_clip_rect(rect.shrink(5.0).intersect(canvas));
            let font_size = if rect.width() < 100.0 { 10.0 } else { 13.0 };
            text_painter.text(
                rect.center() - egui::vec2(0.0, 16.0),
                egui::Align2::CENTER_CENTER,
                &monitor.label,
                egui::FontId::proportional(font_size),
                theme.foreground,
            );
            text_painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                monitor.peer.as_deref().unwrap_or("This computer"),
                egui::FontId::proportional(font_size - 1.0),
                theme.muted_foreground,
            );
            text_painter.text(
                rect.center() + egui::vec2(0.0, 17.0),
                egui::Align2::CENTER_CENTER,
                format!("{} × {}", monitor.width, monitor.height),
                egui::FontId::monospace(font_size - 2.0),
                theme.muted_foreground,
            );
            response.on_hover_text(format!(
                "{} · {}\nDrag to arrange; arrow keys to nudge.",
                monitor.label,
                monitor.peer.as_deref().unwrap_or("This computer")
            ));
        }
        if display.validate().is_ok() {
            for transition in display.transitions() {
                let rect = view.rect(&display.monitors[transition.source]);
                let start = transition.source_start as f32;
                let end = transition.source_end as f32;
                let segment = match transition.edge {
                    Edge::Left => [
                        egui::pos2(rect.left(), rect.top() + start * rect.height()),
                        egui::pos2(rect.left(), rect.top() + end * rect.height()),
                    ],
                    Edge::Right => [
                        egui::pos2(rect.right(), rect.top() + start * rect.height()),
                        egui::pos2(rect.right(), rect.top() + end * rect.height()),
                    ],
                    Edge::Top => [
                        egui::pos2(rect.left() + start * rect.width(), rect.top()),
                        egui::pos2(rect.left() + end * rect.width(), rect.top()),
                    ],
                    Edge::Bottom => [
                        egui::pos2(rect.left() + start * rect.width(), rect.bottom()),
                        egui::pos2(rect.left() + end * rect.width(), rect.bottom()),
                    ],
                };
                painter.line_segment(segment, egui::Stroke::new(4.0, theme.primary));
            }
        }
        if ui.input(|input| input.pointer.any_released())
            && let Some(drag) = self.drag.take()
        {
            if let Some((x, y)) = drag.position {
                layout.monitors[drag.index].x = x;
                layout.monitors[drag.index].y = y;
            } else {
                self.notice =
                    "That drop overlaps another display or exceeds the layout bounds.".into();
            }
        }
    }
}

fn world_rect(monitor: &Monitor) -> egui::Rect {
    egui::Rect::from_min_size(
        egui::pos2(monitor.x as f32, monitor.y as f32),
        egui::vec2(monitor.width as f32, monitor.height as f32),
    )
}

fn suggested_layout(config: &Config) -> Layout {
    let mut layout = Layout::default();
    add_monitor(&mut layout, None);
    for peer in config.peers.keys().take(31) {
        add_monitor(&mut layout, Some(peer.clone()));
    }
    layout
}

fn add_monitor(layout: &mut Layout, peer: Option<String>) {
    let mut serial = 1;
    while layout
        .monitors
        .iter()
        .any(|monitor| monitor.id == format!("display-{serial}"))
    {
        serial += 1;
    }
    let x = layout
        .monitors
        .iter()
        .map(|monitor| monitor.x + monitor.width as i32)
        .max()
        .unwrap_or(0);
    layout.monitors.push(Monitor {
        id: format!("display-{serial}"),
        label: format!("Display {serial}"),
        peer,
        x,
        y: 0,
        width: 1920,
        height: 1080,
    });
}

fn transitions(ui: &mut egui::Ui, layout: &Layout) {
    let links = layout.transitions();
    Card::new().padding(18).show(ui, |ui| {
        ui.set_width(ui.available_width());
        Card::header(ui, "Crossing zones", "The thick shared edges mark the configured transition space.");
        if links.is_empty() { muted(ui, "No crossing zones. Place displays from different computers against each other with a shared edge."); }
        for link in links.iter().filter(|link| link.source < link.target) {
            let source = &layout.monitors[link.source];
            let target = &layout.monitors[link.target];
            let edge = match link.edge { Edge::Left => "left", Edge::Right => "right", Edge::Top => "top", Edge::Bottom => "bottom" };
            ui.label(format!("{} ({}) ↔ {} ({})", source.label, source.peer.as_deref().unwrap_or("This computer"), target.label, target.peer.as_deref().unwrap_or("This computer")));
            muted(ui, &format!("{edge} edge: {:.1}–{:.1}% → {:.1}–{:.1}% of the receiving edge", link.source_start * 100.0, link.source_end * 100.0, link.target_start * 100.0, link.target_end * 100.0));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas_frame(
        editor: &mut LayoutEditor,
        layout: &mut Layout,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
    ) {
        ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(600.0, 340.0),
                )),
                events,
                ..Default::default()
            },
            |ui| editor.canvas(ui, layout),
        )
        .drop_without_applying_deltas();
    }

    fn pointer_button(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        }
    }

    fn drag_display(
        editor: &mut LayoutEditor,
        layout: &mut Layout,
        ctx: &egui::Context,
        start: egui::Pos2,
        delta: egui::Vec2,
    ) {
        canvas_frame(
            editor,
            layout,
            ctx,
            vec![
                egui::Event::PointerMoved(start),
                pointer_button(start, true),
            ],
        );
        canvas_frame(
            editor,
            layout,
            ctx,
            vec![egui::Event::PointerMoved(start + delta * 0.5)],
        );
        canvas_frame(
            editor,
            layout,
            ctx,
            vec![egui::Event::PointerMoved(start + delta)],
        );
        canvas_frame(
            editor,
            layout,
            ctx,
            vec![pointer_button(start + delta, false)],
        );
    }

    #[test]
    fn real_drag_uses_total_pointer_displacement_once() {
        let directory = tempfile::tempdir().unwrap();
        let mut editor = LayoutEditor::open(&directory.path().join("config.toml"));
        let mut layout = suggested_layout(&Config::default());
        add_monitor(&mut layout, Some("ubuntu".into()));
        let ctx = egui::Context::default();
        Theme::light().apply(&ctx);
        canvas_frame(&mut editor, &mut layout, &ctx, vec![]);
        let view = View::fit(
            &layout,
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(600.0, 300.0)),
        );
        let start = view.rect(&layout.monitors[1]).center();
        drag_display(
            &mut editor,
            &mut layout,
            &ctx,
            start,
            egui::vec2(140.0, 0.0),
        );
        assert_eq!(
            layout.monitors[1].x,
            1920 + (140.0 / view.scale).round() as i32
        );
        assert_eq!(layout.monitors[1].y, 0);
        assert!(editor.drag.is_none());
        assert!(!directory.path().join("config.toml.layout.toml").exists());
    }

    #[test]
    fn real_drag_snaps_shared_edge_and_rejects_overlap() {
        let directory = tempfile::tempdir().unwrap();
        let mut editor = LayoutEditor::open(&directory.path().join("config.toml"));
        let mut layout = suggested_layout(&Config::default());
        add_monitor(&mut layout, Some("ubuntu".into()));
        layout.monitors[1].x = 2400;
        let ctx = egui::Context::default();
        Theme::light().apply(&ctx);
        canvas_frame(&mut editor, &mut layout, &ctx, vec![]);
        let canvas = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(600.0, 300.0));
        let view = View::fit(&layout, canvas);
        let start = view.rect(&layout.monitors[1]).center();
        drag_display(
            &mut editor,
            &mut layout,
            &ctx,
            start,
            egui::vec2(-480.0 * view.scale + 5.0, 0.0),
        );
        assert_eq!(layout.monitors[1].x, 1920);
        assert_eq!(layout.transitions().len(), 2);
        canvas_frame(&mut editor, &mut layout, &ctx, vec![]);
        let view = View::fit(&layout, canvas);
        let start = view.rect(&layout.monitors[1]).center();
        drag_display(
            &mut editor,
            &mut layout,
            &ctx,
            start,
            egui::vec2(-960.0 * view.scale, 0.0),
        );
        assert_eq!(layout.monitors[1].x, 1920);
        assert!(editor.notice.contains("overlaps"));
    }

    #[test]
    fn empty_saved_layout_stays_empty_and_main_config_stays_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        Config::default().save(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        let mut editor = LayoutEditor::open(&path);
        editor.edited = true;
        editor.save(&Config::default());
        assert!(!editor.suggested());
        editor.reload();
        assert!(
            editor
                .effective_layout(&Config::default())
                .monitors
                .is_empty()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn suggested_layout_and_view_are_deterministic() {
        let config = Config::default();
        let layout = suggested_layout(&config);
        assert_eq!(layout.monitors.len(), 1);
        layout.validate().unwrap();
        let view = View::fit(
            &layout,
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(600.0, 300.0)),
        );
        assert!(view.rect(&layout.monitors[0]).width() <= 536.0);
        assert_eq!(layout, suggested_layout(&config));
    }
}
