use std::{collections::BTreeMap, collections::BTreeSet, path::PathBuf};

use evdev::{EventType, InputEvent, KeyCode, SynchronizationCode};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipPhase {
    Idle,
    Arming,
    Remote,
    Releasing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipEffect {
    None,
    /// Take every configured EVIOCGRAB transactionally, then report the result
    /// with `grab_succeeded` or `grab_failed`.
    AcquireGrabs,
    /// Queue the reliable terminal state. Ungrabbing still waits for a complete
    /// physical SYN_REPORT boundary.
    QueueTerminal,
    /// Ungrab the complete capture set, then call `release_completed`.
    ReleaseGrabs,
    /// A lifecycle failure cannot wait for network or another input frame.
    /// Close the activation and ungrab immediately.
    CloseActivationAndReleaseGrabs,
    /// Cancel an activation which never acquired physical ownership.
    CancelActivation,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipError {
    #[error("activation can only be requested from Idle, not {0:?}")]
    CannotArm(OwnershipPhase),
    #[error("grab result is only valid while an acquisition is pending")]
    NoGrabPending,
    #[error("release completion is only valid while an ungrab is pending")]
    NoReleasePending,
    #[error("terminal delivery is not pending")]
    NoTerminalPending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceOwnership {
    phase: OwnershipPhase,
    grab_pending: bool,
    release_pending: bool,
    terminal_pending: bool,
    last_complete_boundary: u64,
    phase_transition_pending: bool,
}

impl Default for SourceOwnership {
    fn default() -> Self {
        Self {
            phase: OwnershipPhase::Idle,
            grab_pending: false,
            release_pending: false,
            terminal_pending: false,
            last_complete_boundary: 0,
            phase_transition_pending: false,
        }
    }
}

impl SourceOwnership {
    pub fn phase(&self) -> OwnershipPhase {
        self.phase
    }

    pub fn request_activation(&mut self) -> Result<OwnershipEffect, OwnershipError> {
        if self.phase != OwnershipPhase::Idle {
            return Err(OwnershipError::CannotArm(self.phase));
        }
        self.phase = OwnershipPhase::Arming;
        self.phase_transition_pending = true;
        Ok(OwnershipEffect::None)
    }

    /// Advances ownership only when every device is between complete evdev
    /// frames. Arming additionally needs both tracked aggregate state and a
    /// fresh EVIOCGKEY observation to be neutral.
    pub fn at_complete_boundary(
        &mut self,
        aggregate: &AggregateInputState,
        kernel_neutral: bool,
    ) -> OwnershipEffect {
        if !aggregate.all_at_boundary() {
            return OwnershipEffect::None;
        }
        let boundary = aggregate.boundary_generation();
        if boundary == self.last_complete_boundary && !self.phase_transition_pending {
            return OwnershipEffect::None;
        }
        self.last_complete_boundary = boundary;
        self.phase_transition_pending = false;
        match self.phase {
            OwnershipPhase::Arming
                if !self.grab_pending && aggregate.is_neutral() && kernel_neutral =>
            {
                self.grab_pending = true;
                OwnershipEffect::AcquireGrabs
            }
            OwnershipPhase::Releasing if !self.terminal_pending && !self.release_pending => {
                self.release_pending = true;
                OwnershipEffect::ReleaseGrabs
            }
            _ => OwnershipEffect::None,
        }
    }

    pub fn grab_succeeded(&mut self) -> Result<(), OwnershipError> {
        if self.phase != OwnershipPhase::Arming || !self.grab_pending {
            return Err(OwnershipError::NoGrabPending);
        }
        self.grab_pending = false;
        self.phase = OwnershipPhase::Remote;
        Ok(())
    }

    pub fn grab_failed(&mut self) -> Result<(), OwnershipError> {
        if self.phase != OwnershipPhase::Arming || !self.grab_pending {
            return Err(OwnershipError::NoGrabPending);
        }
        self.grab_pending = false;
        self.phase = OwnershipPhase::Idle;
        Ok(())
    }

    pub fn request_release(&mut self, transport_live: bool) -> OwnershipEffect {
        match self.phase {
            OwnershipPhase::Idle => OwnershipEffect::None,
            OwnershipPhase::Arming => {
                self.grab_pending = false;
                self.phase = OwnershipPhase::Idle;
                self.phase_transition_pending = false;
                OwnershipEffect::CancelActivation
            }
            OwnershipPhase::Remote => {
                self.phase = OwnershipPhase::Releasing;
                self.phase_transition_pending = true;
                if transport_live {
                    self.terminal_pending = true;
                    OwnershipEffect::QueueTerminal
                } else {
                    OwnershipEffect::None
                }
            }
            OwnershipPhase::Releasing => OwnershipEffect::None,
        }
    }

    /// Confirms that the reliable terminal state reached the transport write
    /// boundary. Physical grabs may be released only after this point.
    pub fn terminal_sent(&mut self) -> Result<(), OwnershipError> {
        if self.phase != OwnershipPhase::Releasing || !self.terminal_pending {
            return Err(OwnershipError::NoTerminalPending);
        }
        self.terminal_pending = false;
        self.phase_transition_pending = true;
        Ok(())
    }

    pub fn release_completed(&mut self) -> Result<(), OwnershipError> {
        if self.phase != OwnershipPhase::Releasing || !self.release_pending {
            return Err(OwnershipError::NoReleasePending);
        }
        self.release_pending = false;
        self.terminal_pending = false;
        self.phase = OwnershipPhase::Idle;
        self.phase_transition_pending = false;
        Ok(())
    }

    pub fn suspend(&mut self) -> OwnershipEffect {
        self.force_release()
    }

    pub fn device_removed(&mut self) -> OwnershipEffect {
        self.force_release()
    }

    fn force_release(&mut self) -> OwnershipEffect {
        match self.phase {
            OwnershipPhase::Idle => OwnershipEffect::None,
            OwnershipPhase::Arming => {
                self.grab_pending = false;
                self.phase = OwnershipPhase::Idle;
                self.phase_transition_pending = false;
                OwnershipEffect::CancelActivation
            }
            OwnershipPhase::Remote | OwnershipPhase::Releasing => {
                self.phase = OwnershipPhase::Releasing;
                self.terminal_pending = false;
                self.release_pending = true;
                self.phase_transition_pending = false;
                OwnershipEffect::CloseActivationAndReleaseGrabs
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct DeviceInputState {
    held: BTreeSet<KeyCode>,
    in_frame: bool,
}

/// Physical key/button state is kept per evdev node so overlapping composite
/// nodes do not release each other's keys. Neutrality is the union of all held
/// state, and ownership changes only while no node has a partial frame.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AggregateInputState {
    devices: BTreeMap<PathBuf, DeviceInputState>,
    boundary_generation: u64,
}

impl AggregateInputState {
    pub fn add_device(
        &mut self,
        path: impl Into<PathBuf>,
        initially_held: impl IntoIterator<Item = KeyCode>,
    ) {
        self.devices.insert(
            path.into(),
            DeviceInputState {
                held: initially_held.into_iter().collect(),
                in_frame: false,
            },
        );
    }

    pub fn remove_device(&mut self, path: &std::path::Path) -> bool {
        self.devices.remove(path).is_some()
    }

    pub fn replace_held(
        &mut self,
        path: &std::path::Path,
        held: impl IntoIterator<Item = KeyCode>,
    ) -> bool {
        let Some(device) = self.devices.get_mut(path) else {
            return false;
        };
        device.held = held.into_iter().collect();
        true
    }

    pub fn observe(&mut self, path: &std::path::Path, event: InputEvent) -> bool {
        let Some(device) = self.devices.get_mut(path) else {
            return false;
        };
        match event.event_type() {
            EventType::SYNCHRONIZATION
                if SynchronizationCode(event.code()) == SynchronizationCode::SYN_REPORT =>
            {
                device.in_frame = false;
                self.boundary_generation = self.boundary_generation.wrapping_add(1);
            }
            EventType::SYNCHRONIZATION => {}
            EventType::KEY => {
                device.in_frame = true;
                let key = KeyCode::new(event.code());
                match event.value() {
                    0 => {
                        device.held.remove(&key);
                    }
                    1 | 2 => {
                        device.held.insert(key);
                    }
                    _ => {}
                }
            }
            _ => device.in_frame = true,
        }
        true
    }

    pub fn is_neutral(&self) -> bool {
        self.devices.values().all(|device| device.held.is_empty())
    }

    pub fn all_at_boundary(&self) -> bool {
        self.devices.values().all(|device| !device.in_frame)
    }

    pub fn boundary_generation(&self) -> u64 {
        self.boundary_generation
    }

    pub fn held_on(&self, path: &std::path::Path) -> Option<&BTreeSet<KeyCode>> {
        self.devices.get(path).map(|device| &device.held)
    }

    pub fn len(&self) -> usize {
        self.devices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, value: i32) -> InputEvent {
        InputEvent::new(EventType::KEY.0, code.code(), value)
    }

    fn report() -> InputEvent {
        InputEvent::new(
            EventType::SYNCHRONIZATION.0,
            SynchronizationCode::SYN_REPORT.0,
            0,
        )
    }

    #[test]
    fn arming_waits_for_aggregate_neutral_and_complete_frames() {
        let one = std::path::Path::new("/dev/input/event1");
        let two = std::path::Path::new("/dev/input/event2");
        let mut aggregate = AggregateInputState::default();
        aggregate.add_device(one, [KeyCode::KEY_A]);
        aggregate.add_device(two, []);
        let mut ownership = SourceOwnership::default();
        ownership.request_activation().unwrap();

        assert_eq!(
            ownership.at_complete_boundary(&aggregate, false),
            OwnershipEffect::None
        );
        aggregate.observe(one, key(KeyCode::KEY_A, 0));
        assert!(!aggregate.all_at_boundary());
        assert_eq!(
            ownership.at_complete_boundary(&aggregate, true),
            OwnershipEffect::None
        );
        aggregate.observe(one, report());
        assert_eq!(
            ownership.at_complete_boundary(&aggregate, true),
            OwnershipEffect::AcquireGrabs
        );
        ownership.grab_succeeded().unwrap();
        assert_eq!(ownership.phase(), OwnershipPhase::Remote);
    }

    #[test]
    fn failed_grab_returns_to_idle() {
        let mut ownership = SourceOwnership::default();
        let path = std::path::Path::new("/dev/input/event1");
        let mut aggregate = AggregateInputState::default();
        aggregate.add_device(path, []);
        ownership.request_activation().unwrap();
        aggregate.observe(path, report());
        assert_eq!(
            ownership.at_complete_boundary(&aggregate, true),
            OwnershipEffect::AcquireGrabs
        );
        ownership.grab_failed().unwrap();
        assert_eq!(ownership.phase(), OwnershipPhase::Idle);
    }

    #[test]
    fn graceful_release_queues_terminal_then_waits_for_boundary() {
        let path = std::path::Path::new("/dev/input/event1");
        let mut aggregate = AggregateInputState::default();
        aggregate.add_device(path, []);
        let mut ownership = SourceOwnership::default();
        ownership.request_activation().unwrap();
        aggregate.observe(path, report());
        assert_eq!(
            ownership.at_complete_boundary(&aggregate, true),
            OwnershipEffect::AcquireGrabs
        );
        ownership.grab_succeeded().unwrap();

        assert_eq!(
            ownership.request_release(true),
            OwnershipEffect::QueueTerminal
        );
        aggregate.observe(path, key(KeyCode::KEY_ESC, 1));
        assert_eq!(
            ownership.at_complete_boundary(&aggregate, true),
            OwnershipEffect::None
        );
        aggregate.observe(path, report());
        assert_eq!(
            ownership.at_complete_boundary(&aggregate, true),
            OwnershipEffect::None
        );
        ownership.terminal_sent().unwrap();
        assert_eq!(
            ownership.at_complete_boundary(&aggregate, true),
            OwnershipEffect::ReleaseGrabs
        );
        ownership.release_completed().unwrap();
        assert_eq!(ownership.phase(), OwnershipPhase::Idle);
    }

    #[test]
    fn quiescent_remote_release_uses_the_current_complete_boundary() {
        let path = std::path::Path::new("/dev/input/event1");
        let mut aggregate = AggregateInputState::default();
        aggregate.add_device(path, []);
        let mut ownership = SourceOwnership::default();
        ownership.request_activation().unwrap();
        assert_eq!(
            ownership.at_complete_boundary(&aggregate, true),
            OwnershipEffect::AcquireGrabs
        );
        ownership.grab_succeeded().unwrap();

        assert_eq!(
            ownership.request_release(true),
            OwnershipEffect::QueueTerminal
        );
        assert_eq!(
            ownership.at_complete_boundary(&aggregate, true),
            OwnershipEffect::None
        );
        ownership.terminal_sent().unwrap();
        assert_eq!(
            ownership.at_complete_boundary(&aggregate, true),
            OwnershipEffect::ReleaseGrabs
        );
        ownership.release_completed().unwrap();
        assert_eq!(ownership.phase(), OwnershipPhase::Idle);
    }

    #[test]
    fn suspend_and_removal_force_remote_release() {
        let mut ownership = SourceOwnership {
            phase: OwnershipPhase::Remote,
            ..SourceOwnership::default()
        };
        assert_eq!(
            ownership.suspend(),
            OwnershipEffect::CloseActivationAndReleaseGrabs
        );
        ownership.release_completed().unwrap();
        assert_eq!(ownership.phase(), OwnershipPhase::Idle);

        ownership.request_activation().unwrap();
        assert_eq!(
            ownership.device_removed(),
            OwnershipEffect::CancelActivation
        );
    }
}
