use eguicn::{Badge, ButtonVariant, Theme, egui};
use sha2::{Digest, Sha256};

use crate::config::Config;

use super::{
    heading,
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
    detected: Option<Layout>,
    ready: std::collections::BTreeSet<String>,
    waiting: Vec<String>,
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
            detected: None,
            ready: Default::default(),
            waiting: Vec::new(),
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
                self.detected = None;
                self.ready.clear();
                self.waiting.clear();
            }
            Err(error) => self.error = Some(format!("{error:#}")),
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.edited || self.document.as_ref().is_some_and(LayoutDocument::is_dirty)
    }

    fn suggested(&self) -> bool {
        !self.edited
            && self
                .document
                .as_ref()
                .is_some_and(|document| document.is_new() && document.draft.monitors.is_empty())
    }

    fn effective_layout(&self, config: &Config) -> Layout {
        if let Some(detected) = &self.detected {
            return detected.clone();
        }
        if self.suggested() {
            suggested_layout(config)
        } else {
            self.document
                .as_ref()
                .map(|document| document.draft.clone())
                .unwrap_or_default()
        }
    }

    pub fn update_desktops(
        &mut self,
        local: Option<&super::displays::Desktop>,
        remote: &std::collections::BTreeMap<String, super::displays::Desktop>,
        config: &Config,
    ) {
        if self.drag.is_some() {
            return;
        }
        let previous = self
            .detected
            .as_ref()
            .or_else(|| self.document.as_ref().map(|doc| &doc.draft));
        let mut layout = Layout::default();
        self.ready.clear();
        self.waiting = config
            .peers
            .keys()
            .filter(|name| !remote.contains_key(*name))
            .cloned()
            .collect();
        for owner in
            std::iter::once(None).chain(config.peers.keys().map(|name| Some(name.as_str())))
        {
            if layout.monitors.len() == super::layout_model::MAX_MONITORS {
                break;
            }
            let desktop = owner
                .map_or(local, |name| remote.get(name))
                .filter(|d| d.valid());
            // Group older per-output layouts by computer without rewriting the saved file.
            let old = previous
                .and_then(|layout| layout.monitors.iter().find(|m| m.peer.as_deref() == owner));
            if owner.is_some() && desktop.is_none() && old.is_none() {
                continue;
            }
            let (width, height) = desktop
                .map(|d| (d.width, d.height))
                .or_else(|| old.map(|m| (m.width, m.height)))
                .unwrap_or((1920, 1080));
            let id = computer_id(owner);
            let right = layout
                .monitors
                .iter()
                .map(|m| m.x + m.width as i32)
                .max()
                .unwrap_or(0);
            layout.monitors.push(Monitor {
                id: id.clone(),
                label: owner.unwrap_or("This computer").into(),
                peer: owner.map(str::to_owned),
                x: old.map_or(right, |m| m.x),
                y: old.map_or(0, |m| m.y),
                width,
                height,
            });
            if layout.validate().is_err() {
                let monitor = layout.monitors.last_mut().unwrap();
                monitor.x = right;
                monitor.y = 0;
            }
            if layout.validate().is_err() {
                layout.monitors.pop();
                continue;
            }
            if desktop.is_some() {
                self.ready.insert(id);
            }
        }
        self.detected = Some(layout);
    }

    pub fn can_save(&self, config: &Config) -> bool {
        self.error.is_none()
            && self.document.as_ref().is_some_and(|document| {
                document.is_new()
                    || self.is_dirty()
                    || self.effective_layout(config) != document.draft
            })
            && self.ready.contains(&computer_id(None))
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
            "Arrange your computers",
            "Drag each computer beside its neighbour. Touching edges define where the cursor will cross.",
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
        let mut layout = self.effective_layout(config);
        let before = layout.clone();
        let theme = Theme::from_ui(ui);
        ui.horizontal_wrapped(|ui| {
            ui.add(Badge::new("Layout preview").variant(ButtonVariant::Secondary));
            muted(ui, "Cursor handoff is not active yet.");
        });
        muted(
            ui,
            "One tile per computer, sized to its desktop. Keep zflow open on both computers.",
        );
        if !self.waiting.is_empty() {
            muted(
                ui,
                &format!(
                    "Waiting for desktop information: {}.",
                    self.waiting.join(", ")
                ),
            );
        }
        ui.add_space(12.0);
        self.canvas(ui, &mut layout);
        ui.add_space(10.0);
        muted(
            ui,
            "Drag to snap edges. Arrow keys nudge; hold Shift for smaller steps.",
        );
        let validation = layout.validate().err();
        if let Some(error) = &validation {
            ui.colored_label(theme.destructive, format!("{error:#}"));
        }

        if layout != before {
            self.document.as_mut().expect("loaded layout").draft = layout.clone();
            if self.detected.is_some() {
                self.detected = Some(layout);
            }
            self.edited = true;
            self.notice.clear();
        }
        ui.add_space(12.0);
        if !self.notice.is_empty() {
            ui.label(&self.notice);
        }
        muted(ui, "Layout changes do not grant input access.");
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
                ui.id().with(("computer", &monitor.id)),
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
            let mut title = egui::text::LayoutJob::simple(
                monitor.label.clone(),
                egui::FontId::proportional(font_size + 2.0),
                theme.foreground,
                (rect.width() - 20.0).max(1.0),
            );
            title.wrap.max_rows = 1;
            let title = text_painter.layout_job(title);
            text_painter.galley(
                rect.center() - egui::vec2(title.size().x / 2.0, title.size().y + 2.0),
                title,
                theme.foreground,
            );
            let ready = self.ready.contains(&monitor.id);
            text_painter.text(
                rect.center() + egui::vec2(0.0, 10.0),
                egui::Align2::CENTER_CENTER,
                if !ready {
                    "Waiting for desktop"
                } else if monitor.peer.is_none() {
                    "Local"
                } else {
                    "Paired computer"
                },
                egui::FontId::proportional(font_size - 1.0),
                theme.muted_foreground,
            );
            response.on_hover_text(format!(
                "{}\n{}\nDrag to arrange; arrow keys to nudge.{}",
                monitor.label,
                if ready {
                    format!("{} × {} desktop coordinates", monitor.width, monitor.height)
                } else {
                    "Waiting for desktop information; keeping the previous layout size.".into()
                },
                if monitor.peer.is_some() {
                    "\nRemote dimensions are unverified network hints."
                } else {
                    ""
                },
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
                    "That drop overlaps another computer or exceeds the layout bounds.".into();
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

fn computer_id(peer: Option<&str>) -> String {
    match peer {
        Some(name) => format!("computer:{:x}", Sha256::digest(name.as_bytes())),
        None => "computer:local".into(),
    }
}

fn suggested_layout(_config: &Config) -> Layout {
    let mut layout = Layout::default();
    add_monitor(&mut layout, None);
    layout
}

fn add_monitor(layout: &mut Layout, peer: Option<String>) {
    let x = layout
        .monitors
        .iter()
        .map(|monitor| monitor.x + monitor.width as i32)
        .max()
        .unwrap_or(0);
    layout.monitors.push(Monitor {
        id: computer_id(peer.as_deref()),
        label: peer.as_deref().unwrap_or("This computer").into(),
        peer,
        x,
        y: 0,
        width: 1920,
        height: 1080,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_tile_per_computer_preserves_drag_save_and_report_loss() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut config = Config::default();
        config.peers.insert(
            "mac".into(),
            crate::config::PeerConfig::from_spki(b"test", Vec::new(), Default::default()).unwrap(),
        );
        let mut editor = LayoutEditor::open(&path);
        let local = super::super::displays::Desktop {
            width: 5696,
            height: 1692,
        };
        editor.update_desktops(Some(&local), &Default::default(), &config);
        assert_eq!(editor.effective_layout(&config).monitors.len(), 1);
        assert_eq!(editor.waiting, ["mac"]);
        let remote = std::collections::BTreeMap::from([(
            "mac".into(),
            super::super::displays::Desktop {
                width: 2880,
                height: 1620,
            },
        )]);
        editor.update_desktops(Some(&local), &remote, &config);
        let mut layout = editor.effective_layout(&config);
        assert_eq!(layout.monitors.len(), 2);
        assert_eq!(layout.monitors[0].label, "This computer");
        assert_eq!(layout.monitors[1].label, "mac");
        assert_eq!(
            (layout.monitors[0].width, layout.monitors[1].width),
            (5696, 2880)
        );
        assert!(!editor.is_dirty());
        layout.monitors[1].x = -2880;
        editor.document.as_mut().unwrap().draft = layout.clone();
        editor.detected = Some(layout.clone());
        editor.edited = true;
        editor.update_desktops(Some(&local), &remote, &config);
        assert_eq!(editor.effective_layout(&config), layout);
        assert!(editor.is_dirty());
        editor.save(&config);
        assert!(!path.exists());
        assert!(!editor.is_dirty());
        editor.reload();
        editor.update_desktops(Some(&local), &remote, &config);
        assert_eq!(editor.effective_layout(&config), layout);
        editor.update_desktops(Some(&local), &Default::default(), &config);
        assert_eq!(editor.effective_layout(&config), layout);
        assert_eq!(editor.waiting, ["mac"]);
        assert_eq!(editor.ready.len(), 1);
        assert!(!editor.is_dirty());
        editor.update_desktops(Some(&local), &remote, &config);
        assert_eq!(editor.effective_layout(&config), layout);
        assert_eq!(editor.ready.len(), 2);
    }

    #[test]
    fn old_output_tiles_collapse_without_writing_or_dirtying_the_layout() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut config = Config::default();
        config.peers.insert(
            "local".into(),
            crate::config::PeerConfig::from_spki(b"test", Vec::new(), Default::default()).unwrap(),
        );
        let mut document = LayoutDocument::open(&path).unwrap();
        for (i, peer) in [None, None, Some("local"), Some("local")]
            .into_iter()
            .enumerate()
        {
            document.draft.monitors.push(Monitor {
                id: format!("old-{i}"),
                label: format!("Display {i}"),
                peer: peer.map(str::to_owned),
                x: i as i32 * 1920,
                y: 0,
                width: 1920,
                height: 1080,
            });
        }
        document.save().unwrap();
        let before = std::fs::read(&document.path).unwrap();
        let desktop = super::super::displays::Desktop {
            width: 3840,
            height: 1080,
        };
        let remote = std::collections::BTreeMap::from([("local".into(), desktop)]);
        let mut editor = LayoutEditor::open(&path);
        editor.update_desktops(Some(&desktop), &remote, &config);
        let layout = editor.effective_layout(&config);
        assert_eq!(layout.monitors.len(), 2);
        assert_eq!(layout.monitors[1].x, 3840);
        assert_ne!(layout.monitors[0].id, layout.monitors[1].id);
        assert!(!editor.is_dirty());
        assert!(editor.can_save(&config));
        assert_eq!(std::fs::read(&document.path).unwrap(), before);
        editor.save(&config);
        assert_eq!(LayoutDocument::open(&path).unwrap().draft, layout);
        assert!(!path.exists());
        assert!(!editor.can_save(&config));
    }

    #[test]
    fn reload_discards_drag_and_unpaired_reports_cannot_add_tiles() {
        let directory = tempfile::tempdir().unwrap();
        let mut editor = LayoutEditor::open(&directory.path().join("config.toml"));
        let config = Config::default();
        let desktop = super::super::displays::Desktop {
            width: 1920,
            height: 1080,
        };
        let remote = std::collections::BTreeMap::from([("stranger".into(), desktop)]);
        editor.update_desktops(Some(&desktop), &remote, &config);
        assert_eq!(editor.effective_layout(&config).monitors.len(), 1);
        editor.save(&config);
        editor.document.as_mut().unwrap().draft.monitors[0].x = 100;
        editor.detected.as_mut().unwrap().monitors[0].x = 100;
        editor.edited = true;
        editor.reload();
        editor.update_desktops(Some(&desktop), &remote, &config);
        assert_eq!(editor.effective_layout(&config).monitors[0].x, 0);
        assert!(!editor.is_dirty());
        let smaller = super::super::displays::Desktop {
            width: 1280,
            height: 720,
        };
        editor.update_desktops(Some(&smaller), &remote, &config);
        assert!(!editor.is_dirty());
        assert!(editor.can_save(&config));
        editor.update_desktops(None, &remote, &config);
        assert!(!editor.can_save(&config));
    }

    #[test]
    fn canvas_labels_computers_once_and_keeps_resolution_out_of_tiles() {
        let directory = tempfile::tempdir().unwrap();
        let mut editor = LayoutEditor::open(&directory.path().join("config.toml"));
        let mut config = Config::default();
        config.peers.insert(
            "ubuntu".into(),
            crate::config::PeerConfig::from_spki(b"test", Vec::new(), Default::default()).unwrap(),
        );
        editor.update_desktops(
            Some(&super::super::displays::Desktop {
                width: 5696,
                height: 1692,
            }),
            &std::collections::BTreeMap::from([(
                "ubuntu".into(),
                super::super::displays::Desktop {
                    width: 2880,
                    height: 1620,
                },
            )]),
            &config,
        );
        let ctx = egui::Context::default();
        Theme::light().apply(&ctx);
        let mut layout = editor.effective_layout(&config);
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 340.0),
                )),
                ..Default::default()
            },
            |ui| editor.canvas(ui, &mut layout),
        );
        output.textures_delta.clear();
        let labels: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::epaint::Shape::Text(text) => Some(text.galley.job.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            labels
                .iter()
                .filter(|label| **label == "This computer")
                .count(),
            1
        );
        assert_eq!(labels.iter().filter(|label| **label == "ubuntu").count(), 1);
        assert!(
            !labels.iter().any(|label| label.contains("Display ")
                || label.contains('×')
                || label.contains('%'))
        );
        assert_eq!(layout.transitions().len(), 2);
    }

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
