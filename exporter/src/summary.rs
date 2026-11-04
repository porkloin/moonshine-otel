//! Exact per-run statistics, computed with HdrHistogram.
//!
//! These numbers are the source of truth for the result JSON, the `bench.run` span and the
//! terminal summary. Prometheus quantiles are only for trends.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use serde::{Deserialize, Serialize};

use crate::sample::{Derived, FrameKind, FrameSample, Stage};

/// Significant digits kept by every histogram (0.1 % relative error).
const SIGFIG: u8 = 3;

/// Name of the group holding all captured frames (delta + key, not IDR re-encodes).
pub const GROUP_CAPTURED: &str = "captured";
/// Name used for the end-to-end latency next to the stage names.
pub const TOTAL: &str = "total";

/// Summary statistics of one distribution. Latencies are in microseconds, sizes in bytes.
///
/// Quantiles, `min` and `max` are exact to 3 significant figures (HdrHistogram reports the
/// highest value equivalent to the recorded one, so they can read up to 0.1 % high).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Distribution {
	pub count: u64,
	pub mean: f64,
	pub stddev: f64,
	pub min: f64,
	pub p50: f64,
	pub p90: f64,
	pub p95: f64,
	pub p99: f64,
	pub p999: f64,
	pub max: f64,
}

impl Distribution {
	fn from_histogram(h: &Histogram<u64>) -> Self {
		if h.is_empty() {
			return Self::default();
		}
		let q = |p: f64| h.value_at_quantile(p) as f64;
		Self {
			count: h.len(),
			mean: h.mean(),
			stddev: h.stdev(),
			min: h.min() as f64,
			p50: q(0.50),
			p90: q(0.90),
			p95: q(0.95),
			p99: q(0.99),
			p999: q(0.999),
			max: h.max() as f64,
		}
	}

	/// Look up a statistic by name (`p99`, `mean`, ...).
	pub fn stat(&self, name: &str) -> Option<f64> {
		Some(match name {
			"count" => self.count as f64,
			"mean" => self.mean,
			"stddev" => self.stddev,
			"min" => self.min,
			"p50" => self.p50,
			"p90" => self.p90,
			"p95" => self.p95,
			"p99" => self.p99,
			"p999" => self.p999,
			"max" => self.max,
			_ => return None,
		})
	}
}

/// Frame counts by kind.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FrameCounts {
	pub delta: u64,
	pub key: u64,
	pub idr_reencode: u64,
	/// Frames received during warmup and discarded.
	pub warmup_discarded: u64,
	/// How many of the recorded frames had their kind inferred rather than reported.
	pub kind_inferred: u64,
}

impl FrameCounts {
	pub fn captured(&self) -> u64 {
		self.delta + self.key
	}

	pub fn recorded(&self) -> u64 {
		self.delta + self.key + self.idr_reencode
	}
}

/// A count and its share of captured frames.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Rate {
	pub count: u64,
	pub pct: f64,
}

/// Exact statistics for one measurement window (one iteration).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
	pub frames: FrameCounts,
	/// Seconds between the first and last recorded frame.
	pub duration_s: f64,
	pub target_fps: u32,
	/// Captured frames per second.
	pub effective_fps: f64,
	/// `effective_fps / target_fps`.
	pub fps_ratio: f64,
	/// Encoded megabits per second, all frame kinds.
	pub mbps: f64,
	pub encoded_bytes: u64,
	/// Captured frames with `total` above the frame budget.
	pub over_budget: Rate,
	/// Captured frames arriving more than 1.5 budgets after the previous one.
	pub pacing_miss: Rate,
	/// Samples lost because the harness fell behind the stats channel.
	pub lagged: u64,
	/// Frames reported dropped through [`crate::RunRecorder::record_dropped`]. `None` when nothing reports drops.
	pub dropped: Option<u64>,
	/// Frames whose stages summed to more than `total`.
	pub unaccounted_negative: u64,
	/// Most negative `unaccounted` seen, in µs (0 when none were negative).
	pub unaccounted_min: f64,
	/// Latency by group (`captured`, `delta`, `key`, `idr_reencode`) then by stage or `total` (µs).
	pub latency: BTreeMap<String, BTreeMap<String, Distribution>>,
	/// Time between captured frames (µs).
	pub interval: Distribution,
	/// Encoded frame size by kind (bytes).
	pub frame_size: BTreeMap<String, Distribution>,
}

impl Summary {
	/// Latency distribution of a stage (or `total`) in a group.
	pub fn latency(&self, group: &str, stage: &str) -> Option<&Distribution> {
		self.latency.get(group)?.get(stage)
	}

	/// Share of samples lost to lag or pipeline drops, relative to all frames seen.
	pub fn loss_ratio(&self) -> f64 {
		let lost = self.lagged + self.dropped.unwrap_or(0);
		let seen = self.frames.recorded() + lost;
		if seen == 0 { 0.0 } else { lost as f64 / seen as f64 }
	}
}

/// Histograms for one group of frames: `total` plus one per stage.
#[derive(Debug)]
struct StageHistograms {
	total: Histogram<u64>,
	stages: Vec<Histogram<u64>>,
}

impl StageHistograms {
	fn new() -> Self {
		Self {
			total: new_histogram(),
			stages: Stage::ALL.iter().map(|_| new_histogram()).collect(),
		}
	}

	fn record(&mut self, sample: &FrameSample, derived: &Derived) {
		record(&mut self.total, micros(sample.total));
		for stage in Stage::ALL {
			record(
				&mut self.stages[stage.index()],
				micros(derived.stage_duration(sample, stage)),
			);
		}
	}

	fn add(&mut self, other: &Self) {
		self.total
			.add(&other.total)
			.expect("auto-resizing histograms never fail to add");
		for (a, b) in self.stages.iter_mut().zip(&other.stages) {
			a.add(b).expect("auto-resizing histograms never fail to add");
		}
	}

	fn distributions(&self) -> BTreeMap<String, Distribution> {
		let mut out = BTreeMap::new();
		out.insert(TOTAL.to_string(), Distribution::from_histogram(&self.total));
		for stage in Stage::ALL {
			out.insert(
				stage.as_str().to_string(),
				Distribution::from_histogram(&self.stages[stage.index()]),
			);
		}
		out
	}
}

fn new_histogram() -> Histogram<u64> {
	Histogram::new(SIGFIG).expect("3 significant figures is valid")
}

/// Record into an auto-resizing histogram. `saturating_record` would clamp instead of growing.
fn record(h: &mut Histogram<u64>, value: u64) {
	if h.record(value).is_err() {
		h.saturating_record(value);
	}
}

fn micros(d: Duration) -> u64 {
	d.as_micros().min(u64::MAX as u128) as u64
}

/// Accumulates one measurement window.
#[derive(Debug)]
pub struct SummaryBuilder {
	target_fps: u32,
	by_kind: Vec<StageHistograms>,
	interval: Histogram<u64>,
	size_by_kind: Vec<Histogram<u64>>,
	frames: FrameCounts,
	encoded_bytes: u64,
	over_budget: u64,
	pacing_miss: u64,
	lagged: u64,
	dropped: Option<u64>,
	unaccounted_negative: u64,
	unaccounted_min_ns: i64,
	first: Option<Instant>,
	last: Option<Instant>,
}

impl SummaryBuilder {
	pub fn new(target_fps: u32) -> Self {
		Self {
			target_fps,
			by_kind: FrameKind::ALL.iter().map(|_| StageHistograms::new()).collect(),
			interval: new_histogram(),
			size_by_kind: FrameKind::ALL.iter().map(|_| new_histogram()).collect(),
			frames: FrameCounts::default(),
			encoded_bytes: 0,
			over_budget: 0,
			pacing_miss: 0,
			lagged: 0,
			dropped: None,
			unaccounted_negative: 0,
			unaccounted_min_ns: 0,
			first: None,
			last: None,
		}
	}

	pub fn record(&mut self, sample: &FrameSample, derived: &Derived) {
		let kind = derived.kind;
		match kind {
			FrameKind::Delta => self.frames.delta += 1,
			FrameKind::Key => self.frames.key += 1,
			FrameKind::IdrReencode => self.frames.idr_reencode += 1,
		}
		if sample.kind_is_inferred() {
			self.frames.kind_inferred += 1;
		}
		self.by_kind[kind.index()].record(sample, derived);
		record(&mut self.size_by_kind[kind.index()], sample.encoded_bytes);
		self.encoded_bytes += sample.encoded_bytes;
		if let Some(interval) = derived.interval {
			record(&mut self.interval, micros(interval));
		}
		self.over_budget += derived.over_budget as u64;
		self.pacing_miss += derived.pacing_miss as u64;
		if derived.unaccounted_ns < 0 {
			self.unaccounted_negative += 1;
			self.unaccounted_min_ns = self.unaccounted_min_ns.min(derived.unaccounted_ns);
		}
		self.first.get_or_insert(sample.observed_at);
		self.last = Some(sample.observed_at);
	}

	pub fn record_warmup(&mut self) {
		self.frames.warmup_discarded += 1;
	}

	pub fn record_lagged(&mut self, n: u64) {
		self.lagged += n;
	}

	pub fn record_dropped(&mut self, n: u64) {
		*self.dropped.get_or_insert(0) += n;
	}

	pub fn frames(&self) -> &FrameCounts {
		&self.frames
	}

	pub fn build(&self) -> Summary {
		let duration_s = match (self.first, self.last) {
			(Some(a), Some(b)) => b.duration_since(a).as_secs_f64(),
			_ => 0.0,
		};
		let captured = self.frames.captured();
		// n frames span n-1 intervals.
		let effective_fps = if duration_s > 0.0 && captured > 1 {
			(captured - 1) as f64 / duration_s
		} else {
			0.0
		};
		let mbps = if duration_s > 0.0 {
			self.encoded_bytes as f64 * 8.0 / duration_s / 1e6
		} else {
			0.0
		};
		let rate = |count: u64| Rate {
			count,
			pct: if captured == 0 {
				0.0
			} else {
				count as f64 * 100.0 / captured as f64
			},
		};

		let mut latency = BTreeMap::new();
		let mut captured_hist = StageHistograms::new();
		captured_hist.add(&self.by_kind[FrameKind::Delta.index()]);
		captured_hist.add(&self.by_kind[FrameKind::Key.index()]);
		latency.insert(GROUP_CAPTURED.to_string(), captured_hist.distributions());
		let mut frame_size = BTreeMap::new();
		for kind in FrameKind::ALL {
			latency.insert(kind.as_str().to_string(), self.by_kind[kind.index()].distributions());
			frame_size.insert(
				kind.as_str().to_string(),
				Distribution::from_histogram(&self.size_by_kind[kind.index()]),
			);
		}

		Summary {
			frames: self.frames.clone(),
			duration_s,
			target_fps: self.target_fps,
			effective_fps,
			fps_ratio: if self.target_fps == 0 {
				0.0
			} else {
				effective_fps / self.target_fps as f64
			},
			mbps,
			encoded_bytes: self.encoded_bytes,
			over_budget: rate(self.over_budget),
			pacing_miss: rate(self.pacing_miss),
			lagged: self.lagged,
			dropped: self.dropped,
			unaccounted_negative: self.unaccounted_negative,
			unaccounted_min: self.unaccounted_min_ns as f64 / 1000.0,
			latency,
			interval: Distribution::from_histogram(&self.interval),
			frame_size,
		}
	}
}

/// Render a summary in the same layout as `moonshine-bench`, so the two can be
/// cross-checked line by line.
pub fn format_summary(label: &str, s: &Summary) -> String {
	use std::fmt::Write;
	let mut out = String::new();
	let get = |stage: &str| s.latency(GROUP_CAPTURED, stage).cloned().unwrap_or_default();
	let total = get(TOTAL);
	let _ = writeln!(
		out,
		"{label} [{} frames, {:.1} fps, {:.2} Mbps]",
		s.frames.captured(),
		s.effective_fps,
		s.mbps
	);
	for (name, key) in [
		("total:   ", TOTAL),
		("submit:  ", "submit"),
		("enc_wait:", "encode_wait"),
	] {
		let d = if key == TOTAL { total.clone() } else { get(key) };
		let _ = writeln!(
			out,
			"  {name} avg={:.0}us  min={:.0}us  max={:.0}us",
			d.mean, d.min, d.max
		);
		let _ = writeln!(
			out,
			"            p50={:.0}us  p95={:.0}us  p99={:.0}us  p99.9={:.0}us",
			d.p50, d.p95, d.p99, d.p999
		);
	}
	let avg = |stage: &str| get(stage).mean;
	let _ = writeln!(
		out,
		"  avg breakdown: ch_wait={:.0}us  import={:.0}us  convert={:.0}us  submit={:.0}us  enc_wait={:.0}us  pkt={:.0}us  send={:.0}us",
		avg("channel_wait"),
		avg("import"),
		avg("convert"),
		avg("submit"),
		avg("encode_wait"),
		avg("packetize"),
		avg("send"),
	);
	let accounted: f64 = Stage::SEQUENTIAL.iter().map(|st| avg(st.as_str())).sum();
	let _ = writeln!(
		out,
		"  accounted: avg={accounted:.0}us  delta={:.0}us  queue_diag={:.0}us",
		total.mean - accounted,
		avg("consumer_queue")
	);
	let _ = writeln!(
		out,
		"  key_frames: {}  idr_reencodes: {}",
		s.frames.key, s.frames.idr_reencode
	);
	let _ = writeln!(
		out,
		"  interval: p50={:.0}us  p99={:.0}us  max={:.0}us  pacing_miss={} ({:.2}%)  over_budget={} ({:.2}%)",
		s.interval.p50,
		s.interval.p99,
		s.interval.max,
		s.pacing_miss.count,
		s.pacing_miss.pct,
		s.over_budget.count,
		s.over_budget.pct
	);
	if s.lagged > 0 || s.dropped.unwrap_or(0) > 0 || s.unaccounted_negative > 0 {
		let _ = writeln!(
			out,
			"  lagged={}  dropped={}  negative_unaccounted={} (min {:.0}us)",
			s.lagged,
			s.dropped.map_or("n/a".to_string(), |d| d.to_string()),
			s.unaccounted_negative,
			s.unaccounted_min
		);
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::sample::Deriver;

	fn frame(at: Instant, total_us: u64, key: bool) -> FrameSample {
		let us = Duration::from_micros;
		FrameSample {
			channel_wait: us(100),
			import: us(100),
			convert: us(100),
			submit: us(100),
			encode_wait: us(total_us.saturating_sub(500)),
			packetize: us(50),
			send: us(50),
			total: us(total_us),
			encoded_bytes: if key { 100_000 } else { 10_000 },
			is_key_frame: key,
			..FrameSample::zeroed(at)
		}
	}

	#[test]
	fn exact_percentiles_and_rates() {
		let t0 = Instant::now();
		let mut deriver = Deriver::new(100);
		let mut b = SummaryBuilder::new(100);
		// 1000 frames at 100 fps; totals 1..=1000 µs, frame 500 is a key frame, frame 999 is 20 ms.
		for i in 0..1000u64 {
			let at = t0 + Duration::from_millis(10 * i);
			let total = if i == 999 { 20_000 } else { i + 1 };
			let s = frame(at, total.max(500), i == 500);
			let d = deriver.derive(&s);
			b.record(&s, &d);
		}
		let s = b.build();
		assert_eq!(s.frames.delta, 999);
		assert_eq!(s.frames.key, 1);
		assert_eq!(s.frames.kind_inferred, 1000);
		assert!((s.effective_fps - 100.0).abs() < 1e-6, "{}", s.effective_fps);
		assert!((s.fps_ratio - 1.0).abs() < 1e-6);
		assert_eq!(s.over_budget.count, 1);
		assert!((s.over_budget.pct - 0.1).abs() < 1e-9);
		assert_eq!(s.pacing_miss.count, 0);
		let total = s.latency(GROUP_CAPTURED, TOTAL).unwrap();
		assert_eq!(total.count, 1000);
		// HdrHistogram reports the highest value equivalent at 3 significant figures.
		assert!((total.max - 20_000.0).abs() / 20_000.0 < 1e-3, "{}", total.max);
		// Values under 500 were clamped to 500, so p50 is exactly 500.
		assert_eq!(total.p50, 500.0);
		assert!((total.p99 - 990.0).abs() <= 1.0, "{}", total.p99);
		assert_eq!(s.latency("key", TOTAL).unwrap().count, 1);
		assert_eq!(s.latency("idr_reencode", TOTAL).unwrap().count, 0);
		assert_eq!(s.interval.count, 999);
		assert!((s.interval.p50 - 10_000.0).abs() / 10_000.0 < 1e-3);
		assert!((s.frame_size["key"].max - 100_000.0).abs() / 100_000.0 < 1e-3);
	}

	#[test]
	fn reencodes_do_not_pollute_captured_group() {
		let t0 = Instant::now();
		let mut deriver = Deriver::new(60);
		let mut b = SummaryBuilder::new(60);
		let s = frame(t0, 5000, false);
		b.record(&s, &deriver.derive(&s));
		let mut idr = FrameSample::zeroed(t0 + Duration::from_millis(16));
		idr.is_key_frame = true;
		idr.total = Duration::from_micros(700);
		b.record(&idr, &deriver.derive(&idr));
		let sum = b.build();
		assert_eq!(sum.frames.idr_reencode, 1);
		assert_eq!(sum.latency(GROUP_CAPTURED, "import").unwrap().count, 1);
		assert_eq!(sum.latency(GROUP_CAPTURED, "import").unwrap().min, 100.0);
		assert_eq!(sum.latency("idr_reencode", TOTAL).unwrap().max, 700.0);
	}

	#[test]
	fn negative_unaccounted_is_counted() {
		let t0 = Instant::now();
		let mut s = frame(t0, 5000, false);
		s.total = Duration::from_micros(4000);
		let mut b = SummaryBuilder::new(60);
		b.record(&s, &Deriver::new(60).derive(&s));
		let sum = b.build();
		assert_eq!(sum.unaccounted_negative, 1);
		assert_eq!(sum.unaccounted_min, -1000.0);
		assert_eq!(sum.latency(GROUP_CAPTURED, "unaccounted").unwrap().max, 0.0);
	}

	#[test]
	fn loss_ratio_counts_lag_and_drops() {
		let mut b = SummaryBuilder::new(60);
		let t0 = Instant::now();
		let mut d = Deriver::new(60);
		for i in 0..998 {
			let s = frame(t0 + Duration::from_millis(i), 1000, false);
			b.record(&s, &d.derive(&s));
		}
		b.record_lagged(1);
		b.record_dropped(1);
		assert!((b.build().loss_ratio() - 0.002).abs() < 1e-12);
	}

	#[test]
	fn empty_summary_is_all_zero() {
		let s = SummaryBuilder::new(60).build();
		assert_eq!(s.effective_fps, 0.0);
		assert_eq!(s.latency(GROUP_CAPTURED, TOTAL).unwrap().count, 0);
		assert!(format_summary("Session", &s).contains("0 frames"));
	}
}
