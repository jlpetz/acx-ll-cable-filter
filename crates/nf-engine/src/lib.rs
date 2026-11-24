//! nf-engine: WASAPI streams, the MMCSS audio thread, clocks, telemetry and the latency test.
//!
//! Everything public is in `api` (re-exported at the crate root); the modules behind it are
//! private and free to change.
//!
//! - `devices`: enumeration, by-id and default lookup.
//! - `stream`: the WASAPI wrappers (capture, render, loopback, keep-alive), converting at the edge.
//! - `exclusive`: exclusive mode apart from WASAPI: its period and formats, the fall-back to shared mode, and the
//!   event-driven output's ring.
//! - `out_target`: the adaptive output target.
//! - `ring_output`, `ring_headroom`: the direct driver route's output (the driver's live ring) and its fill headroom.
//! - `pipeline`: the audio thread's per-block bookkeeping (FIFOs, slips, coarse corrections, the
//!   latency readout, statistics) behind stream traits, so tests drive it with fakes.
//! - `reference`: the reference's timeline, read in step with the mic so the echo path holds still.
//! - `budget`: the latency readout's budget, block by block: where its first sample spent the time.
//! - `clock_fit`: the clocks' drift fitted over a chain recording's blocks (`devices --check`).
//! - `audio`: the audio thread itself.
//! - `control`: start/stop/apply/status, the device listener, user-worded errors (`errors`).
//! - `latency_test`: the sweep tests (through the engine, and through a cable alone) and their analysis.
//! - `disturbed`: which of the engine test's sweeps the engine's own events disturbed, from the tap's blocks' facts.
//! - `recorder`: the chain recorder, what the chain was fed live and what it gave (`Engine::record_chain`).
//! - `flight`: the flight recorder, the last seconds of the blocks' facts kept and written out when something goes wrong
//!   (`Engine::arm_flight_recorder`).
//! - `csv`: the recorders' blocks as CSV (`chain_blocks_csv`).
//! - `handover`: handing a ring's producer to the audio thread and back (the latency test's tap, the recorders).
//! - `play_record`: playing signals and recording a capture meanwhile (the latency tests' recorder and player, and the
//!   quality test's `play_record`).
//! - `timing`: QPC helpers and sample-index → QPC mapping.

mod api;
mod audio;
mod budget;
mod clock_fit;
mod control;
mod convert;
mod csv;
mod devices;
mod disturbed;
mod echo_guard;
mod effect;
mod errors;
mod exclusive;
mod fifo;
mod flight;
mod handover;
mod latency_test;
#[cfg(all(test, feature = "live-probe"))]
mod live_probe;
pub mod models;
mod out_target;
mod pipeline;
mod play_record;
mod recorder;
mod reference;
mod ring_headroom;
mod ring_output;
mod sched;
mod stream;
#[cfg(test)]
mod test_alloc;
mod timing;

pub use api::*;
