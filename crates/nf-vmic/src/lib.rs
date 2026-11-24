//! nf-vmic: the app's side of noise-filter's virtual cables, ACX-LL-Cable (TODO 19, spike check 4; the per-cable design,
//! doc/todo/19-per-cable-design.md).
//!
//! Every cable is a device node of the one driver package (driver/src), with its own control device, `\\.\AcxLlCable_<id>`,
//! named from the cable's identity. An app hands a cable's control device a ring of 48 kHz float32 stereo frames in one pending
//! `METHOD_OUT_DIRECT` IOCTL; that cable's microphones ("Microphone (ACX-LL-Cable A)") then deliver what the app writes there,
//! sample for sample, until the request ends. The protocol is `driver/src/Shared/nf_vmic.h`.
//!
//! - [`cables`]: a cable's identity, the instance ID that carries it (`ROOT\MEDIA\ACXLLCABLE_<label>_<id>_<n>`), and the cables
//!   present (interim, until discovery through each cable's device interface: N1).
//! - [`protocol`]: its constants and layouts, checked against the header's offsets at compile time, and every status ATTACH
//!   completes with, in Win32 terms.
//! - [`ring`]: the ring's arithmetic, and [`ring::ReaderModel`], the driver's capture pass in Rust (the tests' reader).
//! - [`signal`]: the test signal (frame n carries the code `n mod 2^23`, a different float32 on each channel, never silence's
//!   bits) and its checker.
//! - [`memory`]: [`RingMemory`] (the page-aligned ring, marked once attached until reset) and [`Writer`] (fills it and
//!   publishes `write_frame`).
//! - [`device`]: [`Device`] (a cable's control device: open, STATS) and [`Attachment`] (the pending ATTACH; detached by
//!   cancelling it, with one wait).
//! - [`latency`]: the arithmetic of the tool's latency mode: preallocated logs, a packet's frames decoded back to frame
//!   numbers, publication-to-receipt latency per frame, and nearest-rank percentiles.
//! - [`variant`]: the driver's three variants (one INF, three hardware IDs: both microphones, the call one, the low-latency one):
//!   which is installed, and the elevated switch between them (`noise-filter mic variant`).
//! - [`sessions`]: our cables' endpoints, known by the KS filter's instance ID; which apps record from our microphones (Windows'
//!   audio sessions), and which microphone suits each (`noise-filter mic sessions`).
//! - [`win_error`]: a failed Windows call, named for the user.
//!
//! The tool `nf-vmic-test` (src/bin) feeds, verifies, prints the statistics, kills feeders, opens an endpoint in a chosen format
//! (the format-refusal tests) and measures the ring's latency. Nothing in the tests opens the driver or an audio device: they
//! run the writer against the model reader over real `VirtualAlloc` memory (4 KB pages).
#![warn(clippy::undocumented_unsafe_blocks)]

pub mod broker;
pub mod cables;
pub mod device;
pub mod latency;
pub mod memory;
pub mod protocol;
pub mod ring;
pub mod sessions;
pub mod signal;
pub mod variant;
pub mod win_error;

pub use device::{AttachError, Attachment, DETACH_WAIT_MS, Device};
pub use memory::{RingMemory, Writer, top_up};
