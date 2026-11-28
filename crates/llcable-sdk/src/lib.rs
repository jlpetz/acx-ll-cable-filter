//! The Windows cable SDK. Discovery and diagnostics are passive and never start an audio stream.
//! For regular playback or recording use a port's current WASAPI endpoint ID. Persist cable identity and role.
use std::io;

pub use nf_vmic::broker::{LIVE_CAPACITY, LiveFeed, attach_live_to, authorize, authorized};
pub use nf_vmic::cables::EndpointKind;
pub use nf_vmic::identity::{CableId, Label};
pub use nf_vmic::memory::Writer;
pub use nf_vmic::protocol::Diagnostics;

/// Driver-facing types for native hosts that need topology or per-packet state beyond [`cables`] and [`diagnostics`].
pub mod driver {
    pub use nf_vmic::identity::CableInstance;
    pub use nf_vmic::protocol::{ReaderSample, STATE_ATTACHED, STATE_FORMAT_MISMATCH, STATE_LIVE_HANDOFF, STATE_READING};
    pub use nf_vmic::roles::{Resolved, Snapshot};
}
use nf_vmic::roles::Snapshot;
use nf_vmic::setup::endpoints::State;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CableInfo {
    pub id: CableId,
    pub label: String,
    pub health: String,
    pub ports: Vec<Port>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Port {
    pub role: EndpointKind,
    pub name: String,
    pub id: String,
    pub active: bool,
}

/// One consistent enumeration for every role and cable, including disabled cable devices.
pub fn cables() -> io::Result<Vec<CableInfo>> {
    let snapshot = Snapshot::read().map_err(io::Error::other)?;
    let mut cables = Vec::new();
    for node in &snapshot.nodes {
        let Some(instance) = &node.cable else { continue };
        let mut ports = Vec::new();
        for role in [EndpointKind::Speakers, EndpointKind::Microphone, EndpointKind::CallMicrophone] {
            if let Some(port) = snapshot.resolve(instance.id, role).endpoint() {
                let name =
                    if port.name == role.friendly_name(&instance.label) { role.display_name(&instance.label) } else { port.name.clone() };
                ports.push(Port { role, name, id: port.id.clone(), active: port.state == State::Active });
            }
        }
        cables.push(CableInfo { id: instance.id, label: instance.label.to_string(), health: node.health.describe(), ports });
    }
    cables.sort_by(|a, b| a.label.cmp(&b.label).then(a.id.cmp(&b.id)));
    Ok(cables)
}

/// Read an aggregate snapshot. Opens a temporary read-only handle and closes it before returning.
pub fn diagnostics(id: CableId) -> io::Result<Diagnostics> {
    nf_vmic::Device::diagnostics(id)
}
