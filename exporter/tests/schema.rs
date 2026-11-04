//! Guards the result JSON schema. If this fails because you changed the schema on purpose,
//! bump `SCHEMA_VERSION` when the change is incompatible, then regenerate with
//! `UPDATE_SNAPSHOTS=1 cargo test -p moonshine-otel-exporter --test schema`.

use std::path::Path;
use std::time::{Duration, Instant};

use moonshine_otel_exporter::results::{IterationResult, ResultsFile, RunResult};
use moonshine_otel_exporter::{Deriver, FrameSample, RunConfig, RunIdentity, SummaryBuilder};

fn fixture() -> ResultsFile {
	let t0 = Instant::now();
	let mut deriver = Deriver::new(60);
	let mut builder = SummaryBuilder::new(60);
	for i in 0..120u64 {
		let us = Duration::from_micros;
		let s = FrameSample {
			channel_wait: us(40 + i % 7),
			import: us(30),
			convert: us(250 + i % 13),
			submit: us(120),
			consumer_queue: us(15),
			encode_wait: us(2400 + (i * 37) % 900),
			packetize: us(60),
			send: us(25),
			total: us(3000 + (i * 37) % 900),
			encoded_bytes: if i == 0 { 180_000 } else { 40_000 + i * 11 },
			is_key_frame: i == 0,
			..FrameSample::zeroed(t0 + Duration::from_micros(16_667 * i))
		};
		builder.record(&s, &deriver.derive(&s));
	}
	builder.record_warmup();
	let mut identity = RunIdentity::default();
	identity.build.profile = Some("release".into());
	identity.build.moonshine_rev = Some("5a83892a0c86a22e5d74d7a816b6df2f8e06f751".into());
	identity.gpu.model = Some("Example GPU".into());
	identity.labels.insert("branch".into(), "main".into());
	let config = RunConfig {
		command: vec!["vkcube".into()],
		width: 1920,
		height: 1080,
		target_fps: 60,
		codec: "h264".into(),
		bitrate: 20_000_000,
		hdr: false,
		chroma: "yuv420".into(),
		warmup_s: 4.0,
		duration_s: 2.0,
		iterations: 1,
	};
	let iteration = IterationResult::new(0, "2026-09-24T10:15:00Z".into(), builder.build());
	let run = RunResult::new(
		"20260924-101500-abcd".into(),
		"2026-09-24T10:15:00Z".into(),
		identity,
		config,
		vec![iteration],
		vec![],
	);
	let mut file = ResultsFile::new(vec![run]);
	file.created_at = "2026-09-24T10:15:10Z".into();
	file.tool_version = "0.0.0".into();
	file
}

#[test]
fn schema_snapshot() {
	let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/schema_v1.json");
	let actual = fixture().to_json() + "\n";
	if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
		std::fs::write(&path, &actual).unwrap();
	}
	let expected = std::fs::read_to_string(&path).expect("run with UPDATE_SNAPSHOTS=1 to create the snapshot");
	assert!(
		actual == expected,
		"result JSON changed; see the comment at the top of tests/schema.rs"
	);
}

#[test]
fn snapshot_round_trips() {
	let file = fixture();
	assert_eq!(ResultsFile::from_json(&file.to_json()).unwrap(), file);
}
