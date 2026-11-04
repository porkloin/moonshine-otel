//! Metric and span shape, checked against the SDK's in-memory exporters.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use moonshine_otel_exporter::*;
use opentelemetry::{KeyValue, Value};
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, Metric, MetricData, ResourceMetrics};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, InMemoryMetricExporterBuilder, Temporality};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData};

struct Harness {
	telemetry: Telemetry,
	metrics: InMemoryMetricExporter,
	spans: InMemorySpanExporter,
}

fn harness(trace_mode: TraceMode, buckets: HistogramBuckets) -> Harness {
	let metrics = InMemoryMetricExporterBuilder::new()
		.with_temporality(Temporality::Cumulative)
		.build();
	let spans = InMemorySpanExporter::default();
	let config = TelemetryConfig {
		trace_mode,
		histogram_buckets: buckets,
		metric_interval: Duration::from_secs(3600),
		resource: vec![KeyValue::new("gpu.model", "Test GPU")],
		..Default::default()
	};
	Harness {
		telemetry: Telemetry::with_exporters(&config, metrics.clone(), spans.clone()),
		metrics,
		spans,
	}
}

fn config() -> RunConfig {
	RunConfig {
		command: vec!["synthetic".into()],
		width: 1920,
		height: 1080,
		target_fps: 100, // 10 ms budget
		codec: "hevc".into(),
		bitrate: 20_000_000,
		chroma: "yuv420".into(),
		iterations: 1,
		..Default::default()
	}
}

/// A normal 4 ms frame observed at `t`.
fn frame(t: Instant, total_ms: u64) -> FrameSample {
	let total = Duration::from_millis(total_ms);
	FrameSample {
		channel_wait: Duration::from_micros(100),
		import: Duration::from_micros(100),
		convert: Duration::from_micros(300),
		submit: Duration::from_micros(200),
		consumer_queue: Duration::from_micros(10),
		encode_wait: total - Duration::from_micros(800),
		packetize: Duration::from_micros(50),
		send: Duration::from_micros(20),
		total,
		encoded_bytes: 20_000,
		..FrameSample::zeroed(t)
	}
}

/// Records 100 frames at 100 fps starting in the past (so nothing is warmup), with frame 50
/// over budget and frame 80 arriving late.
fn record_run(h: &Harness) -> RunResult {
	let mut run = RunRecorder::with_run_id(&h.telemetry, "run-1".into(), RunIdentity::default(), config());
	run.begin_iteration(Duration::ZERO);
	let t0 = Instant::now();
	let mut at = t0;
	for i in 0..100u64 {
		at += Duration::from_millis(if i == 80 { 30 } else { 10 });
		let total = if i == 50 { 25 } else { 4 };
		run.record(&frame(at, total));
	}
	run.record_lagged(0);
	let result = run.finish();
	h.telemetry.flush().unwrap();
	result
}

fn find<'a>(rm: &'a [ResourceMetrics], name: &str) -> &'a Metric {
	// Each flush appends an export; the last one holds the cumulative totals.
	rm.iter()
		.rev()
		.flat_map(|r| r.scope_metrics())
		.flat_map(|s| s.metrics())
		.find(|m| m.name() == name)
		.unwrap_or_else(|| panic!("metric {name} not exported"))
}

fn attrs<'a>(it: impl Iterator<Item = &'a KeyValue>) -> HashMap<String, String> {
	it.map(|kv| (kv.key.to_string(), kv.value.to_string())).collect()
}

#[test]
fn stage_histograms_are_exponential_seconds_with_run_attributes() {
	let h = harness(TraceMode::None, HistogramBuckets::Exponential);
	record_run(&h);
	let rm = h.metrics.get_finished_metrics().unwrap();

	let stage = find(&rm, "moonshine.frame.stage.duration");
	assert_eq!(stage.unit(), "s");
	let AggregatedMetrics::F64(MetricData::ExponentialHistogram(hist)) = stage.data() else {
		panic!("expected an exponential histogram, got {:?}", stage.data());
	};
	let points: Vec<_> = hist.data_points().collect();
	// One kind (delta) × nine stages.
	assert_eq!(points.len(), 9);
	for p in &points {
		assert_eq!(p.count(), 100);
		let a = attrs(p.attributes());
		assert_eq!(a["frame.kind"], "delta");
		assert_eq!(a["codec"], "hevc");
		assert_eq!(a["resolution"], "1920x1080");
		assert_eq!(a["target_fps"], "100");
		assert_eq!(a["hdr"], "false");
		assert_eq!(a["run_id"], "run-1");
		assert!(a.contains_key("stage"));
	}
	let convert = points
		.iter()
		.find(|p| attrs(p.attributes())["stage"] == "convert")
		.unwrap();
	assert!((convert.min().unwrap() - 0.0003).abs() < 1e-9);

	let total = find(&rm, "moonshine.frame.duration");
	let AggregatedMetrics::F64(MetricData::ExponentialHistogram(total)) = total.data() else {
		panic!("expected exponential histogram");
	};
	let p = total.data_points().next().unwrap();
	assert_eq!(p.max(), Some(0.025));
}

#[test]
fn counters_start_at_zero_before_any_frame() {
	let h = harness(TraceMode::None, HistogramBuckets::Exponential);
	let _run = RunRecorder::with_run_id(&h.telemetry, "z".into(), RunIdentity::default(), config());
	let rm = h.metrics.get_finished_metrics().unwrap();
	assert_eq!(rm.len(), 1, "the recorder exports its zeroed counters immediately");
	for name in [
		"moonshine.frames.over_budget",
		"moonshine.frames.pacing_miss",
		"moonshine.stats.lagged",
	] {
		let AggregatedMetrics::U64(MetricData::Sum(sum)) = find(&rm, name).data() else {
			panic!("{name} missing");
		};
		assert_eq!(sum.data_points().map(|p| p.value()).sum::<u64>(), 0);
	}
	let AggregatedMetrics::U64(MetricData::Sum(frames)) = find(&rm, "moonshine.frames").data() else {
		panic!()
	};
	assert_eq!(frames.data_points().count(), 3, "one zero series per frame kind");
}

#[test]
fn explicit_buckets_fallback() {
	let h = harness(TraceMode::None, HistogramBuckets::Explicit);
	record_run(&h);
	let rm = h.metrics.get_finished_metrics().unwrap();
	let m = find(&rm, "moonshine.frame.duration");
	let AggregatedMetrics::F64(MetricData::Histogram(hist)) = m.data() else {
		panic!("expected explicit histogram, got {:?}", m.data());
	};
	let p = hist.data_points().next().unwrap();
	assert_eq!(p.bounds().collect::<Vec<_>>(), metrics::DURATION_BOUNDS_S.to_vec());
	let size = find(&rm, "moonshine.frame.size");
	let AggregatedMetrics::U64(MetricData::Histogram(size)) = size.data() else {
		panic!("expected explicit size histogram");
	};
	assert_eq!(
		size.data_points().next().unwrap().bounds().collect::<Vec<_>>(),
		metrics::SIZE_BOUNDS_BYTES.to_vec()
	);
}

#[test]
fn counters_match_the_exact_summary() {
	let h = harness(TraceMode::None, HistogramBuckets::Exponential);
	let result = record_run(&h);
	let s = &result.iterations[0].summary;
	let rm = h.metrics.get_finished_metrics().unwrap();
	let sum = |name: &str| -> u64 {
		let AggregatedMetrics::U64(MetricData::Sum(sum)) = find(&rm, name).data() else {
			panic!("{name} is not a u64 sum");
		};
		sum.data_points().map(|p| p.value()).sum()
	};
	assert_eq!(sum("moonshine.frames"), s.frames.recorded());
	assert_eq!(sum("moonshine.frames.over_budget"), s.over_budget.count);
	assert_eq!(sum("moonshine.frames.pacing_miss"), s.pacing_miss.count);
	assert_eq!(sum("moonshine.encoded.bytes"), s.encoded_bytes);
	assert_eq!(s.over_budget.count, 1);
	assert_eq!(s.pacing_miss.count, 1);
	// Self-overhead is recorded once per frame.
	let AggregatedMetrics::F64(MetricData::ExponentialHistogram(overhead)) =
		find(&rm, "moonshine.exporter.record.duration").data()
	else {
		panic!("overhead histogram missing");
	};
	assert_eq!(overhead.data_points().next().unwrap().count(), 100);
}

fn spans(h: &Harness) -> Vec<SpanData> {
	h.spans.get_finished_spans().unwrap()
}

fn attr<'a>(span: &'a SpanData, key: &str) -> Option<&'a Value> {
	span.attributes
		.iter()
		.find(|kv| kv.key.as_str() == key)
		.map(|kv| &kv.value)
}

#[test]
fn outlier_mode_span_shape() {
	let h = harness(TraceMode::Outliers, HistogramBuckets::Exponential);
	let result = record_run(&h);
	let spans = spans(&h);
	let by_name = |n: &str| spans.iter().filter(|s| s.name == n).collect::<Vec<_>>();
	let run = by_name("bench.run");
	let iteration = by_name("bench.iteration");
	let frames = by_name("frame");
	assert_eq!((run.len(), iteration.len()), (1, 1));
	let (run, iteration) = (run[0], iteration[0]);
	assert_eq!(iteration.parent_span_id, run.span_context.span_id());
	assert_eq!(iteration.span_context.trace_id(), run.span_context.trace_id());

	// Frame 50 (over budget) and frame 80 (late) only.
	let mut seqs: Vec<i64> = frames
		.iter()
		.map(|f| match attr(f, "frame.seq") {
			Some(Value::I64(v)) => *v,
			other => panic!("frame.seq missing: {other:?}"),
		})
		.collect();
	seqs.sort();
	assert_eq!(seqs, vec![50, 80]);
	for f in &frames {
		assert_eq!(f.parent_span_id, iteration.span_context.span_id());
		assert_eq!(attr(f, "timing"), Some(&Value::from("reconstructed")));
		assert_eq!(attr(f, "frame.outlier"), Some(&Value::Bool(true)));
		for stage in Stage::ALL {
			assert!(attr(f, &format!("frame.{}_us", stage.as_str())).is_some(), "{stage:?}");
		}
		// Span duration equals the frame's total latency.
		let total_us = match attr(f, "frame.total_us") {
			Some(Value::I64(v)) => *v as u128,
			_ => unreachable!(),
		};
		let dur = f.end_time.duration_since(f.start_time).unwrap();
		assert_eq!(dur.as_micros(), total_us);
		// The lead-up event carries the preceding 8 frames.
		let ev = f.events.iter().find(|e| e.name == "lead_up").expect("lead_up event");
		let seq = ev
			.attributes
			.iter()
			.find(|kv| kv.key.as_str() == "lead_up.seq")
			.unwrap();
		match &seq.value {
			Value::Array(opentelemetry::Array::I64(v)) => assert_eq!(v.len(), 8),
			other => panic!("unexpected {other:?}"),
		}
	}
	let late = frames
		.iter()
		.find(|f| attr(f, "frame.seq") == Some(&Value::I64(80)))
		.unwrap();
	assert_eq!(attr(late, "frame.pacing_miss"), Some(&Value::Bool(true)));
	assert_eq!(attr(late, "frame.interval_us"), Some(&Value::I64(30_000)));

	// The run span carries identity, workload and the exact aggregate.
	assert_eq!(attr(run, "run_id"), Some(&Value::from("run-1")));
	assert_eq!(attr(run, "bench.codec"), Some(&Value::from("hevc")));
	let p99 = match attr(run, "result.latency.captured.total.p99") {
		Some(Value::F64(v)) => *v,
		other => panic!("summary attribute missing: {other:?}"),
	};
	assert_eq!(p99, result.median("latency.captured.total.p99").unwrap());
	assert_eq!(attr(run, "result.valid"), Some(&Value::Bool(true)));

	let stats = h.telemetry.span_stats();
	assert_eq!(stats.dropped(), 0);
	assert_eq!(stats.exported.load(std::sync::atomic::Ordering::Relaxed), 4);
}

#[test]
fn sampled_mode_keeps_every_nth_frame_plus_outliers() {
	let h = harness(TraceMode::Sampled(0.1), HistogramBuckets::Exponential);
	record_run(&h);
	let mut seqs: Vec<i64> = spans(&h)
		.iter()
		.filter(|s| s.name == "frame")
		.filter_map(|s| match attr(s, "frame.seq") {
			Some(Value::I64(v)) => Some(*v),
			_ => None,
		})
		.collect();
	seqs.sort();
	assert_eq!(seqs, vec![0, 10, 20, 30, 40, 50, 60, 70, 80, 90]);
}

#[test]
fn none_mode_emits_only_run_spans() {
	let h = harness(TraceMode::None, HistogramBuckets::Exponential);
	record_run(&h);
	let names: Vec<_> = spans(&h).iter().map(|s| s.name.to_string()).collect();
	assert_eq!(names.len(), 2, "{names:?}");
	assert!(!names.contains(&"frame".to_string()));
}

#[test]
fn warmup_frames_are_not_exported() {
	let h = harness(TraceMode::All, HistogramBuckets::Exponential);
	let mut run = RunRecorder::with_run_id(&h.telemetry, "warm".into(), RunIdentity::default(), config());
	run.begin_iteration(Duration::from_secs(3600));
	for i in 0..10 {
		run.record(&frame(Instant::now() + Duration::from_millis(i), 4));
	}
	assert!(!run.is_warm());
	let result = run.finish();
	h.telemetry.flush().unwrap();
	let s = &result.iterations[0].summary;
	assert_eq!(s.frames.warmup_discarded, 10);
	assert_eq!(s.frames.recorded(), 0);
	assert!(!result.valid, "a run without frames is invalid");
	assert!(spans(&h).iter().all(|s| s.name != "frame"));
}

#[test]
fn lag_invalidates_the_run() {
	let h = harness(TraceMode::None, HistogramBuckets::Exponential);
	let mut run = RunRecorder::new(&h.telemetry, RunIdentity::default(), config());
	run.begin_iteration(Duration::ZERO);
	let t0 = Instant::now();
	for i in 0..100 {
		run.record(&frame(t0 + Duration::from_millis(10 * i), 4));
	}
	run.record_lagged(5);
	let result = run.finish();
	assert!(!result.valid);
	assert!(result.iterations[0].warnings[0].contains("lagged 5"));
	h.telemetry.flush().unwrap();
	let rm = h.metrics.get_finished_metrics().unwrap();
	let AggregatedMetrics::U64(MetricData::Sum(lagged)) = find(&rm, "moonshine.stats.lagged").data() else {
		panic!("lagged counter missing");
	};
	assert_eq!(lagged.data_points().map(|p| p.value()).sum::<u64>(), 5);
}

#[test]
fn disabled_telemetry_still_summarizes() {
	let t = Telemetry::disabled();
	let mut run = RunRecorder::new(&t, RunIdentity::default(), config());
	assert!(run.metrics().is_none());
	run.begin_iteration(Duration::ZERO);
	let t0 = Instant::now();
	for i in 0..10 {
		run.record(&frame(t0 + Duration::from_millis(10 * i), 4));
	}
	let result = run.finish();
	assert_eq!(result.iterations[0].summary.frames.recorded(), 10);
	t.shutdown().unwrap();
}

#[test]
fn content_switches_count_changes_after_warmup_by_target() {
	let h = harness(TraceMode::None, HistogramBuckets::Exponential);
	let mut run = RunRecorder::with_run_id(&h.telemetry, "switch".into(), RunIdentity::default(), config());
	let t0 = Instant::now();
	// Frames before t0 + 50 ms are warmup: the change to hdr10 there sets the starting content but isn't counted.
	run.begin_iteration(Duration::from_millis(50));
	let contents = [
		Some("sdr"),
		Some("hdr10"),
		Some("hdr10"),
		Some("hdr10"),
		Some("hdr10"),
		Some("hdr10"),
		Some("hdr10"),
		Some("scrgb"),
		None,
		Some("scrgb"),
		Some("sdr"),
		Some("hdr10"),
	];
	for (i, content) in contents.into_iter().enumerate() {
		let mut f = frame(t0 + Duration::from_millis(10 * i as u64), 4);
		f.content = content;
		run.record(&f);
	}
	run.finish();
	h.telemetry.flush().unwrap();
	let rm = h.metrics.get_finished_metrics().unwrap();
	let AggregatedMetrics::U64(MetricData::Sum(sum)) = find(&rm, "moonshine.content.switches").data() else {
		panic!("moonshine.content.switches is not a u64 sum");
	};
	let by_target: HashMap<String, u64> = sum
		.data_points()
		.map(|p| (attrs(p.attributes()).get("to").cloned().unwrap_or_default(), p.value()))
		.collect();
	// After warmup: hdr10 -> scrgb, (a re-encode in between), scrgb -> sdr, sdr -> hdr10; and the zero it starts at.
	assert_eq!(by_target.get("scrgb"), Some(&1));
	assert_eq!(by_target.get("sdr"), Some(&1));
	assert_eq!(by_target.get("hdr10"), Some(&1));
	assert_eq!(by_target.get(""), Some(&0));
}
