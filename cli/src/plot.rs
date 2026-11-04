//! `plot`: charts for pull requests, as SVG and PNG (GitHub takes PNG attachments).
//!
//! - `clicks`: the distribution of click-to-flash latency per build, as violins with every sample drawn, from
//!   gamestream-test-client's JSON (`click_ms`).
//! - `timeline`: one stream over time, from gamestream-test-client's per-frame CSV: keyframes, recovery frames,
//!   frames that never arrived or failed to decode, the gap before each frame, and each frame's size.
//! - `pacing`: frame time through each stream (the gap after the frame before), with a histogram, from gamestream-test-client CSVs.
//! - `stages`: the time each of Moonshine's pipeline stages takes, per stream, from `watch`'s frames CSVs.
//!
//! The SVG is written directly; the PNG is the same SVG rendered with resvg, with DejaVu Sans built in so the
//! output doesn't depend on the fonts installed.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::{Args, Subcommand};

const FONT: &[u8] = include_bytes!("../fonts/DejaVuSans.ttf");
const FONT_BOLD: &[u8] = include_bytes!("../fonts/DejaVuSans-Bold.ttf");
const FAMILY: &str = "DejaVu Sans";

const WIDTH: f64 = 1440.0;
const HEIGHT: f64 = 960.0;
/// Fill colors for the groups, and a darker shade of each for its dots.
const COLORS: [(&str, &str); 6] = [
	("#8da9d4", "#4a6fae"),
	("#ecb294", "#d27a4c"),
	("#9ccb9f", "#4f9a55"),
	("#c5a8d8", "#8a5cab"),
	("#e8d38f", "#b39a2e"),
	("#9fd0d6", "#3f9aa5"),
];
const GRID: &str = "#e2e2e2";
const INK: &str = "#222222";
const MUTED: &str = "#666666";

#[derive(Args, Debug)]
pub struct PlotArgs {
	#[command(subcommand)]
	kind: PlotKind,
}

#[derive(Subcommand, Debug)]
enum PlotKind {
	/// Click-to-flash latency per build, from gamestream-test-client JSON files (`click_ms`).
	Clicks(ClicksArgs),
	/// One stream's frame arrivals over time, from a gamestream-test-client CSV (`--csv`).
	Timeline(TimelineArgs),
	/// Frame time through each stream, from gamestream-test-client CSVs.
	Pacing(GroupArgs),
	/// Time per pipeline stage, per stream, from `moonshine-otel watch` frames CSVs.
	Stages(GroupArgs),
}

#[derive(Args, Debug)]
struct GroupArgs {
	/// NAME=FILE, or FILE (named after it). Files with the same NAME are one group.
	#[arg(required = true)]
	inputs: Vec<String>,
	/// Output path without extension (NAME.svg and NAME.png are written).
	#[arg(long)]
	out: PathBuf,
	/// Chart title.
	#[arg(long)]
	title: Option<String>,
}

#[derive(Args, Debug)]
struct ClicksArgs {
	/// NAME=FILE, or FILE (named after it). Files with the same NAME are one group.
	#[arg(required = true)]
	inputs: Vec<String>,
	/// Output path without extension (NAME.svg and NAME.png are written).
	#[arg(long, default_value = "click-latency")]
	out: PathBuf,
	/// Chart title.
	#[arg(long, default_value = "Click-to-flash latency")]
	title: String,
}

#[derive(Args, Debug)]
struct TimelineArgs {
	/// gamestream-test-client's per-frame CSV.
	csv: PathBuf,
	/// Output path without extension (NAME.svg and NAME.png are written).
	#[arg(long, default_value = "timeline")]
	out: PathBuf,
	/// Chart title (default: the CSV's file name).
	#[arg(long)]
	title: Option<String>,
}

pub fn plot(args: &PlotArgs) -> anyhow::Result<()> {
	let (svg, out) = match &args.kind {
		PlotKind::Clicks(a) => (clicks_svg(&read_click_groups(&a.inputs)?, &a.title), &a.out),
		PlotKind::Timeline(a) => {
			let title = a.title.clone().unwrap_or_else(|| {
				a.csv
					.file_name()
					.map(|n| n.to_string_lossy().into_owned())
					.unwrap_or_default()
			});
			(timeline_svg(&read_frames(&a.csv)?, &title), &a.out)
		},
		PlotKind::Pacing(a) => {
			let mut entries = Vec::new();
			for input in &a.inputs {
				let (name, path) = named_input(input);
				entries.push((name, read_gaps(&path)?));
			}
			let title = a.title.as_deref().unwrap_or("Frame pacing");
			(pacing_svg(&merge_groups(entries), title, None), &a.out)
		},
		PlotKind::Stages(a) => {
			let mut entries = Vec::new();
			for input in &a.inputs {
				let (name, path) = named_input(input);
				entries.push((name, read_stage_rows(&path)?));
			}
			let groups: Vec<(String, StageSummary)> = merge_groups(entries)
				.into_iter()
				.filter_map(|(name, rows)| summarize_stages(&rows).map(|s| (name, s)))
				.collect();
			if groups.is_empty() {
				bail!("no frames after warmup in the inputs");
			}
			let title = a.title.as_deref().unwrap_or("Server pipeline stages");
			(stages_svg(&groups, title), &a.out)
		},
	};
	write_outputs(&svg, out)
}

fn write_outputs(svg: &str, out: &Path) -> anyhow::Result<()> {
	let svg_path = out.with_extension("svg");
	let png_path = out.with_extension("png");
	std::fs::write(&svg_path, svg).with_context(|| format!("writing {}", svg_path.display()))?;
	std::fs::write(&png_path, render_png(svg)?).with_context(|| format!("writing {}", png_path.display()))?;
	println!("wrote {} and {}", svg_path.display(), png_path.display());
	Ok(())
}

/// Render an SVG to PNG with the built-in fonts.
pub(crate) fn render_png(svg: &str) -> anyhow::Result<Vec<u8>> {
	use resvg::{tiny_skia, usvg};
	let mut options = usvg::Options {
		font_family: FAMILY.to_string(),
		..Default::default()
	};
	options.fontdb_mut().load_font_data(FONT.to_vec());
	options.fontdb_mut().load_font_data(FONT_BOLD.to_vec());
	let tree = usvg::Tree::from_str(svg, &options).context("parsing the chart's SVG")?;
	let size = tree.size().to_int_size();
	let mut pixmap = tiny_skia::Pixmap::new(size.width(), size.height()).context("allocating the PNG")?;
	resvg::render(&tree, tiny_skia::Transform::default(), &mut pixmap.as_mut());
	pixmap.encode_png().context("encoding the PNG")
}

// ---------------------------------------------------------------------------------------------
// SVG building
// ---------------------------------------------------------------------------------------------

/// How a piece of text is set. `anchor`: start, middle or end.
#[derive(Clone, Copy)]
struct Font {
	size: f64,
	anchor: &'static str,
	bold: bool,
	fill: &'static str,
}

impl Font {
	const fn new(size: f64, anchor: &'static str, bold: bool, fill: &'static str) -> Self {
		Self {
			size,
			anchor,
			bold,
			fill,
		}
	}
}

struct Svg(String);

impl Svg {
	fn new() -> Self {
		Self::with_height(HEIGHT)
	}

	fn with_height(height: f64) -> Self {
		let mut s = String::new();
		let _ = write!(
			s,
			r##"<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{height}" viewBox="0 0 {WIDTH} {height}" font-family="{FAMILY}"><rect width="100%" height="100%" fill="#fafafa"/>"##
		);
		Self(s)
	}

	fn line(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, stroke: &str, width: f64) {
		let _ = write!(
			self.0,
			r#"<line x1="{x1:.1}" y1="{y1:.1}" x2="{x2:.1}" y2="{y2:.1}" stroke="{stroke}" stroke-width="{width}"/>"#
		);
	}

	fn circle(&mut self, cx: f64, cy: f64, r: f64, fill: &str, opacity: f64) {
		let _ = write!(
			self.0,
			r#"<circle cx="{cx:.1}" cy="{cy:.1}" r="{r}" fill="{fill}" fill-opacity="{opacity}"/>"#
		);
	}

	fn path(&mut self, d: &str, fill: &str, opacity: f64) {
		let _ = write!(self.0, r#"<path d="{d}" fill="{fill}" fill-opacity="{opacity}"/>"#);
	}

	fn text(&mut self, x: f64, y: f64, text: &str, font: Font) {
		let Font {
			size,
			anchor,
			bold,
			fill,
		} = font;
		let weight = if bold { "bold" } else { "normal" };
		let _ = write!(
			self.0,
			r#"<text x="{x:.1}" y="{y:.1}" font-size="{size}" text-anchor="{anchor}" font-weight="{weight}" fill="{fill}">{}</text>"#,
			escape(text)
		);
	}

	fn rotated_text(&mut self, x: f64, y: f64, text: &str, size: f64) {
		let _ = write!(
			self.0,
			r#"<text x="{x:.1}" y="{y:.1}" font-size="{size}" text-anchor="middle" fill="{INK}" transform="rotate(-90 {x:.1} {y:.1})">{}</text>"#,
			escape(text)
		);
	}

	fn finish(mut self) -> String {
		self.0.push_str("</svg>\n");
		self.0
	}
}

fn escape(s: &str) -> String {
	s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Round tick values covering [lo, hi], about `target` of them.
fn ticks(lo: f64, hi: f64, target: usize) -> Vec<f64> {
	let span = (hi - lo).max(1e-9);
	let raw = span / target as f64;
	let mag = 10f64.powf(raw.log10().floor());
	let step = [1.0, 2.0, 5.0, 10.0]
		.into_iter()
		.map(|m| m * mag)
		.find(|s| span / s <= target as f64)
		.unwrap_or(10.0 * mag);
	let mut t = (lo / step).ceil() * step;
	let mut out = Vec::new();
	while t <= hi + step * 1e-9 {
		out.push(t);
		t += step;
	}
	out
}

fn fmt_tick(v: f64) -> String {
	if v.fract().abs() < 1e-9 {
		format!("{v:.0}")
	} else {
		format!("{v:.1}")
	}
}

/// The y axis: horizontal grid lines with labels, and the axis title.
fn y_axis(svg: &mut Svg, plot: &Area, lo: f64, hi: f64, label: &str) {
	for t in ticks(lo, hi, 7) {
		let y = plot.y(t, lo, hi);
		svg.line(plot.left, y, plot.right, y, GRID, 1.0);
		svg.text(
			plot.left - 12.0,
			y + 6.0,
			&fmt_tick(t),
			Font::new(18.0, "end", false, INK),
		);
	}
	svg.line(plot.left, plot.top, plot.left, plot.bottom, INK, 1.5);
	svg.line(plot.left, plot.bottom, plot.right, plot.bottom, INK, 1.5);
	svg.rotated_text(34.0, (plot.top + plot.bottom) / 2.0, label, 20.0);
}

struct Area {
	left: f64,
	right: f64,
	top: f64,
	bottom: f64,
}

impl Area {
	fn y(&self, v: f64, lo: f64, hi: f64) -> f64 {
		self.bottom - (v - lo) / (hi - lo) * (self.bottom - self.top)
	}
}

pub(crate) fn quantile(sorted: &[f64], q: f64) -> f64 {
	if sorted.is_empty() {
		return f64::NAN;
	}
	let pos = q * (sorted.len() - 1) as f64;
	let (i, f) = (pos.floor() as usize, pos.fract());
	if i + 1 < sorted.len() {
		sorted[i] * (1.0 - f) + sorted[i + 1] * f
	} else {
		sorted[i]
	}
}

// ---------------------------------------------------------------------------------------------
// clicks
// ---------------------------------------------------------------------------------------------

/// Samples per group name, in the order the names first appear.
fn read_click_groups(inputs: &[String]) -> anyhow::Result<Vec<(String, Vec<f64>)>> {
	let mut entries = Vec::new();
	for input in inputs {
		let (name, path) = named_input(input);
		let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
		let value: serde_json::Value =
			serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
		let samples = value
			.get("click_ms")
			.and_then(|v| v.as_array())
			.with_context(|| {
				format!(
					"{} has no click_ms (was it streamed with --click-every?)",
					path.display()
				)
			})?
			.iter()
			.filter_map(|v| v.as_f64())
			.collect::<Vec<_>>();
		entries.push((name, samples));
	}
	let out = merge_groups(entries);
	if out.iter().all(|(_, s)| s.is_empty()) {
		bail!("no click samples in the inputs");
	}
	Ok(out)
}

/// Gaussian kernel density of `sorted` at `y`, with Silverman's bandwidth `h`.
fn density(sorted: &[f64], h: f64, y: f64) -> f64 {
	sorted.iter().map(|v| (-0.5 * ((y - v) / h).powi(2)).exp()).sum::<f64>() / (sorted.len() as f64 * h)
}

fn bandwidth(sorted: &[f64]) -> f64 {
	let n = sorted.len() as f64;
	let mean = sorted.iter().sum::<f64>() / n;
	let sd = (sorted.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0).max(1.0)).sqrt();
	let iqr = quantile(sorted, 0.75) - quantile(sorted, 0.25);
	let spread = if iqr > 0.0 { sd.min(iqr / 1.34) } else { sd };
	(1.06 * spread * n.powf(-0.2)).max(0.05)
}

/// A small deterministic hash in [0, 1), for jittering dots the same way every time.
fn jitter(i: usize) -> f64 {
	let mut x = (i as u64).wrapping_mul(0x9E3779B97F4A7C15);
	x ^= x >> 29;
	x = x.wrapping_mul(0xBF58476D1CE4E5B9);
	x ^= x >> 32;
	(x >> 11) as f64 / (1u64 << 53) as f64
}

pub(crate) fn clicks_svg(groups: &[(String, Vec<f64>)], title: &str) -> String {
	let mut svg = Svg::new();
	let plot = Area {
		left: 110.0,
		right: WIDTH - 40.0,
		top: 110.0,
		bottom: HEIGHT - 140.0,
	};
	let all: Vec<f64> = groups.iter().flat_map(|(_, s)| s.iter().copied()).collect();
	let (min, max) = all
		.iter()
		.fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), v| (a.min(*v), b.max(*v)));
	let pad = ((max - min) * 0.08).max(1.0);
	let (lo, hi) = ((min - pad).max(0.0), max + pad);

	let total: usize = groups.iter().map(|(_, s)| s.len()).sum();
	svg.text(plot.left - 10.0, 52.0, title, Font::new(32.0, "start", true, INK));
	svg.text(
		plot.left - 10.0,
		84.0,
		&format!("click to decoded picture, {total} clicks"),
		Font::new(19.0, "start", false, MUTED),
	);
	y_axis(&mut svg, &plot, lo, hi, "Latency (ms)");

	let slot = (plot.right - plot.left) / groups.len() as f64;
	let half_width = (slot * 0.32).min(150.0);
	for (gi, (name, samples)) in groups.iter().enumerate() {
		let (fill, dot) = COLORS[gi % COLORS.len()];
		let cx = plot.left + slot * (gi as f64 + 0.5);
		let mut sorted = samples.clone();
		sorted.sort_by(f64::total_cmp);
		if sorted.is_empty() {
			svg.text(cx, plot.bottom + 40.0, name, Font::new(22.0, "middle", false, INK));
			svg.text(
				cx,
				plot.bottom + 68.0,
				"(no samples)",
				Font::new(18.0, "middle", false, MUTED),
			);
			continue;
		}
		let (gmin, gmax) = (sorted[0], sorted[sorted.len() - 1]);
		if sorted.len() >= 3 && gmax > gmin {
			// The violin: density from the group's lowest to highest sample, widest at half_width.
			let h = bandwidth(&sorted);
			let steps = 120;
			let pts: Vec<(f64, f64)> = (0..=steps)
				.map(|i| {
					let y = gmin + (gmax - gmin) * i as f64 / steps as f64;
					(y, density(&sorted, h, y))
				})
				.collect();
			let peak = pts.iter().map(|p| p.1).fold(0.0, f64::max).max(1e-12);
			let mut d = String::new();
			for (i, (y, v)) in pts.iter().enumerate() {
				let _ = write!(
					d,
					"{}{:.1},{:.1} ",
					if i == 0 { "M" } else { "L" },
					cx - v / peak * half_width,
					plot.y(*y, lo, hi)
				);
			}
			for (y, v) in pts.iter().rev() {
				let _ = write!(d, "L{:.1},{:.1} ", cx + v / peak * half_width, plot.y(*y, lo, hi));
			}
			d.push('Z');
			svg.path(&d, fill, 0.9);
		}
		for (i, v) in samples.iter().enumerate() {
			let x = cx + (jitter(i + gi * 7919) - 0.5) * half_width * 0.35;
			svg.circle(x, plot.y(*v, lo, hi), 5.0, dot, 0.85);
		}
		let median = quantile(&sorted, 0.5);
		let y = plot.y(median, lo, hi);
		svg.line(cx - half_width * 0.5, y, cx + half_width * 0.5, y, "#111111", 3.0);
		svg.text(cx, plot.bottom + 38.0, name, Font::new(22.0, "middle", false, INK));
		svg.text(
			cx,
			plot.bottom + 66.0,
			&format!("median {:.1} ms, p95 {:.1} ms", median, quantile(&sorted, 0.95)),
			Font::new(18.0, "middle", false, MUTED),
		);
		svg.text(
			cx,
			plot.bottom + 90.0,
			&format!("{} clicks", sorted.len()),
			Font::new(16.0, "middle", false, MUTED),
		);
	}
	svg.finish()
}

// ---------------------------------------------------------------------------------------------
// shared input handling
// ---------------------------------------------------------------------------------------------

/// `NAME=FILE`, or `FILE` named after its file stem.
fn named_input(input: &str) -> (String, PathBuf) {
	match input.split_once('=') {
		Some((n, p)) if !n.is_empty() => (n.to_string(), PathBuf::from(p)),
		_ => {
			let p = PathBuf::from(input);
			let stem = p
				.file_stem()
				.map(|s| s.to_string_lossy().into_owned())
				.unwrap_or_default();
			(stem, p)
		},
	}
}

/// Merge entries with the same name, keeping the order the names first appear in.
fn merge_groups<T>(entries: Vec<(String, Vec<T>)>) -> Vec<(String, Vec<T>)> {
	let mut out: Vec<(String, Vec<T>)> = Vec::new();
	for (name, values) in entries {
		match out.iter_mut().find(|(n, _)| *n == name) {
			Some((_, existing)) => existing.extend(values),
			None => out.push((name, values)),
		}
	}
	out
}

/// A CSV's rows, with a lookup from column name to index.
pub(crate) struct Table {
	path: PathBuf,
	headers: csv::StringRecord,
	pub(crate) rows: Vec<csv::StringRecord>,
}

impl Table {
	pub(crate) fn read(path: &Path) -> anyhow::Result<Self> {
		let mut reader = csv::Reader::from_path(path).with_context(|| format!("reading {}", path.display()))?;
		let headers = reader.headers()?.clone();
		let rows = reader
			.records()
			.collect::<Result<Vec<_>, _>>()
			.with_context(|| format!("reading {}", path.display()))?;
		Ok(Self {
			path: path.to_path_buf(),
			headers,
			rows,
		})
	}

	pub(crate) fn column(&self, name: &str) -> anyhow::Result<usize> {
		self.optional_column(name)
			.with_context(|| format!("{} has no {name} column", self.path.display()))
	}

	pub(crate) fn optional_column(&self, name: &str) -> Option<usize> {
		self.headers.iter().position(|h| h == name)
	}
}

pub(crate) fn field(row: &csv::StringRecord, i: usize) -> &str {
	row.get(i).unwrap_or("")
}

pub(crate) fn number(row: &csv::StringRecord, i: usize) -> f64 {
	field(row, i).parse().unwrap_or(0.0)
}

/// The x axis along `plot.bottom`, with tick labels and a title.
fn x_axis(svg: &mut Svg, plot: &Area, lo: f64, hi: f64, label: &str) {
	for t in ticks(lo, hi, 10) {
		let px = plot.left + (t - lo) / (hi - lo) * (plot.right - plot.left);
		svg.line(px, plot.bottom, px, plot.bottom + 8.0, INK, 1.5);
		svg.text(
			px,
			plot.bottom + 32.0,
			&fmt_tick(t),
			Font::new(18.0, "middle", false, INK),
		);
	}
	svg.text(
		(plot.left + plot.right) / 2.0,
		plot.bottom + 70.0,
		label,
		Font::new(20.0, "middle", false, INK),
	);
}

/// Legend entries in a row ending at `right`, each a colored swatch and a label.
fn legend(svg: &mut Svg, right: f64, y: f64, entries: &[(&str, &str)]) {
	// DejaVu Sans at 17px averages about 9.5px per character.
	let width = |label: &str| 30.0 + label.chars().count() as f64 * 9.5 + 28.0;
	let mut x = right - entries.iter().map(|(_, l)| width(l)).sum::<f64>();
	for (color, label) in entries {
		let _ = write!(
			svg.0,
			r#"<rect x="{x:.1}" y="{:.1}" width="22" height="12" rx="2" fill="{color}"/>"#,
			y - 12.0
		);
		svg.text(x + 30.0, y, label, Font::new(17.0, "start", false, INK));
		x += width(label);
	}
}

fn header(svg: &mut Svg, left: f64, title: &str, subtitle: &str) {
	svg.text(left - 10.0, 52.0, title, Font::new(32.0, "start", true, INK));
	svg.text(left - 10.0, 84.0, subtitle, Font::new(19.0, "start", false, MUTED));
}

// ---------------------------------------------------------------------------------------------
// timeline
// ---------------------------------------------------------------------------------------------

const KEY: &str = "#d27a4c";
const RECOVERY: &str = "#2e9e6a";
const LOST: &str = "#c0392b";
const FAILED: &str = "#8a5cab";

pub(crate) struct FrameRow {
	pub(crate) number: i64,
	pub(crate) keyframe: bool,
	/// A recovery frame after a reference frame invalidation (the CSV's optional `recovery` column).
	pub(crate) recovery: bool,
	pub(crate) receive_us: u64,
	pub(crate) bytes: u64,
	pub(crate) failed: bool,
}

pub(crate) fn read_frames(path: &Path) -> anyhow::Result<Vec<FrameRow>> {
	let table = Table::read(path)?;
	let (number_col, kind, receive, status, bytes) = (
		table.column("frame_number")?,
		table.column("frame_type")?,
		table.column("receive_us")?,
		table.column("decode_status")?,
		table.column("bytes")?,
	);
	let recovery = table.optional_column("recovery");
	let out: Vec<FrameRow> = table
		.rows
		.iter()
		.map(|r| FrameRow {
			number: field(r, number_col).parse().unwrap_or(0),
			keyframe: field(r, kind) == "1",
			recovery: recovery.is_some_and(|i| field(r, i) == "1"),
			receive_us: field(r, receive).parse().unwrap_or(0),
			bytes: field(r, bytes).parse().unwrap_or(0),
			failed: field(r, status) != "0",
		})
		.collect();
	if out.len() < 2 {
		bail!("{} has fewer than two frames", path.display());
	}
	Ok(out)
}

/// Three panels over one time axis: a row of marks for keyframes, recovery frames, frames that never arrived and
/// frames that failed to decode; the gap before each frame; and each frame's size.
pub(crate) fn timeline_svg(frames: &[FrameRow], title: &str) -> String {
	let mut svg = Svg::new();
	let (left, right) = (130.0, WIDTH - 40.0);
	let marks = Area {
		left,
		right,
		top: 128.0,
		bottom: 204.0,
	};
	let gap_plot = Area {
		left,
		right,
		top: 226.0,
		bottom: 560.0,
	};
	let size_plot = Area {
		left,
		right,
		top: 600.0,
		bottom: 836.0,
	};
	let t0 = frames[0].receive_us;
	let seconds = |f: &FrameRow| (f.receive_us.saturating_sub(t0)) as f64 / 1e6;
	let (x_lo, x_hi) = (0.0, seconds(&frames[frames.len() - 1]).max(1.0));
	let x = |s: f64| left + (s - x_lo) / (x_hi - x_lo) * (right - left);

	let lost_at: Vec<f64> = frames
		.windows(2)
		.filter(|w| w[1].number - w[0].number > 1)
		.map(|w| seconds(&w[1]))
		.collect();
	let missing: i64 = frames.windows(2).map(|w| (w[1].number - w[0].number - 1).max(0)).sum();
	let keyframes = frames.iter().filter(|f| f.keyframe).count();
	let recoveries = frames.iter().filter(|f| f.recovery).count();
	header(
		&mut svg,
		left,
		title,
		&format!(
			"{} frames, {keyframes} keyframes, {recoveries} recovery frames, {missing} never arrived",
			frames.len()
		),
	);

	// Marks: one row per kind of event.
	let rows: [(&str, &str, Vec<f64>); 4] = [
		(
			"keyframe",
			KEY,
			frames.iter().filter(|f| f.keyframe).map(seconds).collect(),
		),
		(
			"recovery",
			RECOVERY,
			frames.iter().filter(|f| f.recovery).map(seconds).collect(),
		),
		("lost", LOST, lost_at),
		(
			"failed",
			FAILED,
			frames.iter().filter(|f| f.failed).map(seconds).collect(),
		),
	];
	let row_height = (marks.bottom - marks.top) / rows.len() as f64;
	for (i, (label, color, times)) in rows.iter().enumerate() {
		let y = marks.top + row_height * i as f64;
		svg.text(
			left - 12.0,
			y + row_height * 0.75,
			label,
			Font::new(15.0, "end", false, MUTED),
		);
		svg.line(left, y + row_height / 2.0, right, y + row_height / 2.0, GRID, 1.0);
		for t in times {
			svg.line(x(*t), y + 3.0, x(*t), y + row_height - 3.0, color, 2.5);
		}
	}

	timeline_gaps(&mut svg, &gap_plot, frames, &x, &seconds);
	timeline_sizes(&mut svg, &size_plot, frames, &x, &seconds);
	x_axis(&mut svg, &size_plot, x_lo, x_hi, "Time (s)");
	legend(
		&mut svg,
		right,
		112.0,
		&[
			(KEY, "keyframe"),
			(RECOVERY, "recovery frame"),
			(LOST, "never arrived"),
			(FAILED, "decode failed"),
		],
	);
	svg.finish()
}

// ---------------------------------------------------------------------------------------------
// pacing
// ---------------------------------------------------------------------------------------------

/// The gaps between consecutive frames' arrivals, in milliseconds, from a gamestream-test-client CSV.
fn read_gaps(path: &Path) -> anyhow::Result<Vec<f64>> {
	let frames = read_frames(path)?;
	Ok(frames
		.windows(2)
		.map(|w| (w[1].receive_us.saturating_sub(w[0].receive_us)) as f64 / 1e3)
		.collect())
}

/// Frame time over the stream, one panel per group: each frame's gap after the one before, against when it arrived,
/// with the target interval dashed and a sideways histogram of the gaps on the right. Gaps past the axis are drawn as
/// markers on its top edge.
pub(crate) fn pacing_svg(groups: &[(String, Vec<f64>)], title: &str, target_ms: Option<f64>) -> String {
	let groups: Vec<&(String, Vec<f64>)> = groups.iter().filter(|(_, g)| !g.is_empty()).collect();
	let n = groups.len().max(1) as f64;
	let (top, gap, bottom_pad) = (160.0, 80.0, 110.0);
	let panel_h = if n == 1.0 { HEIGHT - top - bottom_pad } else { 320.0 };
	let mut svg = Svg::with_height(top + n * panel_h + (n - 1.0) * gap + bottom_pad);
	let (left, right) = (130.0, WIDTH - 300.0);
	let (hist_l, hist_r) = (WIDTH - 270.0, WIDTH - 60.0);

	let sorted: Vec<Vec<f64>> = groups
		.iter()
		.map(|(_, g)| {
			let mut s = g.clone();
			s.sort_by(f64::total_cmp);
			s
		})
		.collect();
	let longest = sorted.iter().map(|s| s[s.len() - 1]).fold(0.0, f64::max);
	let typical = sorted.iter().map(|s| quantile(s, 0.5)).fold(0.0, f64::max).max(0.1);
	let usual = target_ms.unwrap_or(typical).max(typical);
	// One y range for every panel; past four usual intervals the axis is cut.
	let y_hi = (longest * 1.08).max(usual * 1.5).min(usual * 4.0);
	let t_hi = groups
		.iter()
		.map(|(_, g)| g.iter().sum::<f64>() / 1e3)
		.fold(0.0, f64::max)
		.max(0.1);

	let cut = if longest > y_hi {
		format!("; axis cut at {y_hi:.0} ms, longest gap {longest:.0} ms")
	} else {
		String::new()
	};
	let target_text = target_ms
		.map(|t| format!(", target {t:.1} ms ({:.0} fps) dashed", 1000.0 / t))
		.unwrap_or_default();
	header(
		&mut svg,
		left,
		title,
		&format!("time between each frame and the one before, through the stream{target_text}{cut}"),
	);

	for (gi, ((name, g), s)) in groups.iter().zip(&sorted).enumerate() {
		let color = COLORS[gi % COLORS.len()].1;
		let pt = top + gi as f64 * (panel_h + gap);
		let plot = Area {
			left,
			right,
			top: pt,
			bottom: pt + panel_h,
		};
		let y = |ms: f64| plot.y(ms.min(y_hi), 0.0, y_hi);
		let x = |sec: f64| left + sec / t_hi * (right - left);

		pacing_label(&mut svg, left, pt - 16.0, name, s);

		y_axis(&mut svg, &plot, 0.0, y_hi, "Frame time (ms)");
		for t in ticks(0.0, t_hi, 10) {
			let px = x(t);
			svg.line(px, plot.bottom, px, plot.bottom + 8.0, INK, 1.5);
			svg.text(
				px,
				plot.bottom + 32.0,
				&fmt_tick(t),
				Font::new(18.0, "middle", false, INK),
			);
		}

		// One dot per frame, so dense bands read as where most frames sit.
		let mut t = 0.0;
		for v in g.iter() {
			t += v / 1e3;
			if *v > y_hi {
				let px = x(t);
				svg.path(&format!("M{:.1},{:.1} l-7,12 h14 z", px, plot.top), LOST, 1.0);
			} else {
				svg.circle(x(t), y(*v), 2.2, color, 0.5);
			}
		}

		if let Some(tm) = target_ms.filter(|t| *t < y_hi) {
			let py = y(tm);
			let _ = write!(
				svg.0,
				r#"<line x1="{left:.1}" y1="{py:.1}" x2="{hist_r:.1}" y2="{py:.1}" stroke="{INK}" stroke-width="1.5" stroke-dasharray="8 6"/>"#
			);
		}

		frame_histogram(
			&mut svg,
			&plot,
			s,
			y_hi,
			(hist_l, hist_r),
			COLORS[gi % COLORS.len()].0,
			gi + 1 == groups.len(),
		);
	}
	let last_bottom = top + n * panel_h + (n - 1.0) * gap;
	svg.text(
		(left + right) / 2.0,
		last_bottom + 70.0,
		"Time in stream (s)",
		Font::new(20.0, "middle", false, INK),
	);
	svg.finish()
}

/// A timeline's gap band: the gap before each frame, against when it arrived.
fn timeline_gaps(
	svg: &mut Svg,
	plot: &Area,
	frames: &[FrameRow],
	x: &dyn Fn(f64) -> f64,
	seconds: &dyn Fn(&FrameRow) -> f64,
) {
	let gaps: Vec<(f64, f64)> = frames
		.windows(2)
		.map(|w| {
			(
				seconds(&w[1]),
				(w[1].receive_us.saturating_sub(w[0].receive_us)) as f64 / 1e3,
			)
		})
		.collect();
	let mut sorted: Vec<f64> = gaps.iter().map(|g| g.1).collect();
	sorted.sort_by(f64::total_cmp);
	let gap_hi = (sorted[sorted.len() - 1] * 1.08)
		.max(quantile(&sorted, 0.5) * 3.0)
		.max(1.0);
	y_axis(svg, plot, 0.0, gap_hi, "Gap before (ms)");
	let mut d = String::new();
	for (i, (s, gap)) in gaps.iter().enumerate() {
		let _ = write!(
			d,
			"{}{:.1},{:.1} ",
			if i == 0 { "M" } else { "L" },
			x(*s),
			plot.y(*gap, 0.0, gap_hi)
		);
	}
	let _ = write!(
		svg.0,
		r#"<path d="{d}" fill="none" stroke="{}" stroke-width="1.5"/>"#,
		COLORS[0].1
	);
}

/// A timeline's size band: each frame's size, keyframes and recovery frames in their colors.
fn timeline_sizes(
	svg: &mut Svg,
	plot: &Area,
	frames: &[FrameRow],
	x: &dyn Fn(f64) -> f64,
	seconds: &dyn Fn(&FrameRow) -> f64,
) {
	let kb = |f: &FrameRow| f.bytes as f64 / 1024.0;
	let size_hi = (frames.iter().map(kb).fold(0.0, f64::max) * 1.08).max(1.0);
	y_axis(svg, plot, 0.0, size_hi, "Frame size (KB)");
	let bar = ((plot.right - plot.left) / frames.len() as f64).clamp(1.0, 4.0);
	for f in frames {
		let color = if f.recovery {
			RECOVERY
		} else if f.keyframe {
			KEY
		} else {
			COLORS[0].0
		};
		let px = x(seconds(f));
		svg.line(px, plot.bottom, px, plot.y(kb(f), 0.0, size_hi), color, bar);
	}
}

/// A pacing panel's label: the group, and its p50, p99, longest gap and stutters (gaps over 3× p50).
fn pacing_label(svg: &mut Svg, left: f64, y: f64, name: &str, sorted: &[f64]) {
	let p50 = quantile(sorted, 0.5);
	let stutters = sorted.iter().filter(|v| **v > 3.0 * p50).count();
	svg.text(left, y, name, Font::new(20.0, "start", true, INK));
	svg.text(
		left + 14.0 + name.chars().count() as f64 * 12.5,
		y,
		&format!(
			"p50 {:.1} ms · p99 {:.1} ms · max {:.0} ms · {} stutter{}",
			p50,
			quantile(sorted, 0.99),
			sorted[sorted.len() - 1],
			stutters,
			if stutters == 1 { "" } else { "s" }
		),
		Font::new(17.0, "start", false, MUTED),
	);
}

/// A sideways histogram of frame times beside a pacing panel, sharing its y axis: bar length is the share of the
/// panel's frames in that slice of frame times, with a percent scale (titled under the last panel).
fn frame_histogram(
	svg: &mut Svg,
	plot: &Area,
	sorted: &[f64],
	y_hi: f64,
	(left, right): (f64, f64),
	fill: &str,
	titled: bool,
) {
	let bins = 60usize;
	let mut counts = vec![0usize; bins];
	for v in sorted {
		let b = ((v.min(y_hi) / y_hi) * bins as f64) as usize;
		counts[b.min(bins - 1)] += 1;
	}
	let most = *counts.iter().max().unwrap_or(&1) as f64;
	let bh = (plot.bottom - plot.top) / bins as f64;
	for (b, c) in counts.iter().enumerate().filter(|(_, c)| **c > 0) {
		let w = (*c as f64 / most) * (right - left);
		let by = plot.bottom - (b + 1) as f64 * bh;
		let _ = write!(
			svg.0,
			r#"<rect x="{left:.1}" y="{:.1}" width="{w:.1}" height="{:.1}" fill="{fill}"/>"#,
			by + 0.5,
			(bh - 1.0).max(0.5),
		);
	}
	svg.line(left, plot.top, left, plot.bottom, INK, 1.5);
	svg.line(left, plot.bottom, right, plot.bottom, INK, 1.5);
	let peak = 100.0 * most / sorted.len() as f64;
	for t in ticks(0.0, peak, 3) {
		let px = left + t / peak * (right - left);
		svg.line(px, plot.bottom, px, plot.bottom + 8.0, INK, 1.5);
		svg.text(
			px,
			plot.bottom + 32.0,
			&fmt_tick(t),
			Font::new(18.0, "middle", false, INK),
		);
	}
	if titled {
		svg.text(
			(left + right) / 2.0,
			plot.bottom + 70.0,
			"% of frames",
			Font::new(20.0, "middle", false, INK),
		);
	}
}

// ---------------------------------------------------------------------------------------------
// stages
// ---------------------------------------------------------------------------------------------

/// Moonshine's pipeline stages in a moonshine-otel frames CSV, in order, with their colors.
const STAGES: [(&str, &str, &str); 8] = [
	("channel_wait_us", "wait for frame", "#b8c4d6"),
	("import_us", "import", "#8da9d4"),
	("convert_us", "convert", "#4a6fae"),
	("submit_us", "submit", "#9ccb9f"),
	("consumer_queue_us", "queue", "#e8d38f"),
	("encode_wait_us", "encode", "#d27a4c"),
	("packetize_us", "packetize", "#c5a8d8"),
	("send_us", "send", "#8a5cab"),
];

/// One group's per-stage medians and total quantiles, in milliseconds.
pub(crate) struct StageSummary {
	frames: usize,
	medians: [f64; STAGES.len()],
	total_p50: f64,
	total_p99: f64,
}

/// Per-stage times (µs) and the total of every frame after warmup, from moonshine-otel frames CSVs.
pub(crate) fn read_stage_rows(path: &Path) -> anyhow::Result<Vec<([f64; STAGES.len()], f64)>> {
	let table = Table::read(path)?;
	let mut cols = [0usize; STAGES.len()];
	for (i, (name, _, _)) in STAGES.iter().enumerate() {
		cols[i] = table.column(name)?;
	}
	let total = table.column("total_us")?;
	let warmup = table.optional_column("warmup");
	Ok(table
		.rows
		.iter()
		.filter(|r| warmup.is_none_or(|i| field(r, i) != "1"))
		.map(|r| (cols.map(|c| number(r, c)), number(r, total)))
		.collect())
}

pub(crate) fn summarize_stages(rows: &[([f64; STAGES.len()], f64)]) -> Option<StageSummary> {
	if rows.is_empty() {
		return None;
	}
	let sorted = |values: Vec<f64>| {
		let mut v = values;
		v.sort_by(f64::total_cmp);
		v
	};
	let mut medians = [0.0; STAGES.len()];
	for (i, m) in medians.iter_mut().enumerate() {
		*m = quantile(&sorted(rows.iter().map(|r| r.0[i]).collect()), 0.5) / 1e3;
	}
	let totals = sorted(rows.iter().map(|r| r.1).collect());
	Some(StageSummary {
		frames: rows.len(),
		medians,
		total_p50: quantile(&totals, 0.5) / 1e3,
		total_p99: quantile(&totals, 0.99) / 1e3,
	})
}

/// A bar per group: each stage's median, stacked, with the total's p50 and p99 marked.
pub(crate) fn stages_svg(groups: &[(String, StageSummary)], title: &str) -> String {
	// A row per group, the chart only as tall as its rows need.
	let slot = 100.0;
	let plot = Area {
		left: 330.0,
		right: WIDTH - 60.0,
		top: 160.0,
		bottom: 160.0 + slot * groups.len().max(1) as f64,
	};
	let mut svg = Svg::with_height(plot.bottom + 110.0);
	header(
		&mut svg,
		110.0,
		title,
		"median of each stage, stacked; ┃ total p50, ◆ total p99 (ms, measured in Moonshine)",
	);
	let x_hi = groups
		.iter()
		.map(|(_, s)| s.total_p99.max(s.medians.iter().sum()))
		.fold(0.0, f64::max)
		.max(0.5)
		* 1.1;
	let x = |ms: f64| plot.left + ms / x_hi * (plot.right - plot.left);
	for t in ticks(0.0, x_hi, 10) {
		svg.line(x(t), plot.top, x(t), plot.bottom, GRID, 1.0);
	}
	svg.line(plot.left, plot.bottom, plot.right, plot.bottom, INK, 1.5);
	x_axis(&mut svg, &plot, 0.0, x_hi, "Milliseconds");

	let bar = slot * 0.5;
	for (gi, (name, s)) in groups.iter().enumerate() {
		let cy = plot.top + slot * (gi as f64 + 0.5);
		svg.text(plot.left - 16.0, cy, name, Font::new(19.0, "end", true, INK));
		svg.text(
			plot.left - 16.0,
			cy + 22.0,
			&format!("{} frames", s.frames),
			Font::new(15.0, "end", false, MUTED),
		);
		let mut at = 0.0;
		for (i, ms) in s.medians.iter().enumerate() {
			if *ms <= 0.0 {
				continue;
			}
			let _ = write!(
				svg.0,
				r#"<rect x="{:.1}" y="{:.1}" width="{:.1}" height="{bar:.1}" fill="{}"/>"#,
				x(at),
				cy - bar / 2.0,
				(x(at + ms) - x(at)).max(0.5),
				STAGES[i].2
			);
			at += ms;
		}
		let p50 = x(s.total_p50);
		svg.line(p50, cy - bar * 0.8, p50, cy + bar * 0.8, INK, 3.0);
		let p99 = x(s.total_p99);
		let r = 9.0;
		svg.path(
			&format!(
				"M{p99:.1},{:.1} L{:.1},{cy:.1} L{p99:.1},{:.1} L{:.1},{cy:.1} Z",
				cy - r,
				p99 + r,
				cy + r,
				p99 - r
			),
			INK,
			1.0,
		);
		svg.text(
			p99 + 16.0,
			cy + 6.0,
			&format!("{:.2} / {:.2}", s.total_p50, s.total_p99),
			Font::new(16.0, "start", false, MUTED),
		);
	}
	let entries: Vec<(&str, &str)> = STAGES.iter().map(|(_, label, color)| (*color, *label)).collect();
	legend(&mut svg, WIDTH - 60.0, 126.0, &entries);
	svg.finish()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn tick_steps_are_round() {
		assert_eq!(ticks(0.0, 10.0, 5), vec![0.0, 2.0, 4.0, 6.0, 8.0, 10.0]);
		assert_eq!(ticks(12.3, 20.9, 7), vec![14.0, 16.0, 18.0, 20.0]);
	}

	#[test]
	fn quantiles_interpolate() {
		let v = [1.0, 2.0, 3.0, 4.0];
		assert_eq!(quantile(&v, 0.5), 2.5);
		assert_eq!(quantile(&v, 1.0), 4.0);
	}

	#[test]
	fn groups_merge_by_name_in_order() {
		let merged = merge_groups(vec![
			("b".to_string(), vec![1]),
			("a".to_string(), vec![2]),
			("b".to_string(), vec![3]),
		]);
		assert_eq!(merged, vec![("b".to_string(), vec![1, 3]), ("a".to_string(), vec![2])]);
		assert_eq!(named_input("x=y/z.csv"), ("x".to_string(), PathBuf::from("y/z.csv")));
		assert_eq!(named_input("y/z.csv"), ("z".to_string(), PathBuf::from("y/z.csv")));
	}

	fn frames() -> Vec<FrameRow> {
		(0..120)
			.filter(|i| *i != 50)
			.map(|i| FrameRow {
				number: i + 1,
				keyframe: i == 0,
				recovery: i == 51,
				receive_us: 1_000_000 + i as u64 * 16_667 + if i > 50 { 40_000 } else { 0 },
				bytes: if i == 0 { 90_000 } else { 20_000 },
				failed: false,
			})
			.collect()
	}

	#[test]
	fn renders_every_chart_to_png() {
		let groups = vec![
			("a".to_string(), (0..40).map(|i| 30.0 + (i % 9) as f64).collect()),
			("b & c".to_string(), (0..40).map(|i| 25.0 + (i % 7) as f64).collect()),
		];
		let svg = clicks_svg(&groups, "Click-to-flash latency");
		assert!(svg.contains("b &amp; c"));
		assert_eq!(&render_png(&svg).unwrap()[1..4], b"PNG");

		let svg = timeline_svg(&frames(), "hevc-loss.csv");
		assert!(svg.contains("1 keyframes, 1 recovery frames, 1 never arrived"));
		assert_eq!(&render_png(&svg).unwrap()[1..4], b"PNG");

		let gaps: Vec<f64> = frames()
			.windows(2)
			.map(|w| (w[1].receive_us - w[0].receive_us) as f64 / 1e3)
			.collect();
		let svg = pacing_svg(
			&[("hevc".to_string(), gaps.clone()), ("av1".to_string(), gaps)],
			"Pacing",
			Some(1000.0 / 60.0),
		);
		assert!(svg.contains("p50 16.7"));
		assert_eq!(&render_png(&svg).unwrap()[1..4], b"PNG");

		let rows: Vec<([f64; STAGES.len()], f64)> = (0..100)
			.map(|i| {
				(
					[100.0, 50.0, 300.0, 200.0, 10.0, 2000.0 + i as f64 * 10.0, 20.0, 40.0],
					3000.0 + i as f64 * 10.0,
				)
			})
			.collect();
		let summary = summarize_stages(&rows).unwrap();
		assert_eq!(summary.frames, 100);
		assert!((summary.medians[5] - 2.495).abs() < 1e-9);
		let svg = stages_svg(&[("hevc".to_string(), summary)], "Server stages");
		assert!(svg.contains("100 frames"));
		assert_eq!(&render_png(&svg).unwrap()[1..4], b"PNG");
	}

	#[test]
	fn reads_frames_with_and_without_the_recovery_column() {
		let dir = std::env::temp_dir().join(format!("plot-test-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let header = "frame_number,frame_type,bytes,host_latency_ms,receive_us,enqueue_us,decode_start_us,decode_end_us,rtp_timestamp,decode_status,width,height";
		let old = dir.join("old.csv");
		std::fs::write(
			&old,
			format!("{header}\n1,1,900,1.0,10,11,12,13,0,0,1920,1080\n2,0,100,1.0,20,21,22,23,0,0,1920,1080\n"),
		)
		.unwrap();
		let new = dir.join("new.csv");
		std::fs::write(
			&new,
			format!(
				"{header},recovery\n1,1,900,1.0,10,11,12,13,0,0,1920,1080,0\n3,0,100,1.0,20,21,22,23,0,0,1920,1080,1\n"
			),
		)
		.unwrap();
		let old_frames = read_frames(&old).unwrap();
		assert!(!old_frames[1].recovery);
		let new_frames = read_frames(&new).unwrap();
		assert!(new_frames[1].recovery && new_frames[0].keyframe);
		assert_eq!(read_gaps(&new).unwrap(), vec![0.01]);
		std::fs::remove_dir_all(&dir).unwrap();
	}
}
