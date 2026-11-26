//! Endpoint enumeration, by-id lookup and the default-endpoint lookup.
//!
//! wasapi objects are `!Send` and live in the apartment that made them, so callers outside the
//! engine's own MTA threads (the UI is STA) go through [`list_devices`], which enumerates on a
//! short-lived MTA thread and returns plain data.

use nf_vmic::cables::EndpointKind;
use nf_vmic::identity::{CableId, CableInstance};
use nf_vmic::roles::{Resolved, Snapshot};
use nf_vmic::setup::endpoints::State;
use wasapi::{Device, DeviceEnumerator, DeviceState, Direction, Role, WasapiError};

use crate::api::{DeviceInfo, EngineError, Flow};
use crate::errors::{StartFailure, StreamRole, hr};
use crate::sched::Com;
use crate::stream::hr_of;

// Saved in the existing string fields. A WASAPI endpoint ID is always passed through verbatim.
const CABLE_PREFIX: &str = "acx-ll-cable:";

/// A persisted device selection for one of our cables, resolved to its active Windows endpoint each time streams open.
pub fn cable_endpoint_selection(id: CableId, role: EndpointKind) -> String {
    format!("{CABLE_PREFIX}{id}/{}", role.arg())
}

fn cable_role(selection: &str) -> Option<(CableId, EndpointKind)> {
    let (id, role) = selection.strip_prefix(CABLE_PREFIX)?.split_once('/')?;
    Some((id.parse().ok()?, EndpointKind::from_arg(role)?))
}

pub(crate) fn is_cable_selection(selection: Option<&str>) -> bool {
    selection.and_then(cable_role).is_some()
}

/// Resolve saved cable roles once per open, on the calling control thread. The ordinary endpoint IDs and system defaults in
/// `selections` need no enumeration. Keep an unavailable cable selection explicit so its open reports a missing device.
pub(crate) fn resolve_selections<'a>(selections: impl IntoIterator<Item = &'a mut String>) {
    let snapshot = std::cell::OnceCell::new();
    for selection in selections {
        if let Some((id, role)) = cable_role(selection)
            && let Some(s) = snapshot.get_or_init(|| Snapshot::read().ok())
            && let Some(endpoint) = active_cable_endpoint(s, id, role)
        {
            *selection = endpoint;
        }
    }
}

fn active_cable_endpoint(snapshot: &Snapshot, id: CableId, role: EndpointKind) -> Option<String> {
    match snapshot.resolve(id, role) {
        Resolved::Endpoint(e) if e.state == State::Active => Some(e.id),
        _ => None,
    }
}

/// Lists the active capture endpoints, then the active render endpoints.
pub(crate) fn list_devices() -> Result<Vec<DeviceInfo>, EngineError> {
    let worker = std::thread::Builder::new()
        .name("nf-devices".into())
        .spawn(list_on_this_thread)
        .map_err(|_| EngineError("Couldn't start listing audio devices.".into()))?;
    worker.join().map_err(|_| EngineError("Listing audio devices failed unexpectedly.".into()))?
}

fn list_on_this_thread() -> Result<Vec<DeviceInfo>, EngineError> {
    let fail = |e: WasapiError| EngineError(format!("Windows couldn't list the audio devices (error 0x{:08X}).", hr_of(&e) as u32));
    let _com = Com::mta().map_err(|c| EngineError(format!("Windows audio couldn't be initialised (error 0x{:08X}).", c as u32)))?;
    let en = DeviceEnumerator::new().map_err(fail)?;
    // Optional metadata. Non-cable devices and raw Windows IDs remain usable if the cable inventory cannot be read.
    let cables = Snapshot::read().unwrap_or_default();
    let mut out = Vec::new();
    for (dir, flow) in [(Direction::Capture, Flow::Capture), (Direction::Render, Flow::Render)] {
        let console = default_id(&en, dir, Role::Console);
        let comms = default_id(&en, dir, Role::Communications);
        let coll = en.get_device_collection(&dir).map_err(fail)?;
        // Index access, not the collection's iterator: that one unwraps a COM call on every step.
        let n = coll.get_nbr_devices().map_err(fail)?;
        for i in 0..n {
            let Ok(d) = coll.get_device_at_index(i) else { continue };
            let Ok(id) = d.get_id() else { continue };
            let name = d.get_friendlyname().unwrap_or_else(|_| id.clone());
            let (sample_rate, channels) = mix_format(&d).unwrap_or((0, 0));
            out.push(DeviceInfo {
                is_default: console.as_deref() == Some(id.as_str()),
                is_default_communications: comms.as_deref() == Some(id.as_str()),
                cable_selection: cables.endpoints.iter().find(|e| e.id == id && e.state == State::Active).and_then(|e| {
                    let cable = CableInstance::parse(&e.instance_id)?;
                    // A duplicate role on two nodes is a setup error. Keep a raw selection for any endpoint the forward
                    // resolver wouldn't open, so choosing the displayed device never silently opens a different one.
                    (active_cable_endpoint(&cables, cable.id, e.kind).as_deref() == Some(&id))
                        .then(|| cable_endpoint_selection(cable.id, e.kind))
                }),
                id,
                name,
                flow,
                sample_rate,
                channels,
            });
        }
    }
    Ok(out)
}

/// The shared-mode mix format's rate and channel count (what the engine converts from).
fn mix_format(d: &Device) -> Result<(u32, u16), WasapiError> {
    let f = d.get_iaudioclient()?.get_mixformat()?;
    Ok((f.get_samplespersec(), f.get_nchannels()))
}

/// The default endpoint's id for a direction and role, if there is one.
pub(crate) fn default_id(en: &DeviceEnumerator, dir: Direction, role: Role) -> Option<String> {
    en.get_default_device_for_role(&dir, &role).ok()?.get_id().ok()
}

/// Opens the endpoint `id`, or the console default for `dir` when `id` is `None`. Checks that it
/// is active and of the expected direction, so the user gets "not found" or "wrong kind of
/// device" rather than a later COM error.
pub(crate) fn resolve(en: &DeviceEnumerator, id: Option<&str>, dir: Direction, role: StreamRole) -> Result<Device, StartFailure> {
    if is_cable_selection(id) {
        return Err(StartFailure::NotFound(role)); // The requested role had no active endpoint at this open.
    }
    let device = match id {
        Some(id) => en.get_device(id),
        None => en.get_default_device_for_role(&dir, &Role::Console),
    };
    let device = device.map_err(|e| match hr_of(&e) {
        hr::E_NOTFOUND | hr::E_INVALIDARG => StartFailure::NotFound(role),
        code => StartFailure::Open { role, hr: code },
    })?;
    if device.get_direction() != dir {
        return Err(StartFailure::WrongDirection(role));
    }
    match device.get_state() {
        Ok(DeviceState::Active) => Ok(device),
        Ok(_) => Err(StartFailure::NotFound(role)),
        Err(e) => Err(StartFailure::Open { role, hr: hr_of(&e) }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_selection_reopens_a_new_windows_id_for_the_same_cable_and_role() {
        use nf_vmic::setup::endpoints::Endpoint;
        use nf_vmic::setup::plan::{Health, Model, Node};
        use nf_vmic::variant::Variant;
        let cable = CableInstance { label: "B".parse().unwrap(), id: CableId(17), n: 1 };
        let role = EndpointKind::CallMicrophone;
        let selection = cable_endpoint_selection(cable.id, role);
        assert_eq!(cable_role(&selection), Some((cable.id, role)));
        assert!(cable_role("raw Windows ID").is_none());
        let node = Node {
            instance_id: cable.instance_id(),
            model: Model::Cable(Variant::Both),
            health: Health::Working,
            cable: Some(cable.clone()),
            running: None,
            start_attempts: None,
            problem_status: None,
            config_flags: None,
        };
        let mut old =
            Endpoint { id: "old".into(), name: "Test".into(), kind: role, state: State::Active, instance_id: cable.instance_id() };
        let mut snapshot = Snapshot { nodes: vec![node], endpoints: vec![old.clone()] };
        assert_eq!(active_cable_endpoint(&snapshot, cable.id, role).as_deref(), Some("old"));
        old.state = State::NotPresent;
        snapshot.endpoints = vec![old.clone()];
        assert!(active_cable_endpoint(&snapshot, cable.id, role).is_none());
        snapshot.endpoints.push(Endpoint { id: "new".into(), state: State::Active, ..old });
        assert_eq!(active_cable_endpoint(&snapshot, cable.id, role).as_deref(), Some("new"));
        assert!(active_cable_endpoint(&snapshot, CableId(18), role).is_none());
        assert!(active_cable_endpoint(&snapshot, cable.id, EndpointKind::Microphone).is_none());
    }

    /// Enumerates the machine's devices (no streams are opened). Ignored by default: the owner
    /// runs it on a box with audio devices.
    #[test]
    #[ignore]
    fn lists_devices() {
        let list = list_devices().expect("enumeration");
        for d in &list {
            println!(
                "{:?} {:>6} Hz {} ch  default={} comms={}  {}  [{}]",
                d.flow, d.sample_rate, d.channels, d.is_default, d.is_default_communications, d.name, d.id
            );
        }
        for flow in [Flow::Capture, Flow::Render] {
            let defaults = list.iter().filter(|d| d.flow == flow && d.is_default).count();
            assert!(defaults <= 1, "at most one console default per direction");
        }
    }
}
