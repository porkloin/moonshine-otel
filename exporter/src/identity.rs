//! What produced a set of numbers: build, hardware, environment and workload.

use std::collections::BTreeMap;

use opentelemetry::KeyValue;
use serde::{Deserialize, Serialize};

/// Build, hardware and environment identity of a run. Unknown values stay `None`; nothing is
/// ever guessed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RunIdentity {
	pub build: BuildInfo,
	pub host: HostInfo,
	pub gpu: GpuInfo,
	/// Free-form `--label key=value` pairs.
	#[serde(default)]
	pub labels: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BuildInfo {
	/// Where Moonshine came from: `git+<url>#<rev>` or `path+<dir>`.
	pub moonshine_source: Option<String>,
	/// Commit hash of Moonshine.
	pub moonshine_rev: Option<String>,
	/// `git describe --always --dirty` for a path dependency.
	pub moonshine_describe: Option<String>,
	pub moonshine_dirty: Option<bool>,
	pub moonshine_version: Option<String>,
	pub tool_version: Option<String>,
	pub rustc_version: Option<String>,
	/// Cargo profile, e.g. `release` or `debug`.
	pub profile: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HostInfo {
	pub hostname: Option<String>,
	pub os: Option<String>,
	pub kernel: Option<String>,
	pub cpu_model: Option<String>,
	pub cpu_governor: Option<String>,
	pub energy_performance_preference: Option<String>,
	pub platform_profile: Option<String>,
	pub on_ac_power: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuInfo {
	pub vendor: Option<String>,
	pub model: Option<String>,
	/// `vendor:device` PCI ids.
	pub pci_id: Option<String>,
	/// Kernel driver bound to the render node, e.g. `amdgpu`.
	pub kernel_driver: Option<String>,
	pub vulkan_driver: Option<String>,
	/// Device name reported by the Vulkan driver, e.g. `AMD Radeon RX 6800 XT (RADV NAVI21)`.
	pub vulkan_device: Option<String>,
	pub vulkan_driver_version: Option<String>,
	pub mesa_version: Option<String>,
	/// amdgpu `power_dpm_force_performance_level`.
	pub power_level: Option<String>,
}

/// How much an identity difference matters for a comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
	/// The numbers are likely affected, but a comparison may still be what you want.
	Warning,
	/// Comparing these runs is not meaningful.
	Invalidating,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IdentityMismatch {
	pub field: String,
	pub baseline: Option<String>,
	pub candidate: Option<String>,
	pub severity: Severity,
}

impl RunIdentity {
	/// Flat `(key, value)` view of every known field. Keys double as OTel resource attribute
	/// names.
	pub fn fields(&self) -> Vec<(String, String)> {
		let mut out = Vec::new();
		let mut push = |k: &str, v: &Option<String>| {
			if let Some(v) = v {
				out.push((k.to_string(), v.clone()));
			}
		};
		let b = &self.build;
		push("moonshine.source", &b.moonshine_source);
		push("moonshine.rev", &b.moonshine_rev);
		push("moonshine.describe", &b.moonshine_describe);
		push("moonshine.dirty", &b.moonshine_dirty.map(|d| d.to_string()));
		push("moonshine.version", &b.moonshine_version);
		push("service.version", &b.tool_version);
		push("build.rustc", &b.rustc_version);
		push("build.profile", &b.profile);
		let h = &self.host;
		push("host.name", &h.hostname);
		push("os.description", &h.os);
		push("os.kernel", &h.kernel);
		push("host.cpu.model.name", &h.cpu_model);
		push("host.cpu.governor", &h.cpu_governor);
		push("host.cpu.epp", &h.energy_performance_preference);
		push("host.platform_profile", &h.platform_profile);
		push("host.on_ac_power", &h.on_ac_power.map(|v| v.to_string()));
		let g = &self.gpu;
		push("gpu.vendor", &g.vendor);
		push("gpu.model", &g.model);
		push("gpu.pci_id", &g.pci_id);
		push("gpu.kernel_driver", &g.kernel_driver);
		push("gpu.vulkan.driver", &g.vulkan_driver);
		push("gpu.vulkan.device", &g.vulkan_device);
		push("gpu.vulkan.driver_version", &g.vulkan_driver_version);
		push("gpu.mesa_version", &g.mesa_version);
		push("gpu.power_level", &g.power_level);
		for (k, v) in &self.labels {
			out.push((format!("label.{k}"), v.clone()));
		}
		out
	}

	/// Resource attributes for OTel.
	pub fn resource_attributes(&self) -> Vec<KeyValue> {
		self.fields().into_iter().map(|(k, v)| KeyValue::new(k, v)).collect()
	}

	/// Conditions that commonly add noise to benchmark numbers.
	pub fn variance_warnings(&self) -> Vec<String> {
		let mut w = Vec::new();
		if let Some(p) = &self.build.profile
			&& p != "release"
		{
			w.push(format!(
				"built with the `{p}` profile; timings are not representative, use --release"
			));
		}
		if let Some(g) = &self.host.cpu_governor
			&& g != "performance"
		{
			w.push(format!(
				"CPU governor is `{g}`, not `performance`; expect more run-to-run noise"
			));
		}
		if self.host.on_ac_power == Some(false) {
			w.push("running on battery power".to_string());
		}
		if let Some(p) = &self.gpu.power_level
			&& p != "auto"
		{
			w.push(format!(
				"GPU power_dpm_force_performance_level is `{p}`, not `auto` (a stuck level often comes from a previous session)"
			));
		}
		if self.build.moonshine_dirty == Some(true) {
			w.push("Moonshine has uncommitted changes; the result is not reproducible from a revision".to_string());
		}
		w
	}

	/// Differences that affect whether two runs can be compared.
	pub fn mismatches(&self, other: &RunIdentity) -> Vec<IdentityMismatch> {
		use Severity::*;
		let ac = |id: &RunIdentity| id.host.on_ac_power.map(|v| v.to_string());
		let (ac_a, ac_b) = (ac(self), ac(other));
		let checks: [(&str, Severity, &Option<String>, &Option<String>); 13] = [
			("gpu.model", Invalidating, &self.gpu.model, &other.gpu.model),
			("gpu.pci_id", Invalidating, &self.gpu.pci_id, &other.gpu.pci_id),
			("build.profile", Invalidating, &self.build.profile, &other.build.profile),
			("host.name", Warning, &self.host.hostname, &other.host.hostname),
			(
				"gpu.kernel_driver",
				Warning,
				&self.gpu.kernel_driver,
				&other.gpu.kernel_driver,
			),
			(
				"gpu.vulkan.driver_version",
				Warning,
				&self.gpu.vulkan_driver_version,
				&other.gpu.vulkan_driver_version,
			),
			(
				"gpu.mesa_version",
				Warning,
				&self.gpu.mesa_version,
				&other.gpu.mesa_version,
			),
			("os.kernel", Warning, &self.host.kernel, &other.host.kernel),
			(
				"host.cpu.governor",
				Warning,
				&self.host.cpu_governor,
				&other.host.cpu_governor,
			),
			(
				"host.cpu.epp",
				Warning,
				&self.host.energy_performance_preference,
				&other.host.energy_performance_preference,
			),
			(
				"host.platform_profile",
				Warning,
				&self.host.platform_profile,
				&other.host.platform_profile,
			),
			("host.on_ac_power", Warning, &ac_a, &ac_b),
			(
				"gpu.power_level",
				Warning,
				&self.gpu.power_level,
				&other.gpu.power_level,
			),
		];
		let mut out: Vec<IdentityMismatch> = checks
			.into_iter()
			.filter(|(_, _, a, b)| a != b)
			.map(|(field, severity, a, b)| IdentityMismatch {
				field: field.to_string(),
				baseline: a.clone(),
				candidate: b.clone(),
				severity,
			})
			.collect();
		// A debug build on either side is invalid even when both sides agree.
		let non_release = |id: &RunIdentity| id.build.profile.as_deref().is_some_and(|p| p != "release");
		if (non_release(self) || non_release(other)) && !out.iter().any(|m| m.field == "build.profile") {
			out.push(IdentityMismatch {
				field: "build.profile".to_string(),
				baseline: self.build.profile.clone(),
				candidate: other.build.profile.clone(),
				severity: Invalidating,
			});
		}
		out
	}
}

/// The workload a run measured.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RunConfig {
	pub command: Vec<String>,
	pub width: u32,
	pub height: u32,
	pub target_fps: u32,
	/// `h264`, `hevc` or `av1`.
	pub codec: String,
	/// Bits per second.
	pub bitrate: u64,
	pub hdr: bool,
	/// `yuv420` or `yuv444`.
	pub chroma: String,
	pub warmup_s: f64,
	pub duration_s: f64,
	pub iterations: u32,
}

impl RunConfig {
	pub fn resolution(&self) -> String {
		format!("{}x{}", self.width, self.height)
	}

	/// Key used to match runs between two result files: everything that changes the workload.
	pub fn key(&self) -> String {
		format!(
			"{}@{} {} {}bps{}{}",
			self.resolution(),
			self.target_fps,
			self.codec,
			self.bitrate,
			if self.hdr { " hdr" } else { "" },
			if self.chroma == "yuv420" || self.chroma.is_empty() {
				String::new()
			} else {
				format!(" {}", self.chroma)
			}
		)
	}

	/// The low-cardinality attributes every metric of this run carries.
	pub fn metric_attributes(&self, run_id: &str) -> Vec<KeyValue> {
		vec![
			KeyValue::new("codec", self.codec.clone()),
			KeyValue::new("resolution", self.resolution()),
			KeyValue::new("target_fps", self.target_fps as i64),
			KeyValue::new("hdr", self.hdr),
			KeyValue::new("run_id", run_id.to_string()),
		]
	}

	/// Span attributes describing the workload.
	pub fn span_attributes(&self) -> Vec<KeyValue> {
		vec![
			KeyValue::new("bench.command", self.command.join(" ")),
			KeyValue::new("bench.codec", self.codec.clone()),
			KeyValue::new("bench.resolution", self.resolution()),
			KeyValue::new("bench.target_fps", self.target_fps as i64),
			KeyValue::new("bench.bitrate", self.bitrate as i64),
			KeyValue::new("bench.hdr", self.hdr),
			KeyValue::new("bench.chroma", self.chroma.clone()),
			KeyValue::new("bench.warmup_s", self.warmup_s),
			KeyValue::new("bench.duration_s", self.duration_s),
			KeyValue::new("bench.iterations", self.iterations as i64),
		]
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn id() -> RunIdentity {
		let mut id = RunIdentity::default();
		id.build.profile = Some("release".into());
		id.gpu.model = Some("RX 6800 XT".into());
		id.host.cpu_governor = Some("performance".into());
		id.gpu.power_level = Some("auto".into());
		id
	}

	#[test]
	fn same_identity_matches() {
		assert!(id().mismatches(&id()).is_empty());
		assert!(id().variance_warnings().is_empty());
	}

	#[test]
	fn gpu_change_invalidates() {
		let mut b = id();
		b.gpu.model = Some("Arc B580".into());
		let m = id().mismatches(&b);
		assert_eq!(m.len(), 1);
		assert_eq!(m[0].severity, Severity::Invalidating);
	}

	#[test]
	fn debug_on_both_sides_still_invalidates() {
		let mut a = id();
		a.build.profile = Some("debug".into());
		let m = a.mismatches(&a.clone());
		assert_eq!(m.len(), 1);
		assert_eq!(m[0].field, "build.profile");
		assert_eq!(m[0].severity, Severity::Invalidating);
	}

	#[test]
	fn governor_is_warning() {
		let mut b = id();
		b.host.cpu_governor = Some("powersave".into());
		assert_eq!(id().mismatches(&b)[0].severity, Severity::Warning);
		assert_eq!(b.variance_warnings().len(), 1);
	}

	#[test]
	fn config_key() {
		let c = RunConfig {
			width: 1920,
			height: 1080,
			target_fps: 60,
			codec: "hevc".into(),
			bitrate: 20_000_000,
			hdr: true,
			chroma: "yuv420".into(),
			..Default::default()
		};
		assert_eq!(c.key(), "1920x1080@60 hevc 20000000bps hdr");
	}
}
