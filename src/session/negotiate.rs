//! Session negotiation and the checks that keep a peer inside what it negotiated.

use anyhow::{Context, Result, bail};

use crate::{
    core::{
        HeldState, HidUsage, HidUsagePage, InputCapabilities, InputCapability, MotionAnchor,
        NegotiatedSession, NegotiationOffer, ReliableControl, TouchState,
    },
    transport::{InputChannels, InputControlMessage},
};

pub(super) async fn negotiate(
    channels: &mut InputChannels,
    local: &NegotiationOffer,
) -> Result<NegotiatedSession> {
    channels.control_send.send_negotiation_offer(local).await?;
    let remote = match channels.control_receive.receive().await? {
        InputControlMessage::NegotiationOffer(offer) => offer,
        _ => bail!("peer did not begin with a negotiation offer"),
    };
    let selected = select_negotiation(local, &remote)?;
    channels
        .control_send
        .send_negotiated_session(&selected)
        .await?;
    let peer_selected = match channels.control_receive.receive().await? {
        InputControlMessage::NegotiatedSession(session) => session,
        _ => bail!("peer did not finish session negotiation"),
    };
    if peer_selected != selected {
        bail!("peer selected a different session schema");
    }
    Ok(selected)
}

pub(super) fn select_negotiation(
    left: &NegotiationOffer,
    right: &NegotiationOffer,
) -> Result<NegotiatedSession> {
    let version = left
        .protocol_versions
        .iter()
        .filter(|version| right.protocol_versions.contains(version))
        .max()
        .copied()
        .context("peers have no protocol version in common")?;
    let capabilities = InputCapabilities::new(
        left.supported_capabilities
            .iter()
            .filter(|capability| right.supported_capabilities.contains(*capability)),
    );
    if !capabilities.is_superset(&left.required_capabilities)
        || !capabilities.is_superset(&right.required_capabilities)
    {
        bail!("peer lacks a required input capability");
    }
    let pointer_unit = left
        .pointer_units
        .intersection(&right.pointer_units)
        .next()
        .copied();
    let scroll_fields = intersect_scroll_fields(left.scroll_fields, right.scroll_fields);
    let contact_limit = if capabilities.contains(InputCapability::Touch) {
        left.maximum_contacts.min(right.maximum_contacts)
    } else {
        0
    };
    let selected = NegotiatedSession {
        protocol_version: version,
        maximum_datagram_size: left.maximum_datagram_size.min(right.maximum_datagram_size),
        capabilities,
        pointer_unit,
        scroll_fields,
        contact_limit,
        receiver_lease_ms: left
            .maximum_receiver_lease_ms
            .min(right.maximum_receiver_lease_ms),
        checkpoint_bound_ms: left
            .maximum_checkpoint_bound_ms
            .min(right.maximum_checkpoint_bound_ms),
    };
    selected.validate_for(left)?;
    selected.validate_for(right)?;
    Ok(selected)
}

pub(super) fn validate_negotiated_control(
    payload: &ReliableControl,
    negotiated: &NegotiatedSession,
) -> Result<()> {
    match payload {
        ReliableControl::Enter | ReliableControl::SnapshotAck(_) => {}
        ReliableControl::KeyDown { key } | ReliableControl::KeyUp { key } => {
            validate_negotiated_usage(*key, negotiated)?;
        }
        ReliableControl::ButtonDown { anchor, .. } | ReliableControl::ButtonUp { anchor, .. } => {
            require_capability(negotiated, InputCapability::Pointer, "pointer button")?;
            validate_negotiated_anchor(anchor, negotiated)?;
        }
        ReliableControl::TouchBegin { initial_state } => {
            require_capability(negotiated, InputCapability::Touch, "touch control")?;
            validate_negotiated_touch(initial_state, negotiated)?;
        }
        ReliableControl::TouchEnd { anchor } | ReliableControl::TouchCancel { anchor } => {
            require_capability(negotiated, InputCapability::Touch, "touch control")?;
            validate_negotiated_anchor(anchor, negotiated)?;
        }
        ReliableControl::StateSnapshot(snapshot) => {
            validate_negotiated_held(&snapshot.held, negotiated)?;
            validate_negotiated_anchor(&snapshot.motion_anchor, negotiated)?;
        }
        ReliableControl::SessionClose { final_anchor, .. } => {
            if let Some(anchor) = final_anchor {
                validate_negotiated_anchor(anchor, negotiated)?;
            }
        }
    }
    Ok(())
}

pub(super) fn validate_negotiated_motion(
    frame: &crate::core::MotionFrame,
    negotiated: &NegotiatedSession,
) -> Result<()> {
    let totals = frame.totals;
    if totals.total_dx() != 0 || totals.total_dy() != 0 {
        require_capability(negotiated, InputCapability::Pointer, "pointer motion")?;
    }
    if totals.total_scroll_x() != 0 || totals.total_scroll_y() != 0 {
        require_capability(negotiated, InputCapability::Scroll, "scroll motion")?;
        if !negotiated.scroll_fields.high_resolution {
            bail!("peer sent high-resolution scroll totals without negotiating them");
        }
    }
    if let Some(touch) = &frame.touch_snapshot {
        require_capability(negotiated, InputCapability::Touch, "touch snapshot")?;
        validate_negotiated_touch(touch, negotiated)?;
    }
    Ok(())
}

fn validate_negotiated_usage(usage: HidUsage, negotiated: &NegotiatedSession) -> Result<()> {
    let capability = match usage.page {
        HidUsagePage::KEYBOARD_KEYPAD => InputCapability::Keyboard,
        HidUsagePage::CONSUMER => InputCapability::ConsumerControls,
        _ => bail!("peer sent a key from an unsupported HID usage page"),
    };
    require_capability(negotiated, capability, "key usage")
}

fn validate_negotiated_held(state: &HeldState, negotiated: &NegotiatedSession) -> Result<()> {
    for key in &state.pressed_keys {
        validate_negotiated_usage(*key, negotiated)?;
    }
    if !state.pressed_buttons.is_empty() {
        require_capability(negotiated, InputCapability::Pointer, "pointer button")?;
    }
    validate_negotiated_touch(&state.active_touch, negotiated)
}

fn validate_negotiated_anchor(anchor: &MotionAnchor, negotiated: &NegotiatedSession) -> Result<()> {
    let totals = anchor.totals;
    if totals.total_dx() != 0 || totals.total_dy() != 0 {
        require_capability(negotiated, InputCapability::Pointer, "pointer anchor")?;
    }
    if totals.total_scroll_x() != 0 || totals.total_scroll_y() != 0 {
        require_capability(negotiated, InputCapability::Scroll, "scroll anchor")?;
        if !negotiated.scroll_fields.high_resolution {
            bail!("peer sent high-resolution scroll totals without negotiating them");
        }
    }
    validate_negotiated_touch(&anchor.final_touch_state, negotiated)
}

fn require_capability(
    negotiated: &NegotiatedSession,
    capability: InputCapability,
    event: &str,
) -> Result<()> {
    if !negotiated.capabilities.contains(capability) {
        bail!("peer sent {event} without negotiating {capability:?}");
    }
    Ok(())
}

fn validate_negotiated_touch(state: &TouchState, negotiated: &NegotiatedSession) -> Result<()> {
    if state.len() > usize::from(negotiated.contact_limit) {
        bail!("peer exceeded the negotiated touch contact limit");
    }
    if !state.is_empty() && !negotiated.capabilities.contains(InputCapability::Touch) {
        bail!("peer sent touch state without negotiating touch support");
    }
    Ok(())
}

fn intersect_scroll_fields(
    left: crate::core::ScrollFields,
    right: crate::core::ScrollFields,
) -> crate::core::ScrollFields {
    crate::core::ScrollFields {
        high_resolution: left.high_resolution && right.high_resolution,
        source_unit: left.source_unit && right.source_unit,
        source_resolution: left.source_resolution && right.source_resolution,
        discrete_steps: left.discrete_steps && right.discrete_steps,
        phase: left.phase && right.phase,
        momentum_phase: left.momentum_phase && right.momentum_phase,
    }
}
