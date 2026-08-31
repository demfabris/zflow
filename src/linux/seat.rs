use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, io,
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use thiserror::Error;

const PRIMARY_SEAT: &str = "seat0";
const MAX_LOGINCTL_OUTPUT: usize = 64 * 1024;
const DEFAULT_QUERY_TIMEOUT: Duration = Duration::from_millis(200);

/// The permission class required before remote input may reach the active
/// Linux seat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectionGate {
    /// An unlocked, authenticated local user session is active.
    Normal { uid: u32 },
    /// The active seat is in a known greeter, lock, or login state.
    PreLogin,
    /// Logind did not provide a complete and self-consistent answer.
    Denied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthenticatedSessionKind {
    Wayland,
    VirtualTerminal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedSession {
    pub id: String,
    pub uid: u32,
    pub kind: AuthenticatedSessionKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestrictedSeatState {
    /// No logind session owns the foreground VT. This includes a getty login
    /// prompt and the period before a display manager starts.
    NoActiveSession,
    Greeter {
        session_id: String,
    },
    LockScreen {
        session_id: String,
    },
    LockedUser(AuthenticatedSession),
}

/// A point-in-time classification of the local Linux seat.
///
/// `Unknown` is intentionally different from `Restricted`: pre-login
/// permission can authorize only states logind identified conclusively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeatState {
    Unlocked(AuthenticatedSession),
    Restricted(RestrictedSeatState),
    Unknown { reason: String },
}

impl SeatState {
    pub fn injection_gate(&self) -> InjectionGate {
        match self {
            Self::Unlocked(session) => InjectionGate::Normal { uid: session.uid },
            Self::Restricted(_) => InjectionGate::PreLogin,
            Self::Unknown { .. } => InjectionGate::Denied,
        }
    }

    /// Returns the interactive user's UID for local IPC authorization.
    ///
    /// A locked user remains the authenticated owner of their session. A
    /// greeter or lock-screen pseudo-session is not an interactive user.
    pub fn active_authenticated_uid(&self) -> Option<u32> {
        match self {
            Self::Unlocked(session)
            | Self::Restricted(RestrictedSeatState::LockedUser(session)) => Some(session.uid),
            Self::Restricted(
                RestrictedSeatState::NoActiveSession
                | RestrictedSeatState::Greeter { .. }
                | RestrictedSeatState::LockScreen { .. },
            )
            | Self::Unknown { .. } => None,
        }
    }
}

/// Injectable boundary around logind. Tests and simulators do not need a
/// desktop or a running system bus.
pub trait LogindQuery {
    type Error: fmt::Display;

    fn show_seat(&self, seat: &str) -> Result<String, Self::Error>;
    fn show_session(&self, session: &str) -> Result<String, Self::Error>;
}

#[derive(Debug, Clone)]
pub struct LoginctlQuery {
    program: PathBuf,
    timeout: Duration,
}

impl Default for LoginctlQuery {
    fn default() -> Self {
        Self {
            program: PathBuf::from("/usr/bin/loginctl"),
            timeout: DEFAULT_QUERY_TIMEOUT,
        }
    }
}

impl LoginctlQuery {
    pub fn new(program: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            program: program.into(),
            timeout,
        }
    }

    fn run(&self, subject: &'static str, args: &[&str]) -> Result<String, LoginctlError> {
        // Capture and uinput handles are opened through Rust/evdev
        // OpenOptions, which sets O_CLOEXEC on Unix. Keep the helper's three
        // standard streams explicit and never clear close-on-exec flags.
        let mut child = Command::new(&self.program)
            .args(["--no-pager", "--no-legend", "--all"])
            .args(args)
            .env_clear()
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| LoginctlError::Spawn { subject, source })?;

        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if started.elapsed() < self.timeout => {
                    thread::sleep(Duration::from_millis(2));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(LoginctlError::Timeout {
                        subject,
                        timeout: self.timeout,
                    });
                }
                Err(source) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(LoginctlError::Wait { subject, source });
                }
            }
        }

        let output = child
            .wait_with_output()
            .map_err(|source| LoginctlError::Wait { subject, source })?;
        if output.stdout.len() > MAX_LOGINCTL_OUTPUT || output.stderr.len() > MAX_LOGINCTL_OUTPUT {
            return Err(LoginctlError::OutputTooLarge { subject });
        }
        if !output.status.success() {
            return Err(LoginctlError::Failed {
                subject,
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        String::from_utf8(output.stdout).map_err(|_| LoginctlError::InvalidUtf8 { subject })
    }
}

impl LogindQuery for LoginctlQuery {
    type Error = LoginctlError;

    fn show_seat(&self, seat: &str) -> Result<String, Self::Error> {
        validate_identifier("seat", seat)?;
        self.run(
            "seat",
            &[
                "--property=Id",
                "--property=ActiveSession",
                "--property=Sessions",
                "show-seat",
                seat,
            ],
        )
    }

    fn show_session(&self, session: &str) -> Result<String, Self::Error> {
        validate_identifier("session", session)?;
        self.run(
            "session",
            &[
                "--property=Id",
                "--property=User",
                "--property=VTNr",
                "--property=Seat",
                "--property=TTY",
                "--property=Remote",
                "--property=Type",
                "--property=Class",
                "--property=Active",
                "--property=State",
                "--property=CanLock",
                "--property=LockedHint",
                "show-session",
                session,
            ],
        )
    }
}

#[derive(Debug, Error)]
pub enum LoginctlError {
    #[error("invalid {kind} identifier {value:?}")]
    InvalidIdentifier { kind: &'static str, value: String },
    #[error("could not start loginctl {subject} query: {source}")]
    Spawn {
        subject: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("loginctl {subject} query exceeded {timeout:?}")]
    Timeout {
        subject: &'static str,
        timeout: Duration,
    },
    #[error("could not wait for loginctl {subject} query: {source}")]
    Wait {
        subject: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("loginctl {subject} query returned too much output")]
    OutputTooLarge { subject: &'static str },
    #[error("loginctl {subject} query returned non-UTF-8 output")]
    InvalidUtf8 { subject: &'static str },
    #[error("loginctl {subject} query failed with status {status:?}: {stderr}")]
    Failed {
        subject: &'static str,
        status: Option<i32>,
        stderr: String,
    },
}

fn validate_identifier(kind: &'static str, value: &str) -> Result<(), LoginctlError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(LoginctlError::InvalidIdentifier {
            kind,
            value: value.to_owned(),
        });
    }
    Ok(())
}

/// Classifies the primary Linux seat. Query failure is represented as
/// `SeatState::Unknown`, which always denies input.
pub fn query_primary_seat() -> SeatState {
    inspect_seat(&LoginctlQuery::default(), PRIMARY_SEAT)
}

pub fn inspect_seat<Q: LogindQuery>(query: &Q, seat: &str) -> SeatState {
    let first_seat = match query.show_seat(seat) {
        Ok(output) => match parse_seat(&output, seat) {
            Ok(snapshot) => snapshot,
            Err(reason) => return SeatState::Unknown { reason },
        },
        Err(error) => return unknown_query("seat", error),
    };

    let Some(active_session) = first_seat.active_session.as_deref() else {
        let second_seat = match query.show_seat(seat) {
            Ok(output) => match parse_seat(&output, seat) {
                Ok(snapshot) => snapshot,
                Err(reason) => return SeatState::Unknown { reason },
            },
            Err(error) => return unknown_query("seat confirmation", error),
        };
        if second_seat.active_session.is_none() {
            return SeatState::Restricted(RestrictedSeatState::NoActiveSession);
        }
        return SeatState::Unknown {
            reason: "the active session changed while logind was queried".to_owned(),
        };
    };

    if !first_seat.sessions.contains(active_session) {
        return SeatState::Unknown {
            reason: format!(
                "logind names active session {active_session:?} but omits it from the seat session list"
            ),
        };
    }

    let first_session = match query.show_session(active_session) {
        Ok(output) => output,
        Err(error) => return unknown_query("session", error),
    };
    let second_seat = match query.show_seat(seat) {
        Ok(output) => match parse_seat(&output, seat) {
            Ok(snapshot) => snapshot,
            Err(reason) => return SeatState::Unknown { reason },
        },
        Err(error) => return unknown_query("seat confirmation", error),
    };
    if second_seat.active_session.as_deref() != Some(active_session)
        || !second_seat.sessions.contains(active_session)
    {
        return SeatState::Unknown {
            reason: "the active session changed while logind was queried".to_owned(),
        };
    }
    let second_session = match query.show_session(active_session) {
        Ok(output) => output,
        Err(error) => return unknown_query("session confirmation", error),
    };

    let first_properties = match parse_properties(&first_session) {
        Ok(properties) => properties,
        Err(reason) => return SeatState::Unknown { reason },
    };
    let second_properties = match parse_properties(&second_session) {
        Ok(properties) => properties,
        Err(reason) => return SeatState::Unknown { reason },
    };
    if first_properties != second_properties {
        return SeatState::Unknown {
            reason: "the active session properties changed while logind was queried".to_owned(),
        };
    }
    classify_session(&first_properties, seat, active_session)
        .unwrap_or_else(|reason| SeatState::Unknown { reason })
}

fn unknown_query(error_context: &str, error: impl fmt::Display) -> SeatState {
    SeatState::Unknown {
        reason: format!("logind {error_context} query failed: {error}"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SeatSnapshot {
    active_session: Option<String>,
    sessions: BTreeSet<String>,
}

fn parse_seat(output: &str, expected_seat: &str) -> Result<SeatSnapshot, String> {
    let properties = parse_properties(output)?;
    require_exact(&properties, "Id", expected_seat)?;
    let active_session = match require(&properties, "ActiveSession")? {
        "" => None,
        value => {
            validate_identifier("session", value).map_err(|error| error.to_string())?;
            Some(value.to_owned())
        }
    };
    let mut sessions = BTreeSet::new();
    for session in require(&properties, "Sessions")?.split_whitespace() {
        validate_identifier("session", session).map_err(|error| error.to_string())?;
        if !sessions.insert(session.to_owned()) {
            return Err(format!(
                "logind seat lists session {session:?} more than once"
            ));
        }
    }
    Ok(SeatSnapshot {
        active_session,
        sessions,
    })
}

fn classify_session(
    properties: &BTreeMap<String, String>,
    expected_seat: &str,
    expected_session: &str,
) -> Result<SeatState, String> {
    require_exact(properties, "Id", expected_session)?;
    require_exact(properties, "Seat", expected_seat)?;
    require_exact(properties, "Active", "yes")?;
    require_exact(properties, "State", "active")?;
    require_exact(properties, "Remote", "no")?;

    match require(properties, "Class")? {
        "greeter" => Ok(SeatState::Restricted(RestrictedSeatState::Greeter {
            session_id: expected_session.to_owned(),
        })),
        "lock-screen" => Ok(SeatState::Restricted(RestrictedSeatState::LockScreen {
            session_id: expected_session.to_owned(),
        })),
        "user" => classify_user_session(properties, expected_session),
        class => Err(format!(
            "active logind session has unsupported class {class:?}"
        )),
    }
}

fn classify_user_session(
    properties: &BTreeMap<String, String>,
    session_id: &str,
) -> Result<SeatState, String> {
    let uid = require(properties, "User")?
        .parse::<u32>()
        .map_err(|_| "active logind session has an invalid User property".to_owned())?;
    let locked = parse_bool(properties, "LockedHint")?;
    let kind = match require(properties, "Type")? {
        "tty" => {
            validate_vt(properties)?;
            AuthenticatedSessionKind::VirtualTerminal
        }
        "wayland" => {
            require_exact(properties, "CanLock", "yes")?;
            validate_vt(properties)?;
            AuthenticatedSessionKind::Wayland
        }
        session_type => {
            return Err(format!(
                "active user session has unsupported type {session_type:?}"
            ));
        }
    };
    let session = AuthenticatedSession {
        id: session_id.to_owned(),
        uid,
        kind,
    };
    if locked {
        Ok(SeatState::Restricted(RestrictedSeatState::LockedUser(
            session,
        )))
    } else {
        Ok(SeatState::Unlocked(session))
    }
}

fn validate_vt(properties: &BTreeMap<String, String>) -> Result<(), String> {
    let vt = require(properties, "VTNr")?
        .parse::<u32>()
        .map_err(|_| "active logind session has an invalid VTNr property".to_owned())?;
    if vt == 0 {
        return Err("active seat0 user session does not name a virtual terminal".to_owned());
    }
    require_exact(properties, "TTY", &format!("tty{vt}"))
}

fn parse_bool(properties: &BTreeMap<String, String>, name: &str) -> Result<bool, String> {
    match require(properties, name)? {
        "yes" => Ok(true),
        "no" => Ok(false),
        value => Err(format!(
            "logind property {name:?} has invalid boolean {value:?}"
        )),
    }
}

fn require<'a>(properties: &'a BTreeMap<String, String>, name: &str) -> Result<&'a str, String> {
    properties
        .get(name)
        .map(String::as_str)
        .ok_or_else(|| format!("logind output omits required property {name:?}"))
}

fn require_exact(
    properties: &BTreeMap<String, String>,
    name: &str,
    expected: &str,
) -> Result<(), String> {
    let actual = require(properties, name)?;
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "logind property {name:?} is {actual:?}, expected {expected:?}"
        ))
    }
}

fn parse_properties(output: &str) -> Result<BTreeMap<String, String>, String> {
    if output.len() > MAX_LOGINCTL_OUTPUT {
        return Err("logind output exceeds the size limit".to_owned());
    }
    let mut properties = BTreeMap::new();
    for line in output.lines() {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once('=')
            .ok_or_else(|| format!("malformed logind property line {line:?}"))?;
        if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(format!("invalid logind property name {name:?}"));
        }
        if properties
            .insert(name.to_owned(), value.to_owned())
            .is_some()
        {
            return Err(format!("logind property {name:?} appears more than once"));
        }
    }
    Ok(properties)
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::VecDeque, fs, os::unix::fs::PermissionsExt, path::Path};

    use super::*;

    const SEAT_ACTIVE: &str = "Id=seat0\nActiveSession=2\nSessions=2 c1\n";
    const WAYLAND: &str = "Id=2\nUser=1000\nVTNr=2\nSeat=seat0\nTTY=tty2\nRemote=no\nType=wayland\nClass=user\nActive=yes\nState=active\nCanLock=yes\nLockedHint=no\n";

    #[derive(Debug)]
    struct FakeQuery {
        seats: RefCell<VecDeque<Result<String, &'static str>>>,
        sessions: RefCell<VecDeque<Result<String, &'static str>>>,
    }

    impl FakeQuery {
        fn new(seats: &[&str], sessions: &[&str]) -> Self {
            Self {
                seats: RefCell::new(seats.iter().map(|value| Ok((*value).to_owned())).collect()),
                sessions: RefCell::new(
                    sessions
                        .iter()
                        .map(|value| Ok((*value).to_owned()))
                        .collect(),
                ),
            }
        }
    }

    impl LogindQuery for FakeQuery {
        type Error = &'static str;

        fn show_seat(&self, _seat: &str) -> Result<String, Self::Error> {
            self.seats
                .borrow_mut()
                .pop_front()
                .expect("unexpected seat query")
        }

        fn show_session(&self, _session: &str) -> Result<String, Self::Error> {
            self.sessions
                .borrow_mut()
                .pop_front()
                .expect("unexpected session query")
        }
    }

    fn inspect(seats: &[&str], sessions: &[&str]) -> SeatState {
        inspect_seat(&FakeQuery::new(seats, sessions), "seat0")
    }

    #[test]
    fn unlocked_wayland_session_grants_normal_input_and_ipc_uid() {
        let state = inspect(&[SEAT_ACTIVE, SEAT_ACTIVE], &[WAYLAND, WAYLAND]);
        assert_eq!(
            state,
            SeatState::Unlocked(AuthenticatedSession {
                id: "2".to_owned(),
                uid: 1000,
                kind: AuthenticatedSessionKind::Wayland,
            })
        );
        assert_eq!(state.injection_gate(), InjectionGate::Normal { uid: 1000 });
        assert_eq!(state.active_authenticated_uid(), Some(1000));
    }

    #[test]
    fn authenticated_active_vt_is_a_normal_session() {
        let tty = WAYLAND
            .replace("Type=wayland", "Type=tty")
            .replace("CanLock=yes", "CanLock=no");
        let state = inspect(&[SEAT_ACTIVE, SEAT_ACTIVE], &[&tty, &tty]);
        assert!(matches!(
            state,
            SeatState::Unlocked(AuthenticatedSession {
                kind: AuthenticatedSessionKind::VirtualTerminal,
                ..
            })
        ));
    }

    #[test]
    fn locked_user_requires_prelogin_permission_but_keeps_ipc_uid() {
        let locked = WAYLAND.replace("LockedHint=no", "LockedHint=yes");
        let state = inspect(&[SEAT_ACTIVE, SEAT_ACTIVE], &[&locked, &locked]);
        assert!(matches!(
            state,
            SeatState::Restricted(RestrictedSeatState::LockedUser(_))
        ));
        assert_eq!(state.injection_gate(), InjectionGate::PreLogin);
        assert_eq!(state.active_authenticated_uid(), Some(1000));
    }

    #[test]
    fn greeter_and_lock_screen_are_known_prelogin_states() {
        for class in ["greeter", "lock-screen"] {
            let pseudo = WAYLAND.replace("Class=user", &format!("Class={class}"));
            let state = inspect(&[SEAT_ACTIVE, SEAT_ACTIVE], &[&pseudo, &pseudo]);
            assert_eq!(state.injection_gate(), InjectionGate::PreLogin);
            assert_eq!(state.active_authenticated_uid(), None);
        }
    }

    #[test]
    fn no_active_logind_session_is_a_known_login_state() {
        let no_active = "Id=seat0\nActiveSession=\nSessions=2\n";
        let state = inspect(&[no_active, no_active], &[]);
        assert_eq!(
            state,
            SeatState::Restricted(RestrictedSeatState::NoActiveSession)
        );
        assert_eq!(state.injection_gate(), InjectionGate::PreLogin);
    }

    #[test]
    fn seat_or_session_change_fails_closed() {
        let other = "Id=seat0\nActiveSession=c1\nSessions=2 c1\n";
        let changed_session = WAYLAND.replace("LockedHint=no", "LockedHint=yes");
        for state in [
            inspect(&[SEAT_ACTIVE, other], &[WAYLAND]),
            inspect(&[SEAT_ACTIVE, SEAT_ACTIVE], &[WAYLAND, &changed_session]),
        ] {
            assert!(matches!(state, SeatState::Unknown { .. }));
            assert_eq!(state.injection_gate(), InjectionGate::Denied);
        }
    }

    #[test]
    fn contradictory_or_incomplete_properties_fail_closed() {
        let cases = [
            WAYLAND.replace("Remote=no", "Remote=yes"),
            WAYLAND.replace("State=active", "State=online"),
            WAYLAND.replace("CanLock=yes", "CanLock=no"),
            WAYLAND.replace("Type=wayland", "Type=x11"),
            WAYLAND.replace("Class=user", "Class=manager"),
            WAYLAND.replace("LockedHint=no\n", ""),
            WAYLAND.replace("TTY=tty2", "TTY=tty3"),
            format!("{WAYLAND}User=1000\n"),
        ];
        for session in cases {
            let state = inspect(&[SEAT_ACTIVE, SEAT_ACTIVE], &[&session, &session]);
            assert!(
                matches!(state, SeatState::Unknown { .. }),
                "unexpected state: {state:?}"
            );
            assert_eq!(state.injection_gate(), InjectionGate::Denied);
            assert_eq!(state.active_authenticated_uid(), None);
        }
    }

    #[test]
    fn active_session_must_belong_to_the_seat_session_list() {
        let contradictory = "Id=seat0\nActiveSession=2\nSessions=c1\n";
        let state = inspect(&[contradictory], &[]);
        assert!(matches!(state, SeatState::Unknown { .. }));
    }

    #[test]
    fn query_error_fails_closed() {
        let query = FakeQuery {
            seats: RefCell::new(VecDeque::from([Err("logind unavailable")])),
            sessions: RefCell::new(VecDeque::new()),
        };
        let state = inspect_seat(&query, "seat0");
        assert!(matches!(state, SeatState::Unknown { .. }));
        assert_eq!(state.injection_gate(), InjectionGate::Denied);
    }

    #[test]
    fn loginctl_runner_times_out() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("slow-loginctl");
        fs::write(&script, "#!/bin/sh\nsleep 1\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let query = LoginctlQuery::new(&script, Duration::from_millis(20));
        let error = query.show_seat("seat0").unwrap_err();
        assert!(matches!(error, LoginctlError::Timeout { .. }));
    }

    #[test]
    fn loginctl_runner_returns_bounded_machine_readable_output() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("fake-loginctl");
        fs::write(
            &script,
            "#!/bin/sh\nprintf 'Id=seat0\\nActiveSession=\\nSessions=\\n'\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let query = LoginctlQuery::new(&script, Duration::from_secs(1));
        assert_eq!(
            query.show_seat("seat0").unwrap(),
            "Id=seat0\nActiveSession=\nSessions=\n"
        );
    }

    #[test]
    fn loginctl_runner_rejects_identifiers_before_spawn() {
        let query = LoginctlQuery::new(Path::new("/does/not/exist"), Duration::from_secs(1));
        assert!(matches!(
            query.show_session("../../evil"),
            Err(LoginctlError::InvalidIdentifier { .. })
        ));
    }
}
