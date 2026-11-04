//! Run identity capture and resource sampling (GPU engines via DRM fdinfo, CPU per thread).
//!
//! Every value is read from the kernel or the driver; anything unreadable stays unknown.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;

use moonshine_otel_exporter::RunIdentity;
use moonshine_otel_exporter::identity::{BuildInfo, GpuInfo, HostInfo};
use moonshine_otel_exporter::metrics::RunMetrics;

fn read(path: impl AsRef<Path>) -> Option<String> {
	std::fs::read_to_string(path)
		.ok()
		.map(|s| s.trim().to_string())
		.filter(|s| !s.is_empty())
}

pub fn build_info() -> BuildInfo {
	let opt = |s: &str| (!s.is_empty()).then(|| s.to_string());
	BuildInfo {
		tool_version: Some(env!("CARGO_PKG_VERSION").to_string()),
		rustc_version: opt(env!("BENCH_RUSTC_VERSION")),
		// Cargo's PROFILE: "release" for release-based profiles, "debug" otherwise.
		profile: opt(env!("BENCH_PROFILE")),
		// This tool doesn't build Moonshine; the server's revision is whatever is running.
		..Default::default()
	}
}

pub fn host_info() -> HostInfo {
	let os = read("/etc/os-release").and_then(|t| {
		t.lines()
			.find_map(|l| l.strip_prefix("PRETTY_NAME="))
			.map(|v| v.trim_matches('"').to_string())
	});
	let cpu_model = read("/proc/cpuinfo").and_then(|t| {
		t.lines()
			.find_map(|l| l.strip_prefix("model name"))
			.and_then(|l| l.split_once(':'))
			.map(|(_, v)| v.trim().to_string())
	});
	let cpufreq = Path::new("/sys/devices/system/cpu/cpu0/cpufreq");
	HostInfo {
		hostname: gethostname::gethostname().into_string().ok(),
		os,
		kernel: read("/proc/sys/kernel/osrelease"),
		cpu_model,
		cpu_governor: read(cpufreq.join("scaling_governor")),
		energy_performance_preference: read(cpufreq.join("energy_performance_preference")),
		platform_profile: read("/sys/firmware/acpi/platform_profile"),
		on_ac_power: on_ac_power(),
	}
}

/// `Some(true)` on mains, `Some(false)` on battery, `None` without power supply info.
fn on_ac_power() -> Option<bool> {
	let mut saw_mains = false;
	for entry in std::fs::read_dir("/sys/class/power_supply").ok()?.flatten() {
		let p = entry.path();
		if read(p.join("type")).as_deref() == Some("Mains") {
			saw_mains = true;
			if read(p.join("online")).as_deref() == Some("1") {
				return Some(true);
			}
		}
	}
	saw_mains.then_some(false)
}

/// The render node Moonshine will use and its sysfs device directory.
pub fn render_device(gpu: Option<&str>) -> Option<(PathBuf, PathBuf)> {
	let node = find_render_node(gpu)?;
	let node = node.canonicalize().unwrap_or(node);
	let name = node.file_name()?.to_owned();
	let device = Path::new("/sys/class/drm").join(name).join("device");
	Some((node, device.canonicalize().unwrap_or(device)))
}

/// The same search as Moonshine's:
/// `MOONSHINE_RENDER_NODE`, then the configured GPU (path, node name or uevent substring),
/// then the first Nvidia, then AMD, then any render node.
fn find_render_node(gpu: Option<&str>) -> Option<PathBuf> {
	if let Ok(node) = std::env::var("MOONSHINE_RENDER_NODE") {
		return Some(PathBuf::from(node));
	}
	let mut nodes: Vec<(String, String)> = std::fs::read_dir("/dev/dri")
		.ok()?
		.flatten()
		.filter_map(|e| e.file_name().into_string().ok())
		.filter(|n| n.starts_with("renderD"))
		.map(|n| {
			let uevent = read(Path::new("/sys/class/drm").join(&n).join("device/uevent")).unwrap_or_default();
			(n, uevent)
		})
		.collect();
	nodes.sort();
	if let Some(g) = gpu {
		if Path::new(g).is_absolute() {
			return Some(PathBuf::from(g));
		}
		return nodes
			.iter()
			.find(|(n, u)| n == g || u.to_lowercase().contains(&g.to_lowercase()))
			.map(|(n, _)| Path::new("/dev/dri").join(n));
	}
	let score = |u: &str| {
		if u.contains("PCI_ID=10DE") {
			100
		} else if u.contains("PCI_ID=1002") {
			50
		} else {
			0
		}
	};
	// max_by_key keeps the last maximum; iterate in reverse so the first node wins ties.
	nodes
		.iter()
		.rev()
		.max_by_key(|(_, u)| score(u))
		.map(|(n, _)| Path::new("/dev/dri").join(n))
}

pub fn gpu_info(gpu: Option<&str>, vulkan: Option<&VulkanInfo>) -> GpuInfo {
	let mut info = GpuInfo::default();
	if let Some((_, dev)) = render_device(gpu) {
		let vendor = read(dev.join("vendor")).map(|v| v.trim_start_matches("0x").to_string());
		let device = read(dev.join("device")).map(|v| v.trim_start_matches("0x").to_string());
		if let (Some(v), Some(d)) = (&vendor, &device) {
			info.pci_id = Some(format!("{v}:{d}"));
			let (vn, dn) = pci_names(v, d);
			info.vendor = vn.or_else(|| Some(v.clone()));
			info.model = dn;
		}
		info.kernel_driver = std::fs::read_link(dev.join("driver"))
			.ok()
			.and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()));
		info.power_level = read(dev.join("power_dpm_force_performance_level"));
	}
	if let Some(vk) = vulkan {
		// `model` stays the pci.ids name for the selected render node, which is deterministic;
		// the Vulkan probe may pick another device on multi-GPU hosts.
		info.vulkan_device = Some(vk.device_name.clone());
		info.vulkan_driver_version = vk.driver_version.clone();
		// RADV, ANV and NVK report the Mesa version as their driver version.
		if matches!(
			info.kernel_driver.as_deref(),
			Some("amdgpu" | "radeon" | "i915" | "xe" | "nouveau")
		) {
			info.mesa_version = vk.driver_version.clone();
		}
	}
	info
}

/// Vendor and device names from the system pci.ids database.
fn pci_names(vendor: &str, device: &str) -> (Option<String>, Option<String>) {
	let db = [
		"/usr/share/hwdata/pci.ids",
		"/usr/share/misc/pci.ids",
		"/usr/share/pci.ids",
	]
	.iter()
	.find_map(|p| std::fs::read_to_string(p).ok());
	let Some(db) = db else { return (None, None) };
	let mut vendor_name = None;
	let mut in_vendor = false;
	for line in db.lines() {
		if line.starts_with('#') || line.is_empty() {
			continue;
		}
		if !line.starts_with('\t') {
			if in_vendor {
				break;
			}
			if let Some(rest) = line.strip_prefix(vendor) {
				vendor_name = Some(rest.trim().to_string());
				in_vendor = true;
			}
		} else if in_vendor
			&& !line.starts_with("\t\t")
			&& let Some(rest) = line.trim_start_matches('\t').strip_prefix(device)
		{
			return (vendor_name, Some(rest.trim().to_string()));
		}
	}
	(vendor_name, None)
}

/// The Vulkan device of the render node, from `vulkaninfo --summary`.
#[derive(Debug, Clone, PartialEq)]
pub struct VulkanInfo {
	pub device_name: String,
	pub driver_version: Option<String>,
}

/// Run `vulkaninfo --summary` and pick the device whose PCI ids match the render node Moonshine
/// would use. `None` when vulkaninfo isn't installed or lists no such device.
pub fn vulkan_info(gpu: Option<&str>) -> Option<VulkanInfo> {
	let out = std::process::Command::new("vulkaninfo")
		.arg("--summary")
		.output()
		.ok()?;
	let text = String::from_utf8_lossy(&out.stdout);
	let ids = render_device(gpu).and_then(|(_, dev)| Some((read(dev.join("vendor"))?, read(dev.join("device"))?)));
	parse_vulkaninfo(&text, ids.as_ref().map(|(v, d)| (v.as_str(), d.as_str())))
}

/// The device matching `ids` (sysfs `0x1002`, `0x73bf`), or the first non-CPU device.
fn parse_vulkaninfo(text: &str, ids: Option<(&str, &str)>) -> Option<VulkanInfo> {
	let mut devices: Vec<BTreeMap<&str, &str>> = Vec::new();
	for line in text.lines() {
		let line = line.trim();
		if line.starts_with("GPU") && line.ends_with(':') {
			devices.push(BTreeMap::new());
		} else if let (Some(device), Some((k, v))) = (devices.last_mut(), line.split_once('=')) {
			device.insert(k.trim(), v.trim());
		}
	}
	let same = |a: Option<&&str>, b: &str| {
		a.and_then(|a| u32::from_str_radix(a.trim_start_matches("0x"), 16).ok())
			== u32::from_str_radix(b.trim_start_matches("0x"), 16).ok()
	};
	let device = match ids {
		Some((vendor, id)) => devices
			.iter()
			.find(|d| same(d.get("vendorID"), vendor) && same(d.get("deviceID"), id)),
		None => devices
			.iter()
			.find(|d| d.get("deviceType") != Some(&"PHYSICAL_DEVICE_TYPE_CPU")),
	}?;
	Some(VulkanInfo {
		device_name: device.get("deviceName")?.to_string(),
		driver_version: device.get("driverVersion").map(|v| v.to_string()),
	})
}

pub fn identity(gpu: Option<&str>, vulkan: Option<&VulkanInfo>, labels: BTreeMap<String, String>) -> RunIdentity {
	RunIdentity {
		build: build_info(),
		host: host_info(),
		gpu: gpu_info(gpu, vulkan),
		labels,
	}
}

// ---------------------------------------------------------------------------------------------
// DRM fdinfo sampling
// ---------------------------------------------------------------------------------------------

/// Cumulative per-client counters read from one DRM fdinfo entry.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct DrmClient {
	pub driver: String,
	pub pdev: String,
	pub client_id: String,
	/// `drm-engine-<name>`: busy nanoseconds.
	pub engine_ns: BTreeMap<String, u64>,
	/// `drm-cycles-<name>` / `drm-total-cycles-<name>` (xe).
	pub cycles: BTreeMap<String, (u64, u64)>,
	/// Resident (or, for older kernels, `drm-memory-*`) bytes by region.
	pub memory: BTreeMap<String, u64>,
}

/// Parse one `/proc/<pid>/fdinfo/<fd>` file. `None` if it is not a DRM client.
pub fn parse_fdinfo(text: &str) -> Option<DrmClient> {
	let mut c = DrmClient::default();
	let mut legacy_memory = BTreeMap::new();
	let mut total_cycles = BTreeMap::new();
	for line in text.lines() {
		let Some((key, value)) = line.split_once(':') else {
			continue;
		};
		let value = value.trim();
		match key {
			"drm-driver" => c.driver = value.to_string(),
			"drm-pdev" => c.pdev = value.to_string(),
			"drm-client-id" => c.client_id = value.to_string(),
			_ => {
				if let Some(engine) = key.strip_prefix("drm-engine-") {
					if engine != "capacity"
						&& !engine.starts_with("capacity-")
						&& let Some(ns) = value.strip_suffix("ns").and_then(|v| v.trim().parse().ok())
					{
						c.engine_ns.insert(engine.to_string(), ns);
					}
				} else if let Some(engine) = key.strip_prefix("drm-total-cycles-") {
					if let Ok(v) = value.parse() {
						total_cycles.insert(engine.to_string(), v);
					}
				} else if let Some(engine) = key.strip_prefix("drm-cycles-") {
					if let Ok(v) = value.parse::<u64>() {
						c.cycles.entry(engine.to_string()).or_insert((0, 0)).0 = v;
					}
				} else if let Some(region) = key.strip_prefix("drm-resident-") {
					if let Some(b) = parse_bytes(value) {
						c.memory.insert(region.to_string(), b);
					}
				} else if let Some(region) = key.strip_prefix("drm-memory-")
					&& let Some(b) = parse_bytes(value)
				{
					legacy_memory.insert(region.to_string(), b);
				}
			},
		}
	}
	if c.driver.is_empty() {
		return None;
	}
	for (engine, total) in total_cycles {
		c.cycles.entry(engine).or_insert((0, 0)).1 = total;
	}
	if c.memory.is_empty() {
		c.memory = legacy_memory;
	}
	Some(c)
}

fn parse_bytes(v: &str) -> Option<u64> {
	let mut parts = v.split_whitespace();
	let n: u64 = parts.next()?.parse().ok()?;
	let mult = match parts.next() {
		None => 1,
		Some("KiB") => 1 << 10,
		Some("MiB") => 1 << 20,
		Some("GiB") => 1 << 30,
		Some(_) => return None,
	};
	Some(n * mult)
}

/// All DRM clients held by `pid`, deduplicated by (device, client id).
fn drm_clients(pid: u32) -> Vec<DrmClient> {
	let mut seen = std::collections::HashSet::new();
	let Ok(dir) = std::fs::read_dir(format!("/proc/{pid}/fdinfo")) else {
		return Vec::new();
	};
	dir.flatten()
		.filter_map(|e| std::fs::read_to_string(e.path()).ok())
		.filter_map(|t| parse_fdinfo(&t))
		.filter(|c| seen.insert((c.pdev.clone(), c.client_id.clone())))
		.collect()
}

/// systemd unit Moonshine launches the application in.
const APP_UNIT: &str = "/moonshine-session.service";

/// Processes belonging to the benchmarked application: everything below `root` in the process
/// tree (Xwayland and friends) plus everything in the transient `moonshine-session.service`
/// unit, which is where Moonshine starts the app (so it is not our descendant).
fn app_processes(root: u32) -> Vec<u32> {
	let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
	let mut out = Vec::new();
	if let Ok(dir) = std::fs::read_dir("/proc") {
		for e in dir.flatten() {
			let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
				continue;
			};
			if let Some(stat) = read(e.path().join("stat"))
				&& let Some(ppid) = stat_fields(&stat).and_then(|f| f.get(1).and_then(|v| v.parse().ok()))
			{
				children.entry(ppid).or_default().push(pid);
			}
			if read(e.path().join("cgroup")).is_some_and(|c| c.lines().any(|l| l.ends_with(APP_UNIT))) {
				out.push(pid);
			}
		}
	}
	let mut stack = vec![root];
	while let Some(p) = stack.pop() {
		for c in children.get(&p).into_iter().flatten() {
			out.push(*c);
			stack.push(*c);
		}
	}
	out.sort_unstable();
	out.dedup();
	out
}

/// Fields of `/proc/<pid>/stat` after the `(comm)` field: index 0 is `state`.
fn stat_fields(stat: &str) -> Option<Vec<&str>> {
	let rest = &stat[stat.rfind(')')? + 1..];
	Some(rest.split_whitespace().collect())
}

/// Samples GPU engine busy time and memory for the bench process (the pipeline) and the
/// application it launched, plus per-thread CPU time of the bench process.
///
/// Busy time is differenced per DRM client, so a client that appears mid-run contributes only
/// what it used since it was first seen, and one that disappears does not make the total go
/// backwards. Clients are deduplicated by (device, client id) across all processes, so an
/// inherited fd is counted once.
pub struct ResourceSampler {
	metrics: Option<RunMetrics>,
	/// The process that runs Moonshine's encoder and compositor.
	pid: u32,
	prev: Option<Snapshot>,
	/// `(busy, wall)` sums since [`ResourceSampler::start_window`]: busy seconds for GPU engines
	/// and CPU threads.
	window: BTreeMap<String, (f64, f64)>,
	gpu_supported: bool,
	#[cfg(feature = "nvml")]
	nvml: Option<nvml::NvmlSampler>,
}

/// Process groups, in priority order for deduplication.
const GROUPS: [&str; 2] = ["bench", "app"];

struct Snapshot {
	at: Instant,
	/// (pdev, client id) → (group index, counters)
	clients: HashMap<(String, String), (usize, DrmClient)>,
	cpu: BTreeMap<String, f64>,
}

fn collect_clients(groups: &[Vec<u32>]) -> HashMap<(String, String), (usize, DrmClient)> {
	let mut out = HashMap::new();
	for (g, pids) in groups.iter().enumerate() {
		for pid in pids {
			for c in drm_clients(*pid) {
				out.entry((c.pdev.clone(), c.client_id.clone())).or_insert((g, c));
			}
		}
	}
	out
}

impl ResourceSampler {
	/// Sample `pid`, the process running Moonshine's encoder and compositor, and its app.
	pub fn new(metrics: Option<RunMetrics>, pid: u32) -> Self {
		Self {
			metrics,
			pid,
			prev: None,
			window: BTreeMap::new(),
			gpu_supported: false,
			#[cfg(feature = "nvml")]
			nvml: nvml::NvmlSampler::new(),
		}
	}

	/// Whether any DRM fdinfo (or NVML) data was seen.
	pub fn gpu_supported(&self) -> bool {
		self.gpu_supported
	}

	/// Take a sample and start averaging from it (call when warmup ends).
	pub fn start_window(&mut self) {
		self.sample();
		self.window.clear();
	}

	/// Take one sample; the first call only establishes a baseline.
	pub fn sample(&mut self) {
		let now = Instant::now();
		let me = self.pid;
		let clients = collect_clients(&[vec![me], app_processes(me)]);
		self.gpu_supported |= !clients.is_empty();
		let cpu = thread_cpu_seconds(me);

		if let Some(prev) = self.prev.take() {
			let wall = now.duration_since(prev.at).as_secs_f64();
			if wall > 0.0 {
				self.record_gpu(&prev, &clients, wall);
				for (thread, secs) in &cpu {
					// A thread group that is new since the last sample started at zero.
					let delta = (secs - prev.cpu.get(thread).copied().unwrap_or(0.0)).max(0.0);
					if let Some(m) = &self.metrics
						&& delta > 0.0
					{
						m.record_cpu_time(thread, delta);
					}
					let w = self.window.entry(format!("cpu.bench.{thread}.util")).or_default();
					w.0 += delta;
					w.1 += wall;
				}
			}
		}
		#[cfg(feature = "nvml")]
		if let Some(n) = &mut self.nvml {
			self.gpu_supported |= n.sample(&self.metrics, me, &app_processes(me), &mut self.window);
		}
		self.prev = Some(Snapshot { at: now, clients, cpu });
	}

	fn record_gpu(&mut self, prev: &Snapshot, clients: &HashMap<(String, String), (usize, DrmClient)>, wall: f64) {
		let mut busy: [BTreeMap<String, f64>; 2] = Default::default();
		let mut memory: [BTreeMap<String, u64>; 2] = Default::default();
		for (key, (g, c)) in clients {
			for (region, bytes) in &c.memory {
				*memory[*g].entry(region.clone()).or_default() += bytes;
			}
			// Engines the group uses show up even when idle this interval.
			for engine in c.engine_ns.keys().chain(c.cycles.keys()) {
				busy[*g].entry(engine.clone()).or_default();
			}
			// A client first seen now only sets the baseline.
			let Some((_, p)) = prev.clients.get(key) else { continue };
			for (engine, ns) in &c.engine_ns {
				let before = p.engine_ns.get(engine).copied().unwrap_or(*ns);
				*busy[*g].entry(engine.clone()).or_default() += ns.saturating_sub(before) as f64 / 1e9;
			}
			// xe: busy share = Δcycles / Δtotal_cycles, where total cycles is a per-client read
			// of the shared GPU timestamp.
			for (engine, (cyc, total)) in &c.cycles {
				let (c0, t0) = p.cycles.get(engine).copied().unwrap_or((*cyc, *total));
				let dt = total.saturating_sub(t0);
				if dt > 0 {
					*busy[*g].entry(engine.clone()).or_default() += cyc.saturating_sub(c0) as f64 / dt as f64 * wall;
				}
			}
		}
		for (g, process) in GROUPS.iter().enumerate() {
			for (engine, busy_s) in &busy[g] {
				// Several contexts can run on one engine class at once; clamp to 100 %.
				let busy_s = busy_s.min(wall);
				if let Some(m) = &self.metrics {
					m.record_gpu_utilization(process, engine, busy_s / wall);
				}
				let w = self.window.entry(format!("gpu.{process}.{engine}.busy")).or_default();
				w.0 += busy_s;
				w.1 += wall;
			}
			if let Some(m) = &self.metrics {
				for (region, bytes) in &memory[g] {
					m.record_gpu_memory(process, region, *bytes);
				}
			}
		}
	}

	/// Mean utilization per key over the window: GPU engines as a busy share (0–1), CPU thread
	/// groups in cores.
	pub fn window_means(&self) -> BTreeMap<String, f64> {
		self.window
			.iter()
			.filter(|(_, (_, wall))| *wall > 0.0)
			.map(|(k, (busy, wall))| (k.clone(), busy / wall))
			.collect()
	}
}

/// CPU seconds (user + system) of `pid`'s threads, grouped by thread name with trailing digits
/// removed (`tokio-runtime-w`, `rayon-worker-`, ...).
pub fn thread_cpu_seconds(pid: u32) -> BTreeMap<String, f64> {
	// SAFETY: sysconf has no preconditions.
	let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
	let mut out = BTreeMap::new();
	let Ok(dir) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
		return out;
	};
	for e in dir.flatten() {
		let Some(stat) = read(e.path().join("stat")) else {
			continue;
		};
		let name = read(e.path().join("comm")).unwrap_or_default();
		let name = name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '-' || c == ' ' || c == '#');
		let name = if name.is_empty() { "unnamed" } else { name };
		// After (comm): state=0, ..., utime=11, stime=12
		let Some(f) = stat_fields(&stat) else { continue };
		let (Some(u), Some(s)) = (f.get(11), f.get(12)) else {
			continue;
		};
		let (Ok(u), Ok(s)) = (u.parse::<u64>(), s.parse::<u64>()) else {
			continue;
		};
		*out.entry(name.to_string()).or_insert(0.0) += (u + s) as f64 / ticks;
	}
	out
}

#[cfg(feature = "nvml")]
mod nvml {
	//! Proprietary Nvidia driver: no DRM fdinfo engine stats, so ask NVML instead.

	use std::collections::BTreeMap;

	use moonshine_otel_exporter::metrics::RunMetrics;
	use nvml_wrapper::Nvml;

	pub struct NvmlSampler {
		nvml: Nvml,
		last_ts: u64,
	}

	impl NvmlSampler {
		pub fn new() -> Option<Self> {
			Nvml::init().ok().map(|nvml| Self { nvml, last_ts: 0 })
		}

		pub fn sample(
			&mut self,
			metrics: &Option<RunMetrics>,
			me: u32,
			app: &[u32],
			window: &mut BTreeMap<String, (f64, f64)>,
		) -> bool {
			let Ok(device) = self.nvml.device_by_index(0) else {
				return false;
			};
			let Ok(samples) = device.process_utilization_stats(self.last_ts) else {
				return false;
			};
			let mut any = false;
			for s in samples {
				let process = if s.pid == me {
					"bench"
				} else if app.contains(&s.pid) {
					"app"
				} else {
					continue;
				};
				any = true;
				self.last_ts = self.last_ts.max(s.timestamp);
				for (engine, pct) in [("sm", s.sm_util), ("enc", s.enc_util), ("dec", s.dec_util)] {
					let util = pct as f64 / 100.0;
					if let Some(m) = metrics {
						m.record_gpu_utilization(process, engine, util);
					}
					// Treated as one unit of wall time per sample, so the mean is the mean utilization.
					let w = window.entry(format!("gpu.{process}.{engine}.busy")).or_default();
					w.0 += util;
					w.1 += 1.0;
				}
			}
			any
		}
	}
}

/// Resource sampling period.
const SAMPLE_PERIOD: Duration = Duration::from_secs(1);

/// Run a [`ResourceSampler`] on its own thread until the returned sender fires (or is
/// dropped). Its averaging window starts at `warm_at`.
pub fn spawn_sampler(
	metrics: Option<RunMetrics>,
	pid: u32,
	warm_at: Instant,
) -> anyhow::Result<(std::sync::mpsc::Sender<()>, std::thread::JoinHandle<ResourceSampler>)> {
	let (tx, rx) = std::sync::mpsc::channel::<()>();
	let handle = std::thread::Builder::new()
		.name("resource-sampler".into())
		.spawn(move || {
			let mut sampler = ResourceSampler::new(metrics, pid);
			sampler.sample();
			let mut windowed = false;
			while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(SAMPLE_PERIOD) {
				if !windowed && Instant::now() >= warm_at {
					sampler.start_window();
					windowed = true;
				} else {
					sampler.sample();
				}
			}
			sampler.sample();
			sampler
		})
		.context("starting the resource sampler")?;
	Ok((tx, handle))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn picks_the_render_nodes_vulkan_device() {
		let summary = "\
Devices:
========
GPU0:
	apiVersion         = 1.4.311
	driverVersion      = 25.1.4
	vendorID           = 0x8086
	deviceID           = 0x56a0
	deviceType         = PHYSICAL_DEVICE_TYPE_DISCRETE_GPU
	deviceName         = Intel(R) Arc(tm) A770 Graphics (DG2)
GPU1:
	apiVersion         = 1.4.311
	driverVersion      = 25.1.5
	vendorID           = 0x1002
	deviceID           = 0x73bf
	deviceType         = PHYSICAL_DEVICE_TYPE_DISCRETE_GPU
	deviceName         = AMD Radeon RX 6800 XT (RADV NAVI21)
GPU2:
	vendorID           = 0x10005
	deviceType         = PHYSICAL_DEVICE_TYPE_CPU
	deviceName         = llvmpipe (LLVM 20.1.8, 256 bits)
";
		let amd = parse_vulkaninfo(summary, Some(("0x1002", "0x73bf"))).unwrap();
		assert_eq!(amd.device_name, "AMD Radeon RX 6800 XT (RADV NAVI21)");
		assert_eq!(amd.driver_version.as_deref(), Some("25.1.5"));
		assert_eq!(
			parse_vulkaninfo(summary, None).unwrap().driver_version.as_deref(),
			Some("25.1.4")
		);
		assert_eq!(parse_vulkaninfo(summary, Some(("0x10de", "0x2684"))), None);
		assert_eq!(parse_vulkaninfo("", None), None);
	}

	#[test]
	fn amdgpu_fdinfo() {
		let text = "pos:\t0\nflags:\t02100002\nmnt_id:\t27\nino:\t1234\ndrm-driver:\tamdgpu\ndrm-client-id:\t42\ndrm-pdev:\t0000:03:00.0\npasid:\t32790\ndrm-memory-vram:\t102400 KiB\ndrm-memory-gtt:\t2048 KiB\ndrm-memory-cpu:\t0 KiB\ndrm-resident-vram:\t98304 KiB\ndrm-engine-gfx:\t1540000 ns\ndrm-engine-enc:\t320000 ns\ndrm-engine-capacity-gfx:\t1\n";
		let c = parse_fdinfo(text).unwrap();
		assert_eq!(c.driver, "amdgpu");
		assert_eq!(c.client_id, "42");
		assert_eq!(c.engine_ns["gfx"], 1_540_000);
		assert_eq!(c.engine_ns["enc"], 320_000);
		assert!(!c.engine_ns.contains_key("capacity-gfx"));
		// drm-resident-* wins over the legacy drm-memory-* keys.
		assert_eq!(c.memory, BTreeMap::from([("vram".to_string(), 98304 * 1024)]));
	}

	#[test]
	fn legacy_memory_keys() {
		let c =
			parse_fdinfo("drm-driver: i915\ndrm-client-id: 7\ndrm-engine-render: 10 ns\ndrm-memory-system: 4 MiB\n")
				.unwrap();
		assert_eq!(c.memory["system"], 4 << 20);
		assert_eq!(c.engine_ns["render"], 10);
	}

	#[test]
	fn xe_cycles() {
		let c = parse_fdinfo(
			"drm-driver: xe\ndrm-client-id: 3\ndrm-cycles-vcs: 500\ndrm-total-cycles-vcs: 2000\ndrm-resident-vram0: 1024 KiB\n",
		)
		.unwrap();
		assert_eq!(c.cycles["vcs"], (500, 2000));
	}

	#[test]
	fn not_drm() {
		assert!(parse_fdinfo("pos: 0\nflags: 02\n").is_none());
	}

	#[test]
	fn thread_cpu_is_readable() {
		let cpu = thread_cpu_seconds(std::process::id());
		assert!(!cpu.is_empty());
	}

	#[test]
	fn stat_fields_handle_parens_in_comm() {
		let f = stat_fields("123 (weird) name) S 1 2 3").unwrap();
		assert_eq!(f[0], "S");
		assert_eq!(f[1], "1");
	}
}
