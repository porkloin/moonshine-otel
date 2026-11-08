//! `bench.run` → `bench.iteration` → `frame` spans for outlier forensics.
//!
//! Stages are frame span *attributes*, not child spans: the stages overlap (`consumer_queue`
//! is inside `encode_wait`) and `FrameStats` only reports durations, so any sequential layout of
//! child spans would be invented.

use std::collections::VecDeque;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};

use opentelemetry::trace::{Link, Span, SpanKind, TraceContextExt, Tracer};
use opentelemetry::{Array, Context, KeyValue, Value};
use opentelemetry_sdk::trace::SdkTracer;

use crate::frame_ids::{FrameIds, with_ids};
use crate::results::flatten;
use crate::sample::{Derived, FrameSample, Stage};
use crate::summary::Summary;
use crate::telemetry::SpanStats;

/// Which frames get a `frame` span.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum TraceMode {
	/// No frame spans. `bench.run` and `bench.iteration` spans are still exported.
	None,
	/// Frames over budget or arriving late (pacing miss).
	#[default]
	Outliers,
	/// Every ⌈1/p⌉-th frame by frame counter, plus all outliers.
	Sampled(f64),
	/// Every frame. For short, focused runs.
	All,
}

impl TraceMode {
	/// For `Sampled(p)`, keep every `n`-th frame.
	pub fn sample_every(self) -> Option<u64> {
		match self {
			TraceMode::Sampled(p) if p > 0.0 => Some((1.0 / p.min(1.0)).ceil() as u64),
			_ => None,
		}
	}

	/// Whether frame `seq` gets a span. Deterministic in `seq`, so the same frames are sampled
	/// no matter how the buffer pool cycles.
	pub fn should_trace(self, seq: u64, is_outlier: bool) -> bool {
		match self {
			TraceMode::None => false,
			TraceMode::All => true,
			TraceMode::Outliers => is_outlier,
			TraceMode::Sampled(_) => is_outlier || self.sample_every().is_some_and(|n| seq.is_multiple_of(n)),
		}
	}
}

impl FromStr for TraceMode {
	type Err = String;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		match s {
			"none" => Ok(TraceMode::None),
			"outliers" => Ok(TraceMode::Outliers),
			"all" => Ok(TraceMode::All),
			_ => {
				let p = s
					.strip_prefix("sampled:")
					.ok_or_else(|| format!("expected none, outliers, all or sampled:<p>, got `{s}`"))?;
				let p: f64 = p.parse().map_err(|e| format!("invalid sample rate `{p}`: {e}"))?;
				if !(p > 0.0 && p <= 1.0) {
					return Err(format!("sample rate must be in (0, 1], got {p}"));
				}
				Ok(TraceMode::Sampled(p))
			},
		}
	}
}

impl fmt::Display for TraceMode {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			TraceMode::None => f.write_str("none"),
			TraceMode::Outliers => f.write_str("outliers"),
			TraceMode::All => f.write_str("all"),
			TraceMode::Sampled(p) => write!(f, "sampled:{p}"),
		}
	}
}

/// How many preceding frames an outlier span carries in its `lead_up` event.
pub const LEAD_UP_FRAMES: usize = 8;

/// Maps monotonic [`Instant`]s onto wall-clock time for span timestamps.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
	instant: Instant,
	system: SystemTime,
}

impl Clock {
	pub fn now() -> Self {
		Self {
			instant: Instant::now(),
			system: SystemTime::now(),
		}
	}

	pub fn to_system(&self, t: Instant) -> SystemTime {
		if t >= self.instant {
			self.system + (t - self.instant)
		} else {
			self.system - (self.instant - t)
		}
	}
}

#[derive(Clone, Copy, Debug)]
struct LeadUp {
	seq: u64,
	total: Duration,
	interval: Option<Duration>,
}

/// Emits the spans for one run.
pub(crate) struct RunTracer {
	tracer: SdkTracer,
	mode: TraceMode,
	clock: Clock,
	run_cx: Context,
	/// Also set on iteration and frame spans: Tempo may store a run's spans in separate
	/// blocks until they are compacted, so filtering frames by their parent is unreliable.
	run_id: KeyValue,
	iteration_cx: Option<Context>,
	lead_up: VecDeque<LeadUp>,
	stats: Arc<SpanStats>,
	/// With a key, a frame with Moonshine's frame number is a trace of its own, linked to the run's.
	pub(crate) stream_key: Option<String>,
}

impl RunTracer {
	pub(crate) fn start(
		tracer: SdkTracer,
		mode: TraceMode,
		clock: Clock,
		run_id: String,
		attributes: Vec<KeyValue>,
		stats: Arc<SpanStats>,
	) -> Self {
		let span = tracer
			.span_builder("bench.run")
			.with_kind(SpanKind::Internal)
			.with_start_time(clock.to_system(Instant::now()))
			.with_attributes(attributes)
			.start(&tracer);
		Self {
			tracer,
			mode,
			clock,
			run_cx: Context::current_with_span(span),
			run_id: KeyValue::new("run_id", run_id),
			iteration_cx: None,
			lead_up: VecDeque::with_capacity(LEAD_UP_FRAMES),
			stats,
			stream_key: None,
		}
	}

	pub(crate) fn begin_iteration(&mut self, index: u32) {
		let span = self
			.tracer
			.span_builder("bench.iteration")
			.with_start_time(self.clock.to_system(Instant::now()))
			.with_attributes([KeyValue::new("bench.iteration", index as i64), self.run_id.clone()])
			.start_with_context(&self.tracer, &self.run_cx);
		self.iteration_cx = Some(self.run_cx.with_span(span));
		self.lead_up.clear();
	}

	pub(crate) fn record_frame(&mut self, sample: &FrameSample, derived: &Derived) {
		if self.mode.should_trace(derived.seq, derived.is_outlier()) {
			self.emit_frame(sample, derived);
		}
		if self.lead_up.len() == LEAD_UP_FRAMES {
			self.lead_up.pop_front();
		}
		self.lead_up.push_back(LeadUp {
			seq: derived.seq,
			total: sample.total,
			interval: derived.interval,
		});
	}

	fn emit_frame(&mut self, sample: &FrameSample, derived: &Derived) {
		let parent = self.iteration_cx.as_ref().unwrap_or(&self.run_cx);
		let (start, reconstructed) = sample.start_time();
		let us = |d: Duration| d.as_micros() as i64;
		let mut attrs = Vec::with_capacity(24);
		attrs.push(self.run_id.clone());
		attrs.push(KeyValue::new("frame.seq", derived.seq as i64));
		attrs.push(KeyValue::new("frame.kind", derived.kind.as_str()));
		attrs.push(KeyValue::new("frame.kind_inferred", sample.kind_is_inferred()));
		attrs.push(KeyValue::new("frame.total_us", us(sample.total)));
		for stage in Stage::ALL {
			let v = match sample.stage(stage) {
				Some(d) => us(d),
				None => derived.unaccounted_ns / 1000,
			};
			attrs.push(KeyValue::new(format!("frame.{}_us", stage.as_str()), v));
		}
		if let Some(i) = derived.interval {
			attrs.push(KeyValue::new("frame.interval_us", us(i)));
		}
		// The GPU video queue's timings, carried like `interval`: only on a frame that reports them.
		for (key, d) in [
			("frame.gpu_encode_us", sample.gpu_encode),
			("frame.bitstream_ready_us", sample.bitstream_ready),
		] {
			if let Some(d) = d {
				attrs.push(KeyValue::new(key, us(d)));
			}
		}
		attrs.push(KeyValue::new("frame.encoded_bytes", sample.encoded_bytes as i64));
		if let Some(content) = sample.content {
			attrs.push(KeyValue::new("frame.content", content));
		}
		attrs.push(KeyValue::new("frame.over_budget", derived.over_budget));
		attrs.push(KeyValue::new("frame.pacing_miss", derived.pacing_miss));
		if let Some(c) = derived.cadence {
			attrs.push(KeyValue::new("frame.cadence_us", us(c)));
		}
		attrs.push(KeyValue::new("frame.outlier", derived.is_outlier()));
		attrs.push(KeyValue::new(
			"timing",
			if reconstructed { "reconstructed" } else { "captured" },
		));

		let builder = self
			.tracer
			.span_builder("frame")
			.with_start_time(self.clock.to_system(start));
		let mut span = match (&self.stream_key, sample.seq) {
			(Some(key), Some(number)) => {
				attrs.push(KeyValue::new("stream.key", key.clone()));
				attrs.push(KeyValue::new("frame.number", number as i64));
				let run = Link::with_context(self.run_cx.span().span_context().clone());
				with_ids(FrameIds::new(key, number), || {
					builder
						.with_attributes(attrs)
						.with_links(vec![run])
						.start_with_context(&self.tracer, &Context::new())
				})
			},
			_ => builder.with_attributes(attrs).start_with_context(&self.tracer, parent),
		};
		if derived.is_outlier() && !self.lead_up.is_empty() {
			let seqs = self.lead_up.iter().map(|l| l.seq as i64).collect();
			let totals = self.lead_up.iter().map(|l| us(l.total)).collect();
			let intervals = self.lead_up.iter().map(|l| l.interval.map_or(-1, us)).collect();
			span.add_event(
				"lead_up",
				vec![
					KeyValue::new("lead_up.seq", Value::Array(Array::I64(seqs))),
					KeyValue::new("lead_up.total_us", Value::Array(Array::I64(totals))),
					KeyValue::new("lead_up.interval_us", Value::Array(Array::I64(intervals))),
				],
			);
		}
		span.end_with_timestamp(self.clock.to_system(start + sample.total));
		self.stats.ended.fetch_add(1, Ordering::Relaxed);
	}

	pub(crate) fn end_iteration(&mut self, summary: &Summary, valid: bool) {
		if let Some(cx) = self.iteration_cx.take() {
			let span = cx.span();
			span.set_attributes(result_attributes(&flatten(summary)));
			span.set_attribute(KeyValue::new("result.valid", valid));
			span.end_with_timestamp(self.clock.to_system(Instant::now()));
			self.stats.ended.fetch_add(1, Ordering::Relaxed);
		}
	}

	pub(crate) fn finish(self, medians: &std::collections::BTreeMap<String, f64>, valid: bool) {
		let span = self.run_cx.span();
		span.set_attributes(result_attributes(medians));
		span.set_attribute(KeyValue::new("result.valid", valid));
		span.end_with_timestamp(self.clock.to_system(Instant::now()));
		self.stats.ended.fetch_add(1, Ordering::Relaxed);
	}
}

/// `result.<flattened name>` attributes, e.g. `result.latency.captured.total.p99` (µs).
fn result_attributes(values: &std::collections::BTreeMap<String, f64>) -> Vec<KeyValue> {
	values
		.iter()
		.map(|(k, v)| KeyValue::new(format!("result.{k}"), *v))
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parse_and_display() {
		for s in ["none", "outliers", "all", "sampled:0.1"] {
			assert_eq!(s.parse::<TraceMode>().unwrap().to_string(), s);
		}
		assert!("sampled:0".parse::<TraceMode>().is_err());
		assert!("sampled:1.5".parse::<TraceMode>().is_err());
		assert!("sometimes".parse::<TraceMode>().is_err());
	}

	#[test]
	fn sampling_is_every_nth_frame() {
		let m = TraceMode::Sampled(0.1);
		assert_eq!(m.sample_every(), Some(10));
		let kept: Vec<u64> = (0..50).filter(|s| m.should_trace(*s, false)).collect();
		assert_eq!(kept, vec![0, 10, 20, 30, 40]);
		// Rounds the period up, so the rate never exceeds p.
		assert_eq!(TraceMode::Sampled(0.3).sample_every(), Some(4));
		assert_eq!(TraceMode::Sampled(1.0).sample_every(), Some(1));
	}

	#[test]
	fn outliers_always_traced_except_none() {
		for m in [TraceMode::Outliers, TraceMode::Sampled(0.01), TraceMode::All] {
			assert!(m.should_trace(7, true), "{m}");
		}
		assert!(!TraceMode::None.should_trace(0, true));
		assert!(!TraceMode::Outliers.should_trace(0, false));
	}

	#[test]
	fn clock_maps_both_directions() {
		let c = Clock::now();
		let later = c.instant + Duration::from_millis(5);
		assert_eq!(c.to_system(later), c.system + Duration::from_millis(5));
		let earlier = c.instant - Duration::from_millis(5);
		assert_eq!(c.to_system(earlier), c.system - Duration::from_millis(5));
	}
}
