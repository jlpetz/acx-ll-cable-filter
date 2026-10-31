//! nf-engine: WASAPI streams, the MMCSS audio thread, clocks, telemetry and the latency test.
//!
//! Everything public is in [`api`] (re-exported at the crate root); the modules behind it are
//! private and free to change.
//!
//! - `devices`: enumeration, by-id and default lookup.
//! - `stream`: the WASAPI wrappers (capture, render, loopback, keep-alive), converting at the edge.
//! - `pipeline`: the audio thread's per-block bookkeeping (FIFOs, slips, coarse corrections, the
//!   latency readout, statistics) behind stream traits, so tests drive it with fakes.
//! - `reference`: the reference's timeline, read in step with the mic so the echo path holds still.
//! - `audio`: the audio thread itself.
//! - `control`: start/stop/apply/status, the device listener, user-worded errors (`errors`).
//! - `latency_test`: the sweep tests (through the engine, and through a cable alone) and their analysis.
//! - `play_record`: playing signals and recording a capture meanwhile (the latency tests' recorder and player, and the
//!   quality test's `play_record`).
//! - `timing`: QPC helpers and sample-index → QPC mapping.

mod api;
mod audio;
mod control;
mod convert;
mod devices;
mod errors;
mod fifo;
mod latency_test;
#[cfg(all(test, feature = "live-probe"))]
mod live_probe;
mod pipeline;
mod play_record;
mod reference;
mod sched;
mod stream;
#[cfg(test)]
mod test_alloc;
mod timing;

pub use api::*;
