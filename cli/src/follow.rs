//! Following a file of JSON lines that another process appends to as things happen (a test client's statistics, a
//! workload's log): each whole line goes to a handler as it arrives.

use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use tokio::io::AsyncBufReadExt;
use tokio::signal::unix::{SignalKind, signal};

const POLL: Duration = Duration::from_millis(100);

/// Hand every whole line of `path` to `line`, waiting for the file to appear and for more to be written, until
/// SIGTERM or Ctrl+C (then to the end of what's there). With `once`, read to the end and stop. Returns the number of
/// lines.
pub async fn follow(path: &Path, once: bool, mut line: impl FnMut(&str)) -> anyhow::Result<u64> {
	let file = loop {
		match tokio::fs::File::open(path).await {
			Ok(f) => break f,
			Err(_) if !once => tokio::time::sleep(POLL).await,
			Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
		}
	};
	let mut reader = tokio::io::BufReader::new(file);
	let mut terminate = signal(SignalKind::terminate()).context("installing the SIGTERM handler")?;
	let mut stopping = once;
	let (mut text, mut lines) = (String::new(), 0u64);
	loop {
		let n = reader.read_line(&mut text).await?;
		if n > 0 && text.ends_with('\n') {
			line(text.trim_end());
			lines += 1;
			text.clear();
			continue;
		}
		// At the end of what's written so far (a partial line stays in `text` until the rest arrives).
		if stopping {
			break;
		}
		tokio::select! {
			_ = tokio::signal::ctrl_c() => stopping = true,
			_ = terminate.recv() => stopping = true,
			_ = tokio::time::sleep(POLL) => {},
		}
	}
	Ok(lines)
}
