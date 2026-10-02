//! Limits a peer's message must respect. The family payload caps already bound
//! what decoding can allocate, so these checks run on the decoded value.

use crate::core::{ReliableControl, TouchState};

use super::{MAX_NAME_BYTES, MAX_VERSION_BYTES, WireError, WireMessage};

pub use crate::discovery::MAX_DISCOVERY_CANDIDATES;

pub const MAX_CONTACTS: usize = 32;
pub const MAX_HELD_KEYS: usize = 32;
pub const MAX_HELD_BUTTONS: usize = 16;
pub const MAX_STRING_BYTES: usize = 255;

/// Runs before encoding and after decoding, so neither side sends or accepts
/// a message the other would refuse.
pub(super) fn validate(message: &WireMessage) -> Result<(), WireError> {
    match message {
        WireMessage::NegotiationOffer(offer) => limit(
            "maximum contacts",
            offer.maximum_contacts.into(),
            MAX_CONTACTS,
        ),
        WireMessage::NegotiatedSession(session) => {
            limit("contact limit", session.contact_limit.into(), MAX_CONTACTS)
        }
        WireMessage::ReliableControl(message) => {
            if let Some(anchor) = message.payload.motion_anchor() {
                if anchor.activation_id != message.session.activation_id {
                    return Err(WireError::Invalid(
                        "motion anchor activation differs from the session".into(),
                    ));
                }
                touch(&anchor.final_touch_state)?;
            }
            match &message.payload {
                ReliableControl::TouchBegin { initial_state } => touch(initial_state),
                ReliableControl::StateSnapshot(snapshot) => {
                    let held = &snapshot.held;
                    limit("pressed keys", held.pressed_keys.len(), MAX_HELD_KEYS)?;
                    limit(
                        "pressed buttons",
                        held.pressed_buttons.len(),
                        MAX_HELD_BUTTONS,
                    )?;
                    touch(&held.active_touch)
                }
                _ => Ok(()),
            }
        }
        WireMessage::Motion(frame) => frame.touch_snapshot.as_ref().map_or(Ok(()), touch),
        WireMessage::Probe(_) => Ok(()),
        WireMessage::Pairing(offer) => {
            if let Some(label) = &offer.device_label {
                limit("device label bytes", label.len(), MAX_STRING_BYTES)?;
            }
            let candidates = &offer.input_candidates;
            limit(
                "input candidates",
                candidates.len(),
                MAX_DISCOVERY_CANDIDATES,
            )?;
            candidates.iter().try_for_each(|candidate| {
                limit("input candidate bytes", candidate.len(), MAX_STRING_BYTES)
            })
        }
        WireMessage::Hello(hello) => {
            limit("name bytes", hello.name.len(), MAX_NAME_BYTES)?;
            limit("version bytes", hello.version.len(), MAX_VERSION_BYTES)?;
            limit(
                "hello candidates",
                hello.candidates.len(),
                MAX_DISCOVERY_CANDIDATES,
            )?;
            if hello.input_port == 0 {
                return Err(WireError::Invalid("input port must be non-zero".into()));
            }
            Ok(())
        }
        WireMessage::Desktop(message) => message
            .validate()
            .map_err(|error| WireError::Invalid(error.to_string())),
    }
}

fn touch(state: &TouchState) -> Result<(), WireError> {
    limit("touch contacts", state.len(), MAX_CONTACTS)
}

fn limit(what: &'static str, actual: usize, maximum: usize) -> Result<(), WireError> {
    if actual > maximum {
        return Err(WireError::SizeLimit {
            what,
            actual,
            maximum,
        });
    }
    Ok(())
}
