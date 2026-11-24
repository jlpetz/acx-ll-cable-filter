//! A cable's endpoints, by its instance ID (TODO 19, the per-cable design, sections 2 and 8): MMDevice enumeration, render
//! and capture, in the states ACTIVE, DISABLED and UNPLUGGED (not NOTPRESENT: a removed node's endpoints linger in that state).
//! Each endpoint's KS filter path ([`sessions::parse_filter`]: the documented device topology route first, then the property
//! store's copy) gives the instance ID of the node it's on and the circuit's reference string: `speaker0` the Speakers,
//! `microphone0` the Microphone, `microphone1` the Call Microphone. An endpoint belongs to the cable whose node has that
//! instance ID, compared without case; instance IDs are never another cable's (section 4), so a stale endpoint can't be
//! credited to the wrong cable. Names don't decide anything here; they are checked, after an install, against
//! "Speakers (ACX-LL-Cable D)" and so on.
//!
//! Reading the endpoints opens no stream: the enumeration, each endpoint's property store and its device topology. Tests call
//! only the pure checks.

use std::fmt;
use std::time::Duration;

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    DEVICE_STATE, DEVICE_STATE_ACTIVE, DEVICE_STATE_DISABLED, DEVICE_STATE_UNPLUGGED, IMMDeviceEnumerator, MMDeviceEnumerator, eAll,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, STGM_READ};

use super::Clock;
use crate::identity::{CableInstance, same_instance};
use crate::sessions::{self, PKEY_ENDPOINT_FILTER, connected_filter, device_id, on_mta_thread, string_property};
use crate::variant::Variant;
use crate::win_error::WinError;

/// The render circuit's KS filter, by its device interface's reference string (the INF's `KSNAME_Speaker`).
pub const SPEAKERS_FILTER: &str = "Speaker0";
/// How long Windows has, once a node works, to make its endpoints.
pub const ENDPOINTS_WAIT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(250);

/// Which of a cable's endpoints.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Speakers,
    Microphone,
    CallMicrophone,
}

impl Kind {
    /// The part of the endpoint's name before the brackets, as Windows names it: "Speakers", "Microphone", "Call Microphone".
    pub fn word(self) -> &'static str {
        match self {
            Kind::Speakers => "Speakers",
            Kind::Microphone => "Microphone",
            Kind::CallMicrophone => "Call Microphone",
        }
    }

    /// The kind a KS filter's reference string names, with or without a suffix (`microphone0/00010001`), any case.
    pub fn of_reference(reference: &str) -> Option<Self> {
        let name = reference.trim_start().split(|c: char| !c.is_ascii_alphanumeric()).next().unwrap_or_default();
        if name.eq_ignore_ascii_case(SPEAKERS_FILTER) {
            Some(Kind::Speakers)
        } else {
            match sessions::mic_of_filter(name) {
                sessions::Mic::LowLatency => Some(Kind::Microphone),
                sessions::Mic::Call => Some(Kind::CallMicrophone),
                sessions::Mic::Other => None,
            }
        }
    }

    /// A variant's endpoints: the Speakers, then its microphones.
    pub fn of_variant(v: Variant) -> Vec<Kind> {
        let mut kinds = vec![Kind::Speakers];
        kinds.extend(v.mics().iter().map(|m| match m {
            sessions::Mic::Call => Kind::CallMicrophone,
            _ => Kind::Microphone,
        }));
        kinds
    }
}

/// "Speakers (ACX-LL-Cable D)": the name we expect Windows to give (to confirm on the box: D1's FriendlyName).
pub fn expected_name(cable: &CableInstance, kind: Kind) -> String {
    format!("{} ({})", kind.word(), cable.label.friendly_name())
}

/// An endpoint's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Active,
    /// Disabled in the Sound panel.
    Disabled,
    Unplugged,
}

/// One endpoint on one of our nodes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// The MMDevice id (`{0.0.1.00000000}.{...}`).
    pub id: String,
    /// As Windows names it now.
    pub name: String,
    pub kind: Kind,
    pub state: State,
    /// The node it's on, as its KS filter path has it (lower case, usually).
    pub instance_id: String,
}

/// Every endpoint on a node in our instance-ID form. On a thread of its own, in the MTA. Any user may call it.
pub fn all() -> Result<Vec<Endpoint>, WinError> {
    on_mta_thread(all_on_this_thread)
}

fn all_on_this_thread() -> Result<Vec<Endpoint>, WinError> {
    // SAFETY: COM is initialised on this thread (on_mta_thread's); a plain in-process creation.
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(WinError::of("CoCreateInstance(MMDeviceEnumerator)"))?;
    let states = DEVICE_STATE(DEVICE_STATE_ACTIVE.0 | DEVICE_STATE_DISABLED.0 | DEVICE_STATE_UNPLUGGED.0);
    // SAFETY: a live enumerator; constant arguments.
    let endpoints =
        unsafe { enumerator.EnumAudioEndpoints(eAll, states) }.map_err(WinError::of("IMMDeviceEnumerator::EnumAudioEndpoints"))?;
    // SAFETY: a live collection.
    let count = unsafe { endpoints.GetCount() }.map_err(WinError::of("IMMDeviceCollection::GetCount"))?;
    let mut out = Vec::new();
    for i in 0..count {
        // One that went away meanwhile, or can't be read, isn't listed; the others still are
        // SAFETY: a live collection and an index below its count.
        let Ok(device) = (unsafe { endpoints.Item(i) }) else { continue };
        // SAFETY: a live device; read access only.
        let Ok(store) = (unsafe { device.OpenPropertyStore(STGM_READ) }) else { continue };
        let cached = string_property(&store, &PKEY_ENDPOINT_FILTER).ok().filter(|p| !p.is_empty());
        let topology = connected_filter(&device);
        let Some((instance_id, kind)) = attribute(&[topology.as_deref(), cached.as_deref()]) else { continue };
        // SAFETY: a live device.
        let Ok(state) = (unsafe { device.GetState() }) else { continue };
        let state = match state {
            s if s == DEVICE_STATE_ACTIVE => State::Active,
            s if s == DEVICE_STATE_DISABLED => State::Disabled,
            _ => State::Unplugged,
        };
        let Ok(id) = device_id(&device) else { continue };
        let name = string_property(&store, &PKEY_Device_FriendlyName).unwrap_or_default();
        out.push(Endpoint { id, name, kind, state, instance_id });
    }
    Ok(out)
}

/// The node and kind of an endpoint, from the copies of its KS filter path read (the topology's first): the first that parses,
/// names one of our circuits, and is on a node of our instance-ID form.
pub fn attribute(filters: &[Option<&str>]) -> Option<(String, Kind)> {
    filters.iter().flatten().find_map(|path| {
        let (instance, reference) = sessions::parse_filter(path)?;
        CableInstance::parse(&instance)?;
        Some((instance, Kind::of_reference(&reference)?))
    })
}

/// What a check found of a cable's endpoints ([`check`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EndpointCheck {
    /// Each expected endpoint there, its name as Windows has it.
    pub found: Vec<(Kind, Endpoint)>,
    pub missing: Vec<Kind>,
    /// There, but not named as expected: the kind, Windows' name, the expected one.
    pub misnamed: Vec<(Kind, String, String)>,
}

impl EndpointCheck {
    pub fn is_done(&self) -> bool {
        self.missing.is_empty() && self.misnamed.is_empty()
    }
}

impl fmt::Display for EndpointCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if !self.missing.is_empty() {
            parts.push(format!("Windows hasn't made {}", self.missing.iter().map(|k| k.word()).collect::<Vec<_>>().join(", ")));
        }
        for (_, got, want) in &self.misnamed {
            parts.push(format!("\"{got}\" isn't named \"{want}\""));
        }
        if parts.is_empty() { f.write_str("every endpoint there, as named") } else { f.write_str(&parts.join("; ")) }
    }
}

/// `cable`'s endpoints for `variant` in `now`: each kind there on that instance (an active one before another), named as
/// expected. A second endpoint of a kind is ignored.
pub fn check(cable: &CableInstance, variant: Variant, now: &[Endpoint]) -> EndpointCheck {
    let id = cable.instance_id();
    let mut out = EndpointCheck::default();
    for kind in Kind::of_variant(variant) {
        let mine = |active: bool| {
            now.iter().find(|e| e.kind == kind && same_instance(&e.instance_id, &id) && (e.state == State::Active) == active)
        };
        match mine(true).or_else(|| mine(false)) {
            Some(e) => {
                let want = expected_name(cable, kind);
                if e.name != want {
                    out.misnamed.push((kind, e.name.clone(), want));
                }
                out.found.push((kind, e.clone()));
            }
            None => out.missing.push(kind),
        }
    }
    out
}

/// Endpoints of `old` instances (a removed or switched cable's) still active.
pub fn still_active<'a>(old: &[String], now: &'a [Endpoint]) -> Vec<&'a Endpoint> {
    now.iter().filter(|e| e.state == State::Active && old.iter().any(|o| same_instance(o, &e.instance_id))).collect()
}

/// Up to `wait` on `clock` for `cable`'s endpoints, reading them with `read` every [`POLL`]: the last look that read them (with
/// the last error if a look failed), done or not.
pub fn wait_for(
    cable: &CableInstance, variant: Variant, wait: Duration, read: &mut dyn FnMut() -> Result<Vec<Endpoint>, WinError>,
    clock: &mut dyn Clock,
) -> (Option<EndpointCheck>, Option<WinError>) {
    let mut last = None;
    loop {
        let failed = match read() {
            Ok(now) => {
                last = Some(check(cable, variant, &now));
                None
            }
            Err(e) => Some(e),
        };
        if (failed.is_none() && last.as_ref().is_some_and(EndpointCheck::is_done)) || clock.elapsed() >= wait {
            return (last, failed);
        }
        clock.sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::plan::tests::{A_ID, B_ID, cid, label};
    use crate::setup::tests::FakeClock;

    fn cable(l: &str, id: &str, n: u32) -> CableInstance {
        CableInstance { label: label(l), id: cid(id), n }
    }

    /// An endpoint as its KS filter path has the instance ID: lower case.
    fn ep(c: &CableInstance, kind: Kind, state: State, name: Option<&str>) -> Endpoint {
        Endpoint {
            id: format!("{{{}.{kind:?}}}", c.n),
            name: name.map_or_else(|| expected_name(c, kind), str::to_string),
            kind,
            state,
            instance_id: c.instance_id().to_ascii_lowercase(),
        }
    }

    #[test]
    fn an_endpoint_is_attributed_by_its_filter_path() {
        let a = cable("A", A_ID, 1);
        let path = |reference: &str| {
            format!(r"{{2}}.\\?\{}#{{6994ad04-93ef-11d0-a3cc-00a0c9223196}}\{reference}", a.instance_id().replace('\\', "#").to_lowercase())
        };
        let speaker = path("speaker0");
        assert_eq!(attribute(&[Some(&speaker)]), Some((a.instance_id().to_lowercase(), Kind::Speakers)));
        assert_eq!(attribute(&[Some(&path("microphone1/00010001"))]).map(|x| x.1), Some(Kind::CallMicrophone));
        // The topology's names neither, or is another node's: the property store's copy decides
        let other = r"\\?\root#media#0003#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\microphone0";
        assert_eq!(attribute(&[Some(other), Some(&path("microphone0"))]).map(|x| x.1), Some(Kind::Microphone));
        assert_eq!(attribute(&[Some(&path("pin3")), None]), None);
        assert_eq!(attribute(&[None, Some(other)]), None, "a node made outside the tool isn't a cable");
        assert_eq!(Kind::of_variant(Variant::Both), [Kind::Speakers, Kind::Microphone, Kind::CallMicrophone]);
        assert_eq!(Kind::of_variant(Variant::Call), [Kind::Speakers, Kind::CallMicrophone]);
        assert_eq!(expected_name(&a, Kind::CallMicrophone), "Call Microphone (ACX-LL-Cable A)");
    }

    /// The per-cable filter on endpoints, with two cables present: B's check sees only B's.
    #[test]
    fn a_cables_check_sees_only_its_own_endpoints() {
        let a = cable("A", A_ID, 1);
        let b = cable("B", B_ID, 1);
        let mut now: Vec<Endpoint> = Kind::of_variant(Variant::Both).into_iter().map(|k| ep(&a, k, State::Active, None)).collect();
        now.push(ep(&b, Kind::Speakers, State::Active, None));
        let check_b = check(&b, Variant::LowLatency, &now);
        assert_eq!((check_b.found.len(), check_b.missing.clone()), (1, vec![Kind::Microphone]), "A's Microphone isn't B's");
        now.push(ep(&b, Kind::Microphone, State::Active, None));
        assert!(check(&b, Variant::LowLatency, &now).is_done());
        assert!(check(&a, Variant::Both, &now).is_done());
        // A disabled one is there; an active one is taken first
        let mut off = now.clone();
        off.push(ep(&b, Kind::Microphone, State::Disabled, Some("Studio mic (ACX-LL-Cable B)")));
        assert!(check(&b, Variant::LowLatency, &off).is_done());
        // Named otherwise (Windows' "2- " numbering, a missing friendly name): there, but reported
        let numbered =
            vec![ep(&b, Kind::Speakers, State::Active, Some("Speakers (2- ACX-LL-Cable)")), ep(&b, Kind::Microphone, State::Active, None)];
        let c = check(&b, Variant::LowLatency, &numbered);
        assert!(!c.is_done() && c.missing.is_empty());
        assert_eq!(c.to_string(), "\"Speakers (2- ACX-LL-Cable)\" isn't named \"Speakers (ACX-LL-Cable B)\"");
        // The new instance's endpoints, not the old one's (a variant switch: n + 1)
        let b2 = cable("B", B_ID, 2);
        assert_eq!(check(&b2, Variant::LowLatency, &now).missing, [Kind::Speakers, Kind::Microphone]);
        let old = [b.instance_id()];
        assert_eq!(still_active(&old, &now).len(), 2);
        assert!(still_active(&[a.instance_id()], &now[3..]).is_empty());
    }

    #[test]
    fn the_wait_for_the_endpoints_ends_as_it_should() {
        let b = cable("B", B_ID, 2);
        let full: Vec<Endpoint> = Kind::of_variant(Variant::Call).into_iter().map(|k| ep(&b, k, State::Active, None)).collect();
        let failed = WinError { call: "IMMDeviceEnumerator::EnumAudioEndpoints", code: 5 };
        let over = |looks: Vec<Result<Vec<Endpoint>, WinError>>| {
            let mut i = 0;
            let mut read = || {
                i += 1;
                looks[(i - 1).min(looks.len() - 1)].clone()
            };
            let mut clock = FakeClock(Duration::ZERO);
            let r = wait_for(&b, Variant::Call, Duration::from_secs(1), &mut read, &mut clock);
            (r, clock.0)
        };
        let ((check, err), took) = over(vec![Ok(full.clone())]);
        assert!(check.unwrap().is_done() && err.is_none() && took == Duration::ZERO);
        let ((check, _), took) = over(vec![Ok(vec![]), Err(failed.clone()), Ok(full.clone())]);
        assert!(check.unwrap().is_done());
        assert_eq!(took, POLL * 2);
        let ((check, err), took) = over(vec![Ok(full[..1].to_vec()), Err(failed.clone())]);
        assert_eq!((check.unwrap().missing, err, took), (vec![Kind::CallMicrophone], Some(failed.clone()), Duration::from_secs(1)));
        let ((check, err), _) = over(vec![Err(failed.clone())]);
        assert_eq!((check, err), (None, Some(failed)));
    }
}
