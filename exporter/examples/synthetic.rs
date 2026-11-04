//! Feed synthetic frames through the exporter, in real time, to exercise the whole
//! exporter → collector → Prometheus/Tempo → Grafana path without a GPU.
//!
//! ```text
//! cargo run -p moonshine-otel-exporter --example synthetic -- --duration 60 --out base.json
//! cargo run -p moonshine-otel-exporter --example synthetic -- --duration 60 --convert-shift-us 800 --out slow.json
//! ```
//!
//! The pipeline model: steady stage costs with gaussian-ish jitter, an encode stall every
//! `--spike-every` frames, a delivery gap (pacing miss) every `--gap-every` frames, and an
//! IDR re-encode after each gap, like Moonshine does when no frame arrives in time.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use clap::Parser;
use moonshine_otel_exporter::results::FrameCsvWriter;
use moonshine_otel_exporter::summary::format_summary;
use moonshine_otel_exporter::telemetry::check_endpoint;
use moonshine_otel_exporter::*;

#[derive(Parser, Debug)]
struct Args {
	/// OTLP endpoint.
	#[arg(long, default_value = "http://localhost:4317")]
	endpoint: String,
	#[arg(long, default_value = "grpc")]
	protocol: Protocol,
	/// Write OTLP batches into this directory instead of sending them (see `moonshine-otel-relay`).
	#[arg(long)]
	spool: Option<PathBuf>,
	/// Don't export anything; only print the summary and write results.
	#[arg(long)]
	no_export: bool,
	#[arg(long, default_value = "outliers")]
	trace_mode: TraceMode,
	#[arg(long, default_value = "exponential")]
	histogram_buckets: HistogramBuckets,
	#[arg(long, default_value_t = 60)]
	fps: u32,
	/// Seconds per iteration.
	#[arg(long, default_value_t = 30)]
	duration: u64,
	#[arg(long, default_value_t = 2)]
	warmup: u64,
	#[arg(long, default_value_t = 1)]
	iterations: u32,
	/// Extra convert time in µs, to simulate a regression.
	#[arg(long, default_value_t = 0.0)]
	convert_shift_us: f64,
	/// Add an encode stall every N frames (0 = never).
	#[arg(long, default_value_t = 240)]
	spike_every: u64,
	/// Skip a frame (delivery gap) every N frames (0 = never).
	#[arg(long, default_value_t = 600)]
	gap_every: u64,
	#[arg(long, default_value = "h264")]
	codec: String,
	#[arg(long)]
	seed: Option<u64>,
	#[arg(long)]
	out: Option<PathBuf>,
	#[arg(long)]
	frames_csv: Option<PathBuf>,
	/// `key=value` labels.
	#[arg(long = "label", value_parser = parse_label)]
	labels: Vec<(String, String)>,
}

fn parse_label(s: &str) -> Result<(String, String), String> {
	s.split_once('=')
		.map(|(k, v)| (k.to_string(), v.to_string()))
		.ok_or_else(|| format!("expected key=value, got `{s}`"))
}

struct Rng(u64);

impl Rng {
	fn uniform(&mut self) -> f64 {
		self.0 ^= self.0 >> 12;
		self.0 ^= self.0 << 25;
		self.0 ^= self.0 >> 27;
		(self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64
	}

	fn normal(&mut self) -> f64 {
		(0..6).map(|_| self.uniform()).sum::<f64>() - 3.0
	}
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	tracing_subscriber::fmt()
		.with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
		.init();
	let args = Args::parse();

	let mut identity = RunIdentity::default();
	identity.build.profile = Some(if cfg!(debug_assertions) { "debug" } else { "release" }.into());
	identity.build.tool_version = Some(env!("CARGO_PKG_VERSION").into());
	identity.host.hostname = gethostname::gethostname().into_string().ok();
	identity.gpu.model = Some("synthetic".into());
	identity.labels = args.labels.iter().cloned().collect();
	identity.labels.insert("source".into(), "synthetic".into());

	let telemetry_config = TelemetryConfig {
		endpoint: args.endpoint.clone(),
		protocol: args.protocol,
		trace_mode: args.trace_mode,
		histogram_buckets: args.histogram_buckets,
		resource: identity.resource_attributes(),
		service_name: "moonshine-otel-synthetic".into(),
		..Default::default()
	};
	let telemetry = if args.no_export {
		Telemetry::disabled()
	} else if let Some(dir) = &args.spool {
		Telemetry::spool(&telemetry_config, dir, SpoolLimits::default())?
	} else {
		check_endpoint(&args.endpoint, Duration::from_secs(2))?;
		Telemetry::otlp(&telemetry_config)?
	};

	let config = RunConfig {
		command: vec!["synthetic".into()],
		width: 1920,
		height: 1080,
		target_fps: args.fps,
		codec: args.codec.clone(),
		bitrate: 20_000_000,
		hdr: false,
		chroma: "yuv420".into(),
		warmup_s: args.warmup as f64,
		duration_s: args.duration as f64,
		iterations: args.iterations,
	};
	let mut run = RunRecorder::new(&telemetry, identity, config);
	if let Some(path) = &args.frames_csv {
		run = run.with_frames_csv(FrameCsvWriter::create(path, Instant::now())?);
	}
	tracing::info!(run_id = run.run_id(), "starting synthetic run");

	let mut rng = Rng(args.seed.unwrap_or_else(|| {
		std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.unwrap()
			.as_nanos() as u64
			| 1
	}));
	let period = Duration::from_secs(1) / args.fps;
	let mut frame_no: u64 = 0;
	for _ in 0..args.iterations {
		run.begin_iteration(Duration::from_secs(args.warmup));
		let end = Instant::now() + Duration::from_secs(args.warmup + args.duration);
		let mut next = Instant::now();
		while Instant::now() < end {
			frame_no += 1;
			next += period;
			if args.gap_every > 0 && frame_no.is_multiple_of(args.gap_every) {
				// No frame this period: the pipeline re-encodes an IDR instead.
				tokio::time::sleep_until(next.into()).await;
				let mut idr = FrameSample::zeroed(Instant::now());
				idr.is_key_frame = true;
				idr.submit = Duration::from_micros(150);
				idr.encode_wait = Duration::from_micros(4500);
				idr.packetize = Duration::from_micros(200);
				idr.send = Duration::from_micros(40);
				idr.total = Duration::from_micros(4900);
				idr.encoded_bytes = 160_000;
				run.record(&idr);
				continue;
			}
			let us = |v: f64| Duration::from_micros(v.max(1.0) as u64);
			let stall = if args.spike_every > 0 && frame_no.is_multiple_of(args.spike_every) {
				12_000.0 + 8_000.0 * rng.uniform()
			} else {
				0.0
			};
			let channel_wait = 80.0 + 30.0 * rng.normal().abs();
			let import = 45.0 + 5.0 * rng.normal();
			let convert = 280.0 + args.convert_shift_us + 25.0 * rng.normal();
			let submit = 140.0 + 20.0 * rng.normal();
			let encode_wait = 2600.0 + stall + 250.0 * rng.normal();
			let packetize = 70.0 + 10.0 * rng.normal();
			let send = 25.0 + 5.0 * rng.normal();
			let slack = 35.0 + 10.0 * rng.normal().abs();
			let total = channel_wait + import + convert + submit + encode_wait + packetize + send + slack;
			// The frame is delivered `total` after it was captured, at the start of this period.
			tokio::time::sleep_until((next - period + us(total)).into()).await;
			let key = frame_no.is_multiple_of(3600);
			let sample = FrameSample {
				channel_wait: us(channel_wait),
				import: us(import),
				convert: us(convert),
				submit: us(submit),
				consumer_queue: us(12.0 + 4.0 * rng.normal()),
				encode_wait: us(encode_wait),
				packetize: us(packetize),
				send: us(send),
				total: us(total),
				encoded_bytes: if key {
					180_000
				} else {
					38_000 + (6_000.0 * rng.uniform()) as u64
				},
				is_key_frame: key,
				..FrameSample::zeroed(Instant::now())
			};
			run.record(&sample);
			tokio::time::sleep_until(next.into()).await;
		}
		if let Some(it) = run.end_iteration() {
			print!("{}", format_summary(&format!("Iteration {}", it.index), &it.summary));
		}
	}
	let result = run.finish();
	for w in &result.warnings {
		tracing::warn!("{w}");
	}
	if let Some(path) = &args.out {
		ResultsFile::new(vec![result]).write(path)?;
		tracing::info!("wrote {}", path.display());
	}
	let stats_before = telemetry.span_stats().ended.load(std::sync::atomic::Ordering::Relaxed);
	telemetry.shutdown()?;
	tracing::info!(spans = stats_before, "telemetry flushed");
	Ok(())
}
