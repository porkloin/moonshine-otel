//! `client`: follows a test client's statistics (gamestream-test-client `--stats`, JSON lines with wall clock times)
//! and exports them as it reads: each frame's spans go into the frame's trace (the trace and parent ids come from the
//! stream key and the frame number, see `moonshine_otel_exporter::frame_ids`, the same ones `watch
//! --stream-key-file` gives Moonshine's side of the frame), and the client's view of the stream goes out as
//! `moonshine.client.*` metrics. The client decides what happened (a freeze, a flash, a dropped frame); this only
//! carries it to OTel.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Args;
use moonshine_otel_exporter::frame_ids::FrameIds;
use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter};
use opentelemetry::trace::{Span, SpanContext, TraceContextExt, TraceFlags, TraceState, Tracer};
use opentelemetry::{Context, KeyValue};
use serde::Deserialize;

use crate::{ExportArgs, parse_label};

#[derive(Args, Debug)]
pub struct ClientArgs {
	/// The client's statistics file (gamestream-test-client --stats).
	stats: PathBuf,
	/// The stream's key, the same one Moonshine's side was given (watch --stream-key-file).
	#[arg(long)]
	stream_key: String,
	/// A label on every metric, e.g. --label workload=vkcube. Repeatable.
	#[arg(long = "label", value_parser = parse_label)]
	labels: Vec<(String, String)>,
	/// Leave the stream's first SECONDS (from its first frame) out of the metrics, e.g. while a game loads; their spans
	/// are still sent, marked `warmup`.
	#[arg(long, default_value_t = 0.0)]
	warmup: f64,
	/// Read the file to its end and exit, instead of following it until SIGTERM.
	#[arg(long)]
	once: bool,
	#[command(flatten)]
	export: ExportArgs,
}

/// One line of the client's statistics.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Line {
	/// A frame handed to the decoder.
	Frame {
		frame_number: u64,
		frame_type: i64,
		bytes: i64,
		receive_unix_us: u64,
		enqueue_unix_us: u64,
		decode_start_unix_us: u64,
		decode_end_unix_us: u64,
		/// 0 decoded, negative a decode error, positive decoded corrupt.
		decode_status: i64,
		recovery: bool,
	},
	/// The client's loss model started dropping this frame on purpose.
	Drop { frame_number: u64, unix_us: u64 },
	/// Frames never reached the decoder: the picture stood still from `after_frame` until `frame_number`.
	Freeze {
		after_frame: i64,
		frame_number: u64,
		missing: u64,
		duration_us: u64,
		/// `recovery_frame`, `keyframe` or `neither`.
		recovered_by: String,
		unix_us: u64,
	},
	/// The client's measurement of the host's clock (NTP to the harness), before or after the stream. The client's
	/// times are on the host's clock, shifted by `applied_offset_us`, when the start one succeeded.
	Clock {
		/// `start` or `end`.
		when: String,
		server: String,
		ok: bool,
		#[serde(default)]
		offset_us: i64,
		#[serde(default)]
		uncertainty_us: i64,
		#[serde(default)]
		applied_offset_us: i64,
	},
	/// A second of the stream: its bytes, and the busiest sliding one-second window so far.
	Second {
		bytes: u64,
		peak_window_bytes: u64,
		requested_kbps: u64,
	},
	/// A click and the flash it caused.
	Click {
		/// The click's id, which the app reads back from the pointer's position; absent from older clients.
		#[serde(default)]
		click: Option<u64>,
		/// The frame the flash was decoded from.
		#[serde(default)]
		flash_frame: Option<u64>,
		sent_unix_us: u64,
		decoded_unix_us: u64,
		/// 0 when unknown.
		received_unix_us: u64,
	},
	/// The client's own build, the file's first line.
	Client { version: String },
	/// What a display would have done with a decoded frame, written once the stream ends: how long after it was
	/// decoded it went on screen with V-Sync and with moonlight-qt's frame pacer (none: dropped, never shown), and
	/// whether it went up a refresh or more later than the stream's rate needs (the frame before it held).
	Display {
		frame_number: u64,
		decoded_unix_us: u64,
		vsync_wait_us: Option<u64>,
		vsync_judder: bool,
		paced_wait_us: Option<u64>,
		paced_judder: bool,
	},
	/// The display the frames were replayed on.
	DisplaySummary { hz: u64 },
	#[serde(other)]
	Other,
}

pub async fn client(args: &ClientArgs) -> anyhow::Result<i32> {
	let resource = vec![KeyValue::new("stream.key", args.stream_key.clone())];
	let telemetry = crate::setup_telemetry(&args.export, "gamestream-test-client", resource)?;
	let mut attrs = vec![KeyValue::new("stream_key", args.stream_key.clone())];
	attrs.extend(args.labels.iter().map(|(k, v)| KeyValue::new(k.clone(), v.clone())));
	let mut out = Exporter {
		tracer: telemetry.span_tracer(),
		metrics: telemetry.meter().map(|m| Metrics::new(&m)),
		key: args.stream_key.clone(),
		attrs,
		last_receive_us: None,
		first_receive_us: None,
		latest_receive_us: 0,
		warmup_us: (args.warmup * 1e6) as u64,
		clock: vec![KeyValue::new("clock.alignment", "unaligned: the client's own clock")],
		version: None,
	};
	if out.tracer.is_none() && out.metrics.is_none() {
		tracing::warn!("no export; nothing to do");
		return Ok(0);
	}

	let lines = crate::follow::follow(&args.stats, args.once, |text| {
		match serde_json::from_str::<Line>(text) {
			Ok(l) => out.line(&l),
			Err(e) => tracing::debug!("unreadable statistics line ({e}): {text}"),
		}
	})
	.await?;
	tracing::info!("{lines} lines from {}", args.stats.display());
	if let Err(e) = telemetry.flush() {
		tracing::warn!("{e:#}");
	}
	if let Err(e) = telemetry.shutdown() {
		tracing::warn!("{e:#}");
	}
	Ok(0)
}

/// The stream as the client saw it.
struct Metrics {
	frames: Counter<u64>,
	interval: Histogram<f64>,
	decode: Histogram<f64>,
	missing: Counter<u64>,
	freeze: Histogram<f64>,
	dropped: Counter<u64>,
	click: Histogram<f64>,
	clock_offset: Gauge<f64>,
	clock_uncertainty: Gauge<f64>,
	bitrate: Histogram<f64>,
	bitrate_peak: Gauge<f64>,
	bitrate_requested: Gauge<f64>,
	stream_start: Gauge<f64>,
	stream_end: Gauge<f64>,
	display_latency: Histogram<f64>,
	display_frames: Counter<u64>,
	display_judder: Counter<u64>,
	display_refresh: Gauge<u64>,
}

impl Metrics {
	fn new(m: &Meter) -> Self {
		Metrics {
			frames: m
				.u64_counter("moonshine.client.frames")
				.with_description("Frames handed to the client's decoder, by type and how they decoded")
				.build(),
			interval: m
				.f64_histogram("moonshine.client.frame.interval")
				.with_unit("s")
				.with_description("Time between frames' first packets arriving at the client")
				.build(),
			decode: m
				.f64_histogram("moonshine.client.decode.duration")
				.with_unit("s")
				.with_description("Time the client took to decode a frame")
				.build(),
			missing: m
				.u64_counter("moonshine.client.frames.missing")
				.with_description("Frames that never reached the client's decoder")
				.build(),
			freeze: m
				.f64_histogram("moonshine.client.freeze.duration")
				.with_unit("s")
				.with_description("How long the picture stood still over missing frames, by what recovered it")
				.build(),
			dropped: m
				.u64_counter("moonshine.client.frames.dropped")
				.with_description("Frames the client's loss model dropped on purpose")
				.build(),
			click: m
				.f64_histogram("moonshine.client.click.duration")
				.with_unit("s")
				.with_description("From the client sending a click to it decoding the flash the click caused")
				.build(),
			clock_offset: m
				.f64_gauge("moonshine.client.clock.offset")
				.with_unit("s")
				.with_description("The host's clock less the client's, measured by NTP before and after the stream")
				.build(),
			clock_uncertainty: m
				.f64_gauge("moonshine.client.clock.uncertainty")
				.with_unit("s")
				.with_description("How far the true clock offset can be from the measured one: half the NTP round trip")
				.build(),
			bitrate: m
				.f64_histogram("moonshine.client.bitrate")
				.with_unit("bit/s")
				.with_description("Bits the client received in each second of the stream")
				.build(),
			bitrate_peak: m
				.f64_gauge("moonshine.client.bitrate.peak")
				.with_unit("bit/s")
				.with_description(
					"The busiest sliding one-second window of the stream so far, as the client received it",
				)
				.build(),
			bitrate_requested: m
				.f64_gauge("moonshine.client.bitrate.requested")
				.with_unit("bit/s")
				.with_description("The bitrate the client asked for")
				.build(),
			stream_start: m
				.f64_gauge("moonshine.client.stream.start")
				.with_unit("s")
				.with_description("When the stream's first frame reached the client, seconds since the Unix epoch")
				.build(),
			stream_end: m
				.f64_gauge("moonshine.client.stream.end")
				.with_unit("s")
				.with_description("When the stream's latest frame reached the client, seconds since the Unix epoch")
				.build(),
			display_latency: m
				.f64_histogram("moonshine.client.display.latency")
				.with_unit("s")
				.with_description(
					"From a frame decoding to a display showing it, by how it's presented (mode: vsync or paced), in the \
					 client's model of moonlight-qt's presenting",
				)
				.build(),
			display_frames: m
				.u64_counter("moonshine.client.display.frames")
				.with_description(
					"Decoded frames a display showed or dropped (shown: true or false), by how they're presented (mode)",
				)
				.build(),
			display_judder: m
				.u64_counter("moonshine.client.display.judder")
				.with_description(
					"Frames that went on screen a refresh or more later than the stream's rate needs, the frame before \
					 them held too long, by how they're presented (mode)",
				)
				.build(),
			display_refresh: m
				.u64_gauge("moonshine.client.display.refresh_rate")
				.with_unit("Hz")
				.with_description("The refresh rate of the display the client modelled")
				.build(),
		}
	}
}

struct Exporter<T> {
	tracer: Option<T>,
	metrics: Option<Metrics>,
	key: String,
	/// On every metric: the stream key and the labels.
	attrs: Vec<KeyValue>,
	/// The latest frame in the metrics, for the next one's interval.
	last_receive_us: Option<u64>,
	/// The stream's first frame and latest frame, counted or not.
	first_receive_us: Option<u64>,
	latest_receive_us: u64,
	/// How long from the first frame the metrics leave out.
	warmup_us: u64,
	/// How the client's times line up with the host's, on every client span.
	clock: Vec<KeyValue>,
	/// The client's build, on its frame spans.
	version: Option<KeyValue>,
}

fn at(us: u64) -> SystemTime {
	UNIX_EPOCH + Duration::from_micros(us)
}

fn secs(us: u64) -> f64 {
	us as f64 / 1e6
}

impl<T> Exporter<T>
where
	T: Tracer,
	T::Span: Send + Sync + 'static,
{
	/// The metrics, unless `unix_us` is in the warmup.
	fn metrics_at(&self, unix_us: u64) -> Option<&Metrics> {
		let counted = match self.first_receive_us {
			Some(first) => unix_us >= first + self.warmup_us,
			None => self.warmup_us == 0,
		};
		self.metrics.as_ref().filter(|_| counted)
	}

	fn with(&self, extra: &[KeyValue]) -> Vec<KeyValue> {
		self.attrs.iter().chain(extra).cloned().collect()
	}

	/// The context of Moonshine's `frame` span for a frame, to parent the client's spans to.
	fn frame_parent(&self, frame_number: u64) -> Context {
		let ids = FrameIds::new(&self.key, frame_number);
		let host = SpanContext::new(
			ids.trace_id,
			ids.span_id,
			TraceFlags::SAMPLED,
			true,
			TraceState::default(),
		);
		Context::new().with_remote_span_context(host)
	}

	fn line(&mut self, line: &Line) {
		match *line {
			Line::Frame {
				frame_number,
				frame_type,
				bytes,
				receive_unix_us,
				enqueue_unix_us,
				decode_start_unix_us,
				decode_end_unix_us,
				decode_status,
				recovery,
			} => {
				self.first_receive_us.get_or_insert(receive_unix_us);
				self.latest_receive_us = receive_unix_us;
				let counted = self.metrics_at(receive_unix_us).is_some();
				if let Some(m) = self.metrics_at(receive_unix_us) {
					let kind = if frame_type == 1 { "key" } else { "delta" };
					let result = match decode_status {
						0 => "ok",
						s if s < 0 => "decode_error",
						_ => "corrupt",
					};
					m.frames.add(
						1,
						&self.with(&[KeyValue::new("frame_kind", kind), KeyValue::new("result", result)]),
					);
					m.decode.record(
						secs(decode_end_unix_us.saturating_sub(decode_start_unix_us)),
						&self.attrs,
					);
					match self.last_receive_us {
						Some(last) => m
							.interval
							.record(secs(receive_unix_us.saturating_sub(last)), &self.attrs),
						None => m.stream_start.record(secs(receive_unix_us), &self.attrs),
					}
					m.stream_end.record(secs(receive_unix_us), &self.attrs);
				}
				if counted {
					self.last_receive_us = Some(receive_unix_us);
				}
				let Some(tracer) = &self.tracer else { return };
				let parent = self.frame_parent(frame_number);
				let mut attrs = vec![
					KeyValue::new("frame.number", frame_number as i64),
					KeyValue::new("frame.type", frame_type),
					KeyValue::new("frame.bytes", bytes),
					KeyValue::new("frame.decode_status", decode_status),
					KeyValue::new("frame.recovery", recovery),
				];
				if !counted && self.warmup_us > 0 {
					attrs.push(KeyValue::new("warmup", true));
				}
				attrs.extend(self.clock.iter().cloned());
				attrs.extend(self.version.clone());
				let span = tracer
					.span_builder("client frame")
					.with_start_time(at(receive_unix_us))
					.with_attributes(attrs)
					.start_with_context(tracer, &parent);
				let cx = parent.with_span(span);
				for (name, start, end) in [
					("client: reassemble", receive_unix_us, enqueue_unix_us),
					("client: wait for decoder", enqueue_unix_us, decode_start_unix_us),
					("client: decode", decode_start_unix_us, decode_end_unix_us),
				] {
					tracer
						.span_builder(name)
						.with_start_time(at(start))
						.start_with_context(tracer, &cx)
						.end_with_timestamp(at(end.max(start)));
				}
				cx.span()
					.end_with_timestamp(at(decode_end_unix_us.max(receive_unix_us)));
			},
			Line::Drop { frame_number, unix_us } => {
				if let Some(m) = self.metrics_at(unix_us) {
					m.dropped.add(1, &self.attrs);
				}
				if let Some(tracer) = &self.tracer {
					tracer
						.span_builder("client: dropped on purpose")
						.with_start_time(at(unix_us))
						.with_attributes([KeyValue::new("frame.number", frame_number as i64)])
						.start_with_context(tracer, &self.frame_parent(frame_number))
						.end_with_timestamp(at(unix_us));
				}
			},
			Line::Freeze {
				after_frame,
				frame_number,
				missing,
				duration_us,
				ref recovered_by,
				unix_us,
			} => {
				if let Some(m) = self.metrics_at(unix_us) {
					m.missing.add(missing, &self.attrs);
					m.freeze.record(
						secs(duration_us),
						&self.with(&[KeyValue::new("recovered_by", recovered_by.clone())]),
					);
				}
				// In the trace of the frame that ended it.
				if let Some(tracer) = &self.tracer {
					tracer
						.span_builder("client: freeze")
						.with_start_time(at(unix_us.saturating_sub(duration_us)))
						.with_attributes([
							KeyValue::new("freeze.after_frame", after_frame),
							KeyValue::new("freeze.missing", missing as i64),
							KeyValue::new("freeze.recovered_by", recovered_by.clone()),
						])
						.start_with_context(tracer, &self.frame_parent(frame_number))
						.end_with_timestamp(at(unix_us));
				}
			},
			Line::Clock {
				ref when,
				ref server,
				ok,
				offset_us,
				uncertainty_us,
				applied_offset_us,
			} => {
				if !ok {
					tracing::warn!("the client couldn't measure the host's clock at {server} ({when})");
					return;
				}
				if let Some(m) = &self.metrics {
					let attrs = self.with(&[KeyValue::new("when", when.clone())]);
					m.clock_offset.record(offset_us as f64 / 1e6, &attrs);
					m.clock_uncertainty.record(uncertainty_us as f64 / 1e6, &attrs);
				}
				if when == "start" {
					self.clock = vec![
						KeyValue::new("clock.alignment", format!("host's clock, by NTP to {server}")),
						KeyValue::new("clock.offset_us", applied_offset_us),
						KeyValue::new("clock.uncertainty_us", uncertainty_us),
					];
				} else {
					tracing::info!(
						"clocks drifted {} µs during the stream (measured ±{uncertainty_us} µs)",
						offset_us - applied_offset_us
					);
				}
			},
			Line::Second {
				bytes,
				peak_window_bytes,
				requested_kbps,
			} => {
				if let Some(m) = self.metrics_at(self.latest_receive_us) {
					m.bitrate.record(bytes as f64 * 8.0, &self.attrs);
					m.bitrate_peak.record(peak_window_bytes as f64 * 8.0, &self.attrs);
					m.bitrate_requested.record(requested_kbps as f64 * 1000.0, &self.attrs);
				}
			},
			Line::Click {
				click,
				flash_frame,
				sent_unix_us,
				decoded_unix_us,
				received_unix_us,
			} => {
				if let Some(m) = self.metrics_at(sent_unix_us) {
					m.click
						.record(secs(decoded_unix_us.saturating_sub(sent_unix_us)), &self.attrs);
				}
				let (Some(tracer), Some(id)) = (&self.tracer, click) else {
					return;
				};
				// The click's own trace: from sending it to decoding its flash, the app's share under it (from the
				// app's log), and a link to the trace of the frame the flash was in (Moonshine's capture to send, the
				// network, the client's decode).
				let mut attrs = vec![KeyValue::new("click.id", id as i64)];
				attrs.extend(flash_frame.map(|f| KeyValue::new("click.flash_frame", f as i64)));
				attrs.extend(self.clock.iter().cloned());
				let links = flash_frame
					.map(|f| {
						let ids = FrameIds::new(&self.key, f);
						vec![opentelemetry::trace::Link::with_context(SpanContext::new(
							ids.trace_id,
							ids.span_id,
							TraceFlags::SAMPLED,
							true,
							TraceState::default(),
						))]
					})
					.unwrap_or_default();
				let span = moonshine_otel_exporter::frame_ids::with_ids(FrameIds::click(&self.key, id), || {
					tracer
						.span_builder("click")
						.with_start_time(at(sent_unix_us))
						.with_attributes(attrs)
						.with_links(links)
						.start_with_context(tracer, &Context::new())
				});
				let cx = Context::new().with_span(span);
				if received_unix_us > 0 {
					tracer
						.span_builder("client: flash arrives to decoded")
						.with_start_time(at(received_unix_us))
						.start_with_context(tracer, &cx)
						.end_with_timestamp(at(decoded_unix_us.max(received_unix_us)));
				}
				cx.span().end_with_timestamp(at(decoded_unix_us.max(sent_unix_us)));
			},
			Line::Client { ref version } => {
				tracing::info!("gamestream-test-client {version}");
				self.version = Some(KeyValue::new("client.version", version.clone()));
			},
			Line::Display {
				frame_number,
				decoded_unix_us,
				vsync_wait_us,
				vsync_judder,
				paced_wait_us,
				paced_judder,
			} => {
				if let Some(m) = self.metrics_at(decoded_unix_us) {
					for (mode, wait, judder) in [
						("vsync", vsync_wait_us, vsync_judder),
						("paced", paced_wait_us, paced_judder),
					] {
						let mode = KeyValue::new("mode", mode);
						m.display_frames
							.add(1, &self.with(&[mode.clone(), KeyValue::new("shown", wait.is_some())]));
						if let Some(wait) = wait {
							m.display_latency
								.record(secs(wait), &self.with(std::slice::from_ref(&mode)));
						}
						if judder {
							m.display_judder.add(1, &self.with(&[mode]));
						}
					}
				}
				// One span per frame, V-Sync's (moonlight-qt's default), with the pacer's fate on it.
				let Some(tracer) = &self.tracer else { return };
				let mut attrs = vec![
					KeyValue::new("frame.number", frame_number as i64),
					KeyValue::new("display.shown", vsync_wait_us.is_some()),
					KeyValue::new("display.judder", vsync_judder),
					KeyValue::new("display.paced_shown", paced_wait_us.is_some()),
					KeyValue::new("display.paced_judder", paced_judder),
				];
				attrs.extend(paced_wait_us.map(|w| KeyValue::new("display.paced_wait_us", w as i64)));
				let name = if vsync_wait_us.is_some() {
					"client: decoded to on screen"
				} else {
					"client: dropped by the display"
				};
				tracer
					.span_builder(name)
					.with_start_time(at(decoded_unix_us))
					.with_attributes(attrs)
					.start_with_context(tracer, &self.frame_parent(frame_number))
					.end_with_timestamp(at(decoded_unix_us + vsync_wait_us.unwrap_or(0)));
			},
			Line::DisplaySummary { hz } => {
				if let Some(m) = &self.metrics {
					m.display_refresh.record(hz, &self.attrs);
				}
			},
			Line::Other => {},
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reads_the_clients_lines() {
		let line: Line = serde_json::from_str(
			r#"{"type":"frame","frame_number":2,"frame_type":0,"bytes":31640,"receive_unix_us":10,"enqueue_unix_us":20,"decode_start_unix_us":30,"decode_end_unix_us":40,"decode_status":0,"recovery":false}"#,
		)
		.unwrap();
		assert!(matches!(
			line,
			Line::Frame {
				frame_number: 2,
				decode_end_unix_us: 40,
				..
			}
		));
		let freeze: Line = serde_json::from_str(
			r#"{"type":"freeze","after_frame":120,"frame_number":124,"missing":3,"duration_us":66000,"recovered_by":"keyframe","unix_us":5}"#,
		)
		.unwrap();
		assert!(matches!(
			freeze,
			Line::Freeze {
				missing: 3,
				duration_us: 66000,
				..
			}
		));
		let drop: Line = serde_json::from_str(r#"{"type":"drop","frame_number":121,"unix_us":4}"#).unwrap();
		assert_eq!(
			drop,
			Line::Drop {
				frame_number: 121,
				unix_us: 4
			}
		);
		let clock: Line = serde_json::from_str(
			r#"{"type":"clock","when":"start","server":"10.0.0.1:58011","ok":true,"offset_us":-3812,"uncertainty_us":240,"samples":16,"applied_offset_us":0}"#,
		)
		.unwrap();
		assert!(matches!(
			clock,
			Line::Clock {
				ok: true,
				offset_us: -3812,
				uncertainty_us: 240,
				..
			}
		));
		let failed: Line = serde_json::from_str(r#"{"type":"clock","when":"end","server":"h:1","ok":false}"#).unwrap();
		assert!(matches!(failed, Line::Clock { ok: false, .. }));
		let second: Line = serde_json::from_str(
			r#"{"type":"second","bytes":2500000,"frames":60,"peak_window_bytes":2600000,"requested_kbps":20000,"unix_us":7}"#,
		)
		.unwrap();
		assert!(matches!(
			second,
			Line::Second {
				bytes: 2_500_000,
				requested_kbps: 20000,
				..
			}
		));
		let click: Line =
			serde_json::from_str(r#"{"type":"click","sent_unix_us":1,"decoded_unix_us":9,"received_unix_us":0}"#)
				.unwrap();
		assert!(matches!(click, Line::Click { decoded_unix_us: 9, .. }));
		assert_eq!(
			serde_json::from_str::<Line>(r#"{"type":"client","version":"96bfc47"}"#).unwrap(),
			Line::Client {
				version: "96bfc47".into()
			}
		);
		assert_eq!(
			serde_json::from_str::<Line>(
				r#"{"type":"display","frame_number":100,"decoded_unix_us":7,"vsync_wait_us":6500,"vsync_judder":true,"paced_wait_us":null,"paced_judder":false}"#
			)
			.unwrap(),
			Line::Display {
				frame_number: 100,
				decoded_unix_us: 7,
				vsync_wait_us: Some(6500),
				vsync_judder: true,
				paced_wait_us: None,
				paced_judder: false
			}
		);
		assert_eq!(
			serde_json::from_str::<Line>(
				r#"{"type":"display_summary","hz":120,"fps":120,"vsync":{"shown":2399,"dropped":1,"judder":1,"latency_ms_p50":4.17,"latency_ms_p99":12.5},"paced":{"shown":2400,"dropped":0,"judder":0,"latency_ms_p50":12.5,"latency_ms_p99":12.5},"unix_us":9}"#
			)
			.unwrap(),
			Line::DisplaySummary { hz: 120 }
		);
		assert_eq!(
			serde_json::from_str::<Line>(r#"{"type":"something_new","x":1}"#).unwrap(),
			Line::Other
		);
	}
}
