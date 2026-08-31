//! Spike E: pre-login uinput injection.
//!
//! The system service creates and classifies a virtual keyboard, reports
//! readiness to systemd, then emits one visible sentinel after the display
//! manager starts. The sentinel never includes Enter: it types ZFLOW and
//! removes the five characters with Backspace.

use std::env;
use std::fs::OpenOptions;
use std::io::{Error, ErrorKind, Write};
use std::path::PathBuf;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{AttributeSet, EventType, InputEvent, Key};

const DEVICE_NAME: &str = "zflow-spike-keyboard";
const SENTINEL: [Key; 5] = [Key::KEY_Z, Key::KEY_F, Key::KEY_L, Key::KEY_O, Key::KEY_W];

fn uptime() -> f64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse().ok()))
        .unwrap_or(0.0)
}

fn command_stdout(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default()
}

fn display_manager_state() -> String {
    let state = command_stdout("systemctl", &["is-active", "display-manager.service"]);
    if state.is_empty() {
        "unknown".into()
    } else {
        state
    }
}

struct Log(std::fs::File);

impl Log {
    fn open(path: &str) -> std::io::Result<Self> {
        Ok(Self(
            OpenOptions::new().create(true).append(true).open(path)?,
        ))
    }

    fn line(&mut self, message: &str) {
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let _ = writeln!(self.0, "[uptime {:.2}s wall {wall}] {message}", uptime());
        let _ = self.0.flush();
    }

    fn block(&mut self, label: &str, text: &str) {
        if text.is_empty() {
            self.line(&format!("{label}: NOT FOUND"));
            return;
        }
        for line in text.lines() {
            self.line(&format!("{label}: {line}"));
        }
    }
}

fn tap(device: &mut VirtualDevice, key: Key) -> std::io::Result<()> {
    device.emit(&[InputEvent::new(EventType::KEY, key.code(), 1)])?;
    device.emit(&[InputEvent::new(EventType::KEY, key.code(), 0)])?;
    Ok(())
}

fn keyboard_keys() -> AttributeSet<Key> {
    let mut keys = AttributeSet::<Key>::new();
    // Advertise the conventional keyboard range. systemd's input_id builtin
    // requires the low key-code block before it sets ID_INPUT_KEYBOARD=1.
    for code in Key::KEY_ESC.code()..=Key::KEY_MICMUTE.code() {
        keys.insert(Key::new(code));
    }
    keys
}

fn event_node(device: &mut VirtualDevice) -> std::io::Result<PathBuf> {
    device
        .enumerate_dev_nodes_blocking()?
        .next()
        .transpose()?
        .ok_or_else(|| Error::new(ErrorKind::NotFound, "uinput event node did not appear"))
}

fn libinput_device_block<'a>(output: &'a str, name: &str) -> Option<&'a str> {
    output.split("\n\n").find(|block| {
        block.lines().any(|line| {
            line.strip_prefix("Device:")
                .is_some_and(|value| value.trim() == name)
        })
    })
}

fn record_classification(log: &mut Log, event_node: &str) {
    let _ = Command::new("udevadm").arg("settle").status();

    let udev = command_stdout(
        "udevadm",
        &["info", "--query=property", "--name", event_node],
    );
    log.block("udev", &udev);

    let devices = command_stdout("libinput", &["list-devices"]);
    let block = libinput_device_block(&devices, DEVICE_NAME).unwrap_or("");
    log.block("libinput", block);
}

fn notify_ready(log: &mut Log) -> std::io::Result<()> {
    if env::var_os("NOTIFY_SOCKET").is_none() {
        log.line("NOTIFY_SOCKET absent; readiness barrier not active");
        return Ok(());
    }

    let status = Command::new("systemd-notify")
        .args([
            "--ready",
            "--status=zflow spike keyboard created and classified",
        ])
        .status()?;
    if !status.success() {
        return Err(Error::other(format!("systemd-notify failed with {status}")));
    }
    log.line("READY=1 sent; display manager may start");
    Ok(())
}

fn wait_for_display_manager(log: &mut Log, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if display_manager_state() == "active" {
            log.line("display manager is active");
            return true;
        }
        sleep(Duration::from_millis(100));
    }
    log.line("display manager did not become active before timeout; skipping injection");
    false
}

fn active_user_on_seat0() -> Option<String> {
    let sessions = command_stdout("loginctl", &["list-sessions", "--no-legend", "--no-pager"]);
    for session_id in sessions
        .lines()
        .filter_map(|line| line.split_whitespace().next())
    {
        let details = command_stdout(
            "loginctl",
            &[
                "show-session",
                session_id,
                "--property=Active",
                "--property=Class",
                "--property=Seat",
            ],
        );
        let active = details.lines().any(|line| line == "Active=yes");
        let user = details.lines().any(|line| line == "Class=user");
        let seat0 = details.lines().any(|line| line == "Seat=seat0");
        if active && user && seat0 {
            return Some(session_id.to_string());
        }
    }
    None
}

fn emit_sentinel(device: &mut VirtualDevice) -> std::io::Result<()> {
    for key in SENTINEL {
        tap(device, key)?;
        sleep(Duration::from_millis(250));
    }
    sleep(Duration::from_millis(400));
    for _ in 0..SENTINEL.len() {
        tap(device, Key::KEY_BACKSPACE)?;
        sleep(Duration::from_millis(120));
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let mut log_path = "/var/log/zflow-spike-e.log".to_string();
    let mut greeter_timeout = Duration::from_secs(30);
    let mut settle = Duration::from_millis(1_000);
    let mut no_inject = false;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--log" => log_path = args.next().unwrap_or(log_path),
            "--greeter-timeout" | "--delay" => {
                greeter_timeout = args
                    .next()
                    .and_then(|value| value.parse().ok())
                    .map(Duration::from_secs)
                    .unwrap_or(greeter_timeout);
            }
            "--settle-ms" => {
                settle = args
                    .next()
                    .and_then(|value| value.parse().ok())
                    .map(Duration::from_millis)
                    .unwrap_or(settle);
            }
            "--no-inject" => no_inject = true,
            // Consume arguments from the first installed spike. The corrected
            // harness emits one cycle regardless of these legacy values.
            "--rounds" | "--interval" => {
                let _ = args.next();
            }
            _ => {}
        }
    }

    let mut log = Log::open(&log_path)?;
    log.line(&format!(
        "START pid={} display-manager={} (creating keyboard)",
        std::process::id(),
        display_manager_state()
    ));

    let keys = keyboard_keys();
    let mut device = VirtualDeviceBuilder::new()?
        .name(DEVICE_NAME)
        .with_keys(&keys)?
        .build()?;
    let event_node = event_node(&mut device)?;
    let event_node = event_node.to_string_lossy();
    log.line(&format!("uinput keyboard created at {event_node}"));
    record_classification(&mut log, &event_node);
    notify_ready(&mut log)?;

    if no_inject {
        log.line("DONE (classification-only run; removing device)");
        return Ok(());
    }
    if !wait_for_display_manager(&mut log, greeter_timeout) {
        return Ok(());
    }

    sleep(settle);
    if let Some(session) = active_user_on_seat0() {
        log.line(&format!(
            "authenticated seat0 session {session} already active; skipping injection"
        ));
        return Ok(());
    }

    log.line("typing one ZFLOW + 5x Backspace cycle; no Enter");
    emit_sentinel(&mut device)?;
    log.line("DONE (sentinel emitted; removing device)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_advertises_classification_and_sentinel_keys() {
        let keys = keyboard_keys();
        for code in Key::KEY_ESC.code()..=Key::KEY_S.code() {
            assert!(keys.contains(Key::new(code)));
        }
        for key in SENTINEL.into_iter().chain([Key::KEY_BACKSPACE]) {
            assert!(keys.contains(key));
        }
    }

    #[test]
    fn extracts_named_libinput_device() {
        let output = "Device:                  other\nKernel:                  /dev/input/event1\n\nDevice:                  zflow-spike-keyboard\nKernel:                  /dev/input/event2\nSeat:                    seat0, default\n";
        let block = libinput_device_block(output, DEVICE_NAME).unwrap();
        assert!(block.contains("/dev/input/event2"));
        assert!(block.contains("seat0, default"));
    }
}
