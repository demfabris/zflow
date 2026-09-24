use std::collections::{BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

const DEFAULT_WINDOW: usize = 4_096;
const SEQUENCE_TRACKING_WINDOW: usize = 4_096;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SampleSummary {
    pub count: usize,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub p999: f64,
    pub maximum: f64,
}

#[derive(Debug, Clone)]
pub struct SampleWindow {
    capacity: usize,
    values: VecDeque<f64>,
}

impl Default for SampleWindow {
    fn default() -> Self {
        Self::new(DEFAULT_WINDOW)
    }
}

impl SampleWindow {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "sample capacity must be non-zero");
        Self {
            capacity,
            values: VecDeque::with_capacity(capacity),
        }
    }

    pub fn record(&mut self, value: f64) {
        if !value.is_finite() {
            return;
        }
        if self.values.len() == self.capacity {
            self.values.pop_front();
        }
        self.values.push_back(value);
    }

    pub fn summary(&self) -> Option<SampleSummary> {
        self.clone().into_summary()
    }

    fn into_summary(self) -> Option<SampleSummary> {
        if self.values.is_empty() {
            return None;
        }
        let mut sorted = Vec::from(self.values);
        sorted.sort_by(f64::total_cmp);
        Some(SampleSummary {
            count: sorted.len(),
            p50: percentile(&sorted, 0.50),
            p95: percentile(&sorted, 0.95),
            p99: percentile(&sorted, 0.99),
            p999: percentile(&sorted, 0.999),
            maximum: *sorted.last().unwrap(),
        })
    }
}

fn percentile(sorted: &[f64], quantile: f64) -> f64 {
    let index = ((sorted.len() - 1) as f64 * quantile).ceil() as usize;
    sorted[index]
}

/// Declares the live session metrics and their snapshot from one field list.
/// A window becomes an optional percentile summary; a value is copied as is.
macro_rules! session_metrics {
    (
        windows { $($(#[$window_doc:meta])* $window:ident,)* }
        values { $($(#[$value_doc:meta])* $value:ident: $value_type:ty,)* }
    ) => {
        #[derive(Debug, Default)]
        pub struct SessionMetrics {
            $($(#[$window_doc])* pub $window: SampleWindow,)*
            $($(#[$value_doc])* pub $value: $value_type,)*
            highest_motion_sequence: u64,
            recent_motion_sequences: BTreeSet<u64>,
            last_packet_delay_us: Option<u64>,
        }

        #[derive(Debug, Clone, Serialize, Deserialize)]
        pub struct SessionMetricsSnapshot {
            $($(#[$window_doc])* pub $window: Option<SampleSummary>,)*
            $($(#[$value_doc])* pub $value: $value_type,)*
        }

        /// A copy taken under the session lock. Sorting the windows waits
        /// until the lock is released.
        #[derive(Debug)]
        pub(crate) struct SessionMetricsSnapshotData {
            $($window: SampleWindow,)*
            $($value: $value_type,)*
        }

        impl SessionMetrics {
            pub(crate) fn snapshot_data(&self) -> SessionMetricsSnapshotData {
                SessionMetricsSnapshotData {
                    $($window: self.$window.clone(),)*
                    $($value: self.$value,)*
                }
            }
        }

        impl SessionMetricsSnapshotData {
            pub(crate) fn summarize(self) -> SessionMetricsSnapshot {
                SessionMetricsSnapshot {
                    $($window: self.$window.into_summary(),)*
                    $($value: self.$value,)*
                }
            }
        }
    };
}

session_metrics! {
    windows {
        capture_to_send_us,
        /// Network receive through successful uinput application.
        receive_to_inject_us,
        /// Network receive through accepted runtime-command dispatch.
        receive_to_runtime_dispatch_us,
        arming_to_grab_us,
        rtt_us,
        delay_variation_us,
        /// Samples of the playout engine's rolling packet-delay percentile, not
        /// raw per-datagram delay variation.
        adaptive_delay_variation_percentile_us,
        playout_delay_us,
        /// How far past its deadline the session timer woke.
        scheduler_lateness_us,
        clock_residual_us,
    }
    values {
        /// Mappable evdev events seen during the local Arming window, before the
        /// physical devices were grabbed.
        switch_time_leakage_events: u64,
        clock_offset_us: Option<f64>,
        clock_skew: Option<f64>,
        clock_skew_ppm: Option<f64>,
        /// Unrecovered motion-sequence gaps observed in this live session.
        loss: u64,
        reordered: u64,
        duplicate_datagrams: u64,
        /// Datagrams replaced in the bounded latest-wins application queue.
        datagram_queue_drops: u64,
        /// Motion targets that started playing after their playout deadline.
        scheduler_late_events: u64,
        catch_up_steps: u64,
        catch_up_pointer_units: u64,
        catch_up_scroll_units: u64,
        lease_renewals: u64,
        snapshot_acknowledgements: u64,
        synthetic_releases: u64,
        stale_events_rejected: u64,
        /// Keys and buttons the local input backend cannot inject.
        unsupported_inputs_dropped: u64,
    }
}

impl SessionMetrics {
    pub fn begin_activation(&mut self) {
        self.highest_motion_sequence = 0;
        self.recent_motion_sequences.clear();
        self.last_packet_delay_us = None;
    }

    pub fn observe_packet_delay(&mut self, delay_us: u64) {
        if let Some(previous) = self.last_packet_delay_us {
            self.delay_variation_us
                .record(delay_us.abs_diff(previous) as f64);
        }
        self.last_packet_delay_us = Some(delay_us);
    }

    /// Tracks sequence gaps without retaining an unbounded session history.
    /// Late arrivals repair the loss estimate while they remain in the recent
    /// window. Older arrivals are not guessed at because they may be repeats.
    pub fn observe_motion_sequence(&mut self, sequence: u64) {
        if sequence == 0 {
            return;
        }
        if sequence > self.highest_motion_sequence {
            self.loss = self.loss.saturating_add(
                sequence
                    .saturating_sub(self.highest_motion_sequence)
                    .saturating_sub(1),
            );
            self.highest_motion_sequence = sequence;
            self.recent_motion_sequences.insert(sequence);
            let floor = sequence.saturating_sub(SEQUENCE_TRACKING_WINDOW as u64 - 1);
            self.recent_motion_sequences = self.recent_motion_sequences.split_off(&floor);
            return;
        }

        let floor = self
            .highest_motion_sequence
            .saturating_sub(SEQUENCE_TRACKING_WINDOW as u64 - 1);
        if sequence < floor {
            return;
        }
        if self.recent_motion_sequences.insert(sequence) {
            self.reordered = self.reordered.saturating_add(1);
            self.loss = self.loss.saturating_sub(1);
        } else {
            self.duplicate_datagrams = self.duplicate_datagrams.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_is_bounded_and_reports_percentiles() {
        let mut window = SampleWindow::new(4);
        for value in 1..=6 {
            window.record(value as f64);
        }
        let summary = window.summary().unwrap();
        assert_eq!(summary.count, 4);
        assert_eq!(summary.p50, 5.0);
        assert_eq!(summary.maximum, 6.0);
    }

    #[test]
    fn non_finite_samples_are_ignored() {
        let mut window = SampleWindow::new(2);
        window.record(f64::NAN);
        window.record(f64::INFINITY);
        assert!(window.summary().is_none());
    }

    #[test]
    fn sequence_gaps_are_repaired_by_bounded_late_arrivals() {
        let mut metrics = SessionMetrics::default();
        metrics.begin_activation();
        metrics.observe_motion_sequence(1);
        metrics.observe_motion_sequence(3);
        assert_eq!(metrics.loss, 1);

        metrics.observe_motion_sequence(2);
        assert_eq!(metrics.loss, 0);
        assert_eq!(metrics.reordered, 1);

        metrics.observe_motion_sequence(2);
        assert_eq!(metrics.duplicate_datagrams, 1);
    }

    #[test]
    fn packet_delay_variation_uses_consecutive_absolute_deltas() {
        let mut metrics = SessionMetrics::default();
        metrics.observe_packet_delay(1_000);
        metrics.observe_packet_delay(1_600);
        metrics.observe_packet_delay(1_100);

        let summary = metrics.delay_variation_us.summary().unwrap();
        assert_eq!(summary.count, 2);
        assert_eq!(summary.maximum, 600.0);
    }

    #[test]
    fn detached_snapshot_preserves_samples_and_counters() {
        let mut metrics = SessionMetrics::default();
        metrics.capture_to_send_us.record(10.0);
        metrics.capture_to_send_us.record(20.0);
        metrics.loss = 3;

        let snapshot = metrics.snapshot_data().summarize();

        assert_eq!(snapshot.capture_to_send_us.unwrap().maximum, 20.0);
        assert_eq!(snapshot.loss, 3);
    }

    #[test]
    fn snapshot_json_keeps_its_field_names() {
        let snapshot = SessionMetrics::default().snapshot_data().summarize();
        let json = serde_json::to_value(snapshot).unwrap();
        let names: BTreeSet<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            names,
            BTreeSet::from([
                "adaptive_delay_variation_percentile_us",
                "arming_to_grab_us",
                "capture_to_send_us",
                "catch_up_pointer_units",
                "catch_up_scroll_units",
                "catch_up_steps",
                "clock_offset_us",
                "clock_residual_us",
                "clock_skew",
                "clock_skew_ppm",
                "datagram_queue_drops",
                "delay_variation_us",
                "duplicate_datagrams",
                "lease_renewals",
                "loss",
                "playout_delay_us",
                "receive_to_inject_us",
                "receive_to_runtime_dispatch_us",
                "reordered",
                "rtt_us",
                "scheduler_late_events",
                "scheduler_lateness_us",
                "snapshot_acknowledgements",
                "stale_events_rejected",
                "switch_time_leakage_events",
                "synthetic_releases",
                "unsupported_inputs_dropped",
            ])
        );
    }
}
