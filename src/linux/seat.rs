use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    sync::OnceLock,
    time::Duration,
};

use thiserror::Error;
use tokio::{runtime::Runtime, sync::Mutex, time::timeout_at};
use zbus::{
    Connection,
    zvariant::{OwnedObjectPath, OwnedValue},
};

const PRIMARY_SEAT: &str = "seat0";
const SYSTEM_BUS_ADDRESS: &str = "unix:path=/run/dbus/system_bus_socket";
const LOGIND: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";

const DEFAULT_QUERY_TIMEOUT: Duration = Duration::from_millis(200);

type Properties = BTreeMap<String, String>;

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

    fn show_seat(&self, seat: &str) -> Result<Properties, Self::Error>;
    fn show_session(&self, session: &str) -> Result<Properties, Self::Error>;
}

struct LogindBusQuery {
    runtime: Runtime,
    connection: Mutex<Option<Connection>>,
    address: String,
    timeout: Duration,
}

impl LogindBusQuery {
    fn new(address: &str, timeout: Duration) -> std::io::Result<Self> {
        Ok(Self {
            runtime: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("zflow-logind")
                .enable_all()
                .build()?,
            connection: Mutex::new(None),
            address: address.to_owned(),
            timeout,
        })
    }

    fn query(&self, kind: &str, id: &str) -> Result<HashMap<String, OwnedValue>, LogindError> {
        self.runtime.block_on(async {
            let deadline = tokio::time::Instant::now() + self.timeout;
            let mut connection = timeout_at(deadline, self.connection.lock())
                .await
                .map_err(|_| LogindError::Timeout(self.timeout))?;
            let result = timeout_at(deadline, async {
                if connection.is_none() {
                    *connection = Some(
                        zbus::connection::Builder::address(self.address.as_str())?
                            .build()
                            .await?,
                    );
                }
                let connection = connection.as_ref().unwrap();
                let path: OwnedObjectPath = connection
                    .call_method(
                        Some(LOGIND),
                        MANAGER_PATH,
                        Some("org.freedesktop.login1.Manager"),
                        format!("Get{kind}").as_str(),
                        &(id,),
                    )
                    .await?
                    .body()
                    .deserialize()?;
                // GetAll bypasses proxy property caches: both confirmation reads
                // must observe logind, including lock changes without a signal.
                let properties = connection
                    .call_method(
                        Some(LOGIND),
                        path.as_str(),
                        Some("org.freedesktop.DBus.Properties"),
                        "GetAll",
                        &(format!("org.freedesktop.login1.{kind}"),),
                    )
                    .await?
                    .body()
                    .deserialize()?;
                Ok::<_, zbus::Error>(properties)
            })
            .await;
            let result = result
                .map_err(|_| LogindError::Timeout(self.timeout))
                .and_then(|result| result.map_err(LogindError::Bus));
            if result.is_err() {
                // A lost bus or cancelled request must not poison later polls.
                *connection = None;
            }
            result
        })
    }
}

impl LogindQuery for LogindBusQuery {
    type Error = LogindError;

    fn show_seat(&self, seat: &str) -> Result<Properties, Self::Error> {
        validate_identifier("seat", seat)?;
        seat_properties(self.query("Seat", seat)?)
    }

    fn show_session(&self, session: &str) -> Result<Properties, Self::Error> {
        validate_identifier("session", session)?;
        session_properties(self.query("Session", session)?)
    }
}

fn property<T>(values: &mut HashMap<String, OwnedValue>, name: &str) -> Result<T, LogindError>
where
    T: TryFrom<OwnedValue>,
    T::Error: fmt::Display,
{
    let value = values.remove(name).ok_or_else(|| LogindError::Property {
        name: name.to_owned(),
        reason: "missing".into(),
    })?;
    T::try_from(value).map_err(|error| LogindError::Property {
        name: name.to_owned(),
        reason: error.to_string(),
    })
}

fn seat_properties(mut values: HashMap<String, OwnedValue>) -> Result<Properties, LogindError> {
    let id: String = property(&mut values, "Id")?;
    let (active, _): (String, OwnedObjectPath) = property(&mut values, "ActiveSession")?;
    let sessions: Vec<(String, OwnedObjectPath)> = property(&mut values, "Sessions")?;
    for (session, _) in &sessions {
        validate_identifier("session", session)?;
    }
    Ok(BTreeMap::from([
        ("Id".into(), id),
        ("ActiveSession".into(), active),
        (
            "Sessions".into(),
            sessions
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>()
                .join(" "),
        ),
    ]))
}

fn session_properties(mut values: HashMap<String, OwnedValue>) -> Result<Properties, LogindError> {
    let mut properties = Properties::new();
    for name in ["Id", "TTY", "Type", "Class", "State"] {
        properties.insert(name.into(), property::<String>(&mut values, name)?);
    }
    for name in ["Remote", "Active", "CanLock", "LockedHint"] {
        properties.insert(
            name.into(),
            if property::<bool>(&mut values, name)? {
                "yes"
            } else {
                "no"
            }
            .into(),
        );
    }
    let (uid, _): (u32, OwnedObjectPath) = property(&mut values, "User")?;
    let (seat, _): (String, OwnedObjectPath) = property(&mut values, "Seat")?;
    properties.insert("User".into(), uid.to_string());
    properties.insert("Seat".into(), seat);
    properties.insert(
        "VTNr".into(),
        property::<u32>(&mut values, "VTNr")?.to_string(),
    );
    Ok(properties)
}

#[derive(Debug, Error)]
pub enum LogindError {
    #[error("invalid {kind} identifier {value:?}")]
    InvalidIdentifier { kind: &'static str, value: String },
    #[error("system bus query exceeded {0:?}")]
    Timeout(Duration),
    #[error("system bus query failed: {0}")]
    Bus(#[source] zbus::Error),
    #[error("logind property {name:?} is invalid: {reason}")]
    Property { name: String, reason: String },
}

fn validate_identifier(kind: &'static str, value: &str) -> Result<(), LogindError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(LogindError::InvalidIdentifier {
            kind,
            value: value.to_owned(),
        });
    }
    Ok(())
}

/// Classifies the primary Linux seat. Query failure is represented as
/// `SeatState::Unknown`, which always denies input.
pub fn query_primary_seat() -> SeatState {
    static QUERY: OnceLock<Result<LogindBusQuery, std::io::Error>> = OnceLock::new();
    // Use the system socket directly, as the old env-cleared loginctl did.
    // User-provided bus-address environment variables cannot authorize input.
    match QUERY.get_or_init(|| LogindBusQuery::new(SYSTEM_BUS_ADDRESS, DEFAULT_QUERY_TIMEOUT)) {
        Ok(query) => inspect_seat(query, PRIMARY_SEAT),
        Err(error) => unknown_query("runtime", error),
    }
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

    if first_session != second_session {
        return SeatState::Unknown {
            reason: "the active session properties changed while logind was queried".to_owned(),
        };
    }
    classify_session(&first_session, seat, active_session)
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

fn parse_seat(properties: &Properties, expected_seat: &str) -> Result<SeatSnapshot, String> {
    require_exact(properties, "Id", expected_seat)?;
    let active_session = match require(properties, "ActiveSession")? {
        "" => None,
        value => {
            validate_identifier("session", value).map_err(|error| error.to_string())?;
            Some(value.to_owned())
        }
    };
    let mut sessions = BTreeSet::new();
    for session in require(properties, "Sessions")?.split_whitespace() {
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

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::VecDeque};

    use super::*;

    fn parse_properties(output: &str) -> Result<BTreeMap<String, String>, String> {
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
        type Error = String;

        fn show_seat(&self, _seat: &str) -> Result<Properties, Self::Error> {
            self.seats
                .borrow_mut()
                .pop_front()
                .expect("unexpected seat query")
                .map_err(str::to_owned)
                .and_then(|output| parse_properties(&output))
        }

        fn show_session(&self, _session: &str) -> Result<Properties, Self::Error> {
            self.sessions
                .borrow_mut()
                .pop_front()
                .expect("unexpected session query")
                .map_err(str::to_owned)
                .and_then(|output| parse_properties(&output))
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

    fn bus_properties(text: &str) -> HashMap<String, OwnedValue> {
        use zbus::zvariant::{ObjectPath, Value};
        let path = ObjectPath::try_from("/org/freedesktop/login1/test").unwrap();
        parse_properties(text)
            .unwrap()
            .into_iter()
            .map(|(name, value)| {
                let value = match name.as_str() {
                    "ActiveSession" | "Seat" => Value::from((value, path.clone())),
                    "Sessions" => Value::from(
                        value
                            .split_whitespace()
                            .map(|id| (id.to_owned(), path.clone()))
                            .collect::<Vec<_>>(),
                    ),
                    "User" => Value::from((value.parse::<u32>().unwrap(), path.clone())),
                    "VTNr" => Value::from(value.parse::<u32>().unwrap()),
                    "Remote" | "Active" | "CanLock" | "LockedHint" => Value::from(value == "yes"),
                    _ => Value::from(value),
                };
                (name, value.try_into().unwrap())
            })
            .collect()
    }

    #[test]
    fn typed_bus_properties_preserve_seat_and_session_classification() {
        let seat = seat_properties(bus_properties(SEAT_ACTIVE)).unwrap();
        let session = session_properties(bus_properties(WAYLAND)).unwrap();
        assert_eq!(seat, parse_properties(SEAT_ACTIVE).unwrap());
        assert_eq!(session, parse_properties(WAYLAND).unwrap());
        assert_eq!(
            classify_session(&session, "seat0", "2")
                .unwrap()
                .injection_gate(),
            InjectionGate::Normal { uid: 1000 }
        );
        let no_active = "Id=seat0\nActiveSession=\nSessions=\n";
        assert_eq!(
            seat_properties(bus_properties(no_active)).unwrap(),
            parse_properties(no_active).unwrap()
        );
    }

    #[test]
    fn wrong_bus_types_and_missing_security_properties_are_rejected() {
        for name in ["LockedHint", "Remote", "User", "Seat", "VTNr"] {
            let mut missing = bus_properties(WAYLAND);
            missing.remove(name);
            assert!(session_properties(missing).is_err(), "missing {name}");
            let mut wrong = bus_properties(WAYLAND);
            wrong.insert(name.into(), OwnedValue::from(17u8));
            assert!(session_properties(wrong).is_err(), "wrong type for {name}");
        }
        for name in ["ActiveSession", "Sessions"] {
            let mut wrong = bus_properties(SEAT_ACTIVE);
            wrong.insert(name.into(), OwnedValue::from(17u8));
            assert!(seat_properties(wrong).is_err(), "wrong type for {name}");
        }
    }

    #[test]
    fn bus_authentication_timeout_denies_input_and_reconnects() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bus");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let query = LogindBusQuery::new(
            &format!("unix:path={}", path.display()),
            Duration::from_millis(20),
        )
        .unwrap();
        // A listening socket that never answers authentication must be bounded,
        // too. Each failed poll must attempt a fresh connection.
        for _ in 0..2 {
            let started = std::time::Instant::now();
            let error = query.show_seat("seat0").unwrap_err();
            assert!(matches!(error, LogindError::Timeout(_)), "{error}");
            assert!(started.elapsed() < Duration::from_secs(1));
            assert_eq!(
                unknown_query("seat", error).injection_gate(),
                InjectionGate::Denied
            );
            assert!(query.connection.blocking_lock().is_none());
            let _ = listener.accept().unwrap();
        }
    }

    #[test]
    fn waiting_for_an_inflight_bus_query_is_also_bounded() {
        let query =
            LogindBusQuery::new("unix:path=/does/not/exist", Duration::from_millis(20)).unwrap();
        let _guard = query.connection.blocking_lock();
        let error = query.show_seat("seat0").unwrap_err();
        assert!(matches!(error, LogindError::Timeout(_)));
    }

    #[test]
    fn invalid_identifier_and_unavailable_bus_deny_input() {
        let query =
            LogindBusQuery::new("unix:path=/does/not/exist", Duration::from_millis(20)).unwrap();
        assert!(matches!(
            query.show_session("../../evil"),
            Err(LogindError::InvalidIdentifier { .. })
        ));
        assert_eq!(
            inspect_seat(&query, "seat0").injection_gate(),
            InjectionGate::Denied
        );
    }

    #[test]
    #[ignore = "requires a local system bus, logind seat0 and loginctl"]
    fn live_logind_matches_loginctl_and_reuses_connection() {
        struct Cli;
        impl LogindQuery for Cli {
            type Error = String;
            fn show_seat(&self, seat: &str) -> Result<Properties, String> {
                cli_query("show-seat", seat, "Id,ActiveSession,Sessions")
            }
            fn show_session(&self, session: &str) -> Result<Properties, String> {
                cli_query(
                    "show-session",
                    session,
                    "Id,User,VTNr,Seat,TTY,Remote,Type,Class,Active,State,CanLock,LockedHint",
                )
            }
        }
        fn cli_query(kind: &str, id: &str, fields: &str) -> Result<Properties, String> {
            let output = std::process::Command::new("/usr/bin/loginctl")
                .args(["--no-pager", "--all", kind, id])
                .args(fields.split(',').map(|field| format!("--property={field}")))
                .env_clear()
                .env("LANG", "C")
                .output()
                .map_err(|e| e.to_string())?;
            if !output.status.success() {
                return Err(String::from_utf8_lossy(&output.stderr).into_owned());
            }
            parse_properties(&String::from_utf8(output.stdout).map_err(|e| e.to_string())?)
        }
        let query = LogindBusQuery::new(SYSTEM_BUS_ADDRESS, DEFAULT_QUERY_TIMEOUT).unwrap();
        let expected = inspect_seat(&Cli, PRIMARY_SEAT);
        assert!(
            !matches!(expected, SeatState::Unknown { .. }),
            "{expected:?}"
        );
        assert_eq!(inspect_seat(&query, PRIMARY_SEAT), expected);
        let unique_name = query
            .connection
            .blocking_lock()
            .as_ref()
            .unwrap()
            .unique_name()
            .unwrap()
            .to_owned();
        let started = std::time::Instant::now();
        for _ in 0..20 {
            assert_eq!(inspect_seat(&query, PRIMARY_SEAT), expected);
            assert_eq!(
                query
                    .connection
                    .blocking_lock()
                    .as_ref()
                    .unwrap()
                    .unique_name(),
                Some(&unique_name)
            );
        }
        let bus_elapsed = started.elapsed();
        let closed = query.connection.blocking_lock().as_ref().unwrap().clone();
        query.runtime.block_on(closed.close()).unwrap();
        assert_eq!(
            inspect_seat(&query, PRIMARY_SEAT).injection_gate(),
            InjectionGate::Denied
        );
        assert!(query.connection.blocking_lock().is_none());
        assert_eq!(inspect_seat(&query, PRIMARY_SEAT), expected);
        assert_ne!(
            query
                .connection
                .blocking_lock()
                .as_ref()
                .unwrap()
                .unique_name(),
            Some(&unique_name)
        );
        let started = std::time::Instant::now();
        for _ in 0..20 {
            assert_eq!(inspect_seat(&Cli, PRIMARY_SEAT), expected);
        }
        eprintln!(
            "20 seat inspections: D-Bus {bus_elapsed:?}, loginctl {:?}; state {expected:?}",
            started.elapsed()
        );
    }
}
