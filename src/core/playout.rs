//! Deterministic receiver playout for cumulative motion frames.
//!
//! Playout never predicts input. It waits for both the mapped sender deadline
//! and the reliable-control watermark, selects the newest mature cumulative
//! target, then approaches that target with bounded catch-up steps. Since each
//! step is reconciled against the last successfully emitted totals, the final
//! displacement remains exact through loss, reordering, and jitter bursts.

use std::{
    collections::{BTreeMap, VecDeque},
    error::Error,
    fmt,
    time::Duration,
};

use super::{
    ControlSequence, CumulativeMotion, MonotonicTimeMicros, MotionDelta, MotionFrame,
    MotionOverflow, MotionSequence, SessionContext, TouchState,
    clock::{ClockError, ClockMapper},
};

const DEFAULT_FIXED_DELAY: Duration = Duration::from_millis(8);
const DEFAULT_MINIMUM_DELAY: Duration = Duration::from_millis(3);
const DEFAULT_MAXIMUM_DELAY: Duration = Duration::from_millis(35);
const DEFAULT_SCHEDULER_MARGIN: Duration = Duration::from_millis(2);
const DEFAULT_DELAY_SAMPLE_AGE: Duration = Duration::from_secs(3);
const DEFAULT_SCHEDULER_TICK: Duration = Duration::from_millis(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayoutDelayMode {
    Fixed,
    Adaptive,
}

/// Velocity-scaled catch-up tuning derived from capture timestamps.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CatchUpConfig {
    /// Drain backlog this many times faster than measured sender motion.
    pub speedup: f64,
    /// Absolute lower bound per scheduler tick and two-axis motion family.
    pub minimum_step: u64,
    /// Absolute upper bound per scheduler tick and two-axis motion family.
    pub maximum_step: u64,
    /// Weight of the newest velocity sample in the range `0.0..=1.0`.
    pub velocity_sample_weight: f64,
}

impl Default for CatchUpConfig {
    fn default() -> Self {
        Self {
            speedup: 3.0,
            minimum_step: 5,
            maximum_step: 60,
            velocity_sample_weight: 0.1,
        }
    }
}

/// Runtime-tunable receiver playout configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayoutConfig {
    pub delay_mode: PlayoutDelayMode,
    pub fixed_delay: Duration,
    /// Nearest-rank percentile used in adaptive mode. The measured default is p80.
    pub adaptive_percentile: u8,
    /// Measured treated-link floor.
    pub minimum_delay: Duration,
    /// Measured untreated-link cap.
    pub maximum_delay: Duration,
    pub scheduler_margin: Duration,
    pub delay_sample_window: usize,
    pub delay_sample_max_age: Duration,
    /// Adaptive growth divisor. `1` reaches a larger target immediately.
    pub growth_divisor: u32,
    /// Adaptive contraction divisor. Larger values contract more slowly.
    pub contraction_divisor: u32,
    pub scheduler_tick: Duration,
    pub maximum_queued_frames: usize,
    pub catch_up: CatchUpConfig,
}

impl Default for PlayoutConfig {
    fn default() -> Self {
        Self {
            delay_mode: PlayoutDelayMode::Adaptive,
            fixed_delay: DEFAULT_FIXED_DELAY,
            adaptive_percentile: 80,
            minimum_delay: DEFAULT_MINIMUM_DELAY,
            maximum_delay: DEFAULT_MAXIMUM_DELAY,
            scheduler_margin: DEFAULT_SCHEDULER_MARGIN,
            delay_sample_window: 256,
            delay_sample_max_age: DEFAULT_DELAY_SAMPLE_AGE,
            growth_divisor: 1,
            contraction_divisor: 16,
            scheduler_tick: DEFAULT_SCHEDULER_TICK,
            maximum_queued_frames: 4_096,
            catch_up: CatchUpConfig::default(),
        }
    }
}

impl PlayoutConfig {
    pub fn validate(self) -> Result<Self, PlayoutError> {
        if !(1..=100).contains(&self.adaptive_percentile) {
            return Err(PlayoutError::InvalidConfig(
                "adaptive percentile must be in 1..=100",
            ));
        }
        if self.minimum_delay > self.maximum_delay {
            return Err(PlayoutError::InvalidConfig(
                "minimum playout delay exceeds maximum delay",
            ));
        }
        if self.delay_sample_window == 0 || self.maximum_queued_frames == 0 {
            return Err(PlayoutError::InvalidConfig(
                "playout windows and queues must not be empty",
            ));
        }
        if self.delay_sample_max_age.is_zero() || self.scheduler_tick.is_zero() {
            return Err(PlayoutError::InvalidConfig(
                "sample age and scheduler tick must be positive",
            ));
        }
        if self.growth_divisor == 0 || self.contraction_divisor == 0 {
            return Err(PlayoutError::InvalidConfig(
                "adaptive delay divisors must be positive",
            ));
        }
        if !self.catch_up.speedup.is_finite() || self.catch_up.speedup <= 0.0 {
            return Err(PlayoutError::InvalidConfig(
                "catch-up speedup must be finite and positive",
            ));
        }
        if !self.catch_up.velocity_sample_weight.is_finite()
            || !(0.0..=1.0).contains(&self.catch_up.velocity_sample_weight)
        {
            return Err(PlayoutError::InvalidConfig(
                "velocity sample weight must be in 0.0..=1.0",
            ));
        }
        if self.catch_up.minimum_step == 0
            || self.catch_up.minimum_step > self.catch_up.maximum_step
        {
            return Err(PlayoutError::InvalidConfig(
                "catch-up step bounds must satisfy 0 < minimum <= maximum",
            ));
        }

        duration_micros(self.fixed_delay)?;
        duration_micros(self.minimum_delay)?;
        duration_micros(self.maximum_delay)?;
        duration_micros(self.scheduler_margin)?;
        duration_micros(self.delay_sample_max_age)?;
        duration_micros(self.scheduler_tick)?;
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Queued {
        motion_sequence: MotionSequence,
        deadline: MonotonicTimeMicros,
    },
    Duplicate,
    RetiredByCumulativeTarget,
    RetiredByRebase,
}

/// One backend-neutral playout operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayoutStep {
    /// Receiver-domain capture time for the selected snapshot.
    pub mapped_capture_time: MonotonicTimeMicros,
    pub through_sequence: MotionSequence,
    pub delta: MotionDelta,
    /// A complete newest-wins touch snapshot, emitted once when it matures.
    pub touch_snapshot: Option<TouchState>,
    /// True when this operation reaches the selected cumulative target exactly.
    pub target_reached: bool,
    pub pointer_catch_up_limited: bool,
    pub scroll_catch_up_limited: bool,
}

/// Exact accounting for an explicit, caller-authorized motion-history rebase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebaseRecord {
    pub through_sequence: MotionSequence,
    pub discarded_displacement: MotionDelta,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayoutStats {
    pub current_delay: Duration,
    pub packet_delay_variation_percentile: Option<Duration>,
    pub queued_frames: usize,
    pub highest_seen_sequence: MotionSequence,
    pub selected_target_sequence: MotionSequence,
    pub completed_sequence: MotionSequence,
    pub injected_totals: CumulativeMotion,
    pub late_frame_count: u64,
    pub scheduler_late_count: u64,
    pub maximum_lateness: Duration,
    pub last_scheduler_lateness: Option<Duration>,
    pub catch_up_step_count: u64,
    pub last_packet_delay: Option<Duration>,
    pub duplicate_frame_count: u64,
    pub retired_frame_count: u64,
    pub rebase_count: u64,
    pub last_rebase: Option<RebaseRecord>,
    pub reset_count: u64,
}

#[derive(Debug, Clone)]
struct QueuedFrame {
    frame: MotionFrame,
    mapped_capture_time: MonotonicTimeMicros,
}

#[derive(Debug, Clone)]
struct Target {
    sequence: MotionSequence,
    totals: CumulativeMotion,
    sender_capture_time: Option<MonotonicTimeMicros>,
    mapped_capture_time: MonotonicTimeMicros,
    pending_touch_snapshot: Option<TouchState>,
}

#[derive(Debug, Clone, Copy)]
struct DelaySample {
    received_at: MonotonicTimeMicros,
    packet_delay_micros: u64,
}

#[derive(Debug, Clone, Copy)]
struct VelocitySample {
    sender_capture_time: MonotonicTimeMicros,
    totals: CumulativeMotion,
}

/// Pure cumulative-motion playout state machine.
#[derive(Debug, Clone)]
pub struct ReceiverPlayout {
    config: PlayoutConfig,
    session: SessionContext,
    applied_control_sequence: ControlSequence,
    queue: BTreeMap<MotionSequence, QueuedFrame>,
    delay_samples: VecDeque<DelaySample>,
    adaptive_delay_micros: u64,
    delay_percentile_micros: Option<u64>,
    highest_seen_sequence: MotionSequence,
    target: Target,
    completed_sequence: MotionSequence,
    injected_totals: CumulativeMotion,
    rebase_cutoff: Option<MotionSequence>,
    pointer_velocity_per_micro: f64,
    scroll_velocity_per_micro: f64,
    velocity_sample: Option<VelocitySample>,
    last_observed_time: Option<MonotonicTimeMicros>,
    last_poll_time: Option<MonotonicTimeMicros>,
    late_frame_count: u64,
    scheduler_late_count: u64,
    maximum_lateness_micros: u64,
    last_scheduler_lateness_micros: Option<u64>,
    catch_up_step_count: u64,
    duplicate_frame_count: u64,
    retired_frame_count: u64,
    rebase_count: u64,
    last_rebase: Option<RebaseRecord>,
    reset_count: u64,
}

impl ReceiverPlayout {
    pub fn new(config: PlayoutConfig, session: SessionContext) -> Result<Self, PlayoutError> {
        let config = config.validate()?;
        Ok(Self {
            config,
            session,
            applied_control_sequence: ControlSequence(0),
            queue: BTreeMap::new(),
            delay_samples: VecDeque::with_capacity(config.delay_sample_window),
            adaptive_delay_micros: duration_micros(config.minimum_delay)?,
            delay_percentile_micros: None,
            highest_seen_sequence: MotionSequence(0),
            target: Target {
                sequence: MotionSequence(0),
                totals: CumulativeMotion::ZERO,
                sender_capture_time: None,
                mapped_capture_time: MonotonicTimeMicros(0),
                pending_touch_snapshot: None,
            },
            completed_sequence: MotionSequence(0),
            injected_totals: CumulativeMotion::ZERO,
            rebase_cutoff: None,
            pointer_velocity_per_micro: 0.0,
            scroll_velocity_per_micro: 0.0,
            velocity_sample: None,
            last_observed_time: None,
            last_poll_time: None,
            late_frame_count: 0,
            scheduler_late_count: 0,
            maximum_lateness_micros: 0,
            last_scheduler_lateness_micros: None,
            catch_up_step_count: 0,
            duplicate_frame_count: 0,
            retired_frame_count: 0,
            rebase_count: 0,
            last_rebase: None,
            reset_count: 0,
        })
    }

    pub fn config(&self) -> PlayoutConfig {
        self.config
    }

    /// Changes playout tuning without discarding queued cumulative state.
    pub fn set_config(&mut self, config: PlayoutConfig) -> Result<(), PlayoutError> {
        let config = config.validate()?;
        self.config = config;
        while self.delay_samples.len() > config.delay_sample_window {
            self.delay_samples.pop_front();
        }
        self.recompute_adaptive_delay()?;
        Ok(())
    }

    pub fn session(&self) -> SessionContext {
        self.session
    }

    pub fn current_delay(&self) -> Duration {
        Duration::from_micros(self.current_delay_micros())
    }

    /// Recomputes stored receiver-domain timestamps after a clock model
    /// transition without discarding cumulative motion state.
    pub fn remap_clock(&mut self, clock: &ClockMapper) -> Result<(), PlayoutError> {
        let remapped_queue = self
            .queue
            .iter()
            .map(|(sequence, queued)| {
                clock
                    .map(queued.frame.sender_capture_time)
                    .map(|mapped| (*sequence, mapped))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let remapped_target = self
            .target
            .sender_capture_time
            .map(|sender_time| clock.map(sender_time))
            .transpose()?;

        for (sequence, mapped) in remapped_queue {
            self.queue
                .get_mut(&sequence)
                .expect("remapped sequence came from the queue")
                .mapped_capture_time = mapped;
        }
        if let Some(mapped) = remapped_target {
            self.target.mapped_capture_time = mapped;
        }
        self.delay_samples.clear();
        self.adaptive_delay_micros = duration_micros(self.config.minimum_delay)?;
        self.delay_percentile_micros = None;
        self.pointer_velocity_per_micro = 0.0;
        self.scroll_velocity_per_micro = 0.0;
        self.velocity_sample = None;
        Ok(())
    }

    pub fn advance_control_watermark(
        &mut self,
        applied: ControlSequence,
    ) -> Result<(), PlayoutError> {
        if applied < self.applied_control_sequence {
            return Err(PlayoutError::ControlWatermarkMovedBackwards);
        }
        self.applied_control_sequence = applied;
        Ok(())
    }

    /// Queues a cumulative frame and records its packet-delay observation.
    pub fn ingest_frame(
        &mut self,
        frame: MotionFrame,
        received_at: MonotonicTimeMicros,
        clock: &ClockMapper,
    ) -> Result<EnqueueOutcome, PlayoutError> {
        self.observe_time(received_at)?;
        if frame.session != self.session {
            return Err(PlayoutError::SessionMismatch);
        }
        if self
            .rebase_cutoff
            .is_some_and(|cutoff| frame.motion_sequence <= cutoff)
        {
            self.retired_frame_count = self.retired_frame_count.saturating_add(1);
            return Ok(EnqueueOutcome::RetiredByRebase);
        }
        if frame.motion_sequence <= self.target.sequence {
            // Although the cumulative payload is already represented by a
            // newer target, this authenticated arrival is still valid path
            // evidence. Keeping it in the bounded delay window lets a burst
            // that reorders frames past playout grow the adaptive delay.
            let mapped_capture_time = clock.map(frame.sender_capture_time)?;
            let prior_deadline = add_micros(mapped_capture_time, self.current_delay_micros());
            if received_at > prior_deadline {
                self.record_lateness(received_at.0 - prior_deadline.0, true);
            }
            let packet_delay = received_at.0.saturating_sub(mapped_capture_time.0);
            self.record_delay_sample(received_at, packet_delay)?;
            self.retired_frame_count = self.retired_frame_count.saturating_add(1);
            return Ok(EnqueueOutcome::RetiredByCumulativeTarget);
        }
        if let Some(existing) = self.queue.get(&frame.motion_sequence) {
            if existing.frame == frame {
                self.duplicate_frame_count = self.duplicate_frame_count.saturating_add(1);
                return Ok(EnqueueOutcome::Duplicate);
            }
            return Err(PlayoutError::ConflictingMotionSequence);
        }
        if self.queue.len() == self.config.maximum_queued_frames {
            return Err(PlayoutError::QueueFull);
        }

        let mapped_capture_time = clock.map(frame.sender_capture_time)?;
        self.validate_capture_order(frame.motion_sequence, frame.sender_capture_time)?;

        let prior_deadline = add_micros(mapped_capture_time, self.current_delay_micros());
        if received_at > prior_deadline {
            self.record_lateness(received_at.0 - prior_deadline.0, true);
        }
        let packet_delay = received_at.0.saturating_sub(mapped_capture_time.0);
        self.record_delay_sample(received_at, packet_delay)?;
        let deadline = add_micros(mapped_capture_time, self.current_delay_micros());

        self.highest_seen_sequence = self.highest_seen_sequence.max(frame.motion_sequence);
        let sequence = frame.motion_sequence;
        self.queue.insert(
            sequence,
            QueuedFrame {
                frame,
                mapped_capture_time,
            },
        );
        Ok(EnqueueOutcome::Queued {
            motion_sequence: sequence,
            deadline,
        })
    }

    /// Earliest deadline whose control watermark has already been applied.
    pub fn next_deadline(&self) -> Option<MonotonicTimeMicros> {
        let delay = self.current_delay_micros();
        self.queue
            .values()
            .filter(|queued| queued.frame.control_watermark <= self.applied_control_sequence)
            .map(|queued| add_micros(queued.mapped_capture_time, delay))
            .min()
    }

    /// Advances playout at one receiver-monotonic scheduler instant.
    pub fn poll(&mut self, now: MonotonicTimeMicros) -> Result<Option<PlayoutStep>, PlayoutError> {
        self.observe_time(now)?;
        let elapsed = self.poll_elapsed(now)?;
        self.select_newest_mature_target(now)?;

        let remaining = self
            .target
            .totals
            .checked_delta_from(self.injected_totals)?;
        let touch_snapshot = self.target.pending_touch_snapshot.take();
        if remaining == MotionDelta::default() && touch_snapshot.is_none() {
            self.completed_sequence = self.completed_sequence.max(self.target.sequence);
            return Ok(None);
        }

        let pointer_cap = self.catch_up_cap(self.pointer_velocity_per_micro, elapsed)?;
        let scroll_cap = self.catch_up_cap(self.scroll_velocity_per_micro, elapsed)?;
        let (dx, dy, pointer_limited) = limit_pair(remaining.dx, remaining.dy, pointer_cap);
        let (scroll_x, scroll_y, scroll_limited) =
            limit_pair(remaining.scroll_x, remaining.scroll_y, scroll_cap);
        let delta = MotionDelta {
            dx,
            dy,
            scroll_x,
            scroll_y,
        };

        self.injected_totals = self.injected_totals.checked_add(delta)?;
        let target_reached = self.injected_totals == self.target.totals;
        if pointer_limited || scroll_limited {
            self.catch_up_step_count = self.catch_up_step_count.saturating_add(1);
        }
        if target_reached {
            self.completed_sequence = self.completed_sequence.max(self.target.sequence);
        }

        Ok(Some(PlayoutStep {
            mapped_capture_time: self.target.mapped_capture_time,
            through_sequence: self.target.sequence,
            delta,
            touch_snapshot,
            target_reached,
            pointer_catch_up_limited: pointer_limited,
            scroll_catch_up_limited: scroll_limited,
        }))
    }

    /// Explicitly discards displacement through a named cumulative sequence.
    ///
    /// Nothing calls this automatically for age or lateness. Delayed frames at
    /// or below the named cutoff are retired so they cannot restore history.
    pub fn rebase(
        &mut self,
        through_sequence: MotionSequence,
        authoritative_totals: CumulativeMotion,
    ) -> Result<RebaseRecord, PlayoutError> {
        if through_sequence < self.target.sequence {
            return Err(PlayoutError::RebaseMovedBackwards);
        }
        let discarded_displacement =
            authoritative_totals.checked_delta_from(self.injected_totals)?;
        let record = RebaseRecord {
            through_sequence,
            discarded_displacement,
        };
        self.queue
            .retain(|sequence, _| *sequence > through_sequence);
        self.highest_seen_sequence = self.highest_seen_sequence.max(through_sequence);
        self.target = Target {
            sequence: through_sequence,
            totals: authoritative_totals,
            sender_capture_time: self.target.sender_capture_time,
            mapped_capture_time: self.target.mapped_capture_time,
            pending_touch_snapshot: None,
        };
        self.completed_sequence = through_sequence;
        self.injected_totals = authoritative_totals;
        self.rebase_cutoff = Some(
            self.rebase_cutoff
                .map_or(through_sequence, |cutoff| cutoff.max(through_sequence)),
        );
        self.velocity_sample = None;
        self.pointer_velocity_per_micro = 0.0;
        self.scroll_velocity_per_micro = 0.0;
        self.rebase_count = self.rebase_count.saturating_add(1);
        self.last_rebase = Some(record);
        Ok(record)
    }

    /// Starts a new epoch/activation at zero cumulative state.
    pub fn reset(&mut self, session: SessionContext) {
        self.session = session;
        self.applied_control_sequence = ControlSequence(0);
        self.queue.clear();
        self.delay_samples.clear();
        self.adaptive_delay_micros = duration_micros(self.config.minimum_delay)
            .expect("validated minimum delay fits in microseconds");
        self.delay_percentile_micros = None;
        self.highest_seen_sequence = MotionSequence(0);
        self.target = Target {
            sequence: MotionSequence(0),
            totals: CumulativeMotion::ZERO,
            sender_capture_time: None,
            mapped_capture_time: MonotonicTimeMicros(0),
            pending_touch_snapshot: None,
        };
        self.completed_sequence = MotionSequence(0);
        self.injected_totals = CumulativeMotion::ZERO;
        self.rebase_cutoff = None;
        self.pointer_velocity_per_micro = 0.0;
        self.scroll_velocity_per_micro = 0.0;
        self.velocity_sample = None;
        self.last_observed_time = None;
        self.last_poll_time = None;
        self.last_scheduler_lateness_micros = None;
        self.reset_count = self.reset_count.saturating_add(1);
    }

    pub fn stats(&self) -> PlayoutStats {
        PlayoutStats {
            current_delay: self.current_delay(),
            packet_delay_variation_percentile: self
                .delay_percentile_micros
                .map(Duration::from_micros),
            queued_frames: self.queue.len(),
            highest_seen_sequence: self.highest_seen_sequence,
            selected_target_sequence: self.target.sequence,
            completed_sequence: self.completed_sequence,
            injected_totals: self.injected_totals,
            late_frame_count: self.late_frame_count,
            scheduler_late_count: self.scheduler_late_count,
            maximum_lateness: Duration::from_micros(self.maximum_lateness_micros),
            last_scheduler_lateness: self
                .last_scheduler_lateness_micros
                .map(Duration::from_micros),
            catch_up_step_count: self.catch_up_step_count,
            last_packet_delay: self
                .delay_samples
                .back()
                .map(|sample| Duration::from_micros(sample.packet_delay_micros)),
            duplicate_frame_count: self.duplicate_frame_count,
            retired_frame_count: self.retired_frame_count,
            rebase_count: self.rebase_count,
            last_rebase: self.last_rebase,
            reset_count: self.reset_count,
        }
    }

    fn current_delay_micros(&self) -> u64 {
        match self.config.delay_mode {
            PlayoutDelayMode::Fixed => duration_micros(self.config.fixed_delay)
                .expect("validated fixed delay fits in microseconds"),
            PlayoutDelayMode::Adaptive => self.adaptive_delay_micros,
        }
    }

    fn record_delay_sample(
        &mut self,
        received_at: MonotonicTimeMicros,
        packet_delay_micros: u64,
    ) -> Result<(), PlayoutError> {
        self.delay_samples.push_back(DelaySample {
            received_at,
            packet_delay_micros,
        });
        let maximum_age = duration_micros(self.config.delay_sample_max_age)?;
        while self
            .delay_samples
            .front()
            .is_some_and(|sample| received_at.0.saturating_sub(sample.received_at.0) > maximum_age)
        {
            self.delay_samples.pop_front();
        }
        while self.delay_samples.len() > self.config.delay_sample_window {
            self.delay_samples.pop_front();
        }
        self.recompute_adaptive_delay()
    }

    fn recompute_adaptive_delay(&mut self) -> Result<(), PlayoutError> {
        if self.delay_samples.is_empty() {
            self.delay_percentile_micros = None;
            self.adaptive_delay_micros = duration_micros(self.config.minimum_delay)?;
            return Ok(());
        }

        let minimum = self
            .delay_samples
            .iter()
            .map(|sample| sample.packet_delay_micros)
            .min()
            .expect("non-empty delay sample window has a minimum");
        let mut variations: Vec<u64> = self
            .delay_samples
            .iter()
            .map(|sample| sample.packet_delay_micros - minimum)
            .collect();
        variations.sort_unstable();
        let rank = variations
            .len()
            .saturating_mul(usize::from(self.config.adaptive_percentile))
            .div_ceil(100)
            .saturating_sub(1);
        let percentile = variations[rank];
        self.delay_percentile_micros = Some(percentile);

        let margin = duration_micros(self.config.scheduler_margin)?;
        let minimum_delay = duration_micros(self.config.minimum_delay)?;
        let maximum_delay = duration_micros(self.config.maximum_delay)?;
        let target = percentile
            .saturating_add(margin)
            .clamp(minimum_delay, maximum_delay);
        self.adaptive_delay_micros = approach_asymmetrically(
            self.adaptive_delay_micros,
            target,
            self.config.growth_divisor,
            self.config.contraction_divisor,
        );
        Ok(())
    }

    fn validate_capture_order(
        &self,
        sequence: MotionSequence,
        sender_capture_time: MonotonicTimeMicros,
    ) -> Result<(), PlayoutError> {
        if sequence > self.target.sequence
            && self
                .target
                .sender_capture_time
                .is_some_and(|target_time| sender_capture_time < target_time)
        {
            return Err(PlayoutError::CaptureTimeMovedBackwards);
        }
        if let Some((_, prior)) = self.queue.range(..sequence).next_back()
            && sender_capture_time < prior.frame.sender_capture_time
        {
            return Err(PlayoutError::CaptureTimeMovedBackwards);
        }
        if let Some((_, next)) = self.queue.range(sequence..).next()
            && sender_capture_time > next.frame.sender_capture_time
        {
            return Err(PlayoutError::CaptureTimeMovedBackwards);
        }
        Ok(())
    }

    fn select_newest_mature_target(
        &mut self,
        now: MonotonicTimeMicros,
    ) -> Result<(), PlayoutError> {
        let delay = self.current_delay_micros();
        let candidate = self
            .queue
            .iter()
            .rev()
            .find(|(_, queued)| {
                queued.frame.control_watermark <= self.applied_control_sequence
                    && add_micros(queued.mapped_capture_time, delay) <= now
            })
            .map(|(sequence, _)| *sequence);
        let Some(sequence) = candidate else {
            return Ok(());
        };
        let queued = self
            .queue
            .remove(&sequence)
            .expect("mature candidate came from queue");
        self.queue
            .retain(|queued_sequence, _| *queued_sequence > sequence);

        let deadline = add_micros(queued.mapped_capture_time, delay);
        if now > deadline {
            self.record_lateness(now.0 - deadline.0, false);
        }
        self.update_velocity(VelocitySample {
            sender_capture_time: queued.frame.sender_capture_time,
            totals: queued.frame.totals,
        })?;
        self.target = Target {
            sequence,
            totals: queued.frame.totals,
            sender_capture_time: Some(queued.frame.sender_capture_time),
            mapped_capture_time: queued.mapped_capture_time,
            pending_touch_snapshot: queued.frame.touch_snapshot,
        };
        Ok(())
    }

    fn update_velocity(&mut self, newest: VelocitySample) -> Result<(), PlayoutError> {
        if let Some(previous) = self.velocity_sample {
            let elapsed = newest
                .sender_capture_time
                .0
                .checked_sub(previous.sender_capture_time.0)
                .ok_or(PlayoutError::CaptureTimeMovedBackwards)?;
            if elapsed > 0 {
                let delta = newest.totals.checked_delta_from(previous.totals)?;
                let pointer = pair_magnitude(delta.dx, delta.dy) / elapsed as f64;
                let scroll = pair_magnitude(delta.scroll_x, delta.scroll_y) / elapsed as f64;
                let weight = self.config.catch_up.velocity_sample_weight;
                self.pointer_velocity_per_micro =
                    self.pointer_velocity_per_micro * (1.0 - weight) + pointer * weight;
                self.scroll_velocity_per_micro =
                    self.scroll_velocity_per_micro * (1.0 - weight) + scroll * weight;
            }
        }
        self.velocity_sample = Some(newest);
        Ok(())
    }

    fn poll_elapsed(&mut self, now: MonotonicTimeMicros) -> Result<u64, PlayoutError> {
        let nominal = duration_micros(self.config.scheduler_tick)?;
        let elapsed = self
            .last_poll_time
            .map_or(nominal, |prior| now.0.saturating_sub(prior.0).max(1));
        self.last_poll_time = Some(now);
        Ok(elapsed)
    }

    fn catch_up_cap(
        &self,
        velocity_per_micro: f64,
        elapsed_micros: u64,
    ) -> Result<u64, PlayoutError> {
        let nominal_tick = duration_micros(self.config.scheduler_tick)?;
        let tick_scale = elapsed_micros.div_ceil(nominal_tick).max(1);
        let minimum = self.config.catch_up.minimum_step.saturating_mul(tick_scale);
        let maximum = self.config.catch_up.maximum_step.saturating_mul(tick_scale);
        let velocity_scaled =
            velocity_per_micro * elapsed_micros as f64 * self.config.catch_up.speedup;
        let rounded = if velocity_scaled >= u64::MAX as f64 {
            u64::MAX
        } else {
            velocity_scaled.ceil() as u64
        };
        Ok(rounded.clamp(minimum, maximum))
    }

    fn record_lateness(&mut self, lateness_micros: u64, network: bool) {
        if network {
            self.late_frame_count = self.late_frame_count.saturating_add(1);
        } else {
            self.scheduler_late_count = self.scheduler_late_count.saturating_add(1);
            self.last_scheduler_lateness_micros = Some(lateness_micros);
        }
        self.maximum_lateness_micros = self.maximum_lateness_micros.max(lateness_micros);
    }

    fn observe_time(&mut self, now: MonotonicTimeMicros) -> Result<(), PlayoutError> {
        if self.last_observed_time.is_some_and(|prior| now < prior) {
            return Err(PlayoutError::ReceiverClockMovedBackwards);
        }
        self.last_observed_time = Some(now);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayoutError {
    InvalidConfig(&'static str),
    Clock(ClockError),
    MotionOverflow(MotionOverflow),
    SessionMismatch,
    ReceiverClockMovedBackwards,
    ControlWatermarkMovedBackwards,
    CaptureTimeMovedBackwards,
    ConflictingMotionSequence,
    QueueFull,
    RebaseMovedBackwards,
}

impl fmt::Display for PlayoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => formatter.write_str(message),
            Self::Clock(error) => write!(formatter, "clock mapping failed: {error}"),
            Self::MotionOverflow(error) => write!(formatter, "cumulative motion overflow: {error}"),
            Self::SessionMismatch => formatter.write_str("motion frame belongs to another session"),
            Self::ReceiverClockMovedBackwards => {
                formatter.write_str("receiver monotonic time moved backwards")
            }
            Self::ControlWatermarkMovedBackwards => {
                formatter.write_str("applied control watermark moved backwards")
            }
            Self::CaptureTimeMovedBackwards => {
                formatter.write_str("motion capture time moved backwards across sequences")
            }
            Self::ConflictingMotionSequence => {
                formatter.write_str("one motion sequence carried conflicting cumulative state")
            }
            Self::QueueFull => {
                formatter.write_str("playout frame queue reached its configured bound")
            }
            Self::RebaseMovedBackwards => {
                formatter.write_str("explicit rebase moved behind the selected cumulative target")
            }
        }
    }
}

impl Error for PlayoutError {}

impl From<ClockError> for PlayoutError {
    fn from(value: ClockError) -> Self {
        Self::Clock(value)
    }
}

impl From<MotionOverflow> for PlayoutError {
    fn from(value: MotionOverflow) -> Self {
        Self::MotionOverflow(value)
    }
}

fn duration_micros(duration: Duration) -> Result<u64, PlayoutError> {
    u64::try_from(duration.as_micros())
        .map_err(|_| PlayoutError::InvalidConfig("duration does not fit in microseconds"))
}

fn add_micros(time: MonotonicTimeMicros, micros: u64) -> MonotonicTimeMicros {
    MonotonicTimeMicros(time.0.saturating_add(micros))
}

fn approach_asymmetrically(current: u64, target: u64, growth: u32, contraction: u32) -> u64 {
    if target > current {
        current.saturating_add((target - current).div_ceil(u64::from(growth)))
    } else {
        current.saturating_sub((current - target).div_ceil(u64::from(contraction)))
    }
}

fn pair_magnitude(first: i64, second: i64) -> f64 {
    (first as f64).hypot(second as f64)
}

fn limit_pair(first: i64, second: i64, cap: u64) -> (i64, i64, bool) {
    let magnitude = pair_magnitude(first, second);
    if magnitude <= cap as f64 || magnitude == 0.0 {
        return (first, second, false);
    }
    let scale = cap as f64 / magnitude;
    let mut limited_first = (first as f64 * scale).round() as i64;
    let mut limited_second = (second as f64 * scale).round() as i64;
    if limited_first == 0 && limited_second == 0 {
        if first.unsigned_abs() >= second.unsigned_abs() {
            limited_first = first.signum();
        } else {
            limited_second = second.signum();
        }
    }
    (limited_first, limited_second, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ActivationId, ProtocolVersion, SessionEpoch, TransportGeneration};

    fn time(value: u64) -> MonotonicTimeMicros {
        MonotonicTimeMicros(value)
    }

    fn session(epoch: u8) -> SessionContext {
        SessionContext {
            protocol_version: ProtocolVersion(1),
            session_epoch: SessionEpoch([epoch; 16]),
            transport_generation: TransportGeneration(1),
            activation_id: ActivationId(1),
        }
    }

    fn frame(sequence: u64, capture: u64, total_dx: i64, watermark: u64) -> MotionFrame {
        MotionFrame {
            session: session(1),
            motion_sequence: MotionSequence(sequence),
            control_watermark: ControlSequence(watermark),
            sender_capture_time: time(capture),
            totals: CumulativeMotion::new(total_dx, 0, 0, 0),
            touch_snapshot: None,
        }
    }

    fn identity_clock() -> ClockMapper {
        let mut clock = ClockMapper::default();
        clock
            .ingest_sample(super::super::clock::ClockSample::exact(time(0), time(0)))
            .unwrap();
        clock
    }

    fn unbounded_test_config() -> PlayoutConfig {
        PlayoutConfig {
            catch_up: CatchUpConfig {
                minimum_step: 1_000,
                maximum_step: 1_000,
                ..CatchUpConfig::default()
            },
            ..PlayoutConfig::default()
        }
    }

    #[test]
    fn measured_adaptive_defaults_and_treated_trace_settle_at_three_ms() {
        let clock = identity_clock();
        let mut playout = ReceiverPlayout::new(PlayoutConfig::default(), session(1)).unwrap();
        for sequence in 1..=64 {
            let capture = sequence * 4_000;
            playout
                .ingest_frame(
                    frame(sequence, capture, sequence as i64, 0),
                    time(capture + 1_000),
                    &clock,
                )
                .unwrap();
        }

        assert_eq!(playout.config().adaptive_percentile, 80);
        assert_eq!(playout.config().minimum_delay, Duration::from_millis(3));
        assert_eq!(playout.config().maximum_delay, Duration::from_millis(35));
        assert_eq!(playout.current_delay(), Duration::from_millis(3));
        assert_eq!(
            playout.stats().packet_delay_variation_percentile,
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn probe_transition_remaps_dense_bootstrap_queue_without_losing_motion() {
        let mut clock = ClockMapper::default();
        clock
            .bootstrap_from_arrival(time(1_000), time(51_000))
            .unwrap();
        let mut playout = ReceiverPlayout::new(PlayoutConfig::default(), session(1)).unwrap();
        playout
            .ingest_frame(frame(1, 1_000, 10, 0), time(51_000), &clock)
            .unwrap();
        playout
            .ingest_frame(frame(2, 2_000, 20, 0), time(52_000), &clock)
            .unwrap();

        clock
            .ingest_probe(crate::core::ProbeExchange {
                receiver_sent_at: time(100_000),
                sender_received_at: time(100_500),
                sender_echoed_at: time(100_500),
                receiver_received_at: time(101_000),
            })
            .unwrap();
        playout.remap_clock(&clock).unwrap();

        playout
            .ingest_frame(frame(3, 3_000, 30, 0), time(101_001), &clock)
            .unwrap();
        assert_eq!(playout.stats().queued_frames, 3);
        assert_eq!(playout.next_deadline(), Some(time(4_000)));
        while playout.stats().injected_totals != CumulativeMotion::new(30, 0, 0, 0) {
            playout.poll(time(101_001)).unwrap();
        }
        assert_eq!(
            playout.stats().injected_totals,
            CumulativeMotion::new(30, 0, 0, 0)
        );
        assert!(matches!(
            playout.ingest_frame(frame(4, 2_500, 40, 0), time(101_002), &clock),
            Err(PlayoutError::CaptureTimeMovedBackwards)
        ));
    }

    #[test]
    fn deadlines_follow_capture_order_even_when_arrival_is_reordered() {
        let clock = identity_clock();
        let mut config = unbounded_test_config();
        config.delay_mode = PlayoutDelayMode::Fixed;
        config.fixed_delay = Duration::from_millis(8);
        let mut playout = ReceiverPlayout::new(config, session(1)).unwrap();

        playout
            .ingest_frame(frame(2, 2_000, 20, 0), time(3_000), &clock)
            .unwrap();
        playout
            .ingest_frame(frame(1, 1_000, 10, 0), time(3_100), &clock)
            .unwrap();
        assert_eq!(playout.next_deadline(), Some(time(9_000)));
        assert!(playout.poll(time(8_999)).unwrap().is_none());
        let first = playout.poll(time(9_000)).unwrap().unwrap();
        assert_eq!(first.through_sequence, MotionSequence(1));
        assert_eq!(first.delta.dx, 10);
        assert_eq!(playout.next_deadline(), Some(time(10_000)));
    }

    #[test]
    fn control_watermark_blocks_an_otherwise_mature_frame() {
        let clock = identity_clock();
        let mut playout = ReceiverPlayout::new(unbounded_test_config(), session(1)).unwrap();
        playout
            .ingest_frame(frame(1, 1_000, 10, 2), time(2_000), &clock)
            .unwrap();
        assert!(playout.poll(time(10_000)).unwrap().is_none());
        assert_eq!(playout.next_deadline(), None);

        playout
            .advance_control_watermark(ControlSequence(2))
            .unwrap();
        let step = playout.poll(time(10_001)).unwrap().unwrap();
        assert_eq!(step.delta.dx, 10);
        assert!(step.target_reached);
    }

    #[test]
    fn jitter_burst_grows_fast_counts_late_frames_and_conserves_final_totals() {
        let clock = identity_clock();
        let mut playout = ReceiverPlayout::new(PlayoutConfig::default(), session(1)).unwrap();
        let delays = [
            1_000, 1_000, 1_000, 40_000, 150_000, 90_000, 120_000, 1_000, 1_000,
        ];
        let mut arrivals: Vec<_> = delays
            .into_iter()
            .enumerate()
            .map(|(index, delay)| {
                let sequence = index as u64 + 1;
                let capture = sequence * 5_000;
                (
                    capture + delay,
                    frame(sequence, capture, sequence as i64 * 10, 0),
                )
            })
            .collect();
        arrivals.sort_by_key(|(arrival, frame)| (*arrival, frame.motion_sequence));

        let final_time = arrivals.last().unwrap().0 + 500_000;
        let mut next_arrival = 0;
        let mut emitted_dx = 0;
        let mut catch_up_dx = 0;
        for now in (0..=final_time).step_by(1_000) {
            while arrivals
                .get(next_arrival)
                .is_some_and(|(arrival, _)| *arrival <= now)
            {
                let (arrival, frame) = arrivals[next_arrival].clone();
                playout.ingest_frame(frame, time(arrival), &clock).unwrap();
                next_arrival += 1;
            }
            if let Some(step) = playout
                .poll(time(
                    now.max(playout.last_observed_time.unwrap_or(time(0)).0),
                ))
                .unwrap()
            {
                emitted_dx += step.delta.dx;
                if step.pointer_catch_up_limited {
                    catch_up_dx += step.delta.dx;
                }
            }
        }

        assert_eq!(next_arrival, arrivals.len());
        assert_eq!(emitted_dx, 90);
        assert_eq!(
            playout.stats().injected_totals,
            CumulativeMotion::new(90, 0, 0, 0)
        );
        assert_eq!(playout.current_delay(), Duration::from_millis(35));
        assert!(playout.stats().late_frame_count >= 3);
        assert!(playout.stats().catch_up_step_count > 0);
        assert!(catch_up_dx > 0);
    }

    #[test]
    fn no_default_stale_discard_and_explicit_rebase_is_accounted() {
        let clock = identity_clock();
        let mut playout = ReceiverPlayout::new(PlayoutConfig::default(), session(1)).unwrap();
        playout
            .ingest_frame(frame(1, 1_000, 100, 0), time(151_000), &clock)
            .unwrap();

        // A very late frame still contributes its complete displacement.
        let mut total = 0;
        for now in 151_000..200_000 {
            if let Some(step) = playout.poll(time(now)).unwrap() {
                total += step.delta.dx;
            }
            if total == 100 {
                break;
            }
        }
        assert_eq!(total, 100);

        playout
            .ingest_frame(frame(2, 2_000, 140, 0), time(200_000), &clock)
            .unwrap();
        let record = playout
            .rebase(MotionSequence(2), CumulativeMotion::new(140, 0, 0, 0))
            .unwrap();
        assert_eq!(record.discarded_displacement.dx, 40);
        assert_eq!(playout.stats().rebase_count, 1);
        assert_eq!(playout.stats().last_rebase, Some(record));
        assert_eq!(
            playout
                .ingest_frame(frame(2, 2_000, 140, 0), time(201_000), &clock)
                .unwrap(),
            EnqueueOutcome::RetiredByRebase
        );
    }

    #[test]
    fn reset_drops_old_epoch_history_and_restores_adaptive_floor() {
        let clock = identity_clock();
        let mut playout = ReceiverPlayout::new(PlayoutConfig::default(), session(1)).unwrap();
        playout
            .ingest_frame(frame(1, 1_000, 10, 0), time(100_000), &clock)
            .unwrap();
        playout
            .ingest_frame(frame(2, 2_000, 20, 0), time(200_000), &clock)
            .unwrap();
        assert!(playout.current_delay() > Duration::from_millis(3));

        playout.reset(session(2));
        let stats = playout.stats();
        assert_eq!(stats.current_delay, Duration::from_millis(3));
        assert_eq!(stats.queued_frames, 0);
        assert_eq!(stats.injected_totals, CumulativeMotion::ZERO);
        assert_eq!(stats.reset_count, 1);
        assert!(matches!(
            playout.ingest_frame(frame(2, 2_000, 20, 0), time(101_000), &clock),
            Err(PlayoutError::SessionMismatch)
        ));
    }

    #[test]
    fn runtime_tuning_contracts_slowly_after_a_burst() {
        let clock = identity_clock();
        let mut playout = ReceiverPlayout::new(PlayoutConfig::default(), session(1)).unwrap();
        playout
            .ingest_frame(frame(1, 1_000, 1, 0), time(2_000), &clock)
            .unwrap();
        playout
            .ingest_frame(frame(2, 2_000, 2, 0), time(102_000), &clock)
            .unwrap();
        assert_eq!(playout.current_delay(), Duration::from_millis(35));

        for sequence in 3..=260 {
            let capture = 200_000 + sequence * 1_000;
            playout
                .ingest_frame(
                    frame(sequence, capture, sequence as i64, 0),
                    time(capture + 1_000),
                    &clock,
                )
                .unwrap();
        }
        assert!(playout.current_delay() < Duration::from_millis(35));
        assert!(playout.current_delay() >= Duration::from_millis(3));

        let mut config = playout.config();
        config.adaptive_percentile = 95;
        config.maximum_delay = Duration::from_millis(80);
        playout.set_config(config).unwrap();
        assert_eq!(playout.config().adaptive_percentile, 95);
        assert_eq!(playout.config().maximum_delay, Duration::from_millis(80));
    }
}
