//! Trace and span ids for one frame, derived from the stream's key and the frame's number, so every process that
//! handled the frame (Moonshine's recorder on the host, the test client's) puts its spans in one trace without
//! passing context over the wire. Moonshine numbers frames on the wire (the frame index), and the client receives
//! that number, so both sides know it; the key is whatever both were told for the stream.
//!
//! - trace id: FNV-1a 128 of `<key>/<frame number>`
//! - the frame's root span id: FNV-1a 64 of `<key>/<frame number>/frame`
//!
//! The host's `frame` span is the root; other processes parent their spans to [`FrameIds::span_id`].
//!
//! A click gets a trace the same way, from the stream key and the click's id (which travels with the click in the
//! pointer's position, see gamestream-test-client's `click_position`):
//!
//! - trace id: FNV-1a 128 of `<key>/click/<id>`
//! - the click's root span id: FNV-1a 64 of `<key>/click/<id>/click`

use std::cell::Cell;

use opentelemetry::trace::{SpanId, TraceId};
use opentelemetry_sdk::trace::{IdGenerator, RandomIdGenerator};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameIds {
	pub trace_id: TraceId,
	pub span_id: SpanId,
}

impl FrameIds {
	pub fn new(stream_key: &str, frame_number: u64) -> Self {
		Self::derive(&format!("{stream_key}/{frame_number}"), "frame")
	}

	/// A click's trace and root span.
	pub fn click(stream_key: &str, click_id: u64) -> Self {
		Self::derive(&format!("{stream_key}/click/{click_id}"), "click")
	}

	fn derive(name: &str, root: &str) -> Self {
		let trace = fnv1a_128(name.as_bytes());
		let span = fnv1a_64(format!("{name}/{root}").as_bytes());
		// All zeros is the invalid id.
		FrameIds {
			trace_id: TraceId::from_bytes(trace.max(1).to_be_bytes()),
			span_id: SpanId::from_bytes(span.max(1).to_be_bytes()),
		}
	}
}

fn fnv1a_128(bytes: &[u8]) -> u128 {
	const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
	const PRIME: u128 = 0x0000000001000000000000000000013b;
	bytes
		.iter()
		.fold(OFFSET, |h, b| (h ^ u128::from(*b)).wrapping_mul(PRIME))
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
	const OFFSET: u64 = 0xcbf29ce484222325;
	const PRIME: u64 = 0x100000001b3;
	bytes
		.iter()
		.fold(OFFSET, |h, b| (h ^ u64::from(*b)).wrapping_mul(PRIME))
}

thread_local! {
	static NEXT: Cell<Option<FrameIds>> = const { Cell::new(None) };
}

/// Random ids, except for a root span started inside [`with_ids`], which gets those ids. The SDK has no way to set
/// a span's ids, so the tracer provider uses this generator.
#[derive(Debug, Default)]
pub struct FrameIdGenerator {
	random: RandomIdGenerator,
}

impl IdGenerator for FrameIdGenerator {
	fn new_trace_id(&self) -> TraceId {
		NEXT.with(Cell::get)
			.map_or_else(|| self.random.new_trace_id(), |ids| ids.trace_id)
	}

	fn new_span_id(&self) -> SpanId {
		NEXT.with(Cell::get)
			.map_or_else(|| self.random.new_span_id(), |ids| ids.span_id)
	}
}

/// Run `start` (which starts one root span) with `ids` for that span, on a tracer whose provider uses
/// [`FrameIdGenerator`].
pub fn with_ids<R>(ids: FrameIds, start: impl FnOnce() -> R) -> R {
	NEXT.with(|n| n.set(Some(ids)));
	let r = start();
	NEXT.with(|n| n.set(None));
	r
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn fnv_matches_the_published_vectors() {
		assert_eq!(fnv1a_64(b""), 0xcbf29ce484222325);
		assert_eq!(fnv1a_64(b"a"), 0xaf63dc4c8601ec8c);
		assert_eq!(fnv1a_128(b"a"), 0xd228cb696f1a8caf78912b704e4a8964);
	}

	#[test]
	fn the_same_frame_gets_the_same_ids_and_others_differ() {
		let a = FrameIds::new("stream", 42);
		assert_eq!(a, FrameIds::new("stream", 42));
		assert_ne!(a.trace_id, FrameIds::new("stream", 43).trace_id);
		assert_ne!(a.trace_id, FrameIds::new("other", 42).trace_id);
		let click = FrameIds::click("stream", 42);
		assert_ne!(click.trace_id, a.trace_id);
		assert_eq!(click, FrameIds::click("stream", 42));
	}

	#[test]
	fn the_generator_uses_the_ids_only_inside_with_ids() {
		let g = FrameIdGenerator::default();
		let ids = FrameIds::new("stream", 7);
		let (t, s) = with_ids(ids, || (g.new_trace_id(), g.new_span_id()));
		assert_eq!((t, s), (ids.trace_id, ids.span_id));
		assert_ne!(g.new_trace_id(), ids.trace_id);
	}
}
