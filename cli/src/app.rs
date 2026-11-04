//! `app`: follows a test workload's log (clickflash's and hdrneon's `--log`, JSON lines with wall clock times) and
//! exports what the app saw as `moonshine.app.*` metrics, labeled like the client's (`stream_key` and the labels), so
//! a dashboard can set the app's view of a stream beside the client's and Moonshine's. The app decides what happened
//! (an input arrived, a flash went to the GPU, the content switched); this only carries it to OTel.

use std::path::PathBuf;

use std::time::{Duration, UNIX_EPOCH};

use clap::Args;
use moonshine_otel_exporter::frame_ids::FrameIds;
use opentelemetry::metrics::{Counter, Histogram, Meter};
use opentelemetry::trace::{Span, SpanContext, TraceContextExt, TraceFlags, TraceState, Tracer};
use opentelemetry::{Context, KeyValue};
use serde::Deserialize;

use crate::{ExportArgs, parse_label};

#[derive(Args, Debug)]
pub struct AppArgs {
	/// The workload's log (clickflash --log, hdrneon --log).
	log: PathBuf,
	/// The stream's key, as given to the client's reader and Moonshine's side.
	#[arg(long)]
	stream_key: String,
	/// A label on every metric, e.g. --label workload=clickflash. Repeatable.
	#[arg(long = "label", value_parser = parse_label)]
	labels: Vec<(String, String)>,
	/// Read the file to its end and exit, instead of following it until SIGTERM.
	#[arg(long)]
	once: bool,
	#[command(flatten)]
	export: ExportArgs,
}

/// One line of a workload's log.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Event {
	/// clickflash: an input reached the app (`click` or `key`).
	Input { kind: String },
	/// clickflash: the first white frame of a flash was submitted to the GPU at `unix_us`, this long after its input,
	/// with the click's id from the pointer's position when it has one.
	Flash {
		input_to_submit_us: u64,
		#[serde(default)]
		unix_us: u64,
		#[serde(default)]
		click_id: Option<u64>,
	},
	/// hdrneon: the mode it presents in, at start (`switch` false) and after each switch.
	Mode { mode: String, switch: bool },
	#[serde(other)]
	Other,
}

struct Metrics {
	inputs: Counter<u64>,
	flashes: Counter<u64>,
	flash: Histogram<f64>,
	switches: Counter<u64>,
}

impl Metrics {
	fn new(m: &Meter) -> Self {
		Metrics {
			inputs: m
				.u64_counter("moonshine.app.inputs")
				.with_description("Inputs that reached the workload, by kind")
				.build(),
			flashes: m
				.u64_counter("moonshine.app.flashes")
				.with_description("Flashes the workload drew, one per input it answered")
				.build(),
			flash: m
				.f64_histogram("moonshine.app.flash.duration")
				.with_unit("s")
				.with_description("From an input reaching the workload to its flash's first frame going to the GPU")
				.build(),
			switches: m
				.u64_counter("moonshine.app.content.switches")
				.with_description(
					"Times the workload switched what it presents (SDR, HDR10, scRGB), by the mode it switched to",
				)
				.build(),
		}
	}
}

pub async fn app(args: &AppArgs) -> anyhow::Result<i32> {
	let resource = vec![KeyValue::new("stream.key", args.stream_key.clone())];
	let telemetry = crate::setup_telemetry(&args.export, "moonshine-test-workload", resource)?;
	let Some(meter) = telemetry.meter() else {
		tracing::warn!("no export; nothing to do");
		return Ok(0);
	};
	let metrics = Metrics::new(&meter);
	let tracer = telemetry.span_tracer();
	let mut attrs = vec![KeyValue::new("stream_key", args.stream_key.clone())];
	attrs.extend(args.labels.iter().map(|(k, v)| KeyValue::new(k.clone(), v.clone())));
	let with = |extra: KeyValue| attrs.iter().cloned().chain([extra]).collect::<Vec<_>>();
	// Counters start at zero, so a stream with no switches says 0 rather than nothing.
	metrics.switches.add(0, &attrs);
	let lines = crate::follow::follow(&args.log, args.once, |text| match serde_json::from_str::<Event>(text) {
		Ok(Event::Input { kind }) => metrics.inputs.add(1, &with(KeyValue::new("kind", kind))),
		Ok(Event::Flash {
			input_to_submit_us,
			unix_us,
			click_id,
		}) => {
			metrics.flashes.add(1, &attrs);
			metrics.flash.record(input_to_submit_us as f64 / 1e6, &attrs);
			// The app's share of the click, in the click's trace (whose root the client's reader writes).
			if let (Some(tracer), Some(id), true) = (&tracer, click_id, unix_us > 0) {
				let ids = FrameIds::click(&args.stream_key, id);
				let parent = Context::new().with_remote_span_context(SpanContext::new(
					ids.trace_id,
					ids.span_id,
					TraceFlags::SAMPLED,
					true,
					TraceState::default(),
				));
				let at = |us: u64| UNIX_EPOCH + Duration::from_micros(us);
				tracer
					.span_builder("app: input to flash submitted")
					.with_start_time(at(unix_us.saturating_sub(input_to_submit_us)))
					.with_attributes([KeyValue::new("click.id", id as i64)])
					.start_with_context(tracer, &parent)
					.end_with_timestamp(at(unix_us));
			}
		},
		Ok(Event::Mode { mode, switch: true }) => metrics.switches.add(1, &with(KeyValue::new("mode", mode))),
		Ok(Event::Mode { .. } | Event::Other) => {},
		Err(e) => tracing::debug!("unreadable log line ({e}): {text}"),
	})
	.await?;
	tracing::info!("{lines} lines from {}", args.log.display());
	if let Err(e) = telemetry.flush() {
		tracing::warn!("{e:#}");
	}
	if let Err(e) = telemetry.shutdown() {
		tracing::warn!("{e:#}");
	}
	Ok(0)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reads_clickflash_and_hdrneon_logs() {
		let input: Event =
			serde_json::from_str(r#"{"event":"input","input":1,"kind":"click","t_us":5,"unix_us":9}"#).unwrap();
		assert_eq!(input, Event::Input { kind: "click".into() });
		let flash: Event = serde_json::from_str(
			r#"{"event":"flash","flash":1,"input":1,"t_us":86,"unix_us":90,"input_to_submit_us":81}"#,
		)
		.unwrap();
		assert_eq!(
			flash,
			Event::Flash {
				input_to_submit_us: 81,
				unix_us: 90,
				click_id: None
			}
		);
		let flash: Event = serde_json::from_str(
			r#"{"event":"flash","flash":2,"input":2,"click_id":130,"t_us":86,"unix_us":90,"input_to_submit_us":81}"#,
		)
		.unwrap();
		assert!(matches!(
			flash,
			Event::Flash {
				click_id: Some(130),
				..
			}
		));
		let mode: Event = serde_json::from_str(r#"{"event":"mode","mode":"scrgb","switch":true,"unix_us":3}"#).unwrap();
		assert_eq!(
			mode,
			Event::Mode {
				mode: "scrgb".into(),
				switch: true
			}
		);
		assert_eq!(
			serde_json::from_str::<Event>(r#"{"event":"new","x":1}"#).unwrap(),
			Event::Other
		);
	}
}
