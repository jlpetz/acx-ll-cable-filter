//! Which apps record from our microphones, and which of the two suits each (TODO 19, the owner's option A: two microphones,
//! and the app says which apps use which).
//!
//! [`list`] reads Windows' audio sessions: each active capture endpoint whose name has "AudioCodec Device" in it
//! (`IMMDeviceEnumerator::EnumAudioEndpoints`), its `IAudioSessionManager2`'s sessions, and for each session its process
//! (`IAudioSessionControl2::GetProcessId`, then `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` and
//! `QueryFullProcessImageNameW`) and its state. It returns data, for the CLI's `mic sessions` and later the window. A session
//! exists for each app that opened the endpoint in shared mode; an app recording it in exclusive mode has none, so it isn't
//! listed.
//!
//! [`classify`] (pure) sorts an app by its exe's name with [`KNOWN_APPS`]; [`fit`] says whether the microphone it records
//! from suits it.
//!
//! [`capture_endpoints`] reads our capture endpoints alone, active or disabled, without their sessions: what `mic variant`
//! shows, and what its switch waits for (`variant::check_mics`).

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::S_OK;
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, AudioSessionStateExpired, AudioSessionStateInactive, DEVICE_STATE, DEVICE_STATE_ACTIVE, DEVICE_STATE_DISABLED,
    IAudioSessionControl2, IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, STGM_READ,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
use windows::core::{Interface, Owned, PWSTR};

use crate::variant::DEVICE_NAME;
use crate::win_error::WinError;

/// Which of our microphones an endpoint is, by its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mic {
    /// "Microphone (AudioCodec Device)": IAudioClient3's small periods (2.67-10 ms), for apps that ask for them.
    LowLatency,
    /// "Call Microphone (AudioCodec Device)": a 5 ms default period, for apps that ask for none (call apps).
    Call,
    /// An AudioCodec endpoint by another name (renamed in the Sound panel, say).
    Other,
}

impl Mic {
    /// "the Call Microphone", "the Microphone".
    pub fn describe(self) -> &'static str {
        match self {
            Mic::LowLatency => "the Microphone",
            Mic::Call => "the Call Microphone",
            Mic::Other => "another AudioCodec microphone",
        }
    }
}

/// An endpoint's [`Mic`], from its friendly name.
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

/// Whether an endpoint is one of the driver's (its name has "AudioCodec Device" in it, any case).
pub fn is_ours(endpoint_name: &str) -> bool {
    endpoint_name.to_ascii_lowercase().contains(&DEVICE_NAME.to_ascii_lowercase())
}

/// One of our capture endpoints and the sessions on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// The endpoint's id (`{0.0.1.00000000}.{...}`), as `noise-filter devices` shows it.
    pub id: String,
    /// "Call Microphone (AudioCodec Device)".
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
    /// "Call Microphone (AudioCodec Device)".
    pub name: String,
    /// Active; otherwise disabled (in the Sound panel's Recording tab).
    pub active: bool,
}

/// Our capture endpoints, active or disabled (an absent device's aren't listed). On a thread of its own, as [`list`].
pub fn capture_endpoints() -> Result<Vec<CaptureEndpoint>, WinError> {
    on_mta_thread(capture_endpoints_on_this_thread)
}

/// Runs `work` on a thread of its own, in the MTA.
fn on_mta_thread<T: Send + 'static>(work: fn() -> Result<T, WinError>) -> Result<T, WinError> {
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
    for (device, name) in our_capture_endpoints(DEVICE_STATE_ACTIVE)? {
        out.push(Endpoint { id: device_id(&device)?, mic: mic_of(&name), sessions: sessions_of(&device)?, name });
    }
    Ok(out)
}

fn capture_endpoints_on_this_thread() -> Result<Vec<CaptureEndpoint>, WinError> {
    let mut out = Vec::new();
    for (device, name) in our_capture_endpoints(DEVICE_STATE(DEVICE_STATE_ACTIVE.0 | DEVICE_STATE_DISABLED.0))? {
        // One that went away meanwhile isn't listed
        // SAFETY: a live device.
        let Ok(state) = (unsafe { device.GetState() }) else { continue };
        out.push(CaptureEndpoint { id: device_id(&device)?, name, active: state == DEVICE_STATE_ACTIVE });
    }
    Ok(out)
}

/// Our capture endpoints in `states`, with their names. COM must be initialised on this thread.
fn our_capture_endpoints(states: DEVICE_STATE) -> Result<Vec<(IMMDevice, String)>, WinError> {
    // SAFETY: COM is initialised on this thread (the caller's); a plain in-process creation.
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(WinError::of("CoCreateInstance(MMDeviceEnumerator)"))?;
    // SAFETY: a live enumerator; constant arguments.
    let endpoints =
        unsafe { enumerator.EnumAudioEndpoints(eCapture, states) }.map_err(WinError::of("IMMDeviceEnumerator::EnumAudioEndpoints"))?;
    // SAFETY: a live collection.
    let count = unsafe { endpoints.GetCount() }.map_err(WinError::of("IMMDeviceCollection::GetCount"))?;
    let mut out = Vec::new();
    for i in 0..count {
        // SAFETY: a live collection and an index below its count.
        let device = unsafe { endpoints.Item(i) }.map_err(WinError::of("IMMDeviceCollection::Item"))?;
        // An endpoint that went away meanwhile, or whose name can't be read, isn't ours to list
        let Ok(name) = friendly_name(&device) else { continue };
        if is_ours(&name) {
            out.push((device, name));
        }
    }
    Ok(out)
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

fn device_id(device: &IMMDevice) -> Result<String, WinError> {
    // SAFETY: a live device.
    let id = unsafe { device.GetId() }.map_err(WinError::of("IMMDevice::GetId"))?;
    Ok(take_string(id))
}

fn friendly_name(device: &IMMDevice) -> Result<String, WinError> {
    // SAFETY: a live device; read access only.
    let store = unsafe { device.OpenPropertyStore(STGM_READ) }.map_err(WinError::of("IMMDevice::OpenPropertyStore"))?;
    // SAFETY: a live store; the key is a constant.
    let mut value = unsafe { store.GetValue(&PKEY_Device_FriendlyName) }.map_err(WinError::of("IPropertyStore::GetValue"))?;
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
    fn our_endpoints_are_told_apart_by_name() {
        assert!(is_ours("Microphone (AudioCodec Device)") && is_ours("Call Microphone (AudioCodec Device)"));
        assert!(is_ours("Studio mic (audiocodec device)"), "any case");
        assert!(!is_ours("CABLE Output (VB-Audio Virtual Cable)") && !is_ours("Microphone (USB Audio Device)"));
        assert_eq!(mic_of(crate::variant::MICROPHONE), Mic::LowLatency);
        assert_eq!(mic_of(crate::variant::CALL_MICROPHONE), Mic::Call);
        assert_eq!(mic_of("2- Microphone (AudioCodec Device)"), Mic::Other);
        assert_eq!(mic_of("Studio mic (AudioCodec Device)"), Mic::Other);
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
