//! OTel instruments for live exploration in Prometheus/Grafana.

use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter};
use opentelemetry_sdk::metrics::{Aggregation, Instrument, InstrumentKind, Stream};

use crate::sample::{Derived, FrameKind, FrameSample, Stage};

/// How latency and size histograms are bucketed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HistogramBuckets {
	/// Base-2 exponential histograms. With Prometheus native histograms these give accurate
	/// quantiles from 100 µs to 1 s without any bucket tuning.
	#[default]
	Exponential,
	/// Fixed buckets ([`DURATION_BOUNDS_S`], [`SIZE_BOUNDS_BYTES`]) for setups without native
	/// histograms.
	Explicit,
}

impl std::str::FromStr for HistogramBuckets {
	type Err = String;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		match s {
			"exponential" => Ok(Self::Exponential),
			"explicit" => Ok(Self::Explicit),
			_ => Err(format!("expected `exponential` or `explicit`, got `{s}`")),
		}
	}
}

/// Explicit duration buckets in seconds: dense from 0.25 ms to 50 ms, where frame budgets live.
pub const DURATION_BOUNDS_S: [f64; 30] = [
	0.0001, 0.00025, 0.0005, 0.00075, 0.001, 0.0015, 0.002, 0.0025, 0.003, 0.004, 0.005, 0.006, 0.007, 0.008, 0.00833,
	0.01, 0.0125, 0.015, 0.0167, 0.02, 0.025, 0.03, 0.035, 0.04, 0.05, 0.075, 0.1, 0.25, 0.5, 1.0,
];

/// Explicit frame size buckets in bytes.
pub const SIZE_BOUNDS_BYTES: [f64; 14] = [
	1e3, 2.5e3, 5e3, 1e4, 2.5e4, 5e4, 1e5, 2e5, 3e5, 5e5, 7.5e5, 1e6, 2e6, 4e6,
];

/// Explicit buckets for the per-frame recording overhead: 1 µs to 1 ms.
pub const OVERHEAD_BOUNDS_S: [f64; 10] = [1e-6, 2e-6, 5e-6, 1e-5, 2e-5, 5e-5, 1e-4, 2e-4, 5e-4, 1e-3];

/// Exponential histogram size. 160 buckets span 1 µs–1 s at scale ≥ 3 (≈9 % bucket width).
const EXPO_MAX_SIZE: u32 = 160;
const EXPO_MAX_SCALE: i8 = 20;

/// A view that applies `buckets` to every `moonshine.*` histogram.
pub fn histogram_view(buckets: HistogramBuckets) -> impl Fn(&Instrument) -> Option<Stream> + Send + Sync + 'static {
	move |inst: &Instrument| {
		if inst.kind() != InstrumentKind::Histogram || !inst.name().starts_with("moonshine.") {
			return None;
		}
		let aggregation = match buckets {
			HistogramBuckets::Exponential => Aggregation::Base2ExponentialHistogram {
				max_size: EXPO_MAX_SIZE,
				max_scale: EXPO_MAX_SCALE,
				record_min_max: true,
			},
			HistogramBuckets::Explicit => Aggregation::ExplicitBucketHistogram {
				boundaries: if inst.unit() == "By" {
					SIZE_BOUNDS_BYTES.to_vec()
				} else if inst.name() == "moonshine.exporter.record.duration" {
					OVERHEAD_BOUNDS_S.to_vec()
				} else {
					DURATION_BOUNDS_S.to_vec()
				},
				record_min_max: true,
			},
		};
		Stream::builder().with_aggregation(aggregation).build().ok()
	}
}

/// All instruments. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Instruments {
	frame_duration: Histogram<f64>,
	stage_duration: Histogram<f64>,
	interval: Histogram<f64>,
	frame_size: Histogram<u64>,
	frames: Counter<u64>,
	encoded_bytes: Counter<u64>,
	over_budget: Counter<u64>,
	pacing_miss: Counter<u64>,
	dropped: Counter<u64>,
	lagged: Counter<u64>,
	content_switches: Counter<u64>,
	gpu_utilization: Gauge<f64>,
	gpu_memory: Gauge<u64>,
	cpu_time: Counter<f64>,
	record_duration: Histogram<f64>,
}

impl Instruments {
	pub fn new(meter: &Meter) -> Self {
		Self {
			frame_duration: meter
				.f64_histogram("moonshine.frame.duration")
				.with_unit("s")
				.with_description("End-to-end host latency of a frame, capture to send")
				.build(),
			stage_duration: meter
				.f64_histogram("moonshine.frame.stage.duration")
				.with_unit("s")
				.with_description("Time a frame spent in one pipeline stage")
				.build(),
			interval: meter
				.f64_histogram("moonshine.frame.interval")
				.with_unit("s")
				.with_description("Time between consecutive captured frames")
				.build(),
			frame_size: meter
				.u64_histogram("moonshine.frame.size")
				.with_unit("By")
				.with_description("Encoded frame size")
				.build(),
			frames: meter
				.u64_counter("moonshine.frames")
				.with_description("Frames recorded")
				.build(),
			encoded_bytes: meter
				.u64_counter("moonshine.encoded.bytes")
				.with_unit("By")
				.with_description("Encoded bytes produced")
				.build(),
			over_budget: meter
				.u64_counter("moonshine.frames.over_budget")
				.with_description("Captured frames whose latency exceeded 1/target_fps")
				.build(),
			pacing_miss: meter
				.u64_counter("moonshine.frames.pacing_miss")
				.with_description("Captured frames arriving more than 1.5/target_fps after the previous one")
				.build(),
			dropped: meter
				.u64_counter("moonshine.frames.dropped")
				.with_description("Frames dropped by the pipeline")
				.build(),
			content_switches: meter
				.u64_counter("moonshine.content.switches")
				.with_description(
					"Times the captured content changed color space between frames, by the one it changed to",
				)
				.build(),
			lagged: meter
				.u64_counter("moonshine.stats.lagged")
				.with_description("Frame stats lost because the harness fell behind the broadcast channel")
				.build(),
			gpu_utilization: meter
				.f64_gauge("moonshine.gpu.engine.utilization")
				.with_unit("1")
				.with_description("Share of time a GPU engine was busy over the last sample interval")
				.build(),
			gpu_memory: meter
				.u64_gauge("moonshine.gpu.memory.used")
				.with_unit("By")
				.with_description("GPU memory used by the benchmark process")
				.build(),
			cpu_time: meter
				.f64_counter("moonshine.process.cpu.time")
				.with_unit("s")
				.with_description("CPU time of the benchmark process by thread name")
				.build(),
			record_duration: meter
				.f64_histogram("moonshine.exporter.record.duration")
				.with_unit("s")
				.with_description("Time spent recording one frame (instrumentation overhead)")
				.build(),
		}
	}
}

/// Attribute sets for one run, built once so recording a frame does not allocate.
#[derive(Debug, Clone)]
struct RunAttributes {
	base: Vec<KeyValue>,
	by_kind: Vec<Vec<KeyValue>>,
	/// Indexed `[kind][stage]`.
	by_kind_stage: Vec<Vec<Vec<KeyValue>>>,
}

impl RunAttributes {
	fn new(base: Vec<KeyValue>) -> Self {
		let with =
			|extra: &[KeyValue]| -> Vec<KeyValue> { base.iter().cloned().chain(extra.iter().cloned()).collect() };
		let by_kind = FrameKind::ALL
			.iter()
			.map(|k| with(&[KeyValue::new("frame.kind", k.as_str())]))
			.collect();
		let by_kind_stage = FrameKind::ALL
			.iter()
			.map(|k| {
				Stage::ALL
					.iter()
					.map(|s| {
						with(&[
							KeyValue::new("frame.kind", k.as_str()),
							KeyValue::new("stage", s.as_str()),
						])
					})
					.collect()
			})
			.collect();
		Self {
			base,
			by_kind,
			by_kind_stage,
		}
	}
}

/// Records one run's metrics. Cheap to clone, so samplers on other tasks can hold one.
#[derive(Debug, Clone)]
pub struct RunMetrics {
	inst: Instruments,
	attrs: std::sync::Arc<RunAttributes>,
}

impl RunMetrics {
	pub fn new(inst: Instruments, base: Vec<KeyValue>) -> Self {
		Self {
			inst,
			attrs: std::sync::Arc::new(RunAttributes::new(base)),
		}
	}

	pub fn record_frame(&self, sample: &FrameSample, derived: &Derived) {
		let k = derived.kind.index();
		let a = &*self.attrs;
		self.inst
			.frame_duration
			.record(sample.total.as_secs_f64(), &a.by_kind[k]);
		for stage in Stage::ALL {
			self.inst.stage_duration.record(
				derived.stage_duration(sample, stage).as_secs_f64(),
				&a.by_kind_stage[k][stage.index()],
			);
		}
		if let Some(interval) = derived.interval {
			self.inst.interval.record(interval.as_secs_f64(), &a.base);
		}
		self.inst.frame_size.record(sample.encoded_bytes, &a.by_kind[k]);
		self.inst.frames.add(1, &a.by_kind[k]);
		self.inst.encoded_bytes.add(sample.encoded_bytes, &a.base);
		if derived.over_budget {
			self.inst.over_budget.add(1, &a.base);
		}
		if derived.pacing_miss {
			self.inst.pacing_miss.add(1, &a.base);
		}
	}

	/// Create every counter series at zero. Prometheus `rate`/`increase` ignore a series'
	/// first sample, so a counter that first appears with value 3 would read as 0; call this at
	/// run start and export once before any frame is recorded.
	pub fn initialize(&self) {
		let a = &*self.attrs;
		for kind in &a.by_kind {
			self.inst.frames.add(0, kind);
		}
		for c in [
			&self.inst.over_budget,
			&self.inst.pacing_miss,
			&self.inst.lagged,
			&self.inst.encoded_bytes,
			&self.inst.content_switches,
		] {
			c.add(0, &a.base);
		}
	}

	pub fn record_lagged(&self, n: u64) {
		self.inst.lagged.add(n, &self.attrs.base);
	}

	/// The captured content changed color space, to `to`.
	pub fn record_content_switch(&self, to: &'static str) {
		let mut attrs = self.attrs.base.clone();
		attrs.push(KeyValue::new("to", to));
		self.inst.content_switches.add(1, &attrs);
	}

	pub fn record_dropped(&self, n: u64, reason: &str) {
		let mut attrs = self.attrs.base.clone();
		attrs.push(KeyValue::new("reason", reason.to_string()));
		self.inst.dropped.add(n, &attrs);
	}

	pub fn record_overhead(&self, d: Duration) {
		self.inst.record_duration.record(d.as_secs_f64(), &self.attrs.base);
	}

	/// Busy share (0–1) of one GPU engine (`gfx`, `enc`, ...) used by `process` (`bench` for
	/// the pipeline itself, `app` for the benchmarked application).
	pub fn record_gpu_utilization(&self, process: &str, engine: &str, value: f64) {
		let mut attrs = self.attrs.base.clone();
		attrs.push(KeyValue::new("process", process.to_string()));
		attrs.push(KeyValue::new("engine", engine.to_string()));
		self.inst.gpu_utilization.record(value, &attrs);
	}

	/// Resident GPU memory of `process`, by region (`vram`, `gtt`, ...).
	pub fn record_gpu_memory(&self, process: &str, region: &str, bytes: u64) {
		let mut attrs = self.attrs.base.clone();
		attrs.push(KeyValue::new("process", process.to_string()));
		attrs.push(KeyValue::new("region", region.to_string()));
		self.inst.gpu_memory.record(bytes, &attrs);
	}

	/// CPU seconds a thread group used since the previous call.
	pub fn record_cpu_time(&self, thread: &str, seconds: f64) {
		let mut attrs = self.attrs.base.clone();
		attrs.push(KeyValue::new("thread", thread.to_string()));
		self.inst.cpu_time.add(seconds, &attrs);
	}
}
