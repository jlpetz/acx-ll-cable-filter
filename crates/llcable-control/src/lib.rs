//! Local control interface shared by the cable panel, voice app and other Rust front ends.
//! Passive clients need no voice engine. Each running voice engine belongs to the process that created it.
pub use llcable_routing as routing;
pub use llcable_sdk::{CableId, CableInfo, Diagnostics, EndpointKind, Label, Port, authorize, authorized, cables, diagnostics};
#[cfg(feature = "voice")]
pub use nf_engine as voice;
