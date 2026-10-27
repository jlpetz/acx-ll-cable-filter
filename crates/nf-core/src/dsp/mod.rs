//! Per-block DSP building blocks. Every `process` here is real-time: no allocation, locks or calls
//! out of the crate. Construction may allocate.

pub mod delay;
pub mod drift;
pub mod fade;
pub mod gate;
pub mod meter;
pub mod mix;
pub mod noise_post;
pub mod slip;
