//! Per-frame input data and the fields derived from it.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// What kind of frame a sample describes.
///
/// Moonshine's `FrameStats` only carries `is_key_frame`, so unless a sample sets
/// [`FrameSample::kind`] explicitly the kind is inferred (see [`FrameSample::kind`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameKind {
	/// A captured frame encoded as a P-frame.
	Delta,
	/// A captured frame encoded as a key (IDR) frame.
	Key,
	/// A key frame re-encoded from the encoder's last input because no new frame arrived in time.
	///
	/// These never went through capture, import or convert, so mixing them with captured frames
	/// would skew those stages towards zero.
	IdrReencode,
}

impl FrameKind {
	pub const ALL: [FrameKind; 3] = [FrameKind::Delta, FrameKind::Key, FrameKind::IdrReencode];

	pub fn as_str(self) -> &'static str {
		match self {
			FrameKind::Delta => "delta",
			FrameKind::Key => "key",
			FrameKind::IdrReencode => "idr_reencode",
		}
	}

	/// Whether the frame came from the compositor (as opposed to an IDR re-encode).
	pub fn is_captured(self) -> bool {
		!matches!(self, FrameKind::IdrReencode)
	}

	pub(crate) fn index(self) -> usize {
		self as usize
	}
}

/// A pipeline stage, as reported by `FrameStats`, plus the derived `unaccounted` remainder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
	ChannelWait,
	Import,
	Convert,
	Submit,
	/// Sub-interval of `encode_wait`; never added to the sequential sum.
	ConsumerQueue,
	EncodeWait,
	Packetize,
	Send,
	/// `total - Σ SEQUENTIAL`. Recorded clamped at zero in histograms; see [`Derived::unaccounted_ns`].
	Unaccounted,
}

impl Stage {
	pub const ALL: [Stage; 9] = [
		Stage::ChannelWait,
		Stage::Import,
		Stage::Convert,
		Stage::Submit,
		Stage::ConsumerQueue,
		Stage::EncodeWait,
		Stage::Packetize,
		Stage::Send,
		Stage::Unaccounted,
	];

	/// Stages that run one after another and together should add up to `total`.
	pub const SEQUENTIAL: [Stage; 7] = [
		Stage::ChannelWait,
		Stage::Import,
		Stage::Convert,
		Stage::Submit,
		Stage::EncodeWait,
		Stage::Packetize,
		Stage::Send,
	];

	pub fn as_str(self) -> &'static str {
		match self {
			Stage::ChannelWait => "channel_wait",
			Stage::Import => "import",
			Stage::Convert => "convert",
			Stage::Submit => "submit",
			Stage::ConsumerQueue => "consumer_queue",
			Stage::EncodeWait => "encode_wait",
			Stage::Packetize => "packetize",
			Stage::Send => "send",
			Stage::Unaccounted => "unaccounted",
		}
	}

	pub(crate) fn index(self) -> usize {
		self as usize
	}
}

/// One frame's worth of pipeline timing.
///
/// Moonshine's `FrameStats` plus the time the harness received it. The optional fields
/// make the derived values exact when the caller knows them; left as `None`, they're inferred.
#[derive(Clone, Debug)]
pub struct FrameSample {
	pub channel_wait: Duration,
	pub import: Duration,
	pub convert: Duration,
	pub submit: Duration,
	pub consumer_queue: Duration,
	pub encode_wait: Duration,
	/// Time the GPU's video queue spent encoding this frame, measured inside `encode_wait`.
	/// `None` when that queue reports no timestamps, which is how Moonshine leaves it out of a frame line.
	pub gpu_encode: Option<Duration>,
	/// When the encoded bitstream was ready, as the GPU's video queue reports it, inside `encode_wait`.
	/// `None` when that queue reports no timestamps.
	pub bitstream_ready: Option<Duration>,
	pub packetize: Duration,
	pub send: Duration,
	pub total: Duration,
	pub encoded_bytes: u64,
	pub is_key_frame: bool,
	/// When the harness received the sample.
	pub observed_at: Instant,
	/// When the compositor produced the frame. Otherwise `observed_at − total` is used.
	pub captured_at: Option<Instant>,
	/// Pipeline frame sequence number. Otherwise a per-run counter is used.
	pub seq: Option<u64>,
	/// Exact frame kind. Otherwise it's inferred from the stage timings.
	pub kind: Option<FrameKind>,
	/// The captured content's color space (`sdr`, `hdr10`, `scrgb`), when the source says; none for a keyframe
	/// re-encoded without a capture.
	pub content: Option<&'static str>,
}

impl FrameSample {
	/// A sample with every stage zero, observed now. Handy in tests and examples.
	pub fn zeroed(observed_at: Instant) -> Self {
		Self {
			channel_wait: Duration::ZERO,
			import: Duration::ZERO,
			convert: Duration::ZERO,
			submit: Duration::ZERO,
			consumer_queue: Duration::ZERO,
			encode_wait: Duration::ZERO,
			gpu_encode: None,
			bitstream_ready: None,
			packetize: Duration::ZERO,
			send: Duration::ZERO,
			total: Duration::ZERO,
			encoded_bytes: 0,
			is_key_frame: false,
			observed_at,
			captured_at: None,
			seq: None,
			kind: None,
			content: None,
		}
	}

	/// The frame kind: [`FrameSample::kind`] if set, otherwise inferred.
	///
	/// The inference relies on how Moonshine builds IDR re-encodes: a key frame with
	/// `channel_wait`, `import` and `convert` all exactly zero. A captured frame always spends
	/// at least a few nanoseconds in each of those.
	pub fn kind(&self) -> FrameKind {
		if let Some(kind) = self.kind {
			return kind;
		}
		if !self.is_key_frame {
			FrameKind::Delta
		} else if self.channel_wait.is_zero() && self.import.is_zero() && self.convert.is_zero() {
			FrameKind::IdrReencode
		} else {
			FrameKind::Key
		}
	}

	/// Whether [`FrameSample::kind`] had to guess.
	pub fn kind_is_inferred(&self) -> bool {
		self.kind.is_none()
	}

	/// Duration of a reported stage. `Unaccounted` is derived and returns `None`; use
	/// [`FrameSample::unaccounted_ns`].
	pub fn stage(&self, stage: Stage) -> Option<Duration> {
		Some(match stage {
			Stage::ChannelWait => self.channel_wait,
			Stage::Import => self.import,
			Stage::Convert => self.convert,
			Stage::Submit => self.submit,
			Stage::ConsumerQueue => self.consumer_queue,
			Stage::EncodeWait => self.encode_wait,
			Stage::Packetize => self.packetize,
			Stage::Send => self.send,
			Stage::Unaccounted => return None,
		})
	}

	/// `total − Σ sequential stages`, in nanoseconds. Negative means the reported stage
	/// timings overlap.
	pub fn unaccounted_ns(&self) -> i64 {
		let accounted: u128 = Stage::SEQUENTIAL
			.iter()
			.filter_map(|s| self.stage(*s))
			.map(|d| d.as_nanos())
			.sum();
		let diff = self.total.as_nanos() as i128 - accounted as i128;
		diff.clamp(i64::MIN as i128, i64::MAX as i128) as i64
	}

	/// Best estimate of when the frame entered the pipeline, and whether it was reconstructed.
	pub fn start_time(&self) -> (Instant, bool) {
		match self.captured_at {
			Some(t) => (t, false),
			None => (
				self.observed_at.checked_sub(self.total).unwrap_or(self.observed_at),
				true,
			),
		}
	}
}

/// Fields derived from a sample in the context of its run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Derived {
	/// Per-run frame counter (or `FrameSample::seq` when present).
	pub seq: u64,
	pub kind: FrameKind,
	/// Signed `total − Σ sequential stages`.
	pub unaccounted_ns: i64,
	/// Time since the previous captured frame. `None` for the first frame and for IDR re-encodes.
	pub interval: Option<Duration>,
	/// `total` exceeded the frame budget (captured frames only).
	pub over_budget: bool,
	/// The source's recent frame cadence: the median interval over the last
	/// [`CADENCE_WINDOW`] captured frames. `None` until [`CADENCE_MIN_SAMPLES`] intervals exist.
	pub cadence: Option<Duration>,
	/// `interval` exceeded [`PACING_MISS_FACTOR`] × the expected interval: the cadence, or the
	/// frame budget when that is longer (or there's no cadence yet). A game that steadily
	/// renders below the target fps is not missing its pacing; a hitch in its rhythm is.
	pub pacing_miss: bool,
}

impl Derived {
	/// Clamped duration of a stage for histogram purposes (`Unaccounted` below zero becomes zero).
	pub fn stage_duration(&self, sample: &FrameSample, stage: Stage) -> Duration {
		match sample.stage(stage) {
			Some(d) => d,
			None => Duration::from_nanos(self.unaccounted_ns.max(0) as u64),
		}
	}

	/// Whether this frame should be singled out as an outlier (over budget or a pacing miss).
	pub fn is_outlier(&self) -> bool {
		self.over_budget || self.pacing_miss
	}
}

/// How far past the expected interval an interval must be to count as a pacing miss.
pub const PACING_MISS_FACTOR: f64 = 1.5;
/// Captured frames the cadence is measured over (about half a second at 120 fps).
pub const CADENCE_WINDOW: usize = 64;
/// Intervals needed before the cadence is used instead of the frame budget.
pub const CADENCE_MIN_SAMPLES: usize = 8;

/// Computes [`Derived`] fields for a stream of samples.
#[derive(Debug, Clone)]
pub struct Deriver {
	budget: Duration,
	counter: u64,
	previous: Option<Instant>,
	/// The last [`CADENCE_WINDOW`] intervals, oldest first, and the same values sorted.
	recent: std::collections::VecDeque<Duration>,
	sorted: Vec<Duration>,
}

impl Deriver {
	pub fn new(target_fps: u32) -> Self {
		let budget = frame_budget(target_fps);
		Self {
			budget,
			counter: 0,
			previous: None,
			recent: std::collections::VecDeque::with_capacity(CADENCE_WINDOW),
			sorted: Vec::with_capacity(CADENCE_WINDOW),
		}
	}

	pub fn budget(&self) -> Duration {
		self.budget
	}

	/// Forget the previous frame and the cadence, e.g. between iterations.
	pub fn reset(&mut self) {
		self.previous = None;
		self.recent.clear();
		self.sorted.clear();
	}

	fn cadence(&self) -> Option<Duration> {
		(self.sorted.len() >= CADENCE_MIN_SAMPLES).then(|| self.sorted[self.sorted.len() / 2])
	}

	fn push_interval(&mut self, interval: Duration) {
		if self.recent.len() == CADENCE_WINDOW
			&& let Some(oldest) = self.recent.pop_front()
			&& let Ok(i) = self.sorted.binary_search(&oldest)
		{
			self.sorted.remove(i);
		}
		self.recent.push_back(interval);
		let i = self.sorted.partition_point(|d| *d < interval);
		self.sorted.insert(i, interval);
	}

	pub fn derive(&mut self, sample: &FrameSample) -> Derived {
		let seq = sample.seq.unwrap_or(self.counter);
		self.counter += 1;
		let kind = sample.kind();

		// Intervals only make sense between captured frames. A re-encode stands in for a frame
		// that never arrived, so it must not hide the gap it was filling.
		let interval = if kind.is_captured() {
			let at = sample.captured_at.unwrap_or(sample.observed_at);
			let interval = self.previous.map(|p| at.saturating_duration_since(p));
			self.previous = Some(at);
			interval
		} else {
			None
		};

		// Judge against the cadence before this interval joins it.
		let cadence = self.cadence();
		let expected = cadence.map_or(self.budget, |c| c.max(self.budget));
		let pacing_miss = interval.is_some_and(|i| i > expected.mul_f64(PACING_MISS_FACTOR));
		if let Some(i) = interval {
			self.push_interval(i);
		}

		Derived {
			seq,
			kind,
			unaccounted_ns: sample.unaccounted_ns(),
			interval,
			cadence,
			over_budget: kind.is_captured() && sample.total > self.budget,
			pacing_miss,
		}
	}
}

/// `1 / fps`, or one second for a zero fps.
pub fn frame_budget(target_fps: u32) -> Duration {
	Duration::from_secs(1) / target_fps.max(1)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn us(v: u64) -> Duration {
		Duration::from_micros(v)
	}

	fn sample(at: Instant) -> FrameSample {
		FrameSample {
			channel_wait: us(100),
			import: us(200),
			convert: us(300),
			submit: us(400),
			consumer_queue: us(50),
			encode_wait: us(2000),
			packetize: us(100),
			send: us(20),
			total: us(3200),
			encoded_bytes: 10_000,
			..FrameSample::zeroed(at)
		}
	}

	#[test]
	fn unaccounted_excludes_consumer_queue() {
		let s = sample(Instant::now());
		// 100+200+300+400+2000+100+20 = 3120
		assert_eq!(s.unaccounted_ns(), 80_000);
	}

	#[test]
	fn unaccounted_can_be_negative() {
		let mut s = sample(Instant::now());
		s.total = us(3000);
		assert_eq!(s.unaccounted_ns(), -120_000);
		let d = Deriver::new(60).derive(&s);
		assert_eq!(d.stage_duration(&s, Stage::Unaccounted), Duration::ZERO);
	}

	#[test]
	fn kind_inference() {
		let now = Instant::now();
		let mut s = sample(now);
		assert_eq!(s.kind(), FrameKind::Delta);
		s.is_key_frame = true;
		assert_eq!(s.kind(), FrameKind::Key);
		s.channel_wait = Duration::ZERO;
		s.import = Duration::ZERO;
		s.convert = Duration::ZERO;
		assert_eq!(s.kind(), FrameKind::IdrReencode);
		assert!(s.kind_is_inferred());
		s.kind = Some(FrameKind::Key);
		assert_eq!(s.kind(), FrameKind::Key);
		assert!(!s.kind_is_inferred());
	}

	#[test]
	fn a_delta_frame_with_zero_stages_is_still_delta() {
		assert_eq!(FrameSample::zeroed(Instant::now()).kind(), FrameKind::Delta);
	}

	#[test]
	fn intervals_budget_and_pacing() {
		let t0 = Instant::now();
		let mut d = Deriver::new(100); // 10 ms budget, 15 ms pacing threshold
		let a = d.derive(&sample(t0));
		assert_eq!(a.seq, 0);
		assert_eq!(a.interval, None);
		assert!(!a.over_budget && !a.pacing_miss);

		let b = d.derive(&sample(t0 + Duration::from_millis(10)));
		assert_eq!(b.seq, 1);
		assert_eq!(b.interval, Some(Duration::from_millis(10)));
		assert!(!b.pacing_miss);

		// A re-encode gets no interval and does not reset the gap.
		let mut idr = FrameSample::zeroed(t0 + Duration::from_millis(20));
		idr.is_key_frame = true;
		idr.total = Duration::from_millis(50);
		let c = d.derive(&idr);
		assert_eq!(c.kind, FrameKind::IdrReencode);
		assert_eq!(c.interval, None);
		assert!(!c.over_budget, "re-encodes never count as over budget");

		let mut slow = sample(t0 + Duration::from_millis(30));
		slow.total = Duration::from_millis(11);
		let e = d.derive(&slow);
		assert_eq!(e.interval, Some(Duration::from_millis(20)));
		assert!(e.pacing_miss && e.over_budget && e.is_outlier());
	}

	#[test]
	fn pacing_follows_the_source_cadence() {
		let t0 = Instant::now();
		let mut d = Deriver::new(120); // 8.3 ms budget
		let mut t = t0;
		let mut at = |ms: f64| {
			t += Duration::from_secs_f64(ms / 1000.0);
			d.derive(&sample(t))
		};
		at(0.0);
		// Before there's a cadence, a steady 15 ms (66 fps) is measured against the budget.
		let early: Vec<_> = (0..CADENCE_MIN_SAMPLES).map(|_| at(15.0)).collect();
		assert!(early.iter().all(|f| f.pacing_miss && f.cadence.is_none()));
		// Then against the cadence: steady 66 fps is fine...
		let steady: Vec<_> = (0..100).map(|_| at(15.0)).collect();
		assert!(steady.iter().all(|f| !f.pacing_miss));
		assert_eq!(steady[99].cadence, Some(Duration::from_millis(15)));
		// ...and a hitch in it isn't.
		assert!(at(30.0).pacing_miss);
		assert!(!at(15.0).pacing_miss);
	}

	#[test]
	fn cadence_never_goes_below_the_budget() {
		let t0 = Instant::now();
		let mut d = Deriver::new(60); // 16.7 ms budget
		for i in 0..=20 {
			d.derive(&sample(t0 + Duration::from_millis(5 * i)));
		}
		// A 20 ms gap after 5 ms frames is under 1.5 × the budget: not a miss at 60 fps.
		let f = d.derive(&sample(t0 + Duration::from_millis(120)));
		assert_eq!(f.cadence, Some(Duration::from_millis(5)));
		assert!(!f.pacing_miss);
	}

	#[test]
	fn reset_forgets_the_cadence() {
		let t0 = Instant::now();
		let mut d = Deriver::new(120);
		for i in 0..20 {
			d.derive(&sample(t0 + Duration::from_millis(15 * i)));
		}
		d.reset();
		d.derive(&sample(t0 + Duration::from_secs(1)));
		assert_eq!(d.derive(&sample(t0 + Duration::from_millis(1015))).cadence, None);
	}

	#[test]
	fn captured_at_is_preferred_for_intervals() {
		let t0 = Instant::now();
		let mut d = Deriver::new(60);
		let mut a = sample(t0 + Duration::from_millis(100));
		a.captured_at = Some(t0);
		let mut b = sample(t0 + Duration::from_millis(101));
		b.captured_at = Some(t0 + Duration::from_millis(16));
		d.derive(&a);
		assert_eq!(d.derive(&b).interval, Some(Duration::from_millis(16)));
	}

	#[test]
	fn sample_seq_wins() {
		let mut s = sample(Instant::now());
		s.seq = Some(42);
		assert_eq!(Deriver::new(60).derive(&s).seq, 42);
	}

	#[test]
	fn start_time_reconstruction() {
		let t = Instant::now() + Duration::from_secs(1);
		let s = sample(t);
		assert_eq!(s.start_time(), (t - us(3200), true));
	}
}
