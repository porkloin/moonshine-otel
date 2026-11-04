//! [`RunRecorder`]: the single entry point a harness feeds frames into.

use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime};

use opentelemetry::KeyValue;

use crate::Telemetry;
use crate::identity::{RunConfig, RunIdentity};
use crate::metrics::RunMetrics;
use crate::results::{FrameCsvWriter, IterationResult, RunResult, new_run_id, rfc3339};
use crate::sample::{Deriver, FrameSample};
use crate::summary::SummaryBuilder;
use crate::traces::{Clock, RunTracer};

struct Iteration {
	index: u32,
	started_at: String,
	warmup_until: Instant,
	summary: SummaryBuilder,
}

/// Records one run (one configuration, one or more iterations).
///
/// ```no_run
/// # use moonshine_otel_exporter::*;
/// # use std::time::Duration;
/// # fn frames() -> Vec<FrameSample> { vec![] }
/// let telemetry = Telemetry::disabled();
/// let mut run = RunRecorder::new(&telemetry, RunIdentity::default(), RunConfig::default());
/// run.begin_iteration(Duration::from_secs(4));
/// for f in frames() {
///     run.record(&f);
/// }
/// run.end_iteration();
/// let result = run.finish();
/// ```
pub struct RunRecorder {
	run_id: String,
	started_at: String,
	identity: RunIdentity,
	config: RunConfig,
	deriver: Deriver,
	metrics: Option<RunMetrics>,
	tracer: Option<RunTracer>,
	csv: Option<FrameCsvWriter>,
	current: Option<Iteration>,
	iterations: Vec<IterationResult>,
	warnings: Vec<String>,
	/// The last frame's content color space, to count switches.
	last_content: Option<&'static str>,
}

impl RunRecorder {
	pub fn new(telemetry: &Telemetry, identity: RunIdentity, config: RunConfig) -> Self {
		Self::with_run_id(telemetry, new_run_id(), identity, config)
	}

	pub fn with_run_id(telemetry: &Telemetry, run_id: String, identity: RunIdentity, config: RunConfig) -> Self {
		Self::build(telemetry, run_id, identity, config, None)
	}

	/// A run whose stream has a key: every frame with Moonshine's frame number is a trace of its own, with ids
	/// derived from `key` and that number (see `frame_ids`), so a client told the same key can add its spans to
	/// it, and the metrics carry `stream_key` to match the client's.
	pub fn keyed(telemetry: &Telemetry, identity: RunIdentity, config: RunConfig, key: String) -> Self {
		Self::build(telemetry, new_run_id(), identity, config, Some(key))
	}

	fn build(
		telemetry: &Telemetry,
		run_id: String,
		identity: RunIdentity,
		config: RunConfig,
		stream_key: Option<String>,
	) -> Self {
		let mut metric_attributes = config.metric_attributes(&run_id);
		if let Some(key) = &stream_key {
			metric_attributes.push(KeyValue::new("stream_key", key.clone()));
		}
		let metrics = telemetry
			.instruments()
			.map(|inst| RunMetrics::new(inst.clone(), metric_attributes));
		if let Some(m) = &metrics {
			m.initialize();
			// Export the zeros now, before warmup ends, so the first real increments count.
			if let Err(e) = telemetry.flush() {
				tracing::warn!("{e:#}");
			}
		}
		let tracer = telemetry.tracer().map(|(tracer, mode, stats)| {
			let mut attrs = vec![
				KeyValue::new("run_id", run_id.clone()),
				KeyValue::new("bench.trace_mode", mode.to_string()),
			];
			attrs.extend(config.span_attributes());
			attrs.extend(identity.resource_attributes());
			let mut t = RunTracer::start(tracer.clone(), mode, Clock::now(), run_id.clone(), attrs, stats);
			t.stream_key = stream_key;
			t
		});
		Self {
			run_id,
			started_at: rfc3339(SystemTime::now()),
			deriver: Deriver::new(config.target_fps),
			identity,
			config,
			metrics,
			tracer,
			csv: None,
			current: None,
			iterations: Vec::new(),
			warnings: Vec::new(),
			last_content: None,
		}
	}

	/// Also write every frame (including warmup) to a CSV file.
	pub fn with_frames_csv(mut self, csv: FrameCsvWriter) -> Self {
		self.csv = Some(csv);
		self
	}

	pub fn run_id(&self) -> &str {
		&self.run_id
	}

	pub fn config(&self) -> &RunConfig {
		&self.config
	}

	/// Take the frame CSV writer back (flushed), e.g. to hand it to the next matrix cell.
	pub fn take_frames_csv(&mut self) -> Option<FrameCsvWriter> {
		let mut csv = self.csv.take()?;
		if let Err(e) = csv.flush() {
			self.warnings.push(format!("frame CSV is incomplete: {e:#}"));
		}
		Some(csv)
	}

	/// Iterations completed so far.
	pub fn iterations(&self) -> &[IterationResult] {
		&self.iterations
	}

	/// Handle for samplers (GPU, CPU) running alongside the pipeline.
	pub fn metrics(&self) -> Option<RunMetrics> {
		self.metrics.clone()
	}

	/// Add a warning that ends up in the result file.
	pub fn warn(&mut self, warning: impl Into<String>) {
		self.warnings.push(warning.into());
	}

	/// Start a measurement window. Frames observed within `warmup` from now are discarded.
	pub fn begin_iteration(&mut self, warmup: Duration) {
		if self.current.is_some() {
			self.end_iteration();
		}
		let index = self.iterations.len() as u32;
		self.deriver.reset();
		if let Some(t) = &mut self.tracer {
			t.begin_iteration(index);
		}
		self.current = Some(Iteration {
			index,
			started_at: rfc3339(SystemTime::now()),
			warmup_until: Instant::now() + warmup,
			summary: SummaryBuilder::new(self.config.target_fps),
		});
	}

	/// Whether the current iteration is past its warmup.
	pub fn is_warm(&self) -> bool {
		self.current
			.as_ref()
			.is_some_and(|it| Instant::now() >= it.warmup_until)
	}

	/// Frames recorded (post-warmup) in the current iteration.
	pub fn recorded_frames(&self) -> u64 {
		self.current.as_ref().map_or(0, |it| it.summary.frames().recorded())
	}

	/// Record one frame. Never blocks; cost is measured and exported as
	/// `moonshine.exporter.record.duration`.
	pub fn record(&mut self, sample: &FrameSample) {
		let t0 = Instant::now();
		let Some(it) = &mut self.current else {
			return;
		};
		let derived = self.deriver.derive(sample);
		let warmup = sample.observed_at < it.warmup_until;
		// A content switch counts after warmup, like the frames, so switches and keyframes cover the same frames.
		if let Some(content) = sample.content {
			if !warmup
				&& self.last_content.is_some_and(|last| last != content)
				&& let Some(m) = &self.metrics
			{
				m.record_content_switch(content);
			}
			self.last_content = Some(content);
		}
		if let Some(csv) = &mut self.csv
			&& let Err(e) = csv.write(&self.run_id, it.index, warmup, sample, &derived)
		{
			tracing::warn!("disabling frame CSV after write error: {e:#}");
			self.csv = None;
		}
		if warmup {
			it.summary.record_warmup();
			return;
		}
		it.summary.record(sample, &derived);
		if let Some(m) = &self.metrics {
			m.record_frame(sample, &derived);
		}
		if let Some(t) = &mut self.tracer {
			t.record_frame(sample, &derived);
		}
		if let Some(m) = &self.metrics {
			m.record_overhead(t0.elapsed());
		}
	}

	/// The harness missed `n` samples (broadcast `Lagged`).
	pub fn record_lagged(&mut self, n: u64) {
		if let Some(it) = &mut self.current {
			it.summary.record_lagged(n);
		}
		if let Some(m) = &self.metrics {
			m.record_lagged(n);
		}
	}

	/// The pipeline dropped `n` frames.
	pub fn record_dropped(&mut self, n: u64, reason: &str) {
		if let Some(it) = &mut self.current {
			it.summary.record_dropped(n);
		}
		if let Some(m) = &self.metrics {
			m.record_dropped(n, reason);
		}
	}

	/// Close the current measurement window, attaching averaged resource usage (see
	/// [`IterationResult::resources`]) and any warnings. An `incomplete` window (cut short by
	/// Ctrl+C or a pipeline failure) is marked invalid so it stays out of the aggregate.
	pub fn end_iteration_with(
		&mut self,
		resources: BTreeMap<String, f64>,
		warnings: Vec<String>,
		incomplete: Option<String>,
	) -> Option<&IterationResult> {
		let it = self.current.take()?;
		let mut result = IterationResult::new(it.index, it.started_at, it.summary.build());
		result.resources = resources;
		result.warnings.extend(warnings);
		if let Some(reason) = incomplete {
			result.valid = false;
			result.warnings.push(format!("incomplete iteration: {reason}"));
		}
		if let Some(t) = &mut self.tracer {
			t.end_iteration(&result.summary, result.valid);
		}
		self.iterations.push(result);
		self.iterations.last()
	}

	/// Close the current measurement window.
	pub fn end_iteration(&mut self) -> Option<&IterationResult> {
		self.end_iteration_with(BTreeMap::new(), Vec::new(), None)
	}

	/// Finish the run: close any open iteration, end the `bench.run` span with the aggregate,
	/// and return the result.
	pub fn finish(mut self) -> RunResult {
		self.end_iteration();
		if let Some(csv) = &mut self.csv
			&& let Err(e) = csv.flush()
		{
			self.warnings.push(format!("frame CSV is incomplete: {e:#}"));
		}
		let result = RunResult::new(
			self.run_id,
			self.started_at,
			self.identity,
			self.config,
			self.iterations,
			self.warnings,
		);
		if let Some(t) = self.tracer {
			let medians = result.aggregate.iter().map(|(k, s)| (k.clone(), s.median)).collect();
			t.finish(&medians, result.valid);
		}
		result
	}
}
