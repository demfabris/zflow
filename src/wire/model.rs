use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::core::*;

use super::bounds::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireNegotiationOffer {
    protocol_versions: BoundedVec<ProtocolVersion, MAX_PROTOCOL_VERSIONS>,
    maximum_datagram_size: u32,
    supported_capabilities: BoundedVec<InputCapability, MAX_CAPABILITIES>,
    required_capabilities: BoundedVec<InputCapability, MAX_CAPABILITIES>,
    pointer_units: BoundedVec<PointerUnit, MAX_POINTER_UNITS>,
    scroll_fields: ScrollFields,
    maximum_contacts: u16,
    maximum_receiver_lease_ms: u32,
    maximum_checkpoint_bound_ms: u32,
}

impl TryFrom<&NegotiationOffer> for WireNegotiationOffer {
    type Error = BoundError;

    fn try_from(value: &NegotiationOffer) -> Result<Self, Self::Error> {
        if usize::from(value.maximum_contacts) > MAX_CONTACTS {
            return Err(BoundError::Invalid(
                "maximum_contacts exceeds the wire contact limit",
            ));
        }
        Ok(Self {
            protocol_versions: BoundedVec::try_from_vec(
                value.protocol_versions.clone(),
                "protocol_versions",
            )?,
            maximum_datagram_size: value.maximum_datagram_size,
            supported_capabilities: BoundedVec::try_from_vec(
                value.supported_capabilities.iter().collect(),
                "supported_capabilities",
            )?,
            required_capabilities: BoundedVec::try_from_vec(
                value.required_capabilities.iter().collect(),
                "required_capabilities",
            )?,
            pointer_units: BoundedVec::try_from_vec(
                value.pointer_units.iter().copied().collect(),
                "pointer_units",
            )?,
            scroll_fields: value.scroll_fields,
            maximum_contacts: value.maximum_contacts,
            maximum_receiver_lease_ms: value.maximum_receiver_lease_ms,
            maximum_checkpoint_bound_ms: value.maximum_checkpoint_bound_ms,
        })
    }
}

impl TryFrom<WireNegotiationOffer> for NegotiationOffer {
    type Error = BoundError;

    fn try_from(value: WireNegotiationOffer) -> Result<Self, Self::Error> {
        if usize::from(value.maximum_contacts) > MAX_CONTACTS {
            return Err(BoundError::Invalid(
                "maximum_contacts exceeds the wire contact limit",
            ));
        }
        Ok(Self {
            protocol_versions: unique_vec(value.protocol_versions, "protocol_versions")?,
            maximum_datagram_size: value.maximum_datagram_size,
            supported_capabilities: InputCapabilities::new(unique_vec(
                value.supported_capabilities,
                "supported_capabilities",
            )?),
            required_capabilities: InputCapabilities::new(unique_vec(
                value.required_capabilities,
                "required_capabilities",
            )?),
            pointer_units: unique_set(value.pointer_units, "pointer_units")?,
            scroll_fields: value.scroll_fields,
            maximum_contacts: value.maximum_contacts,
            maximum_receiver_lease_ms: value.maximum_receiver_lease_ms,
            maximum_checkpoint_bound_ms: value.maximum_checkpoint_bound_ms,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireNegotiatedSession {
    maximum_datagram_size: u32,
    capabilities: BoundedVec<InputCapability, MAX_CAPABILITIES>,
    pointer_unit: Option<PointerUnit>,
    scroll_fields: ScrollFields,
    contact_limit: u16,
    receiver_lease_ms: u32,
    checkpoint_bound_ms: u32,
}

impl TryFrom<&NegotiatedSession> for WireNegotiatedSession {
    type Error = BoundError;

    fn try_from(value: &NegotiatedSession) -> Result<Self, Self::Error> {
        check_contact_limit(value.contact_limit)?;
        Ok(Self {
            maximum_datagram_size: value.maximum_datagram_size,
            capabilities: BoundedVec::try_from_vec(
                value.capabilities.iter().collect(),
                "capabilities",
            )?,
            pointer_unit: value.pointer_unit,
            scroll_fields: value.scroll_fields,
            contact_limit: value.contact_limit,
            receiver_lease_ms: value.receiver_lease_ms,
            checkpoint_bound_ms: value.checkpoint_bound_ms,
        })
    }
}

impl WireNegotiatedSession {
    pub(crate) fn into_model(
        self,
        protocol_version: ProtocolVersion,
    ) -> Result<NegotiatedSession, BoundError> {
        check_contact_limit(self.contact_limit)?;
        Ok(NegotiatedSession {
            protocol_version,
            maximum_datagram_size: self.maximum_datagram_size,
            capabilities: InputCapabilities::new(unique_vec(self.capabilities, "capabilities")?),
            pointer_unit: self.pointer_unit,
            scroll_fields: self.scroll_fields,
            contact_limit: self.contact_limit,
            receiver_lease_ms: self.receiver_lease_ms,
            checkpoint_bound_ms: self.checkpoint_bound_ms,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireTouchState(BoundedVec<TouchContact, MAX_CONTACTS>);

impl TryFrom<&TouchState> for WireTouchState {
    type Error = BoundError;

    fn try_from(value: &TouchState) -> Result<Self, Self::Error> {
        Ok(Self(BoundedVec::try_from_vec(
            value.iter().cloned().collect(),
            "touch contacts",
        )?))
    }
}

impl TryFrom<WireTouchState> for TouchState {
    type Error = BoundError;

    fn try_from(value: WireTouchState) -> Result<Self, Self::Error> {
        TouchState::new(value.0.into_vec()).map_err(|_| BoundError::Duplicate("touch contacts"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireHeldState {
    pressed_keys: BoundedVec<HidUsage, MAX_HELD_KEYS>,
    pressed_buttons: BoundedVec<PointerButton, MAX_HELD_BUTTONS>,
    active_touch: WireTouchState,
}

impl TryFrom<&HeldState> for WireHeldState {
    type Error = BoundError;

    fn try_from(value: &HeldState) -> Result<Self, Self::Error> {
        Ok(Self {
            pressed_keys: BoundedVec::try_from_vec(
                value.pressed_keys.iter().copied().collect(),
                "pressed_keys",
            )?,
            pressed_buttons: BoundedVec::try_from_vec(
                value.pressed_buttons.iter().copied().collect(),
                "pressed_buttons",
            )?,
            active_touch: WireTouchState::try_from(&value.active_touch)?,
        })
    }
}

impl TryFrom<WireHeldState> for HeldState {
    type Error = BoundError;

    fn try_from(value: WireHeldState) -> Result<Self, Self::Error> {
        Ok(Self {
            pressed_keys: unique_set(value.pressed_keys, "pressed_keys")?,
            pressed_buttons: unique_set(value.pressed_buttons, "pressed_buttons")?,
            active_touch: value.active_touch.try_into()?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireMotionAnchor {
    activation_id: ActivationId,
    through_motion_sequence: MotionSequence,
    sender_capture_time: MonotonicTimeMicros,
    totals: CumulativeMotion,
    final_touch_state: WireTouchState,
    kind: AnchorKind,
}

impl TryFrom<&MotionAnchor> for WireMotionAnchor {
    type Error = BoundError;

    fn try_from(value: &MotionAnchor) -> Result<Self, Self::Error> {
        Ok(Self {
            activation_id: value.activation_id,
            through_motion_sequence: value.through_motion_sequence,
            sender_capture_time: value.sender_capture_time,
            totals: value.totals,
            final_touch_state: WireTouchState::try_from(&value.final_touch_state)?,
            kind: value.kind,
        })
    }
}

impl TryFrom<WireMotionAnchor> for MotionAnchor {
    type Error = BoundError;

    fn try_from(value: WireMotionAnchor) -> Result<Self, Self::Error> {
        Ok(Self {
            activation_id: value.activation_id,
            through_motion_sequence: value.through_motion_sequence,
            sender_capture_time: value.sender_capture_time,
            totals: value.totals,
            final_touch_state: value.final_touch_state.try_into()?,
            kind: value.kind,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireStateSnapshot {
    held: WireHeldState,
    motion_anchor: WireMotionAnchor,
}

impl TryFrom<&StateSnapshot> for WireStateSnapshot {
    type Error = BoundError;

    fn try_from(value: &StateSnapshot) -> Result<Self, Self::Error> {
        Ok(Self {
            held: WireHeldState::try_from(&value.held)?,
            motion_anchor: WireMotionAnchor::try_from(&value.motion_anchor)?,
        })
    }
}

impl TryFrom<WireStateSnapshot> for StateSnapshot {
    type Error = BoundError;

    fn try_from(value: WireStateSnapshot) -> Result<Self, Self::Error> {
        Ok(Self {
            held: value.held.try_into()?,
            motion_anchor: value.motion_anchor.try_into()?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum WireReliableControl {
    Enter,
    KeyDown {
        key: HidUsage,
    },
    KeyUp {
        key: HidUsage,
    },
    ButtonDown {
        button: PointerButton,
        anchor: WireMotionAnchor,
    },
    ButtonUp {
        button: PointerButton,
        anchor: WireMotionAnchor,
    },
    TouchBegin {
        initial_state: WireTouchState,
    },
    TouchEnd {
        anchor: WireMotionAnchor,
    },
    TouchCancel {
        anchor: WireMotionAnchor,
    },
    StateSnapshot(WireStateSnapshot),
    SnapshotAck(SnapshotAck),
    SessionClose {
        reason: SessionCloseReason,
        final_anchor: Option<WireMotionAnchor>,
    },
}

impl TryFrom<&ReliableControl> for WireReliableControl {
    type Error = BoundError;

    fn try_from(value: &ReliableControl) -> Result<Self, Self::Error> {
        Ok(match value {
            ReliableControl::Enter => Self::Enter,
            ReliableControl::KeyDown { key } => Self::KeyDown { key: *key },
            ReliableControl::KeyUp { key } => Self::KeyUp { key: *key },
            ReliableControl::ButtonDown { button, anchor } => Self::ButtonDown {
                button: *button,
                anchor: anchor.try_into()?,
            },
            ReliableControl::ButtonUp { button, anchor } => Self::ButtonUp {
                button: *button,
                anchor: anchor.try_into()?,
            },
            ReliableControl::TouchBegin { initial_state } => Self::TouchBegin {
                initial_state: initial_state.try_into()?,
            },
            ReliableControl::TouchEnd { anchor } => Self::TouchEnd {
                anchor: anchor.try_into()?,
            },
            ReliableControl::TouchCancel { anchor } => Self::TouchCancel {
                anchor: anchor.try_into()?,
            },
            ReliableControl::StateSnapshot(snapshot) => Self::StateSnapshot(snapshot.try_into()?),
            ReliableControl::SnapshotAck(ack) => Self::SnapshotAck(*ack),
            ReliableControl::SessionClose {
                reason,
                final_anchor,
            } => Self::SessionClose {
                reason: *reason,
                final_anchor: final_anchor.as_ref().map(TryInto::try_into).transpose()?,
            },
        })
    }
}

impl TryFrom<WireReliableControl> for ReliableControl {
    type Error = BoundError;

    fn try_from(value: WireReliableControl) -> Result<Self, Self::Error> {
        Ok(match value {
            WireReliableControl::Enter => Self::Enter,
            WireReliableControl::KeyDown { key } => Self::KeyDown { key },
            WireReliableControl::KeyUp { key } => Self::KeyUp { key },
            WireReliableControl::ButtonDown { button, anchor } => Self::ButtonDown {
                button,
                anchor: anchor.try_into()?,
            },
            WireReliableControl::ButtonUp { button, anchor } => Self::ButtonUp {
                button,
                anchor: anchor.try_into()?,
            },
            WireReliableControl::TouchBegin { initial_state } => Self::TouchBegin {
                initial_state: initial_state.try_into()?,
            },
            WireReliableControl::TouchEnd { anchor } => Self::TouchEnd {
                anchor: anchor.try_into()?,
            },
            WireReliableControl::TouchCancel { anchor } => Self::TouchCancel {
                anchor: anchor.try_into()?,
            },
            WireReliableControl::StateSnapshot(snapshot) => {
                Self::StateSnapshot(snapshot.try_into()?)
            }
            WireReliableControl::SnapshotAck(ack) => Self::SnapshotAck(ack),
            WireReliableControl::SessionClose {
                reason,
                final_anchor,
            } => Self::SessionClose {
                reason,
                final_anchor: final_anchor.map(TryInto::try_into).transpose()?,
            },
        })
    }
}

impl WireReliableControl {
    pub(crate) fn message_type(&self) -> u8 {
        match self {
            Self::Enter => 1,
            Self::KeyDown { .. } => 2,
            Self::KeyUp { .. } => 3,
            Self::ButtonDown { .. } => 4,
            Self::ButtonUp { .. } => 5,
            Self::TouchBegin { .. } => 6,
            Self::TouchEnd { .. } => 7,
            Self::TouchCancel { .. } => 8,
            Self::StateSnapshot(_) => 9,
            Self::SnapshotAck(_) => 10,
            Self::SessionClose { .. } => 11,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WireMotionBody {
    sender_capture_time: MonotonicTimeMicros,
    totals: CumulativeMotion,
    touch_snapshot: Option<WireTouchState>,
}

impl TryFrom<&MotionFrame> for WireMotionBody {
    type Error = BoundError;

    fn try_from(value: &MotionFrame) -> Result<Self, Self::Error> {
        Ok(Self {
            sender_capture_time: value.sender_capture_time,
            totals: value.totals,
            touch_snapshot: value
                .touch_snapshot
                .as_ref()
                .map(TryInto::try_into)
                .transpose()?,
        })
    }
}

impl WireMotionBody {
    pub(crate) fn into_model(
        self,
        session: SessionContext,
        motion_sequence: MotionSequence,
        control_watermark: ControlSequence,
    ) -> Result<MotionFrame, BoundError> {
        Ok(MotionFrame {
            session,
            motion_sequence,
            control_watermark,
            sender_capture_time: self.sender_capture_time,
            totals: self.totals,
            touch_snapshot: self.touch_snapshot.map(TryInto::try_into).transpose()?,
        })
    }
}

/// Metadata exchanged only after the pairing-only TLS handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingOffer {
    pub handshake_nonce: [u8; 32],
    pub device_label: Option<String>,
    pub input_port: u16,
    pub input_candidates: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WirePairingOffer {
    handshake_nonce: [u8; 32],
    device_label: Option<BoundedString<MAX_STRING_BYTES>>,
    input_port: u16,
    input_candidates: BoundedVec<BoundedString<MAX_STRING_BYTES>, MAX_DISCOVERY_CANDIDATES>,
}

impl TryFrom<&PairingOffer> for WirePairingOffer {
    type Error = BoundError;

    fn try_from(value: &PairingOffer) -> Result<Self, Self::Error> {
        Ok(Self {
            handshake_nonce: value.handshake_nonce,
            device_label: value
                .device_label
                .clone()
                .map(|label| BoundedString::try_from_string(label, "device_label"))
                .transpose()?,
            input_port: value.input_port,
            input_candidates: BoundedVec::try_from_vec(
                value
                    .input_candidates
                    .iter()
                    .cloned()
                    .map(|candidate| BoundedString::try_from_string(candidate, "input_candidate"))
                    .collect::<Result<_, _>>()?,
                "input_candidates",
            )?,
        })
    }
}

impl TryFrom<WirePairingOffer> for PairingOffer {
    type Error = BoundError;

    fn try_from(value: WirePairingOffer) -> Result<Self, Self::Error> {
        Ok(Self {
            handshake_nonce: value.handshake_nonce,
            device_label: value.device_label.map(BoundedString::into_string),
            input_port: value.input_port,
            input_candidates: unique_vec(value.input_candidates, "input_candidates")?
                .into_iter()
                .map(BoundedString::into_string)
                .collect(),
        })
    }
}

fn check_contact_limit(limit: u16) -> Result<(), BoundError> {
    if usize::from(limit) > MAX_CONTACTS {
        Err(BoundError::Invalid(
            "contact_limit exceeds the wire contact limit",
        ))
    } else {
        Ok(())
    }
}

fn unique_vec<T: Ord, const N: usize>(
    values: BoundedVec<T, N>,
    name: &'static str,
) -> Result<Vec<T>, BoundError> {
    let values = values.into_vec();
    if values.iter().collect::<BTreeSet<_>>().len() != values.len() {
        return Err(BoundError::Duplicate(name));
    }
    Ok(values)
}

fn unique_set<T: Ord, const N: usize>(
    values: BoundedVec<T, N>,
    name: &'static str,
) -> Result<BTreeSet<T>, BoundError> {
    let original_len = values.as_slice().len();
    let values: BTreeSet<_> = values.into_vec().into_iter().collect();
    if values.len() != original_len {
        return Err(BoundError::Duplicate(name));
    }
    Ok(values)
}
