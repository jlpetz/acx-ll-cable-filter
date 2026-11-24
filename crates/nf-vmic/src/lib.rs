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
//! - [`latency`]: the arithmetic of the tool's latency mode: preallocated logs, a packet's frames decoded back to frame
//!   numbers, publication-to-receipt latency per frame, and nearest-rank percentiles.
//! - [`identity`]: a cable's names (TODO 19, the per-cable design): its identity, its label and its device node's fixed
//!   instance ID, `ROOT\MEDIA\ACXLLCABLE_<label>_<id>_<n>`, parsed as the driver parses it.
//! - [`setup`]: the cables' setup, behind the `acx-ll-cable` tool and the driver scripts: list, add, remove, update and the
//!   migration from the old driver's devices; the package rule, the store of identities, the lock; the sequences of update and
//!   the migration over a seam the tests fake.
//! - [`variant`]: the driver's three variants (one INF, three hardware IDs shared by every cable: both microphones, the call
//!   one, the low-latency one), and the elevated switch of one cable between them (`noise-filter mic variant`,
//!   `acx-ll-cable variant`).
//! - [`sessions`]: which apps record from our microphones (Windows' audio sessions), and which microphone suits each
//!   (`noise-filter mic sessions`).
//! - [`win_error`]: a failed Windows call, named for the user.
//!
//! The tool `nf-vmic-test` (src/bin) feeds, verifies, prints the statistics, kills feeders, opens an endpoint in a chosen format
//! (the format-refusal tests) and measures the ring's latency. The tool `acx-ll-cable` (src/bin) is the cables' setup, over
//! [`setup`] and [`variant`]. Nothing in the tests opens the driver or an audio device, or changes a device node: they run the
//! writer against the model reader over real `VirtualAlloc` memory (4 KB pages), the setup's plans as pure functions, and its
//! sequences against a fake of Windows.
#![warn(clippy::undocumented_unsafe_blocks)]

pub mod broker;
pub mod device;
pub mod identity;
pub mod latency;
pub mod memory;
pub mod protocol;
pub mod ring;
pub mod sessions;
pub mod setup;
pub mod signal;
pub mod variant;
pub mod win_error;

pub use device::{AttachError, Attachment, DETACH_WAIT_MS, Device};
pub use memory::{RingMemory, Writer, top_up};
