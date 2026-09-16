use std::{
    fs::{self, OpenOptions},
    io::{BufRead, IsTerminal, Write},
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
use serde::Serialize;

use crate::{
    config::{Config, PeerConfig, PeerPermissions},
    control::{DaemonStatus, Request, Response, read_message, write_message},
    identity::Identity,
};

#[derive(Debug)]
pub enum Command {
    Settings,
    DesktopAgent {
        install: bool,
    },
    Setup {
        state_dir: Option<PathBuf>,
        control_socket: Option<PathBuf>,
        listen: Option<std::net::SocketAddr>,
        devices: Vec<PathBuf>,
        activation_chord: Vec<String>,
        escape_chord: Vec<String>,
        udev_rules: Option<PathBuf>,
        allow_prelogin: Option<bool>,
        experimental_touchpad: Option<bool>,
    },
    Status {
        json: bool,
    },
    Doctor,
    Devices,
    Peers,
    PairConnect {
        peer: String,
        address: SocketAddr,
        advertised: Vec<SocketAddr>,
        code: Option<String>,
        timeout: Duration,
    },
    PairListen {
        peer: String,
        listen: SocketAddr,
        advertised: Vec<SocketAddr>,
        code: Option<String>,
        timeout: Duration,
    },
    RevokePeer {
        peer: String,
    },
    AllowPrelogin {
        peer: String,
        allowed: bool,
    },
    Switch {
        peer: String,
    },
    Local,
    Playout {
        mode: crate::config::PlayoutMode,
        fixed_delay_ms: Option<u64>,
        minimum_delay_ms: Option<u64>,
        maximum_delay_ms: Option<u64>,
        percentile: Option<f64>,
    },
    Simulate,
}

struct SetupOptions {
    state_dir: Option<PathBuf>,
    control_socket: Option<PathBuf>,
    listen: Option<std::net::SocketAddr>,
    devices: Vec<PathBuf>,
    activation_chord: Vec<String>,
    escape_chord: Vec<String>,
    udev_rules: Option<PathBuf>,
    allow_prelogin: Option<bool>,
    experimental_touchpad: Option<bool>,
}

pub fn run(path: PathBuf, command: Command) -> Result<()> {
    match command {
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
        Command::Setup {
            state_dir,
            control_socket,
            listen,
            devices,
            activation_chord,
            escape_chord,
            udev_rules,
            allow_prelogin,
            experimental_touchpad,
        } => setup(
            path,
            SetupOptions {
                state_dir,
                control_socket,
                listen,
                devices,
                activation_chord,
                escape_chord,
                udev_rules,
                allow_prelogin,
                experimental_touchpad,
            },
        ),
        Command::Status { json } => status(path, json),
        Command::Doctor => doctor(path),
        Command::Devices => devices(path),
        Command::Peers => peers(path),
        Command::PairConnect {
            peer,
            address,
            advertised,
            code,
            timeout,
        } => pair_connect(path, peer, address, advertised, code, timeout),
        Command::PairListen {
            peer,
            listen,
            advertised,
            code,
            timeout,
        } => pair_listen(path, peer, listen, advertised, code, timeout),
        Command::RevokePeer { peer } => revoke_peer(path, peer),
        Command::AllowPrelogin { peer, allowed } => allow_prelogin(path, peer, allowed),
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
        Command::Simulate => simulate(),
    }
}

fn setup(path: PathBuf, options: SetupOptions) -> Result<()> {
    let SetupOptions {
        state_dir,
        control_socket,
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
    if let Some(state_dir) = state_dir {
        config.daemon.state_dir = state_dir;
    }
    if let Some(control_socket) = control_socket {
        config.daemon.control_socket = control_socket;
    }
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
    if let Some(previous) = &original_config
        && previous.daemon.control_socket != config.daemon.control_socket
        && previous.daemon.control_socket.exists()
    {
        bail!(
            "changing daemon.control_socket while the previous socket exists is unsafe; stop zflowd, rerun setup, then start zflowd"
        );
    }
    let restart_required = original_config.as_ref().map_or_else(Vec::new, |previous| {
        restart_required_fields(previous, &config)
    });
    let rendered_rules = udev_rules
        .map(|rules_path| {
            render_capture_rules(&config.input.capture_devices).map(|rules| (rules_path, rules))
        })
        .transpose()?;
    let identity = Identity::load_or_create(&config.daemon.state_dir)?;

    let previous_config = FileSnapshot::capture(&path)?;
    let previous_rules = rendered_rules
        .as_ref()
        .map(|(rules_path, _)| FileSnapshot::capture(rules_path))
        .transpose()?;
    let commit: Result<()> = (|| {
        config.save(&path)?;
        if let Some((rules_path, rules)) = &rendered_rules {
            write_atomic_bytes(rules_path, rules.as_bytes(), 0o644)?;
        }
        if restart_required.is_empty() {
            let reload_socket = original_config
                .as_ref()
                .map(|previous| previous.daemon.control_socket.as_path())
                .filter(|socket| socket.exists())
                .unwrap_or(&config.daemon.control_socket);
            reload_running_daemon_at(reload_socket)?;
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
        if rollback_failures.is_empty() {
            bail!("setup failed and changes were rolled back: {error}");
        }
        bail!(
            "setup failed: {error}; rollback also failed: {}",
            rollback_failures.join("; ")
        );
    }
    if let Some((rules_path, _)) = &rendered_rules {
        println!("wrote capture permissions: {}", rules_path.display());
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
    if !restart_required.is_empty() {
        println!("{}", restart_required_message(&restart_required));
    }
    println!("run `zflow devices` to inspect capture candidates");
    Ok(())
}

fn restart_required_fields(previous: &Config, next: &Config) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if previous.daemon.state_dir != next.daemon.state_dir {
        fields.push("daemon.state_dir");
    }
    if previous.daemon.control_socket != next.daemon.control_socket {
        fields.push("daemon.control_socket");
    }
    if previous.transport.listen != next.transport.listen {
        fields.push("transport.listen");
    }
    if previous.input.experimental_touchpad != next.input.experimental_touchpad {
        fields.push("input.experimental_touchpad");
    }
    fields
}

fn restart_required_message(fields: &[&str]) -> String {
    format!(
        "daemon restart required to apply this setup (changed: {})",
        fields.join(", ")
    )
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
    let daemon = if config.daemon.control_socket.exists() {
        "socket-present"
    } else {
        "offline"
    };
    let status = OfflineStatus {
        daemon,
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
    #[cfg(unix)]
    {
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
    #[cfg(not(unix))]
    {
        let _ = expected_owner;
        match fs::symlink_metadata(path) {
            Ok(metadata) if kind.matches(&metadata) => {
                println!("ok  {}: {}", kind.name(), path.display());
                Some(0)
            }
            Ok(_) => {
                *failed = true;
                println!("fail {} {}: wrong file type", kind.name(), path.display());
                None
            }
            Err(error) => {
                *failed = true;
                println!("fail {} {}: {error}", kind.name(), path.display());
                None
            }
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

#[cfg(unix)]
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
        *failed = true;
        println!("fail capture selection: no physical input devices are configured");
        None
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
}

#[cfg(target_os = "linux")]
fn doctor_prelogin_ordering(failed: &mut bool) {
    let mut command = ProcessCommand::new("/usr/bin/systemctl");
    command
        .arg("show")
        .arg("--property=Before")
        .arg("--value")
        .arg("zflowd.service")
        .env_clear()
        .env("LANG", "C")
        .env("LC_ALL", "C");
    match bounded_command_output(&mut command, Duration::from_millis(750), 64 * 1024) {
        Ok(output)
            if output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .split_ascii_whitespace()
                    .any(|unit| unit == "display-manager.service") =>
        {
            println!("ok  pre-login boot ordering: before display-manager.service");
        }
        Ok(output) if output.status.success() => {
            *failed = true;
            println!(
                "fail pre-login boot ordering: zflowd.service is not ordered before display-manager.service"
            );
        }
        Ok(output) => {
            *failed = true;
            println!(
                "fail pre-login boot ordering: systemctl exited with {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Err(error) => {
            *failed = true;
            println!("fail pre-login boot ordering: {error}");
        }
    }
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
            let selected = configured
                .iter()
                .any(|selector| crate::runtime::capture_selector_matches(selector, info));
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
                .with_context(|| format!("could not connect to {}", socket.display()))?;
            write_message(&mut stream, &request).await?;
            Ok(read_message(&mut stream).await?)
        })
}

fn reload_running_daemon(config: &Config) -> Result<()> {
    reload_running_daemon_at(&config.daemon.control_socket)
}

fn reload_running_daemon_at(control_socket: &Path) -> Result<()> {
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

fn simulate() -> Result<()> {
    crate::core::simulator::run_prototype_scenario()
        .context("prototype simulator scenario failed")?;
    println!("prototype simulator: PASS");
    Ok(())
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
         # Grants only the configured physical devices to the zflow service account.\n",
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
        let physical_path = device.phys.as_deref().context(
            "selected device has no stable physical path; refusing to grant every identical device",
        )?;
        matches.push_str(&format!(
            ", ATTRS{{phys}}==\"{}\"",
            escape_udev_value(physical_path)?
        ));
        matches.push_str(", ENV{ZFLOW_CAPTURE}=\"1\", GROUP=\"zflow\", MODE=\"0640\"\n");
        text.push_str(&matches);
    }
    Ok(text)
}

#[cfg(test)]
fn write_capture_rules(path: &Path, devices: &[crate::config::DeviceSelector]) -> Result<()> {
    let text = render_capture_rules(devices)?;
    write_atomic_bytes(path, text.as_bytes(), 0o644)
}

fn write_atomic_bytes(path: &Path, contents: &[u8], create_mode: u32) -> Result<()> {
    let parent = path.parent().context("udev rules path has no parent")?;
    fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("udev rules path has no file name")?;
    let temporary = parent.join(format!(".{name}.tmp-{}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    #[cfg(unix)]
    options.mode(create_mode);
    #[cfg(unix)]
    let existing = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            Some((metadata.mode(), metadata.uid(), metadata.gid()))
        }
        Ok(_) => bail!("refusing to replace non-regular file {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    #[cfg(not(unix))]
    let _ = create_mode;
    #[cfg(not(unix))]
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => bail!("refusing to replace non-regular file {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut file = options.open(&temporary)?;
    let write_result = (|| {
        file.write_all(contents)?;
        file.sync_all()?;
        #[cfg(unix)]
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

fn escape_udev_value(value: &str) -> Result<String> {
    if value.chars().any(char::is_control) {
        bail!("udev match values cannot contain control characters");
    }
    Ok(value.replace('\\', "\\\\").replace('"', "\\\""))
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

fn pair_connect(
    path: PathBuf,
    peer: String,
    address: SocketAddr,
    advertised: Vec<SocketAddr>,
    code: Option<String>,
    timeout: Duration,
) -> Result<()> {
    let config = Config::load(&path)?;
    let identity = Identity::load_or_create(&config.daemon.state_dir)?;
    let offer = crate::pairing::make_offer(
        local_device_label(),
        config.transport.listen.port(),
        advertised,
    )?;
    println!("connecting to pairing listener at {address}");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    let session = runtime.block_on(async {
        tokio::time::timeout(timeout, crate::pairing::connect(&identity, address, &offer))
            .await
            .context("pairing timed out")?
    })?;
    confirm_and_store(path, peer, session.observation().clone(), code)
}

fn pair_listen(
    path: PathBuf,
    peer: String,
    listen: SocketAddr,
    advertised: Vec<SocketAddr>,
    code: Option<String>,
    timeout: Duration,
) -> Result<()> {
    let config = Config::load(&path)?;
    let identity = Identity::load_or_create(&config.daemon.state_dir)?;
    let offer = crate::pairing::make_offer(
        local_device_label(),
        config.transport.listen.port(),
        advertised,
    )?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    let session = runtime.block_on(async {
        let listener = crate::pairing::PairingListener::bind(&identity, listen, offer)?;
        println!("pairing listener: {}", listener.local_addr()?);
        tokio::time::timeout(timeout, listener.accept())
            .await
            .context("pairing timed out")?
    })?;
    confirm_and_store(path, peer, session.observation().clone(), code)
}

fn confirm_and_store(
    path: PathBuf,
    peer: String,
    observation: crate::pairing::PairingObservation,
    supplied_code: Option<String>,
) -> Result<()> {
    println!("pairing code: {}", observation.authentication_code);
    if let Some(label) = &observation.peer_label {
        println!("peer label: {label}");
    }
    let peer_code = match supplied_code {
        Some(code) => code,
        None => {
            if !std::io::stdin().is_terminal() {
                bail!("standard input is not interactive; pass --code from the peer display");
            }
            print!("enter the code shown on the peer: ");
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line)?;
            line.trim().to_owned()
        }
    };
    if peer_code != observation.authentication_code {
        bail!("pairing code mismatch; no trust record was written");
    }

    let record = PeerConfig::from_spki(
        &observation.peer_spki,
        observation.peer_candidates,
        PeerPermissions {
            connect: true,
            send_normal: true,
            receive_normal: true,
            inject_prelogin: false,
        },
    )?;
    let fingerprint = record.fingerprint_hex()?;
    let config = Config::load(&path)?;
    if config.daemon.control_socket.exists() {
        match daemon_request(
            &config.daemon.control_socket,
            Request::AddPeer {
                peer: peer.clone(),
                record,
            },
        )? {
            Response::Ack => {}
            Response::Error { message } => bail!("daemon rejected paired peer: {message}"),
            response => bail!("unexpected daemon response: {response:?}"),
        }
    } else {
        let mut config = config;
        config.peers.insert(peer.clone(), record);
        config.save(&path)?;
    }
    println!("paired {peer} ({fingerprint})");
    println!("pre-login permission remains off");
    Ok(())
}

fn local_device_label() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|label| !label.is_empty() && label.len() <= 255)
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

    #[cfg(unix)]
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
    fn bounded_command_caps_helper_output() {
        let mut command = ProcessCommand::new("/usr/bin/printf");
        command.arg("12345");
        assert!(bounded_command_output(&mut command, Duration::from_secs(1), 4).is_err());
    }

    #[test]
    fn udev_values_escape_quotes_and_backslashes() {
        assert_eq!(
            escape_udev_value("board \\\"left\"").unwrap(),
            "board \\\\\\\"left\\\""
        );
        assert!(escape_udev_value("line\nbreak").is_err());
    }

    #[test]
    fn capture_rule_is_narrow_and_excludes_virtual_devices() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("71-zflow-capture.rules");
        write_capture_rules(
            &path,
            &[crate::config::DeviceSelector {
                path: "/dev/input/event7".into(),
                name: Some("Example Keyboard".into()),
                phys: Some("usb-1/input0".into()),
                vendor: Some(0x1234),
                product: Some(0xabcd),
            }],
        )
        .unwrap();
        let rule = fs::read_to_string(path).unwrap();
        assert!(rule.contains("ATTRS{id/vendor}==\"1234\""));
        assert!(rule.contains("ATTRS{id/product}==\"abcd\""));
        assert!(rule.contains("ENV{ZFLOW_CAPTURE_EXCLUDE}!=\"1\""));
        assert!(rule.contains("ATTRS{phys}==\"usb-1/input0\""));
    }

    #[test]
    fn capture_rule_refuses_a_device_without_a_stable_physical_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("71-zflow-capture.rules");
        let error = write_capture_rules(
            &path,
            &[crate::config::DeviceSelector {
                path: "/dev/input/event7".into(),
                name: Some("Indistinguishable Keyboard".into()),
                phys: None,
                vendor: Some(0x1234),
                product: Some(0xabcd),
            }],
        )
        .unwrap_err();

        assert!(error.to_string().contains("no stable physical path"));
        assert!(!path.exists());
    }

    #[test]
    fn confirmed_pairing_pins_key_and_keeps_prelogin_off() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let mut config = Config::default();
        config.daemon.state_dir = directory.path().join("state");
        config.daemon.control_socket = directory.path().join("missing.sock");
        config.save(&path).unwrap();
        let address = "127.0.0.1:43119".parse().unwrap();
        let observation = crate::pairing::PairingObservation {
            peer_spki: b"peer public key".to_vec(),
            peer_label: Some("desk".into()),
            peer_candidates: vec![address],
            authentication_code: "123456".into(),
        };

        confirm_and_store(
            path.clone(),
            "desk".into(),
            observation,
            Some("123456".into()),
        )
        .unwrap();

        let saved = Config::load(&path).unwrap();
        let peer = &saved.peers["desk"];
        assert_eq!(peer.spki_der().unwrap(), b"peer public key");
        assert_eq!(peer.addresses, vec![address]);
        assert!(peer.permissions.connect);
        assert!(peer.permissions.send_normal);
        assert!(peer.permissions.receive_normal);
        assert!(!peer.permissions.inject_prelogin);
    }

    #[test]
    fn mismatched_pairing_code_writes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let mut config = Config::default();
        config.daemon.state_dir = directory.path().join("state");
        config.daemon.control_socket = directory.path().join("missing.sock");
        config.save(&path).unwrap();
        let observation = crate::pairing::PairingObservation {
            peer_spki: b"peer public key".to_vec(),
            peer_label: None,
            peer_candidates: vec!["127.0.0.1:43119".parse().unwrap()],
            authentication_code: "123456".into(),
        };

        assert!(
            confirm_and_store(
                path.clone(),
                "desk".into(),
                observation,
                Some("654321".into())
            )
            .is_err()
        );
        assert!(Config::load(&path).unwrap().peers.is_empty());
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
                state_dir: None,
                control_socket: None,
                listen: None,
                devices: Vec::new(),
                activation_chord: Vec::new(),
                escape_chord: Vec::new(),
                udev_rules: None,
                allow_prelogin: Some(false),
                experimental_touchpad: None,
            },
        )
        .unwrap();

        assert!(!Config::load(&path).unwrap().input.allow_prelogin_input);
    }

    #[test]
    fn setup_persists_experimental_touchpad_as_restart_required() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let mut config = Config::default();
        config.daemon.state_dir = directory.path().join("state");
        config.daemon.control_socket = directory.path().join("missing.sock");
        config.save(&path).unwrap();

        setup(
            path.clone(),
            SetupOptions {
                state_dir: None,
                control_socket: None,
                listen: None,
                devices: Vec::new(),
                activation_chord: Vec::new(),
                escape_chord: Vec::new(),
                udev_rules: None,
                allow_prelogin: None,
                experimental_touchpad: Some(true),
            },
        )
        .unwrap();

        let saved = Config::load(&path).unwrap();
        assert!(saved.input.experimental_touchpad);
        assert_eq!(
            restart_required_fields(&config, &saved),
            ["input.experimental_touchpad"]
        );
    }

    #[test]
    fn setup_persists_offline_restart_only_overrides_without_live_reload() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let stale_socket = directory.path().join("stale.sock");
        let new_state_dir = directory.path().join("new-state");
        let new_socket = directory.path().join("new.sock");
        let new_listen = "127.0.0.1:43219".parse().unwrap();
        let mut config = Config::default();
        config.daemon.state_dir = directory.path().join("old-state");
        config.daemon.control_socket = stale_socket.clone();
        config.save(&path).unwrap();
        setup(
            path.clone(),
            SetupOptions {
                state_dir: Some(new_state_dir.clone()),
                control_socket: Some(new_socket.clone()),
                listen: Some(new_listen),
                devices: Vec::new(),
                activation_chord: Vec::new(),
                escape_chord: Vec::new(),
                udev_rules: None,
                allow_prelogin: None,
                experimental_touchpad: None,
            },
        )
        .unwrap();

        let saved = Config::load(&path).unwrap();
        assert_eq!(saved.daemon.state_dir, new_state_dir);
        assert_eq!(saved.daemon.control_socket, new_socket);
        assert_eq!(saved.transport.listen, new_listen);
        assert_eq!(
            restart_required_fields(&config, &saved),
            [
                "daemon.state_dir",
                "daemon.control_socket",
                "transport.listen"
            ]
        );
        assert_eq!(
            restart_required_message(&restart_required_fields(&config, &saved)),
            "daemon restart required to apply this setup (changed: daemon.state_dir, daemon.control_socket, transport.listen)"
        );
    }

    #[test]
    fn setup_refuses_live_control_socket_migration_before_any_write() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let old_socket = directory.path().join("old.sock");
        let new_state_dir = directory.path().join("new-state");
        let rules = directory.path().join("capture.rules");
        let mut config = Config::default();
        config.daemon.state_dir = directory.path().join("old-state");
        config.daemon.control_socket = old_socket.clone();
        config.save(&path).unwrap();
        fs::write(old_socket, []).unwrap();
        let before = fs::read(&path).unwrap();

        let error = setup(
            path.clone(),
            SetupOptions {
                state_dir: Some(new_state_dir.clone()),
                control_socket: Some(directory.path().join("new.sock")),
                listen: None,
                devices: Vec::new(),
                activation_chord: Vec::new(),
                escape_chord: Vec::new(),
                udev_rules: Some(rules.clone()),
                allow_prelogin: None,
                experimental_touchpad: None,
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("stop zflowd"));
        assert_eq!(fs::read(path).unwrap(), before);
        assert!(!new_state_dir.exists());
        assert!(!rules.exists());
    }

    #[cfg(unix)]
    #[test]
    fn pending_restart_makes_a_later_live_setup_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("zflow.toml");
        let stale_socket = directory.path().join("stale.sock");
        let new_state_dir = directory.path().join("new-state");
        let new_listen = "127.0.0.1:43219".parse().unwrap();
        let mut config = Config::default();
        config.daemon.state_dir = directory.path().join("old-state");
        config.daemon.control_socket = stale_socket.clone();
        config.input.allow_prelogin_input = true;
        config.save(&path).unwrap();
        fs::write(stale_socket, []).unwrap();

        setup(
            path.clone(),
            SetupOptions {
                state_dir: Some(new_state_dir.clone()),
                control_socket: None,
                listen: Some(new_listen),
                devices: Vec::new(),
                activation_chord: Vec::new(),
                escape_chord: Vec::new(),
                udev_rules: None,
                allow_prelogin: None,
                experimental_touchpad: None,
            },
        )
        .unwrap();
        let pending = fs::read(&path).unwrap();

        let result = setup(
            path.clone(),
            SetupOptions {
                state_dir: None,
                control_socket: None,
                listen: None,
                devices: Vec::new(),
                activation_chord: Vec::new(),
                escape_chord: Vec::new(),
                udev_rules: None,
                allow_prelogin: Some(false),
                experimental_touchpad: None,
            },
        );

        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), pending);
        let saved = Config::load(&path).unwrap();
        assert_eq!(saved.daemon.state_dir, new_state_dir);
        assert_eq!(saved.transport.listen, new_listen);
        assert!(saved.input.allow_prelogin_input);
    }

    #[cfg(unix)]
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
                state_dir: None,
                control_socket: None,
                listen: None,
                devices: Vec::new(),
                activation_chord: Vec::new(),
                escape_chord: Vec::new(),
                udev_rules: None,
                allow_prelogin: Some(false),
                experimental_touchpad: None,
            },
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
                state_dir: None,
                control_socket: None,
                listen: None,
                devices: Vec::new(),
                activation_chord: vec!["KEY_THIS_DOES_NOT_EXIST".into()],
                escape_chord: Vec::new(),
                udev_rules: None,
                allow_prelogin: None,
                experimental_touchpad: None,
            },
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
                state_dir: None,
                control_socket: None,
                listen: None,
                devices: Vec::new(),
                activation_chord: Vec::new(),
                escape_chord: Vec::new(),
                udev_rules: Some(rules_path.clone()),
                allow_prelogin: None,
                experimental_touchpad: None,
            },
        );

        assert!(result.is_err());
        assert_eq!(fs::read(path).unwrap(), config_before);
        assert_eq!(fs::read(rules_path).unwrap(), rules_before);
    }

    #[cfg(unix)]
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
