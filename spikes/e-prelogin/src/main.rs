//! Spike E: pre-login uinput injection.
//!
//! Runs as a root systemd service ordered BEFORE the display manager. It
//! creates a uinput keyboard immediately (the early-boot artifact: the device
//! must exist before the greeter's compositor enumerates input), logs boot
//! ordering, waits for the greeter, then repeatedly types a short visible
//! sequence into whatever field has focus so a human can watch machine-paced
//! dots appear in the greeter password box.
//!
//! Everything is logged to a file that survives the reboot, since the Claude
//! session that built this is gone once the machine restarts. Read the log
//! afterward to confirm ordering and device creation; confirm the injection
//! visually at the greeter.
//!
//! Args: --log PATH  --delay SECS (initial wait)  --rounds N  --interval SECS

use std::fs::OpenOptions;
use std::io::Write;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use evdev::uinput::VirtualDeviceBuilder;
use evdev::{AttributeSet, EventType, InputEvent, Key};

fn uptime() -> f64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse().ok()))
        .unwrap_or(0.0)
}

fn gdm_state() -> String {
    Command::new("systemctl")
        .args(["is-active", "display-manager.service"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

struct Log(std::fs::File);
impl Log {
    fn open(path: &str) -> std::io::Result<Self> {
        Ok(Self(
            OpenOptions::new().create(true).append(true).open(path)?,
        ))
    }
    fn line(&mut self, msg: &str) {
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(self.0, "[uptime {:.2}s wall {}] {}", uptime(), wall, msg);
        let _ = self.0.flush();
    }
}

fn tap(dev: &mut evdev::uinput::VirtualDevice, key: Key) -> std::io::Result<()> {
    dev.emit(&[InputEvent::new(EventType::KEY, key.code(), 1)])?;
    dev.emit(&[InputEvent::new(EventType::KEY, key.code(), 0)])?;
    Ok(())
}

fn main() -> std::io::Result<()> {
    let mut log_path = "/var/log/zflow-spike-e.log".to_string();
    let mut delay = 20u64;
    let mut rounds = 24u64;
    let mut interval = 5u64;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--log" => log_path = args.next().unwrap_or(log_path),
            "--delay" => delay = args.next().and_then(|v| v.parse().ok()).unwrap_or(delay),
            "--rounds" => rounds = args.next().and_then(|v| v.parse().ok()).unwrap_or(rounds),
            "--interval" => interval = args.next().and_then(|v| v.parse().ok()).unwrap_or(interval),
            _ => {}
        }
    }

    let mut log = Log::open(&log_path)?;
    log.line(&format!(
        "START pid {} gdm={} (device creation happens now, before greeter)",
        std::process::id(),
        gdm_state()
    ));

    // the sequence typed each round: 5 visible keys, then 5 backspaces to
    // leave the field clean for a normal login
    let word = [Key::KEY_Z, Key::KEY_F, Key::KEY_L, Key::KEY_O, Key::KEY_W];
    let mut keys = AttributeSet::<Key>::new();
    for k in word.iter().copied().chain([Key::KEY_BACKSPACE, Key::KEY_ENTER]) {
        keys.insert(k);
    }
    let mut dev = VirtualDeviceBuilder::new()?
        .name("zflow-spike-keyboard")
        .with_keys(&keys)?
        .build()?;
    log.line("uinput keyboard created");

    // let udev classify it, then record how it landed
    sleep(Duration::from_secs(2));
    let class = Command::new("sh")
        .arg("-c")
        .arg("for d in /sys/devices/virtual/input/input*; do grep -ql zflow-spike-keyboard $d/name 2>/dev/null && echo $d; done")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    log.line(&format!("sysfs path: {}", if class.is_empty() { "NOT FOUND" } else { &class }));

    log.line(&format!("waiting {delay}s for the greeter to come up"));
    sleep(Duration::from_secs(delay));
    log.line(&format!(
        "greeter wait done, gdm={}; typing {rounds} rounds every {interval}s",
        gdm_state()
    ));

    for r in 1..=rounds {
        for k in word {
            tap(&mut dev, k)?;
            sleep(Duration::from_millis(250));
        }
        sleep(Duration::from_millis(400));
        for _ in 0..word.len() {
            tap(&mut dev, Key::KEY_BACKSPACE)?;
            sleep(Duration::from_millis(120));
        }
        log.line(&format!("round {r}/{rounds} typed ZFLOW + 5x backspace, gdm={}", gdm_state()));
        sleep(Duration::from_secs(interval));
    }

    log.line("DONE (all rounds typed; removing device)");
    Ok(())
}
