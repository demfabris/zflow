//! Pure domain types shared by protocol, simulator, and platform adapters.
//!
//! This module deliberately models semantics rather than a wire encoding. The
//! encoding is still an open protocol decision, while the channel split and
//! state carried by each family are fixed by the specification.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProtocolVersion(pub u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionEpoch(pub [u8; 16]);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TransportGeneration(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ActivationId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ControlSequence(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MotionSequence(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProbeSequence(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ScrollId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ContactId(pub u32);

/// A timestamp from one process's monotonic clock, with no shared origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MonotonicTimeMicros(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionContext {
    pub protocol_version: ProtocolVersion,
    pub session_epoch: SessionEpoch,
    pub transport_generation: TransportGeneration,
    pub activation_id: ActivationId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HidUsagePage(pub u16);

impl HidUsagePage {
    pub const KEYBOARD_KEYPAD: Self = Self(0x07);
    pub const CONSUMER: Self = Self(0x0c);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HidUsageId(pub u16);

/// A physical key or control expressed in USB HID vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HidUsage {
    pub page: HidUsagePage,
    pub usage: HidUsageId,
}

impl HidUsage {
    pub const fn new(page: HidUsagePage, usage: u16) -> Self {
        Self {
            page,
            usage: HidUsageId(usage),
        }
    }

    pub const fn keyboard(usage: u16) -> Self {
        Self::new(HidUsagePage::KEYBOARD_KEYPAD, usage)
    }

    pub const fn consumer(usage: u16) -> Self {
        Self::new(HidUsagePage::CONSUMER, usage)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PointerButton(pub u16);

impl PointerButton {
    pub const PRIMARY: Self = Self(1);
    pub const SECONDARY: Self = Self(2);
    pub const MIDDLE: Self = Self(3);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Modifier {
    LeftControl,
    LeftShift,
    LeftAlt,
    LeftMeta,
    RightControl,
    RightShift,
    RightAlt,
    RightMeta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum InputCapability {
    Keyboard,
    ConsumerControls,
    Pointer,
    Scroll,
    Touch,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InputCapabilities(BTreeSet<InputCapability>);

impl InputCapabilities {
    pub fn new(capabilities: impl IntoIterator<Item = InputCapability>) -> Self {
        Self(capabilities.into_iter().collect())
    }

    pub fn contains(&self, capability: InputCapability) -> bool {
        self.0.contains(&capability)
    }

    pub fn insert(&mut self, capability: InputCapability) -> bool {
        self.0.insert(capability)
    }

    pub fn is_superset(&self, other: &Self) -> bool {
        self.0.is_superset(&other.0)
    }

    pub fn iter(&self) -> impl Iterator<Item = InputCapability> + '_ {
        self.0.iter().copied()
    }
}

impl<const N: usize> From<[InputCapability; N]> for InputCapabilities {
    fn from(value: [InputCapability; N]) -> Self {
        Self::new(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum PeerPermission {
    Connect,
    ReceiveNormalSessionInput,
    SendNormalSessionInput,
    InjectBeforeLogin,
    Clipboard,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PeerPermissions(BTreeSet<PeerPermission>);

impl PeerPermissions {
    pub fn new(permissions: impl IntoIterator<Item = PeerPermission>) -> Self {
        Self(permissions.into_iter().collect())
    }

    pub fn contains(&self, permission: PeerPermission) -> bool {
        self.0.contains(&permission)
    }

    pub fn grant(&mut self, permission: PeerPermission) -> bool {
        self.0.insert(permission)
    }

    pub fn revoke(&mut self, permission: PeerPermission) -> bool {
        self.0.remove(&permission)
    }

    pub fn iter(&self) -> impl Iterator<Item = PeerPermission> + '_ {
        self.0.iter().copied()
    }
}

impl<const N: usize> From<[PeerPermission; N]> for PeerPermissions {
    fn from(value: [PeerPermission; N]) -> Self {
        Self::new(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum PointerUnit {
    /// Device-like motion which the target input stack accelerates.
    DeviceUnaccelerated,
    /// Motion already accelerated by the source desktop.
    DesktopAccelerated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ScrollUnit {
    Device,
    DiscreteStep,
    Line,
    Pixel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrollSource {
    pub unit: ScrollUnit,
    /// Source units represented by one whole unit, when known.
    pub resolution_x: Option<u32>,
    pub resolution_y: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ScrollFields {
    pub high_resolution: bool,
    pub source_unit: bool,
    pub source_resolution: bool,
    pub discrete_steps: bool,
    pub phase: bool,
    pub momentum_phase: bool,
}

impl ScrollFields {
    pub fn is_subset_of(self, supported: Self) -> bool {
        (!self.high_resolution || supported.high_resolution)
            && (!self.source_unit || supported.source_unit)
            && (!self.source_resolution || supported.source_resolution)
            && (!self.discrete_steps || supported.discrete_steps)
            && (!self.phase || supported.phase)
            && (!self.momentum_phase || supported.momentum_phase)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrollPhase {
    Begin,
    Update,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MomentumPhase {
    Begin,
    Update,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveScroll {
    pub id: ScrollId,
    pub source: ScrollSource,
    pub phase: ScrollPhase,
    pub momentum_phase: Option<MomentumPhase>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TouchTool {
    Finger,
    Stylus,
    Eraser,
    Palm,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDimensions {
    pub width: u32,
    pub height: u32,
}

/// Integer source coordinates keep the logical model exact and codec-neutral.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TouchContact {
    pub id: ContactId,
    pub x: i32,
    pub y: i32,
    pub pressure: Option<u16>,
    pub major: Option<u32>,
    pub minor: Option<u32>,
    pub orientation_millidegrees: Option<i32>,
    pub tool: TouchTool,
    pub source_dimensions: Option<SourceDimensions>,
}

/// A complete touch state at one capture point.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TouchState(BTreeMap<ContactId, TouchContact>);

impl TouchState {
    pub fn new(contacts: impl IntoIterator<Item = TouchContact>) -> Result<Self, ContactId> {
        let mut state = Self::default();
        for contact in contacts {
            let id = contact.id;
            if state.0.insert(id, contact).is_some() {
                return Err(id);
            }
        }
        Ok(state)
    }

    pub fn get(&self, id: ContactId) -> Option<&TouchContact> {
        self.0.get(&id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &TouchContact> {
        self.0.values()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HeldState {
    pub pressed_keys: BTreeSet<HidUsage>,
    pub pressed_buttons: BTreeSet<PointerButton>,
    pub modifiers: BTreeSet<Modifier>,
    pub active_scroll: Option<ActiveScroll>,
    pub active_touch: TouchState,
}

impl HeldState {
    pub fn is_neutral(&self) -> bool {
        self.pressed_keys.is_empty()
            && self.pressed_buttons.is_empty()
            && self.modifiers.is_empty()
            && self.active_scroll.is_none()
            && self.active_touch.is_empty()
    }

    pub fn press_key(&mut self, key: HidUsage) -> bool {
        self.pressed_keys.insert(key)
    }

    pub fn release_key(&mut self, key: HidUsage) -> bool {
        self.pressed_keys.remove(&key)
    }

    pub fn press_button(&mut self, button: PointerButton) -> bool {
        self.pressed_buttons.insert(button)
    }

    pub fn release_button(&mut self, button: PointerButton) -> bool {
        self.pressed_buttons.remove(&button)
    }

    pub fn set_modifier(&mut self, modifier: Modifier, held: bool) -> bool {
        if held {
            self.modifiers.insert(modifier)
        } else {
            self.modifiers.remove(&modifier)
        }
    }

    pub fn begin_scroll(&mut self, scroll: ActiveScroll) -> Option<ActiveScroll> {
        self.active_scroll.replace(scroll)
    }

    pub fn end_scroll(&mut self, id: ScrollId) -> bool {
        if self.active_scroll.is_some_and(|scroll| scroll.id == id) {
            self.active_scroll = None;
            true
        } else {
            false
        }
    }

    pub fn replace_touch(&mut self, touch: TouchState) -> TouchState {
        std::mem::replace(&mut self.active_touch, touch)
    }

    pub fn clear_touch(&mut self) -> TouchState {
        std::mem::take(&mut self.active_touch)
    }

    pub fn release_all(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MotionDelta {
    pub dx: i64,
    pub dy: i64,
    pub scroll_x: i64,
    pub scroll_y: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CumulativeMotion {
    total_dx: i64,
    total_dy: i64,
    total_scroll_x: i64,
    total_scroll_y: i64,
}

impl CumulativeMotion {
    pub const ZERO: Self = Self {
        total_dx: 0,
        total_dy: 0,
        total_scroll_x: 0,
        total_scroll_y: 0,
    };

    pub const fn new(
        total_dx: i64,
        total_dy: i64,
        total_scroll_x: i64,
        total_scroll_y: i64,
    ) -> Self {
        Self {
            total_dx,
            total_dy,
            total_scroll_x,
            total_scroll_y,
        }
    }

    pub const fn total_dx(self) -> i64 {
        self.total_dx
    }

    pub const fn total_dy(self) -> i64 {
        self.total_dy
    }

    pub const fn total_scroll_x(self) -> i64 {
        self.total_scroll_x
    }

    pub const fn total_scroll_y(self) -> i64 {
        self.total_scroll_y
    }

    pub fn checked_add(self, delta: MotionDelta) -> Result<Self, MotionOverflow> {
        Ok(Self {
            total_dx: checked_add(self.total_dx, delta.dx, MotionAxis::PointerX)?,
            total_dy: checked_add(self.total_dy, delta.dy, MotionAxis::PointerY)?,
            total_scroll_x: checked_add(self.total_scroll_x, delta.scroll_x, MotionAxis::ScrollX)?,
            total_scroll_y: checked_add(self.total_scroll_y, delta.scroll_y, MotionAxis::ScrollY)?,
        })
    }

    /// Advances all axes atomically: on overflow `self` remains unchanged.
    pub fn checked_advance(&mut self, delta: MotionDelta) -> Result<(), MotionOverflow> {
        let next = self.checked_add(delta)?;
        *self = next;
        Ok(())
    }

    /// Computes the displacement needed to reconcile `previous` to `self`.
    pub fn checked_delta_from(self, previous: Self) -> Result<MotionDelta, MotionOverflow> {
        Ok(MotionDelta {
            dx: checked_sub(self.total_dx, previous.total_dx, MotionAxis::PointerX)?,
            dy: checked_sub(self.total_dy, previous.total_dy, MotionAxis::PointerY)?,
            scroll_x: checked_sub(
                self.total_scroll_x,
                previous.total_scroll_x,
                MotionAxis::ScrollX,
            )?,
            scroll_y: checked_sub(
                self.total_scroll_y,
                previous.total_scroll_y,
                MotionAxis::ScrollY,
            )?,
        })
    }
}

fn checked_add(left: i64, right: i64, axis: MotionAxis) -> Result<i64, MotionOverflow> {
    left.checked_add(right).ok_or(MotionOverflow { axis })
}

fn checked_sub(left: i64, right: i64, axis: MotionAxis) -> Result<i64, MotionOverflow> {
    left.checked_sub(right).ok_or(MotionOverflow { axis })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MotionAxis {
    PointerX,
    PointerY,
    ScrollX,
    ScrollY,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MotionOverflow {
    pub axis: MotionAxis,
}

impl std::fmt::Display for MotionOverflow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "cumulative {:?} motion overflowed", self.axis)
    }
}

impl std::error::Error for MotionOverflow {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnchorKind {
    Checkpoint,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MotionAnchor {
    pub activation_id: ActivationId,
    pub through_motion_sequence: MotionSequence,
    pub sender_capture_time: MonotonicTimeMicros,
    pub totals: CumulativeMotion,
    pub final_touch_state: TouchState,
    pub kind: AnchorKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub held: HeldState,
    pub motion_anchor: MotionAnchor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotAck {
    pub snapshot_sequence: ControlSequence,
    pub accepted_generation: TransportGeneration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TakeoverNonce(pub [u8; 16]);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTakeover {
    pub prior_generation: TransportGeneration,
    pub proposed_generation: TransportGeneration,
    pub proposal_nonce: TakeoverNonce,
    pub last_control_sequence: ControlSequence,
    pub final_motion_anchor: MotionAnchor,
    pub authoritative_held_state: HeldState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TakeoverAccepted {
    pub accepted_generation: TransportGeneration,
    pub proposal_nonce: TakeoverNonce,
    pub receiver_lease_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionCloseReason {
    LocalRelease,
    LeaseExpired,
    Superseded,
    PermissionRevoked,
    ProtocolViolation,
    MotionOverflow,
    BackendUnavailable,
    Suspend,
}

/// Payloads for the one reliable, ordered input-control stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReliableControl {
    Enter,
    Leave {
        anchor: MotionAnchor,
    },
    KeyDown {
        key: HidUsage,
    },
    KeyUp {
        key: HidUsage,
    },
    ButtonDown {
        button: PointerButton,
        anchor: MotionAnchor,
    },
    ButtonUp {
        button: PointerButton,
        anchor: MotionAnchor,
    },
    ScrollBegin {
        scroll: ActiveScroll,
    },
    ScrollEnd {
        scroll_id: ScrollId,
        anchor: MotionAnchor,
    },
    ScrollCancel {
        scroll_id: ScrollId,
        anchor: MotionAnchor,
    },
    TouchBegin {
        initial_state: TouchState,
    },
    TouchEnd {
        anchor: MotionAnchor,
    },
    TouchCancel {
        anchor: MotionAnchor,
    },
    StateSnapshot(StateSnapshot),
    SnapshotAck(SnapshotAck),
    SessionTakeover(SessionTakeover),
    TakeoverAccepted(TakeoverAccepted),
    SessionClose {
        reason: SessionCloseReason,
        final_anchor: Option<MotionAnchor>,
    },
}

impl ReliableControl {
    pub fn motion_anchor(&self) -> Option<&MotionAnchor> {
        match self {
            Self::Leave { anchor }
            | Self::ButtonDown { anchor, .. }
            | Self::ButtonUp { anchor, .. }
            | Self::ScrollEnd { anchor, .. }
            | Self::ScrollCancel { anchor, .. }
            | Self::TouchEnd { anchor }
            | Self::TouchCancel { anchor } => Some(anchor),
            Self::StateSnapshot(snapshot) => Some(&snapshot.motion_anchor),
            Self::SessionTakeover(takeover) => Some(&takeover.final_motion_anchor),
            Self::SessionClose {
                final_anchor: Some(anchor),
                ..
            } => Some(anchor),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReliableControlMessage {
    pub session: SessionContext,
    pub sequence: ControlSequence,
    pub payload: ReliableControl,
}

/// Latest-wins datagram carrying cumulative state from activation start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MotionFrame {
    pub session: SessionContext,
    pub motion_sequence: MotionSequence,
    pub control_watermark: ControlSequence,
    pub sender_capture_time: MonotonicTimeMicros,
    pub totals: CumulativeMotion,
    pub touch_snapshot: Option<TouchState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProbePayload {
    Probe {
        sequence: ProbeSequence,
        sent_at: MonotonicTimeMicros,
    },
    ProbeEcho {
        sequence: ProbeSequence,
        probe_sent_at: MonotonicTimeMicros,
        received_at: MonotonicTimeMicros,
        echoed_at: MonotonicTimeMicros,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeMessage {
    pub session: SessionContext,
    pub payload: ProbePayload,
}

/// Values one peer is willing to negotiate for an authenticated input session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NegotiationOffer {
    pub protocol_versions: Vec<ProtocolVersion>,
    pub maximum_datagram_size: u32,
    pub supported_capabilities: InputCapabilities,
    pub required_capabilities: InputCapabilities,
    pub pointer_units: BTreeSet<PointerUnit>,
    pub scroll_fields: ScrollFields,
    pub maximum_contacts: u16,
    pub maximum_receiver_lease_ms: u32,
    pub maximum_checkpoint_bound_ms: u32,
}

/// The single schema selected after authenticated negotiation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NegotiatedSession {
    pub protocol_version: ProtocolVersion,
    pub maximum_datagram_size: u32,
    pub capabilities: InputCapabilities,
    pub pointer_unit: Option<PointerUnit>,
    pub scroll_fields: ScrollFields,
    pub contact_limit: u16,
    pub receiver_lease_ms: u32,
    pub checkpoint_bound_ms: u32,
}

impl NegotiatedSession {
    pub const MAX_RECEIVER_LEASE_MS: u32 = 1_000;
    pub const MAX_CHECKPOINT_BOUND_MS: u32 = 250;

    pub fn validate_for(&self, offer: &NegotiationOffer) -> Result<(), NegotiationError> {
        if !offer.protocol_versions.contains(&self.protocol_version) {
            return Err(NegotiationError::UnsupportedProtocolVersion);
        }
        if self.maximum_datagram_size == 0
            || self.maximum_datagram_size > offer.maximum_datagram_size
        {
            return Err(NegotiationError::InvalidDatagramSize);
        }
        if !offer.supported_capabilities.is_superset(&self.capabilities) {
            return Err(NegotiationError::UnsupportedCapability);
        }
        if !self.capabilities.is_superset(&offer.required_capabilities) {
            return Err(NegotiationError::MissingRequiredCapability);
        }

        match (
            self.capabilities.contains(InputCapability::Pointer),
            self.pointer_unit,
        ) {
            (true, Some(unit)) if offer.pointer_units.contains(&unit) => {}
            (false, None) => {}
            _ => return Err(NegotiationError::UnsupportedPointerUnit),
        }

        if !self.scroll_fields.is_subset_of(offer.scroll_fields) {
            return Err(NegotiationError::UnsupportedScrollFields);
        }
        if self.contact_limit > offer.maximum_contacts
            || (self.capabilities.contains(InputCapability::Touch) && self.contact_limit == 0)
            || (!self.capabilities.contains(InputCapability::Touch) && self.contact_limit != 0)
        {
            return Err(NegotiationError::InvalidContactLimit);
        }
        if self.receiver_lease_ms == 0
            || self.receiver_lease_ms > Self::MAX_RECEIVER_LEASE_MS
            || self.receiver_lease_ms > offer.maximum_receiver_lease_ms
        {
            return Err(NegotiationError::InvalidReceiverLease);
        }
        if self.checkpoint_bound_ms == 0
            || self.checkpoint_bound_ms > Self::MAX_CHECKPOINT_BOUND_MS
            || self.checkpoint_bound_ms > offer.maximum_checkpoint_bound_ms
        {
            return Err(NegotiationError::InvalidCheckpointBound);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NegotiationError {
    UnsupportedProtocolVersion,
    InvalidDatagramSize,
    UnsupportedCapability,
    MissingRequiredCapability,
    UnsupportedPointerUnit,
    UnsupportedScrollFields,
    InvalidContactLimit,
    InvalidReceiverLease,
    InvalidCheckpointBound,
}

impl std::fmt::Display for NegotiationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "session negotiation failed: {self:?}")
    }
}

impl std::error::Error for NegotiationError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cumulative_motion_advances_and_reconciles() {
        let mut totals = CumulativeMotion::ZERO;
        totals
            .checked_advance(MotionDelta {
                dx: 12,
                dy: -7,
                scroll_x: 3,
                scroll_y: -9,
            })
            .unwrap();

        assert_eq!(totals, CumulativeMotion::new(12, -7, 3, -9));
        assert_eq!(
            totals.checked_delta_from(CumulativeMotion::ZERO).unwrap(),
            MotionDelta {
                dx: 12,
                dy: -7,
                scroll_x: 3,
                scroll_y: -9,
            }
        );
    }

    #[test]
    fn cumulative_motion_overflow_is_atomic_and_names_axis() {
        let original = CumulativeMotion::new(4, 5, 6, i64::MAX);
        let mut totals = original;

        let error = totals
            .checked_advance(MotionDelta {
                dx: 1,
                dy: 1,
                scroll_x: 1,
                scroll_y: 1,
            })
            .unwrap_err();

        assert_eq!(error.axis, MotionAxis::ScrollY);
        assert_eq!(totals, original);
    }

    #[test]
    fn held_state_tracks_idempotent_transitions_and_release_all() {
        let key = HidUsage::keyboard(0x04);
        let mut held = HeldState::default();

        assert!(held.is_neutral());
        assert!(held.press_key(key));
        assert!(!held.press_key(key));
        assert!(held.press_button(PointerButton::PRIMARY));
        assert!(held.set_modifier(Modifier::LeftShift, true));
        assert!(!held.is_neutral());
        assert!(held.release_key(key));
        assert!(!held.release_key(key));

        held.release_all();
        assert!(held.is_neutral());
    }

    #[test]
    fn held_state_ends_only_the_matching_scroll() {
        let mut held = HeldState::default();
        held.begin_scroll(ActiveScroll {
            id: ScrollId(7),
            source: ScrollSource {
                unit: ScrollUnit::Device,
                resolution_x: Some(120),
                resolution_y: Some(120),
            },
            phase: ScrollPhase::Begin,
            momentum_phase: None,
        });

        assert!(!held.end_scroll(ScrollId(8)));
        assert!(held.active_scroll.is_some());
        assert!(held.end_scroll(ScrollId(7)));
        assert!(held.is_neutral());
    }
}
