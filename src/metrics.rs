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
        if self.values.is_empty() {
            return None;
        }
        let mut sorted: Vec<_> = self.values.iter().copied().collect();
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

#[derive(Debug, Default)]
pub struct SessionMetrics {
    pub capture_to_send_us: SampleWindow,
    pub receive_to_inject_us: SampleWindow,
    pub receive_to_runtime_dispatch_us: SampleWindow,
    pub arming_to_grab_us: SampleWindow,
    pub switch_time_leakage_events: u64,
    pub rtt_us: SampleWindow,
    pub delay_variation_us: SampleWindow,
    pub adaptive_delay_variation_percentile_us: SampleWindow,
    pub playout_delay_us: SampleWindow,
    pub scheduler_lateness_us: SampleWindow,
    pub clock_residual_us: SampleWindow,
    pub clock_offset_us: Option<f64>,
    pub clock_skew: Option<f64>,
    pub clock_skew_ppm: Option<f64>,
    pub clock_reset_count: u64,
    pub loss: u64,
    pub reordered: u64,
    pub duplicate_datagrams: u64,
    pub datagram_queue_drops: u64,
    pub scheduler_late_events: u64,
    pub catch_up_steps: u64,
    pub catch_up_pointer_units: u64,
    pub catch_up_scroll_units: u64,
    pub lease_renewals: u64,
    pub snapshot_acknowledgements: u64,
    pub synthetic_releases: u64,
    pub stale_events_rejected: u64,
    pub epoch_changes: u64,
    pub generation_changes: u64,
    pub explicit_rebases: u64,
    pub rebase_discarded_pointer_units: u64,
    pub rebase_discarded_scroll_units: u64,
    highest_motion_sequence: u64,
    recent_motion_sequences: BTreeSet<u64>,
    last_packet_delay_us: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetricsSnapshot {
    pub capture_to_send_us: Option<SampleSummary>,
    /// Network receive through successful uinput application.
    pub receive_to_inject_us: Option<SampleSummary>,
    /// Network receive through accepted runtime-command dispatch.
    pub receive_to_runtime_dispatch_us: Option<SampleSummary>,
    pub arming_to_grab_us: Option<SampleSummary>,
    /// Mappable evdev events seen during the local Arming window, before the
    /// physical devices were grabbed.
    pub switch_time_leakage_events: u64,
    pub rtt_us: Option<SampleSummary>,
    pub delay_variation_us: Option<SampleSummary>,
    /// Samples of the playout engine's rolling packet-delay percentile, not
    /// raw per-datagram delay variation.
    pub adaptive_delay_variation_percentile_us: Option<SampleSummary>,
    pub playout_delay_us: Option<SampleSummary>,
    pub scheduler_lateness_us: Option<SampleSummary>,
    pub clock_residual_us: Option<SampleSummary>,
    pub clock_offset_us: Option<f64>,
    pub clock_skew: Option<f64>,
    pub clock_skew_ppm: Option<f64>,
    pub clock_reset_count: u64,
    /// Unrecovered motion-sequence gaps observed in this live session.
    pub loss: u64,
    pub reordered: u64,
    pub duplicate_datagrams: u64,
    /// Datagrams replaced in the bounded latest-wins application queue.
    pub datagram_queue_drops: u64,
    pub scheduler_late_events: u64,
    pub catch_up_steps: u64,
    pub catch_up_pointer_units: u64,
    pub catch_up_scroll_units: u64,
    pub lease_renewals: u64,
    pub snapshot_acknowledgements: u64,
    pub synthetic_releases: u64,
    pub stale_events_rejected: u64,
    pub epoch_changes: u64,
    pub generation_changes: u64,
    pub explicit_rebases: u64,
    pub rebase_discarded_pointer_units: u64,
    pub rebase_discarded_scroll_units: u64,
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

    pub fn snapshot(&self) -> SessionMetricsSnapshot {
        SessionMetricsSnapshot {
            capture_to_send_us: self.capture_to_send_us.summary(),
            receive_to_inject_us: self.receive_to_inject_us.summary(),
            receive_to_runtime_dispatch_us: self.receive_to_runtime_dispatch_us.summary(),
            arming_to_grab_us: self.arming_to_grab_us.summary(),
            switch_time_leakage_events: self.switch_time_leakage_events,
            rtt_us: self.rtt_us.summary(),
            delay_variation_us: self.delay_variation_us.summary(),
            adaptive_delay_variation_percentile_us: self
                .adaptive_delay_variation_percentile_us
                .summary(),
            playout_delay_us: self.playout_delay_us.summary(),
            scheduler_lateness_us: self.scheduler_lateness_us.summary(),
            clock_residual_us: self.clock_residual_us.summary(),
            clock_offset_us: self.clock_offset_us,
            clock_skew: self.clock_skew,
            clock_skew_ppm: self.clock_skew_ppm,
            clock_reset_count: self.clock_reset_count,
            loss: self.loss,
            reordered: self.reordered,
            duplicate_datagrams: self.duplicate_datagrams,
            datagram_queue_drops: self.datagram_queue_drops,
            scheduler_late_events: self.scheduler_late_events,
            catch_up_steps: self.catch_up_steps,
            catch_up_pointer_units: self.catch_up_pointer_units,
            catch_up_scroll_units: self.catch_up_scroll_units,
            lease_renewals: self.lease_renewals,
            snapshot_acknowledgements: self.snapshot_acknowledgements,
            synthetic_releases: self.synthetic_releases,
            stale_events_rejected: self.stale_events_rejected,
            epoch_changes: self.epoch_changes,
            generation_changes: self.generation_changes,
            explicit_rebases: self.explicit_rebases,
            rebase_discarded_pointer_units: self.rebase_discarded_pointer_units,
            rebase_discarded_scroll_units: self.rebase_discarded_scroll_units,
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
}
