//! Noise-aware A/B comparison of two result files.
//!
//! A metric is flagged only when the difference of medians exceeds
//! `max(rel_threshold × |baseline|, k × σ̂, floor)`, where `σ̂ = 1.4826 × √(MAD_a² + MAD_b²)`
//! is the combined robust spread across iterations and `floor` is a per-metric absolute
//! minimum (below which differences are not practically meaningful).

use std::collections::BTreeMap;
use std::fmt::Write;

use serde::{Deserialize, Serialize};

use crate::identity::{IdentityMismatch, Severity};
use crate::results::{ResultsFile, RunResult, Spread};
use crate::sample::Stage;

/// Scales a MAD to an estimate of the standard deviation for normal data.
const MAD_TO_SIGMA: f64 = 1.4826;

/// Frames that must lie beyond a tail quantile, per iteration, for it to decide a verdict.
pub const MIN_TAIL_FRAMES: f64 = 10.0;

/// The quantile a metric name ends in (`….p99` → 0.99), for the tail quantiles.
fn tail_quantile(metric: &str) -> Option<f64> {
	match metric.rsplit('.').next()? {
		"p95" => Some(0.95),
		"p99" => Some(0.99),
		"p999" => Some(0.999),
		_ => None,
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
	LowerIsBetter,
	HigherIsBetter,
	/// A change is reported but is neither better nor worse (e.g. bitrate).
	Neutral,
}

/// A metric `compare` looks at.
#[derive(Clone, Debug, PartialEq)]
pub struct MetricSpec {
	/// Flattened metric name (see [`crate::results::flatten`]).
	pub name: String,
	pub direction: Direction,
	/// Differences smaller than this are never flagged.
	pub floor: f64,
	/// Overrides [`CompareOptions::rel_threshold`] for this metric.
	pub rel_threshold: Option<f64>,
	pub unit: &'static str,
	/// Always shown in the table, changed or not.
	pub headline: bool,
}

/// The default set: per-stage and total latency percentiles, stutter, budget, fps and bitrate.
pub fn default_metrics() -> Vec<MetricSpec> {
	let mut out = Vec::new();
	let spec = |name: String, direction, floor, unit, headline| MetricSpec {
		name,
		direction,
		floor,
		unit,
		headline,
		rel_threshold: None,
	};
	for stat in ["p50", "p95", "p99", "p999"] {
		out.push(spec(
			format!("latency.captured.total.{stat}"),
			Direction::LowerIsBetter,
			20.0,
			"us",
			true,
		));
	}
	for stage in Stage::ALL {
		for stat in ["p50", "p95", "p99", "p999"] {
			out.push(spec(
				format!("latency.captured.{}.{stat}", stage.as_str()),
				Direction::LowerIsBetter,
				20.0,
				"us",
				false,
			));
		}
	}
	out.push(spec("interval.p99".into(), Direction::LowerIsBetter, 100.0, "us", true));
	out.push(spec("over_budget_pct".into(), Direction::LowerIsBetter, 0.1, "%", true));
	out.push(spec("pacing_miss_pct".into(), Direction::LowerIsBetter, 0.1, "%", true));
	// Lost frames can masquerade as a latency win, so fps gets a tight threshold: 0.5 fps or
	// 1 %, whichever is larger, instead of the default 5 %.
	out.push(MetricSpec {
		rel_threshold: Some(0.01),
		..spec("effective_fps".into(), Direction::HigherIsBetter, 0.5, "fps", true)
	});
	out.push(spec("mbps".into(), Direction::Neutral, 0.5, "Mbps", true));
	out
}

#[derive(Clone, Debug)]
pub struct CompareOptions {
	/// Relative threshold (0.05 = 5 %).
	pub rel_threshold: f64,
	/// Multiples of the combined spread a difference must exceed.
	pub k: f64,
	/// Treat identity warnings (driver, kernel, governor, host) as invalidating.
	pub strict_identity: bool,
	/// Compare even when identities differ in invalidating ways (GPU, debug build).
	pub allow_identity_mismatch: bool,
	pub metrics: Vec<MetricSpec>,
}

impl Default for CompareOptions {
	fn default() -> Self {
		Self {
			rel_threshold: 0.05,
			k: 3.0,
			strict_identity: false,
			allow_identity_mismatch: false,
			metrics: default_metrics(),
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
	Improved,
	Regressed,
	Unchanged,
	/// Neutral metric moved beyond its threshold.
	Changed,
	/// Missing on one side.
	Missing,
}

impl Verdict {
	fn label(self) -> &'static str {
		match self {
			Verdict::Improved => "improved",
			Verdict::Regressed => "REGRESSED",
			Verdict::Unchanged => "unchanged",
			Verdict::Changed => "changed",
			Verdict::Missing => "missing",
		}
	}
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MetricComparison {
	pub metric: String,
	pub unit: String,
	pub baseline: Option<Spread>,
	pub candidate: Option<Spread>,
	/// `candidate − baseline` medians.
	pub delta: f64,
	/// `delta / |baseline|`, in percent (0 when the baseline is 0).
	pub delta_pct: f64,
	/// The difference needed to flag this metric.
	pub threshold: f64,
	pub verdict: Verdict,
	#[serde(skip)]
	headline: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunComparison {
	/// The workload key both runs share.
	pub key: String,
	pub baseline_run_id: String,
	pub candidate_run_id: String,
	pub baseline_iterations: u32,
	pub candidate_iterations: u32,
	/// True when either side has a single valid iteration.
	pub low_confidence: bool,
	pub identity: Vec<IdentityMismatch>,
	pub metrics: Vec<MetricComparison>,
}

impl RunComparison {
	pub fn regressions(&self) -> impl Iterator<Item = &MetricComparison> {
		self.metrics.iter().filter(|m| m.verdict == Verdict::Regressed)
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
	NoRegression,
	Regression,
	Invalid,
}

impl Outcome {
	/// Process exit code: 0 no regression, 1 regression, 2 invalid comparison.
	pub fn exit_code(self) -> i32 {
		match self {
			Outcome::NoRegression => 0,
			Outcome::Regression => 1,
			Outcome::Invalid => 2,
		}
	}
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Comparison {
	pub outcome: Outcome,
	pub runs: Vec<RunComparison>,
	/// Workload keys only present in the baseline / candidate.
	pub unmatched_baseline: Vec<String>,
	pub unmatched_candidate: Vec<String>,
	/// Why the comparison is invalid, if it is.
	pub invalid: Vec<String>,
	pub warnings: Vec<String>,
	pub rel_threshold: f64,
	pub k: f64,
}

/// A tail quantile with only a handful of frames beyond it is the slowest few frames of each iteration: it moves with
/// single frames. Report it, but don't let it decide the verdict. The warning, when any was demoted.
fn demote_undersampled(metrics: &mut [MetricComparison], b: &RunResult, c: &RunResult) -> Option<String> {
	let frames = [b, c]
		.iter()
		.filter_map(|r| r.aggregate.get("frames.captured").map(|s| s.min))
		.fold(f64::INFINITY, f64::min);
	let mut undersampled = Vec::new();
	for m in metrics {
		if let Some(q) = tail_quantile(&m.metric)
			&& frames.is_finite()
			&& frames * (1.0 - q) < MIN_TAIL_FRAMES
		{
			if matches!(m.verdict, Verdict::Regressed | Verdict::Improved) {
				m.verdict = Verdict::Changed;
			}
			let stat = format!("p{}", q * 100.0);
			if !undersampled.contains(&stat) {
				undersampled.push(stat);
			}
		}
	}
	(!undersampled.is_empty()).then(|| {
		format!(
			"{} reported but not judged: {frames:.0} frames per iteration leave fewer than {MIN_TAIL_FRAMES:.0} beyond them (record longer streams)",
			undersampled.join(", ")
		)
	})
}

pub fn compare(baseline: &ResultsFile, candidate: &ResultsFile, opts: &CompareOptions) -> Comparison {
	let index = |f: &ResultsFile| -> BTreeMap<String, RunResult> {
		f.runs.iter().map(|r| (r.config.key(), r.clone())).collect()
	};
	let base = index(baseline);
	let cand = index(candidate);
	let mut invalid = Vec::new();
	let mut warnings = Vec::new();
	let mut runs = Vec::new();

	for (key, b) in &base {
		let Some(c) = cand.get(key) else { continue };
		for (side, r) in [("baseline", b), ("candidate", c)] {
			if !r.valid {
				invalid.push(format!("{key}: {side} run {} is marked invalid", r.run_id));
			}
		}
		let identity = b.identity.mismatches(&c.identity);
		for m in &identity {
			let text = format!(
				"{key}: {} differs ({} → {})",
				m.field,
				m.baseline.as_deref().unwrap_or("unknown"),
				m.candidate.as_deref().unwrap_or("unknown")
			);
			let fatal = match m.severity {
				Severity::Invalidating => !opts.allow_identity_mismatch,
				Severity::Warning => opts.strict_identity,
			};
			if fatal { invalid.push(text) } else { warnings.push(text) }
		}
		let valid_iterations = |r: &RunResult| r.iterations.iter().filter(|i| i.valid).count() as u32;
		let (bn, cn) = (valid_iterations(b), valid_iterations(c));
		let low_confidence = bn < 2 || cn < 2;
		if low_confidence {
			warnings.push(format!(
				"{key}: only {bn} vs {cn} valid iterations; noise is unknown, verdicts use the relative threshold alone (use --iterations 5)"
			));
		}
		let mut metrics: Vec<MetricComparison> = opts
			.metrics
			.iter()
			.map(|spec| compare_metric(spec, b.aggregate.get(&spec.name), c.aggregate.get(&spec.name), opts))
			.collect();
		if let Some(w) = demote_undersampled(&mut metrics, b, c) {
			warnings.push(format!("{key}: {w}"));
		}
		runs.push(RunComparison {
			key: key.clone(),
			baseline_run_id: b.run_id.clone(),
			candidate_run_id: c.run_id.clone(),
			baseline_iterations: bn,
			candidate_iterations: cn,
			low_confidence,
			identity,
			metrics,
		});
	}

	let unmatched_baseline: Vec<String> = base.keys().filter(|k| !cand.contains_key(*k)).cloned().collect();
	let unmatched_candidate: Vec<String> = cand.keys().filter(|k| !base.contains_key(*k)).cloned().collect();
	if runs.is_empty() {
		invalid.push("no run in the candidate matches a run in the baseline (resolution, fps, codec, bitrate, hdr and chroma must all match)".into());
	} else if !unmatched_baseline.is_empty() || !unmatched_candidate.is_empty() {
		warnings.push(format!(
			"{} baseline and {} candidate runs have no counterpart and were skipped",
			unmatched_baseline.len(),
			unmatched_candidate.len()
		));
	}

	let outcome = if !invalid.is_empty() {
		Outcome::Invalid
	} else if runs.iter().any(|r| r.regressions().next().is_some()) {
		Outcome::Regression
	} else {
		Outcome::NoRegression
	};
	Comparison {
		outcome,
		runs,
		unmatched_baseline,
		unmatched_candidate,
		invalid,
		warnings,
		rel_threshold: opts.rel_threshold,
		k: opts.k,
	}
}

fn compare_metric(
	spec: &MetricSpec,
	b: Option<&Spread>,
	c: Option<&Spread>,
	opts: &CompareOptions,
) -> MetricComparison {
	let (Some(b), Some(c)) = (b, c) else {
		return MetricComparison {
			metric: spec.name.clone(),
			unit: spec.unit.to_string(),
			baseline: b.cloned(),
			candidate: c.cloned(),
			delta: 0.0,
			delta_pct: 0.0,
			threshold: 0.0,
			verdict: Verdict::Missing,
			headline: spec.headline,
		};
	};
	let delta = c.median - b.median;
	let delta_pct = if b.median == 0.0 {
		0.0
	} else {
		delta / b.median.abs() * 100.0
	};
	let sigma = MAD_TO_SIGMA * (b.mad.powi(2) + c.mad.powi(2)).sqrt();
	let rel = spec.rel_threshold.unwrap_or(opts.rel_threshold);
	let threshold = (rel * b.median.abs()).max(opts.k * sigma).max(spec.floor);
	let verdict = if delta.abs() <= threshold {
		Verdict::Unchanged
	} else {
		match (spec.direction, delta > 0.0) {
			(Direction::Neutral, _) => Verdict::Changed,
			(Direction::LowerIsBetter, true) | (Direction::HigherIsBetter, false) => Verdict::Regressed,
			_ => Verdict::Improved,
		}
	};
	MetricComparison {
		metric: spec.name.clone(),
		unit: spec.unit.to_string(),
		baseline: Some(b.clone()),
		candidate: Some(c.clone()),
		delta,
		delta_pct,
		threshold,
		verdict,
		headline: spec.headline,
	}
}

/// Output format for [`Comparison::render`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
	Text,
	Markdown,
	Json,
}

impl std::str::FromStr for Format {
	type Err = String;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		match s {
			"text" => Ok(Self::Text),
			"markdown" | "md" => Ok(Self::Markdown),
			"json" => Ok(Self::Json),
			_ => Err(format!("expected text, markdown or json, got `{s}`")),
		}
	}
}

impl Comparison {
	/// Render the comparison. Rows that are unchanged and not headline metrics are hidden
	/// unless `all` is set.
	pub fn render(&self, format: Format, all: bool) -> String {
		match format {
			Format::Json => serde_json::to_string_pretty(self).expect("comparison serializes") + "\n",
			Format::Text => self.render_table(false, all),
			Format::Markdown => self.render_table(true, all),
		}
	}

	fn render_table(&self, md: bool, all: bool) -> String {
		let mut out = String::new();
		let verdict = match self.outcome {
			Outcome::NoRegression => "no regression",
			Outcome::Regression => "REGRESSION",
			Outcome::Invalid => "INVALID comparison",
		};
		if md {
			let _ = writeln!(out, "### moonshine-otel compare: {verdict}\n");
		} else {
			let _ = writeln!(out, "Result: {verdict}\n");
		}
		for reason in &self.invalid {
			let _ = writeln!(out, "{}invalid: {reason}", if md { "- " } else { "  " });
		}
		for w in &self.warnings {
			let _ = writeln!(out, "{}warning: {w}", if md { "- " } else { "  " });
		}
		if !self.invalid.is_empty() || !self.warnings.is_empty() {
			out.push('\n');
		}
		for run in &self.runs {
			let conf = if run.low_confidence { ", low confidence" } else { "" };
			if md {
				let _ = writeln!(
					out,
					"#### {}\n\nbaseline `{}` ({} it) vs candidate `{}` ({} it){conf}\n",
					run.key,
					run.baseline_run_id,
					run.baseline_iterations,
					run.candidate_run_id,
					run.candidate_iterations
				);
				let _ = writeln!(out, "| metric | baseline | candidate | Δ | Δ% | threshold | verdict |");
				let _ = writeln!(out, "|---|---:|---:|---:|---:|---:|---|");
			} else {
				let _ = writeln!(
					out,
					"{}  (baseline {} ×{}, candidate {} ×{}{conf})",
					run.key,
					run.baseline_run_id,
					run.baseline_iterations,
					run.candidate_run_id,
					run.candidate_iterations
				);
				let _ = writeln!(
					out,
					"  {:<38} {:>14} {:>14} {:>10} {:>8} {:>9}  verdict",
					"metric", "baseline", "candidate", "Δ", "Δ%", "threshold"
				);
			}
			let mut hidden = 0;
			for m in &run.metrics {
				if !all && !m.headline && m.verdict == Verdict::Unchanged {
					hidden += 1;
					continue;
				}
				metric_row(&mut out, m, md);
			}
			if hidden > 0 {
				let note = format!("{hidden} unchanged per-stage rows hidden (use --all)");
				let _ = if md {
					writeln!(out, "\n_{note}_")
				} else {
					writeln!(out, "  {note}")
				};
			}
			out.push('\n');
		}
		let footer = format!(
			"flagged when |Δ| > max({:.0}% of baseline, {}×1.4826×√(MAD_b²+MAD_c²), floor)",
			self.rel_threshold * 100.0,
			self.k
		);
		let _ = if md {
			writeln!(out, "_{footer}_")
		} else {
			writeln!(out, "{footer}")
		};
		out
	}
}

/// One metric's row: both sides (median ± MAD), the change, the threshold and the verdict.
fn metric_row(out: &mut String, m: &MetricComparison, md: bool) {
	let fmt_side = |s: &Option<Spread>| match s {
		Some(s) if s.n > 1 => format!("{} ±{}", num(s.median), num(s.mad)),
		Some(s) => num(s.median),
		None => "—".into(),
	};
	let name = format!("{} ({})", m.metric, m.unit);
	if md {
		let v = match m.verdict {
			Verdict::Regressed => "**regressed**".to_string(),
			v => v.label().to_string(),
		};
		let _ = writeln!(
			out,
			"| {name} | {} | {} | {} | {:+.1}% | {} | {v} |",
			fmt_side(&m.baseline),
			fmt_side(&m.candidate),
			signed(m.delta),
			m.delta_pct,
			num(m.threshold)
		);
	} else {
		let _ = writeln!(
			out,
			"  {:<38} {:>14} {:>14} {:>10} {:>7.1}% {:>9}  {}",
			name,
			fmt_side(&m.baseline),
			fmt_side(&m.candidate),
			signed(m.delta),
			m.delta_pct,
			num(m.threshold),
			m.verdict.label()
		);
	}
}

fn signed(v: f64) -> String {
	if v > 0.0 { format!("+{}", num(v)) } else { num(v) }
}

/// Short human number: integers above 100, two decimals below.
fn num(v: f64) -> String {
	if v.abs() >= 100.0 {
		format!("{v:.0}")
	} else {
		format!("{v:.2}")
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn spread(median: f64, mad: f64, n: u32) -> Spread {
		Spread {
			median,
			mad,
			min: median - mad,
			max: median + mad,
			n,
		}
	}

	fn spec(direction: Direction) -> MetricSpec {
		MetricSpec {
			name: "m".into(),
			direction,
			floor: 1.0,
			unit: "us",
			headline: true,
			rel_threshold: None,
		}
	}

	#[test]
	fn relative_threshold() {
		let o = CompareOptions::default();
		let r = compare_metric(
			&spec(Direction::LowerIsBetter),
			Some(&spread(1000.0, 0.0, 5)),
			Some(&spread(1049.0, 0.0, 5)),
			&o,
		);
		assert_eq!(r.verdict, Verdict::Unchanged);
		assert_eq!(r.threshold, 50.0);
		let r = compare_metric(
			&spec(Direction::LowerIsBetter),
			Some(&spread(1000.0, 0.0, 5)),
			Some(&spread(1051.0, 0.0, 5)),
			&o,
		);
		assert_eq!(r.verdict, Verdict::Regressed);
		let r = compare_metric(
			&spec(Direction::LowerIsBetter),
			Some(&spread(1000.0, 0.0, 5)),
			Some(&spread(900.0, 0.0, 5)),
			&o,
		);
		assert_eq!(r.verdict, Verdict::Improved);
	}

	#[test]
	fn noise_widens_threshold() {
		let o = CompareOptions::default();
		// MADs of 30 → σ̂ = 1.4826 × 42.4 ≈ 62.9 → 3σ̂ ≈ 188.7
		let r = compare_metric(
			&spec(Direction::LowerIsBetter),
			Some(&spread(1000.0, 30.0, 5)),
			Some(&spread(1150.0, 30.0, 5)),
			&o,
		);
		assert_eq!(r.verdict, Verdict::Unchanged);
		assert!((r.threshold - 188.69).abs() < 0.1, "{}", r.threshold);
	}

	#[test]
	fn floor_suppresses_tiny_changes() {
		let o = CompareOptions::default();
		let mut s = spec(Direction::LowerIsBetter);
		s.floor = 20.0;
		let r = compare_metric(&s, Some(&spread(10.0, 0.0, 5)), Some(&spread(25.0, 0.0, 5)), &o);
		assert_eq!(r.verdict, Verdict::Unchanged);
		assert!((r.delta_pct - 150.0).abs() < 1e-9);
	}

	#[test]
	fn directions() {
		let o = CompareOptions::default();
		let b = spread(60.0, 0.0, 5);
		let c = spread(50.0, 0.0, 5);
		assert_eq!(
			compare_metric(&spec(Direction::HigherIsBetter), Some(&b), Some(&c), &o).verdict,
			Verdict::Regressed
		);
		assert_eq!(
			compare_metric(&spec(Direction::Neutral), Some(&b), Some(&c), &o).verdict,
			Verdict::Changed
		);
		assert_eq!(
			compare_metric(&spec(Direction::Neutral), None, Some(&c), &o).verdict,
			Verdict::Missing
		);
	}

	#[test]
	fn fps_uses_tight_threshold() {
		let fps = default_metrics()
			.into_iter()
			.find(|m| m.name == "effective_fps")
			.unwrap();
		let o = CompareOptions::default();
		// 60 → 58.5 fps is 2.5 %: under the global 5 % but over fps's 1 %.
		let r = compare_metric(&fps, Some(&spread(60.0, 0.0, 5)), Some(&spread(58.5, 0.0, 5)), &o);
		assert_eq!(r.verdict, Verdict::Regressed);
		assert!((r.threshold - 0.6).abs() < 1e-9);
	}

	#[test]
	fn undersampled_tails_dont_decide() {
		use crate::identity::{RunConfig, RunIdentity};
		use crate::results::{IterationResult, RunResult};
		use crate::summary::Summary;
		let run = |p999: f64, frames: u64| {
			let mut summary = Summary::default();
			summary.frames.delta = frames;
			let mut it = IterationResult::new(0, String::new(), summary);
			it.valid = true;
			let mut r = RunResult::new(
				"r".into(),
				String::new(),
				RunIdentity::default(),
				RunConfig::default(),
				vec![it.clone(), it],
				Vec::new(),
			);
			r.aggregate
				.insert("latency.captured.total.p999".into(), spread(p999, 0.0, 2));
			r.aggregate
				.insert("frames.captured".into(), spread(frames as f64, 0.0, 2));
			r
		};
		let opts = CompareOptions::default();
		let file = |r| ResultsFile::new(vec![r]);
		// 600 frames: p99.9 is the slowest frame, so a 10 % change is reported, not judged.
		let c = compare(&file(run(2000.0, 600)), &file(run(2200.0, 600)), &opts);
		assert_eq!(c.outcome, Outcome::NoRegression, "{:?}", c.warnings);
		let m = c.runs[0]
			.metrics
			.iter()
			.find(|m| m.metric == "latency.captured.total.p999")
			.unwrap();
		assert_eq!(m.verdict, Verdict::Changed);
		assert!(
			c.warnings.iter().any(|w| w.contains("p99.9 reported but not judged")),
			"{:?}",
			c.warnings
		);
		// 20 000 frames: 20 beyond p99.9, so it counts.
		let c = compare(&file(run(2000.0, 20_000)), &file(run(2200.0, 20_000)), &opts);
		assert_eq!(c.outcome, Outcome::Regression);
	}

	#[test]
	fn exit_codes() {
		assert_eq!(Outcome::NoRegression.exit_code(), 0);
		assert_eq!(Outcome::Regression.exit_code(), 1);
		assert_eq!(Outcome::Invalid.exit_code(), 2);
	}
}
