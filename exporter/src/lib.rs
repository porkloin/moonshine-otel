//! Telemetry, exact per-run statistics and regression comparison for Moonshine's video
//! pipeline.
//!
//! This crate deliberately does not depend on `moonshine`: everything that decides what
//! the numbers mean lives here. The bench binary converts
//! `FrameStats` into [`FrameSample`] and feeds it to a [`RunRecorder`].

#[cfg(feature = "cli")]
pub mod cli;
pub mod compare;
pub mod frame_ids;
pub mod identity;
pub mod metrics;
pub mod recorder;
#[cfg(feature = "relay")]
pub mod relay;
pub mod results;
pub mod sample;
pub mod spool;
pub mod summary;
pub mod telemetry;
pub mod traces;

pub use identity::{RunConfig, RunIdentity};
pub use metrics::HistogramBuckets;
pub use recorder::RunRecorder;
pub use results::{IterationResult, ResultsFile, RunResult};
pub use sample::{Derived, Deriver, FrameKind, FrameSample, Stage};
pub use spool::SpoolLimits;
pub use summary::{Summary, SummaryBuilder};
pub use telemetry::{Protocol, Telemetry, TelemetryConfig};
pub use traces::TraceMode;
