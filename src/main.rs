use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "zflow", version, about = "Headless zflow setup and control")]
struct Cli {
    /// Configuration file used by setup and offline diagnostics.
    #[arg(long, global = true, default_value = "/etc/zflow/zflow.toml")]
    config: std::path::PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the GNOME session integration without a window.
    DesktopAgent {
        /// Install the GNOME extension and start this agent at login.
        #[arg(long)]
        install: bool,
    },
    /// Create or update the local configuration.
    Setup {
        /// Override the daemon state directory; changes require a daemon restart.
        #[arg(long)]
        state_dir: Option<std::path::PathBuf>,
        /// Override the local daemon control socket; changes require a daemon restart.
        #[arg(long)]
        control_socket: Option<std::path::PathBuf>,
        /// Override the QUIC listen address; changes require a daemon restart.
        #[arg(long)]
        listen: Option<std::net::SocketAddr>,
        /// Select a physical evdev node; repeated values replace the capture set.
        #[arg(long = "device")]
        devices: Vec<std::path::PathBuf>,
        /// Set one evdev key in the activation chord; repeat for every key.
        #[arg(long = "activation-key", value_name = "EVDEV_KEY")]
        activation_chord: Vec<String>,
        /// Set one evdev key in the local escape chord; repeat for every key.
        #[arg(long = "escape-key", value_name = "EVDEV_KEY")]
        escape_chord: Vec<String>,
        /// Write explicit capture permissions for the selected devices.
        #[arg(long)]
        udev_rules: Option<std::path::PathBuf>,
        /// Enable or disable injection outside an unlocked authenticated session.
        #[arg(long = "prelogin", value_enum, value_name = "on|off")]
        allow_prelogin: Option<Toggle>,
        /// Enable or disable experimental raw touchpad forwarding.
        #[arg(long = "experimental-touchpad", value_enum, value_name = "on|off")]
        experimental_touchpad: Option<Toggle>,
    },
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
    /// Pair with another logged-in zflow CLI using an authenticated code.
    Pair {
        #[command(subcommand)]
        command: PairCommand,
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
        #[arg(value_enum)]
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
    /// Run the deterministic protocol failure simulator.
    Simulate,
}

#[derive(Debug, Subcommand)]
enum PeerCommand {
    /// Revoke a peer and close its future access.
    Revoke { peer: String },
    /// Grant or revoke the separate pre-login permission.
    AllowPrelogin {
        peer: String,
        #[arg(value_enum)]
        value: Toggle,
    },
}

#[derive(Debug, Subcommand)]
enum PairCommand {
    /// Connect to a peer's temporary pairing listener.
    Connect {
        /// Local name to assign to the peer.
        peer: String,
        /// Peer pairing address, normally port 43120.
        address: std::net::SocketAddr,
        /// Extra local input address to send to the peer.
        #[arg(long = "advertise")]
        advertised: Vec<std::net::SocketAddr>,
        /// Code displayed by the peer; prompts on a terminal when omitted.
        #[arg(long)]
        code: Option<String>,
        /// Stop waiting after this many seconds.
        #[arg(long, default_value_t = 120)]
        timeout_seconds: u64,
    },
    /// Accept one connection on a temporary pairing listener.
    Listen {
        /// Local name to assign to the peer.
        peer: String,
        /// Pairing listener address.
        #[arg(long, default_value = "[::]:43120")]
        listen: std::net::SocketAddr,
        /// Extra local input address to send to the peer.
        #[arg(long = "advertise")]
        advertised: Vec<std::net::SocketAddr>,
        /// Code displayed by the peer; prompts on a terminal when omitted.
        #[arg(long)]
        code: Option<String>,
        /// Stop waiting after this many seconds.
        #[arg(long, default_value_t = 120)]
        timeout_seconds: u64,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Toggle {
    On,
    Off,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum PlayoutMode {
    Fixed,
    Adaptive,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    zflow::cli::run(cli.config, cli.command.into())
}

impl From<Command> for zflow::cli::Command {
    fn from(value: Command) -> Self {
        match value {
            Command::DesktopAgent { install } => Self::DesktopAgent { install },
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
            } => Self::Setup {
                state_dir,
                control_socket,
                listen,
                devices,
                activation_chord,
                escape_chord,
                udev_rules,
                allow_prelogin: allow_prelogin.map(|value| matches!(value, Toggle::On)),
                experimental_touchpad: experimental_touchpad
                    .map(|value| matches!(value, Toggle::On)),
            },
            Command::Status { json } => Self::Status { json },
            Command::Doctor => Self::Doctor,
            Command::Devices => Self::Devices,
            Command::Peers => Self::Peers,
            Command::Pair { command } => match command {
                PairCommand::Connect {
                    peer,
                    address,
                    advertised,
                    code,
                    timeout_seconds,
                } => Self::PairConnect {
                    peer,
                    address,
                    advertised,
                    code,
                    timeout: std::time::Duration::from_secs(timeout_seconds),
                },
                PairCommand::Listen {
                    peer,
                    listen,
                    advertised,
                    code,
                    timeout_seconds,
                } => Self::PairListen {
                    peer,
                    listen,
                    advertised,
                    code,
                    timeout: std::time::Duration::from_secs(timeout_seconds),
                },
            },
            Command::Peer { command } => match command {
                PeerCommand::Revoke { peer } => Self::RevokePeer { peer },
                PeerCommand::AllowPrelogin { peer, value } => Self::AllowPrelogin {
                    peer,
                    allowed: matches!(value, Toggle::On),
                },
            },
            Command::Switch { peer } => Self::Switch { peer },
            Command::Local => Self::Local,
            Command::Playout {
                mode,
                fixed_delay_ms,
                minimum_delay_ms,
                maximum_delay_ms,
                percentile,
            } => Self::Playout {
                mode: match mode {
                    PlayoutMode::Fixed => zflow::config::PlayoutMode::Fixed,
                    PlayoutMode::Adaptive => zflow::config::PlayoutMode::Adaptive,
                },
                fixed_delay_ms,
                minimum_delay_ms,
                maximum_delay_ms,
                percentile,
            },
            Command::Simulate => Self::Simulate,
        }
    }
}
