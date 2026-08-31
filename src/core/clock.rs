//! Sender-to-receiver monotonic clock mapping.
//!
//! The mapper deliberately knows nothing about wall clocks or authentication.
//! It fits a small, bounded affine model to probe observations and leaves all
//! lifecycle decisions to its caller.

use std::{collections::VecDeque, error::Error, fmt, time::Duration};

use super::MonotonicTimeMicros;

const DEFAULT_SAMPLE_WINDOW: usize = 64;
const DEFAULT_MAXIMUM_SKEW_PPM: u32 = 1_000;

/// Runtime-tunable limits for the affine clock fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockConfig {
    /// Maximum absolute deviation from a 1:1 clock rate.
    pub maximum_skew_ppm: u32,
    /// Number of recent probe observations retained by the fit.
    pub sample_window: usize,
}

impl Default for ClockConfig {
    fn default() -> Self {
        Self {
            maximum_skew_ppm: DEFAULT_MAXIMUM_SKEW_PPM,
            sample_window: DEFAULT_SAMPLE_WINDOW,
        }
    }
}

impl ClockConfig {
    pub fn validate(self) -> Result<Self, ClockError> {
        if self.sample_window < 2 {
            return Err(ClockError::InvalidConfig(
                "clock sample window must contain at least two observations",
            ));
        }
        if self.maximum_skew_ppm > 100_000 {
            return Err(ClockError::InvalidConfig(
                "maximum clock skew must not exceed 100000 ppm",
            ));
        }
        Ok(self)
    }
}

/// One paired observation from the sender and receiver monotonic clocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockSample {
    pub sender_time: MonotonicTimeMicros,
    pub receiver_time: MonotonicTimeMicros,
    /// Half of the probe's unexplained round-trip time, when known.
    pub uncertainty: Duration,
}

impl ClockSample {
    pub const fn exact(
        sender_time: MonotonicTimeMicros,
        receiver_time: MonotonicTimeMicros,
    ) -> Self {
        Self {
            sender_time,
            receiver_time,
            uncertainty: Duration::ZERO,
        }
    }
}

/// Four timestamps from a receiver-originated probe and sender echo.
///
/// The timestamp ordering on each host is independently monotonic. The clock
/// origins are unrelated, so only durations within one host are compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeExchange {
    pub receiver_sent_at: MonotonicTimeMicros,
    pub sender_received_at: MonotonicTimeMicros,
    pub sender_echoed_at: MonotonicTimeMicros,
    pub receiver_received_at: MonotonicTimeMicros,
}

/// Why an established affine fit was explicitly discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockResetReason {
    Suspend,
    SessionEpochTransition,
    MonotonicDiscontinuity,
    Manual,
}

/// A bounded snapshot suitable for metrics and diagnostics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClockStats {
    /// Intercept of `receiver = offset + skew * sender`, in microseconds.
    pub offset_micros: Option<f64>,
    /// Receiver clock ticks per sender clock tick.
    pub skew: Option<f64>,
    pub skew_ppm: Option<f64>,
    /// Root-mean-square residual over the current fit window.
    pub residual_error_micros: Option<f64>,
    pub last_residual_micros: Option<f64>,
    pub active_sample_count: usize,
    pub accepted_sample_count: u64,
    pub rejected_sample_count: u64,
    pub reset_count: u64,
    pub last_reset_reason: Option<ClockResetReason>,
}

#[derive(Debug, Clone, Copy)]
struct Fit {
    sender_origin: MonotonicTimeMicros,
    receiver_origin: MonotonicTimeMicros,
    centered_intercept: f64,
    skew: f64,
    residual_error_micros: f64,
    last_residual_micros: f64,
}

/// Recent-window affine sender-to-receiver clock estimator.
#[derive(Debug, Clone)]
pub struct ClockMapper {
    config: ClockConfig,
    samples: VecDeque<ClockSample>,
    fit: Option<Fit>,
    arrival_bootstrap: bool,
    accepted_sample_count: u64,
    rejected_sample_count: u64,
    reset_count: u64,
    last_reset_reason: Option<ClockResetReason>,
}

impl ClockMapper {
    pub fn new(config: ClockConfig) -> Result<Self, ClockError> {
        let config = config.validate()?;
        Ok(Self {
            config,
            samples: VecDeque::with_capacity(config.sample_window),
            fit: None,
            arrival_bootstrap: false,
            accepted_sample_count: 0,
            rejected_sample_count: 0,
            reset_count: 0,
            last_reset_reason: None,
        })
    }

    pub fn config(&self) -> ClockConfig {
        self.config
    }

    /// Applies new tuning without throwing away useful observations.
    pub fn set_config(&mut self, config: ClockConfig) -> Result<(), ClockError> {
        let config = config.validate()?;
        self.config = config;
        while self.samples.len() > config.sample_window {
            self.samples.pop_front();
        }
        self.refit();
        Ok(())
    }

    pub fn is_ready(&self) -> bool {
        self.fit.is_some()
    }

    pub fn uses_arrival_bootstrap(&self) -> bool {
        self.arrival_bootstrap
    }

    /// Establishes a temporary mapping from the first authenticated motion
    /// arrival. The first valid probe replaces, rather than refits with, this
    /// network-delay-biased observation.
    pub fn bootstrap_from_arrival(
        &mut self,
        sender_time: MonotonicTimeMicros,
        receiver_time: MonotonicTimeMicros,
    ) -> Result<ClockStats, ClockError> {
        if self.is_ready() {
            return Err(ClockError::InvalidSample(
                "arrival bootstrap requires an empty clock fit",
            ));
        }
        let stats = self.ingest_sample(ClockSample::exact(sender_time, receiver_time))?;
        self.arrival_bootstrap = true;
        Ok(stats)
    }

    /// Adds an already-paired clock observation.
    pub fn ingest_sample(&mut self, sample: ClockSample) -> Result<ClockStats, ClockError> {
        if sample.uncertainty.as_micros() > u128::from(u64::MAX) {
            self.rejected_sample_count = self.rejected_sample_count.saturating_add(1);
            return Err(ClockError::InvalidSample(
                "clock sample uncertainty does not fit in microseconds",
            ));
        }
        if self.samples.len() == self.config.sample_window {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
        self.arrival_bootstrap = false;
        self.accepted_sample_count = self.accepted_sample_count.saturating_add(1);
        self.refit();
        Ok(self.stats())
    }

    /// Converts an NTP-style four-timestamp exchange into one midpoint sample.
    pub fn ingest_probe(&mut self, probe: ProbeExchange) -> Result<ClockStats, ClockError> {
        let receiver_round_trip = match probe
            .receiver_received_at
            .0
            .checked_sub(probe.receiver_sent_at.0)
        {
            Some(duration) => duration,
            None => return self.reject_probe("receiver probe clock moved backwards"),
        };
        let sender_processing = match probe
            .sender_echoed_at
            .0
            .checked_sub(probe.sender_received_at.0)
        {
            Some(duration) => duration,
            None => return self.reject_probe("sender probe clock moved backwards"),
        };

        // Clock skew is bounded tightly enough that treating the two short
        // durations as the same unit is conservative for malformed exchanges.
        if sender_processing > receiver_round_trip {
            return self.reject_probe("sender processing exceeded receiver round trip");
        }

        let sender_midpoint = midpoint(probe.sender_received_at.0, probe.sender_echoed_at.0);
        let receiver_midpoint = midpoint(probe.receiver_sent_at.0, probe.receiver_received_at.0);
        let uncertainty = Duration::from_micros((receiver_round_trip - sender_processing) / 2);
        if self.arrival_bootstrap {
            self.samples.clear();
            self.fit = None;
        }
        self.ingest_sample(ClockSample {
            sender_time: MonotonicTimeMicros(sender_midpoint),
            receiver_time: MonotonicTimeMicros(receiver_midpoint),
            uncertainty,
        })
    }

    /// Maps a sender timestamp into the receiver monotonic domain.
    pub fn map(&self, sender_time: MonotonicTimeMicros) -> Result<MonotonicTimeMicros, ClockError> {
        let fit = self.fit.ok_or(ClockError::NotReady)?;
        let mapped = mapped_micros(fit, sender_time);
        if !mapped.is_finite() || !(0.0..=u64::MAX as f64).contains(&mapped) {
            return Err(ClockError::MappedTimeOutOfRange);
        }
        Ok(MonotonicTimeMicros(mapped.round() as u64))
    }

    pub fn stats(&self) -> ClockStats {
        let offset_micros = self.fit.map(|fit| {
            fit.receiver_origin.0 as f64 + fit.centered_intercept
                - fit.skew * fit.sender_origin.0 as f64
        });
        ClockStats {
            offset_micros,
            skew: self.fit.map(|fit| fit.skew),
            skew_ppm: self.fit.map(|fit| (fit.skew - 1.0) * 1_000_000.0),
            residual_error_micros: self.fit.map(|fit| fit.residual_error_micros),
            last_residual_micros: self.fit.map(|fit| fit.last_residual_micros),
            active_sample_count: self.samples.len(),
            accepted_sample_count: self.accepted_sample_count,
            rejected_sample_count: self.rejected_sample_count,
            reset_count: self.reset_count,
            last_reset_reason: self.last_reset_reason,
        }
    }

    /// Clears the fit while preserving lifetime counters.
    pub fn reset(&mut self, reason: ClockResetReason) {
        self.samples.clear();
        self.fit = None;
        self.arrival_bootstrap = false;
        self.reset_count = self.reset_count.saturating_add(1);
        self.last_reset_reason = Some(reason);
    }

    pub fn on_suspend(&mut self) {
        self.reset(ClockResetReason::Suspend);
    }

    pub fn on_session_epoch_transition(&mut self) {
        self.reset(ClockResetReason::SessionEpochTransition);
    }

    pub fn on_monotonic_discontinuity(&mut self) {
        self.reset(ClockResetReason::MonotonicDiscontinuity);
    }

    fn reject_probe(&mut self, message: &'static str) -> Result<ClockStats, ClockError> {
        self.rejected_sample_count = self.rejected_sample_count.saturating_add(1);
        Err(ClockError::InvalidSample(message))
    }

    fn refit(&mut self) {
        let Some(first) = self.samples.front().copied() else {
            self.fit = None;
            return;
        };

        let count = self.samples.len() as f64;
        let (sum_x, sum_y) = self.samples.iter().fold((0.0, 0.0), |(x, y), sample| {
            (
                x + signed_difference(sample.sender_time.0, first.sender_time.0),
                y + signed_difference(sample.receiver_time.0, first.receiver_time.0),
            )
        });
        let mean_x = sum_x / count;
        let mean_y = sum_y / count;
        let (variance, covariance) =
            self.samples
                .iter()
                .fold((0.0, 0.0), |(variance, covariance), sample| {
                    let x = signed_difference(sample.sender_time.0, first.sender_time.0) - mean_x;
                    let y =
                        signed_difference(sample.receiver_time.0, first.receiver_time.0) - mean_y;
                    (variance + x * x, covariance + x * y)
                });

        let maximum_skew = f64::from(self.config.maximum_skew_ppm) / 1_000_000.0;
        let skew = if variance > f64::EPSILON {
            (covariance / variance).clamp(1.0 - maximum_skew, 1.0 + maximum_skew)
        } else {
            1.0
        };
        let centered_intercept = mean_y - skew * mean_x;

        let mut residual_squared = 0.0;
        let mut last_residual = 0.0;
        for sample in &self.samples {
            let x = signed_difference(sample.sender_time.0, first.sender_time.0);
            let observed = signed_difference(sample.receiver_time.0, first.receiver_time.0);
            let residual = observed - (centered_intercept + skew * x);
            residual_squared += residual * residual;
            last_residual = residual;
        }

        self.fit = Some(Fit {
            sender_origin: first.sender_time,
            receiver_origin: first.receiver_time,
            centered_intercept,
            skew,
            residual_error_micros: (residual_squared / count).sqrt(),
            last_residual_micros: last_residual,
        });
    }
}

impl Default for ClockMapper {
    fn default() -> Self {
        Self::new(ClockConfig::default()).expect("default clock config is valid")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockError {
    InvalidConfig(&'static str),
    InvalidSample(&'static str),
    NotReady,
    MappedTimeOutOfRange,
}

impl fmt::Display for ClockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) | Self::InvalidSample(message) => {
                formatter.write_str(message)
            }
            Self::NotReady => formatter.write_str("clock mapping has no probe observations"),
            Self::MappedTimeOutOfRange => {
                formatter.write_str("mapped receiver time is outside its monotonic range")
            }
        }
    }
}

impl Error for ClockError {}

fn midpoint(start: u64, end: u64) -> u64 {
    start + (end - start) / 2
}

fn signed_difference(value: u64, origin: u64) -> f64 {
    if value >= origin {
        (value - origin) as f64
    } else {
        -((origin - value) as f64)
    }
}

fn mapped_micros(fit: Fit, sender_time: MonotonicTimeMicros) -> f64 {
    fit.receiver_origin.0 as f64
        + fit.centered_intercept
        + fit.skew * signed_difference(sender_time.0, fit.sender_origin.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time(value: u64) -> MonotonicTimeMicros {
        MonotonicTimeMicros(value)
    }

    #[test]
    fn affine_fit_recovers_offset_and_skew() {
        let mut mapper = ClockMapper::default();
        for sender in (1_000_000..=6_000_000).step_by(1_000_000) {
            let receiver = 75_000 + sender + sender / 5_000;
            mapper
                .ingest_sample(ClockSample::exact(time(sender), time(receiver)))
                .unwrap();
        }

        let mapped = mapper.map(time(7_500_000)).unwrap();
        assert_eq!(mapped, time(7_576_500));
        let stats = mapper.stats();
        assert!((stats.offset_micros.unwrap() - 75_000.0).abs() < 0.001);
        assert!((stats.skew_ppm.unwrap() - 200.0).abs() < 0.001);
        assert!(stats.residual_error_micros.unwrap() < 0.001);
    }

    #[test]
    fn skew_is_bounded_by_configuration() {
        let mut mapper = ClockMapper::new(ClockConfig {
            maximum_skew_ppm: 100,
            ..ClockConfig::default()
        })
        .unwrap();
        mapper
            .ingest_sample(ClockSample::exact(time(0), time(10_000)))
            .unwrap();
        mapper
            .ingest_sample(ClockSample::exact(time(1_000_000), time(1_015_000)))
            .unwrap();

        assert!((mapper.stats().skew_ppm.unwrap() - 100.0).abs() < 0.001);
        assert!(mapper.stats().residual_error_micros.unwrap() > 1_000.0);
    }

    #[test]
    fn probe_midpoints_create_a_sender_to_receiver_observation() {
        let mut mapper = ClockMapper::default();
        mapper
            .ingest_probe(ProbeExchange {
                receiver_sent_at: time(10_000),
                sender_received_at: time(51_000),
                sender_echoed_at: time(51_200),
                receiver_received_at: time(12_200),
            })
            .unwrap();

        assert_eq!(mapper.map(time(52_000)).unwrap(), time(12_000));
        assert_eq!(mapper.samples[0].uncertainty, Duration::from_millis(1));
    }

    #[test]
    fn malformed_probe_is_counted_without_destroying_the_fit() {
        let mut mapper = ClockMapper::default();
        mapper
            .ingest_sample(ClockSample::exact(time(1), time(2)))
            .unwrap();
        let error = mapper
            .ingest_probe(ProbeExchange {
                receiver_sent_at: time(20),
                sender_received_at: time(50),
                sender_echoed_at: time(40),
                receiver_received_at: time(30),
            })
            .unwrap_err();

        assert!(matches!(error, ClockError::InvalidSample(_)));
        assert!(mapper.is_ready());
        assert_eq!(mapper.stats().rejected_sample_count, 1);
    }

    #[test]
    fn first_probe_replaces_network_delay_biased_arrival_bootstrap() {
        let mut mapper = ClockMapper::default();
        mapper
            .bootstrap_from_arrival(time(100_000), time(150_000))
            .unwrap();
        assert!(mapper.uses_arrival_bootstrap());

        mapper
            .ingest_probe(ProbeExchange {
                receiver_sent_at: time(200_000),
                sender_received_at: time(200_500),
                sender_echoed_at: time(200_500),
                receiver_received_at: time(201_000),
            })
            .unwrap();

        assert!(!mapper.uses_arrival_bootstrap());
        assert_eq!(mapper.stats().active_sample_count, 1);
        assert_eq!(mapper.map(time(201_000)).unwrap(), time(201_000));
    }

    #[test]
    fn lifecycle_resets_clear_fit_and_preserve_counters() {
        let mut mapper = ClockMapper::default();
        mapper
            .ingest_sample(ClockSample::exact(time(1), time(2)))
            .unwrap();
        mapper.on_suspend();
        assert!(!mapper.is_ready());
        assert_eq!(mapper.stats().reset_count, 1);
        assert_eq!(
            mapper.stats().last_reset_reason,
            Some(ClockResetReason::Suspend)
        );

        mapper
            .ingest_sample(ClockSample::exact(time(3), time(4)))
            .unwrap();
        mapper.on_session_epoch_transition();
        mapper.on_monotonic_discontinuity();
        assert_eq!(mapper.stats().accepted_sample_count, 2);
        assert_eq!(mapper.stats().reset_count, 3);
        assert!(matches!(mapper.map(time(5)), Err(ClockError::NotReady)));
    }

    #[test]
    fn recent_window_bounds_memory_and_reacts_to_new_fit() {
        let mut mapper = ClockMapper::new(ClockConfig {
            sample_window: 3,
            ..ClockConfig::default()
        })
        .unwrap();
        for sender in 0..6 {
            mapper
                .ingest_sample(ClockSample::exact(
                    time(sender * 1_000),
                    time(sender * 1_000),
                ))
                .unwrap();
        }
        assert_eq!(mapper.stats().active_sample_count, 3);
        assert_eq!(mapper.stats().accepted_sample_count, 6);
    }
}
