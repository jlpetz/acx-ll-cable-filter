//! Our cables' endpoints, which apps record from our microphones, and which of the two suits each (TODO 19, the owner's option A:
//! two microphones, and the app says which apps use which; the per-cable design, doc/todo/19-per-cable-design.md, section 2).
//!
//! An endpoint is ours, and which cable's, by the KS filter it's connected to ([`identify_endpoint`]): the filter's device
//! interface path holds the device's instance ID (`root#media#acxllcable_d_<id>_1`, which names the cable: [`CableInstance`])
//! and the circuit's reference string (`speaker0` the Speakers, `microphone0` the low-latency Microphone, `microphone1` the Call
//! Microphone), which hold whatever the endpoint is named. Windows may number a device's names
//! ("Microphone (2- ACX-LL-Cable A)"), and the user may rename an endpoint in the Sound panel; only when no copy of the filter's
//! path parses does the name decide ([`is_ours`], [`mic_of`]): its part in brackets, the cable's device's friendly name
//! ("ACX-LL-Cable D"), which a rename leaves alone.
//!
//! [`cable_endpoints`] lists our endpoints, render and capture, active, disabled or unplugged, each with its cable; [`pick`]
//! finds one cable's Speakers or microphone among them, and [`endpoint_by_id`] the cable of an endpoint already chosen.
//! [`cable_of_endpoint`] reads one endpoint by its id alone (the engine's direct-feed guard). Where an endpoint's filter can't
//! be read, [`label_of_name`] reads the cable's label from its name, for a caller that checks it against the cables present.
//!
//! [`list`] reads Windows' audio sessions: each of our active capture endpoints (`IMMDeviceEnumerator::EnumAudioEndpoints`), its
//! `IAudioSessionManager2`'s sessions, and for each session its process (`IAudioSessionControl2::GetProcessId`, then
//! `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` and `QueryFullProcessImageNameW`) and its state. It returns data, for the
//! CLI's `mic sessions` and later the window. A session exists for each app that opened the endpoint in shared mode; an app
//! recording it in exclusive mode has none, so it isn't listed.
//!
//! [`classify`] (pure) sorts an app by its exe's name with [`KNOWN_APPS`]; [`fit`] says whether the microphone it records
//! from suits it.
//!
//! [`capture_endpoints`] reads our capture endpoints alone, active or disabled, without their sessions: what `mic variant`
//! shows, and what its switch waits for (`variant::check_mics`).

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{PROPERTYKEY, S_OK};
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, AudioSessionStateExpired, AudioSessionStateInactive, DEVICE_STATE, DEVICE_STATE_ACTIVE, DEVICE_STATE_DISABLED,
    DEVICE_STATE_UNPLUGGED, EDataFlow, IAudioSessionControl2, IAudioSessionManager2, IDeviceTopology, IMMDevice, IMMDeviceEnumerator,
    IMMEndpoint, MMDeviceEnumerator, eCapture, eRender,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, STGM_READ,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::core::{GUID, HSTRING, Interface, Owned, PWSTR};

use crate::cables::EndpointKind;
use crate::identity::{CableId, CableInstance, Label, PRODUCT};
use crate::win_error::WinError;

/// The endpoint's copy of the KS filter path it's connected to, in its property store: not documented; seen on Windows
/// build 26100 as `{2}.\\?\root#media#0004#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\microphone0`, on a disabled endpoint too.
/// [`identify_endpoint`] goes by it only when the documented route (the endpoint's device topology) doesn't name one of a
/// cable's endpoints.
pub(crate) const PKEY_ENDPOINT_FILTER: PROPERTYKEY = PROPERTYKEY { fmtid: GUID::from_u128(0x233164c8_1b2c_4c7d_bc68_b671687a2567), pid: 1 };

/// Which of our microphones an endpoint is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mic {
    /// "Microphone (ACX-LL-Cable A)" (Microphone0): IAudioClient3's small periods (2.67-10 ms), for apps that ask for them.
    LowLatency,
    /// "Call Microphone (ACX-LL-Cable A)" (Microphone1): a 5 ms default period, for apps that ask for none (call apps).
    Call,
    /// Another endpoint of ours: one whose filter isn't either microphone's, or, without its filter, one by another name
    /// (renamed in the Sound panel, say).
    Other,
}

impl Mic {
    /// "the Call Microphone", "the Microphone".
    pub fn describe(self) -> &'static str {
        match self {
            Mic::LowLatency => "the Microphone",
            Mic::Call => "the Call Microphone",
            Mic::Other => "another ACX-LL-Cable microphone",
        }
    }

    /// The microphone a cable endpoint's kind is (the Speakers, or none known, are [`Mic::Other`]).
    pub fn of_kind(kind: Option<EndpointKind>) -> Mic {
        match kind {
            Some(EndpointKind::Microphone) => Mic::LowLatency,
            Some(EndpointKind::CallMicrophone) => Mic::Call,
            Some(EndpointKind::Speakers) | None => Mic::Other,
        }
    }
}

/// An endpoint's [`Mic`], from its friendly name (when its filter can't be read: [`identify`]). A numbered name
/// ("Microphone (2- ACX-LL-Cable A)") is still the microphone; a renamed one is [`Mic::Other`].
pub fn mic_of(endpoint_name: &str) -> Mic {
    let name = endpoint_name.trim_start().to_ascii_lowercase();
    if name.starts_with("call microphone") {
        Mic::Call
    } else if name.starts_with("microphone") {
        Mic::LowLatency
    } else {
        Mic::Other
    }
}

/// An endpoint's kind from its friendly name, when its filter can't be read: a render endpoint named "Speakers ..." is the
/// Speakers, a capture one as [`mic_of`] reads it (a numbered name still counts; a renamed one is none).
fn kind_of_name(endpoint_name: &str, render: bool) -> Option<EndpointKind> {
    if render {
        endpoint_name.trim_start().to_ascii_lowercase().starts_with("speakers").then_some(EndpointKind::Speakers)
    } else {
        match mic_of(endpoint_name) {
            Mic::LowLatency => Some(EndpointKind::Microphone),
            Mic::Call => Some(EndpointKind::CallMicrophone),
            Mic::Other => None,
        }
    }
}

/// Whether an endpoint is one of our cables': by the KS filter it's connected to (`filters`, the copies of the filter's device
/// interface path that were read, the documented route's first, as [`identify`] takes them), whose device's instance ID has
/// our form, `ROOT\MEDIA\ACXLLCABLE_<label>_<id>_<n>` ([`CableInstance::parse`]), whatever the endpoint is named. The first copy
/// that parses decides: a renamed endpoint of ours is ours, and another vendor's renamed to look like one isn't. Only where
/// none parses (none read, or each in a form [`parse_filter`] doesn't know) does the name decide ([`has_our_name`]).
pub fn is_ours(filters: &[&str], name: &str) -> bool {
    match filters.iter().find_map(|p| parse_filter(p)) {
        Some((instance, _)) => CableInstance::parse(&instance).is_some(),
        None => has_our_name(name),
    }
}

/// Whether an endpoint's name has "ACX-LL-Cable" in it, any case: its part in brackets is its device's friendly name
/// ("ACX-LL-Cable D"), which a rename in the Sound panel leaves alone. [`is_ours`]'s fallback, for an endpoint whose filter
/// can't be read, and the engine's check on a name alone.
pub fn has_our_name(endpoint_name: &str) -> bool {
    endpoint_name.to_ascii_lowercase().contains(&PRODUCT.to_ascii_lowercase())
}

/// The cable label an endpoint's name carries in its last brackets, upper case: "Microphone (ACX-LL-Cable D)", or with Windows'
/// numbering, "Microphone (2- ACX-LL-Cable D)", is "D". The part in brackets is the cable's device's friendly name, which the
/// driver sets from its label and a rename in the Sound panel leaves alone. `None` where the brackets don't hold
/// "ACX-LL-Cable <label>" (any case) with a label of one to eight of A-Z and 0-9. A name alone doesn't tell two cables with
/// one label apart: only for an endpoint whose filter can't be read, and checked against the cables present.
pub fn label_of_name(endpoint_name: &str) -> Option<Label> {
    let inner = endpoint_name.trim_end().strip_suffix(')')?;
    let mut device = inner[inner.rfind('(')? + 1..].trim();
    // Windows numbers a second device of one name: "2- ACX-LL-Cable D"
    if let Some((n, rest)) = device.split_once("- ")
        && !n.is_empty()
        && n.bytes().all(|b| b.is_ascii_digit())
    {
        device = rest;
    }
    let prefix = device.get(..PRODUCT.len()).filter(|s| s.eq_ignore_ascii_case(PRODUCT))?;
    device[prefix.len()..].strip_prefix(' ')?.parse().ok()
}

/// The microphone a KS filter's reference string names: the driver's capture circuits (`microphone0`, `microphone1`; any
/// case, as the path has them in lower case), with or without a suffix after the name (`microphone0/00010001`: whatever
/// follows its first character that isn't a letter or a digit), so `microphone01` isn't `microphone0`
/// ([`EndpointKind::of_reference`]).
pub fn mic_of_filter(reference: &str) -> Mic {
    Mic::of_kind(EndpointKind::of_reference(reference))
}

/// A KS filter's device interface path, with or without DeviceTopology's `{2}.` before it
/// (`{2}.\\?\root#media#0004#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\microphone0`), as its device's instance ID
/// (`root\media\0004`, in the path's case) and its reference string (`microphone0`; empty if it has none).
pub fn parse_filter(path: &str) -> Option<(String, String)> {
    let path = path.trim();
    let path = match path.strip_prefix('{') {
        Some(rest) => rest.split_once("}.")?.1,
        None => path,
    };
    let rest = path.strip_prefix(r"\\?\")?;
    // The instance ID's `\` are `#` in the path, so the first `\` starts the reference string
    let (head, reference) = rest.split_once('\\').unwrap_or((rest, ""));
    let class = head.rfind("#{")?;
    let instance = head[..class].replace('#', r"\");
    (!instance.is_empty()).then(|| (instance, reference.to_string()))
}

/// An endpoint's kind and the instance ID of the device it's on, from the KS filter it's connected to: `filters` are the copies
/// of the filter's device interface path that were read, the documented route's first; `render` its flow. The first that
/// parses and names one of a cable's endpoints of that flow decides (`speaker0` for render, `microphone0` and `microphone1`
/// for capture). If none does (none read or parsed, or each in a form [`parse_filter`] and [`EndpointKind::of_reference`]
/// don't know), the name decides the kind, and the device is the first parsed path's, if any.
pub fn identify_endpoint(filters: &[&str], name: &str, render: bool) -> (Option<EndpointKind>, Option<String>) {
    let parsed: Vec<(String, String)> = filters.iter().filter_map(|p| parse_filter(p)).collect();
    let named = parsed.iter().find_map(|(instance, reference)| {
        EndpointKind::of_reference(reference).filter(|k| k.is_render() == render).map(|k| (k, instance.clone()))
    });
    match named {
        Some((kind, instance)) => (Some(kind), Some(instance)),
        None => (kind_of_name(name, render), parsed.into_iter().next().map(|(instance, _)| instance)),
    }
}

/// A capture endpoint's microphone and the instance ID of the device it's on ([`identify_endpoint`] for capture): the first
/// copy of the filter's path that parses and names one of our microphones decides; else the name ([`mic_of`]), on the device
/// the first parsed path names, if any.
pub fn identify(filters: &[&str], name: &str) -> (Mic, Option<String>) {
    let (kind, instance) = identify_endpoint(filters, name, false);
    (Mic::of_kind(kind), instance)
}

/// An endpoint's state, as Windows keeps it (a removed device's endpoints, NOTPRESENT, aren't listed: [`cable_endpoints`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointState {
    Active,
    /// Disabled in the Sound panel: apps don't see it.
    Disabled,
    /// Its device is there, the endpoint unplugged (a jack's state; not expected for a cable).
    Unplugged,
}

impl EndpointState {
    pub fn describe(self) -> &'static str {
        match self {
            EndpointState::Active => "active",
            EndpointState::Disabled => "disabled in the Sound panel",
            EndpointState::Unplugged => "unplugged",
        }
    }
}

/// One of our cables' endpoints, render or capture, as [`cable_endpoints`] finds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CableEndpoint {
    /// The MMDevice endpoint id (`{0.0.1.00000000}.{...}`).
    pub id: String,
    /// "Microphone (ACX-LL-Cable A)", as Windows names it now (numbered, or renamed in the Sound panel).
    pub name: String,
    /// A render endpoint (the Speakers); otherwise capture.
    pub render: bool,
    /// Which of the cable's endpoints it is ([`identify_endpoint`]); `None` where neither its filter nor its name says.
    pub kind: Option<EndpointKind>,
    pub state: EndpointState,
    /// The instance ID of the device it's on, if its filter was read.
    pub instance: Option<String>,
    /// The cable that instance ID names; `None` where the filter wasn't read (it's ours by its name alone, [`is_ours`]).
    pub cable: Option<CableInstance>,
}

impl CableEndpoint {
    /// "\"Microphone (ACX-LL-Cable A)\": cable A's Microphone (Microphone0), active".
    pub fn describe(&self) -> String {
        let what = match (&self.cable, self.kind) {
            (Some(c), Some(k)) => format!("cable {}'s {} ({})", c.label, k.name(), k.reference()),
            (Some(c), None) => format!("cable {}'s, neither its Speakers nor a microphone", c.label),
            (None, Some(k)) => format!("a {} of ours by its name, its device unknown (its filter wasn't read)", k.name()),
            (None, None) => "ours by its name, its device unknown (its filter wasn't read)".to_string(),
        };
        format!("{:?}: {what}, {}", self.name, self.state.describe())
    }
}

/// What [`pick`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum Pick<'a> {
    /// The one active endpoint of that kind on that cable.
    Found(&'a CableEndpoint),
    /// None of that kind is active on the cable: the cable's endpoints that are listed (of any kind and state), to name them.
    NotActive(Vec<&'a CableEndpoint>),
    /// More than one is active (two device nodes of one cable present at once: a variant switch midway).
    Several(Vec<&'a CableEndpoint>),
}

/// The endpoint of `kind` on cable `id` among `endpoints` ([`cable_endpoints`]), by the instance ID its KS filter names, never by
/// its name: active, and the only one.
pub fn pick<'a>(endpoints: &'a [CableEndpoint], id: &CableId, kind: EndpointKind) -> Pick<'a> {
    let on_cable: Vec<&CableEndpoint> = endpoints.iter().filter(|e| e.cable.as_ref().is_some_and(|c| c.id == *id)).collect();
    let active: Vec<&CableEndpoint> =
        on_cable.iter().copied().filter(|e| e.kind == Some(kind) && e.state == EndpointState::Active).collect();
    match active.as_slice() {
        [one] => Pick::Found(one),
        [] => Pick::NotActive(on_cable),
        _ => Pick::Several(active),
    }
}

/// The endpoint with MMDevice id `endpoint_id` among `endpoints`, if it's ours (ids compared without case).
pub fn endpoint_by_id<'a>(endpoints: &'a [CableEndpoint], endpoint_id: &str) -> Option<&'a CableEndpoint> {
    endpoints.iter().find(|e| e.id.eq_ignore_ascii_case(endpoint_id))
}

/// One of our capture endpoints and the sessions on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// The endpoint's id (`{0.0.1.00000000}.{...}`), as `noise-filter devices` shows it.
    pub id: String,
    /// "Call Microphone (ACX-LL-Cable A)".
    pub name: String,
    pub mic: Mic,
    pub sessions: Vec<Session>,
}

/// A session's state, as Windows keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// A stream in it is running: the app is recording.
    Active,
    /// It has a stream, stopped: the app has the microphone open but isn't recording now.
    Inactive,
    /// Its streams are gone; Windows drops it soon.
    Expired,
}

impl SessionState {
    pub fn describe(self) -> &'static str {
        match self {
            SessionState::Active => "active",
            SessionState::Inactive => "inactive",
            SessionState::Expired => "expired",
        }
    }
}

/// One app's session on an endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    /// 0 when the session spans processes or the id couldn't be read.
    pub pid: u32,
    /// The process's exe (`C:\...\Discord.exe`), or the call that couldn't read it (the process may have exited).
    pub image: Result<String, WinError>,
    pub state: SessionState,
    /// Windows' system-sounds session.
    pub system_sounds: bool,
}

impl Session {
    /// The exe's file name (`Discord.exe`), if the image was read.
    pub fn exe(&self) -> Option<&str> {
        self.image.as_deref().ok().map(file_name)
    }

    pub fn kind(&self) -> AppKind {
        self.exe().map_or(AppKind::Unknown, classify)
    }
}

/// What an app is, for choosing its microphone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AppKind {
    /// A call app: it asks for no period, so the Call Microphone's 5 ms default halves its latency.
    Call,
    /// A browser: web calls, probably.
    WebCall,
    /// A recorder that asks for a small period (a DAW): the low-latency Microphone.
    LowLatency,
    /// Not in [`KNOWN_APPS`]: no suggestion.
    Unknown,
}

impl AppKind {
    /// "a call app", "a browser (web calls, probably)", "a low-latency recorder", "an app we don't know".
    pub fn describe(self) -> &'static str {
        match self {
            AppKind::Call => "a call app",
            AppKind::WebCall => "a browser (web calls, probably)",
            AppKind::LowLatency => "a low-latency recorder",
            AppKind::Unknown => "an app we don't know",
        }
    }

    /// The microphone that suits it, if we know.
    pub fn suited_mic(self) -> Option<Mic> {
        match self {
            AppKind::Call | AppKind::WebCall => Some(Mic::Call),
            AppKind::LowLatency => Some(Mic::LowLatency),
            AppKind::Unknown => None,
        }
    }
}

/// How an exe's name is matched: lower case, the whole name or its start (with `.exe` at the end).
#[derive(Clone, Copy, Debug)]
pub enum ExeName {
    Exact(&'static str),
    Prefix(&'static str),
}

impl ExeName {
    fn matches(self, exe: &str) -> bool {
        match self {
            ExeName::Exact(name) => exe == name,
            ExeName::Prefix(start) => exe.starts_with(start) && exe.ends_with(".exe"),
        }
    }
}

/// THE KNOWN APPS: an exe's name (lower case) and what it is. Add an app here, and a case to the test below.
/// Call apps ask for no period, so they suit the Call Microphone; browsers are there for web calls; the recorders ask for a
/// small period, so they suit the low-latency Microphone.
#[rustfmt::skip]
pub const KNOWN_APPS: &[(ExeName, AppKind)] = &[
    // Call apps
    (ExeName::Exact("discord.exe"), AppKind::Call),
    (ExeName::Exact("ms-teams.exe"), AppKind::Call),       // the new Teams
    (ExeName::Exact("teams.exe"), AppKind::Call),          // Teams classic
    (ExeName::Exact("zoom.exe"), AppKind::Call),
    (ExeName::Exact("slack.exe"), AppKind::Call),
    (ExeName::Exact("skype.exe"), AppKind::Call),
    (ExeName::Prefix("webex"), AppKind::Call),             // webex.exe, webexhost.exe, ...
    // Browsers: web calls, probably
    (ExeName::Exact("chrome.exe"), AppKind::WebCall),
    (ExeName::Exact("msedge.exe"), AppKind::WebCall),
    (ExeName::Exact("firefox.exe"), AppKind::WebCall),
    // Low-latency recorders
    (ExeName::Exact("reaper.exe"), AppKind::LowLatency),
    (ExeName::Prefix("ableton"), AppKind::LowLatency),     // "Ableton Live 12 Suite.exe"
    (ExeName::Prefix("bitwig"), AppKind::LowLatency),      // "Bitwig Studio.exe"
    (ExeName::Exact("fl64.exe"), AppKind::LowLatency),     // FL Studio
    (ExeName::Prefix("cubase"), AppKind::LowLatency),      // "Cubase14.exe"
    (ExeName::Prefix("studio one"), AppKind::LowLatency),  // "Studio One.exe"
];

/// An exe path's file name: what follows the last `\` or `/`.
fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// What an app is, from its exe's path or file name ([`KNOWN_APPS`]; any case).
pub fn classify(image: &str) -> AppKind {
    let exe = file_name(image.trim()).to_lowercase();
    KNOWN_APPS.iter().find(|(name, _)| name.matches(&exe)).map_or(AppKind::Unknown, |&(_, kind)| kind)
}

/// Whether the microphone an app records from suits it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    /// It's on the one that suits it.
    Suits,
    /// The other one suits it better.
    Better(Mic),
    /// We don't know the app, or which microphone this is.
    Unknown,
}

/// How `app` fits the microphone `on`.
pub fn fit(app: AppKind, on: Mic) -> Fit {
    match (app.suited_mic(), on) {
        (None, _) | (_, Mic::Other) => Fit::Unknown,
        (Some(best), on) if best == on => Fit::Suits,
        (Some(best), _) => Fit::Better(best),
    }
}

/// The apps recording from each of our active capture endpoints. On a thread of its own, in the MTA, so any caller (the
/// window's STA thread too) may call it.
pub fn list() -> Result<Vec<Endpoint>, WinError> {
    on_mta_thread(list_on_this_thread)
}

/// One of our capture endpoints, as [`capture_endpoints`] finds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureEndpoint {
    /// `{0.0.1.00000000}.{...}`: a new device's endpoints get new ones.
    pub id: String,
    /// "Call Microphone (ACX-LL-Cable A)", as Windows names it now (numbered, or renamed in the Sound panel).
    pub name: String,
    /// Active; otherwise disabled (in the Sound panel's Recording tab).
    pub active: bool,
    /// Which microphone it is ([`identify`]).
    pub mic: Mic,
    /// The instance ID of the device it's on (`root\media\acxllcable_a_<id>_1`), if its filter was read ([`identify`]).
    pub device: Option<String>,
}

/// Our capture endpoints, active or disabled (an absent device's aren't listed). On a thread of its own, as [`list`]. Any
/// user may call it (the enumeration, the property store and the device topology read nothing an administrator alone may:
/// to be confirmed live from a standard user's prompt).
pub fn capture_endpoints() -> Result<Vec<CaptureEndpoint>, WinError> {
    on_mta_thread(capture_endpoints_on_this_thread)
}

/// Runs `work` on a thread of its own, in the MTA.
pub(crate) fn on_mta_thread<T: Send + 'static>(work: impl FnOnce() -> Result<T, WinError> + Send + 'static) -> Result<T, WinError> {
    let worker = std::thread::Builder::new()
        .name("nf-sessions".into())
        .spawn(move || {
            let _com = Mta::enter()?;
            work()
        })
        .map_err(|e| WinError { call: "CreateThread", code: e.raw_os_error().unwrap_or(0) as u32 })?;
    worker.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// COM in the multithreaded apartment for this thread, uninitialised on drop.
struct Mta;

impl Mta {
    fn enter() -> Result<Self, WinError> {
        // SAFETY: no reserved pointer; this thread is our own and hasn't chosen an apartment.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_ok() { Ok(Mta) } else { Err(WinError { call: "CoInitializeEx", code: hr.0 as u32 }) }
    }
}

impl Drop for Mta {
    fn drop(&mut self) {
        // SAFETY: balances the CoInitializeEx that succeeded (S_OK or S_FALSE) on this thread.
        unsafe { CoUninitialize() };
    }
}

fn list_on_this_thread() -> Result<Vec<Endpoint>, WinError> {
    let mut out = Vec::new();
    for ours in our_endpoints(eCapture, DEVICE_STATE_ACTIVE)? {
        // One that went away meanwhile isn't listed, nor one whose sessions can't be read: the others still are
        let Ok(id) = device_id(&ours.device) else { continue };
        let Ok(sessions) = sessions_of(&ours.device) else { continue };
        let (mic, _) = identify(&ours.filters(), &ours.name);
        out.push(Endpoint { id, mic, sessions, name: ours.name });
    }
    Ok(out)
}

fn capture_endpoints_on_this_thread() -> Result<Vec<CaptureEndpoint>, WinError> {
    let mut out = Vec::new();
    for ours in our_endpoints(eCapture, DEVICE_STATE(DEVICE_STATE_ACTIVE.0 | DEVICE_STATE_DISABLED.0))? {
        // One that went away meanwhile isn't listed, as one whose name couldn't be read isn't
        // SAFETY: a live device.
        let Ok(state) = (unsafe { ours.device.GetState() }) else { continue };
        let Ok(id) = device_id(&ours.device) else { continue };
        let (mic, device) = identify(&ours.filters(), &ours.name);
        out.push(CaptureEndpoint { id, active: state == DEVICE_STATE_ACTIVE, mic, device, name: ours.name });
    }
    Ok(out)
}

/// Our cables' endpoints, render and capture, active, disabled or unplugged, each with the cable its KS filter's instance ID
/// names ([`identify_endpoint`], [`CableInstance::parse`]). Not NOTPRESENT: a removed device's endpoints linger in that state,
/// and a cable's instance ID is never another cable's (the design, section 4), so no lingering endpoint is credited to
/// another cable. On a thread of its own, as [`list`]; any user may call it, as [`capture_endpoints`].
pub fn cable_endpoints() -> Result<Vec<CableEndpoint>, WinError> {
    on_mta_thread(cable_endpoints_on_this_thread)
}

fn cable_endpoints_on_this_thread() -> Result<Vec<CableEndpoint>, WinError> {
    let states = DEVICE_STATE(DEVICE_STATE_ACTIVE.0 | DEVICE_STATE_DISABLED.0 | DEVICE_STATE_UNPLUGGED.0);
    let mut out = Vec::new();
    for (flow, render) in [(eRender, true), (eCapture, false)] {
        for ours in our_endpoints(flow, states)? {
            // One that went away meanwhile isn't listed, as one whose name couldn't be read isn't
            let Ok(Some(endpoint)) = ours.cable_endpoint(render) else { continue };
            out.push(endpoint);
        }
    }
    Ok(out)
}

/// The endpoint with MMDevice id `endpoint_id` as [`cable_endpoints`] would list it, if it's ours ([`is_ours`]): `Ok(None)` for
/// another vendor's endpoint, or one of ours that is NOTPRESENT. For a caller that has an endpoint already (the engine's
/// direct-feed guard, `nf-engine`'s `audio.rs`): one endpoint's property store and device topology are read, not every
/// endpoint's. COM must be initialised on this thread, in either apartment (the engine's audio thread is in the MTA).
pub fn cable_of_endpoint(endpoint_id: &str) -> Result<Option<CableEndpoint>, WinError> {
    // SAFETY: COM is initialised on this thread (the caller's); a plain in-process creation.
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(WinError::of("CoCreateInstance(MMDeviceEnumerator)"))?;
    let id = HSTRING::from(endpoint_id);
    // SAFETY: a live enumerator; `id` is NUL-terminated and outlives the call.
    let device = unsafe { enumerator.GetDevice(&id) }.map_err(WinError::of("IMMDeviceEnumerator::GetDevice"))?;
    let endpoint: IMMEndpoint = device.cast().map_err(WinError::of("IMMDevice as IMMEndpoint"))?;
    // SAFETY: a live endpoint.
    let render = unsafe { endpoint.GetDataFlow() }.map_err(WinError::of("IMMEndpoint::GetDataFlow"))? == eRender;
    let read = ReadEndpoint::of(device)?;
    if read.is_ours() { read.cable_endpoint(render) } else { Ok(None) }
}

/// An endpoint's name and its filter's paths, as [`our_endpoints`] and [`cable_of_endpoint`] read them.
struct ReadEndpoint {
    device: IMMDevice,
    name: String,
    /// The copies of the KS filter's path that were read: the one its device topology is connected to (the documented route),
    /// then the one its property store keeps ([`PKEY_ENDPOINT_FILTER`]), each where there is one.
    filters: Vec<String>,
}

impl ReadEndpoint {
    /// Reads its name (which must be there) and its filter's paths (where they are).
    fn of(device: IMMDevice) -> Result<Self, WinError> {
        // SAFETY: a live device; read access only.
        let store = unsafe { device.OpenPropertyStore(STGM_READ) }.map_err(WinError::of("IMMDevice::OpenPropertyStore"))?;
        let name = string_property(&store, &PKEY_Device_FriendlyName)?;
        let cached = string_property(&store, &PKEY_ENDPOINT_FILTER).ok().filter(|p| !p.is_empty());
        let filters: Vec<String> = connected_filter(&device).into_iter().chain(cached).collect();
        Ok(Self { device, name, filters })
    }

    /// Its filter's paths, as [`identify`] and [`is_ours`] take them.
    fn filters(&self) -> Vec<&str> {
        self.filters.iter().map(String::as_str).collect()
    }

    /// Whether it's one of our cables' ([`is_ours`]).
    fn is_ours(&self) -> bool {
        is_ours(&self.filters(), &self.name)
    }

    /// One of ours as [`cable_endpoints`] lists it (`render`: its flow): `Ok(None)` if it's NOTPRESENT.
    fn cable_endpoint(self, render: bool) -> Result<Option<CableEndpoint>, WinError> {
        // SAFETY: a live device.
        let state = match unsafe { self.device.GetState() }.map_err(WinError::of("IMMDevice::GetState"))? {
            DEVICE_STATE_ACTIVE => EndpointState::Active,
            DEVICE_STATE_DISABLED => EndpointState::Disabled,
            DEVICE_STATE_UNPLUGGED => EndpointState::Unplugged,
            _ => return Ok(None),
        };
        let id = device_id(&self.device)?;
        let (kind, instance) = identify_endpoint(&self.filters(), &self.name, render);
        let cable = instance.as_deref().and_then(CableInstance::parse);
        Ok(Some(CableEndpoint { id, name: self.name, render, kind, state, instance, cable }))
    }
}

/// Our endpoints of `flow` in `states`, with their names and their filters' paths: every endpoint's filter is read, since an
/// endpoint is ours by its filter's instance ID ([`is_ours`]). COM must be initialised on this thread.
fn our_endpoints(flow: EDataFlow, states: DEVICE_STATE) -> Result<Vec<ReadEndpoint>, WinError> {
    // SAFETY: COM is initialised on this thread (the caller's); a plain in-process creation.
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(WinError::of("CoCreateInstance(MMDeviceEnumerator)"))?;
    // SAFETY: a live enumerator; constant arguments.
    let endpoints =
        unsafe { enumerator.EnumAudioEndpoints(flow, states) }.map_err(WinError::of("IMMDeviceEnumerator::EnumAudioEndpoints"))?;
    // SAFETY: a live collection.
    let count = unsafe { endpoints.GetCount() }.map_err(WinError::of("IMMDeviceCollection::GetCount"))?;
    let mut out = Vec::new();
    for i in 0..count {
        // An endpoint that went away meanwhile, or whose name can't be read, isn't ours to list; the others still are
        // SAFETY: a live collection and an index below its count.
        let Ok(device) = (unsafe { endpoints.Item(i) }) else { continue };
        let Ok(read) = ReadEndpoint::of(device) else { continue };
        if read.is_ours() {
            out.push(read);
        }
    }
    Ok(out)
}

/// The device interface path of the KS filter an endpoint is connected to: its device topology's one connector, and the
/// device on the other side (`IConnector::GetDeviceIdConnectedTo`). `None` if any step fails (a disabled endpoint's may:
/// to be confirmed live), or the path is empty.
pub(crate) fn connected_filter(device: &IMMDevice) -> Option<String> {
    // SAFETY: a live device; no activation parameters.
    let topology: IDeviceTopology = unsafe { device.Activate(CLSCTX_ALL, None) }.ok()?;
    // SAFETY: a live topology; an endpoint's has a single connector, index 0.
    let connector = unsafe { topology.GetConnector(0) }.ok()?;
    // SAFETY: a live connector; the string is COM's, freed once by take_string.
    let id = take_string(unsafe { connector.GetDeviceIdConnectedTo() }.ok()?);
    (!id.is_empty()).then_some(id)
}

/// A string COM handed us, freed with CoTaskMemFree.
fn take_string(s: PWSTR) -> String {
    // SAFETY: a NUL-terminated string from COM (or null, which gives an empty string), freed once here.
    unsafe {
        let text = if s.is_null() { String::new() } else { s.to_string().unwrap_or_default() };
        CoTaskMemFree(Some(s.0.cast()));
        text
    }
}

pub(crate) fn device_id(device: &IMMDevice) -> Result<String, WinError> {
    // SAFETY: a live device.
    let id = unsafe { device.GetId() }.map_err(WinError::of("IMMDevice::GetId"))?;
    Ok(take_string(id))
}

/// A string property of an endpoint's (an empty string for an empty value).
pub(crate) fn string_property(store: &IPropertyStore, key: &PROPERTYKEY) -> Result<String, WinError> {
    // SAFETY: a live store; the key outlives the call.
    let mut value = unsafe { store.GetValue(key) }.map_err(WinError::of("IPropertyStore::GetValue"))?;
    // SAFETY: `value` is the PROPVARIANT the store returned, converted, then cleared once.
    let text = unsafe { PropVariantToStringAlloc(&value) };
    // SAFETY: as above.
    let _ = unsafe { PropVariantClear(&mut value) };
    Ok(take_string(text.map_err(WinError::of("PropVariantToStringAlloc"))?))
}

fn sessions_of(device: &IMMDevice) -> Result<Vec<Session>, WinError> {
    // SAFETY: a live device; no activation parameters.
    let manager: IAudioSessionManager2 =
        unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(WinError::of("IMMDevice::Activate(IAudioSessionManager2)"))?;
    // SAFETY: a live session manager.
    let sessions = unsafe { manager.GetSessionEnumerator() }.map_err(WinError::of("IAudioSessionManager2::GetSessionEnumerator"))?;
    // SAFETY: a live enumerator.
    let count = unsafe { sessions.GetCount() }.map_err(WinError::of("IAudioSessionEnumerator::GetCount"))?;
    let mut out = Vec::new();
    for i in 0..count {
        // A session that went away meanwhile is skipped
        // SAFETY: a live enumerator and an index below its count.
        let Ok(control) = (unsafe { sessions.GetSession(i) }) else { continue };
        let Ok(control) = control.cast::<IAudioSessionControl2>() else { continue };
        // SAFETY: a live session control.
        let Ok(state) = (unsafe { control.GetState() }) else { continue };
        let state = match state {
            s if s == AudioSessionStateActive => SessionState::Active,
            s if s == AudioSessionStateInactive => SessionState::Inactive,
            s if s == AudioSessionStateExpired => SessionState::Expired,
            _ => continue,
        };
        // SAFETY: a live session control. A session across processes gives AUDCLNT_S_NO_SINGLE_PROCESS, a success code.
        let pid = unsafe { control.GetProcessId() }.unwrap_or(0);
        // SAFETY: a live session control; it returns S_OK for the system-sounds session, S_FALSE for others.
        let system_sounds = unsafe { control.IsSystemSoundsSession() } == S_OK;
        let image = if pid == 0 { Err(WinError { call: "IAudioSessionControl2::GetProcessId", code: 0 }) } else { image_of(pid) };
        out.push(Session { pid, image, state, system_sounds });
    }
    Ok(out)
}

/// A process's exe path.
fn image_of(pid: u32) -> Result<String, WinError> {
    // SAFETY: plain arguments; the handle returned is ours.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.map_err(WinError::of("OpenProcess"))?;
    // SAFETY: a valid handle that nothing else owns; Owned closes it once.
    let process = unsafe { Owned::new(process) };
    let mut buf = vec![0u16; 32_768];
    let mut len = buf.len() as u32;
    // SAFETY: our process handle; the buffer holds `len` characters and outlives the call; `len` is a live in-out pointer.
    unsafe { QueryFullProcessImageNameW(*process, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len) }
        .map_err(WinError::of("QueryFullProcessImageNameW"))?;
    Ok(String::from_utf16_lossy(&buf[..(len as usize).min(buf.len())]))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Nothing here opens an audio endpoint or a process: the classification and the endpoint names only.

    #[test]
    fn known_apps_are_classified_by_their_exe() {
        #[rustfmt::skip]
        let cases = [
            (r"C:\Users\u\AppData\Local\Discord\app-1.0.9200\Discord.exe", AppKind::Call),
            (r"C:\Program Files\WindowsApps\MSTeams_25.1.0_x64__8wekyb3d8bbwe\ms-teams.exe", AppKind::Call),
            ("Teams.exe", AppKind::Call),
            ("Zoom.exe", AppKind::Call),
            ("slack.exe", AppKind::Call),
            ("Skype.exe", AppKind::Call),
            ("webex.exe", AppKind::Call),
            (r"C:\Program Files\Cisco Spark\WebexHost.exe", AppKind::Call),
            (r"C:\Program Files\Google\Chrome\Application\chrome.exe", AppKind::WebCall),
            ("msedge.exe", AppKind::WebCall),
            ("C:/Program Files/Mozilla Firefox/firefox.exe", AppKind::WebCall),
            (r"C:\Program Files\REAPER (x64)\reaper.exe", AppKind::LowLatency),
            (r"C:\ProgramData\Ableton\Live 12 Suite\Program\Ableton Live 12 Suite.exe", AppKind::LowLatency),
            ("Bitwig Studio.exe", AppKind::LowLatency),
            ("BitwigAudioEngine-X64.exe", AppKind::LowLatency),
            ("FL64.exe", AppKind::LowLatency),
            ("Cubase14.exe", AppKind::LowLatency),
            (r"C:\Program Files\PreSonus\Studio One 6\Studio One.exe", AppKind::LowLatency),
            // Unknown: not in the table, a name that only starts like one, or not an exe
            ("notepad.exe", AppKind::Unknown),
            ("obs64.exe", AppKind::Unknown),
            ("discord.exe.bak", AppKind::Unknown),
            ("mydiscord.exe", AppKind::Unknown),
            ("webex", AppKind::Unknown),
            ("", AppKind::Unknown),
        ];
        for (image, kind) in cases {
            assert_eq!(classify(image), kind, "{image}");
        }
        assert_eq!(file_name(r"C:\a\b.exe"), "b.exe");
        assert_eq!(file_name("b.exe"), "b.exe");
        for (name, _) in KNOWN_APPS {
            let lower = match name {
                ExeName::Exact(n) | ExeName::Prefix(n) => n,
            };
            assert_eq!(*lower, lower.to_lowercase(), "the table is lower case: {lower}");
        }
    }

    #[test]
    fn our_endpoints_are_told_apart_by_name_when_their_filter_cant_be_read() {
        assert!(has_our_name("Microphone (ACX-LL-Cable A)") && has_our_name("Call Microphone (ACX-LL-Cable B)"));
        assert!(has_our_name("Speakers (ACX-LL-Cable D)") && has_our_name("ACX-LL-Cable Microphone"), "an interface's name too");
        assert!(has_our_name("Studio mic (acx-ll-cable a)"), "any case");
        assert!(!has_our_name("CABLE Output (VB-Audio Virtual Cable)") && !has_our_name("Microphone (USB Audio Device)"));
        assert!(!has_our_name("Microphone (AudioCodec Device)"), "the old driver's name isn't ours any more");
        // No filter read: the name decides
        assert!(is_ours(&[], "Microphone (ACX-LL-Cable A)") && !is_ours(&[], "Microphone (AudioCodec Device)"));
        assert!(is_ours(&["garbage"], "Studio mic (ACX-LL-Cable A)"), "a copy that doesn't parse is no copy");
        assert_eq!(mic_of("Microphone (ACX-LL-Cable A)"), Mic::LowLatency);
        assert_eq!(mic_of("Call Microphone (ACX-LL-Cable A)"), Mic::Call);
        // Windows numbers a second device's names in the brackets
        assert!(has_our_name("Microphone (2- ACX-LL-Cable A)"));
        assert_eq!(mic_of("Microphone (2- ACX-LL-Cable A)"), Mic::LowLatency);
        assert_eq!(mic_of("Call Microphone (2- ACX-LL-Cable A)"), Mic::Call);
        assert_eq!(mic_of("2- Microphone (ACX-LL-Cable A)"), Mic::Other);
        assert_eq!(mic_of("Studio mic (ACX-LL-Cable A)"), Mic::Other);
    }

    /// The review of N0: an endpoint whose filter wasn't read says its cable only by the label in its name's brackets.
    #[test]
    fn the_label_is_read_from_the_names_brackets() {
        #[rustfmt::skip]
        let cases = [
            ("Microphone (ACX-LL-Cable A)", Some("A")),
            ("Speakers (ACX-LL-Cable B)", Some("B")),
            ("Studio mic (ACX-LL-Cable D)", Some("D")),
            ("Microphone (2- ACX-LL-Cable A)", Some("A")),
            ("Call Microphone (12- acx-ll-cable c7) ", Some("C7")),
            ("Mic (old) (ACX-LL-Cable CABLE12Z)", Some("CABLE12Z")),
            // Not a label: none, too long, not A-Z and 0-9, not ours, not in the last brackets, no brackets
            ("Microphone (ACX-LL-Cable)", None),
            ("Microphone (ACX-LL-Cable )", None),
            ("Microphone (ACX-LL-Cable ABCDEFGHI)", None),
            ("Microphone (ACX-LL-Cable A-B)", None),
            ("Microphone (ACX-LL-Cable A B)", None),
            ("Microphone (ACX-LL-Cable \u{c4})", None),
            ("Microphone (ACX-LL-CableA)", None),
            ("Microphone (x- ACX-LL-Cable A)", None),
            ("Microphone (ACX-LL-Cable A) (USB Audio Device)", None),
            ("ACX-LL-Cable Microphone", None),
            ("Microphone (AudioCodec Device)", None),
            ("Microphone (\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9})", None),
            ("", None),
            (")", None),
        ];
        for (name, label) in cases {
            assert_eq!(label_of_name(name).as_ref().map(Label::as_str), label, "{name:?}");
        }
    }

    const ID: &str = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0";
    const OTHER_ID: &str = "11111111-2222-3333-4444-555555555555";

    /// A KS filter path on cable `label`, as DeviceTopology gives it (lower case, `#` for `\`).
    fn on_cable(label: &str, id: &str, n: u32, reference: &str) -> String {
        format!(r"\\?\root#media#acxllcable_{}_{id}_{n}#{{6994ad04-93ef-11d0-a3cc-00a0c9223196}}\{reference}", label.to_lowercase())
    }

    /// N0 (the per-cable design, section 2): an endpoint is ours by the instance ID its filter names, whatever its name.
    #[test]
    fn our_endpoints_are_known_by_their_filters_instance_id() {
        let a_mic = on_cable("A", ID, 1, "microphone0");
        let id: CableId = ID.parse().unwrap();
        // Ours, whatever the name: renamed, or numbered
        for name in ["Microphone (ACX-LL-Cable A)", "Studio mic (ACX-LL-Cable A)", "Anything at all"] {
            assert!(is_ours(&[&a_mic], name), "{name}");
        }
        // The property store's copy counts the same, prefix and all
        assert!(is_ours(&[&format!("{{2}}.{a_mic}")], "x"));
        // A filter on a device that isn't a cable's isn't ours, even named like one: the old driver's ROOT\MEDIA\NNNN, or
        // another vendor's endpoint renamed
        let old = r"{2}.\\?\root#media#0004#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\microphone0";
        let vb = r"\\?\root#media#0001#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\wave";
        assert!(!is_ours(&[old], "Microphone (AudioCodec Device)"));
        assert!(!is_ours(&[vb], "Microphone (ACX-LL-Cable A)"));
        // The documented route's copy comes first: it decides over a stale cached one
        assert!(!is_ours(&[vb, &a_mic], "Microphone (ACX-LL-Cable A)"));
        assert!(is_ours(&["garbage", &a_mic], "CABLE Output (VB-Audio Virtual Cable)"));
        // The instance ID it names is the cable's
        let (mic, instance) = identify(&[&a_mic], "Studio mic (ACX-LL-Cable A)");
        assert_eq!(mic, Mic::LowLatency);
        let cable = CableInstance::parse(&instance.unwrap()).unwrap();
        assert_eq!((cable.label.as_str(), cable.id, cable.n), ("A", id, 1));
    }

    #[test]
    fn a_render_endpoint_is_known_by_its_speakers_filter() {
        let speakers = on_cable("B", ID, 3, "speaker0");
        let instance = Some(format!(r"root\media\acxllcable_b_{ID}_3"));
        assert_eq!(identify_endpoint(&[&speakers], "Renamed", true), (Some(EndpointKind::Speakers), instance.clone()));
        // A capture filter path doesn't name a render endpoint, nor the other way round: the name decides, on that device
        let mic = on_cable("B", ID, 3, "microphone1");
        assert_eq!(identify_endpoint(&[&mic], "Speakers (ACX-LL-Cable B)", true), (Some(EndpointKind::Speakers), instance.clone()));
        assert_eq!(identify_endpoint(&[&mic], "Studio", true), (None, instance.clone()));
        assert_eq!(identify_endpoint(&[&speakers], "Studio", false), (None, instance.clone()));
        assert_eq!(identify_endpoint(&[&mic], "Studio", false), (Some(EndpointKind::CallMicrophone), instance));
        // No filter: the name, and no device
        assert_eq!(identify_endpoint(&[], "Speakers (2- ACX-LL-Cable B)", true), (Some(EndpointKind::Speakers), None));
        assert_eq!(identify_endpoint(&[], "Headphones (ACX-LL-Cable B)", true), (None, None));
        assert_eq!(identify_endpoint(&[], "Microphone (ACX-LL-Cable B)", false), (Some(EndpointKind::Microphone), None));
    }

    fn endpoint(id: &str, label: &str, cable_id: &str, n: u32, kind: EndpointKind, state: EndpointState) -> CableEndpoint {
        let instance = format!(r"root\media\acxllcable_{}_{cable_id}_{n}", label.to_lowercase());
        CableEndpoint {
            id: id.into(),
            name: format!("{} (ACX-LL-Cable {label})", kind.name()),
            render: kind.is_render(),
            kind: Some(kind),
            state,
            cable: CableInstance::parse(&instance),
            instance: Some(instance),
        }
    }

    #[test]
    fn a_cables_endpoint_is_picked_by_its_identity_not_its_name() {
        use EndpointKind::{CallMicrophone, Microphone, Speakers};
        use EndpointState::{Active, Disabled};
        let (a, b): (CableId, CableId) = (ID.parse().unwrap(), OTHER_ID.parse().unwrap());
        let mut list = vec![
            endpoint("{a-spk}", "A", ID, 1, Speakers, Active),
            endpoint("{a-mic}", "A", ID, 1, Microphone, Active),
            endpoint("{a-call}", "A", ID, 1, CallMicrophone, Disabled),
            endpoint("{b-mic}", "B", OTHER_ID, 1, Microphone, Active),
            // Ours by name alone: never any cable's
            CableEndpoint {
                id: "{unknown}".into(),
                name: "Microphone (ACX-LL-Cable A)".into(),
                render: false,
                kind: Some(Microphone),
                state: Active,
                instance: None,
                cable: None,
            },
        ];
        // B's microphone is named like A's: still B's
        list[3].name = "Microphone (ACX-LL-Cable A)".into();
        let id_of = |p: Pick| match p {
            Pick::Found(e) => e.id.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(id_of(pick(&list, &a, Speakers)), "{a-spk}");
        assert_eq!(id_of(pick(&list, &a, Microphone)), "{a-mic}");
        assert_eq!(id_of(pick(&list, &b, Microphone)), "{b-mic}");
        // Disabled isn't picked: the cable's endpoints are named instead
        match pick(&list, &a, CallMicrophone) {
            Pick::NotActive(on_a) => assert_eq!(on_a.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["{a-spk}", "{a-mic}", "{a-call}"]),
            other => panic!("{other:?}"),
        }
        assert_eq!(pick(&list, &b, Speakers), Pick::NotActive(vec![&list[3]]));
        let none: CableId = "99999999-9999-9999-9999-999999999999".parse().unwrap();
        assert_eq!(pick(&list, &none, Microphone), Pick::NotActive(vec![]));
        // Two nodes of one cable at once (n and n + 1, a variant switch midway): which one is ambiguous
        list.push(endpoint("{a2-mic}", "A", ID, 2, Microphone, Active));
        assert_eq!(pick(&list, &a, Microphone), Pick::Several(vec![&list[1], &list[5]]));
        // An endpoint chosen by name finds its cable by its id
        assert_eq!(endpoint_by_id(&list, "{B-MIC}").and_then(|e| e.cable.as_ref()).map(|c| c.id), Some(b));
        assert_eq!(endpoint_by_id(&list, "{unknown}").map(|e| e.cable.is_none()), Some(true));
        assert_eq!(endpoint_by_id(&list, "{vb-cable}"), None);
        assert_eq!(list[1].describe(), "\"Microphone (ACX-LL-Cable A)\": cable A's Microphone (Microphone0), active");
        assert!(list[4].describe().contains("its device unknown"), "{}", list[4].describe());
    }

    /// Astra's review: the switch matched endpoints by their exact names, so a numbered or renamed one never counted. The KS
    /// filter an endpoint is connected to says which microphone it is, and on which device.
    #[test]
    fn an_endpoint_is_known_by_its_filter_whatever_its_name() {
        // As the endpoint's property store has it (build 26100), and as GetDeviceIdConnectedTo may give it, without the prefix
        let low = r"{2}.\\?\root#media#0004#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\microphone0";
        assert_eq!(parse_filter(low), Some((r"root\media\0004".into(), "microphone0".into())));
        let call = r"\\?\ROOT#MEDIA#0012#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\Microphone1";
        assert_eq!(parse_filter(call), Some((r"ROOT\MEDIA\0012".into(), "Microphone1".into())));
        assert_eq!(
            parse_filter(r" \\?\root#media#0004#{6994ad04-93ef-11d0-a3cc-00a0c9223196} "),
            Some((r"root\media\0004".into(), String::new())),
            "no reference string"
        );
        for bad in ["", r"ROOT\MEDIA\0004", r"{1}.ROOT\MEDIA\0004", "{2}.", r"{2\\?\root#media#0004#{x}\a", r"\\?\#{x}\a", r"\\?\root\a"] {
            assert_eq!(parse_filter(bad), None, "{bad:?}");
        }
        assert_eq!(mic_of_filter("microphone0"), Mic::LowLatency);
        assert_eq!(mic_of_filter("MICROPHONE1"), Mic::Call);
        assert_eq!(mic_of_filter("speaker0"), Mic::Other);
        assert_eq!(mic_of_filter(""), Mic::Other);
        // The filter decides, whatever the name: renamed, numbered, or another microphone's
        assert_eq!(identify(&[low], "Studio mic (AudioCodec Device)"), (Mic::LowLatency, Some(r"root\media\0004".into())));
        assert_eq!(identify(&[call], crate::variant::MICROPHONE), (Mic::Call, Some(r"ROOT\MEDIA\0012".into())));
        // Without one that parses, the name, and no device
        assert_eq!(identify(&[], "Call Microphone (2- AudioCodec Device)"), (Mic::Call, None));
        assert_eq!(identify(&["garbage"], crate::variant::MICROPHONE), (Mic::LowLatency, None));
        assert_eq!(identify(&[], "Studio mic (AudioCodec Device)"), (Mic::Other, None));
    }

    /// Astra's review of a3421c6: a path that parsed but named neither microphone (a suffix after the reference string, or
    /// another form) made the endpoint Mic::Other, and the property store's copy was never looked at.
    #[test]
    fn each_copy_of_the_filter_is_tried_until_one_names_a_microphone() {
        // A suffix after the reference string (as the endpoint's registry copy has it on some builds) still names it
        assert_eq!(mic_of_filter("microphone0/00010001"), Mic::LowLatency);
        assert_eq!(mic_of_filter("Microphone1|x"), Mic::Call);
        assert_eq!(mic_of_filter(r"microphone1\x"), Mic::Call);
        // ... but a longer name isn't ours
        assert_eq!(mic_of_filter("microphone01"), Mic::Other);
        assert_eq!(mic_of_filter("microphone"), Mic::Other);
        let suffixed = r"{2}.\\?\root#media#0004#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\microphone1/00010001";
        assert_eq!(identify(&[suffixed], crate::variant::MICROPHONE), (Mic::Call, Some(r"root\media\0004".into())));
        // The topology's path parses but names neither (no reference string, or one we don't know): the property store's
        // copy decides
        let bare = r"\\?\root#media#0004#{6994ad04-93ef-11d0-a3cc-00a0c9223196}";
        let unknown = r"\\?\root#media#0004#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\pin3";
        let cached = r"{2}.\\?\root#media#0004#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\microphone1";
        for topology in [bare, unknown, "garbage"] {
            let found = identify(&[topology, cached], "Studio mic (AudioCodec Device)");
            assert_eq!(found, (Mic::Call, Some(r"root\media\0004".into())), "{topology}");
        }
        // The first that names one wins
        let low = r"\\?\root#media#0004#{6994ad04-93ef-11d0-a3cc-00a0c9223196}\Microphone0";
        assert_eq!(identify(&[low, cached], "x (AudioCodec Device)").0, Mic::LowLatency);
        // None names one: the name decides, on the device the first parsed path names
        assert_eq!(identify(&["garbage", bare, unknown], crate::variant::CALL_MICROPHONE), (Mic::Call, Some(r"root\media\0004".into())));
        assert_eq!(identify(&[unknown], "Studio mic (AudioCodec Device)"), (Mic::Other, Some(r"root\media\0004".into())));
    }

    #[test]
    fn the_fit_names_the_better_microphone() {
        assert_eq!(fit(AppKind::Call, Mic::Call), Fit::Suits);
        assert_eq!(fit(AppKind::Call, Mic::LowLatency), Fit::Better(Mic::Call));
        assert_eq!(fit(AppKind::WebCall, Mic::LowLatency), Fit::Better(Mic::Call));
        assert_eq!(fit(AppKind::WebCall, Mic::Call), Fit::Suits);
        assert_eq!(fit(AppKind::LowLatency, Mic::Call), Fit::Better(Mic::LowLatency));
        assert_eq!(fit(AppKind::LowLatency, Mic::LowLatency), Fit::Suits);
        assert_eq!(fit(AppKind::Unknown, Mic::Call), Fit::Unknown);
        assert_eq!(fit(AppKind::Call, Mic::Other), Fit::Unknown);
        let session = |image: Result<String, WinError>| Session { pid: 7, image, state: SessionState::Active, system_sounds: false };
        let discord = session(Ok(r"C:\x\Discord.exe".into()));
        assert_eq!((discord.exe(), discord.kind()), (Some("Discord.exe"), AppKind::Call));
        let gone = session(Err(WinError { call: "OpenProcess", code: 87 }));
        assert_eq!((gone.exe(), gone.kind()), (None, AppKind::Unknown));
    }
}
