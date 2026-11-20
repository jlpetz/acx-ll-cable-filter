//! nf-vmic: the app's side of noise-filter's virtual microphone driver (TODO 19, spike check 4).
//!
//! The driver (driver/src) has one control device, `\\.\NoiseFilterVmic`. An app hands it a ring of 48 kHz float32 stereo frames
//! in one pending `METHOD_OUT_DIRECT` IOCTL; the capture endpoint "Microphone (AudioCodec Device)" then delivers what the app
//! writes there, sample for sample, until the request ends. The protocol is `driver/src/Shared/nf_vmic.h`.
//!
//! - [`protocol`]: its constants and layouts, checked against the header's offsets at compile time, and every status ATTACH
//!   completes with, in Win32 terms.
//! - [`ring`]: the ring's arithmetic, and [`ring::ReaderModel`], the driver's capture pass in Rust (the tests' reader).
//! - [`signal`]: the test signal (frame n carries the code `n mod 2^23`, a different float32 on each channel, never silence's
//!   bits) and its checker.
//! - [`memory`]: [`RingMemory`] (the page-aligned ring, marked once attached until reset) and [`Writer`] (fills it and
//!   publishes `write_frame`).
//! - [`device`]: [`Device`] (open, STATS) and [`Attachment`] (the pending ATTACH; detached by cancelling it, with one wait).
//!
//! The tool `nf-vmic-test` (src/bin) feeds, verifies, prints the statistics and kills feeders. Nothing in the tests opens the
//! driver or an audio device: they run the writer against the model reader over real `VirtualAlloc` memory (4 KB pages).
#![warn(clippy::undocumented_unsafe_blocks)]

pub mod device;
pub mod memory;
pub mod protocol;
pub mod ring;
pub mod signal;

pub use device::{AttachError, Attachment, DETACH_WAIT_MS, Device};
pub use memory::{RingMemory, Writer, top_up};
