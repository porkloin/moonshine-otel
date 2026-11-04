//! `moonshine-otel-relay`: forward a spool written by `--otlp-spool` to an OTLP/gRPC collector.
//!
//! Run it outside the sandbox the spool comes from, with read-only access to the spool.

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use moonshine_otel_exporter::relay::{Relay, RelayConfig};
use tokio::signal::unix::{SignalKind, signal};

#[derive(Parser, Debug)]
#[command(name = "moonshine-otel-relay", version, about, long_about = None)]
struct Args {
	/// Spool directory to read. Nothing above its parent may be writable by the spool's writer.
	#[arg(env = "MOONSHINE_OTEL_RELAY_SPOOL")]
	spool: PathBuf,
	/// OTLP/gRPC endpoint of the collector (default: localhost on MOONSHINE_OTEL_OTLP_GRPC_PORT, or 4317).
	#[arg(long, env = "OTEL_EXPORTER_OTLP_ENDPOINT")]
	otlp_endpoint: Option<String>,
	/// Value of the `moonshine_otel.relay.source` resource attribute (default: the spool path).
	#[arg(long)]
	source: Option<String>,
	/// Seconds between scans of the spool.
	#[arg(long, default_value_t = 1.0)]
	poll_interval: f64,
	/// Skip files larger than this many MiB.
	#[arg(long, default_value_t = 4)]
	max_file_mib: u64,
	/// Forward at most this many MiB per minute; files past it are skipped.
	#[arg(long, default_value_t = 64)]
	max_mib_per_minute: u64,
}

/// The stack's collector: `localhost` on `MOONSHINE_OTEL_OTLP_GRPC_PORT` (4317), as `watch` uses.
fn default_endpoint() -> String {
	let port = std::env::var("MOONSHINE_OTEL_OTLP_GRPC_PORT")
		.ok()
		.and_then(|v| v.trim().parse::<u16>().ok())
		.unwrap_or(4317);
	format!("http://localhost:{port}")
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
	tracing_subscriber::fmt()
		.with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
		// No color codes in the journal.
		.with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
		.init();
	let args = Args::parse();
	let config = RelayConfig {
		source: args.source.unwrap_or_else(|| args.spool.display().to_string()),
		spool: args.spool,
		endpoint: args.otlp_endpoint.unwrap_or_else(default_endpoint),
		poll_interval: Duration::from_secs_f64(args.poll_interval.max(0.1)),
		max_file_bytes: args.max_file_mib << 20,
		max_bytes_per_minute: args.max_mib_per_minute << 20,
	};
	tracing::info!("relaying {} to {}", config.spool.display(), config.endpoint);
	let mut relay = Relay::new(config)?;
	let mut term = signal(SignalKind::terminate())?;
	relay
		.run(async {
			tokio::select! {
				_ = tokio::signal::ctrl_c() => {},
				_ = term.recv() => {},
			}
		})
		.await;
	Ok(())
}
