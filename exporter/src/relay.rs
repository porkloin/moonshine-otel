//! Forward a [spool](crate::spool) to an OTLP/gRPC collector.
//!
//! The relay is the only thing that crosses from a sandboxed server to the collector, so it
//! treats the spool as untrusted input. Whatever runs in the sandbox can write any file there.
//! - Files are opened relative to the spool directory, never following a symlink, and only
//!   regular files under a size cap are read. Names must match the writer's pattern.
//! - The relay only reads. The writer deletes old files, so the relay needs no write access.
//! - Every batch is decoded and rebuilt from allow-listed parts: only the `moonshine-otel`
//!   instrumentation scope, only `moonshine.*` metrics, scalar attribute values only, with
//!   bounded counts and lengths. Exemplars, links and resource entity refs are dropped.
//! - A byte budget per minute bounds how much a writer can push through.
//! - Logs report counts and file names only, never contents.

use std::ffi::CStr;
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::metrics_service_client::MetricsServiceClient;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::trace_service_client::TraceServiceClient;
use opentelemetry_proto::tonic::common::v1::{AnyValue, InstrumentationScope, KeyValue, any_value};
use opentelemetry_proto::tonic::metrics::v1::metric;
use opentelemetry_proto::tonic::resource::v1::Resource;
use prost::Message;
use rustix::fs::{Dir, FileType, Mode, OFlags};
use tonic::transport::Channel;

use crate::spool::BatchKind;

/// The instrumentation scope the bench records under; everything else is dropped.
pub const SCOPE: &str = "moonshine-otel";
/// Resource attribute the relay sets on everything it forwards, naming its spool.
pub const SOURCE_ATTRIBUTE: &str = "moonshine_otel.relay.source";

const MAX_KEY_LEN: usize = 128;
const MAX_STRING_LEN: usize = 256;
const MAX_RESOURCE_ATTRIBUTES: usize = 64;
const MAX_POINT_ATTRIBUTES: usize = 16;
/// `bench.run` spans carry the whole summary.
const MAX_SPAN_ATTRIBUTES: usize = 256;
const MAX_SPAN_EVENTS: usize = 16;
const MAX_EVENT_ATTRIBUTES: usize = 32;
/// How often the running totals are logged, when they change.
const STATS_EVERY: Duration = Duration::from_secs(60);
/// Names from further in the future than this are ignored: the cursor must not jump ahead.
const MAX_CLOCK_SKEW: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub struct RelayConfig {
	/// The spool directory. It isn't followed if it's a symlink, but nothing above its parent may be
	/// writable from the sandbox.
	pub spool: PathBuf,
	/// OTLP/gRPC endpoint of the collector.
	pub endpoint: String,
	/// Value of [`SOURCE_ATTRIBUTE`].
	pub source: String,
	pub poll_interval: Duration,
	/// Larger files are skipped.
	pub max_file_bytes: u64,
	/// Spool bytes forwarded per minute; files past it are skipped.
	pub max_bytes_per_minute: u64,
}

/// What happened to the files the relay looked at, since it started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelayStats {
	pub forwarded: u64,
	/// Unreadable, oversized, not a regular file, or not valid OTLP.
	pub rejected: u64,
	/// Over the byte budget.
	pub over_budget: u64,
}

pub struct Relay {
	config: RelayConfig,
	metrics: MetricsServiceClient<Channel>,
	traces: TraceServiceClient<Channel>,
	/// The last file handled. Files sort in write order, so later ones are new.
	cursor: Option<String>,
	budget_window: Instant,
	budget_used: u64,
	stats: RelayStats,
}

impl Relay {
	/// Connects lazily: an unreachable collector shows up on the first forward, and is retried.
	pub fn new(config: RelayConfig) -> anyhow::Result<Self> {
		let channel = Channel::from_shared(config.endpoint.clone())
			.with_context(|| format!("invalid OTLP endpoint {}", config.endpoint))?
			.connect_timeout(Duration::from_secs(5))
			.timeout(Duration::from_secs(10))
			.connect_lazy();
		Ok(Self {
			metrics: MetricsServiceClient::new(channel.clone()),
			traces: TraceServiceClient::new(channel),
			config,
			cursor: None,
			budget_window: Instant::now(),
			budget_used: 0,
			stats: RelayStats::default(),
		})
	}

	pub fn stats(&self) -> RelayStats {
		self.stats
	}

	/// Poll until `stop` resolves.
	pub async fn run(&mut self, stop: impl std::future::Future<Output = ()>) {
		tokio::pin!(stop);
		let mut tick = tokio::time::interval(self.config.poll_interval);
		tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
		let mut reported = self.stats;
		let mut reported_at = Instant::now();
		let mut last_error: Option<String> = None;
		loop {
			tokio::select! {
				_ = &mut stop => break,
				_ = tick.tick() => {},
			}
			// Each distinct error once, not every poll (a missing spool, a collector that's down).
			match self.poll().await {
				Ok(()) => {
					if last_error.take().is_some() {
						tracing::info!("recovered");
					}
				},
				Err(e) => {
					let message = format!("{e:#}");
					if last_error.as_ref() != Some(&message) {
						tracing::warn!("{message}");
						last_error = Some(message);
					}
				},
			}
			if self.stats != reported && reported_at.elapsed() >= STATS_EVERY {
				let RelayStats {
					forwarded,
					rejected,
					over_budget,
				} = self.stats;
				tracing::info!("forwarded {forwarded}, rejected {rejected}, over budget {over_budget}");
				reported = self.stats;
				reported_at = Instant::now();
			}
		}
	}

	/// Forward every new file once. Stops at the first export failure, leaving that file for the
	/// next poll.
	pub async fn poll(&mut self) -> anyhow::Result<()> {
		let dir = rustix::fs::open(
			&self.config.spool,
			OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
			Mode::empty(),
		)
		.with_context(|| format!("opening spool {}", self.config.spool.display()))?;
		let mut names: Vec<String> = Dir::read_from(&dir)
			.context("listing the spool")?
			.filter_map(Result::ok)
			.filter_map(|e| valid_name(e.file_name()))
			.filter(|n| self.cursor.as_ref().is_none_or(|c| n > c))
			.collect();
		names.sort_unstable();

		for name in names {
			if self.budget_window.elapsed() >= Duration::from_secs(60) {
				self.budget_window = Instant::now();
				self.budget_used = 0;
			}
			let body = match read_file(&dir, &name, self.config.max_file_bytes) {
				Ok(body) => body,
				Err(e) => {
					tracing::debug!("rejected {name}: {e:#}");
					self.stats.rejected += 1;
					self.cursor = Some(name);
					continue;
				},
			};
			if self.budget_used + body.len() as u64 > self.config.max_bytes_per_minute {
				self.stats.over_budget += 1;
				self.cursor = Some(name);
				continue;
			}
			self.budget_used += body.len() as u64;
			match self.forward(&name, &body).await {
				Ok(true) => self.stats.forwarded += 1,
				Ok(false) => {
					tracing::debug!("rejected {name}: not a valid batch");
					self.stats.rejected += 1;
				},
				Err(e) => return Err(e.context(format!("forwarding {name}"))),
			}
			self.cursor = Some(name);
		}
		Ok(())
	}

	/// `Ok(false)` when the file isn't a valid batch, `Err` when the collector refused it.
	async fn forward(&mut self, name: &str, body: &[u8]) -> anyhow::Result<bool> {
		match BatchKind::of(name) {
			Some(BatchKind::Metrics) => {
				let Ok(mut request) = ExportMetricsServiceRequest::decode(body) else {
					return Ok(false);
				};
				sanitize_metrics(&mut request, &self.config.source);
				if !request.resource_metrics.is_empty() {
					self.metrics.export(request).await.context("exporting metrics")?;
				}
			},
			Some(BatchKind::Traces) => {
				let Ok(mut request) = ExportTraceServiceRequest::decode(body) else {
					return Ok(false);
				};
				sanitize_traces(&mut request, &self.config.source);
				if !request.resource_spans.is_empty() {
					self.traces.export(request).await.context("exporting traces")?;
				}
			},
			None => return Ok(false),
		}
		Ok(true)
	}
}

/// `<20 digit unix nanos>-<pid>-<8 digit seq>.{metrics,traces}.pb`, not from the future.
fn valid_name(name: &CStr) -> Option<String> {
	let name = name.to_str().ok()?;
	let kind = BatchKind::of(name)?;
	let stem = name.strip_suffix(match kind {
		BatchKind::Metrics => crate::spool::METRICS_SUFFIX,
		BatchKind::Traces => crate::spool::TRACES_SUFFIX,
	})?;
	let mut parts = stem.split('-');
	let (nanos, pid, seq) = (parts.next()?, parts.next()?, parts.next()?);
	let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
	if parts.next().is_some() || nanos.len() != 20 || seq.len() != 8 || !digits(nanos) || !digits(pid) || !digits(seq) {
		return None;
	}
	let horizon = SystemTime::now().duration_since(UNIX_EPOCH).ok()? + MAX_CLOCK_SKEW;
	(nanos.parse::<u128>().ok()? <= horizon.as_nanos()).then(|| name.to_string())
}

/// Read a regular file in `dir` without following symlinks or blocking on a FIFO.
fn read_file(dir: impl rustix::fd::AsFd, name: &str, max: u64) -> anyhow::Result<Vec<u8>> {
	let fd = rustix::fs::openat(
		dir,
		name,
		OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
		Mode::empty(),
	)?;
	let stat = rustix::fs::fstat(&fd)?;
	anyhow::ensure!(
		FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile,
		"not a regular file"
	);
	anyhow::ensure!((stat.st_size as u64) <= max, "larger than {max} bytes");
	let mut body = Vec::with_capacity(stat.st_size as usize);
	std::fs::File::from(fd).take(max + 1).read_to_end(&mut body)?;
	anyhow::ensure!(body.len() as u64 <= max, "grew past {max} bytes");
	Ok(body)
}

fn truncate(s: &mut String, max: usize) {
	if s.len() > max {
		let mut end = max;
		while !s.is_char_boundary(end) {
			end -= 1;
		}
		s.truncate(end);
	}
}

/// Keep scalar values only, bounded, at most `max` of them.
fn sanitize_attributes(attributes: &mut Vec<KeyValue>, max: usize) {
	attributes.retain_mut(|kv| {
		if kv.key.is_empty() || kv.key.len() > MAX_KEY_LEN {
			return false;
		}
		kv.key_strindex = 0;
		match kv.value.as_mut().and_then(|v| v.value.as_mut()) {
			Some(any_value::Value::StringValue(s)) => {
				truncate(s, MAX_STRING_LEN);
				true
			},
			Some(any_value::Value::BoolValue(_) | any_value::Value::IntValue(_) | any_value::Value::DoubleValue(_)) => {
				true
			},
			_ => false,
		}
	});
	attributes.truncate(max);
}

fn sanitize_resource(resource: &mut Option<Resource>, source: &str) {
	let resource = resource.get_or_insert_with(Resource::default);
	resource.entity_refs.clear();
	resource.dropped_attributes_count = 0;
	resource.attributes.retain(|kv| kv.key != SOURCE_ATTRIBUTE);
	sanitize_attributes(&mut resource.attributes, MAX_RESOURCE_ATTRIBUTES - 1);
	resource.attributes.push(KeyValue {
		key: SOURCE_ATTRIBUTE.into(),
		value: Some(AnyValue {
			value: Some(any_value::Value::StringValue(source.into())),
		}),
		..Default::default()
	});
}

fn allowed_scope(scope: &mut Option<InstrumentationScope>) -> bool {
	let Some(scope) = scope else { return false };
	if scope.name != SCOPE {
		return false;
	}
	truncate(&mut scope.version, MAX_STRING_LEN);
	sanitize_attributes(&mut scope.attributes, MAX_POINT_ATTRIBUTES);
	true
}

/// Keep `moonshine.*` metrics from the bench's scope, with bounded attributes.
pub fn sanitize_metrics(request: &mut ExportMetricsServiceRequest, source: &str) {
	for rm in &mut request.resource_metrics {
		sanitize_resource(&mut rm.resource, source);
		truncate(&mut rm.schema_url, MAX_STRING_LEN);
		rm.scope_metrics.retain_mut(|sm| {
			if !allowed_scope(&mut sm.scope) {
				return false;
			}
			truncate(&mut sm.schema_url, MAX_STRING_LEN);
			sm.metrics.retain_mut(|m| {
				if !m.name.starts_with("moonshine.") || m.name.len() > MAX_KEY_LEN {
					return false;
				}
				truncate(&mut m.description, MAX_STRING_LEN);
				truncate(&mut m.unit, 32);
				m.metadata.clear();
				macro_rules! points {
					($points:expr) => {
						for p in $points.iter_mut() {
							sanitize_attributes(&mut p.attributes, MAX_POINT_ATTRIBUTES);
							p.exemplars.clear();
						}
					};
				}
				match m.data.as_mut() {
					Some(metric::Data::Gauge(g)) => points!(g.data_points),
					Some(metric::Data::Sum(s)) => points!(s.data_points),
					Some(metric::Data::Histogram(h)) => points!(h.data_points),
					Some(metric::Data::ExponentialHistogram(h)) => points!(h.data_points),
					Some(metric::Data::Summary(s)) => {
						for p in s.data_points.iter_mut() {
							sanitize_attributes(&mut p.attributes, MAX_POINT_ATTRIBUTES);
						}
					},
					None => return false,
				}
				true
			});
			!sm.metrics.is_empty()
		});
	}
	request.resource_metrics.retain(|rm| !rm.scope_metrics.is_empty());
}

/// Keep spans from the bench's scope, with bounded attributes and events and no links.
pub fn sanitize_traces(request: &mut ExportTraceServiceRequest, source: &str) {
	for rs in &mut request.resource_spans {
		sanitize_resource(&mut rs.resource, source);
		truncate(&mut rs.schema_url, MAX_STRING_LEN);
		rs.scope_spans.retain_mut(|ss| {
			if !allowed_scope(&mut ss.scope) {
				return false;
			}
			truncate(&mut ss.schema_url, MAX_STRING_LEN);
			for span in &mut ss.spans {
				truncate(&mut span.name, MAX_KEY_LEN);
				truncate(&mut span.trace_state, MAX_STRING_LEN);
				sanitize_attributes(&mut span.attributes, MAX_SPAN_ATTRIBUTES);
				span.events.truncate(MAX_SPAN_EVENTS);
				for event in &mut span.events {
					truncate(&mut event.name, MAX_KEY_LEN);
					sanitize_attributes(&mut event.attributes, MAX_EVENT_ATTRIBUTES);
				}
				span.links.clear();
				if let Some(status) = &mut span.status {
					truncate(&mut status.message, MAX_STRING_LEN);
				}
			}
			!ss.spans.is_empty()
		});
	}
	request.resource_spans.retain(|rs| !rs.scope_spans.is_empty());
}

#[cfg(test)]
mod tests {
	use opentelemetry_proto::tonic::metrics::v1::{Gauge, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics};

	use super::*;

	fn kv(key: &str, value: any_value::Value) -> KeyValue {
		KeyValue {
			key: key.into(),
			value: Some(AnyValue { value: Some(value) }),
			..Default::default()
		}
	}

	fn gauge(name: &str, attributes: Vec<KeyValue>) -> Metric {
		Metric {
			name: name.into(),
			data: Some(metric::Data::Gauge(Gauge {
				data_points: vec![NumberDataPoint {
					attributes,
					..Default::default()
				}],
			})),
			..Default::default()
		}
	}

	fn scope(name: &str, metrics: Vec<Metric>) -> ScopeMetrics {
		ScopeMetrics {
			scope: Some(InstrumentationScope {
				name: name.into(),
				..Default::default()
			}),
			metrics,
			..Default::default()
		}
	}

	#[test]
	fn names() {
		let ok = c"00000000000000000001-42-00000007.metrics.pb";
		assert_eq!(valid_name(ok).as_deref(), ok.to_str().ok());
		assert!(valid_name(c"00000000000000000001-42-00000007.traces.pb").is_some());
		for bad in [
			c".00000000000000000001-42-00000007.metrics.pb.tmp",
			c"1-42-00000007.metrics.pb",
			c"00000000000000000001-42-7.metrics.pb",
			c"00000000000000000001-4a-00000007.metrics.pb",
			c"00000000000000000001-42-00000007-1.metrics.pb",
			c"../00000000000000000001-42-00000007.metrics.pb",
			c"99999999999999999999-42-00000007.metrics.pb", // far future
		] {
			assert_eq!(valid_name(bad), None, "{bad:?}");
		}
	}

	#[test]
	fn metrics_are_allow_listed_and_bounded() {
		let long = "x".repeat(1000);
		let mut request = ExportMetricsServiceRequest {
			resource_metrics: vec![ResourceMetrics {
				resource: Some(Resource {
					attributes: vec![
						kv("service.name", any_value::Value::StringValue("bench".into())),
						kv(SOURCE_ATTRIBUTE, any_value::Value::StringValue("spoofed".into())),
					],
					..Default::default()
				}),
				scope_metrics: vec![
					scope(
						SCOPE,
						vec![
							gauge(
								"moonshine.gpu.engine.utilization",
								vec![
									kv("engine", any_value::Value::StringValue(long.clone())),
									kv("blob", any_value::Value::BytesValue(vec![0; 16])),
								],
							),
							gauge("process.cpu.time", vec![]),
						],
					),
					scope("someone-else", vec![gauge("moonshine.frames", vec![])]),
				],
				..Default::default()
			}],
		};
		sanitize_metrics(&mut request, "spool-a");

		let rm = &request.resource_metrics[0];
		let resource = &rm.resource.as_ref().unwrap().attributes;
		let sources: Vec<_> = resource.iter().filter(|kv| kv.key == SOURCE_ATTRIBUTE).collect();
		assert_eq!(sources.len(), 1);
		assert_eq!(
			sources[0].value.as_ref().unwrap().value,
			Some(any_value::Value::StringValue("spool-a".into()))
		);
		assert_eq!(rm.scope_metrics.len(), 1);
		let metrics = &rm.scope_metrics[0].metrics;
		assert_eq!(metrics.len(), 1);
		let Some(metric::Data::Gauge(g)) = &metrics[0].data else {
			panic!()
		};
		let attrs = &g.data_points[0].attributes;
		assert_eq!(attrs.len(), 1, "bytes value dropped");
		let Some(any_value::Value::StringValue(engine)) = &attrs[0].value.as_ref().unwrap().value else {
			panic!()
		};
		assert_eq!(engine.len(), MAX_STRING_LEN);
	}

	#[test]
	fn nothing_allowed_means_nothing_forwarded() {
		let mut request = ExportMetricsServiceRequest {
			resource_metrics: vec![ResourceMetrics {
				scope_metrics: vec![scope("other", vec![gauge("moonshine.frames", vec![])])],
				..Default::default()
			}],
		};
		sanitize_metrics(&mut request, "s");
		assert!(request.resource_metrics.is_empty());
	}

	#[test]
	fn files_are_read_safely() {
		let dir = std::env::temp_dir().join(format!("moonshine-otel-relay-test-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		std::fs::write(dir.join("ok"), b"hello").unwrap();
		std::fs::write(dir.join("big"), vec![0; 100]).unwrap();
		std::os::unix::fs::symlink("/etc/hostname", dir.join("link")).unwrap();
		std::fs::create_dir(dir.join("sub")).unwrap();
		let fd = rustix::fs::open(&dir, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()).unwrap();
		assert_eq!(read_file(&fd, "ok", 10).unwrap(), b"hello");
		assert!(read_file(&fd, "big", 10).is_err());
		assert!(read_file(&fd, "link", 10).is_err());
		assert!(read_file(&fd, "sub", 10).is_err());
		std::fs::remove_dir_all(&dir).unwrap();
	}
}
