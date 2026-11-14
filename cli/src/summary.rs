//! `summary`: a test run's bundle (its OTLP data: moonshine-test-headless's `results.tar.gz`, or the `logs/otlp`
//! spool in it) as Markdown for a pull request. The bundle is loaded into a throwaway local Prometheus, and the
//! harness's queries (`--queries`, which its board's generator writes beside the board, so the summary and the board
//! can't disagree) are run against it and scored against its targets (`--targets`). With `--baseline`, a second bundle is set beside it;
//! without, a run's own baseline streams (side=base) are, when it has them. `--side` sets any of a run's sides beside
//! each other instead (a calibration's repeats), `--noise-out` writes how far each metric varied across them, and
//! `--noise` uses such a file to say whether test and baseline differ by more than that.

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
	/// With --baseline: its run to use, when it has several (else the one the run's own bundle doesn't have).
	#[arg(long, requires = "baseline")]
	baseline_run: Option<String>,
	/// With --baseline: only that run's streams of this side, e.g. `base` for a retained baseline (the baseline streams
	/// of an earlier run), set beside this run as its baseline.
	#[arg(long, requires = "baseline")]
	baseline_side: Option<String>,
	/// These sides of the run beside each other, instead of test and baseline, e.g. a calibration's repeats: --side
	/// base --side base-2. Repeatable.
	#[arg(long = "side", conflicts_with = "baseline")]
	sides: Vec<String>,
	/// How much each metric varies from run to run (a `--noise-out` file): with two sides, a Change column says whether
	/// they differ by more than that.
	#[arg(long)]
	noise: Option<PathBuf>,
	/// Write how much each metric varied across the sides (two or more) to this file, as JSON.
	#[arg(long)]
	noise_out: Option<PathBuf>,
	/// The run to summarize, when the bundle has several (a run per mode); the latest by default.
	#[arg(long)]
	run: Option<String>,
	/// The report card's queries: {"metrics": [{"key", "expr" (PromQL, {sel} for the run's selector), "empty" (shown
	/// without a value), "decimals"}]}, in the card's order.
	#[arg(long)]
	queries: PathBuf,
	/// The targets they're scored against: {"metrics": {KEY: {"label", "unit", "better" ("higher" or "lower"), "target",
	/// "acceptable"}}}; a metric whose target is null is shown without a verdict.
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

	/// The runs, oldest first (by when their first stream started; by name for runs without that).
	fn runs(&self) -> anyhow::Result<Vec<String>> {
		let started: BTreeMap<String, f64> = self
			.query(&format!(
				r#"min by (run) (last_over_time(moonshine_client_stream_start_seconds{{run!=""}}[{LOOKBACK}]))"#
			))?
			.into_iter()
			.filter_map(|(m, v)| Some((m.get("run")?.clone(), v)))
			.collect();
		let mut runs: Vec<String> = self
			.query(&format!(
				r#"group by (run) (last_over_time(moonshine_client_frames_total{{run!=""}}[{LOOKBACK}]))"#
			))?
			.into_iter()
			.filter_map(|(m, _)| m.get("run").cloned())
			.collect();
		runs.sort_by(|a, b| {
			let (x, y) = (started.get(a).copied(), started.get(b).copied());
			x.partial_cmp(&y)
				.unwrap_or(std::cmp::Ordering::Equal)
				.then_with(|| a.cmp(b))
		});
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
	label: String,
	/// The side is its run's baseline streams: its code is the run's `baseline` fact, not its `code`.
	base_of: bool,
}

/// How much each report card metric varied across repeated runs of the same build (`--noise-out`, read by `--noise`).
#[derive(Debug, serde::Serialize, Deserialize, PartialEq)]
struct Noise {
	/// The sides it was measured across.
	sides: Vec<String>,
	metrics: BTreeMap<String, NoiseMetric>,
}

#[derive(Debug, serde::Serialize, Deserialize, PartialEq)]
struct NoiseMetric {
	/// Each side's value, where it had one.
	values: Vec<f64>,
	mean: f64,
	/// The largest less the smallest: a difference within it is one repeat runs show anyway.
	spread: f64,
}

impl Noise {
	/// From each metric's value per side (in `sides` order); a metric needs two values to have a spread.
	fn measure(sides: Vec<String>, values: &[(String, Vec<Option<f64>>)]) -> Self {
		let metrics = values
			.iter()
			.filter_map(|(key, vs)| {
				let vs: Vec<f64> = vs.iter().flatten().copied().collect();
				if vs.len() < 2 {
					return None;
				}
				let (lo, hi) = vs
					.iter()
					.fold((f64::MAX, f64::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
				let mean = vs.iter().sum::<f64>() / vs.len() as f64;
				Some((
					key.clone(),
					NoiseMetric {
						values: vs,
						mean,
						spread: hi - lo,
					},
				))
			})
			.collect();
		Noise { sides, metrics }
	}
}

/// How far apart test and baseline can be and still be noise: the spread the repeats showed, by half again (a few
/// repeats see less than the full range), but at least 2% of the baseline and half the last digit shown, so a
/// metric that happened not to vary doesn't call every rounding a change.
fn tolerance(spread: f64, baseline: f64, decimals: usize) -> f64 {
	(spread * 1.5)
		.max(baseline.abs() * 0.02)
		.max(0.5 * 10f64.powi(-(decimals as i32)))
}

/// Whether `a` differs from `b` by more than `tolerance`: "within noise", "better" or "worse".
fn change(a: f64, b: f64, tolerance: f64, higher_better: bool) -> &'static str {
	let d = a - b;
	if d.abs() <= tolerance + 1e-9 {
		"within noise"
	} else if (d > 0.0) == higher_better {
		"better"
	} else {
		"worse"
	}
}

pub fn summary(args: &SummaryArgs) -> anyhow::Result<i32> {
	let queries: Queries = read_json(&args.queries)?;
	let targets: Targets = read_json(&args.targets)?;
	if let Some(q) = queries.metrics.iter().find(|q| !targets.metrics.contains_key(&q.key)) {
		bail!("{} has no target for {}", args.targets.display(), q.key);
	}
	let work = std::env::temp_dir().join(format!("moonshine-otel-summary-{}", std::process::id()));
	let prom = Prom::start(&args.prometheus, work.join("prometheus"))?;

	let a_spool = spool(&args.bundle, &work.join("a"))?;
	prom.ingest(&a_spool)?;
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
	let side = |run: &str, side: Option<&str>, label: &str| Side {
		sel: match side {
			Some(sd) => format!(r#"run="{run}", side="{sd}""#),
			None => format!(r#"run="{run}""#),
		},
		run: run.to_string(),
		label: label.to_string(),
		base_of: side.is_some_and(|sd| sd == "base" || sd.starts_with("base-")),
	};
	let mut sides = Vec::new();
	if !args.sides.is_empty() {
		for sd in &args.sides {
			sides.push(side(&a_run, Some(sd), sd));
		}
	} else if let Some(baseline) = &args.baseline {
		prom.ingest(&spool(baseline, &work.join("b"))?)?;
		let runs = prom.runs()?;
		let b_run = match &args.baseline_run {
			Some(run) if runs.contains(run) && !a_runs.contains(run) => run.clone(),
			Some(run) => bail!("{} has no run {run:?} of its own", baseline.display()),
			None => runs
				.into_iter()
				.find(|r| !a_runs.contains(r))
				.context("the baseline bundle has no run of its own (is it the same run?)")?,
		};
		match args.baseline_side.as_deref() {
			// A retained baseline: an earlier run's baseline streams, as this run's baseline.
			Some(sd) => {
				sides.push(side(&a_run, None, "test"));
				sides.push(side(&b_run, Some(sd), "baseline"));
			},
			None => {
				sides.push(side(&a_run, None, "A"));
				sides.push(side(&b_run, None, "B"));
			},
		}
	} else if let Some(b_run) = facts(&prom, &a_run, "run")?
		.get("baseline_run")
		.filter(|r| a_runs.contains(*r))
	{
		// The run reused a retained baseline, and its bundle carries it.
		sides.push(side(&a_run, None, "test"));
		sides.push(side(b_run, Some("base"), "baseline"));
	} else {
		let has_base = !prom
			.query(&format!(
				r#"last_over_time(moonshine_client_frames_total{{run="{a_run}", side="base"}}[{LOOKBACK}])"#
			))?
			.is_empty();
		if has_base {
			sides.push(side(&a_run, Some("test"), "test"));
			sides.push(side(&a_run, Some("base"), "baseline"));
		} else {
			sides.push(side(&a_run, None, ""));
		}
	}
	// --noise, or the noise.json a results folder carries beside its logs/otlp (a run that reused a calibrated baseline).
	let carried = a_spool
		.ends_with("logs/otlp")
		.then(|| a_spool.join("../../noise.json"))
		.filter(|p| p.is_file());
	let noise: Option<Noise> = args.noise.clone().or(carried).as_deref().map(read_json).transpose()?;

	let notes = match &args.notes {
		Some(path) => std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
		None => String::new(),
	};
	let (md, values) = markdown(&prom, &queries, &targets, &sides, &notes, noise.as_ref())?;
	if let Some(path) = &args.noise_out {
		if sides.len() < 2 {
			bail!("--noise-out needs two sides or more to measure across");
		}
		let noise = Noise::measure(sides.iter().map(|s| s.label.clone()).collect(), &values);
		std::fs::write(path, serde_json::to_string_pretty(&noise)? + "\n")
			.with_context(|| format!("writing {}", path.display()))?;
	}
	match &args.out {
		Some(path) => std::fs::write(path, &md).with_context(|| format!("writing {}", path.display()))?,
		None => std::io::stdout().write_all(md.as_bytes())?,
	}
	Ok(0)
}

/// The tools a run records its versions of (moonshine-test-headless's run labels), and their names.
const TOOLS: [(&str, &str); 3] = [
	("harness_version", "moonshine-test-headless"),
	("client_version", "gamestream-test-client"),
	("otel_version", "moonshine-otel"),
];

/// The tools a run was made with, and this summary's own moonshine-otel when it's another.
fn tools(f: &BTreeMap<String, String>) -> String {
	let mut parts: Vec<String> = TOOLS
		.iter()
		.map(|(key, name)| format!("{name} {}", f.get(*key).map_or("N/A", String::as_str)))
		.collect();
	let mine = env!("CARGO_PKG_VERSION");
	if f.get("otel_version").is_none_or(|v| v.trim_start_matches('v') != mine) {
		parts.push(format!("summarized by moonshine-otel {mine}"));
	}
	parts.join(", ")
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

/// The Markdown, and each report card metric's value per side (by key, in `sides` order).
type Values = Vec<(String, Vec<Option<f64>>)>;

fn markdown(
	prom: &Prom,
	queries: &Queries,
	targets: &Targets,
	sides: &[Side],
	notes: &str,
	noise: Option<&Noise>,
) -> anyhow::Result<(String, Values)> {
	let mut md = String::new();
	let a = &sides[0];
	let a_run_facts = facts(prom, &a.run, "run")?;
	// A side's code: a baseline side's is its run's `baseline` fact.
	let build = |f: &BTreeMap<String, String>, side: &Side| {
		f.get(if side.base_of { "baseline" } else { "code" })
			.cloned()
			.unwrap_or_else(|| side.run.clone())
	};
	let title = match sides.get(1) {
		// A calibration: the baseline's streams, repeated.
		Some(_) if a.base_of => format!("{}, {} repeats", build(&a_run_facts, a), sides.len()),
		Some(b) if b.run != a.run => format!(
			"{} vs {}{}",
			build(&a_run_facts, a),
			build(&facts(prom, &b.run, "run")?, b),
			if b.base_of { " (retained baseline)" } else { "" }
		),
		Some(_) => format!(
			"{} vs its baseline {}",
			build(&a_run_facts, a),
			a_run_facts.get("baseline").cloned().unwrap_or_default()
		),
		None => build(&a_run_facts, a),
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
		let chaos = f.get("chaos").map(|c| format!(", chaos {c}")).unwrap_or_default();
		// A retained baseline's code is the earlier run's baseline, which it names by its code alone.
		let code = if side.base_of && side.run != a.run {
			format!("{} (retained, from run {})", get("baseline"), side.run)
		} else if a.base_of {
			// A calibration streams only the baseline.
			get("baseline")
		} else {
			format!("{} ({}, commit {})", get("code"), get("ref"), get("commit"))
		};
		md.push_str(&format!(
			"- {prefix}{code}, {} at {} fps, {}, streams of {} each, route {}{}\n",
			get("resolution"),
			get("fps"),
			get("bitrate"),
			get("seconds"),
			get("route"),
			chaos,
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
		md.push_str(&format!("- {prefix}tools: {}\n", tools(&f)));
	}
	// Runs made with different tools can measure differently: a metric's definition, the client's model.
	if let [a, b] = runs[..] {
		let (fa, fb) = (facts(prom, &a.run, "run")?, facts(prom, &b.run, "run")?);
		let differ: Vec<&str> = TOOLS
			.iter()
			.filter(|(key, _)| fa.get(*key) != fb.get(*key))
			.map(|(_, name)| *name)
			.collect();
		if !differ.is_empty() {
			md.push_str(&format!(
				"\n> [!WARNING]\n> {} and {} were made with different versions of {}, so some figures may not be measured the \
				 same way.\n",
				a.label,
				b.label,
				differ.join(" and ")
			));
		}
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
	// With noise measured and two sides, whether they differ by more than repeat runs do.
	let noise = noise.filter(|_| sides.len() == 2);
	md.push_str("\n**Report card**\n\n| Metric |");
	for side in sides {
		md.push_str(&format!(
			" {} |",
			if side.label.is_empty() { "Value" } else { &side.label }
		));
	}
	if noise.is_some() {
		md.push_str(" Change |");
	}
	md.push_str(" Target |\n|---|");
	md.push_str(&"---|".repeat(sides.len() + 1 + usize::from(noise.is_some())));
	md.push('\n');
	let mut values = Vec::new();
	for q in &queries.metrics {
		let Some(target) = targets.metrics.get(&q.key) else {
			continue;
		};
		let label = target["label"].as_str().unwrap_or(&q.key);
		let unit = target["unit"].as_str().unwrap_or("");
		let higher = target["better"].as_str() == Some("higher");
		// A metric with no thresholds yet is shown, not judged.
		let judged = target["target"].is_number();
		let (good, ok) = (
			target["target"].as_f64().unwrap_or(0.0),
			target["acceptable"].as_f64().unwrap_or(0.0),
		);
		let mut row = format!("| {} |", capitalized(label));
		let mut row_values = Vec::new();
		for side in sides {
			let expr = q
				.expr
				.replace("{sel}", &side.sel)
				.replace("$__range", LOOKBACK)
				.replace("$fps", &fps);
			let value = first_value(prom, &expr)?;
			row_values.push(value);
			row.push_str(&match value {
				Some(v) if !judged => format!(" {} |", show(v, unit, q.decimals, interval_ms)),
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
		if let Some(n) = noise {
			row.push_str(&match (row_values[0], row_values[1], n.metrics.get(&q.key)) {
				(Some(a), Some(b), Some(m)) => {
					let tol = tolerance(m.spread, b, q.decimals);
					format!(
						" {} · {} (±{}) |",
						signed(a - b, unit, q.decimals),
						change(a, b, tol, higher),
						short(tol, unit, q.decimals)
					)
				},
				(Some(_), Some(_), None) => " no noise measured |".to_string(),
				_ => " |".to_string(),
			});
		}
		values.push((q.key.clone(), row_values));
		let op = if higher { "≥" } else { "≤" };
		if judged {
			row.push_str(&format!(" {op} {} |\n", show(good, unit, q.decimals, interval_ms)));
		} else {
			row.push_str(" not judged yet |\n");
		}
		md.push_str(&row);
	}
	if !notes.trim().is_empty() {
		md.push('\n');
		md.push_str(notes.trim_end());
		md.push('\n');
	}
	md.push_str(
		"\n<sub>🟢 at or past the target, 🟡 up to the acceptable value, 🔴 past it (thresholds in targets.json; a metric \
		 without them is shown, not judged). \
		 Made by moonshine-otel summary from the run's telemetry, which is in its results.tar.gz.</sub>\n",
	);
	if let Some(n) = noise {
		md.push_str(&format!(
			"<sub>Change: test less baseline; within noise when no more than ± (half again as much as the baseline \
			 varied across {} repeat runs, and at least 2% of it).</sub>\n",
			n.sides.len()
		));
	}
	Ok((md, values))
}

/// A value in its unit, briefly (frame intervals without the milliseconds), for the Change column.
fn short(v: f64, unit: &str, decimals: usize) -> String {
	let n = format!("{v:.decimals$}");
	match unit {
		"frame intervals" => format!("{n} frames"),
		"ms" => format!("{n} ms"),
		"%" | "% of requested" => format!("{n}%"),
		_ => n,
	}
}

/// A difference with its sign, and 0 for one that rounds to nothing.
fn signed(d: f64, unit: &str, decimals: usize) -> String {
	let s = short(d.abs(), unit, decimals);
	if format!("{:.decimals$}", d.abs())
		.trim_start_matches(['0', '.'])
		.is_empty()
	{
		short(0.0, unit, decimals)
	} else if d > 0.0 {
		format!("+{s}")
	} else {
		format!("−{s}")
	}
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
	fn measures_noise_across_sides() {
		let values = vec![
			("latency".to_string(), vec![Some(4.0), Some(4.6), Some(4.2)]),
			("judder".to_string(), vec![Some(1.0), None, None]),
			("lost".to_string(), vec![None, None, None]),
		];
		let n = Noise::measure(vec!["base".into(), "base-2".into(), "base-3".into()], &values);
		let m = &n.metrics["latency"];
		assert_eq!(m.values, vec![4.0, 4.6, 4.2]);
		assert!((m.spread - 0.6).abs() < 1e-9 && (m.mean - 4.266_666).abs() < 1e-3);
		// One value, or none, is no spread.
		assert!(!n.metrics.contains_key("judder") && !n.metrics.contains_key("lost"));
	}

	#[test]
	fn judges_a_change_against_the_noise() {
		// Lower is better: 0.5 up within 0.6 is noise, 1.0 up is worse, 1.0 down better.
		assert_eq!(change(4.5, 4.0, 0.6, false), "within noise");
		assert_eq!(change(5.0, 4.0, 0.6, false), "worse");
		assert_eq!(change(3.0, 4.0, 0.6, false), "better");
		assert_eq!(change(5.0, 4.0, 0.6, true), "better");
		assert_eq!(change(4.0, 4.0, 0.0, false), "within noise");
		// The tolerance: half again the spread, else 2% of the baseline, else half the last digit shown.
		assert!((tolerance(0.4, 4.0, 2) - 0.6).abs() < 1e-9);
		assert!((tolerance(0.0, 4.0, 2) - 0.08).abs() < 1e-9);
		assert!((tolerance(0.0, 0.0, 0) - 0.5).abs() < 1e-9);
		// A recovery 0.01 frames slower than a baseline at 4 frames that didn't vary: noise, not worse.
		assert_eq!(change(4.01, 4.0, tolerance(0.0, 4.0, 2), false), "within noise");
		// One lost frame against none is a change.
		assert_eq!(change(1.0, 0.0, tolerance(0.0, 0.0, 0), false), "worse");
		assert_eq!(signed(-0.001, "frame intervals", 2), "0.00 frames");
		assert_eq!(signed(2.13, "ms", 2), "+2.13 ms");
		assert_eq!(signed(-1.5, "%", 1), "−1.5%");
	}

	#[test]
	fn names_the_tools() {
		let f: BTreeMap<String, String> = [
			("harness_version", "38daddb"),
			("client_version", "60ae0cc"),
			("otel_version", concat!("v", env!("CARGO_PKG_VERSION"))),
		]
		.into_iter()
		.map(|(k, v)| (k.to_string(), v.to_string()))
		.collect();
		assert_eq!(
			tools(&f),
			format!(
				"moonshine-test-headless 38daddb, gamestream-test-client 60ae0cc, moonshine-otel v{}",
				env!("CARGO_PKG_VERSION")
			)
		);
		assert!(tools(&BTreeMap::new()).ends_with(&format!(
			"moonshine-otel N/A, summarized by moonshine-otel {}",
			env!("CARGO_PKG_VERSION")
		)));
	}

	#[test]
	fn values_read_in_their_units() {
		assert_eq!(show(1.02, "frame intervals", 2, 1000.0 / 60.0), "1.02 frames (17.0 ms)");
		assert_eq!(show(3.384, "ms", 2, 16.7), "3.38 ms");
		assert_eq!(show(104.39, "% of requested", 1, 16.7), "104.4%");
		assert_eq!(show(0.0, "", 0, 16.7), "0");
		assert_eq!(encoded(r#"a{run="x · y"}"#), "a%7Brun%3D%22x%20%C2%B7%20y%22%7D");
	}
}
