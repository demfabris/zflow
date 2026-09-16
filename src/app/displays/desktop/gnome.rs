use std::collections::HashMap;

use anyhow::{Context, Result, ensure};
use zbus::zvariant::OwnedValue;

use super::{Desktop, Rect, desktop_bounds};

type Properties = HashMap<String, OwnedValue>;
type MonitorId = (String, String, String, String);
type Mode = (String, i32, i32, f64, f64, Vec<f64>, Properties);
type PhysicalMonitor = (MonitorId, Vec<Mode>, Properties);
type LogicalMonitor = (i32, i32, f64, u32, bool, Vec<MonitorId>, Properties);
type CurrentState = (u32, Vec<PhysicalMonitor>, Vec<LogicalMonitor>, Properties);

pub(super) async fn detect() -> Result<Desktop> {
    let connection = zbus::Connection::session().await?;
    let message = connection
        .call_method(
            Some("org.gnome.Mutter.DisplayConfig"),
            "/org/gnome/Mutter/DisplayConfig",
            Some("org.gnome.Mutter.DisplayConfig"),
            "GetCurrentState",
            &(),
        )
        .await?;
    let state: CurrentState = message.body().deserialize()?;
    from_state(&state)
}

fn from_state(state: &CurrentState) -> Result<Desktop> {
    let (_, physical, logical, properties) = state;
    let layout_mode = properties
        .get("layout-mode")
        .map(u32::try_from)
        .transpose()?
        .unwrap_or(1);
    ensure!(
        [1, 2].contains(&layout_mode),
        "GNOME returned an unknown desktop layout mode"
    );
    let mut rects = Vec::with_capacity(logical.len());
    for (x, y, scale, transform, _, ids, _) in logical {
        ensure!(
            scale.is_finite() && *scale > 0.0 && *transform <= 7,
            "GNOME returned an invalid scale or rotation"
        );
        let mut size = None;
        for id in ids {
            let (_, modes, _) = physical
                .iter()
                .find(|(candidate, _, _)| candidate == id)
                .context("GNOME logical monitor has no matching physical monitor")?;
            let current: Vec<_> = modes
                .iter()
                .filter(|mode| {
                    mode.6
                        .get("is-current")
                        .and_then(|value| bool::try_from(value).ok())
                        == Some(true)
                })
                .collect();
            let [mode] = current.as_slice() else {
                anyhow::bail!("GNOME logical monitor has no unique active mode");
            };
            ensure!(
                mode.1 > 0 && mode.2 > 0,
                "GNOME returned invalid mode dimensions"
            );
            let (mut width, mut height) = (f64::from(mode.1), f64::from(mode.2));
            if transform % 2 == 1 {
                std::mem::swap(&mut width, &mut height);
            }
            if layout_mode == 1 {
                width = (width / scale).round();
                height = (height / scale).round();
            }
            let candidate = (width, height);
            ensure!(
                size.is_none_or(|size| size == candidate),
                "GNOME mirrored monitors have inconsistent dimensions"
            );
            size = Some(candidate);
        }
        let (width, height) = size.context("GNOME logical monitor has no outputs")?;
        rects.push(Rect {
            x: f64::from(*x),
            y: f64::from(*y),
            width,
            height,
        });
    }
    desktop_bounds(rects)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn physical(connector: &str, active: bool) -> PhysicalMonitor {
        (
            (
                connector.into(),
                "vendor".into(),
                "model".into(),
                connector.into(),
            ),
            vec![(
                "mode".into(),
                3840,
                2160,
                60.0,
                2.0,
                vec![1.0, 2.0],
                HashMap::from([("is-current".into(), OwnedValue::from(active))]),
            )],
            HashMap::new(),
        )
    }

    fn state() -> CurrentState {
        let active = physical("DP-2", true);
        let logical = (
            0,
            0,
            4.0 / 3.0,
            0,
            true,
            vec![active.0.clone()],
            HashMap::new(),
        );
        (
            1,
            vec![physical("DP-1", false), active],
            vec![logical],
            HashMap::new(),
        )
    }

    #[test]
    fn inactive_outputs_do_not_expand_fractionally_scaled_desktop() {
        assert_eq!(
            from_state(&state()).unwrap(),
            Desktop {
                width: 2880,
                height: 1620
            }
        );
    }

    #[test]
    fn mirrored_outputs_count_once_and_rotation_swaps_dimensions() {
        let mut state = state();
        let mirror = physical("HDMI-1", true);
        state.2[0].5.push(mirror.0.clone());
        state.1.push(mirror);
        for transform in [1, 3, 5, 7] {
            state.2[0].3 = transform;
            assert_eq!(
                from_state(&state).unwrap(),
                Desktop {
                    width: 1620,
                    height: 2880
                }
            );
        }
    }

    #[test]
    fn physical_layout_uses_os_coordinates_without_dividing_scale() {
        let mut state = state();
        state.3.insert("layout-mode".into(), OwnedValue::from(2u32));
        assert_eq!(
            from_state(&state).unwrap(),
            Desktop {
                width: 3840,
                height: 2160
            }
        );
    }

    #[test]
    fn mixed_scale_groups_keep_positions_and_fractional_rounding() {
        let mut state = state();
        let second = physical("HDMI-1", true);
        state.2.push((
            -1920,
            -200,
            2.0,
            0,
            false,
            vec![second.0.clone()],
            HashMap::new(),
        ));
        state.1.push(second);
        assert_eq!(
            from_state(&state).unwrap(),
            Desktop {
                width: 4800,
                height: 1820
            }
        );
        state.2[0].2 = 1.3333333730697632;
        assert_eq!(
            from_state(&state).unwrap(),
            Desktop {
                width: 4800,
                height: 1820
            }
        );
    }

    #[test]
    fn invalid_scale_and_missing_active_modes_do_not_advertise_guessed_bounds() {
        let mut state = state();
        state.2[0].2 = f64::NAN;
        assert!(from_state(&state).is_err());
        state.2[0].2 = 1.0;
        state.2[0].5 = vec![state.1[0].0.clone()];
        assert!(from_state(&state).is_err());
    }
}
