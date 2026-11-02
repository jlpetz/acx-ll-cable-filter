//! `noise-filter lab`'s pure half (TODO 14, the Lab mode's phase 0: the offline batch). The command (`commands/lab.rs`) runs
//! the clips through the models and the metrics; these build the clips and turn their scores into the report, without touching
//! a device or a model, so they're unit-tested here.
//!
//! - [`playlist`]: the clips, from the fetched test-audio set: each speech segment in each noise at each SNR, built by
//!   `quality`'s scene builder, the same every run.
//! - [`stats`]: means, and paired differences from a baseline with 95% CIs (Student's t over clips, and why not a bootstrap).
//! - [`report`]: the metrics, the summary overall and per noise type and SNR, the text scorecard, `clips.csv` and `lab.json`.
//!
//! The name is TODO 14's ("the Lab mode", its CLI sketched as `lab run playlist.toml --models all --out report.json`): phase 0
//! is that batch, and the later phases (live capture, the blind-rating player) belong under the same name.

pub mod playlist;
pub mod report;
pub mod stats;
