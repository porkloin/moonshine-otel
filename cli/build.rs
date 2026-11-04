//! Records the compiler and profile, so every result names how this tool was built.

use std::process::Command;

fn main() {
	let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
	let rustc_version = Command::new(rustc)
		.arg("-V")
		.output()
		.ok()
		.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
		.unwrap_or_default();
	let profile = std::env::var("PROFILE").unwrap_or_default();
	println!("cargo:rustc-env=BENCH_RUSTC_VERSION={rustc_version}");
	println!("cargo:rustc-env=BENCH_PROFILE={profile}");
}
