//! Machine-readable run results (versioned JSON) and the raw per-frame CSV dump.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::time::{Instant, SystemTime};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::identity::{RunConfig, RunIdentity};
use crate::sample::{Derived, FrameSample, Stage};
use crate::summary::{GROUP_CAPTURED, Summary, TOTAL};

/// Bump on any incompatible change to [`ResultsFile`].
pub const SCHEMA_VERSION: u32 = 1;

/// Largest share of frames that may be lost (lagged + dropped) before a run is marked invalid.
pub const MAX_LOSS_RATIO: f64 = 0.001;

/// A result file: one run, or one run per matrix cell.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResultsFile {
	pub schema_version: u32,
	pub tool: String,
	pub tool_version: String,
	/// RFC 3339.
	pub created_at: String,
	pub runs: Vec<RunResult>,
}

impl ResultsFile {
	pub fn new(runs: Vec<RunResult>) -> Self {
		Self {
			schema_version: SCHEMA_VERSION,
			tool: "moonshine-otel".to_string(),
			tool_version: env!("CARGO_PKG_VERSION").to_string(),
			created_at: rfc3339(SystemTime::now()),
			runs,
		}
	}

	pub fn read(path: &Path) -> anyhow::Result<Self> {
		let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
		Self::from_json(&text).with_context(|| format!("parsing {}", path.display()))
	}

	/// Read a result file, or every `*.json` result file in a directory. Runs with the same
	/// configuration are combined into one run with all their iterations, so repeated
	/// recordings of the same stream (one file each, from `watch`) compare like iterations.
	pub fn read_path(path: &Path) -> anyhow::Result<Self> {
		if !path.is_dir() {
			return Self::read(path);
		}
		let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(path)
			.with_context(|| format!("reading {}", path.display()))?
			.filter_map(|e| e.ok().map(|e| e.path()))
			.filter(|p| p.extension().is_some_and(|e| e == "json"))
			.collect();
		files.sort();
		if files.is_empty() {
			bail!("no result files (*.json) in {}", path.display());
		}
		let mut runs = Vec::new();
		for file in &files {
			runs.extend(Self::read(file)?.runs);
		}
		Ok(Self::new(merge_runs(runs)))
	}

	pub fn from_json(text: &str) -> anyhow::Result<Self> {
		#[derive(Deserialize)]
		struct Version {
			schema_version: u32,
		}
		let v: Version = serde_json::from_str(text).context("not a moonshine-otel result file")?;
		if v.schema_version != SCHEMA_VERSION {
			bail!(
				"unsupported schema_version {} (this build reads {})",
				v.schema_version,
				SCHEMA_VERSION
			);
		}
		Ok(serde_json::from_str(text)?)
	}

	pub fn to_json(&self) -> String {
		serde_json::to_string_pretty(self).expect("results always serialize")
	}

	/// Write atomically (temp file + rename), so a reader never sees a partial file.
	pub fn write(&self, path: &Path) -> anyhow::Result<()> {
		let mut text = self.to_json();
		text.push('\n');
		let mut tmp = path.as_os_str().to_owned();
		static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
		let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
		tmp.push(format!(".{}-{seq}.tmp", std::process::id()));
		let tmp = std::path::PathBuf::from(tmp);
		std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
		std::fs::rename(&tmp, path).with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))
	}
}

/// One configuration, measured one or more times.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunResult {
	pub run_id: String,
	pub started_at: String,
	pub identity: RunIdentity,
	pub config: RunConfig,
	pub iterations: Vec<IterationResult>,
	/// Median and spread of every flattened metric across iterations.
	pub aggregate: BTreeMap<String, Spread>,
	pub valid: bool,
	pub warnings: Vec<String>,
}

impl RunResult {
	/// Build a run result, computing the aggregate and validity from its iterations.
	pub fn new(
		run_id: String,
		started_at: String,
		identity: RunIdentity,
		config: RunConfig,
		iterations: Vec<IterationResult>,
		mut warnings: Vec<String>,
	) -> Self {
		let mut aggregate = aggregate(iterations.iter().filter(|i| i.valid).map(|i| &i.summary));
		let mut resources: BTreeMap<String, Vec<f64>> = BTreeMap::new();
		for it in iterations.iter().filter(|i| i.valid) {
			for (k, v) in &it.resources {
				resources.entry(format!("resources.{k}")).or_default().push(*v);
			}
		}
		aggregate.extend(resources.into_iter().map(|(k, v)| (k, Spread::from_values(&v))));
		for it in iterations.iter().filter(|i| !i.valid) {
			warnings.push(format!(
				"iteration {} is invalid and excluded from the aggregate",
				it.index
			));
		}
		let valid = !iterations.is_empty() && iterations.iter().all(|i| i.valid);
		if iterations.len() == 1 {
			warnings.push("single iteration: spread is unknown, comparisons are low-confidence".to_string());
		}
		Self {
			run_id,
			started_at,
			identity,
			config,
			iterations,
			aggregate,
			valid,
			warnings,
		}
	}

	/// Median of a flattened metric across valid iterations.
	pub fn median(&self, metric: &str) -> Option<f64> {
		self.aggregate.get(metric).map(|s| s.median)
	}
}

/// Combine runs with the same configuration into one run each, keeping their iterations in
/// order. The first run of each configuration gives the run id, start time and identity.
pub fn merge_runs(runs: Vec<RunResult>) -> Vec<RunResult> {
	let mut groups: Vec<(String, Vec<RunResult>)> = Vec::new();
	for run in runs {
		let key = run.config.key();
		match groups.iter_mut().find(|(k, _)| *k == key) {
			Some((_, group)) => group.push(run),
			None => groups.push((key, vec![run])),
		}
	}
	groups
		.into_iter()
		.map(|(_, group)| {
			if group.len() == 1 {
				return group.into_iter().next().unwrap();
			}
			let mut iterations = Vec::new();
			let mut warnings: Vec<String> = Vec::new();
			for run in &group {
				for it in &run.iterations {
					let mut it = it.clone();
					it.index = iterations.len() as u32;
					iterations.push(it);
				}
				// RunResult::new derives these again from the combined iterations.
				for w in &run.warnings {
					let derived = w.starts_with("single iteration") || w.starts_with("iteration ");
					if !derived && !warnings.contains(w) {
						warnings.push(w.clone());
					}
				}
			}
			let first = &group[0];
			let mut config = first.config.clone();
			config.iterations = iterations.len() as u32;
			RunResult::new(
				first.run_id.clone(),
				first.started_at.clone(),
				first.identity.clone(),
				config,
				iterations,
				warnings,
			)
		})
		.collect()
}

/// One measurement window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IterationResult {
	pub index: u32,
	pub started_at: String,
	pub summary: Summary,
	/// Resource usage averaged over the measurement window, e.g. `gpu.bench.enc.busy` (0–1)
	/// or `cpu.bench.<thread>.util` (cores). Empty when not sampled.
	#[serde(default)]
	pub resources: BTreeMap<String, f64>,
	pub valid: bool,
	pub warnings: Vec<String>,
}

impl IterationResult {
	pub fn new(index: u32, started_at: String, summary: Summary) -> Self {
		let mut warnings = Vec::new();
		let loss = summary.loss_ratio();
		if loss > MAX_LOSS_RATIO {
			warnings.push(format!(
				"{:.2}% of frames lost (lagged {}, dropped {}), above the {:.1}% limit",
				loss * 100.0,
				summary.lagged,
				summary.dropped.map_or("n/a".into(), |d| d.to_string()),
				MAX_LOSS_RATIO * 100.0
			));
		}
		if summary.frames.captured() == 0 {
			warnings.push("no frames were captured".to_string());
		}
		if summary.unaccounted_negative > 0 {
			warnings.push(format!(
				"{} frames have stages summing to more than total (min {:.0}us): the stage timings overlap",
				summary.unaccounted_negative, summary.unaccounted_min
			));
		}
		let valid = loss <= MAX_LOSS_RATIO && summary.frames.captured() > 0;
		Self {
			index,
			started_at,
			summary,
			resources: BTreeMap::new(),
			valid,
			warnings,
		}
	}
}

/// Center and spread of one metric across iterations.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Spread {
	pub median: f64,
	/// Median absolute deviation (unscaled).
	pub mad: f64,
	pub min: f64,
	pub max: f64,
	pub n: u32,
}

impl Spread {
	pub fn from_values(values: &[f64]) -> Self {
		if values.is_empty() {
			return Self::default();
		}
		let median = median(values);
		let deviations: Vec<f64> = values.iter().map(|v| (v - median).abs()).collect();
		Self {
			median,
			mad: median_of(deviations),
			min: values.iter().copied().fold(f64::INFINITY, f64::min),
			max: values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
			n: values.len() as u32,
		}
	}
}

fn median(values: &[f64]) -> f64 {
	median_of(values.to_vec())
}

fn median_of(mut v: Vec<f64>) -> f64 {
	v.sort_by(f64::total_cmp);
	let n = v.len();
	if n == 0 {
		0.0
	} else if n % 2 == 1 {
		v[n / 2]
	} else {
		(v[n / 2 - 1] + v[n / 2]) / 2.0
	}
}

/// Statistics of each latency distribution that are flattened into the aggregate.
pub const FLAT_STATS: [&str; 5] = ["mean", "p50", "p95", "p99", "p999"];

/// Flatten a summary into `name → value`. Names are stable and are what `compare` and the
/// aggregate refer to, e.g. `latency.captured.total.p99` (µs).
pub fn flatten(s: &Summary) -> BTreeMap<String, f64> {
	let mut out = BTreeMap::new();
	if let Some(group) = s.latency.get(GROUP_CAPTURED) {
		let names = std::iter::once(TOTAL).chain(Stage::ALL.iter().map(|st| st.as_str()));
		for stage in names {
			if let Some(d) = group.get(stage) {
				for stat in FLAT_STATS {
					out.insert(
						format!("latency.{GROUP_CAPTURED}.{stage}.{stat}"),
						d.stat(stat).unwrap_or(0.0),
					);
				}
			}
		}
	}
	for stat in ["p50", "p99", "p999", "max"] {
		out.insert(format!("interval.{stat}"), s.interval.stat(stat).unwrap_or(0.0));
	}
	out.insert("over_budget_pct".into(), s.over_budget.pct);
	out.insert("pacing_miss_pct".into(), s.pacing_miss.pct);
	out.insert("effective_fps".into(), s.effective_fps);
	out.insert("fps_ratio".into(), s.fps_ratio);
	out.insert("mbps".into(), s.mbps);
	out.insert("loss_ratio".into(), s.loss_ratio());
	out.insert("frames.captured".into(), s.frames.captured() as f64);
	out.insert("frames.idr_reencode".into(), s.frames.idr_reencode as f64);
	out
}

/// Median/MAD of every flattened metric across the given summaries.
pub fn aggregate<'a>(summaries: impl IntoIterator<Item = &'a Summary>) -> BTreeMap<String, Spread> {
	let mut values: BTreeMap<String, Vec<f64>> = BTreeMap::new();
	for s in summaries {
		for (k, v) in flatten(s) {
			values.entry(k).or_default().push(v);
		}
	}
	values.into_iter().map(|(k, v)| (k, Spread::from_values(&v))).collect()
}

/// Format a time as RFC 3339 (UTC, second precision).
pub fn rfc3339(t: SystemTime) -> String {
	humantime::format_rfc3339_seconds(t).to_string()
}

/// A short, sortable, unique-enough run identifier: `20260924-101500-3f2a`.
pub fn new_run_id() -> String {
	let now = SystemTime::now();
	let stamp = rfc3339(now).replace(['-', ':'], "").replace('T', "-").replace('Z', "");
	let nanos = now
		.duration_since(SystemTime::UNIX_EPOCH)
		.map(|d| d.subsec_nanos())
		.unwrap_or(0);
	let salt = (nanos ^ std::process::id().rotate_left(16)) & 0xffff;
	format!("{stamp}-{salt:04x}")
}

/// Streams one CSV row per frame, for notebook analysis.
pub struct FrameCsvWriter {
	writer: csv::Writer<Box<dyn Write + Send>>,
	origin: Instant,
	/// `origin` by the wall clock, for `captured_unix_us`.
	wall_origin: SystemTime,
}

impl FrameCsvWriter {
	pub const HEADER: [&'static str; 23] = [
		"run_id",
		"iteration",
		"seq",
		"observed_us",
		"warmup",
		"kind",
		"kind_inferred",
		"total_us",
		"channel_wait_us",
		"import_us",
		"convert_us",
		"submit_us",
		"consumer_queue_us",
		"encode_wait_us",
		"packetize_us",
		"send_us",
		"unaccounted_us",
		"interval_us",
		"encoded_bytes",
		"over_budget",
		"pacing_miss",
		"timing_reconstructed",
		"captured_unix_us",
	];

	/// `origin` is the zero point of the `observed_us` column.
	pub fn new(out: Box<dyn Write + Send>, origin: Instant) -> anyhow::Result<Self> {
		let mut writer = csv::Writer::from_writer(out);
		writer.write_record(Self::HEADER)?;
		let wall_origin = SystemTime::now() - origin.elapsed();
		Ok(Self {
			writer,
			origin,
			wall_origin,
		})
	}

	pub fn create(path: &Path, origin: Instant) -> anyhow::Result<Self> {
		let file = std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
		Self::new(Box::new(std::io::BufWriter::new(file)), origin)
	}

	pub fn write(
		&mut self,
		run_id: &str,
		iteration: u32,
		warmup: bool,
		sample: &FrameSample,
		derived: &Derived,
	) -> anyhow::Result<()> {
		let us = |d: std::time::Duration| d.as_micros().to_string();
		let observed = sample.observed_at.saturating_duration_since(self.origin);
		let mut row: Vec<String> = vec![
			run_id.to_string(),
			iteration.to_string(),
			derived.seq.to_string(),
			us(observed),
			(warmup as u8).to_string(),
			derived.kind.as_str().to_string(),
			(sample.kind_is_inferred() as u8).to_string(),
			us(sample.total),
		];
		for stage in Stage::ALL {
			row.push(match sample.stage(stage) {
				Some(d) => us(d),
				None => (derived.unaccounted_ns / 1000).to_string(),
			});
		}
		row.push(derived.interval.map(us).unwrap_or_default());
		row.push(sample.encoded_bytes.to_string());
		row.push((derived.over_budget as u8).to_string());
		row.push((derived.pacing_miss as u8).to_string());
		row.push((sample.captured_at.is_none() as u8).to_string());
		// The capture by the wall clock, to line the frame up with the client's record of it.
		row.push(
			sample
				.captured_at
				.and_then(|c| {
					let wall = if c >= self.origin {
						self.wall_origin + (c - self.origin)
					} else {
						self.wall_origin - (self.origin - c)
					};
					wall.duration_since(SystemTime::UNIX_EPOCH).ok()
				})
				.map(us)
				.unwrap_or_default(),
		);
		self.writer.write_record(&row)?;
		Ok(())
	}

	pub fn flush(&mut self) -> anyhow::Result<()> {
		self.writer.flush()?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn spread_median_and_mad() {
		let s = Spread::from_values(&[1.0, 2.0, 3.0, 4.0, 100.0]);
		assert_eq!(s.median, 3.0);
		// deviations 2,1,0,1,97 → median 1
		assert_eq!(s.mad, 1.0);
		assert_eq!((s.min, s.max, s.n), (1.0, 100.0, 5));
		let even = Spread::from_values(&[1.0, 3.0]);
		assert_eq!(even.median, 2.0);
	}

	fn stream(codec: &str, fps: f64) -> RunResult {
		let config = RunConfig {
			width: 1920,
			height: 1080,
			target_fps: 60,
			codec: codec.into(),
			bitrate: 20_000_000,
			chroma: "yuv420".into(),
			iterations: 1,
			..Default::default()
		};
		let mut summary = Summary {
			effective_fps: fps,
			..Default::default()
		};
		summary.frames.delta = 100;
		let it = IterationResult::new(0, String::new(), summary);
		RunResult::new(
			format!("{codec}-{fps}"),
			String::new(),
			RunIdentity::default(),
			config,
			vec![it],
			vec!["seen".into()],
		)
	}

	#[test]
	fn merges_streams_of_one_configuration() {
		let runs = merge_runs(vec![
			stream("hevc", 60.0),
			stream("av1", 59.0),
			stream("hevc", 58.0),
			stream("hevc", 59.0),
		]);
		assert_eq!(runs.len(), 2);
		let hevc = &runs[0];
		assert_eq!(hevc.run_id, "hevc-60");
		assert_eq!(hevc.config.iterations, 3);
		assert_eq!(hevc.iterations.iter().map(|i| i.index).collect::<Vec<_>>(), [0, 1, 2]);
		assert_eq!(hevc.median("effective_fps"), Some(59.0));
		assert_eq!(hevc.aggregate["effective_fps"].n, 3);
		assert_eq!(hevc.warnings, ["seen"]);
		assert_eq!(runs[1].iterations.len(), 1);
	}

	#[test]
	fn reads_a_directory_of_results() {
		let dir = std::env::temp_dir().join(format!("moonshine-otel-results-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		for (i, fps) in [60.0, 58.0].into_iter().enumerate() {
			ResultsFile::new(vec![stream("hevc", fps)])
				.write(&dir.join(format!("{i}.json")))
				.unwrap();
		}
		std::fs::write(dir.join("0.frames.csv"), "not json").unwrap();
		let file = ResultsFile::read_path(&dir).unwrap();
		std::fs::remove_dir_all(&dir).unwrap();
		assert_eq!(file.runs.len(), 1);
		assert_eq!(file.runs[0].iterations.len(), 2);
	}

	#[test]
	fn rejects_other_schema_versions() {
		let err = ResultsFile::from_json(r#"{"schema_version": 99}"#).unwrap_err();
		assert!(err.to_string().contains("unsupported schema_version 99"));
		assert!(ResultsFile::from_json("{}").is_err());
	}

	#[test]
	fn run_id_shape() {
		let id = new_run_id();
		assert_eq!(id.len(), "20260924-101500-3f2a".len(), "{id}");
		assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
	}

	#[test]
	fn csv_header_matches_row_width() {
		let t0 = Instant::now();
		let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
		struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
		impl Write for Shared {
			fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
				self.0.lock().unwrap().write(b)
			}
			fn flush(&mut self) -> std::io::Result<()> {
				Ok(())
			}
		}
		let mut w = FrameCsvWriter::new(Box::new(Shared(buf.clone())), t0).unwrap();
		let s = FrameSample::zeroed(t0);
		let d = crate::sample::Deriver::new(60).derive(&s);
		w.write("r", 0, false, &s, &d).unwrap();
		w.flush().unwrap();
		let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
		let lines: Vec<&str> = text.lines().collect();
		assert_eq!(lines.len(), 2);
		assert_eq!(lines[0].split(',').count(), lines[1].split(',').count(), "{text}");
	}
}
