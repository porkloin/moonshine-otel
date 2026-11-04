//! Command-line pieces for `moonshine-otel compare`.

use std::path::PathBuf;

use clap::Args;

use crate::compare::{CompareOptions, Comparison, Format, compare};
use crate::results::ResultsFile;

#[derive(Args, Debug, Clone)]
pub struct CompareArgs {
	/// Result file of the reference run (e.g. main), or a directory of them: streams with the
	/// same configuration count as iterations of one run.
	pub baseline: PathBuf,
	/// Result file or directory of the run under test (e.g. your branch).
	pub candidate: PathBuf,
	#[command(flatten)]
	pub thresholds: ThresholdArgs,
	/// Output format.
	#[arg(long, default_value = "text", value_parser = ["text", "markdown", "md", "json"])]
	pub format: String,
	/// Show every metric, not just headline rows and changes.
	#[arg(long)]
	pub all: bool,
}

/// Thresholds for `compare`.
#[derive(Args, Debug, Clone)]
pub struct ThresholdArgs {
	/// Relative change that is always considered noise, in percent.
	#[arg(long, default_value_t = 5.0)]
	pub rel_threshold: f64,
	/// A change must also exceed k × the combined spread across iterations.
	#[arg(long, default_value_t = 3.0)]
	pub k: f64,
	/// Treat driver, kernel, governor or host differences as invalidating (exit 2).
	#[arg(long)]
	pub strict_identity: bool,
	/// Compare even across GPUs or with debug builds.
	#[arg(long)]
	pub allow_identity_mismatch: bool,
}

impl ThresholdArgs {
	pub fn options(&self) -> CompareOptions {
		CompareOptions {
			rel_threshold: self.rel_threshold / 100.0,
			k: self.k,
			strict_identity: self.strict_identity,
			allow_identity_mismatch: self.allow_identity_mismatch,
			..Default::default()
		}
	}
}

/// Run a comparison between two result files and print it. Returns the comparison so the
/// caller can pick the exit code.
pub fn run_compare(args: &CompareArgs) -> anyhow::Result<Comparison> {
	let baseline = ResultsFile::read_path(&args.baseline)?;
	let candidate = ResultsFile::read_path(&args.candidate)?;
	let comparison = compare(&baseline, &candidate, &args.thresholds.options());
	let format: Format = args.format.parse().map_err(anyhow::Error::msg)?;
	print!("{}", comparison.render(format, args.all));
	Ok(comparison)
}
