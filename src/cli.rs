use std::{
    fs::{self, OpenOptions},
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(target_os = "linux")]
use std::{
    os::unix::fs::{FileTypeExt, MetadataExt},
    process::{Command as ProcessCommand, Stdio},
    thread,
    time::Instant,
};

use anyhow::{Context, Result, bail};
use clap::{
    ArgAction, Args, Parser, Subcommand,
    builder::{PossibleValuesParser, TypedValueParser},
};
use serde::Serialize;

use crate::{
    config::{Config, PlayoutMode},
    control::{DaemonStatus, Request, Response, read_message, write_message},
    core::KeyboardMode,
    identity::Identity,
};

#[derive(Debug, Parser)]
#[command(name = "zflow", version, about = "Headless zflow setup and control")]
pub struct Cli {
    /// Configuration file used by setup and offline diagnostics.
    #[arg(long, global = true, default_value = "/etc/zflow/zflow.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Check for or install a published Linux release.
    #[cfg(target_os = "linux")]
    Update {
        #[command(subcommand)]
        command: UpdateCommand,
    },
    /// Open native GNOME settings. Closing the window leaves sharing running.
    Settings,
    /// Run the GNOME session integration without a window.
    DesktopAgent {
        /// Install the GNOME extension and start this agent at login.
        #[arg(long)]
        install: bool,
    },
    /// Create or update the local configuration.
    Setup(SetupOptions),
    /// Show daemon and ownership state.
    Status {
        /// Emit stable machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Check Linux input, permissions, service, and configuration.
    Doctor,
    /// List physical input devices and capture-set membership.
    Devices,
    /// List paired peers and their permissions.
    Peers,
    /// List computers found on the network that are not trusted here.
    Nearby {
        /// Say hello to this IP address first, port optional, for a
        /// computer mDNS cannot see, such as one on Tailscale.
        #[arg(long, value_name = "ADDRESS")]
        add: Option<String>,
    },
    /// Trust a computer `zflow nearby` lists, by name or mark, as dragging
    /// it into the arrangement does.
    Trust {
        /// Its name, or its mark when two share a name.
        computer: String,
    },
    /// Change one paired peer.
    Peer {
        #[command(subcommand)]
        command: PeerCommand,
    },
    /// Arm remote ownership for a paired peer.
    Switch { peer: String },
    /// Return input ownership to this machine.
    Local,
    /// Change persistent receiver playout settings.
    Playout {
        #[arg(value_parser = playout_mode())]
        mode: PlayoutMode,
        #[arg(long)]
        fixed_delay_ms: Option<u64>,
        #[arg(long)]
        minimum_delay_ms: Option<u64>,
        #[arg(long)]
        maximum_delay_ms: Option<u64>,
        #[arg(long)]
        percentile: Option<f64>,
    },
}

#[cfg(target_os = "linux")]
#[derive(Debug, Subcommand)]
enum UpdateCommand {
    /// Check the latest stable release and print JSON. Does not install anything.
    Check,
    /// Install a newer stable release with administrator authorization.
    Install {
        /// Exact release tag returned by update check.
        #[arg(long)]
        version: String,
        /// Require the desktop's graphical authentication agent.
        #[arg(long)]
        gui: bool,
    },
}

#[derive(Debug, Default, Args)]
struct SetupOptions {
    /// Override the QUIC listen address; changes require a daemon restart.
    #[arg(long)]
    listen: Option<SocketAddr>,
    /// Select a physical evdev node; repeated values replace the capture set.
    #[arg(long = "device")]
    devices: Vec<PathBuf>,
    /// Set one evdev key in the activation chord; repeat for every key.
    #[arg(long = "activation-key", value_name = "EVDEV_KEY")]
    activation_chord: Vec<String>,
    /// Set one evdev key in the local escape chord; repeat for every key.
    #[arg(long = "escape-key", value_name = "EVDEV_KEY")]
    escape_chord: Vec<String>,
    /// Write explicit capture permissions for the selected devices.
    #[arg(long)]
    udev_rules: Option<PathBuf>,
    /// Enable or disable injection outside an unlocked authenticated session.
    #[arg(long = "prelogin", value_name = "on|off", value_parser = on_off())]
    allow_prelogin: Option<bool>,
    /// Enable or disable experimental raw touchpad forwarding.
    #[arg(long = "experimental-touchpad", value_name = "on|off", value_parser = on_off())]
    experimental_touchpad: Option<bool>,
}

#[derive(Debug, Subcommand)]
enum PeerCommand {
    /// Revoke a peer and close its future access.
    Revoke { peer: String },
    /// Grant or revoke the separate pre-login permission.
    AllowPrelogin {
        peer: String,
        #[arg(action = ArgAction::Set, value_parser = on_off())]
        value: bool,
    },
    /// Choose how keys from a peer act here, from its next crossing.
    Keyboard {
        peer: String,
        #[arg(value_parser = keyboard_mode())]
        mode: KeyboardMode,
    },
}

fn on_off() -> impl TypedValueParser<Value = bool> {
    PossibleValuesParser::new(["on", "off"]).map(|value| value == "on")
}

fn keyboard_mode() -> impl TypedValueParser<Value = KeyboardMode> {
    PossibleValuesParser::new(["standard", "pc-positions", "mac"]).map(|mode| match mode.as_str() {
        "pc-positions" => KeyboardMode::PcPositions,
        "mac" => KeyboardMode::Mac,
        _ => KeyboardMode::Standard,
    })
}

fn playout_mode() -> impl TypedValueParser<Value = PlayoutMode> {
    PossibleValuesParser::new(["fixed", "adaptive"]).map(|mode| match mode.as_str() {
        "fixed" => PlayoutMode::Fixed,
        _ => PlayoutMode::Adaptive,
    })
}

/// Orders zflowd before the display manager so input works at the greeter.
/// It stays installed only while pre-login input is on: with Type=notify, a
/// zflowd that never becomes ready would otherwise delay every boot.
/// Archive installs used this name, so `--prelogin off` also removes their copy.
const PRELOGIN_DROPIN: &str = "/etc/systemd/system/zflowd.service.d/prelogin.conf";
const PRELOGIN_ORDERING: &str = include_str!("../packaging/systemd/zflowd-prelogin.conf");

pub fn run(cli: Cli) -> Result<()> {
    let path = cli.config;
    match cli.command {
        #[cfg(target_os = "linux")]
        Command::Update { command } => match command {
            UpdateCommand::Check => crate::app::updates::check(),
            UpdateCommand::Install { version, gui } => crate::app::updates::install(&version, gui),
        },
        Command::Settings => {
            #[cfg(target_os = "linux")]
            {
                crate::app::gnome::settings()
            }
            #[cfg(not(target_os = "linux"))]
            {
                bail!("Open Settings from the native zflow menu-bar app")
            }
        }
        Command::DesktopAgent { install } => {
            #[cfg(target_os = "linux")]
            {
                crate::app::desktop_agent::run(install)
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = install;
                bail!("The desktop agent requires Linux with GNOME")
            }
        }
        Command::Setup(options) => setup(
            path,
            options,
            cfg!(target_os = "linux").then(|| PRELOGIN_DROPIN.into()),
        ),
        Command::Status { json } => status(path, json),
        Command::Doctor => doctor(path),
        Command::Devices => devices(path),
        Command::Peers => peers(path),
        Command::Nearby { add } => nearby(path, add),
        Command::Trust { computer } => trust(path, computer),
        Command::Peer { command } => match command {
            PeerCommand::Revoke { peer } => revoke_peer(path, peer),
            PeerCommand::AllowPrelogin { peer, value } => allow_prelogin(path, peer, value),
            PeerCommand::Keyboard { peer, mode } => set_peer_keyboard(path, peer, mode),
        },
        Command::Switch { peer } => daemon_command(path, Request::Activate { peer }),
        Command::Local => daemon_command(path, Request::Local),
        Command::Playout {
            mode,
            fixed_delay_ms,
            minimum_delay_ms,
            maximum_delay_ms,
            percentile,
        } => playout(
            path,
            mode,
            fixed_delay_ms,
            minimum_delay_ms,
            maximum_delay_ms,
            percentile,
        ),
    }
}

/// `prelogin_dropin` is where `--prelogin` installs or removes the boot
/// ordering drop-in; none leaves boot ordering alone.
fn setup(path: PathBuf, options: SetupOptions, prelogin_dropin: Option<PathBuf>) -> Result<()> {
    let SetupOptions {
        listen,
        devices,
        activation_chord,
        escape_chord,
        udev_rules,
        allow_prelogin,
        experimental_touchpad,
    } = options;
    let existed = path.exists();
    let mut config = if existed {
        Config::load(&path)?
    } else {
        Config::default()
    };
    let original_config = existed.then(|| config.clone());
    if let Some(listen) = listen {
        config.transport.listen = listen;
    }
    if !devices.is_empty() {
        config.input.capture_devices = devices
            .into_iter()
            .map(resolve_device_selector)
            .collect::<Result<_>>()?;
    }
    if !activation_chord.is_empty() {
        config.input.activation_chord = activation_chord;
    }
    if !escape_chord.is_empty() {
        config.input.escape_chord = escape_chord;
    }
    if let Some(allow_prelogin) = allow_prelogin {
        config.input.allow_prelogin_input = allow_prelogin;
    }
    if let Some(experimental_touchpad) = experimental_touchpad {
        config.input.experimental_touchpad = experimental_touchpad;
    }

    config.validate()?;
    #[cfg(target_os = "linux")]
    crate::runtime::LinuxRuntimeConfig::from_config(&config).validate()?;
    crate::session::SessionOptions::from_config(&config)?;
    // The daemon binds its listen address once; everything else setup writes
    // reaches a running daemon through a live reload.
    let restart_required = original_config
        .as_ref()
        .is_some_and(|previous| previous.transport.listen != config.transport.listen);
    let rendered_rules = udev_rules
        .map(|rules_path| {
            render_capture_rules(&config.input.capture_devices).map(|rules| (rules_path, rules))
        })
        .transpose()?;
    let identity = load_or_create_identity(&config.daemon.state_dir)?;

    let previous_config = FileSnapshot::capture(&path)?;
    let previous_rules = rendered_rules
        .as_ref()
        .map(|(rules_path, _)| FileSnapshot::capture(rules_path))
        .transpose()?;
    let prelogin_ordering = match prelogin_dropin.zip(allow_prelogin) {
        Some((dropin, enabled)) => {
            let previous = FileSnapshot::capture(&dropin)?;
            let wanted = enabled.then(|| PRELOGIN_ORDERING.as_bytes().to_vec());
            (previous.contents != wanted).then_some((dropin, previous, wanted))
        }
        None => None,
    };
    let commit: Result<()> = (|| {
        config.save(&path)?;
        if let Some((rules_path, rules)) = &rendered_rules {
            write_atomic_bytes(rules_path, rules.as_bytes(), 0o644)?;
        }
        if let Some((dropin, _, wanted)) = &prelogin_ordering {
            // Restoring a snapshot of the wanted state writes or removes the file.
            FileSnapshot {
                contents: wanted.clone(),
            }
            .restore(dropin, 0o644)
            .with_context(|| format!("could not update {}", dropin.display()))?;
            systemd_daemon_reload()?;
        }
        if !restart_required {
            reload_running_daemon(&config)?;
        }
        Ok(())
    })();
    if let Err(error) = commit {
        let mut rollback_failures = Vec::new();
        if let Err(rollback) = previous_config.restore(&path, 0o600) {
            rollback_failures.push(format!("configuration: {rollback}"));
        }
        if let (Some((rules_path, _)), Some(previous_rules)) = (&rendered_rules, previous_rules)
            && let Err(rollback) = previous_rules.restore(rules_path, 0o644)
        {
            rollback_failures.push(format!("udev rules: {rollback}"));
        }
        if let Some((dropin, previous, _)) = prelogin_ordering
            && let Err(rollback) = previous.restore(&dropin, 0o644)
        {
            rollback_failures.push(format!("pre-login boot ordering: {rollback}"));
        }
        if rollback_failures.is_empty() {
            bail!("setup failed and changes were rolled back: {error:#}");
        }
        bail!(
            "setup failed: {error:#}; rollback also failed: {}",
            rollback_failures.join("; ")
        );
    }
    if let Some((rules_path, _)) = &rendered_rules {
        println!("wrote capture permissions: {}", rules_path.display());
    }
    if let Some((dropin, _, wanted)) = &prelogin_ordering {
        let action = if wanted.is_some() { "wrote" } else { "removed" };
        println!("{action} pre-login boot ordering: {}", dropin.display());
    }
    println!(
        "{} private configuration: {}",
        if existed { "updated" } else { "created" },
        path.display()
    );
    println!("identity: {}", identity.fingerprint_hex());
    println!("capture devices: {}", config.input.capture_devices.len());
    println!("paired peers: {}", config.peers.len());
    println!(
        "pre-login injection: {}",
        if config.input.allow_prelogin_input {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "experimental touchpad: {}",
        if config.input.experimental_touchpad {
            "enabled"
        } else {
            "disabled"
        }
    );
    for warning in chord_warnings(&config.input.activation_chord) {
        println!("warning: activation chord {warning}");
    }
    if restart_required {
        println!("daemon restart required to apply this setup (changed: transport.listen)");
    }
    println!("run `zflow devices` to inspect capture candidates");
    Ok(())
}

#[derive(Debug, Serialize)]
struct OfflineStatus<'a> {
    daemon: &'static str,
    config: &'a std::path::Path,
    capture_devices: usize,
    paired_peers: usize,
    allow_prelogin_input: bool,
    experimental_touchpad: bool,
    control_socket: &'a std::path::Path,
}

fn status(path: PathBuf, json: bool) -> Result<()> {
    let config = Config::load(&path)?;
    if config.daemon.control_socket.exists() {
        match daemon_request(&config.daemon.control_socket, Request::Status)? {
            Response::Status(status) => {
                return print_daemon_status(&status, config.input.experimental_touchpad, json);
            }
            Response::Error { message } => bail!("daemon rejected status request: {message}"),
            response => bail!("unexpected daemon response: {response:?}"),
        }
    }
    let status = OfflineStatus {
        daemon: "offline",
        config: &path,
        capture_devices: config.input.capture_devices.len(),
        paired_peers: config.peers.len(),
        allow_prelogin_input: config.input.allow_prelogin_input,
        experimental_touchpad: config.input.experimental_touchpad,
        control_socket: &config.daemon.control_socket,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        println!("daemon: {}", status.daemon);
        println!("configuration: {}", status.config.display());
        println!("control socket: {}", status.control_socket.display());
        println!("capture devices: {}", status.capture_devices);
        println!("paired peers: {}", status.paired_peers);
        println!("pre-login input: {}", status.allow_prelogin_input);
        println!("experimental touchpad: {}", status.experimental_touchpad);
    }
    Ok(())
}

fn print_daemon_status(
    status: &DaemonStatus,
    experimental_touchpad: bool,
    json: bool,
) -> Result<()> {
    if json {
        let mut value = serde_json::to_value(status)?;
        value
            .as_object_mut()
            .expect("DaemonStatus serializes as an object")
            .insert("experimental_touchpad".into(), experimental_touchpad.into());
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("daemon: online");
        println!("identity: {}", status.identity);
        println!("ownership: {:?}", status.ownership);
        println!(
            "selected peer: {}",
            status.selected_peer.as_deref().unwrap_or("none")
        );
        if let Some(generation) = status.transport_generation {
            println!("transport generation: {generation}");
        }
        if let Some(activation) = status.activation_id {
            println!("activation: {activation}");
        }
        println!("experimental touchpad: {experimental_touchpad}");
    }
    Ok(())
}

fn doctor(path: PathBuf) -> Result<()> {
    let mut failed = false;
    let config_owner = doctor_private_path(&path, DoctorPathKind::File, None, &mut failed);
    let config = match Config::load(&path) {
        Ok(config) => {
            println!("ok  configuration: {}", path.display());
            for warning in chord_warnings(&config.input.activation_chord) {
                println!("warn activation chord: {warning}");
            }
            Some(config)
        }
        Err(error) => {
            failed = true;
            println!("fail configuration: {error}");
            None
        }
    };

    let mut identity_fingerprint = None;
    if let Some(config) = &config {
        let state_owner = doctor_private_path(
            &config.daemon.state_dir,
            DoctorPathKind::Directory,
            config_owner,
            &mut failed,
        );
        let identity_path = config.daemon.state_dir.join("identity.pk8");
        doctor_private_path(
            &identity_path,
            DoctorPathKind::File,
            state_owner,
            &mut failed,
        );
        match Identity::load(&identity_path) {
            Ok(identity) => {
                let fingerprint = identity.fingerprint_hex();
                println!("ok  identity: {fingerprint}");
                identity_fingerprint = Some(fingerprint);
            }
            Err(error) => {
                failed = true;
                println!("fail identity: {error}");
            }
        }
    }

    #[cfg(target_os = "linux")]
    if let Some(config) = &config {
        doctor_linux(config, &mut failed);
    }

    #[cfg(not(target_os = "linux"))]
    if config.is_some() {
        println!("warn Linux input checks: unavailable on this platform");
    }

    if let Some(config) = &config {
        match doctor_daemon_status(&config.daemon.control_socket) {
            Ok(status) => {
                if identity_fingerprint.as_deref() == Some(status.identity.as_str()) {
                    println!("ok  daemon: online, identity matches");
                } else {
                    failed = true;
                    println!(
                        "fail daemon: online identity {} does not match the local identity",
                        status.identity
                    );
                }
            }
            Err(error) => {
                failed = true;
                println!("fail daemon: {error}");
            }
        }
    }

    if failed {
        bail!("one or more checks failed");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum DoctorPathKind {
    File,
    Directory,
}

fn doctor_private_path(
    path: &Path,
    kind: DoctorPathKind,
    expected_owner: Option<u32>,
    failed: &mut bool,
) -> Option<u32> {
    match private_path_owner(path, kind, expected_owner) {
        Ok(owner) => {
            println!("ok  private {}: {}", kind.name(), path.display());
            Some(owner)
        }
        Err(error) => {
            *failed = true;
            println!("fail private {} {}: {error}", kind.name(), path.display());
            None
        }
    }
}

impl DoctorPathKind {
    fn name(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
        }
    }

    fn matches(self, metadata: &fs::Metadata) -> bool {
        match self {
            Self::File => metadata.file_type().is_file(),
            Self::Directory => metadata.file_type().is_dir(),
        }
    }
}

fn private_path_owner(
    path: &Path,
    kind: DoctorPathKind,
    expected_owner: Option<u32>,
) -> Result<u32> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = fs::symlink_metadata(path)?;
    if !kind.matches(&metadata) {
        bail!("expected a regular {}", kind.name());
    }
    if metadata.mode() & 0o077 != 0 {
        bail!(
            "mode {:04o} grants access to group or others",
            metadata.mode() & 0o7777
        );
    }
    if let Some(owner) = expected_owner
        && owner != metadata.uid()
    {
        bail!(
            "owner uid {} differs from expected uid {owner}",
            metadata.uid()
        );
    }
    Ok(metadata.uid())
}

fn doctor_daemon_status(socket: &Path) -> Result<Box<DaemonStatus>> {
    const TIMEOUT: Duration = Duration::from_millis(750);

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            tokio::time::timeout(TIMEOUT, async {
                let mut stream = tokio::net::UnixStream::connect(socket)
                    .await
                    .with_context(|| format!("could not connect to {}", socket.display()))?;
                write_message(&mut stream, &Request::Status).await?;
                let response = read_message(&mut stream).await?;
                Ok::<Response, anyhow::Error>(response)
            })
            .await
            .with_context(|| format!("status request exceeded {TIMEOUT:?}"))?
        })
        .and_then(|response| match response {
            Response::Status(status) => Ok(status),
            Response::Error { message } => bail!("daemon rejected status request: {message}"),
            response => bail!("unexpected daemon response: {response:?}"),
        })
}

#[cfg(target_os = "linux")]
fn doctor_linux(config: &Config, failed: &mut bool) {
    let service_group = match nix::unistd::Group::from_name("zflow") {
        Ok(Some(group)) => {
            println!("ok  service group: zflow ({})", group.gid.as_raw());
            Some(group.gid.as_raw())
        }
        Ok(None) => {
            *failed = true;
            println!("fail service group: zflow does not exist");
            None
        }
        Err(error) => {
            *failed = true;
            println!("fail service group: could not resolve zflow: {error}");
            None
        }
    };

    if let Some(group) = service_group {
        doctor_device_access(
            Path::new("/dev/uinput"),
            group,
            0o060,
            "uinput access",
            failed,
        );
    }

    match crate::runtime::probe_virtual_device_readiness() {
        Ok(readiness) if readiness.is_ready_for(config.input.experimental_touchpad) => {
            if config.input.experimental_touchpad {
                println!(
                    "ok  virtual input: keyboard, pointer, and experimental touchpad are ready on seat0"
                );
                doctor_touchpad_integration(failed);
            } else {
                println!("ok  virtual input: keyboard and pointer are ready on seat0");
            }
        }
        Ok(readiness) => {
            *failed = true;
            println!(
                "fail virtual input: keyboard_ready={}, pointer_ready={}, touchpad_ready={}, touchpad_required={}",
                readiness.keyboard,
                readiness.pointer,
                readiness.touchpad,
                config.input.experimental_touchpad
            );
        }
        Err(error) => {
            *failed = true;
            println!("fail virtual input: {error}");
        }
    }

    let selection = if config.input.capture_devices.is_empty() {
        // No selectors captures every keyboard and pointer. Receiving never
        // reads local devices, so a computer without one only warns.
        match crate::runtime::diagnose_capture_selection(&[]) {
            Ok(selection) => {
                if selection.is_complete() {
                    println!(
                        "ok  capture selection: every keyboard and pointer, {} event node(s)",
                        selection.selected_paths.len()
                    );
                } else {
                    println!(
                        "warn capture selection: no keyboard or pointer could be opened, so this computer only receives"
                    );
                }
                if selection.scan_failures > 0 {
                    println!(
                        "warn input scan: {} event node(s) could not be inspected",
                        selection.scan_failures
                    );
                }
                Some(selection)
            }
            Err(error) => {
                *failed = true;
                println!("fail capture selection: {error}");
                None
            }
        }
    } else {
        match crate::runtime::diagnose_capture_selection(&config.input.capture_devices) {
            Ok(selection)
                if selection.is_complete()
                    && selection.selected_paths.len() == selection.configured =>
            {
                println!(
                    "ok  capture selection: {} selector(s), {} event node(s)",
                    selection.configured,
                    selection.selected_paths.len()
                );
                if selection.scan_failures > 0 {
                    println!(
                        "warn input scan: {} unrelated event node(s) could not be inspected",
                        selection.scan_failures
                    );
                }
                Some(selection)
            }
            Ok(selection) => {
                *failed = true;
                println!(
                    "fail capture selection: configured={}, selected={}, unmatched={}, ambiguous={}, scan_failures={}",
                    selection.configured,
                    selection.selected_paths.len(),
                    selection.unmatched,
                    selection.ambiguous,
                    selection.scan_failures
                );
                None
            }
            Err(error) => {
                *failed = true;
                println!("fail capture selection: {error}");
                None
            }
        }
    };

    if let Some(selection) = selection {
        for path in selection.selected_paths {
            if let Some(group) = service_group {
                doctor_device_access(&path, group, 0o040, "capture access", failed);
            }
            match udev_properties(&path) {
                Ok(properties) if properties.get("ZFLOW_CAPTURE") == Some("1") => {
                    println!("ok  capture udev property: {}", path.display());
                }
                Ok(_) => {
                    *failed = true;
                    println!(
                        "fail capture udev property {}: ZFLOW_CAPTURE=1 is missing",
                        path.display()
                    );
                }
                Err(error) => {
                    *failed = true;
                    println!("fail capture udev property {}: {error}", path.display());
                }
            }
        }
    }

    match crate::linux::query_primary_seat() {
        crate::linux::SeatState::Unlocked(session) => println!(
            "ok  primary seat: unlocked session {} (uid {}, {:?})",
            session.id, session.uid, session.kind
        ),
        crate::linux::SeatState::Restricted(state) => {
            println!("warn primary seat: restricted ({state:?})")
        }
        crate::linux::SeatState::Unknown { reason } => {
            *failed = true;
            println!("fail primary seat: {reason}");
        }
    }

    if config.input.allow_prelogin_input {
        doctor_prelogin_ordering(failed);
    }

    if config
        .peers
        .values()
        .any(|peer| !peer.keyboard.is_standard())
        && let Ok(devices) = fs::read_to_string("/proc/bus/input/devices")
    {
        for remapper in keyboard_remappers(&devices) {
            println!(
                "warn keyboard remapper: {remapper} may also remap zflow's keyboard; scope it away from \"{}\" (vendor {:04x}, product {:04x})",
                crate::linux::ZFLOW_KEYBOARD_NAME,
                crate::linux::ZFLOW_VENDOR_ID,
                crate::linux::ZFLOW_KEYBOARD_PRODUCT_ID
            );
        }
    }
}

/// Remappers that grab every keyboard, by the name of the virtual keyboard
/// they add. xremap may append its pid.
#[cfg(target_os = "linux")]
const KEYBOARD_REMAPPERS: &[(&str, &str)] = &[
    ("XWayKeyz (virtual) Keyboard", "Toshy"),
    ("keyd virtual keyboard", "keyd"),
    ("xremap", "xremap"),
    ("kanata", "kanata"),
];

/// The remappers whose keyboard is listed in /proc/bus/input/devices.
#[cfg(target_os = "linux")]
fn keyboard_remappers(devices: &str) -> Vec<&'static str> {
    let names = devices
        .lines()
        .filter_map(|line| line.strip_prefix("N: Name=\"")?.strip_suffix('"'))
        .collect::<Vec<_>>();
    KEYBOARD_REMAPPERS
        .iter()
        .filter(|(device, _)| names.iter().any(|name| name.starts_with(device)))
        .map(|&(_, remapper)| remapper)
        .collect()
}

/// The udev property the packaged rule sets on the virtual touchpad. Without
/// it libinput treats the touchpad as built in and adds palm zones and
/// disable-while-typing.
#[cfg(target_os = "linux")]
const TOUCHPAD_INTEGRATION: (&str, &str) = ("ID_INPUT_TOUCHPAD_INTEGRATION", "external");

#[cfg(target_os = "linux")]
fn doctor_touchpad_integration(failed: &mut bool) {
    let (property, value) = TOUCHPAD_INTEGRATION;
    let properties = crate::linux::enumerate_devices()
        .context("could not enumerate input devices")
        .and_then(|scan| {
            scan.devices
                .into_iter()
                .find(|device| {
                    device.zflow_role() == Some(crate::linux::VirtualDeviceRole::Touchpad)
                })
                .context("could not open the zflow remote touchpad")
        })
        .and_then(|touchpad| udev_properties(&touchpad.path));
    match properties {
        Ok(properties) if properties.get(property) == Some(value) => {
            println!("ok  touchpad integration: {value}");
        }
        Ok(_) => {
            *failed = true;
            println!("fail touchpad integration: {property}={value} is missing");
        }
        Err(error) => {
            *failed = true;
            println!("fail touchpad integration: {error}");
        }
    }
}

#[cfg(target_os = "linux")]
fn doctor_prelogin_ordering(failed: &mut bool) {
    // display-manager.service is an alias, and systemctl lists dependencies
    // under the name the unit loaded as, such as gdm.service.
    let ordered = systemctl_show("display-manager.service", "Id").and_then(|manager| {
        let before = systemctl_show("zflowd.service", "Before")?;
        Ok(before
            .split_ascii_whitespace()
            .any(|unit| unit == manager.trim()))
    });
    match ordered {
        Ok(true) => println!("ok  pre-login boot ordering: before display-manager.service"),
        Ok(false) => {
            *failed = true;
            println!(
                "fail pre-login boot ordering: zflowd.service is not ordered before display-manager.service"
            );
        }
        Err(error) => {
            *failed = true;
            println!("fail pre-login boot ordering: {error}");
        }
    }
}

#[cfg(target_os = "linux")]
fn systemctl_show(unit: &str, property: &str) -> Result<String> {
    let mut command = ProcessCommand::new("/usr/bin/systemctl");
    command
        .arg("show")
        .arg(format!("--property={property}"))
        .arg("--value")
        .arg(unit)
        .env_clear()
        .env("LANG", "C")
        .env("LC_ALL", "C");
    let output = bounded_command_output(&mut command, Duration::from_millis(750), 64 * 1024)?;
    if !output.status.success() {
        bail!(
            "systemctl exited with {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(target_os = "linux")]
fn systemd_daemon_reload() -> Result<()> {
    let mut command = ProcessCommand::new("/usr/bin/systemctl");
    command
        .arg("daemon-reload")
        .env_clear()
        .env("LANG", "C")
        .env("LC_ALL", "C");
    let output = bounded_command_output(&mut command, Duration::from_secs(30), 64 * 1024)
        .context("could not run systemctl daemon-reload")?;
    if !output.status.success() {
        bail!(
            "systemctl daemon-reload exited with {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn systemd_daemon_reload() -> Result<()> {
    bail!("pre-login boot ordering is managed only on Linux")
}

#[cfg(target_os = "linux")]
fn doctor_device_access(
    path: &Path,
    group: u32,
    required_group_mode: u32,
    label: &str,
    failed: &mut bool,
) {
    match fs::symlink_metadata(path)
        .with_context(|| format!("could not inspect {}", path.display()))
        .and_then(|metadata| {
            if !metadata.file_type().is_char_device() {
                bail!("expected a character device");
            }
            require_group_access(&metadata, group, required_group_mode)?;
            Ok(metadata.mode() & 0o7777)
        }) {
        Ok(mode) => println!("ok  {label}: {} mode {mode:04o}", path.display()),
        Err(error) => {
            *failed = true;
            println!("fail {label} {}: {error}", path.display());
        }
    }
}

#[cfg(target_os = "linux")]
fn require_group_access(
    metadata: &fs::Metadata,
    group: u32,
    required_group_mode: u32,
) -> Result<()> {
    let mode = metadata.mode() & 0o7777;
    if metadata.gid() != group {
        bail!(
            "group gid {} does not match zflow gid {group}",
            metadata.gid()
        );
    }
    if mode & required_group_mode != required_group_mode {
        bail!("mode {mode:04o} lacks required zflow group access {required_group_mode:04o}");
    }
    if mode & 0o007 != 0 {
        bail!("mode {mode:04o} grants access to other users");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn udev_properties(path: &Path) -> Result<crate::runtime::UdevProperties> {
    const TIMEOUT: Duration = Duration::from_millis(750);
    const MAX_OUTPUT: usize = 64 * 1024;

    let mut command = ProcessCommand::new("/usr/bin/udevadm");
    command
        .arg("info")
        .arg("--query=property")
        .arg("--name")
        .arg(path)
        .env_clear()
        .env("LANG", "C")
        .env("LC_ALL", "C");
    let output = bounded_command_output(&mut command, TIMEOUT, MAX_OUTPUT)?;
    if !output.status.success() {
        bail!(
            "udevadm exited with {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8(output.stdout).context("udevadm returned non-UTF-8 output")?;
    Ok(crate::runtime::UdevProperties::parse(&stdout))
}

#[cfg(target_os = "linux")]
fn bounded_command_output(
    command: &mut ProcessCommand,
    timeout: Duration,
    maximum_output: usize,
) -> Result<std::process::Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let started = Instant::now();
    loop {
        match child.try_wait()? {
            Some(_) => break,
            None if started.elapsed() < timeout => thread::sleep(Duration::from_millis(2)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("command exceeded {timeout:?}");
            }
        }
    }
    let output = child.wait_with_output()?;
    if output.stdout.len() > maximum_output || output.stderr.len() > maximum_output {
        bail!("command returned more than {maximum_output} bytes");
    }
    Ok(output)
}

fn chord_warnings(chord: &[String]) -> Vec<&'static str> {
    let keys = chord
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let control = keys.contains("KEY_LEFTCTRL") || keys.contains("KEY_RIGHTCTRL");
    let alt = keys.contains("KEY_LEFTALT") || keys.contains("KEY_RIGHTALT");
    let function_key = (1..=12).any(|number| keys.contains(format!("KEY_F{number}").as_str()));
    if control && alt && function_key {
        vec!["may conflict with Linux virtual-console switching (Ctrl+Alt+Fn)"]
    } else {
        Vec::new()
    }
}

fn devices(path: PathBuf) -> Result<()> {
    let configured = Config::load(&path)
        .ok()
        .map(|config| config.input.capture_devices)
        .unwrap_or_default();

    #[cfg(target_os = "linux")]
    {
        let scan =
            crate::linux::enumerate_devices().context("could not enumerate input devices")?;
        for failure in &scan.failures {
            eprintln!("! {}: {}", failure.path.display(), failure.error);
        }
        if scan.devices.is_empty() && !scan.failures.is_empty() {
            bail!("could not open any input devices; run `zflow devices` as root");
        }

        let mut count = 0;
        for info in scan.physical_devices() {
            count += 1;
            // An empty list captures every keyboard and pointer.
            let selected = if configured.is_empty() {
                info.class.is_some()
            } else {
                configured
                    .iter()
                    .any(|selector| crate::runtime::capture_selector_matches(selector, info))
            };
            let name = info.name.as_deref().unwrap_or("unnamed");
            let phys = info.physical_path.as_deref().unwrap_or("-");
            println!(
                "{} {}\n    name: {name}\n    phys: {phys}",
                if selected { "*" } else { " " },
                info.path.display()
            );
        }
        println!("{count} input device(s)");
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = configured;
        bail!("physical device enumeration is currently available on Linux only")
    }
}

fn daemon_request(socket: &std::path::Path, request: Request) -> Result<Response> {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()?
        .block_on(async {
            let mut stream = tokio::net::UnixStream::connect(socket)
                .await
                .map_err(|error| match error.kind() {
                    // Only root and the service may reach it.
                    std::io::ErrorKind::PermissionDenied => {
                        anyhow::anyhow!("{error}; run it with sudo")
                    }
                    _ => error.into(),
                })
                .with_context(|| format!("could not connect to {}", socket.display()))?;
            write_message(&mut stream, &request).await?;
            Ok(read_message(&mut stream).await?)
        })
}

fn reload_running_daemon(config: &Config) -> Result<()> {
    let control_socket = &config.daemon.control_socket;
    if !control_socket.exists() {
        return Ok(());
    }
    match daemon_request(control_socket, Request::ReloadConfig)? {
        Response::Ack => {
            println!("reloaded running daemon");
            Ok(())
        }
        Response::Error { message } => bail!("daemon rejected configuration reload: {message}"),
        response => bail!("unexpected daemon response: {response:?}"),
    }
}

/// Run as root, setup would leave a root-owned 0600 key that the
/// zflow service account cannot read, so zflowd would fail to start. Anything
/// created here is handed to that account instead.
fn load_or_create_identity(state_dir: &Path) -> Result<Identity> {
    #[cfg(target_os = "linux")]
    {
        let key = state_dir.join("identity.pk8");
        let created = [state_dir, key.as_path()]
            .into_iter()
            .filter(|path| fs::symlink_metadata(path).is_err())
            .collect::<Vec<_>>();
        let owner = if nix::unistd::geteuid().is_root() && !created.is_empty() {
            let user = nix::unistd::User::from_name("zflow")?.context(
                "the zflow service account does not exist; install zflow before running setup as root",
            )?;
            Some((user.uid.as_raw(), user.gid.as_raw()))
        } else {
            None
        };
        let identity = Identity::load_or_create(state_dir)?;
        if let Some((uid, gid)) = owner {
            for path in created {
                std::os::unix::fs::chown(path, Some(uid), Some(gid)).with_context(|| {
                    format!(
                        "could not give {} to the zflow service account",
                        path.display()
                    )
                })?;
            }
        }
        Ok(identity)
    }
    #[cfg(not(target_os = "linux"))]
    Ok(Identity::load_or_create(state_dir)?)
}

#[cfg(target_os = "linux")]
fn resolve_device_selector(path: PathBuf) -> Result<crate::config::DeviceSelector> {
    let device = evdev::Device::open(&path)
        .with_context(|| format!("could not open capture device {}", path.display()))?;
    let info = crate::linux::DeviceInfo::from_device(path.clone(), &device);
    if info.is_zflow_virtual() {
        bail!(
            "refusing to capture zflow virtual device {}",
            path.display()
        );
    }
    Ok(crate::config::DeviceSelector {
        path,
        name: info.name,
        phys: info.physical_path,
        uniq: info.unique_name.filter(|uniq| !uniq.is_empty()),
        vendor: Some(info.vendor),
        product: Some(info.product),
    })
}

#[cfg(not(target_os = "linux"))]
fn resolve_device_selector(path: PathBuf) -> Result<crate::config::DeviceSelector> {
    Ok(crate::config::DeviceSelector {
        path,
        name: None,
        phys: None,
        uniq: None,
        vendor: None,
        product: None,
    })
}

#[derive(Debug)]
struct FileSnapshot {
    contents: Option<Vec<u8>>,
}

impl FileSnapshot {
    fn capture(path: &Path) -> Result<Self> {
        match fs::symlink_metadata(path) {
            Ok(metadata)
                if metadata.file_type().is_file() && !metadata.file_type().is_symlink() =>
            {
                Ok(Self {
                    contents: Some(fs::read(path)?),
                })
            }
            Ok(_) => bail!("refusing to snapshot non-regular file {}", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(Self { contents: None })
            }
            Err(error) => Err(error.into()),
        }
    }

    fn restore(self, path: &Path, create_mode: u32) -> Result<()> {
        if let Some(contents) = self.contents {
            return write_atomic_bytes(path, &contents, create_mode);
        }
        match fs::symlink_metadata(path) {
            Ok(metadata)
                if metadata.file_type().is_file() && !metadata.file_type().is_symlink() =>
            {
                fs::remove_file(path)?;
                Ok(())
            }
            Ok(_) => bail!("refusing to remove non-regular file {}", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

fn render_capture_rules(devices: &[crate::config::DeviceSelector]) -> Result<String> {
    if devices.is_empty() {
        bail!("at least one --device is required when writing udev rules");
    }

    let mut text = String::from(
        "# Generated by zflow setup. Local edits may be replaced.\n\
         # Narrows capture to the configured physical devices.\n",
    );
    for device in devices {
        let vendor = device
            .vendor
            .context("selected device has no vendor identifier")?;
        let product = device
            .product
            .context("selected device has no product identifier")?;
        let mut matches = format!(
            "ACTION==\"add|change\", SUBSYSTEM==\"input\", KERNEL==\"event*\", ENV{{ZFLOW_CAPTURE_EXCLUDE}}!=\"1\", ATTRS{{id/vendor}}==\"{vendor:04x}\", ATTRS{{id/product}}==\"{product:04x}\""
        );
        let physical_path = device
            .phys
            .as_deref()
            .filter(|physical_path| !physical_path.is_empty())
            .context(
                "selected device has no stable physical path; refusing to grant every identical device",
            )?;
        matches.push_str(&format!(
            ", ATTRS{{phys}}==\"{}\"",
            escape_udev_value(physical_path)?
        ));
        if let Some(uniq) = &device.uniq {
            matches.push_str(&format!(
                ", ATTRS{{uniq}}==\"{}\"",
                escape_udev_value(uniq)?
            ));
        }
        matches.push_str(", ENV{ZFLOW_CAPTURE}=\"1\", GROUP=\"zflow\", MODE=\"0640\"\n");
        text.push_str(&matches);
    }
    Ok(text)
}

fn write_atomic_bytes(path: &Path, contents: &[u8], create_mode: u32) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let parent = path.parent().context("udev rules path has no parent")?;
    fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("udev rules path has no file name")?;
    let temporary = parent.join(format!(".{name}.tmp-{}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(create_mode);
    let existing = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            Some((metadata.mode(), metadata.uid(), metadata.gid()))
        }
        Ok(_) => bail!("refusing to replace non-regular file {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let mut file = options.open(&temporary)?;
    let write_result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        if let Some((mode, uid, gid)) = existing {
            use std::os::unix::fs::{PermissionsExt, chown};

            fs::set_permissions(&temporary, fs::Permissions::from_mode(mode & 0o7777))?;
            chown(&temporary, Some(uid), Some(gid))?;
        }
        fs::rename(&temporary, path)?;
        Ok::<_, std::io::Error>(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

/// udev(7) match values are glob patterns with no escape for `* ? [ ] |`, and
/// only `\"` is unescaped inside quotes. A value containing one of those or a
/// backslash would match other devices or none, so it is refused.
fn escape_udev_value(value: &str) -> Result<String> {
    if value
        .chars()
        .any(|c| c.is_control() || matches!(c, '*' | '?' | '[' | ']' | '|' | '\\'))
    {
        bail!(
            "udev cannot match {value:?} literally; it contains a control character, a backslash, or one of * ? [ ] |"
        );
    }
    Ok(value.replace('"', "\\\""))
}

fn peers(path: PathBuf) -> Result<()> {
    let config = Config::load(&path)?;
    if config.daemon.control_socket.exists() {
        match daemon_request(&config.daemon.control_socket, Request::ListPeers)? {
            Response::Peers { peers } => {
                for (name, permissions) in peers {
                    println!(
                        "{name}: connect={} send={} receive={} prelogin={}",
                        permissions.connect,
                        permissions.send_normal,
                        permissions.receive_normal,
                        permissions.inject_prelogin
                    );
                }
                return Ok(());
            }
            Response::Error { message } => bail!("daemon rejected peers request: {message}"),
            response => bail!("unexpected daemon response: {response:?}"),
        }
    }
    for (name, peer) in config.peers {
        println!(
            "{name}: connect={} send={} receive={} prelogin={}",
            peer.permissions.connect,
            peer.permissions.send_normal,
            peer.permissions.receive_normal,
            peer.permissions.inject_prelogin
        );
    }
    Ok(())
}

fn nearby(path: PathBuf, add: Option<String>) -> Result<()> {
    let config = Config::load(&path)?;
    let add = add
        .as_deref()
        .map(crate::peer_view::parse_address)
        .transpose()?;
    let request = Request::Nearby { add };
    let (computers, window) = match daemon_request(&config.daemon.control_socket, request)? {
        Response::Nearby {
            computers,
            pairing_window,
        } => (computers, pairing_window),
        Response::Error { message } => bail!("daemon rejected nearby request: {message}"),
        response => bail!("unexpected daemon response: {response:?}"),
    };
    if let Some(address) = add {
        println!("said hello to {address}; it is listed once it answers");
    }
    for computer in &computers {
        println!("{}", nearby_line(computer));
    }
    if computers.is_empty() {
        println!("no other zflow computers found");
    }
    if let (crate::pairing_window::State::Open, Some(seconds)) = (window.state, window.seconds_left)
    {
        println!(
            "pairing window: open for {} more minutes",
            seconds.div_ceil(60)
        );
    }
    Ok(())
}

/// One found computer: name, mark, system, version and what can be done.
fn nearby_line(computer: &crate::neighbors::Unplaced) -> String {
    use crate::neighbors::UnplacedState;
    let state = match computer.state {
        UnplacedState::Identifying => "not answered yet",
        UnplacedState::Ready => "ready",
        UnplacedState::DifferentVersion => "different zflow version",
        UnplacedState::DuplicateName => "ready; shares its name, so trust it by mark",
    };
    let os = match computer.os {
        Some(crate::wire::Os::Linux) => "Linux",
        Some(crate::wire::Os::Macos) => "macOS",
        Some(crate::wire::Os::Windows) => "Windows",
        None => "-",
    };
    format!(
        "{}  mark {}  {os}  {}  {state}",
        computer.name,
        computer.mark.as_deref().unwrap_or("-"),
        computer.version.as_deref().unwrap_or("-"),
    )
}

fn trust(path: PathBuf, computer: String) -> Result<()> {
    let config = Config::load(&path)?;
    match daemon_request(&config.daemon.control_socket, Request::Trust { computer })? {
        Response::Trusted { name, mark } => {
            println!("trusted {name}, mark {mark}; arrange it in zflow settings");
            Ok(())
        }
        Response::Error { message } => bail!("daemon rejected trust request: {message}"),
        response => bail!("unexpected daemon response: {response:?}"),
    }
}

fn revoke_peer(path: PathBuf, peer: String) -> Result<()> {
    let mut config = Config::load(&path)?;
    if config.daemon.control_socket.exists() {
        return daemon_command(path, Request::RevokePeer { peer });
    }
    if config.peers.remove(&peer).is_none() {
        bail!("unknown peer {peer}");
    }
    config.save(&path)?;
    println!("revoked {peer}");
    Ok(())
}

fn allow_prelogin(path: PathBuf, peer: String, allowed: bool) -> Result<()> {
    let mut config = Config::load(&path)?;
    let Some(record) = config.peers.get_mut(&peer) else {
        bail!("unknown peer {peer}");
    };
    record.permissions.inject_prelogin = allowed;
    if config.daemon.control_socket.exists() {
        return daemon_command(
            path,
            Request::SetPeerPermissions {
                peer,
                permissions: record.permissions,
            },
        );
    }
    config.save(&path)?;
    println!(
        "pre-login permission for {peer}: {}",
        if allowed { "on" } else { "off" }
    );
    Ok(())
}

fn set_peer_keyboard(path: PathBuf, peer: String, keyboard: KeyboardMode) -> Result<()> {
    let mut config = Config::load(&path)?;
    let Some(record) = config.peers.get_mut(&peer) else {
        bail!("unknown peer {peer}");
    };
    record.keyboard = keyboard;
    if config.daemon.control_socket.exists() {
        return daemon_command(path, Request::SetPeerKeyboard { peer, keyboard });
    }
    config.save(&path)?;
    let name = match keyboard {
        KeyboardMode::Standard => "standard",
        KeyboardMode::PcPositions => "pc-positions",
        KeyboardMode::Mac => "mac",
    };
    println!("keyboard for {peer}: {name}");
    Ok(())
}

fn daemon_command(path: PathBuf, request: Request) -> Result<()> {
    let config = Config::load(&path)?;
    match daemon_request(&config.daemon.control_socket, request)? {
        Response::Ack => Ok(()),
        Response::Error { message } => bail!("daemon rejected request: {message}"),
        response => bail!("unexpected daemon response: {response:?}"),
    }
}

fn playout(
    path: PathBuf,
    mode: crate::config::PlayoutMode,
    fixed_delay_ms: Option<u64>,
    minimum_delay_ms: Option<u64>,
    maximum_delay_ms: Option<u64>,
    percentile: Option<f64>,
) -> Result<()> {
    let mut config = Config::load(&path)?;
    config.playout.mode = mode;
    if let Some(value) = fixed_delay_ms {
        config.playout.fixed_delay_ms = value;
    }
    if let Some(value) = minimum_delay_ms {
        config.playout.minimum_delay_ms = value;
    }
    if let Some(value) = maximum_delay_ms {
        config.playout.maximum_delay_ms = value;
    }
    if let Some(value) = percentile {
        config.playout.percentile = value;
    }
    config.validate()?;
    crate::session::SessionOptions::from_config(&config)?;
    let previous = FileSnapshot::capture(&path)?;
    let commit = (|| {
        config.save(&path)?;
        reload_running_daemon(&config)
    })();
    if let Err(error) = commit {
        if let Err(rollback) = previous.restore(&path, 0o600) {
            bail!("playout update failed: {error}; rollback also failed: {rollback}");
        }
        bail!("playout update failed and was rolled back: {error}");
    }
    println!("updated playout configuration");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PeerConfig, PeerPermissions};

    #[test]
    fn command_line_definition_is_consistent() {
        use clap::CommandFactory as _;
        Cli::command().debug_assert();
    }

    #[test]
    fn setup_cannot_move_paths_the_sandboxed_daemon_depends_on() {
        for flag in ["--state-dir", "--control-socket"] {
            assert!(Cli::try_parse_from(["zflow", "setup", flag, "/tmp/zflow"]).is_err());
        }
    }

    #[test]
    fn private_path_check_rejects_shared_modes_and_owner_changes() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private.toml");
        fs::write(&path, "version = 1\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let owner = fs::symlink_metadata(&path).unwrap().uid();

        assert_eq!(
            private_path_owner(&path, DoctorPathKind::File, Some(owner)).unwrap(),
            owner
        );
        assert!(
            private_path_owner(&path, DoctorPathKind::File, Some(owner.wrapping_add(1))).is_err()
        );

        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(private_path_owner(&path, DoctorPathKind::File, None).is_err());
        assert!(private_path_owner(&path, DoctorPathKind::Directory, None).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn group_access_requires_the_exact_group_bits_and_no_world_access() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("event0");
        fs::write(&path, []).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let group = metadata.gid();
        require_group_access(&metadata, group, 0o040).unwrap();
        assert!(require_group_access(&metadata, group.wrapping_add(1), 0o040).is_err());

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(require_group_access(&fs::metadata(&path).unwrap(), group, 0o040).is_err());

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(require_group_access(&fs::metadata(&path).unwrap(), group, 0o040).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn doctor_finds_keyboard_remappers_in_the_input_device_list() {
        let devices = "\
I: Bus=0003 Vendor=05ac Product=024f Version=0111
N: Name=\"Apple Internal Keyboard\"
P: Phys=usb-0000:00:14.0-1/input0

I: Bus=0006 Vendor=1209 Product=5a01 Version=0001
N: Name=\"zflow remote keyboard\"

I: Bus=0003 Vendor=1234 Product=5678 Version=0001
N: Name=\"xremap pid=4242\"

I: Bus=0010 Vendor=0fac Product=0ade Version=0001
N: Name=\"keyd virtual keyboard\"
";
        assert_eq!(keyboard_remappers(devices), ["keyd", "xremap"]);
        assert_eq!(
            keyboard_remappers("N: Name=\"XWayKeyz (virtual) Keyboard\"\n"),
            ["Toshy"]
        );
        assert!(keyboard_remappers("N: Name=\"zflow remote keyboard\"\n").is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn doctor_expects_the_touchpad_integration_the_packaged_rule_sets() {
        let (property, value) = TOUCHPAD_INTEGRATION;
        let rules = include_str!("../packaging/udev/70-zflow.rules");
        assert!(rules.lines().any(|rule| {
            rule.contains("ENV{ZFLOW_DEVICE_ROLE}=\"remote-touchpad\"")
                && rule.contains(&format!("ENV{{{property}}}=\"{value}\""))
        }));
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires systemd, a display manager, and the pre-login drop-in installed"]
    fn live_prelogin_ordering_sees_through_the_display_manager_alias() {
        let mut failed = false;
        doctor_prelogin_ordering(&mut failed);
        assert!(!failed);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bounded_command_caps_helper_output() {
        let mut command = ProcessCommand::new("/usr/bin/printf");
        command.arg("12345");
        assert!(bounded_command_output(&mut command, Duration::from_secs(1), 4).is_err());
    }

    #[test]
    fn udev_values_escape_quotes_and_refuse_what_udev_cannot_match_literally() {
        assert_eq!(
            escape_udev_value("board \"left\"").unwrap(),
            "board \\\"left\\\""
        );
        for value in [
            "line\nbreak",
            "back\\slash",
            "usb-*",
            "usb-?/input0",
            "usb-[01]",
            "usb-]",
            "a|b",
        ] {
            assert!(escape_udev_value(value).is_err(), "{value:?}");
        }
    }

    #[test]
    fn capture_rule_is_narrow_and_excludes_virtual_devices() {
        let rule = render_capture_rules(&[crate::config::DeviceSelector {
            path: "/dev/input/event7".into(),
            name: Some("Example Keyboard".into()),
            phys: Some("usb-1/input0".into()),
            uniq: Some("aa:bb:cc:dd:ee:ff".into()),
            vendor: Some(0x1234),
            product: Some(0xabcd),
        }])
        .unwrap();
        assert!(rule.contains("ATTRS{id/vendor}==\"1234\""));
        assert!(rule.contains("ATTRS{id/product}==\"abcd\""));
        assert!(rule.contains("ENV{ZFLOW_CAPTURE_EXCLUDE}!=\"1\""));
        assert!(rule.contains("ATTRS{phys}==\"usb-1/input0\""));
        assert!(rule.contains("ATTRS{uniq}==\"aa:bb:cc:dd:ee:ff\""));
    }

    #[test]
    fn capture_rule_refuses_a_device_without_a_stable_physical_path() {
        // An empty phys would render ATTRS{phys}=="" and grant every device
        // with the same vendor and product.
        for phys in [None, Some(String::new())] {
            let error = render_capture_rules(&[crate::config::DeviceSelector {
                path: "/dev/input/event7".into(),
                name: Some("Indistinguishable Keyboard".into()),
                phys,
                uniq: None,
                vendor: Some(0x1234),
                product: Some(0xabcd),
            }])
            .unwrap_err();

            assert!(error.to_string().contains("no stable physical path"));
        }
    }

    #[test]
    fn setup_can_revoke_global_prelogin_permission() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let mut config = Config::default();
        config.daemon.state_dir = directory.path().join("state");
        config.daemon.control_socket = directory.path().join("missing.sock");
        config.input.allow_prelogin_input = true;
        config.save(&path).unwrap();

        setup(
            path.clone(),
            SetupOptions {
                allow_prelogin: Some(false),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        assert!(!Config::load(&path).unwrap().input.allow_prelogin_input);
    }

    #[test]
    fn setup_hands_the_experimental_touchpad_to_a_running_daemon() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let socket = directory.path().join("zflowd.sock");
        let mut config = Config::default();
        config.daemon.state_dir = directory.path().join("state");
        config.daemon.control_socket = socket.clone();
        config.save(&path).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let daemon = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let listener = tokio::net::UnixListener::from_std(listener).unwrap();
                    let (mut stream, _) =
                        tokio::time::timeout(Duration::from_secs(10), listener.accept())
                            .await
                            .expect("setup never asked the daemon to reload")
                            .unwrap();
                    let request: Request = read_message(&mut stream).await.unwrap();
                    write_message(&mut stream, &Response::Ack).await.unwrap();
                    request
                })
        });

        setup(
            path.clone(),
            SetupOptions {
                experimental_touchpad: Some(true),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        assert_eq!(daemon.join().unwrap(), Request::ReloadConfig);
        assert!(Config::load(&path).unwrap().input.experimental_touchpad);
    }

    #[test]
    fn peer_keyboard_saves_offline_and_asks_a_running_daemon() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let socket = directory.path().join("zflowd.sock");
        let mut config = Config::default();
        config.daemon.state_dir = directory.path().join("state");
        config.daemon.control_socket = socket.clone();
        let record =
            PeerConfig::from_spki(b"peer public key", Vec::new(), PeerPermissions::default())
                .unwrap();
        config.peers.insert("desk".into(), record);
        config.save(&path).unwrap();
        let keyboard = |peer: &str, mode: &str| {
            let config = path.to_str().unwrap();
            run(
                Cli::try_parse_from(["zflow", "--config", config, "peer", "keyboard", peer, mode])
                    .unwrap(),
            )
        };

        assert!(keyboard("laptop", "mac").is_err());
        keyboard("desk", "pc-positions").unwrap();
        let saved = Config::load(&path).unwrap();
        assert_eq!(saved.peers["desk"].keyboard, KeyboardMode::PcPositions);
        assert!(
            Cli::try_parse_from(["zflow", "peer", "keyboard", "desk", "pc_positions"]).is_err()
        );

        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let daemon = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let listener = tokio::net::UnixListener::from_std(listener).unwrap();
                    let (mut stream, _) =
                        tokio::time::timeout(Duration::from_secs(10), listener.accept())
                            .await
                            .expect("the CLI never asked the daemon")
                            .unwrap();
                    let request: Request = read_message(&mut stream).await.unwrap();
                    write_message(&mut stream, &Response::Ack).await.unwrap();
                    request
                })
        });

        keyboard("desk", "mac").unwrap();
        assert_eq!(
            daemon.join().unwrap(),
            Request::SetPeerKeyboard {
                peer: "desk".into(),
                keyboard: KeyboardMode::Mac,
            }
        );
        // The daemon saves what it applies.
        assert_eq!(Config::load(&path).unwrap(), saved);
    }

    #[test]
    fn pending_restart_makes_a_later_live_setup_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let stale_socket = directory.path().join("stale.sock");
        let new_listen = "127.0.0.1:43219".parse().unwrap();
        let mut config = Config::default();
        config.daemon.state_dir = directory.path().join("state");
        config.daemon.control_socket = stale_socket.clone();
        config.input.allow_prelogin_input = true;
        config.save(&path).unwrap();
        fs::write(stale_socket, []).unwrap();

        setup(
            path.clone(),
            SetupOptions {
                listen: Some(new_listen),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let pending = fs::read(&path).unwrap();

        let result = setup(
            path.clone(),
            SetupOptions {
                allow_prelogin: Some(false),
                ..Default::default()
            },
            None,
        );

        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), pending);
        let saved = Config::load(&path).unwrap();
        assert_eq!(saved.transport.listen, new_listen);
        assert!(saved.input.allow_prelogin_input);
    }

    #[test]
    fn setup_live_reload_failure_restores_the_original_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let stale_socket = directory.path().join("stale.sock");
        let mut config = Config::default();
        config.daemon.state_dir = directory.path().join("state");
        config.daemon.control_socket = stale_socket.clone();
        config.input.allow_prelogin_input = true;
        config.save(&path).unwrap();
        fs::write(stale_socket, []).unwrap();
        let before = fs::read(&path).unwrap();

        let result = setup(
            path.clone(),
            SetupOptions {
                allow_prelogin: Some(false),
                ..Default::default()
            },
            None,
        );

        assert!(result.is_err());
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn setup_rejects_an_unknown_chord_before_writing_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let mut config = Config::default();
        config.daemon.state_dir = directory.path().join("state");
        config.daemon.control_socket = directory.path().join("missing.sock");
        config.save(&path).unwrap();
        let before = fs::read(&path).unwrap();

        let result = setup(
            path.clone(),
            SetupOptions {
                activation_chord: vec!["KEY_THIS_DOES_NOT_EXIST".into()],
                ..Default::default()
            },
            None,
        );

        assert!(result.is_err());
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn setup_rejects_broad_capture_rules_before_writing_either_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let rules_path = directory.path().join("71-zflow-capture.rules");
        let mut config = Config::default();
        config.daemon.state_dir = directory.path().join("state");
        config.daemon.control_socket = directory.path().join("missing.sock");
        config.input.capture_devices = vec![crate::config::DeviceSelector {
            path: "/dev/input/event7".into(),
            name: Some("Indistinguishable Keyboard".into()),
            phys: None,
            uniq: None,
            vendor: Some(0x1234),
            product: Some(0xabcd),
        }];
        config.save(&path).unwrap();
        fs::write(&rules_path, "# keep me\n").unwrap();
        let config_before = fs::read(&path).unwrap();
        let rules_before = fs::read(&rules_path).unwrap();

        let result = setup(
            path.clone(),
            SetupOptions {
                udev_rules: Some(rules_path.clone()),
                ..Default::default()
            },
            None,
        );

        assert!(result.is_err());
        assert_eq!(fs::read(path).unwrap(), config_before);
        assert_eq!(fs::read(rules_path).unwrap(), rules_before);
    }

    #[test]
    fn playout_reload_failure_restores_the_original_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let stale_socket = directory.path().join("stale.sock");
        let mut config = Config::default();
        config.daemon.state_dir = directory.path().join("state");
        config.daemon.control_socket = stale_socket.clone();
        config.save(&path).unwrap();
        fs::write(stale_socket, []).unwrap();
        let before = fs::read(&path).unwrap();

        let result = playout(
            path.clone(),
            crate::config::PlayoutMode::Fixed,
            Some(17),
            None,
            None,
            None,
        );

        assert!(result.is_err());
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn warns_about_linux_virtual_console_chords() {
        assert!(chord_warnings(&Config::default().input.activation_chord).is_empty());
        assert_eq!(
            chord_warnings(&[
                "KEY_LEFTCTRL".into(),
                "KEY_LEFTALT".into(),
                "KEY_F12".into(),
            ])
            .len(),
            1
        );
        assert!(chord_warnings(&["KEY_SCROLLLOCK".into()]).is_empty());
    }
}
