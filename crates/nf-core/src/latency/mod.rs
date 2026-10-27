//! Latency measurement maths (TODO #13): probe signals, arrival detection and rolling statistics.
//! The analysis runs off the audio thread (in the latency-test thread, after recording), so it may
//! allocate. `RollingStats` is the exception: it runs on the audio thread for the live readout.

pub mod analysis;
pub mod stats;
pub mod sweep;
