//! `info`: one `moonshine.test.info` sample (value 1) whose labels carry facts about a test run that no measurement
//! does: the machines, the code under test, the stream's settings. A dashboard reads the labels back as a table.

use clap::Args;
use opentelemetry::KeyValue;

use crate::{ExportArgs, parse_label};

#[derive(Args, Debug)]
pub struct InfoArgs {
	/// A fact, e.g. --label role=host --label gpu="AMD Radeon RX 6800 XT". Repeatable; empty values are left out.
	#[arg(long = "label", value_parser = parse_label)]
	labels: Vec<(String, String)>,
	#[command(flatten)]
	export: ExportArgs,
}

pub fn info(args: &InfoArgs) -> anyhow::Result<i32> {
	let telemetry = crate::setup_telemetry(&args.export, "moonshine-test-headless", Vec::new())?;
	let Some(meter) = telemetry.meter() else {
		tracing::warn!("no export; nothing to do");
		return Ok(0);
	};
	let attrs: Vec<KeyValue> = args
		.labels
		.iter()
		.filter(|(_, v)| !v.is_empty())
		.map(|(k, v)| KeyValue::new(k.clone(), v.clone()))
		.collect();
	meter
		.f64_gauge("moonshine.test.info")
		.with_description("Facts about a test run, as labels: the machines, the code under test, the stream's settings")
		.build()
		.record(1.0, &attrs);
	telemetry.flush()?;
	if let Err(e) = telemetry.shutdown() {
		tracing::warn!("{e:#}");
	}
	Ok(0)
}
