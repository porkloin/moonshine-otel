//! `moonshine-otel`: record Moonshine's streams from its statistics socket with exact
//! per-stream statistics and OTLP export, and compare recordings.
//!
//! Exit codes: 0 ok, 1 regression (`compare`), 2 invalid comparison, 3 failure.

mod app;
mod client;
mod env;
mod follow;
mod info;
mod plot;
mod summary;
mod watch;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand};
use moonshine_otel_exporter::cli::{CompareArgs, run_compare};
use moonshine_otel_exporter::telemetry::check_endpoint;
use moonshine_otel_exporter::{
	HistogramBuckets, Protocol, RunResult, SpoolLimits, Telemetry, TelemetryConfig, TraceMode,
};
use opentelemetry::KeyValue;
use tracing_subscriber::EnvFilter;

const EXIT_FAILED: i32 = 3;
/// The default collector's ports, and the variables that move them (as for the stack's containers).
const DEFAULT_GRPC_PORT: (&str, u16) = ("MOONSHINE_OTEL_OTLP_GRPC_PORT", 4317);
const DEFAULT_HTTP_PORT: (&str, u16) = ("MOONSHINE_OTEL_OTLP_HTTP_PORT", 4318);
/// Tries of the OTLP endpoint, 2 s apart, before failing (explicit) or recording without export (default).
const ENDPOINT_ATTEMPTS: u32 = 15;

#[derive(Parser, Debug)]
#[command(name = "moonshine-otel", version, about, long_about = None)]
struct Cli {
	#[command(subcommand)]
	command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
	/// Record every stream of a running Moonshine from its statistics socket.
	Watch(watch::WatchArgs),
	/// Export a test client's per-frame statistics as spans in each frame's trace, beside Moonshine's.
	Client(client::ClientArgs),
	/// Export a test workload's log (clickflash, hdrneon) as metrics beside the client's.
	App(app::AppArgs),
	/// Export facts about a test run (machines, code, settings) as the labels of one sample.
	Info(info::InfoArgs),
	/// A test run's bundle (results.tar.gz) as Markdown for a pull request, from its telemetry.
	Summary(summary::SummaryArgs),
	/// Compare two result files or directories (exit 0 no regression, 1 regression, 2 invalid).
	Compare(CompareArgs),
	/// Print the captured run identity and warn about sources of noise.
	Env(EnvArgs),
	/// Draw charts (SVG and PNG): click-to-flash latency, a stream's timeline, frame pacing or pipeline stages.
	Plot(plot::PlotArgs),
}

#[derive(Args, Debug, Clone)]
struct ExportArgs {
	/// OTLP endpoint. Unreachable is an error when given explicitly; otherwise the default
	/// endpoint is tried and export is skipped with a warning if nothing listens there.
	#[arg(long, env = "OTEL_EXPORTER_OTLP_ENDPOINT")]
	otlp_endpoint: Option<String>,
	/// OTLP transport: grpc or http/protobuf.
	#[arg(long, default_value = "grpc")]
	otlp_protocol: Protocol,
	/// Which frames get trace spans: `none`, `outliers`, `sampled:<p>` or `all`.
	#[arg(long, default_value = "outliers")]
	trace_mode: TraceMode,
	/// Seconds between metric exports.
	#[arg(long, default_value_t = 5)]
	metric_interval: u64,
	/// exponential (Prometheus native histograms) or explicit buckets.
	#[arg(long, default_value = "exponential")]
	histogram_buckets: HistogramBuckets,
	/// Write OTLP batches into this directory instead of sending them, for a server that runs
	/// sandboxed without network access to a collector. Forward them from outside the sandbox
	/// with `moonshine-otel-relay`.
	#[arg(long, env = "MOONSHINE_OTEL_SPOOL", conflicts_with = "otlp_endpoint")]
	otlp_spool: Option<PathBuf>,
	/// Export nothing. Use to measure the exporter's own overhead.
	#[arg(long, conflicts_with_all = ["otlp_endpoint", "otlp_spool"])]
	no_export: bool,
}

#[derive(Args, Debug)]
struct EnvArgs {
	/// GPU (render node) to describe.
	#[arg(long)]
	gpu: Option<String>,
	/// Print JSON instead of text.
	#[arg(long)]
	json: bool,
}

fn parse_label(s: &str) -> Result<(String, String), String> {
	match s.split_once('=') {
		Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
		_ => Err(format!("expected key=value, got `{s}`")),
	}
}

fn main() {
	// The fmt subscriber goes first so telemetry setup errors are never silent.
	tracing_subscriber::fmt()
		.with_env_filter(
			EnvFilter::try_from_env("MOONSHINE_LOG").unwrap_or_else(|_| EnvFilter::new("info,smithay=warn")),
		)
		// No color codes in the journal.
		.with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
		.init();
	let cli = Cli::parse();
	let runtime = tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.expect("tokio runtime");
	let code = runtime.block_on(async {
		match cli.command {
			Command::Compare(args) => match run_compare(&args) {
				Ok(c) => c.outcome.exit_code(),
				Err(e) => {
					tracing::error!("{e:#}");
					2
				},
			},
			Command::Env(args) => print_env(&args).map_or_else(
				|e| {
					tracing::error!("{e:#}");
					EXIT_FAILED
				},
				|_| 0,
			),
			Command::Plot(args) => plot::plot(&args).map_or_else(
				|e| {
					tracing::error!("{e:#}");
					EXIT_FAILED
				},
				|()| 0,
			),
			// Its blocking HTTP client can't run on the async runtime's threads.
			Command::Summary(args) => tokio::task::spawn_blocking(move || summary::summary(&args))
				.await
				.map_err(anyhow::Error::from)
				.and_then(|r| r)
				.unwrap_or_else(|e| {
					tracing::error!("{e:#}");
					EXIT_FAILED
				}),
			Command::Info(args) => info::info(&args).unwrap_or_else(|e| {
				tracing::error!("{e:#}");
				EXIT_FAILED
			}),
			Command::App(args) => app::app(&args).await.unwrap_or_else(|e| {
				tracing::error!("{e:#}");
				EXIT_FAILED
			}),
			Command::Client(args) => client::client(&args).await.unwrap_or_else(|e| {
				tracing::error!("{e:#}");
				EXIT_FAILED
			}),
			Command::Watch(args) => watch::watch(&args).await.unwrap_or_else(|e| {
				tracing::error!("{e:#}");
				EXIT_FAILED
			}),
		}
	});
	runtime.shutdown_timeout(Duration::from_secs(2));
	std::process::exit(code);
}

fn print_env(args: &EnvArgs) -> anyhow::Result<()> {
	let vulkan = env::vulkan_info(args.gpu.as_deref());
	let identity = env::identity(args.gpu.as_deref(), vulkan.as_ref(), BTreeMap::new());
	if args.json {
		println!("{}", serde_json::to_string_pretty(&identity)?);
	} else {
		for (k, v) in identity.fields() {
			println!("{k:<28} {v}");
		}
	}
	let warnings = identity.variance_warnings();
	if warnings.is_empty() {
		eprintln!("\nno variance warnings");
	}
	for w in warnings {
		eprintln!("warning: {w}");
	}
	Ok(())
}

fn setup_telemetry(e: &ExportArgs, service_name: &str, resource: Vec<KeyValue>) -> anyhow::Result<Telemetry> {
	if e.no_export {
		tracing::info!("--no-export: telemetry disabled");
		return Ok(Telemetry::disabled());
	}
	let config = TelemetryConfig {
		trace_mode: e.trace_mode,
		metric_interval: Duration::from_secs(e.metric_interval.max(1)),
		histogram_buckets: e.histogram_buckets,
		resource,
		service_name: service_name.to_string(),
		..Default::default()
	};
	if let Some(dir) = &e.otlp_spool {
		let telemetry = Telemetry::spool(&config, dir, SpoolLimits::default()).context("setting up the OTLP spool")?;
		tracing::info!("spooling to {} (trace mode {})", dir.display(), e.trace_mode);
		return Ok(telemetry);
	}
	let (endpoint, explicit) = match &e.otlp_endpoint {
		Some(ep) => (ep.clone(), true),
		None => (
			default_endpoint(match e.otlp_protocol {
				Protocol::Grpc => DEFAULT_GRPC_PORT,
				Protocol::HttpProtobuf => DEFAULT_HTTP_PORT,
			}),
			false,
		),
	};
	// The collector may still be starting (both start at login), so give it a moment.
	let mut checked = check_endpoint(&endpoint, Duration::from_secs(2));
	for _ in 1..ENDPOINT_ATTEMPTS {
		if checked.is_ok() {
			break;
		}
		std::thread::sleep(Duration::from_secs(2));
		checked = check_endpoint(&endpoint, Duration::from_secs(2));
	}
	if let Err(err) = checked {
		if explicit {
			bail!("OTLP endpoint check failed: {err:#}");
		}
		tracing::warn!(
			"no OTLP collector at {endpoint} ({err:#}); continuing without export. Start moonshine-otel-stack.target or pass --otlp-endpoint."
		);
		return Ok(Telemetry::disabled());
	}
	let telemetry = Telemetry::otlp(&TelemetryConfig {
		endpoint: endpoint.clone(),
		protocol: e.otlp_protocol,
		..config
	})
	.context("setting up OTLP export")?;
	tracing::info!("exporting to {endpoint} (trace mode {})", e.trace_mode);
	Ok(telemetry)
}

/// `http://localhost:<port>`, the port from its variable when that holds one.
fn default_endpoint((var, port): (&str, u16)) -> String {
	let port = std::env::var(var)
		.ok()
		.and_then(|v| v.trim().parse::<u16>().ok())
		.unwrap_or(port);
	format!("http://localhost:{port}")
}

fn report_run(r: &RunResult) {
	if r.iterations.len() > 1 {
		let m = |k: &str| {
			r.aggregate
				.get(k)
				.map_or("n/a".to_string(), |s| format!("{:.0} ±{:.0}", s.median, s.mad))
		};
		println!(
			"Run {} [{} iterations]: total p50 {}us, p99 {}us, interval p99 {}us, fps {}",
			r.run_id,
			r.iterations.len(),
			m("latency.captured.total.p50"),
			m("latency.captured.total.p99"),
			m("interval.p99"),
			r.aggregate
				.get("effective_fps")
				.map_or("n/a".into(), |s| format!("{:.1}", s.median)),
		);
	}
	for w in &r.warnings {
		tracing::warn!("{w}");
	}
	for it in &r.iterations {
		for w in &it.warnings {
			tracing::warn!("iteration {}: {w}", it.index);
		}
	}
	if !r.valid {
		tracing::warn!("run {} is marked INVALID; compare will refuse it", r.run_id);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Every concrete `moonshine-otel` example in README.md and
	/// AGENTS.md must parse with the current CLI.
	#[test]
	fn readme_examples_parse() {
		let docs = concat!(include_str!("../../README.md"), include_str!("../../AGENTS.md"));
		let mut checked = 0;
		let mut in_block = false;
		for line in docs.lines() {
			if line.starts_with("```") {
				in_block = line == "```sh";
				continue;
			}
			let cmd = line.split(" #").next().unwrap().trim();
			// Skip synopsis lines ([optional] <placeholder> ...) and continuations.
			if !in_block || cmd.contains('[') || cmd.contains('<') || cmd.ends_with('\\') {
				continue;
			}
			let argv: Vec<&str> = cmd.split_whitespace().collect();
			if argv.first() != Some(&"moonshine-otel") {
				continue;
			}
			if let Err(e) = Cli::try_parse_from(&argv) {
				panic!("documented example does not parse: `{cmd}`\n{e}");
			}
			checked += 1;
		}
		assert!(checked >= 5, "only {checked} examples found");
	}

	#[test]
	fn labels() {
		assert_eq!(parse_label("branch=a=b").unwrap(), ("branch".into(), "a=b".into()));
		assert!(parse_label("=x").is_err());
		assert!(parse_label("nokey").is_err());
	}
}
