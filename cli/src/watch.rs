//! `watch`: record every stream of a running Moonshine from its statistics socket, so real
//! streams show up live in the dashboards.
//!
//! Moonshine writes one JSON object per line to the socket set in its configuration's `[stats]`
//! table: `stream_started` with the stream's parameters and the format's `stats_version`, a `frame`
//! per encoded frame, and `stream_stopped`. Every stage time in a `frame` line is a number of
//! seconds. Each stream becomes one run. The socket's peer is Moonshine's process, whose GPU and
//! CPU use is sampled alongside.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Args;
use moonshine_otel_exporter::results::FrameCsvWriter;
use moonshine_otel_exporter::sample::FrameSample;
use moonshine_otel_exporter::summary::format_summary;
use moonshine_otel_exporter::{ResultsFile, RunConfig, RunIdentity, RunRecorder, Telemetry};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};

use crate::env::{ResourceSampler, spawn_sampler};
use crate::{ExportArgs, parse_label, report_run, setup_telemetry};

/// How often to try the socket while Moonshine isn't listening.
const RETRY_PERIOD: Duration = Duration::from_secs(2);

#[derive(Args, Debug)]
pub struct WatchArgs {
	/// Moonshine's statistics socket (default: $XDG_RUNTIME_DIR/moonshine/stats.sock).
	#[arg(long)]
	socket: Option<PathBuf>,
	/// Render node of the GPU Moonshine uses, if it isn't the first one.
	#[arg(long)]
	gpu: Option<String>,
	/// Seconds discarded at the start of every stream.
	#[arg(long, default_value_t = 2)]
	warmup: u64,
	/// Write one result JSON and one per-frame CSV per stream into this directory.
	#[arg(long)]
	results_dir: Option<PathBuf>,
	/// Free-form label recorded with every stream, e.g. --label branch=zero-copy. Repeatable.
	#[arg(long = "label", value_parser = parse_label)]
	labels: Vec<(String, String)>,
	/// Read each stream's key from this file when the stream starts. Each frame then gets a trace of its own, with
	/// ids from the key and the frame's number, which a test client told the same key can add its spans to.
	#[arg(long)]
	stream_key_file: Option<PathBuf>,
	#[command(flatten)]
	export: ExportArgs,
}

/// The statistics format version this build reads: Moonshine's `stats_version`.
const STATS_VERSION: u32 = 1;

/// One line from Moonshine's statistics socket.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Line {
	StreamStarted {
		/// The socket's format version; Moonshine's `stats_version`.
		stats_version: u32,
		width: u32,
		height: u32,
		fps: u32,
		bitrate: u64,
		codec: String,
		/// `sdr` or `hdr`.
		dynamic_range: String,
		chroma: String,
	},
	Frame {
		/// The frame index the client receives, so the two sides' records of a frame can be matched.
		frame_number: u64,
		/// When the frame was captured, in seconds since the Unix epoch.
		captured_at: f64,
		/// The captured content's color space by Moonshine's own names (`srgb`, `bt2020_pq`,
		/// `scrgb_linear`); absent on a keyframe re-encoded without a new capture.
		#[serde(default)]
		content: Option<String>,
		/// Every stage's time in seconds; Moonshine's `Duration`s serialized as floats.
		channel_wait: f64,
		import: f64,
		convert: f64,
		submit: f64,
		consumer_queue: f64,
		encode_wait: f64,
		/// Times measured inside the GPU's video queue, sub-intervals of `encode_wait`; Moonshine
		/// leaves both out entirely when that queue reports no timestamps.
		#[serde(default)]
		gpu_encode: Option<f64>,
		#[serde(default)]
		bitstream_ready: Option<f64>,
		packetize: f64,
		send: f64,
		total: f64,
		encoded_bytes: u64,
		is_key_frame: bool,
	},
	StreamStopped,
	Lagged {
		missed: u64,
	},
	/// A line type this version doesn't know.
	#[serde(other)]
	Other,
}

/// One Moonlight stream being recorded.
struct Stream {
	recorder: RunRecorder,
	stop_sampler: std::sync::mpsc::Sender<()>,
	sampler: std::thread::JoinHandle<ResourceSampler>,
}

/// Why the connection to Moonshine ended.
enum End {
	Shutdown,
	Disconnected,
}

pub async fn watch(args: &WatchArgs) -> anyhow::Result<i32> {
	let socket = match &args.socket {
		Some(path) => path.clone(),
		None => std::env::var_os("XDG_RUNTIME_DIR")
			.map(|dir| PathBuf::from(dir).join("moonshine/stats.sock"))
			.context("XDG_RUNTIME_DIR isn't set; pass --socket")?,
	};
	if let Some(dir) = &args.results_dir {
		std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
	}

	tracing::info!("probing GPU and Vulkan driver");
	let gpu = args.gpu.clone();
	let vulkan = tokio::task::spawn_blocking(move || crate::env::vulkan_info(gpu.as_deref()))
		.await
		.ok()
		.flatten();
	let mut labels: BTreeMap<String, String> = args.labels.iter().cloned().collect();
	labels.insert("mode".into(), "watch".into());
	let identity = crate::env::identity(args.gpu.as_deref(), vulkan.as_ref(), labels);
	for warning in identity.variance_warnings() {
		tracing::warn!("{warning}");
	}
	let telemetry = setup_telemetry(&args.export, "moonshine-otel", identity.resource_attributes())?;

	let mut terminate = signal(SignalKind::terminate()).context("installing the SIGTERM handler")?;
	let mut total_lagged = 0;
	while let Some(stream) = connect(&socket, &mut terminate).await {
		let pid = stream
			.peer_cred()
			.ok()
			.and_then(|c| c.pid())
			.and_then(|p| u32::try_from(p).ok());
		tracing::info!(
			"recording Moonshine{}'s streams",
			pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
		);

		let mut session = Session {
			args,
			telemetry: &telemetry,
			identity: &identity,
			pid,
			current: None,
			lagged: 0,
		};
		let end = record(stream, &mut session, &mut terminate).await?;
		total_lagged += session.lagged;
		session.finish(match end {
			End::Shutdown => "moonshine-otel stopped during the stream",
			End::Disconnected => "Moonshine stopped during the stream",
		});
		match end {
			End::Shutdown => break,
			End::Disconnected => tracing::info!("Moonshine closed its statistics socket"),
		}
	}

	if total_lagged > 0 {
		tracing::warn!("{total_lagged} statistics messages lost to lag");
	}
	if let Err(err) = telemetry.flush() {
		tracing::warn!("{err:#}");
	}
	if let Err(err) = telemetry.shutdown() {
		tracing::warn!("{err:#}");
	}
	Ok(0)
}

/// A stage's time, as Moonshine writes it: a number of seconds.
fn seconds(s: f64) -> Duration {
	Duration::from_secs_f64(s)
}

fn to_sample(frame: &Line, observed_at: Instant) -> FrameSample {
	let mut sample = FrameSample::zeroed(observed_at);
	if let Line::Frame {
		frame_number,
		captured_at,
		ref content,
		channel_wait,
		import,
		convert,
		submit,
		consumer_queue,
		encode_wait,
		gpu_encode,
		bitstream_ready,
		packetize,
		send,
		total,
		encoded_bytes,
		is_key_frame,
	} = *frame
	{
		sample.channel_wait = seconds(channel_wait);
		sample.import = seconds(import);
		sample.convert = seconds(convert);
		sample.submit = seconds(submit);
		sample.consumer_queue = seconds(consumer_queue);
		sample.encode_wait = seconds(encode_wait);
		sample.gpu_encode = gpu_encode.map(seconds);
		sample.bitstream_ready = bitstream_ready.map(seconds);
		sample.packetize = seconds(packetize);
		sample.send = seconds(send);
		sample.total = seconds(total);
		sample.encoded_bytes = encoded_bytes;
		sample.is_key_frame = is_key_frame;
		sample.seq = Some(frame_number);
		// Moonshine names the color space after its encoding; the workloads' mode events, which
		// content is compared with, name the modes.
		sample.content = content.as_deref().map(|c| match c {
			"srgb" => "sdr",
			"bt2020_pq" => "hdr10",
			"scrgb_linear" => "scrgb",
			_ => "other",
		});
		// Moonshine and moonshine-otel share the machine's wall clock; place the capture on this process's
		// monotonic clock by how long ago it was.
		sample.captured_at = Duration::try_from_secs_f64(captured_at).ok().and_then(|since_epoch| {
			let ago = std::time::SystemTime::now()
				.duration_since(std::time::UNIX_EPOCH + since_epoch)
				.ok()?;
			observed_at.checked_sub(ago)
		});
	}
	sample
}

/// Connect to Moonshine's statistics socket, waiting for it to appear; None when asked to stop first.
async fn connect(socket: &std::path::Path, terminate: &mut tokio::signal::unix::Signal) -> Option<UnixStream> {
	let mut waiting = false;
	loop {
		let connected = tokio::select! {
			_ = tokio::signal::ctrl_c() => return None,
			_ = terminate.recv() => return None,
			connected = UnixStream::connect(socket) => connected,
		};
		match connected {
			Ok(stream) => return Some(stream),
			Err(e) if !waiting => {
				tracing::info!(
					"waiting for Moonshine's statistics socket {} ({e}); set it in the [stats] table of Moonshine's config",
					socket.display()
				);
				waiting = true;
			},
			Err(_) => {},
		}
		tokio::select! {
			_ = tokio::signal::ctrl_c() => return None,
			_ = terminate.recv() => return None,
			_ = tokio::time::sleep(RETRY_PERIOD) => {},
		}
	}
}

/// One connection to Moonshine's statistics socket: the stream being recorded, and messages lost to lag.
struct Session<'a> {
	args: &'a WatchArgs,
	telemetry: &'a Telemetry,
	identity: &'a RunIdentity,
	pid: Option<u32>,
	current: Option<Stream>,
	lagged: u64,
}

impl Session<'_> {
	/// One statistics line: a stream starts or stops, a frame, or lag.
	fn handle(&mut self, text: &str) -> anyhow::Result<()> {
		match serde_json::from_str::<Line>(text) {
			Ok(Line::StreamStarted {
				stats_version,
				width,
				height,
				fps,
				bitrate,
				codec,
				dynamic_range,
				chroma,
			}) => {
				if stats_version > STATS_VERSION {
					tracing::warn!(
						"Moonshine's statistics are format version {stats_version}; this build reads version {STATS_VERSION} \
						and defaults anything newer leaves out"
					);
				}
				self.finish("a new stream started before this one stopped");
				let config = RunConfig {
					command: Vec::new(),
					width,
					height,
					target_fps: fps,
					codec,
					bitrate,
					hdr: dynamic_range == "hdr",
					chroma,
					warmup_s: self.args.warmup as f64,
					duration_s: 0.0,
					iterations: 1,
				};
				self.current = Some(start_stream(
					config,
					self.pid,
					self.telemetry,
					self.identity,
					self.args,
				)?);
			},
			Ok(frame @ Line::Frame { .. }) => {
				if let Some(stream) = &mut self.current {
					stream.recorder.record(&to_sample(&frame, Instant::now()));
				}
			},
			Ok(Line::StreamStopped) => {
				if let Some(stream) = self.current.take() {
					finish_stream(stream, self.args, None);
				}
			},
			Ok(Line::Lagged { missed }) => {
				self.lagged += missed;
				tracing::warn!("fell behind Moonshine's statistics: {missed} messages lost");
			},
			Ok(Line::Other) => {},
			Err(e) => tracing::debug!("unreadable statistics line ({e}): {text}"),
		}
		Ok(())
	}

	/// Close the stream being recorded, if any, as incomplete for `why`.
	fn finish(&mut self, why: &str) {
		if let Some(stream) = self.current.take() {
			finish_stream(stream, self.args, Some(why));
		}
	}
}

/// Read a connection's lines into the session until Moonshine closes it or this process is asked to stop.
async fn record(
	stream: UnixStream,
	session: &mut Session<'_>,
	terminate: &mut tokio::signal::unix::Signal,
) -> anyhow::Result<End> {
	let mut lines = BufReader::new(stream).lines();
	loop {
		let line = tokio::select! {
			biased;
			_ = tokio::signal::ctrl_c() => return Ok(End::Shutdown),
			_ = terminate.recv() => return Ok(End::Shutdown),
			line = lines.next_line() => line,
		};
		match line {
			Ok(Some(text)) => session.handle(&text)?,
			Ok(None) | Err(_) => return Ok(End::Disconnected),
		}
	}
}

fn start_stream(
	config: RunConfig,
	pid: Option<u32>,
	telemetry: &Telemetry,
	identity: &RunIdentity,
	args: &WatchArgs,
) -> anyhow::Result<Stream> {
	let key = args.stream_key_file.as_ref().and_then(|path| {
		match std::fs::read_to_string(path).map(|k| k.trim().to_string()) {
			Ok(key) if !key.is_empty() => Some(key),
			Ok(_) => {
				tracing::warn!("{} is empty: no frame traces for this stream", path.display());
				None
			},
			Err(e) => {
				tracing::warn!("no stream key from {}: {e}", path.display());
				None
			},
		}
	});
	let mut recorder = match key {
		Some(key) => {
			tracing::info!("stream key {key}");
			RunRecorder::keyed(telemetry, identity.clone(), config, key)
		},
		None => RunRecorder::new(telemetry, identity.clone(), config),
	};
	if let Some(dir) = &args.results_dir {
		let path = dir.join(format!("{}.frames.csv", recorder.run_id()));
		match FrameCsvWriter::create(&path, Instant::now()) {
			Ok(csv) => recorder = recorder.with_frames_csv(csv),
			Err(e) => tracing::warn!("no frame CSV for this stream: {e:#}"),
		}
	}
	let warmup = Duration::from_secs(args.warmup);
	recorder.begin_iteration(warmup);
	// Without Moonshine's pid there's nothing to sample, and the result says so.
	let (stop_sampler, sampler) = spawn_sampler(recorder.metrics(), pid.unwrap_or(0), Instant::now() + warmup)?;
	let c = recorder.config();
	tracing::info!(
		"stream started: run {} {} {}fps {} {:.1} Mbps{}",
		recorder.run_id(),
		c.resolution(),
		c.target_fps,
		c.codec,
		c.bitrate as f64 / 1e6,
		if c.hdr { " hdr" } else { "" },
	);
	Ok(Stream {
		recorder,
		stop_sampler,
		sampler,
	})
}

fn finish_stream(mut stream: Stream, args: &WatchArgs, incomplete: Option<&str>) {
	let _ = stream.stop_sampler.send(());
	let mut warnings = Vec::new();
	let resources = match stream.sampler.join() {
		Ok(sampler) => {
			if !sampler.gpu_supported() {
				warnings.push("GPU engine stats unsupported: the driver exposes no DRM fdinfo engine data".into());
			}
			sampler.window_means()
		},
		Err(_) => {
			warnings.push("resource sampler thread panicked".into());
			BTreeMap::new()
		},
	};
	if stream.recorder.recorded_frames() == 0 {
		warnings.push("no frames were recorded after warmup".into());
	}
	stream
		.recorder
		.end_iteration_with(resources, warnings, incomplete.map(str::to_string));
	let result = stream.recorder.finish();
	if let Some(it) = result.iterations.last() {
		println!("{}", format_summary(&format!("Stream {}", result.run_id), &it.summary));
	}
	report_run(&result);
	if let Some(dir) = &args.results_dir {
		let path = dir.join(format!("{}.json", result.run_id));
		match ResultsFile::new(vec![result]).write(&path) {
			Ok(()) => tracing::info!("wrote {}", path.display()),
			Err(e) => tracing::warn!("{e:#}"),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reads_moonshines_lines() {
		let started: Line = serde_json::from_str(
			r#"{"type":"stream_started","stats_version":1,"width":1920,"height":1080,"fps":60,"bitrate":20000000,"codec":"hevc","dynamic_range":"sdr","chroma":"yuv420"}"#,
		)
		.unwrap();
		assert_eq!(
			started,
			Line::StreamStarted {
				stats_version: 1,
				width: 1920,
				height: 1080,
				fps: 60,
				bitrate: 20_000_000,
				codec: "hevc".into(),
				dynamic_range: "sdr".into(),
				chroma: "yuv420".into(),
			}
		);
		let frame: Line = serde_json::from_str(
			r#"{"type":"frame","frame_number":1,"captured_at":1793750400.5,"channel_wait":0.000041,"import":0.000028,"convert":0.00019,"submit":0.000035,"consumer_queue":0.000012,"encode_wait":0.00163,"gpu_encode":0.00112,"bitstream_ready":0.00145,"packetize":0.000022,"send":0.000048,"total":0.00201,"encoded_bytes":41666,"is_key_frame":false}"#,
		)
		.unwrap();
		let sample = to_sample(&frame, Instant::now());
		assert_eq!(sample.total, seconds(0.00201));
		assert_eq!(sample.encode_wait, seconds(0.00163));
		assert_eq!(sample.channel_wait, seconds(0.000041));
		assert_eq!(sample.gpu_encode, Some(seconds(0.00112)));
		assert_eq!(sample.bitstream_ready, Some(seconds(0.00145)));
		assert_eq!(sample.encoded_bytes, 41666);
		assert!(!sample.is_key_frame);
		assert_eq!(
			serde_json::from_str::<Line>(r#"{"type":"stream_stopped"}"#).unwrap(),
			Line::StreamStopped
		);
		assert_eq!(
			serde_json::from_str::<Line>(r#"{"type":"lagged","missed":3}"#).unwrap(),
			Line::Lagged { missed: 3 }
		);
	}

	#[test]
	fn reads_a_frame_without_the_gpu_video_queue_times() {
		// Moonshine omits both entirely when its GPU video queue reports no timestamps.
		let frame: Line = serde_json::from_str(
			r#"{"type":"frame","frame_number":1,"captured_at":1793750400.5,"channel_wait":0.000041,"import":0.000028,"convert":0.00019,"submit":0.000035,"consumer_queue":0.000012,"encode_wait":0.00163,"packetize":0.000022,"send":0.000048,"total":0.00201,"encoded_bytes":41666,"is_key_frame":true}"#,
		)
		.unwrap();
		let sample = to_sample(&frame, Instant::now());
		assert_eq!(sample.gpu_encode, None);
		assert_eq!(sample.bitstream_ready, None);
		assert!(sample.is_key_frame);
	}

	#[test]
	fn ignores_unknown_lines_and_fields() {
		assert_eq!(
			serde_json::from_str::<Line>(r#"{"type":"something_new","x":1}"#).unwrap(),
			Line::Other
		);
		let line: Line = serde_json::from_str(r#"{"type":"lagged","missed":1,"extra":true}"#).unwrap();
		assert_eq!(line, Line::Lagged { missed: 1 });
	}

	#[test]
	fn takes_the_frame_number_and_capture_time() {
		let captured = std::time::SystemTime::now() - Duration::from_millis(5);
		let secs = captured.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
		let frame: Line = serde_json::from_str(&format!(
			r#"{{"type":"frame","frame_number":42,"captured_at":{secs},"content":"scrgb_linear","channel_wait":0.000041,"import":0.000028,"convert":0.00019,"submit":0.000035,"consumer_queue":0.000012,"encode_wait":0.00163,"packetize":0.000022,"send":0.000048,"total":0.00201,"encoded_bytes":41666,"is_key_frame":false}}"#
		))
		.unwrap();
		let now = Instant::now();
		let sample = to_sample(&frame, now);
		assert_eq!(sample.seq, Some(42));
		assert_eq!(sample.content, Some("scrgb"));
		assert_eq!(sample.gpu_encode, None);
		assert_eq!(sample.bitstream_ready, None);
		let ago = now - sample.captured_at.unwrap();
		assert!(
			ago >= Duration::from_millis(5) && ago < Duration::from_millis(50),
			"{ago:?}"
		);
	}
}
