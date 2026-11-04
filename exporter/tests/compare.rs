//! Golden tests for `compare` against checked-in result files.
//!
//! The fixtures are generated from a seeded synthetic pipeline. Regenerate with
//! `UPDATE_SNAPSHOTS=1 cargo test -p moonshine-otel-exporter --test compare`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use moonshine_otel_exporter::compare::{CompareOptions, Format, Outcome, Verdict, compare};
use moonshine_otel_exporter::results::{IterationResult, ResultsFile, RunResult};
use moonshine_otel_exporter::{Deriver, FrameSample, RunConfig, RunIdentity, SummaryBuilder};

/// xorshift64*, so fixtures are deterministic.
struct Rng(u64);

impl Rng {
	fn next(&mut self) -> f64 {
		self.0 ^= self.0 >> 12;
		self.0 ^= self.0 << 25;
		self.0 ^= self.0 >> 27;
		(self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64
	}

	/// Roughly normal (sum of uniforms).
	fn normal(&mut self) -> f64 {
		(0..6).map(|_| self.next()).sum::<f64>() - 3.0
	}
}

struct Scenario {
	seed: u64,
	/// Added to `convert` (µs).
	convert_shift: f64,
	/// Per-iteration shift of `encode_wait`, scaled by a normal draw (µs).
	iteration_noise: f64,
	gpu: &'static str,
}

fn identity(gpu: &str) -> RunIdentity {
	let mut id = RunIdentity::default();
	id.build.profile = Some("release".into());
	id.gpu.model = Some(gpu.into());
	id.host.cpu_governor = Some("performance".into());
	id
}

fn config() -> RunConfig {
	RunConfig {
		command: vec!["vkcube".into()],
		width: 1920,
		height: 1080,
		target_fps: 60,
		codec: "hevc".into(),
		bitrate: 20_000_000,
		chroma: "yuv420".into(),
		warmup_s: 4.0,
		duration_s: 20.0,
		iterations: 5,
		..Default::default()
	}
}

fn generate(name: &str, s: Scenario) -> ResultsFile {
	let mut rng = Rng(s.seed);
	let mut iterations = Vec::new();
	for index in 0..5 {
		let t0 = Instant::now();
		let mut deriver = Deriver::new(60);
		let mut b = SummaryBuilder::new(60);
		let shift = s.iteration_noise * rng.normal();
		for i in 0..1200u64 {
			let us = |v: f64| Duration::from_micros(v.max(1.0) as u64);
			let convert = 300.0 + s.convert_shift + 20.0 * rng.normal();
			let encode = 3000.0 + shift + 150.0 * rng.normal();
			let stages = [60.0, 40.0, convert, 150.0, encode, 70.0, 25.0];
			let total: f64 = stages.iter().sum::<f64>() + 40.0;
			let sample = FrameSample {
				channel_wait: us(stages[0]),
				import: us(stages[1]),
				convert: us(stages[2]),
				submit: us(stages[3]),
				consumer_queue: us(20.0),
				encode_wait: us(stages[4]),
				packetize: us(stages[5]),
				send: us(stages[6]),
				total: us(total),
				encoded_bytes: 41_000 + (rng.next() * 2000.0) as u64,
				is_key_frame: i == 0,
				..FrameSample::zeroed(t0 + Duration::from_micros(16_667 * i))
			};
			b.record(&sample, &deriver.derive(&sample));
		}
		iterations.push(IterationResult::new(
			index,
			format!("2026-09-24T10:1{index}:00Z"),
			b.build(),
		));
	}
	let run = RunResult::new(
		format!("{name}-run"),
		"2026-09-24T10:10:00Z".into(),
		identity(s.gpu),
		config(),
		iterations,
		vec![],
	);
	let mut file = ResultsFile::new(vec![run]);
	file.created_at = "2026-09-24T10:20:00Z".into();
	file.tool_version = "0.0.0".into();
	file
}

fn fixtures() -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/compare")
}

fn scenarios() -> Vec<(&'static str, Scenario)> {
	let base = |seed| Scenario {
		seed,
		convert_shift: 0.0,
		iteration_noise: 10.0,
		gpu: "Example GPU",
	};
	vec![
		("baseline", base(1)),
		("no_change", base(2)),
		(
			"regression",
			Scenario {
				convert_shift: 1500.0,
				..base(3)
			},
		),
		(
			"noisy",
			Scenario {
				iteration_noise: 400.0,
				..base(4)
			},
		),
		(
			"identity_mismatch",
			Scenario {
				gpu: "Other GPU",
				..base(5)
			},
		),
	]
}

fn load(name: &str) -> ResultsFile {
	let path = fixtures().join(format!("{name}.json"));
	static GENERATED: std::sync::Once = std::sync::Once::new();
	if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
		GENERATED.call_once(|| {
			std::fs::create_dir_all(fixtures()).unwrap();
			for (n, s) in scenarios() {
				generate(n, s).write(&fixtures().join(format!("{n}.json"))).unwrap();
			}
		});
	}
	ResultsFile::read(&path).expect("run with UPDATE_SNAPSHOTS=1 to create fixtures")
}

fn run(candidate: &str) -> moonshine_otel_exporter::compare::Comparison {
	compare(&load("baseline"), &load(candidate), &CompareOptions::default())
}

#[test]
fn no_change() {
	let c = run("no_change");
	assert_eq!(c.outcome, Outcome::NoRegression, "{}", c.render(Format::Text, true));
	assert!(!c.runs[0].low_confidence);
	assert!(
		c.runs[0].metrics.iter().all(|m| m.verdict == Verdict::Unchanged),
		"{}",
		c.render(Format::Text, true)
	);
}

#[test]
fn clear_regression() {
	let c = run("regression");
	assert_eq!(c.outcome, Outcome::Regression);
	let regressed: Vec<_> = c.runs[0].regressions().map(|m| m.metric.as_str()).collect();
	assert!(regressed.contains(&"latency.captured.convert.p50"), "{regressed:?}");
	assert!(regressed.contains(&"latency.captured.total.p99"), "{regressed:?}");
	assert!(regressed.iter().all(|m| !m.contains("encode_wait")), "{regressed:?}");

	// The markdown rendering is what gets pasted into PRs; keep it stable.
	let md = c.render(Format::Markdown, false);
	let path = fixtures().join("regression.md");
	if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
		std::fs::write(&path, &md).unwrap();
	}
	assert_eq!(md, std::fs::read_to_string(&path).unwrap());
}

#[test]
fn noisy_but_fine() {
	let c = run("noisy");
	assert_eq!(c.outcome, Outcome::NoRegression, "{}", c.render(Format::Text, true));
	// Iteration-to-iteration noise in encode_wait moves the medians, but the spread across
	// iterations widens the threshold beyond the 5 % relative one.
	let total = c.runs[0]
		.metrics
		.iter()
		.find(|m| m.metric == "latency.captured.total.p50")
		.unwrap();
	assert!(
		total.threshold > 0.05 * total.baseline.as_ref().unwrap().median,
		"{total:?}"
	);
}

#[test]
fn identity_mismatch_is_invalid() {
	let c = run("identity_mismatch");
	assert_eq!(c.outcome, Outcome::Invalid);
	assert!(c.invalid[0].contains("gpu.model"), "{:?}", c.invalid);
	let allowed = compare(
		&load("baseline"),
		&load("identity_mismatch"),
		&CompareOptions {
			allow_identity_mismatch: true,
			..Default::default()
		},
	);
	assert_eq!(allowed.outcome, Outcome::NoRegression);
}

#[test]
fn unmatched_workloads_are_invalid() {
	let base = load("baseline");
	let mut other = load("no_change");
	other.runs[0].config.codec = "av1".into();
	let c = compare(&base, &other, &CompareOptions::default());
	assert_eq!(c.outcome, Outcome::Invalid);
	assert_eq!(c.unmatched_candidate.len(), 1);
}

#[test]
fn json_output_round_trips() {
	let c = run("regression");
	let v: serde_json::Value = serde_json::from_str(&c.render(Format::Json, false)).unwrap();
	assert_eq!(v["outcome"], "regression");
}
