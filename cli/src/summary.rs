//! `summary`: a test run's bundle (its OTLP data: moonshine-test-headless's `results.tar.gz`, or the `logs/otlp`
//! spool in it) as Markdown for a pull request. The bundle is loaded into a throwaway local Prometheus, and the
//! harness's queries (`--queries`, which its board's generator writes beside the board, so the summary and the board
//! can't disagree) are run against it and scored against its targets (`--targets`). With `--baseline`, a second bundle is set beside it;
//! without, a run's own baseline streams (side=base) are, when it has them.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::Args;
use serde::Deserialize;

/// How far back a bundle's samples can be: Prometheus takes them out of order within this window (a much larger one
/// makes ingesting slow), and queries look back as far.
const LOOKBACK: &str = "365d";

#[derive(Args, Debug)]
pub struct SummaryArgs {
	/// The run's bundle: results.tar.gz, a results folder, or its logs/otlp spool.
	bundle: PathBuf,
	/// Another run's bundle to set beside it (another build, or another machine).
	#[arg(long)]
	baseline: Option<PathBuf>,
	/// The run to summarize, when the bundle has several (a run per mode); the latest by default.
	#[arg(long)]
	run: Option<String>,
	/// The report card's queries: {"metrics": [{"key", "expr" (PromQL, {sel} for the run's selector), "empty" (shown
	/// without a value), "decimals"}]}, in the card's order.
	#[arg(long)]
	queries: PathBuf,
	/// The targets they're scored against: {"metrics": {KEY: {"label", "unit", "better" ("higher" or "lower"), "target",
	/// "acceptable"}}}.
	#[arg(long)]
	targets: PathBuf,
	/// Markdown to add after the report card, e.g. the harness's checks.
	#[arg(long)]
	notes: Option<PathBuf>,
	/// Write the Markdown here instead of stdout.
	#[arg(long)]
	out: Option<PathBuf>,
	/// The Prometheus binary to load the bundles into.
	#[arg(long, default_value = "prometheus")]
	prometheus: PathBuf,
}

#[derive(Deserialize)]
struct Queries {
	/// In the board's order.
	metrics: Vec<Query>,
}

#[derive(Deserialize)]
struct Query {
	key: String,
	expr: String,
	empty: String,
	decimals: usize,
}

#[derive(Deserialize)]
struct Targets {
	metrics: serde_json::Map<String, serde_json::Value>,
}

/// A throwaway Prometheus, stopped and its data removed when dropped.
struct Prom {
	child: Child,
	dir: PathBuf,
	base: String,
	http: reqwest::blocking::Client,
}

impl Drop for Prom {
	fn drop(&mut self) {
		let _ = self.child.kill();
		let _ = self.child.wait();
		let _ = std::fs::remove_dir_all(&self.dir);
	}
}

type Sample = (BTreeMap<String, String>, f64);

impl Prom {
	fn start(binary: &Path, dir: PathBuf) -> anyhow::Result<Self> {
		std::fs::create_dir_all(&dir)?;
		let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
		let config = dir.join("prometheus.yaml");
		std::fs::write(
			&config,
			format!("storage:\n  tsdb:\n    out_of_order_time_window: {LOOKBACK}\n"),
		)?;
		let child = Command::new(binary)
			.arg(format!("--config.file={}", config.display()))
			.arg(format!("--storage.tsdb.path={}", dir.join("data").display()))
			.arg(format!("--web.listen-address=127.0.0.1:{port}"))
			.arg("--web.enable-otlp-receiver")
			.stdout(Stdio::null())
			.stderr(Stdio::null())
			.spawn()
			.with_context(|| {
				format!(
					"starting {} (install Prometheus or pass --prometheus)",
					binary.display()
				)
			})?;
		let prom = Prom {
			child,
			dir,
			base: format!("http://127.0.0.1:{port}"),
			http: reqwest::blocking::Client::new(),
		};
		let deadline = Instant::now() + Duration::from_secs(30);
		while prom
			.http
			.get(format!("{}/-/ready", prom.base))
			.send()
			.map_or(true, |r| !r.status().is_success())
		{
			if Instant::now() > deadline {
				bail!("Prometheus didn't come up");
			}
			std::thread::sleep(Duration::from_millis(200));
		}
		Ok(prom)
	}

	/// Every metrics batch of a spool, as written: each file is one OTLP request.
	fn ingest(&self, spool: &Path) -> anyhow::Result<usize> {
		let mut files = 0;
		for entry in std::fs::read_dir(spool).with_context(|| format!("reading {}", spool.display()))? {
			let path = entry?.path();
			if !path
				.file_name()
				.and_then(|n| n.to_str())
				.is_some_and(|n| n.ends_with(".metrics.pb"))
			{
				continue;
			}
			let body = std::fs::read(&path)?;
			let response = self
				.http
				.post(format!("{}/api/v1/otlp/v1/metrics", self.base))
				.header("Content-Type", "application/x-protobuf")
				.body(body)
				.send()?;
			if !response.status().is_success() {
				bail!(
					"Prometheus refused {}: {}",
					path.display(),
					response.text().unwrap_or_default()
				);
			}
			files += 1;
		}
		Ok(files)
	}

	fn query(&self, expr: &str) -> anyhow::Result<Vec<Sample>> {
		#[derive(Deserialize)]
		struct Response {
			data: Data,
		}
		#[derive(Deserialize)]
		struct Data {
			result: Vec<Series>,
		}
		#[derive(Deserialize)]
		struct Series {
			metric: BTreeMap<String, String>,
			value: (f64, String),
		}
		let response = self
			.http
			.post(format!("{}/api/v1/query", self.base))
			.header("Content-Type", "application/x-www-form-urlencoded")
			.body(format!("query={}", encoded(expr)))
			.send()?;
		if !response.status().is_success() {
			bail!("query failed ({}): {expr}", response.text().unwrap_or_default());
		}
		let r: Response = response.json()?;
		Ok(r.data
			.result
			.into_iter()
			.filter_map(|s| {
				s.value
					.1
					.parse::<f64>()
					.ok()
					.filter(|v| v.is_finite())
					.map(|v| (s.metric, v))
			})
			.collect())
	}

	fn runs(&self) -> anyhow::Result<Vec<String>> {
		let mut runs: Vec<String> = self
			.query(&format!(
				r#"group by (run) (last_over_time(moonshine_client_frames_total{{run!=""}}[{LOOKBACK}]))"#
			))?
			.into_iter()
			.filter_map(|(m, _)| m.get("run").cloned())
			.collect();
		runs.sort();
		Ok(runs)
	}
}

/// A bundle's spool: logs/otlp under a results folder, a spool folder itself, or results.tar.gz unpacked into `work`.
fn spool(bundle: &Path, work: &Path) -> anyhow::Result<PathBuf> {
	let dir = if bundle.is_dir() {
		bundle.to_path_buf()
	} else {
		std::fs::create_dir_all(work)?;
		let status = Command::new("tar")
			.arg("-xzf")
			.arg(bundle)
			.arg("-C")
			.arg(work)
			.status()
			.context("running tar")?;
		if !status.success() {
			bail!("couldn't unpack {}", bundle.display());
		}
		work.to_path_buf()
	};
	let nested = dir.join("logs/otlp");
	Ok(if nested.is_dir() { nested } else { dir })
}

/// One side of the summary: a run, and the selector that picks its streams.
struct Side {
	run: String,
	sel: String,
	label: &'static str,
}

pub fn summary(args: &SummaryArgs) -> anyhow::Result<i32> {
	let queries: Queries = read_json(&args.queries)?;
	let targets: Targets = read_json(&args.targets)?;
	if let Some(q) = queries.metrics.iter().find(|q| !targets.metrics.contains_key(&q.key)) {
		bail!("{} has no target for {}", args.targets.display(), q.key);
	}
	let work = std::env::temp_dir().join(format!("moonshine-otel-summary-{}", std::process::id()));
	let prom = Prom::start(&args.prometheus, work.join("prometheus"))?;

	prom.ingest(&spool(&args.bundle, &work.join("a"))?)?;
	let a_runs = prom.runs()?;
	let a_run = match &args.run {
		Some(run) if a_runs.contains(run) => run.clone(),
		Some(run) => bail!("{} has no run {run:?} (it has {a_runs:?})", args.bundle.display()),
		None => a_runs.last().cloned().with_context(|| {
			format!(
				"{} has no test run metrics (was it run with telemetry?)",
				args.bundle.display()
			)
		})?,
	};
	let mut sides = Vec::new();
	if let Some(baseline) = &args.baseline {
		prom.ingest(&spool(baseline, &work.join("b"))?)?;
		let b_run = prom
			.runs()?
			.into_iter()
			.find(|r| !a_runs.contains(r))
			.context("the baseline bundle has no run of its own (is it the same run?)")?;
		sides.push(Side {
			sel: format!(r#"run="{a_run}""#),
			run: a_run.clone(),
			label: "A",
		});
		sides.push(Side {
			sel: format!(r#"run="{b_run}""#),
			run: b_run,
			label: "B",
		});
	} else {
		let has_base = !prom
			.query(&format!(
				r#"last_over_time(moonshine_client_frames_total{{run="{a_run}", side="base"}}[{LOOKBACK}])"#
			))?
			.is_empty();
		if has_base {
			sides.push(Side {
				sel: format!(r#"run="{a_run}", side="test""#),
				run: a_run.clone(),
				label: "test",
			});
			sides.push(Side {
				sel: format!(r#"run="{a_run}", side="base""#),
				run: a_run.clone(),
				label: "baseline",
			});
		} else {
			sides.push(Side {
				sel: format!(r#"run="{a_run}""#),
				run: a_run.clone(),
				label: "",
			});
		}
	}

	let notes = match &args.notes {
		Some(path) => std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
		None => String::new(),
	};
	let md = markdown(&prom, &queries, &targets, &sides, &notes)?;
	match &args.out {
		Some(path) => std::fs::write(path, &md).with_context(|| format!("writing {}", path.display()))?,
		None => std::io::stdout().write_all(md.as_bytes())?,
	}
	Ok(0)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
	let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
	serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))
}

/// A run's moonshine.test.info sample for a role, as its labels.
fn facts(prom: &Prom, run: &str, role: &str) -> anyhow::Result<BTreeMap<String, String>> {
	Ok(prom
		.query(&format!(
			r#"last_over_time(moonshine_test_info{{run="{run}", role="{role}"}}[{LOOKBACK}])"#
		))?
		.into_iter()
		.next()
		.map(|(m, _)| m)
		.unwrap_or_default())
}

fn first_value(prom: &Prom, expr: &str) -> anyhow::Result<Option<f64>> {
	Ok(prom.query(expr)?.into_iter().next().map(|(_, v)| v))
}

fn markdown(prom: &Prom, queries: &Queries, targets: &Targets, sides: &[Side], notes: &str) -> anyhow::Result<String> {
	let mut md = String::new();
	let a = &sides[0];
	let a_run_facts = facts(prom, &a.run, "run")?;
	let build = |f: &BTreeMap<String, String>, run: &str| f.get("code").cloned().unwrap_or_else(|| run.to_string());
	let title = match sides.get(1) {
		Some(b) if b.run != a.run => format!(
			"{} vs {}",
			build(&a_run_facts, &a.run),
			build(&facts(prom, &b.run, "run")?, &b.run)
		),
		Some(_) => format!(
			"{} vs its baseline {}",
			build(&a_run_facts, &a.run),
			a_run_facts.get("baseline").cloned().unwrap_or_default()
		),
		None => build(&a_run_facts, &a.run),
	};
	md.push_str(&format!("### Moonshine test: {title}\n\n"));

	// Hardware first: who has which GPU is what a reviewer asks first.
	let runs: Vec<&Side> = {
		let mut v: Vec<&Side> = vec![a];
		if let Some(b) = sides.get(1).filter(|b| b.run != a.run) {
			v.push(b);
		}
		v
	};
	let mut columns = Vec::new();
	for side in &runs {
		for role in ["host", "client"] {
			let name = if runs.len() > 1 {
				format!("{} ({})", if role == "host" { "Host" } else { "Client" }, side.label)
			} else if role == "host" {
				"Host".to_string()
			} else {
				"Client".to_string()
			};
			columns.push((name, facts(prom, &side.run, role)?));
		}
	}
	md.push_str("**Hardware**\n\n|  |");
	for (name, _) in &columns {
		md.push_str(&format!(" {name} |"));
	}
	md.push_str("\n|---|");
	md.push_str(&"---|".repeat(columns.len()));
	md.push('\n');
	for (key, name) in [
		("gpu", "GPU"),
		("driver", "Driver"),
		("other_gpus", "Other GPUs"),
		("cpu", "CPU"),
		("memory", "Memory"),
		("os", "OS"),
		("kernel", "Kernel"),
		("network", "Network"),
		("machine", "Machine"),
	] {
		md.push_str(&format!("| {name} |"));
		for (_, f) in &columns {
			md.push_str(&format!(" {} |", cell(f.get(key).map_or("N/A", String::as_str))));
		}
		md.push('\n');
	}

	// The stream, per run.
	md.push_str("\n**Stream**\n\n");
	for side in &runs {
		let f = facts(prom, &side.run, "run")?;
		let mut workloads: Vec<String> = prom
			.query(&format!(
				r#"count by (workload) (count by (workload, stream_key) (last_over_time(moonshine_client_frames_total{{{}}}[{LOOKBACK}])))"#,
				side.sel
			))?
			.into_iter()
			.filter_map(|(m, n)| m.get("workload").map(|w| format!("{w} ({n:.0})")))
			.collect();
		workloads.sort();
		let frames = first_value(
			prom,
			&format!(
				r#"sum(last_over_time(moonshine_client_frames_total{{{}}}[{LOOKBACK}]))"#,
				side.sel
			),
		)?;
		let clock = first_value(
			prom,
			&format!(
				r#"1000 * max(last_over_time(moonshine_client_clock_uncertainty_seconds{{{}}}[{LOOKBACK}]))"#,
				side.sel
			),
		)?;
		let get = |k: &str| f.get(k).cloned().unwrap_or_else(|| "N/A".to_string());
		let prefix = if runs.len() > 1 {
			format!("{}: ", side.label)
		} else {
			String::new()
		};
		md.push_str(&format!(
			"- {prefix}{} ({}, commit {}), {} at {} fps, {}, streams of {} each, route {}\n",
			get("code"),
			get("ref"),
			get("commit"),
			get("resolution"),
			get("fps"),
			get("bitrate"),
			get("seconds"),
			get("route"),
		));
		md.push_str(&format!(
			"- {prefix}workloads (streams): {}; {} frames decoded; clock sync {}\n",
			if workloads.is_empty() {
				"none".to_string()
			} else {
				workloads.join(", ")
			},
			frames.map_or("N/A".to_string(), |f| format!("{f:.0}")),
			clock.map_or("N/A".to_string(), |c| format!("±{c:.2} ms")),
		));
	}

	// The report card: the board's tiles, for each side.
	let fps = prom
		.query(&format!(
			r#"group by (target_fps) (last_over_time(moonshine_frames_total[{LOOKBACK}]) and on (stream_key) group by (stream_key) (last_over_time(moonshine_client_frames_total{{{}}}[{LOOKBACK}])))"#,
			a.sel
		))?
		.into_iter()
		.find_map(|(m, _)| m.get("target_fps").cloned())
		.unwrap_or_else(|| "60".to_string());
	let interval_ms = 1000.0 / fps.parse::<f64>().unwrap_or(60.0);
	md.push_str("\n**Report card**\n\n| Metric |");
	for side in sides {
		md.push_str(&format!(
			" {} |",
			if side.label.is_empty() { "Value" } else { side.label }
		));
	}
	md.push_str(" Target |\n|---|");
	md.push_str(&"---|".repeat(sides.len() + 1));
	md.push('\n');
	for q in &queries.metrics {
		let Some(target) = targets.metrics.get(&q.key) else {
			continue;
		};
		let label = target["label"].as_str().unwrap_or(&q.key);
		let unit = target["unit"].as_str().unwrap_or("");
		let higher = target["better"].as_str() == Some("higher");
		let (good, ok) = (
			target["target"].as_f64().unwrap_or(0.0),
			target["acceptable"].as_f64().unwrap_or(0.0),
		);
		let mut row = format!("| {} |", capitalized(label));
		for side in sides {
			let expr = q
				.expr
				.replace("{sel}", &side.sel)
				.replace("$__range", LOOKBACK)
				.replace("$fps", &fps);
			let value = first_value(prom, &expr)?;
			row.push_str(&match value {
				Some(v) => {
					let status = if higher {
						if v >= good {
							"🟢"
						} else if v >= ok {
							"🟡"
						} else {
							"🔴"
						}
					} else if v <= good + 1e-9 {
						"🟢"
					} else if v <= ok + 1e-9 {
						"🟡"
					} else {
						"🔴"
					};
					format!(" {status} {} |", show(v, unit, q.decimals, interval_ms))
				},
				None => format!(" {} |", q.empty),
			});
		}
		let op = if higher { "≥" } else { "≤" };
		row.push_str(&format!(" {op} {} |\n", show(good, unit, q.decimals, interval_ms)));
		md.push_str(&row);
	}
	if !notes.trim().is_empty() {
		md.push('\n');
		md.push_str(notes.trim_end());
		md.push('\n');
	}
	md.push_str(
		"\n<sub>🟢 at or past the target, 🟡 up to the acceptable value, 🔴 past it (thresholds in targets.json). \
		 Made by moonshine-otel summary from the run's telemetry, which is in its results.tar.gz.</sub>\n",
	);
	Ok(md)
}

/// A value in its unit; frame intervals also in milliseconds at the run's frame rate.
fn show(v: f64, unit: &str, decimals: usize, interval_ms: f64) -> String {
	match unit {
		"frame intervals" => format!("{v:.decimals$} frames ({:.1} ms)", v * interval_ms),
		"ms" => format!("{v:.decimals$} ms"),
		"%" | "% of requested" => format!("{v:.decimals$}%"),
		_ => format!("{v:.decimals$}"),
	}
}

fn capitalized(s: &str) -> String {
	let mut c = s.chars();
	c.next()
		.map(|f| f.to_uppercase().chain(c).collect())
		.unwrap_or_default()
}

/// Percent-encoding for a form value: everything but the unreserved characters.
fn encoded(s: &str) -> String {
	s.bytes()
		.map(|b| match b {
			b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
			_ => format!("%{b:02X}"),
		})
		.collect()
}

/// Markdown table cells can't hold a pipe.
fn cell(s: &str) -> String {
	s.replace('|', "\\|")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn values_read_in_their_units() {
		assert_eq!(show(1.02, "frame intervals", 2, 1000.0 / 60.0), "1.02 frames (17.0 ms)");
		assert_eq!(show(3.384, "ms", 2, 16.7), "3.38 ms");
		assert_eq!(show(104.39, "% of requested", 1, 16.7), "104.4%");
		assert_eq!(show(0.0, "", 0, 16.7), "0");
		assert_eq!(encoded(r#"a{run="x · y"}"#), "a%7Brun%3D%22x%20%C2%B7%20y%22%7D");
	}
}
