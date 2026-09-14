//! Desktop setup and explicit input-sharing controls.

#[cfg(target_os = "linux")]
mod desktop;
mod displays;
#[cfg(any(target_os = "macos", test))]
mod handoff;
mod layout_editor;
mod layout_model;
mod model;
mod nearby;
mod pairing;
#[cfg(target_os = "macos")]
mod sharing;

use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result, bail};
use eguicn::{Badge, Button, ButtonVariant, Card, Dialog, Input, Select, Switch, Theme, egui};

use crate::config::{Config, PlayoutMode};
use model::ConfigDocument;

pub fn default_config_path() -> Result<PathBuf> {
    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME").context("HOME is not set; pass --config PATH")?;
        Ok(PathBuf::from(home).join("Library/Application Support/zflow/zflow.toml"))
    } else {
        Ok(PathBuf::from("/etc/zflow/zflow.toml"))
    }
}

#[derive(Clone, PartialEq, Eq)]
struct TextFields {
    listen: String,
    activation: String,
    escape: String,
    state_dir: String,
    control_socket: String,
    addresses: BTreeMap<String, String>,
}

impl TextFields {
    fn from_config(config: &Config) -> Self {
        Self {
            listen: config.transport.listen.to_string(),
            activation: config.input.activation_chord.join(" "),
            escape: config.input.escape_chord.join(" "),
            state_dir: config.daemon.state_dir.to_string_lossy().into_owned(),
            control_socket: config.daemon.control_socket.to_string_lossy().into_owned(),
            addresses: config
                .peers
                .iter()
                .map(|(name, peer)| {
                    (
                        name.clone(),
                        peer.addresses
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
                })
                .collect(),
        }
    }

    fn apply(&self, config: &Config) -> Result<Config> {
        let mut candidate = config.clone();
        candidate.transport.listen = parse_address(&self.listen).context("Listen address")?;
        candidate.input.activation_chord = self
            .activation
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        candidate.input.escape_chord = self.escape.split_whitespace().map(str::to_owned).collect();
        if self.state_dir.trim().is_empty() || self.control_socket.trim().is_empty() {
            bail!("State directory and control socket cannot be empty.");
        }
        // Keep non-UTF-8 paths intact unless the user edits their display text.
        if self.state_dir != config.daemon.state_dir.to_string_lossy() {
            candidate.daemon.state_dir = PathBuf::from(&self.state_dir);
        }
        if self.control_socket != config.daemon.control_socket.to_string_lossy() {
            candidate.daemon.control_socket = PathBuf::from(&self.control_socket);
        }
        for (name, text) in &self.addresses {
            if let Some(peer) = candidate.peers.get_mut(name) {
                peer.addresses = text
                    .split_whitespace()
                    .map(parse_address)
                    .collect::<Result<_>>()
                    .with_context(|| format!("Addresses for {name}"))?;
            }
        }
        Ok(candidate)
    }
}

fn parse_address(text: &str) -> Result<SocketAddr> {
    let address: SocketAddr = text
        .trim()
        .parse()
        .context("Use an IP address and port, such as 192.168.1.20:43119 or [::1]:43119")?;
    if address.port() == 0 {
        bail!("Port must be greater than zero.");
    }
    Ok(address)
}

#[derive(Default)]
struct LaunchOptions {
    peer: usize,
    address: String,
    no_touch: bool,
    reduce_wifi_latency: bool,
}

fn source_command(
    path: &std::path::Path,
    config: &Config,
    options: &LaunchOptions,
) -> Result<String> {
    let (name, peer) = config
        .peers
        .iter()
        .nth(options.peer)
        .context("Pair a computer first using zflow pair.")?;
    if !peer.permissions.connect || !peer.permissions.receive_normal {
        bail!("Allow this peer to connect and receive input before launching.");
    }
    let mut args = vec![
        "zflow-macos-source".to_owned(),
        "--config".into(),
        path.to_str()
            .context("The command preview requires a UTF-8 configuration path.")?
            .into(),
        "--peer".into(),
        name.clone(),
    ];
    if !options.address.trim().is_empty() {
        args.extend([
            "--address".into(),
            parse_address(&options.address)?.to_string(),
        ]);
    } else if peer.addresses.is_empty() {
        bail!("Add a peer address or enter an address override.");
    }
    if options.no_touch {
        args.push("--no-touch".into());
    }
    if options.reduce_wifi_latency {
        args.push("--reduce-wifi-latency".into());
    }
    Ok(args
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" "))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub struct SettingsApp {
    displays: displays::DisplayDiscovery,
    desktop_detector: displays::DesktopDetector,
    display_refresh: std::time::Instant,
    service_mode: bool,
    service_load:
        Option<std::sync::mpsc::Receiver<std::result::Result<crate::peer_view::Snapshot, String>>>,
    path: PathBuf,
    document: Option<ConfigDocument>,
    fields: Option<TextFields>,
    load_error: Option<String>,
    notice: String,
    page: usize,
    dark: bool,
    launch: LaunchOptions,
    confirm_reload: bool,
    confirm_close: bool,
    allow_close: bool,
    layout: layout_editor::LayoutEditor,
    nearby: nearby::NearbyBrowser,
    discovery_context: Option<egui::Context>,
    discovery_allowed: Option<bool>,
    pairing: pairing::PairingUi,
    #[cfg(target_os = "macos")]
    sharing: sharing::Sharing,
    #[cfg(target_os = "linux")]
    receiver: desktop::DesktopReceiver,
    #[cfg(target_os = "linux")]
    extension_install: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
}

impl SettingsApp {
    pub fn open_default(dark: bool) -> Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let base = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
                })
                .context("HOME is not set; pass --config PATH")?;
            Ok(Self::open_mode(base.join("zflow/zflow.toml"), dark, true))
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(Self::open(default_config_path()?, dark))
        }
    }

    pub fn open(path: PathBuf, dark: bool) -> Self {
        Self::open_mode(path, dark, false)
    }

    fn open_mode(path: PathBuf, dark: bool, service_mode: bool) -> Self {
        let layout = layout_editor::LayoutEditor::open(&path);
        let mut app = Self {
            displays: displays::DisplayDiscovery::default(),
            desktop_detector: displays::DesktopDetector::default(),
            display_refresh: std::time::Instant::now() - std::time::Duration::from_secs(3),
            service_mode,
            service_load: None,
            path,
            document: None,
            fields: None,
            load_error: None,
            notice: String::new(),
            page: 4,
            dark,
            launch: LaunchOptions::default(),
            confirm_reload: false,
            confirm_close: false,
            allow_close: false,
            layout,
            nearby: nearby::NearbyBrowser::default(),
            discovery_context: None,
            discovery_allowed: None,
            pairing: pairing::PairingUi::default(),
            #[cfg(target_os = "macos")]
            sharing: sharing::Sharing::default(),
            #[cfg(target_os = "linux")]
            receiver: desktop::DesktopReceiver::default(),
            #[cfg(target_os = "linux")]
            extension_install: None,
        };
        app.load();
        app
    }

    fn load(&mut self) {
        self.pairing.stop();
        self.displays.stop();
        if self.service_mode {
            self.load_service();
            return;
        }
        let loaded = if let Some(doc) = &mut self.document {
            doc.reload().map(|()| None)
        } else {
            ConfigDocument::open(self.path.clone()).map(Some)
        };
        match loaded {
            Ok(replacement) => {
                if let Some(document) = replacement {
                    self.document = Some(document);
                }
                let document = self.document.as_ref().expect("loaded document");
                if document.draft.peers.is_empty() {
                    self.page = 0;
                }
                self.path = document.path.clone();
                self.fields = Some(TextFields::from_config(&document.draft));
                self.load_error = None;
                self.notice.clear();
                self.launch.peer = 0;
                self.layout.reload();
                self.refresh_discovery();
            }
            Err(error) => self.load_error = Some(format!("{error:#}")),
        }
    }

    fn load_service(&mut self) {
        self.document = None;
        self.fields = None;
        self.load_error = Some("Reading paired computers from the local service…".into());
        self.refresh_discovery();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        self.service_load = Some(receiver);
        #[cfg(target_os = "linux")]
        std::thread::spawn(move || {
            let result = (|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(crate::peer_view::fetch())
            })()
            .map_err(|error: anyhow::Error| format!("{error:#}"));
            let _ = sender.send(result);
        });
        #[cfg(not(target_os = "linux"))]
        let _ = sender.send(Err("The desktop service API is Linux-only".into()));
    }

    fn poll_service(&mut self, ctx: &egui::Context) {
        let Some(receiver) = &self.service_load else {
            return;
        };
        match receiver.try_recv() {
            Ok(result) => {
                self.service_load = None;
                match result {
                    Ok(snapshot) => {
                        let config = snapshot.into_display_config();
                        self.fields = Some(TextFields::from_config(&config));
                        self.document =
                            Some(ConfigDocument::service_snapshot(self.path.clone(), config));
                        self.load_error = None;
                        self.layout.reload();
                        self.refresh_discovery();
                    }
                    Err(error) => self.load_error = Some(error),
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(50))
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.service_load = None;
                self.load_error = Some(
                    "The desktop API request stopped. Choose Refresh computers to retry.".into(),
                );
            }
        }
    }

    pub fn install_theme(&self, ctx: &egui::Context) {
        Theme::install_fonts(ctx);
        self.theme().apply(ctx);
    }

    /// Native startup opts into browsing; headless UI tests do not start networking.
    pub fn enable_discovery(&mut self, ctx: egui::Context) {
        self.discovery_context = Some(ctx);
        self.refresh_discovery();
    }

    fn refresh_discovery(&mut self) {
        let Some(ctx) = &self.discovery_context else {
            return;
        };
        let allowed = self.load_error.is_none()
            && self
                .document
                .as_ref()
                .is_some_and(|doc| !doc.is_new() && doc.saved().transport.discovery);
        if self.discovery_allowed != Some(allowed) {
            self.discovery_allowed = Some(allowed);
            if allowed {
                self.nearby.start(ctx.clone());
            } else {
                self.nearby.stop();
                self.displays.stop();
            }
        }
    }

    fn theme(&self) -> Theme {
        if self.dark {
            Theme::dark()
        } else {
            Theme::light()
        }
    }

    fn dirty(&self) -> bool {
        self.config_dirty() || self.layout.is_dirty()
    }

    fn config_dirty(&self) -> bool {
        self.document.as_ref().is_some_and(|doc| {
            doc.is_dirty()
                || self
                    .fields
                    .as_ref()
                    .is_some_and(|fields| *fields != TextFields::from_config(&doc.draft))
        })
    }

    fn sync_fields(&mut self) -> Result<()> {
        if self.service_mode {
            return Ok(());
        }
        let doc = self
            .document
            .as_mut()
            .context("No configuration is open.")?;
        doc.draft = self
            .fields
            .as_ref()
            .context("No settings loaded.")?
            .apply(&doc.draft)?;
        doc.validate()
    }

    fn save(&mut self) {
        let result = self
            .sync_fields()
            .and_then(|()| self.document.as_mut().expect("loaded document").save());
        self.notice = match result {
            Ok(()) => {
                self.fields = self
                    .document
                    .as_ref()
                    .map(|doc| TextFields::from_config(&doc.draft));
                "Saved to disk. Restart the affected daemon or source to use these settings.".into()
            }
            Err(error) => format!("Could not save: {error:#}"),
        };
        self.refresh_discovery();
    }

    fn runtime_active(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.sharing.is_active()
        }
        #[cfg(target_os = "linux")]
        {
            self.receiver.is_active()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            false
        }
    }

    #[cfg(target_os = "linux")]
    fn receiver_panel(&mut self, ui: &mut egui::Ui) {
        if let Some(receiver) = &self.extension_install {
            match receiver.try_recv() {
                Ok(result) => {
                    self.extension_install = None;
                    self.notice = match result {
                        Ok(()) => {
                            "GNOME integration installed. Enable desktop handoff below.".into()
                        }
                        Err(error) => error,
                    };
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.extension_install = None;
                    self.notice = "The GNOME integration installer stopped. Try again.".into();
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => ui
                    .ctx()
                    .request_repaint_after(std::time::Duration::from_millis(100)),
            }
        }
        let status = self.receiver.status();
        Card::new().padding(16).show(ui, |ui| {
            ui.set_width(ui.available_width());
            Card::header(ui, "Receive from your Mac", &status);
            if self.receiver.is_active() {
                if ui.add(Button::new("Disable desktop handoff").variant(ButtonVariant::Destructive)).clicked() {
                    self.receiver.stop();
                }
            } else {
                ui.horizontal(|ui| {
                    if ui.add_enabled(!self.pairing.is_active() && self.extension_install.is_none(), Button::new("Enable desktop handoff")).clicked() {
                        self.receiver.start(ui.ctx());
                    }
                    if ui.add_enabled(self.extension_install.is_none(), Button::new("Install GNOME integration").variant(ButtonVariant::Outline)).clicked() {
                        let (sender, receiver) = std::sync::mpsc::channel();
                        self.extension_install = Some(receiver);
                        let ctx = ui.ctx().clone();
                        if let Err(error) = std::thread::Builder::new().name("zflow-extension-install".into()).spawn(move || {
                            let _ = sender.send(desktop::install_extension().map_err(|error| format!("{error:#}")));
                            ctx.request_repaint();
                        }) { self.notice = format!("Could not start installer: {error}"); }
                    }
                });
                ui.label("Install the GNOME integration once. Pair your Mac, then enable desktop handoff and keep this window open.");
            }
        });
    }

    pub fn show(&mut self, root: &mut egui::Ui) {
        let ctx = root.ctx().clone();
        self.poll_service(&ctx);
        if let Some(doc) = &self.document {
            self.layout.update_desktops(
                self.displays.local.as_ref(),
                &self.displays.remote(doc.saved()),
                &doc.draft,
            );
        }
        if ctx.input(|input| input.viewport().close_requested())
            && self.dirty()
            && !self.allow_close
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_close = true;
        }
        let theme = self.theme();
        egui::Panel::top("header")
            .frame(
                egui::Frame::new()
                    .fill(theme.background)
                    .inner_margin(egui::Margin::symmetric(24, 18)),
            )
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("zflow").size(26.0).strong());
                    ui.add_space(12.0);
                    ui.add(
                        Badge::new(if self.service_mode {
                            "System service"
                        } else {
                            "Configuration"
                        })
                        .variant(ButtonVariant::Secondary),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                Button::new(if self.dark {
                                    "Light theme"
                                } else {
                                    "Dark theme"
                                })
                                .variant(ButtonVariant::Ghost),
                            )
                            .clicked()
                        {
                            self.dark = !self.dark;
                            self.theme().apply(&ctx);
                        }
                    });
                });
                muted(
                    ui,
                    "Pair your computers, arrange the layout, then enable sharing.",
                );
            });
        if self.discovery_context.is_some() && self.load_error.is_none() {
            #[cfg(target_os = "macos")]
            {
                let editable = !self.dirty() && !self.pairing.is_active();
                if let Some(doc) = &self.document {
                    egui::Panel::top("sharing").show(root, |ui| {
                        self.sharing
                            .show(ui, &doc.path, doc.saved(), editable && !doc.is_new());
                    });
                }
            }
            #[cfg(target_os = "linux")]
            if self.service_mode {
                egui::Panel::top("desktop-receiver").show(root, |ui| self.receiver_panel(ui));
            }
        }
        let runtime_active = self.runtime_active();
        let editing_allowed = !runtime_active && !self.pairing.is_active();
        let mut save = false;
        let mut save_layout = false;
        let mut reload = false;
        let validation = if self.document.is_some() {
            self.sync_fields().err().map(|e| format!("{e:#}"))
        } else {
            None
        };
        egui::Panel::bottom("actions")
            .frame(
                egui::Frame::new()
                    .fill(theme.background)
                    .inner_margin(egui::Margin::symmetric(24, 14)),
            )
            .show(root, |ui| {
                if let Some(error) = &validation {
                    ui.colored_label(theme.destructive, error);
                }
                if !self.notice.is_empty() {
                    ui.label(&self.notice);
                }
                ui.horizontal(|ui| {
                    ui.label(
                        if self.document.as_ref().is_some_and(ConfigDocument::is_new) {
                            "New file"
                        } else if self.dirty() {
                            if self.layout.is_dirty() {
                                "Unsaved layout changes"
                            } else {
                                "Unsaved changes"
                            }
                        } else {
                            "No unsaved changes"
                        },
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.page == 4 {
                            save_layout = ui
                                .add_enabled(
                                    editing_allowed
                                        && self.load_error.is_none()
                                        && self
                                            .document
                                            .as_ref()
                                            .is_some_and(|doc| self.layout.can_save(&doc.draft)),
                                    Button::new("Save layout"),
                                )
                                .clicked();
                        } else if !self.service_mode {
                            save = ui
                                .add_enabled(
                                    editing_allowed
                                        && self.load_error.is_none()
                                        && validation.is_none()
                                        && self
                                            .document
                                            .as_ref()
                                            .is_some_and(|d| d.is_new() || self.config_dirty()),
                                    Button::new("Save configuration"),
                                )
                                .clicked();
                        }
                        reload = ui
                            .add_enabled(
                                editing_allowed,
                                Button::new(if self.service_mode {
                                    "Refresh computers"
                                } else {
                                    "Reload from disk"
                                })
                                .variant(ButtonVariant::Outline),
                            )
                            .clicked();
                    });
                });
                ui.label(
                    egui::RichText::new(self.path.display().to_string())
                        .small()
                        .color(theme.muted_foreground),
                );
            });
        egui::Panel::left("navigation")
            .exact_size(182.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(theme.background).inner_margin(16))
            .show(root, |ui| {
                for (index, label) in [
                    (4, "Layout"),
                    (5, "Nearby"),
                    (0, "Computers"),
                    (1, "Input"),
                    (2, "Connection"),
                    (3, "Advanced"),
                ] {
                    if self.service_mode && (1..=3).contains(&index) {
                        continue;
                    }
                    if ui
                        .add(
                            Button::new(label)
                                .variant(ButtonVariant::Ghost)
                                .selected(self.page == index)
                                .width(150.0),
                        )
                        .clicked()
                    {
                        self.page = index;
                    }
                    ui.add_space(4.0);
                }
                ui.add_space(24.0);
                muted(ui, "Edge switching");
                ui.add_space(4.0);
                muted(
                    ui,
                    "Save your layout, enable desktop handoff on Ubuntu, then enable sharing on the Mac.",
                );
            });
        let mut paired = false;
        egui::CentralPanel::default().frame(egui::Frame::new().fill(theme.background).inner_margin(24)).show(root, |ui| {
            egui::ScrollArea::vertical().id_salt(("page", self.page)).auto_shrink([false, false]).show(ui, |ui| {
                if let Some(error) = &self.load_error {
                    if self.service_mode {
                        heading(ui, "Local service", "Paired computers come from the service. Private settings stay protected.");
                        ui.colored_label(theme.destructive, error);
                        muted(ui, "Use the active Ubuntu desktop session and the updated zflowd service, then choose Refresh computers. --config PATH opens a separate file editor.");
                        return;
                    }
                    card(ui, "Could not load configuration", "Your file has not been changed.", |ui| { ui.colored_label(theme.destructive, error); muted(ui, "Fix the file or its permissions, then choose Reload from disk. You can choose another file with --config PATH."); });
                    return;
                }
                let Some(doc) = self.document.as_mut() else { return; };
                let fields = self.fields.as_mut().expect("loaded text fields");
                if doc.is_new() {
                    muted(ui, "Choose Save configuration to create your settings, then pair a computer below.");
                    ui.add_space(14.0);
                }
                ui.add_enabled_ui(editing_allowed, |ui| match self.page {
                    4 => {
                        self.layout.show(ui, &doc.draft);
                        if let Some(error) = self.desktop_detector.snapshot().1 { muted(ui, &error); }
                        if let Some(error) = self.displays.error() { muted(ui, &format!("Desktop discovery unavailable: {error}")); }
                    },
                    5 => {
                        heading(ui, "Nearby computers", "Choose a computer, then compare the pairing codes on both screens.");
                        if let Some(address) = self.nearby.show(ui, doc.saved()) {
                            self.pairing.set_address(address);
                            self.notice = "Pairing address selected. Allow pairing on the other computer, then choose Pair with address.".into();
                            self.page = 0;
                        }
                    },
                    0 if self.service_mode => {
                        heading(ui, "Paired computers", "Computers saved by the local service.");
                        for (name, peer) in &doc.draft.peers {
                            card(ui, name, "Saved pairing", |ui| {
                                if let Ok(fingerprint) = peer.fingerprint_hex() { ui.monospace(fingerprint); }
                                for address in &peer.addresses { ui.label(address.to_string()); }
                            });
                        }
                        if doc.draft.peers.is_empty() { muted(ui, "No paired computers yet. Allow pairing below."); }
                    },
                    0 => computers(ui, &mut doc.draft, fields),
                    1 => input(ui, &mut doc.draft, fields),
                    2 => connection(ui, &mut doc.draft, fields),
                    _ => advanced(ui, fields),
                });
                if self.page == 0 {
                    ui.add_space(12.0);
                    let target = if self.service_mode { pairing::PairingTarget::Service }
                        else { pairing::PairingTarget::File { path: doc.path.clone(), config: Box::new(doc.saved().clone()) } };
                    let saved = self.service_mode || (!doc.is_new() && !doc.is_dirty() && *fields == TextFields::from_config(&doc.draft));
                    paired = self.pairing.show(ui, target, saved && !runtime_active);
                }
                if self.page == 3 && cfg!(target_os = "macos") {
                    let dirty = doc.is_dirty() || *fields != TextFields::from_config(&doc.draft) || doc.is_new();
                    launch_panel(ui, &doc.path, &doc.draft, &mut self.launch, dirty);
                }
            });
        });
        if paired {
            self.load();
        }
        if save {
            self.save();
        }
        if save_layout && let Some(document) = &self.document {
            self.layout.save(&document.draft);
        }
        if reload {
            if self.dirty() {
                self.confirm_reload = true;
            } else {
                self.load();
            }
        }
        let mut discard = false;
        Dialog::new(
            "discard-reload",
            &mut self.confirm_reload,
            "Discard unsaved changes?",
            "Reload configuration and layout from disk and discard your edits.",
        )
        .show(&ctx, |ui| {
            ui.horizontal(|ui| {
                discard = ui
                    .add(Button::new("Discard and reload").variant(ButtonVariant::Destructive))
                    .clicked();
                discard
                    || ui
                        .add(Button::new("Keep editing").variant(ButtonVariant::Outline))
                        .clicked()
            })
            .inner
        });
        if discard {
            self.load();
        }
        let mut close = false;
        Dialog::new(
            "discard-close",
            &mut self.confirm_close,
            "Close without saving?",
            "You have unsaved configuration or layout changes.",
        )
        .show(&ctx, |ui| {
            ui.horizontal(|ui| {
                close = ui
                    .add(Button::new("Discard and close").variant(ButtonVariant::Destructive))
                    .clicked();
                close
                    || ui
                        .add(Button::new("Keep editing").variant(ButtonVariant::Outline))
                        .clicked()
            })
            .inner
        });
        if close {
            self.allow_close = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

impl eframe::App for SettingsApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.display_refresh.elapsed() >= std::time::Duration::from_secs(2) {
            self.display_refresh = std::time::Instant::now();
            self.desktop_detector.refresh(ui.ctx());
        }
        let (local, _) = self.desktop_detector.snapshot();
        self.displays
            .update(ui.ctx(), local, self.discovery_allowed == Some(true));
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs(2));
        self.show(ui);
    }
}

fn computers(ui: &mut egui::Ui, config: &mut Config, fields: &mut TextFields) {
    heading(
        ui,
        "Your computers",
        "Paired identities and access. Start sharing with the controls above.",
    );
    if config.peers.is_empty() {
        return;
    }
    for (name, peer) in &mut config.peers {
        ui.push_id(name, |ui| {
            card(ui, name, "Saved peer. Reachability unknown.", |ui| {
                let label = ui.label("Input addresses (one IP:port per line)");
                ui.add(Input::new(fields.addresses.get_mut(name).expect("peer address field")).id((name, "addresses")).multiline(2)).labelled_by(label.id);
                ui.add_space(8.0);
                ui.add(Switch::new(&mut peer.permissions.connect, "Allow connections"));
                ui.add(Switch::new(&mut peer.permissions.send_normal, "Allow this peer to send input"));
                ui.add(Switch::new(&mut peer.permissions.receive_normal, "Allow this peer to receive input"));
                ui.add(Switch::new(&mut peer.permissions.inject_prelogin, "Allow this peer to send input at the login screen"));
                muted(ui, "Login-screen access also requires the receiver's global permission. Enable it only for a trusted computer.");
                ui.add_space(10.0);
                muted(ui, "Identity fingerprint (SHA-256)");
                if let Ok(fingerprint) = peer.fingerprint_hex() {
                    ui.add(egui::Label::new(egui::RichText::new(fingerprint).monospace().size(11.0)).wrap());
                }
            });
        });
    }
    muted(
        ui,
        "Pair another computer below. Use zflow peer revoke to remove an existing identity.",
    );
}

fn input(ui: &mut egui::Ui, config: &mut Config, fields: &mut TextFields) {
    heading(
        ui,
        "Input",
        "Trackpad forwarding, keyboard shortcuts, and local device setup.",
    );
    card(
        ui,
        "Trackpad gestures",
        "Enable raw contact forwarding on both the source and receiver.",
        |ui| {
            ui.add(Switch::new(
                &mut config.input.experimental_touchpad,
                "Experimental touchpad forwarding",
            ));
            muted(
                ui,
                "On the Mac, this enables the raw Magic Trackpad path unless you launch with --no-touch. Restart the Linux receiver after changing this setting.",
            );
        },
    );
    card(
        ui,
        "Linux shortcuts",
        "Use space-separated evdev key names, such as KEY_LEFTCTRL KEY_LEFTMETA KEY_F12.",
        |ui| {
            field(ui, "Activation shortcut", &mut fields.activation);
            field(ui, "Return-to-local shortcut", &mut fields.escape);
            muted(
                ui,
                "These shortcuts apply to the Linux daemon. On macOS, Ctrl+Cmd+Backspace returns input; you cannot change that shortcut yet.",
            );
        },
    );
    card(
        ui,
        "Linux input access",
        "Settings for the receiver and its physical capture devices.",
        |ui| {
            ui.add(Switch::new(
                &mut config.input.allow_prelogin_input,
                "Allow input at the login screen",
            ));
            muted(
                ui,
                "This allows input outside an unlocked session. Each peer also needs its own login-screen permission.",
            );
            ui.add_space(12.0);
            if config.input.capture_devices.is_empty() {
                muted(
                    ui,
                    "No Linux capture devices selected. A receiver-only machine does not need them.",
                );
            }
            for device in &config.input.capture_devices {
                ui.label(device.name.as_deref().unwrap_or("Input device"));
                ui.monospace(device.path.display().to_string());
                if let Some(phys) = &device.phys {
                    muted(ui, phys);
                }
            }
            ui.add_space(8.0);
            muted(
                ui,
                "Select hardware with zflow devices and zflow setup --device PATH --udev-rules PATH. Setup records device identity and creates scoped permissions; this window keeps those records intact.",
            );
        },
    );
}

fn connection(ui: &mut egui::Ui, config: &mut Config, fields: &mut TextFields) {
    heading(
        ui,
        "Connection",
        "Network settings and the balance between buffering and latency.",
    );
    card(
        ui,
        "Network",
        "The listen address applies to the Linux daemon. Discovery also controls this window's Nearby browser.",
        |ui| {
            field(ui, "Listen address", &mut fields.listen);
            ui.add(Switch::new(
                &mut config.transport.discovery,
                "Discover computers on the local network",
            ));
            muted(
                ui,
                "Save to apply discovery to this window. Restart the Linux daemon after changing either setting. Discovery does not pair or authorize computers.",
            );
        },
    );
    card(
        ui,
        "Receiver buffering",
        "Edit this on the receiving computer. Saving on the Mac does not change Ubuntu.",
        |ui| {
            let mut mode = usize::from(config.playout.mode == PlayoutMode::Fixed);
            ui.label("Buffering mode");
            Select::new("playout-mode", &mut mode, &["Adaptive", "Fixed"]).show(ui);
            config.playout.mode = if mode == 0 {
                PlayoutMode::Adaptive
            } else {
                PlayoutMode::Fixed
            };
            ui.add_space(8.0);
            ui.add_enabled_ui(mode == 1, |ui| {
                milliseconds(ui, "Fixed delay", &mut config.playout.fixed_delay_ms);
            });
            ui.add_enabled_ui(mode == 0, |ui| {
                milliseconds(ui, "Minimum delay", &mut config.playout.minimum_delay_ms);
                milliseconds(ui, "Maximum delay", &mut config.playout.maximum_delay_ms);
                ui.horizontal(|ui| {
                    let label = ui.label("Arrival percentile");
                    ui.add(
                        egui::DragValue::new(&mut config.playout.percentile)
                            .speed(0.001)
                            .range(0.5..=0.999)
                            .max_decimals(3),
                    )
                    .labelled_by(label.id);
                });
            });
            muted(
                ui,
                "More buffering can absorb arrival jitter, but adds delay. Fix Wi-Fi latency spikes before increasing these values.",
            );
        },
    );
    card(
        ui,
        "Recovery timing",
        "The lease limits how long the receiver holds input without a renewal.",
        |ui| {
            milliseconds(
                ui,
                "Checkpoint interval",
                &mut config.transport.checkpoint_ms,
            );
            milliseconds(ui, "Input lease", &mut config.transport.lease_ms);
            muted(
                ui,
                "Checkpoint: 1-250 ms. Lease: 1-1000 ms. Save also checks the protocol's timing constraints.",
            );
        },
    );
}

fn advanced(ui: &mut egui::Ui, fields: &mut TextFields) {
    heading(
        ui,
        "Advanced",
        "Local paths. This window does not move files or install services.",
    );
    card(
        ui,
        "Identity and service",
        "Restart the affected daemon or source after changing paths.",
        |ui| {
            field(ui, "State directory", &mut fields.state_dir);
            muted(
                ui,
                "Contains this computer's private identity. Choosing an empty directory creates a different identity on the next launch and requires pairing again.",
            );
            ui.add_space(8.0);
            field(ui, "Control socket", &mut fields.control_socket);
            muted(
                ui,
                "Linux daemon only. The CLI and service must use the same socket path.",
            );
        },
    );
    card(
        ui,
        "Saving settings",
        "Save writes this file only; it does not reload a running process.",
        |ui| {
            muted(
                ui,
                "You need permission to write the selected file. This app does not request administrator access. For a protected Linux configuration, use the existing CLI setup workflow.",
            );
            muted(
                ui,
                "If another process edits the file, reload it before saving. Saving rewrites TOML formatting and removes comments.",
            );
        },
    );
}

fn launch_panel(
    ui: &mut egui::Ui,
    path: &std::path::Path,
    config: &Config,
    launch: &mut LaunchOptions,
    dirty: bool,
) {
    card(
        ui,
        "Mac source launch",
        "Prepare a terminal command. These options last for this window and do not change the configuration file.",
        |ui| {
            let names: Vec<_> = config.peers.keys().map(String::as_str).collect();
            ui.label("Send input to");
            Select::new("launch-peer", &mut launch.peer, &names).show(ui);
            field(ui, "Address override (optional)", &mut launch.address);
            ui.add(Switch::new(
                &mut launch.no_touch,
                "Use pointer and scroll only (--no-touch)",
            ));
            ui.add(Switch::new(
                &mut launch.reduce_wifi_latency,
                "Reduce Wi-Fi latency during remote control",
            ));
            muted(
                ui,
                "Wi-Fi mode needs the installed AWDL helper. It suspends awdl0 while forwarding, which can interrupt AirDrop and Continuity; it restores the interface on release. This window does not run the helper.",
            );
            ui.add_space(8.0);
            if dirty {
                muted(
                    ui,
                    "Save the configuration before copying a command for these settings.",
                );
            }
            match source_command(path, config, launch) {
                Ok(command) => {
                    ui.add(
                        egui::Label::new(egui::RichText::new(&command).monospace().size(12.0))
                            .wrap(),
                    );
                    if ui
                        .add_enabled(
                            !dirty,
                            Button::new("Copy launch command").variant(ButtonVariant::Outline),
                        )
                        .clicked()
                    {
                        ui.ctx().copy_text(command);
                    }
                    muted(
                        ui,
                        "Build zflow-macos-source and put it on PATH, or use ./target/debug/zflow-macos-source from the checkout. Running the command takes control of input. Return with Ctrl+Cmd+Backspace.",
                    );
                }
                Err(error) => {
                    muted(ui, &format!("{error:#}"));
                }
            }
        },
    );
}

fn heading(ui: &mut egui::Ui, title: &str, description: &str) {
    ui.label(egui::RichText::new(title).size(28.0).strong());
    muted(ui, description);
    ui.add_space(20.0);
}

fn card(ui: &mut egui::Ui, title: &str, description: &str, contents: impl FnOnce(&mut egui::Ui)) {
    Card::new().padding(20).show(ui, |ui| {
        ui.set_width(ui.available_width());
        Card::header(ui, title, description);
        contents(ui);
    });
    ui.add_space(16.0);
}

fn field(ui: &mut egui::Ui, label: &str, value: &mut String) {
    let response = ui.label(label);
    ui.add(Input::new(value).id(ui.id().with(label)))
        .labelled_by(response.id);
    ui.add_space(8.0);
}

fn milliseconds(ui: &mut egui::Ui, label: &str, value: &mut u64) {
    ui.horizontal(|ui| {
        let response = ui.label(label);
        ui.add(egui::DragValue::new(value).suffix(" ms").speed(1.0))
            .labelled_by(response.id);
    });
    ui.add_space(4.0);
}

fn muted(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .size(13.0)
            .color(Theme::from_ui(ui).muted_foreground),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PeerConfig, PeerPermissions};

    fn frame(
        app: &mut SettingsApp,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1080.0, 800.0),
                )),
                events,
                ..Default::default()
            },
            |ui| app.show(ui),
        );
        output.textures_delta.clear();
        output
    }

    fn click(app: &mut SettingsApp, ctx: &egui::Context, label: &str) {
        frame(app, ctx, vec![]);
        let output = frame(app, ctx, vec![]);
        let position = output
            .shapes
            .iter()
            .find_map(|shape| {
                if let egui::epaint::Shape::Text(text) = &shape.shape
                    && text.galley.job.text == label
                {
                    return Some(text.pos + text.galley.rect.center().to_vec2());
                }
                None
            })
            .unwrap_or_else(|| panic!("No visible control labelled {label}"));
        for pressed in [true, false] {
            frame(
                app,
                ctx,
                vec![
                    egui::Event::PointerMoved(position),
                    egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: Default::default(),
                    },
                ],
            );
        }
    }

    #[test]
    fn gui_navigation_toggle_save_and_reload_use_real_widgets() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        Config::default().save(&path).unwrap();
        let mut app = SettingsApp::open(path.clone(), false);
        let ctx = egui::Context::default();
        app.install_theme(&ctx);
        click(&mut app, &ctx, "Input");
        assert_eq!(app.page, 1);
        click(&mut app, &ctx, "Experimental touchpad forwarding");
        assert!(app.dirty());
        assert!(!Config::load(&path).unwrap().input.experimental_touchpad);
        click(&mut app, &ctx, "Save configuration");
        assert!(Config::load(&path).unwrap().input.experimental_touchpad);
        assert!(!app.dirty());
        click(&mut app, &ctx, "Experimental touchpad forwarding");
        click(&mut app, &ctx, "Reload from disk");
        assert!(app.confirm_reload);
        click(&mut app, &ctx, "Discard and reload");
        assert!(
            app.document
                .as_ref()
                .unwrap()
                .draft
                .input
                .experimental_touchpad
        );
        assert!(!app.dirty());
        click(&mut app, &ctx, "Dark theme");
        assert!(app.dark);
        for (page, label) in ["Computers", "Input", "Connection", "Advanced"]
            .iter()
            .enumerate()
        {
            click(&mut app, &ctx, label);
            assert_eq!(app.page, page);
        }
    }

    #[test]
    fn invalid_text_is_dirty_and_cannot_be_saved() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        Config::default().save(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        let mut app = SettingsApp::open(path.clone(), false);
        app.fields.as_mut().unwrap().listen = "invalid".into();
        assert!(app.dirty());
        app.save();
        assert!(app.notice.starts_with("Could not save:"));
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert_eq!(app.fields.as_ref().unwrap().listen, "invalid");
    }

    #[test]
    fn opening_and_drawing_missing_or_malformed_files_does_not_write() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let ctx = egui::Context::default();
        let mut app = SettingsApp::open(path.clone(), false);
        app.install_theme(&ctx);
        for page in 0..4 {
            app.page = page;
            frame(&mut app, &ctx, vec![]);
        }
        assert!(!path.exists());
        std::fs::write(&path, "invalid = [").unwrap();
        let mut app = SettingsApp::open(path.clone(), false);
        assert!(app.load_error.is_some());
        frame(&mut app, &ctx, vec![]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "invalid = [");
    }

    fn paired_config() -> Config {
        let mut config = Config::default();
        config.peers.insert(
            "Ubuntu's desk".into(),
            PeerConfig::from_spki(
                &[1, 2, 3],
                vec!["192.0.2.10:43119".parse().unwrap()],
                PeerPermissions {
                    connect: true,
                    receive_normal: true,
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        config
    }

    #[test]
    fn text_fields_round_trip_and_reject_invalid_addresses() {
        let config = paired_config();
        let mut fields = TextFields::from_config(&config);
        assert_eq!(fields.apply(&config).unwrap(), config);
        fields.listen = "localhost".into();
        assert!(fields.apply(&config).is_err());
        fields.listen = "[::]:43119".into();
        fields
            .addresses
            .insert("Ubuntu's desk".into(), "192.0.2.10:0".into());
        assert!(fields.apply(&config).is_err());
    }

    #[test]
    fn source_flags_are_opt_in_and_command_arguments_are_quoted() {
        let config = paired_config();
        let path = std::path::Path::new("/tmp/Mac's config.toml");
        let mut options = LaunchOptions::default();
        let command = source_command(path, &config, &options).unwrap();
        assert!(command.contains("'Ubuntu'\"'\"'s desk'"));
        assert!(command.contains("'/tmp/Mac'\"'\"'s config.toml'"));
        assert!(!command.contains("--no-touch"));
        assert!(!command.contains("--reduce-wifi-latency"));
        options.no_touch = true;
        options.reduce_wifi_latency = true;
        options.address = "[::1]:43119".into();
        let command = source_command(path, &config, &options).unwrap();
        assert!(command.contains("'--no-touch' '--reduce-wifi-latency'"));
        assert!(command.contains("'--address' '[::1]:43119'"));
        options.address = "$(touch /tmp/nope)".into();
        assert!(source_command(path, &config, &options).is_err());
    }

    #[test]
    fn source_preview_requires_pairing_address_and_permission() {
        let path = std::path::Path::new("/tmp/config.toml");
        let options = LaunchOptions::default();
        assert!(source_command(path, &Config::default(), &options).is_err());
        let mut config = paired_config();
        config.peers.values_mut().next().unwrap().addresses.clear();
        assert!(source_command(path, &config, &options).is_err());
        let options = LaunchOptions {
            address: "192.0.2.1:43119".into(),
            ..Default::default()
        };
        config
            .peers
            .values_mut()
            .next()
            .unwrap()
            .permissions
            .connect = false;
        assert!(source_command(path, &config, &options).is_err());
    }
}
