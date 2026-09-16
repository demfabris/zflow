use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, ensure};

use super::Desktop;

#[derive(Default)]
struct Detection {
    running: bool,
    desktop: Option<Desktop>,
    error: Option<String>,
}

#[derive(Default)]
pub(crate) struct DesktopDetector {
    state: Arc<Mutex<Detection>>,
}

impl DesktopDetector {
    pub fn snapshot(&self) -> (Option<Desktop>, Option<String>) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.desktop, state.error.clone())
    }

    pub fn refresh(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.running {
            return;
        }
        state.running = true;
        drop(state);
        let shared = self.state.clone();

        if let Err(error) = std::thread::Builder::new()
            .name("zflow-desktop".into())
            .spawn(move || {
                let result = detect();
                let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                state.running = false;
                match result {
                    Ok(desktop) => {
                        state.desktop = Some(desktop);
                        state.error = None;
                    }
                    Err(error) => {
                        state.desktop = None;
                        state.error = Some(format!("{error:#}"));
                    }
                }
            })
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.running = false;
            state.desktop = None;
            state.error = Some(format!("Could not read desktop size: {error}"));
        }
    }
}

#[derive(Clone, Copy)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

fn desktop_bounds(rects: impl IntoIterator<Item = Rect>) -> Result<Desktop> {
    let mut bounds: Option<(f64, f64, f64, f64)> = None;
    for rect in rects {
        ensure!(
            [rect.x, rect.y, rect.width, rect.height]
                .into_iter()
                .all(f64::is_finite)
                && rect.width > 0.0
                && rect.height > 0.0,
            "The OS returned invalid desktop bounds"
        );
        let right = rect.x + rect.width;
        let bottom = rect.y + rect.height;
        bounds = Some(match bounds {
            Some((left, top, old_right, old_bottom)) => (
                left.min(rect.x),
                top.min(rect.y),
                old_right.max(right),
                old_bottom.max(bottom),
            ),
            None => (rect.x, rect.y, right, bottom),
        });
    }
    let (left, top, right, bottom) = bounds.context("No active desktop found")?;
    let desktop = Desktop {
        width: (right - left).round() as u32,
        height: (bottom - top).round() as u32,
    };
    ensure!(
        desktop.valid(),
        "Desktop dimensions must be between 1 and 16384"
    );
    Ok(desktop)
}

#[cfg(target_os = "macos")]
fn detect() -> Result<Desktop> {
    desktop_bounds(
        crate::macos::active_desktop_rectangles()?
            .into_iter()
            .map(|bounds| Rect {
                x: bounds.x,
                y: bounds.y,
                width: bounds.width,
                height: bounds.height,
            }),
    )
}

#[cfg(target_os = "linux")]
mod gnome;

#[cfg(target_os = "linux")]
fn detect() -> Result<Desktop> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(3), gnome::detect())
                .await
                .context("Desktop detection timed out")?
        })
        .context(
            "Could not read the GNOME desktop. Linux desktop detection currently requires GNOME",
        )
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn detect() -> Result<Desktop> {
    anyhow::bail!("Desktop detection currently supports macOS and GNOME")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_preserve_negative_origins_and_do_not_sum_mirrors_or_overlap() {
        let left = Rect {
            x: -2688.0,
            y: 0.0,
            width: 2688.0,
            height: 1512.0,
        };
        let right = Rect {
            x: 0.0,
            y: 0.0,
            width: 3008.0,
            height: 1692.0,
        };
        assert_eq!(
            desktop_bounds([left, right]).unwrap(),
            Desktop {
                width: 5696,
                height: 1692
            }
        );
        assert_eq!(
            desktop_bounds([right, right]).unwrap(),
            Desktop {
                width: 3008,
                height: 1692
            }
        );
        assert_eq!(
            desktop_bounds([
                right,
                Rect {
                    x: 100.0,
                    y: -100.0,
                    ..right
                }
            ])
            .unwrap(),
            Desktop {
                width: 3108,
                height: 1792
            }
        );
    }

    #[test]
    fn bounds_reject_empty_invalid_and_oversized_desktops() {
        assert!(desktop_bounds([]).is_err());
        for width in [0.0, -1.0, f64::NAN, f64::INFINITY, 16385.0] {
            assert!(
                desktop_bounds([Rect {
                    x: 0.0,
                    y: 0.0,
                    width,
                    height: 100.0
                }])
                .is_err()
            );
        }
    }

    #[test]
    #[ignore = "reads the current desktop session; run with --ignored --nocapture"]
    fn native_desktop_geometry() {
        println!("Native desktop: {:?}", detect().unwrap());
    }
}
