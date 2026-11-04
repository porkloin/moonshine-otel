//! A file spool for OTLP: the same protobuf requests the OTLP/gRPC exporters send, written to a
//! directory instead of a socket.
//!
//! For a server that runs sandboxed, with no network path to a collector. Something outside the
//! sandbox reads the directory and forwards the batches (`moonshine-otel-relay`), so the sandbox
//! only ever writes files and never opens a connection.
//!
//! One request per file, named `<unix_nanos>-<pid>-<seq>.metrics.pb` or `.traces.pb`. Each is
//! written under a dot-prefixed name and renamed into place, so a reader never sees a partial
//! file. Names sort in write order. The writer deletes old files ([`SpoolLimits`]), so a reader
//! needs read access only.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::transform::common::tonic::ResourceAttributesWithSchema;
use opentelemetry_proto::transform::trace::tonic::group_spans_by_resource_and_scope;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::metrics::Temporality;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::trace::{SpanData, SpanExporter};
use prost::Message;

/// File suffix of a metrics batch.
pub const METRICS_SUFFIX: &str = ".metrics.pb";
/// File suffix of a traces batch.
pub const TRACES_SUFFIX: &str = ".traces.pb";

/// How much the spool keeps when nothing reads it.
#[derive(Clone, Copy, Debug)]
pub struct SpoolLimits {
	/// Files older than this are deleted.
	pub max_age: Duration,
	/// Beyond this total size, the oldest files are deleted.
	pub max_bytes: u64,
}

impl Default for SpoolLimits {
	fn default() -> Self {
		Self {
			max_age: Duration::from_secs(10 * 60),
			max_bytes: 64 << 20,
		}
	}
}

/// What a spool file holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchKind {
	Metrics,
	Traces,
}

impl BatchKind {
	/// The kind of a finished spool file, from its name. Temporary and unknown files are `None`.
	pub fn of(name: &str) -> Option<Self> {
		if name.starts_with('.') {
			None
		} else if name.ends_with(METRICS_SUFFIX) {
			Some(Self::Metrics)
		} else if name.ends_with(TRACES_SUFFIX) {
			Some(Self::Traces)
		} else {
			None
		}
	}

	fn suffix(self) -> &'static str {
		match self {
			Self::Metrics => METRICS_SUFFIX,
			Self::Traces => TRACES_SUFFIX,
		}
	}
}

/// Writes batches into a spool directory. Shared by the metric and span exporters.
#[derive(Debug)]
pub struct SpoolWriter {
	dir: PathBuf,
	limits: SpoolLimits,
	seq: AtomicU64,
	/// Serializes pruning, which lists the directory.
	prune: Mutex<()>,
}

impl SpoolWriter {
	/// Create the directory if needed (mode 0755, so a reader outside the sandbox can list it).
	pub fn new(dir: impl Into<PathBuf>, limits: SpoolLimits) -> anyhow::Result<Self> {
		let dir = dir.into();
		fs::create_dir_all(&dir).with_context(|| format!("creating spool directory {}", dir.display()))?;
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			fs::set_permissions(&dir, fs::Permissions::from_mode(0o755))
				.with_context(|| format!("setting permissions on {}", dir.display()))?;
		}
		Ok(Self {
			dir,
			limits,
			seq: AtomicU64::new(0),
			prune: Mutex::new(()),
		})
	}

	pub fn dir(&self) -> &Path {
		&self.dir
	}

	/// Write one batch and prune. Returns the file name.
	pub fn write(&self, kind: BatchKind, body: &[u8]) -> std::io::Result<String> {
		let nanos = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or_default()
			.as_nanos();
		let seq = self.seq.fetch_add(1, Ordering::Relaxed);
		let name = format!("{nanos:020}-{}-{seq:08}{}", std::process::id(), kind.suffix());
		let tmp = self.dir.join(format!(".{name}.tmp"));
		{
			let mut file = fs::File::create(&tmp)?;
			file.write_all(body)?;
		}
		fs::rename(&tmp, self.dir.join(&name))?;
		self.prune_old();
		Ok(name)
	}

	/// Delete files past [`SpoolLimits`]. Errors are ignored: a reader may race us.
	fn prune_old(&self) {
		let Ok(_guard) = self.prune.try_lock() else {
			return;
		};
		let Ok(entries) = fs::read_dir(&self.dir) else {
			return;
		};
		let now = SystemTime::now();
		let mut files: Vec<(String, u64, SystemTime)> = entries
			.filter_map(Result::ok)
			.filter_map(|e| {
				let name = e.file_name().into_string().ok()?;
				BatchKind::of(&name)?;
				let meta = e.metadata().ok()?;
				Some((name, meta.len(), meta.modified().ok()?))
			})
			.collect();
		files.sort_unstable_by(|a, b| b.0.cmp(&a.0)); // newest first
		let mut total = 0u64;
		for (name, len, modified) in files {
			total += len;
			let too_old = now.duration_since(modified).unwrap_or_default() > self.limits.max_age;
			if too_old || total > self.limits.max_bytes {
				let _ = fs::remove_file(self.dir.join(name));
			}
		}
	}
}

fn io_error(e: std::io::Error) -> OTelSdkError {
	OTelSdkError::InternalFailure(format!("writing spool file: {e}"))
}

/// Metrics into the spool, cumulative like the OTLP exporter's default.
#[derive(Debug)]
pub struct SpoolMetricExporter(pub std::sync::Arc<SpoolWriter>);

impl PushMetricExporter for SpoolMetricExporter {
	async fn export(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
		let body = ExportMetricsServiceRequest::from(metrics).encode_to_vec();
		self.0.write(BatchKind::Metrics, &body).map(drop).map_err(io_error)
	}

	fn force_flush(&self) -> OTelSdkResult {
		Ok(())
	}

	fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
		Ok(())
	}

	fn temporality(&self) -> Temporality {
		Temporality::Cumulative
	}
}

/// Spans into the spool.
#[derive(Debug)]
pub struct SpoolSpanExporter {
	writer: std::sync::Arc<SpoolWriter>,
	resource: ResourceAttributesWithSchema,
}

impl SpoolSpanExporter {
	pub fn new(writer: std::sync::Arc<SpoolWriter>) -> Self {
		Self {
			writer,
			resource: ResourceAttributesWithSchema::default(),
		}
	}
}

impl SpanExporter for SpoolSpanExporter {
	async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
		let request = ExportTraceServiceRequest {
			resource_spans: group_spans_by_resource_and_scope(batch, &self.resource),
		};
		self.writer
			.write(BatchKind::Traces, &request.encode_to_vec())
			.map(drop)
			.map_err(io_error)
	}

	fn set_resource(&mut self, resource: &Resource) {
		self.resource = resource.into();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn kinds_from_names() {
		assert_eq!(BatchKind::of("1-2-3.metrics.pb"), Some(BatchKind::Metrics));
		assert_eq!(BatchKind::of("1-2-3.traces.pb"), Some(BatchKind::Traces));
		assert_eq!(BatchKind::of(".1-2-3.metrics.pb.tmp"), None);
		assert_eq!(BatchKind::of(".hidden.metrics.pb"), None);
		assert_eq!(BatchKind::of("notes.txt"), None);
	}

	#[test]
	fn names_sort_in_write_order_and_old_files_are_pruned() {
		let dir = std::env::temp_dir().join(format!("moonshine-otel-spool-test-{}", std::process::id()));
		let _ = fs::remove_dir_all(&dir);
		let writer = SpoolWriter::new(
			&dir,
			SpoolLimits {
				max_age: Duration::from_secs(3600),
				max_bytes: 25,
			},
		)
		.unwrap();
		let names: Vec<_> = (0..5)
			.map(|_| writer.write(BatchKind::Metrics, &[0; 10]).unwrap())
			.collect();
		let mut sorted = names.clone();
		sorted.sort();
		assert_eq!(names, sorted);
		let mut left: Vec<_> = fs::read_dir(&dir)
			.unwrap()
			.map(|e| e.unwrap().file_name().into_string().unwrap())
			.collect();
		left.sort();
		// 25 bytes keeps the two newest 10-byte files.
		assert_eq!(left, names[3..]);
		fs::remove_dir_all(&dir).unwrap();
	}
}
