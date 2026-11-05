//! A smaller copy of a spool, for attaching it somewhere with a size limit.
//!
//! Metrics are exported cumulatively at a fixed interval, so every batch repeats every series the
//! exporter has ever seen, long after a stream ended. The copy keeps a data point only when its value
//! differs from the same series' previous one. A series then has a sample wherever it changed, which
//! keeps whole-run queries (`last_over_time`, histogram quantiles) the same; only rates over windows
//! with no change lose their zero. Traces are copied as they are.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::metrics::v1::metric::Data;
use prost::Message;

/// What [`compact`] did.
#[derive(Debug, Default, Clone, Copy)]
pub struct CompactStats {
	pub files_in: usize,
	pub files_out: usize,
	pub bytes_in: u64,
	pub bytes_out: u64,
	pub points_in: usize,
	pub points_out: usize,
}

/// A series: its resource, scope, metric name and point attributes, as encoded bytes.
fn series_key(resource: &[KeyValue], scope: &str, metric: &str, attributes: &[KeyValue]) -> Vec<u8> {
	let mut key = Vec::new();
	for kv in resource {
		kv.encode(&mut key).ok();
	}
	key.push(0);
	key.extend_from_slice(scope.as_bytes());
	key.push(0);
	key.extend_from_slice(metric.as_bytes());
	key.push(0);
	for kv in attributes {
		kv.encode(&mut key).ok();
	}
	key
}

/// Keep the points of `points` whose value (the point without its times and exemplars) changed since
/// the series' last kept one.
fn keep_changed<P: Message + Clone>(
	last: &mut HashMap<Vec<u8>, Vec<u8>>,
	points: &mut Vec<P>,
	key_of: impl Fn(&P) -> Vec<u8>,
	value_of: impl Fn(&P) -> P,
) {
	points.retain(|p| {
		let value = value_of(p).encode_to_vec();
		let key = key_of(p);
		if last.get(&key) == Some(&value) {
			return false;
		}
		last.insert(key, value);
		true
	});
}

/// Write a compacted copy of the spool `from` into `to` (created if missing).
pub fn compact(from: &Path, to: &Path) -> anyhow::Result<CompactStats> {
	std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
	let mut names: Vec<String> = std::fs::read_dir(from)
		.with_context(|| format!("reading {}", from.display()))?
		.filter_map(Result::ok)
		.filter_map(|e| e.file_name().into_string().ok())
		.filter(|n| n.ends_with(".pb"))
		.collect();
	// Spool names start with the time they were written: name order is time order.
	names.sort_unstable();

	let mut stats = CompactStats::default();
	let mut last: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
	for name in names {
		let body = std::fs::read(from.join(&name)).with_context(|| format!("reading {name}"))?;
		stats.files_in += 1;
		stats.bytes_in += body.len() as u64;
		let out = if name.ends_with(".metrics.pb") {
			let mut request = ExportMetricsServiceRequest::decode(body.as_slice())
				.with_context(|| format!("{name} isn't an OTLP metrics request"))?;
			for rm in &mut request.resource_metrics {
				let resource = rm.resource.as_ref().map(|r| r.attributes.clone()).unwrap_or_default();
				for sm in &mut rm.scope_metrics {
					let scope = sm.scope.as_ref().map(|s| s.name.clone()).unwrap_or_default();
					for m in &mut sm.metrics {
						let key = |a: &[KeyValue]| series_key(&resource, &scope, &m.name, a);
						match &mut m.data {
							Some(Data::Gauge(g)) => {
								stats.points_in += g.data_points.len();
								keep_changed(
									&mut last,
									&mut g.data_points,
									|p| key(&p.attributes),
									|p| {
										let mut v = p.clone();
										(v.time_unix_nano, v.start_time_unix_nano) = (0, 0);
										v.exemplars.clear();
										v
									},
								);
								stats.points_out += g.data_points.len();
							},
							Some(Data::Sum(s)) => {
								stats.points_in += s.data_points.len();
								keep_changed(
									&mut last,
									&mut s.data_points,
									|p| key(&p.attributes),
									|p| {
										let mut v = p.clone();
										(v.time_unix_nano, v.start_time_unix_nano) = (0, 0);
										v.exemplars.clear();
										v
									},
								);
								stats.points_out += s.data_points.len();
							},
							Some(Data::Histogram(h)) => {
								stats.points_in += h.data_points.len();
								keep_changed(
									&mut last,
									&mut h.data_points,
									|p| key(&p.attributes),
									|p| {
										let mut v = p.clone();
										(v.time_unix_nano, v.start_time_unix_nano) = (0, 0);
										v.exemplars.clear();
										v
									},
								);
								stats.points_out += h.data_points.len();
							},
							Some(Data::ExponentialHistogram(h)) => {
								stats.points_in += h.data_points.len();
								keep_changed(
									&mut last,
									&mut h.data_points,
									|p| key(&p.attributes),
									|p| {
										let mut v = p.clone();
										(v.time_unix_nano, v.start_time_unix_nano) = (0, 0);
										v.exemplars.clear();
										v
									},
								);
								stats.points_out += h.data_points.len();
							},
							Some(Data::Summary(s)) => {
								stats.points_in += s.data_points.len();
								keep_changed(
									&mut last,
									&mut s.data_points,
									|p| key(&p.attributes),
									|p| {
										let mut v = p.clone();
										(v.time_unix_nano, v.start_time_unix_nano) = (0, 0);
										v
									},
								);
								stats.points_out += s.data_points.len();
							},
							None => {},
						}
					}
					sm.metrics.retain(|m| match &m.data {
						Some(Data::Gauge(g)) => !g.data_points.is_empty(),
						Some(Data::Sum(s)) => !s.data_points.is_empty(),
						Some(Data::Histogram(h)) => !h.data_points.is_empty(),
						Some(Data::ExponentialHistogram(h)) => !h.data_points.is_empty(),
						Some(Data::Summary(s)) => !s.data_points.is_empty(),
						None => false,
					});
				}
				rm.scope_metrics.retain(|sm| !sm.metrics.is_empty());
			}
			request.resource_metrics.retain(|rm| !rm.scope_metrics.is_empty());
			(!request.resource_metrics.is_empty()).then(|| request.encode_to_vec())
		} else {
			Some(body)
		};
		if let Some(out) = out {
			std::fs::write(to.join(&name), &out).with_context(|| format!("writing {name}"))?;
			stats.files_out += 1;
			stats.bytes_out += out.len() as u64;
		}
	}
	Ok(stats)
}
