//! Endpoint enumeration, by-id lookup and the default-endpoint lookup.
//!
//! wasapi objects are `!Send` and live in the apartment that made them, so callers outside the
//! engine's own MTA threads (the UI is STA) go through [`list_devices`], which enumerates on a
//! short-lived MTA thread and returns plain data.

use wasapi::{Device, DeviceEnumerator, DeviceState, Direction, Role, WasapiError};

use crate::api::{DeviceInfo, EngineError, Flow};
use crate::errors::{StartFailure, StreamRole, hr};
use crate::sched::Com;
use crate::stream::hr_of;

/// Lists the active capture endpoints, then the active render endpoints.
pub(crate) fn list_devices() -> Result<Vec<DeviceInfo>, EngineError> {
    let worker = std::thread::Builder::new()
        .name("nf-devices".into())
        .spawn(list_on_this_thread)
        .map_err(|_| EngineError("Couldn't start listing audio devices.".into()))?;
    worker.join().map_err(|_| EngineError("Listing audio devices failed unexpectedly.".into()))?
}

fn list_on_this_thread() -> Result<Vec<DeviceInfo>, EngineError> {
    let fail = |e: WasapiError| {
        EngineError(format!("Windows couldn't list the audio devices (error 0x{:08X}).", hr_of(&e) as u32))
    };
    let _com = Com::mta()
        .map_err(|c| EngineError(format!("Windows audio couldn't be initialised (error 0x{:08X}).", c as u32)))?;
    let en = DeviceEnumerator::new().map_err(fail)?;
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
