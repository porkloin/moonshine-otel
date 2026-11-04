//! The [`Telemetry`] handle: owns the OTel providers. Nothing is registered globally.

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, bail};
use opentelemetry::KeyValue;
use opentelemetry::metrics::MeterProvider as _;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{
	BatchConfigBuilder, BatchSpanProcessor, Sampler, SdkTracer, SdkTracerProvider, SpanData, SpanExporter,
};

use crate::metrics::{HistogramBuckets, Instruments};
use crate::spool::{SpoolLimits, SpoolMetricExporter, SpoolSpanExporter, SpoolWriter};
use crate::traces::TraceMode;

/// OTLP transport.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Protocol {
	/// OTLP/gRPC, usually port 4317.
	#[default]
	Grpc,
	/// OTLP/HTTP with protobuf bodies, usually port 4318.
	HttpProtobuf,
}

impl std::str::FromStr for Protocol {
	type Err = String;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		match s {
			"grpc" => Ok(Self::Grpc),
			"http/protobuf" | "http" => Ok(Self::HttpProtobuf),
			_ => Err(format!("expected `grpc` or `http/protobuf`, got `{s}`")),
		}
	}
}

#[derive(Clone, Debug)]
pub struct TelemetryConfig {
	pub endpoint: String,
	pub protocol: Protocol,
	pub trace_mode: TraceMode,
	pub metric_interval: Duration,
	pub histogram_buckets: HistogramBuckets,
	pub service_name: String,
	/// Extra resource attributes, typically [`crate::RunIdentity::resource_attributes`].
	pub resource: Vec<KeyValue>,
	/// Span queue size of the batch processor; spans beyond it are dropped, never blocked on.
	pub span_queue_size: usize,
}

impl Default for TelemetryConfig {
	fn default() -> Self {
		Self {
			endpoint: "http://localhost:4317".to_string(),
			protocol: Protocol::Grpc,
			trace_mode: TraceMode::Outliers,
			metric_interval: Duration::from_secs(5),
			histogram_buckets: HistogramBuckets::Exponential,
			service_name: "moonshine-otel".to_string(),
			resource: Vec::new(),
			span_queue_size: 8192,
		}
	}
}

/// Counters describing the telemetry pipeline itself.
#[derive(Debug, Default)]
pub struct SpanStats {
	/// Spans ended by the recorder.
	pub ended: AtomicU64,
	/// Spans the exporter accepted.
	pub exported: AtomicU64,
}

impl SpanStats {
	/// Spans lost to a full queue or failed exports. Only meaningful after a flush.
	pub fn dropped(&self) -> u64 {
		self.ended
			.load(Ordering::Relaxed)
			.saturating_sub(self.exported.load(Ordering::Relaxed))
	}
}

/// Owns the meter and tracer providers for a process.
///
/// Create it once, hand it to every run, and call [`Telemetry::shutdown`] before exiting so
/// the last batch is flushed.
pub struct Telemetry {
	meter_provider: Option<SdkMeterProvider>,
	tracer_provider: Option<SdkTracerProvider>,
	instruments: Option<Instruments>,
	tracer: Option<SdkTracer>,
	trace_mode: TraceMode,
	span_stats: Arc<SpanStats>,
}

impl Telemetry {
	/// No export at all: runs still produce exact summaries and result files. Used by
	/// `--no-export` to measure the instrumentation's own overhead.
	pub fn disabled() -> Self {
		Self {
			meter_provider: None,
			tracer_provider: None,
			instruments: None,
			tracer: None,
			trace_mode: TraceMode::None,
			span_stats: Arc::default(),
		}
	}

	/// Export over OTLP.
	pub fn otlp(config: &TelemetryConfig) -> anyhow::Result<Self> {
		let timeout = Duration::from_secs(5);
		match config.protocol {
			Protocol::Grpc => {
				let metrics = opentelemetry_otlp::MetricExporter::builder()
					.with_tonic()
					.with_endpoint(&config.endpoint)
					.with_timeout(timeout)
					.build()
					.context("building OTLP/gRPC metric exporter")?;
				let spans = opentelemetry_otlp::SpanExporter::builder()
					.with_tonic()
					.with_endpoint(&config.endpoint)
					.with_timeout(timeout)
					.build()
					.context("building OTLP/gRPC span exporter")?;
				Ok(Self::with_exporters(config, metrics, spans))
			},
			Protocol::HttpProtobuf => {
				let base = config.endpoint.trim_end_matches('/');
				let metrics = opentelemetry_otlp::MetricExporter::builder()
					.with_http()
					.with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
					.with_endpoint(format!("{base}/v1/metrics"))
					.with_timeout(timeout)
					.build()
					.context("building OTLP/HTTP metric exporter")?;
				let spans = opentelemetry_otlp::SpanExporter::builder()
					.with_http()
					.with_protocol(opentelemetry_otlp::Protocol::HttpBinary)
					.with_endpoint(format!("{base}/v1/traces"))
					.with_timeout(timeout)
					.build()
					.context("building OTLP/HTTP span exporter")?;
				Ok(Self::with_exporters(config, metrics, spans))
			},
		}
	}

	/// Write OTLP batches into a spool directory instead of sending them (see [`crate::spool`]).
	/// `endpoint` and `protocol` are ignored.
	pub fn spool(config: &TelemetryConfig, dir: &std::path::Path, limits: SpoolLimits) -> anyhow::Result<Self> {
		let writer = Arc::new(SpoolWriter::new(dir, limits)?);
		Ok(Self::with_exporters(
			config,
			SpoolMetricExporter(writer.clone()),
			SpoolSpanExporter::new(writer),
		))
	}

	/// Build from arbitrary exporters, e.g. the SDK's in-memory ones in tests.
	pub fn with_exporters<M, S>(config: &TelemetryConfig, metrics: M, spans: S) -> Self
	where
		M: PushMetricExporter,
		S: SpanExporter + 'static,
	{
		let resource = Resource::builder()
			.with_service_name(config.service_name.clone())
			.with_attributes(config.resource.clone())
			.build();

		let reader = PeriodicReader::builder(metrics)
			.with_interval(config.metric_interval)
			.build();
		let meter_provider = SdkMeterProvider::builder()
			.with_resource(resource.clone())
			.with_reader(reader)
			.with_view(crate::metrics::histogram_view(config.histogram_buckets))
			.build();
		let instruments = Instruments::new(&meter_provider.meter("moonshine-otel"));

		let span_stats = Arc::new(SpanStats::default());
		let processor = BatchSpanProcessor::builder(CountingExporter {
			inner: spans,
			stats: span_stats.clone(),
		})
		.with_batch_config(
			BatchConfigBuilder::default()
				.with_max_queue_size(config.span_queue_size)
				.with_scheduled_delay(Duration::from_secs(1))
				.build(),
		)
		.build();
		let tracer_provider = SdkTracerProvider::builder()
			.with_resource(resource)
			.with_sampler(Sampler::AlwaysOn)
			// Frame traces take ids derived from the stream and frame (see frame_ids).
			.with_id_generator(crate::frame_ids::FrameIdGenerator::default())
			// bench.run spans carry the full summary and identity.
			.with_max_attributes_per_span(256)
			.with_span_processor(processor)
			.build();
		let tracer = tracer_provider.tracer("moonshine-otel");

		Self {
			meter_provider: Some(meter_provider),
			tracer_provider: Some(tracer_provider),
			instruments: Some(instruments),
			tracer: Some(tracer),
			trace_mode: config.trace_mode,
			span_stats,
		}
	}

	pub fn is_enabled(&self) -> bool {
		self.meter_provider.is_some()
	}

	pub fn span_stats(&self) -> &SpanStats {
		&self.span_stats
	}

	pub(crate) fn instruments(&self) -> Option<&Instruments> {
		self.instruments.as_ref()
	}

	/// A meter for metrics of a process's own (a test client's), under moonshine-otel's scope so the relay passes
	/// them; None without export.
	pub fn meter(&self) -> Option<opentelemetry::metrics::Meter> {
		self.meter_provider.as_ref().map(|mp| mp.meter("moonshine-otel"))
	}

	/// The tracer, for spans of a process's own (a test client's frames); None without trace export.
	pub fn span_tracer(&self) -> Option<SdkTracer> {
		self.tracer().map(|(t, _, _)| t.clone())
	}

	pub(crate) fn tracer(&self) -> Option<(&SdkTracer, TraceMode, Arc<SpanStats>)> {
		self.tracer
			.as_ref()
			.map(|t| (t, self.trace_mode, self.span_stats.clone()))
	}

	/// Export everything recorded so far.
	pub fn flush(&self) -> anyhow::Result<()> {
		let mut errors = Vec::new();
		if let Some(tp) = &self.tracer_provider
			&& let Err(e) = tp.force_flush()
		{
			errors.push(format!("traces: {e}"));
		}
		if let Some(mp) = &self.meter_provider
			&& let Err(e) = mp.force_flush()
		{
			errors.push(format!("metrics: {e}"));
		}
		if errors.is_empty() {
			Ok(())
		} else {
			bail!("flushing telemetry failed: {}", errors.join("; "))
		}
	}

	/// Flush and stop the providers.
	pub fn shutdown(self) -> anyhow::Result<()> {
		let flushed = self.flush();
		if let Some(tp) = self.tracer_provider {
			let _ = tp.shutdown();
		}
		if let Some(mp) = self.meter_provider {
			let _ = mp.shutdown();
		}
		flushed
	}
}

/// Counts spans the wrapped exporter accepted, so drops can be reported.
#[derive(Debug)]
struct CountingExporter<E> {
	inner: E,
	stats: Arc<SpanStats>,
}

impl<E: SpanExporter> SpanExporter for CountingExporter<E> {
	async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
		let n = batch.len() as u64;
		let result = self.inner.export(batch).await;
		if result.is_ok() {
			self.stats.exported.fetch_add(n, Ordering::Relaxed);
		}
		result
	}

	fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
		self.inner.shutdown_with_timeout(timeout)
	}

	fn force_flush(&self) -> OTelSdkResult {
		self.inner.force_flush()
	}

	fn set_resource(&mut self, resource: &Resource) {
		self.inner.set_resource(resource);
	}
}

/// Check that something is listening at an OTLP endpoint (`http://host:port[/path]`).
///
/// OTLP exporters fail quietly in the background, so this runs before a bench starts to turn
/// "why is my dashboard empty?" into an immediate error.
pub fn check_endpoint(endpoint: &str, timeout: Duration) -> anyhow::Result<()> {
	let (host, port) = parse_endpoint(endpoint)?;
	let addrs: Vec<_> = (host.as_str(), port)
		.to_socket_addrs()
		.with_context(|| format!("resolving {host}"))?
		.collect();
	let mut last = None;
	for addr in &addrs {
		match TcpStream::connect_timeout(addr, timeout) {
			Ok(_) => return Ok(()),
			Err(e) => last = Some(e),
		}
	}
	match last {
		Some(e) => bail!("nothing is accepting connections at {endpoint}: {e}"),
		None => bail!("{host} did not resolve to any address"),
	}
}

fn parse_endpoint(endpoint: &str) -> anyhow::Result<(String, u16)> {
	let (scheme, rest) = endpoint.split_once("://").unwrap_or(("http", endpoint));
	let authority = rest.split('/').next().unwrap_or(rest);
	let default_port = if scheme == "https" { 443 } else { 4317 };
	// Bracketed IPv6: [::1]:4317
	if let Some(v6) = authority.strip_prefix('[') {
		let (host, after) = v6.split_once(']').context("unterminated IPv6 address")?;
		let port = match after.strip_prefix(':') {
			Some(p) => p.parse().context("invalid port")?,
			None => default_port,
		};
		return Ok((host.to_string(), port));
	}
	match authority.rsplit_once(':') {
		Some((host, port)) => Ok((host.to_string(), port.parse().context("invalid port")?)),
		None => Ok((authority.to_string(), default_port)),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn endpoint_parsing() {
		assert_eq!(
			parse_endpoint("http://localhost:4317").unwrap(),
			("localhost".into(), 4317)
		);
		assert_eq!(
			parse_endpoint("http://10.0.0.2:4318/").unwrap(),
			("10.0.0.2".into(), 4318)
		);
		assert_eq!(parse_endpoint("localhost").unwrap(), ("localhost".into(), 4317));
		assert_eq!(
			parse_endpoint("https://otel.example").unwrap(),
			("otel.example".into(), 443)
		);
		assert_eq!(parse_endpoint("http://[::1]:4317").unwrap(), ("::1".into(), 4317));
		assert!(parse_endpoint("http://host:notaport").is_err());
	}

	#[test]
	fn check_endpoint_reports_closed_port() {
		let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
		let port = listener.local_addr().unwrap().port();
		assert!(check_endpoint(&format!("http://127.0.0.1:{port}"), Duration::from_secs(1)).is_ok());
		drop(listener);
		let err = check_endpoint(&format!("http://127.0.0.1:{port}"), Duration::from_secs(1)).unwrap_err();
		assert!(err.to_string().contains("nothing is accepting"), "{err}");
	}

	#[test]
	fn protocol_parsing() {
		assert_eq!("grpc".parse::<Protocol>().unwrap(), Protocol::Grpc);
		assert_eq!("http/protobuf".parse::<Protocol>().unwrap(), Protocol::HttpProtobuf);
		assert!("udp".parse::<Protocol>().is_err());
	}
}
