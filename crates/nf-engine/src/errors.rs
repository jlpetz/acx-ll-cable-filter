//! Engine failures as plain data, and their wording for the user.
//!
//! The audio thread never formats anything: it keeps a raw HRESULT (or a fault code) and the
//! control thread turns it into a sentence here. Everything in this module is pure.

/// HRESULTs we word specially (values from `windows::Win32::Media::Audio` / `Foundation`).
use crate::api::Remedy;
use crate::exclusive::ExclusiveRefusal;

pub(crate) mod hr {
    pub(crate) const E_FAIL: i32 = 0x8000_4005_u32 as i32;
    pub(crate) const E_ACCESSDENIED: i32 = 0x8007_0005_u32 as i32;
    pub(crate) const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;
    /// HRESULT_FROM_WIN32(ERROR_NOT_FOUND): no such device, or no default device.
    pub(crate) const E_NOTFOUND: i32 = 0x8007_0490_u32 as i32;
    pub(crate) const AUDCLNT_E_NOT_INITIALIZED: i32 = 0x8889_0001_u32 as i32;
    pub(crate) const AUDCLNT_E_DEVICE_INVALIDATED: i32 = 0x8889_0004_u32 as i32;
    pub(crate) const AUDCLNT_E_UNSUPPORTED_FORMAT: i32 = 0x8889_0008_u32 as i32;
    pub(crate) const AUDCLNT_E_DEVICE_IN_USE: i32 = 0x8889_000A_u32 as i32;
    pub(crate) const AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED: i32 = 0x8889_000E_u32 as i32;
    /// The endpoint was unplugged, reconfigured or made unavailable while being opened (a Bluetooth
    /// or USB device mid-reconnect).
    pub(crate) const AUDCLNT_E_ENDPOINT_CREATE_FAILED: i32 = 0x8889_000F_u32 as i32;
    pub(crate) const AUDCLNT_E_SERVICE_NOT_RUNNING: i32 = 0x8889_0010_u32 as i32;
    pub(crate) const AUDCLNT_E_BUFFER_SIZE_ERROR: i32 = 0x8889_0016_u32 as i32;
    /// Exclusive mode: the buffer the period makes isn't aligned as the driver needs; `GetBufferSize` says the next that is.
    pub(crate) const AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED: i32 = 0x8889_0019_u32 as i32;
    pub(crate) const AUDCLNT_E_INVALID_DEVICE_PERIOD: i32 = 0x8889_0020_u32 as i32;
    pub(crate) const AUDCLNT_E_RESOURCES_INVALIDATED: i32 = 0x8889_0026_u32 as i32;
    /// COM is already initialised on this thread in another apartment: an STA (a UI thread) asked for the MTA.
    pub(crate) const RPC_E_CHANGED_MODE: i32 = 0x8001_0106_u32 as i32;
}

/// Which stream (or device) something concerns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StreamRole {
    Mic = 1,
    Output = 2,
    Reference = 3,
    KeepAlive = 4,
    Probe = 5,
    Capture = 6,
    /// The far end of `play_record`'s echo test: the speakers playing the far voice.
    FarEnd = 7,
}

impl StreamRole {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => StreamRole::Mic,
            2 => StreamRole::Output,
            3 => StreamRole::Reference,
            4 => StreamRole::KeepAlive,
            5 => StreamRole::Probe,
            7 => StreamRole::FarEnd,
            _ => StreamRole::Capture,
        }
    }

    /// The device as the user knows it, sentence-initial.
    pub(crate) fn noun(self) -> &'static str {
        match self {
            StreamRole::Mic => "The microphone",
            StreamRole::Output => "The output device",
            StreamRole::Reference | StreamRole::KeepAlive => "The speakers used as the echo reference",
            StreamRole::Probe => "The probe device",
            StreamRole::Capture => "The capture device",
            StreamRole::FarEnd => "The far-end device",
        }
    }
}

/// Why the audio thread couldn't start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StartFailure {
    /// COM couldn't be initialised on the audio thread.
    Com(i32),
    /// The device enumerator couldn't be created.
    Enumerator(i32),
    /// The endpoint doesn't exist, isn't active, or (for a default) there is none.
    NotFound(StreamRole),
    /// The id names an endpoint of the other direction (e.g. a mic picked as the output).
    WrongDirection(StreamRole),
    /// The output resolved to the same endpoint as the echo reference.
    OutputIsReference,
    /// Following the default mic, it resolved to a recording endpoint on the output's own device
    /// (this interface friendly name, e.g. "VB-Audio Virtual Cable"): the output cable's own
    /// recording side. The engine waits for another default, as for a missing mic.
    DefaultMicIsOutput(String),
    /// The selected microphone belongs to the cable that the filter feeds.
    MicIsOutput(String),
    /// Opening or starting a stream failed with this HRESULT.
    Open { role: StreamRole, hr: i32 },
    /// Opening the live driver attachment failed. A removed or busy device can become available again.
    DriverRing { message: String, retry: bool },
    /// A stage refused to start.
    Stage(String),
    /// The audio thread ended without reporting (it panicked).
    ThreadDied,
    /// The streams didn't open within the start timeout.
    Timeout,
    /// The audio thread of an earlier start that timed out is still stuck in the driver, so a new
    /// start would only queue behind it (wasapi/A).
    DriverBusy,
    /// The audio thread couldn't be created.
    Spawn,
}

/// A non-fatal start-up condition: a warning in the status, or a note ([`Warning::is_note`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Warning {
    /// MMCSS registration failed with this Win32 error; the thread runs at a raised normal priority.
    Mmcss(u32),
    /// The noise model's inference worker (a model pack's, TODO 6) couldn't register with MMCSS (this Win32 error); it runs
    /// at a raised normal priority under the audio thread's.
    InferenceMmcss(u32),
    /// The echo reference couldn't be opened; AEC runs on silence.
    ReferenceUnavailable(StartFailure),
    /// The keep-alive stream couldn't be opened; the reference has gaps during silence.
    KeepAliveUnavailable(i32),
    /// The mic and the output are on the same device (this interface friendly name). If the mic is
    /// the output cable's recording side, the filter only hears its own output.
    MicSharesOutputDevice(String),
    /// The output is the default playback device and doesn't look like a virtual cable (this
    /// endpoint name): the cleaned mic plays out of the speakers instead of reaching other apps.
    /// `exclusive_withheld`: it was asked for in exclusive mode and runs shared, as the default
    /// playback device always does (`audio::output_check`).
    OutputIsSpeakers { name: String, exclusive_withheld: bool },
    /// The output cable is also the default playback device (this endpoint name), so every app's
    /// sound goes into the filtered mic. `exclusive_withheld` as above.
    CableIsDefaultPlayback { name: String, exclusive_withheld: bool },
    /// The mic or the output was asked for in exclusive mode and runs in shared mode: the device (this endpoint name)
    /// refused it. A note, not a warning: exclusive mode is the default, so a refusal is ordinary.
    ExclusiveUnavailable { role: StreamRole, device: String, why: ExclusiveRefusal },
    /// The exclusive period guard (control.rs) stepped this stream's exclusive period up after late answers or lost
    /// samples, or turned it to shared mode once a block's period was too short as well. A note.
    ExclusivePeriodRaised { role: StreamRole, device: String, floor: crate::exclusive::PeriodFloor },
}

impl StartFailure {
    /// Whether retrying later can fix it by itself: the device is missing, busy or being reset, or
    /// the start timed out. A timed retry with backoff follows these (wasapi/A).
    pub(crate) fn transient(&self) -> bool {
        match self {
            StartFailure::NotFound(_) | StartFailure::DefaultMicIsOutput(_) => true,
            StartFailure::Timeout | StartFailure::ThreadDied | StartFailure::DriverBusy => true,
            StartFailure::Open { hr, .. } => transient_hr(*hr),
            StartFailure::DriverRing { retry, .. } => *retry,
            _ => false,
        }
    }
}

/// HRESULTs from a device that went away, is held by another app, or is being reset: worth a retry.
pub(crate) fn transient_hr(code: i32) -> bool {
    matches!(
        code,
        hr::AUDCLNT_E_DEVICE_INVALIDATED
            | hr::AUDCLNT_E_RESOURCES_INVALIDATED
            | hr::AUDCLNT_E_DEVICE_IN_USE
            | hr::AUDCLNT_E_SERVICE_NOT_RUNNING
            | hr::AUDCLNT_E_ENDPOINT_CREATE_FAILED
            | hr::E_NOTFOUND
    )
}

/// The fix the app can offer for a failed start: Windows refused a recording endpoint, which is what the
/// microphone privacy settings do to desktop apps.
pub(crate) fn remedy_for_start(f: &StartFailure) -> Option<Remedy> {
    match f {
        StartFailure::Open { role: StreamRole::Mic | StreamRole::Capture, hr: hr::E_ACCESSDENIED } => Some(Remedy::MicPrivacy),
        _ => None,
    }
}

/// The same for a fault that stopped a run (access turned off while recording).
pub(crate) fn remedy_for_fault(f: Fault) -> Option<Remedy> {
    match f {
        Fault::Stream { role: StreamRole::Mic | StreamRole::Capture, hr: hr::E_ACCESSDENIED } => Some(Remedy::MicPrivacy),
        _ => None,
    }
}

/// Whether an endpoint or interface name is one of the virtual cables users install (VB-Audio's
/// cables and Voicemeeter, VAC) or one of ours: "ACX-LL-Cable A" ([`nf_vmic::sessions::has_our_name`],
/// TODO 19), or the old driver's "AudioCodec Device" and "Noise Filter Cable B". Both ends of one of
/// these share a name, and its recording side carries whatever is played into it; two ends of a
/// real sound card share a name too, but are a real mic and real speakers.
pub(crate) fn is_virtual_cable(name: &str) -> bool {
    let n = name.to_lowercase();
    ["vb-audio", "voicemeeter", "virtual audio cable", "virtual cable", "audiocodec device", "noise filter cable b"]
        .iter()
        .any(|k| n.contains(k))
        || nf_vmic::sessions::has_our_name(name)
}

/// What stopped a running audio thread. Packed into one `AtomicU64` so the audio thread can
/// report it without allocating.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    /// A WASAPI call on a running stream failed.
    Stream { role: StreamRole, hr: i32 },
    /// No mic packets for 2 s.
    MicStalled,
}

impl Fault {
    const KIND_STREAM: u64 = 1;
    const KIND_STALL: u64 = 2;

    /// Non-zero for every fault, so 0 can mean "none".
    pub(crate) fn pack(self) -> u64 {
        match self {
            Fault::Stream { role, hr } => (Self::KIND_STREAM << 40) | ((role as u64) << 32) | hr as u32 as u64,
            Fault::MicStalled => Self::KIND_STALL << 40,
        }
    }

    pub(crate) fn unpack(v: u64) -> Option<Self> {
        match v >> 40 {
            Self::KIND_STREAM => Some(Fault::Stream { role: StreamRole::from_u8((v >> 32) as u8), hr: v as u32 as i32 }),
            Self::KIND_STALL => Some(Fault::MicStalled),
            _ => None,
        }
    }

    /// Whether waiting for the device (and restarting then, or on the timed retry) makes sense.
    pub(crate) fn device_related(self) -> bool {
        match self {
            Fault::Stream { hr, .. } => transient_hr(hr),
            Fault::MicStalled => true,
        }
    }
}

/// Words a failed WASAPI call on `role`.
pub(crate) fn describe_hr(role: StreamRole, code: i32) -> String {
    let noun = role.noun();
    match code {
        hr::AUDCLNT_E_DEVICE_INVALIDATED | hr::AUDCLNT_E_RESOURCES_INVALIDATED | hr::AUDCLNT_E_ENDPOINT_CREATE_FAILED => {
            format!("{noun} was disconnected or disabled.")
        }
        hr::E_NOTFOUND => format!("{noun} wasn't found. It may be unplugged or disabled; pick it again in the settings."),
        // A shared open (every open that can fail the start: an exclusive one falls back to shared first) is refused this
        // only while another app holds the endpoint in exclusive mode. With exclusive mode the default, the mic or the output
        // is most often held by another copy of the filter on the same device; the filter takes no other stream exclusive.
        hr::AUDCLNT_E_DEVICE_IN_USE => {
            let (holder, cost) = match role {
                StreamRole::Mic | StreamRole::Output => (
                    "Another copy of the filter counts (the window, or a run, latency or quality command on the same device): \
                     close that app, or stop its filter.",
                    ", the filter's own streams then shared too, with more latency",
                ),
                _ => ("Close that app.", ""),
            };
            format!(
                "{noun} is held in exclusive mode by another app, so the filter can't open it. {holder} Unticking \"Allow \
                 applications to take exclusive control of this device\" for the device (Sound control panel, its Properties > \
                 Advanced) keeps every app from holding it{cost}."
            )
        }
        // Only an exclusive open is refused this, and the streams that can fail the start open shared: worded for a stray one.
        hr::AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED => format!(
            "{noun} doesn't allow exclusive mode: \"Allow applications to take exclusive control of this device\" is unticked \
             for it (Sound control panel, its Properties > Advanced)."
        ),
        hr::AUDCLNT_E_SERVICE_NOT_RUNNING => {
            "The Windows Audio service isn't running. Start it (or restart the PC) and try again.".to_owned()
        }
        hr::AUDCLNT_E_UNSUPPORTED_FORMAT => {
            format!("{noun} doesn't accept a 48 kHz stream. Set its format to 48000 Hz in Sound settings > the device > Advanced.")
        }
        hr::E_ACCESSDENIED if role == StreamRole::Mic || role == StreamRole::Capture => format!(
            "Windows blocked access to {}. Turn on \"Let desktop apps access your microphone\" in Settings > Privacy & security > Microphone.",
            lower_first(noun)
        ),
        hr::E_ACCESSDENIED => format!("Windows denied access to {}.", lower_first(noun)),
        _ => format!("{noun} couldn't be used (error 0x{:08X}).", code as u32),
    }
}

/// Words a failure to initialise COM (as MTA) on the thread of a blocking test call.
pub(crate) fn describe_com_failure(code: i32) -> String {
    if code == hr::RPC_E_CHANGED_MODE {
        "This thread already uses COM in single-threaded mode (a UI thread): run the test from a worker thread.".to_owned()
    } else {
        format!("Windows audio couldn't be initialised (error 0x{:08X}).", code as u32)
    }
}

/// Words a start-up failure.
pub(crate) fn describe_start_failure(f: &StartFailure) -> String {
    match f {
        StartFailure::Com(code) | StartFailure::Enumerator(code) => format!(
            "Windows audio couldn't be initialised (error 0x{:08X}). Restart the app; if it persists, restart the PC.",
            *code as u32
        ),
        StartFailure::NotFound(StreamRole::Reference) => {
            "No speakers were found to take the echo reference from. Pick the speakers you listen on, or turn echo cancellation off."
                .to_owned()
        }
        StartFailure::NotFound(role) => describe_hr(*role, hr::E_NOTFOUND),
        StartFailure::WrongDirection(StreamRole::Mic) => {
            "The device picked as the microphone is a playback device. Pick a recording device.".to_owned()
        }
        StartFailure::WrongDirection(role) => format!("{} must be a playback device, but a recording device was picked.", role.noun()),
        StartFailure::OutputIsReference => same_device_message().to_owned(),
        StartFailure::DefaultMicIsOutput(cable) | StartFailure::MicIsOutput(cable) => {
            let mic = if matches!(f, StartFailure::DefaultMicIsOutput(_)) { "Windows default microphone" } else { "selected microphone" };
            format!(
                "Audio routing loop on {cable}. The {mic} receives the filter's own output. \
                 Choose a microphone on a different device or send the filtered output to another cable."
            )
        }
        StartFailure::Open { role, hr } => describe_hr(*role, *hr),
        StartFailure::DriverRing { message, .. } => message.clone(),
        StartFailure::Stage(msg) => format!("The audio processing couldn't start: {msg}"),
        StartFailure::ThreadDied => "The audio thread stopped unexpectedly while starting. Check the log file.".to_owned(),
        StartFailure::Timeout => "The audio devices took too long to open. Check that they're connected and try again.".to_owned(),
        StartFailure::DriverBusy => {
            "An audio driver is still busy with the last attempt to open the devices. The filter tries again once it's free.".to_owned()
        }
        StartFailure::Spawn => "The audio thread couldn't be created.".to_owned(),
    }
}

/// Words a start-up warning.
pub(crate) fn describe_warning(w: &Warning) -> String {
    match w {
        Warning::Mmcss(code) => format!(
            "The audio thread couldn't join the Windows \"Pro Audio\" scheduling class (error {code}); dropouts are more likely under load."
        ),
        Warning::InferenceMmcss(code) => format!(
            "The noise model's inference thread couldn't join the Windows \"Pro Audio\" scheduling class (error {code}); it runs at a raised normal priority, so under load the model is more likely to miss blocks (they play the unfiltered microphone)."
        ),
        Warning::ReferenceUnavailable(f) => {
            format!("Echo cancellation has no reference signal: {}", describe_start_failure(f))
        }
        Warning::KeepAliveUnavailable(code) => format!(
            "The silent keep-alive stream on the speakers couldn't be opened (error 0x{:08X}); the echo reference may drop out during silence.",
            *code as u32
        ),
        Warning::MicSharesOutputDevice(adapter) => format!(
            "Possible audio routing loop on {adapter}. If this microphone carries the output cable's playback, \
             the filter receives its own output. Choose a microphone on a different device or send the filtered output to another cable."
        ),
        Warning::OutputIsSpeakers { name, exclusive_withheld } => format!(
            "The output is your default playback device ({name}), which doesn't look like a virtual cable: the cleaned \
             microphone plays out loud instead of reaching your other apps. Pick the virtual cable as the output.{}",
            withheld_text(*exclusive_withheld)
        ),
        Warning::CableIsDefaultPlayback { name, exclusive_withheld } => format!(
            "The output cable ({name}) is also Windows' default playback device, so every app's sound goes into your filtered \
             microphone. Make your speakers or headphones the default playback device in Sound settings.{}",
            withheld_text(*exclusive_withheld)
        ),
        Warning::ExclusiveUnavailable { role, device, why } => describe_exclusive_fallback(*role, device, *why),
        Warning::ExclusivePeriodRaised { role, device, floor } => describe_period_raised(*role, device, *floor),
    }
}

/// The output warnings' last sentence when the output was asked for in exclusive mode: the default playback device runs
/// shared (`audio::output_check`), and the change of default restarts the streams (`control::device_event_needs_restart`).
fn withheld_text(exclusive_withheld: bool) -> &'static str {
    if exclusive_withheld {
        " Until then it runs in shared mode, with more latency: in exclusive mode the filter would have it to itself, and no \
         other app could play sound on it."
    } else {
        ""
    }
}

impl Warning {
    /// A note rather than a warning: how the engine runs, which needs nothing done (`EngineStatus::notes`). Exclusive mode is
    /// the default (the owner, 2026-11-02), so a device that refuses it is ordinary: the stream runs shared, as it did before.
    pub(crate) fn is_note(&self) -> bool {
        matches!(self, Warning::ExclusiveUnavailable { .. } | Warning::ExclusivePeriodRaised { .. })
    }
}

/// The note for a stream the exclusive period guard stepped up: the audio thread missed its device's deadline at the
/// shorter period (or lost the mic's samples) again and again, as a PC whose drivers hold the CPU (DPC latency) does.
fn describe_period_raised(role: StreamRole, device: &str, floor: crate::exclusive::PeriodFloor) -> String {
    let noun = role.noun();
    let missed = match role {
        StreamRole::Mic => "it lost samples",
        _ => "the filter answered it late",
    };
    let why = "as the PC held the filter's audio thread up past it (LatencyMon shows which driver does)";
    match floor {
        crate::exclusive::PeriodFloor::AtLeast(frames) => format!(
            "{noun} ({device}) runs at an exclusive period of {:.2} ms or longer until the filter's app restarts: at the \
             shorter one {missed} again and again, {why}. That costs a little latency and saves the clicks.",
            frames as f64 * 1000.0 / f64::from(nf_core::SAMPLE_RATE)
        ),
        crate::exclusive::PeriodFloor::Shared => format!(
            "{noun} ({device}) runs in shared mode until the filter's app restarts: in exclusive mode {missed} again and \
             again, {why}. Shared mode has more latency and more room for that."
        ),
    }
}

/// The note for a stream asked for in exclusive mode that runs shared: which, why, what it costs (the A/B on the dev VM's
/// VB-CABLE cables, TODO 13: shared output 21-30 ms more, shared mic 10-19, depending on the other stream's mode), and what
/// the user can do: fix the cause where there is one, or turn that stream's option off to stop it trying (the window's
/// switch and the CLI's flag both named: the engine doesn't know which front end shows it).
fn describe_exclusive_fallback(role: StreamRole, device: &str, why: ExclusiveRefusal) -> String {
    let cost = match role {
        StreamRole::Output => "about 20-30 ms more on VB-CABLE",
        StreamRole::Mic => "about 10-20 ms more on VB-CABLE",
        _ => "a little more",
    };
    let fix = match why {
        ExclusiveRefusal::Refused(hr::AUDCLNT_E_DEVICE_IN_USE) => Some(
            "To get exclusive mode, close the app that holds it and restart the filter (or start the filter before that app \
             next time).",
        ),
        ExclusiveRefusal::Refused(hr::AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED) => Some(
            "To get exclusive mode, tick \"Allow applications to take exclusive control of this device\" in the Sound control \
             panel (Settings > System > Sound > More sound settings), under the device's Properties > Advanced, and restart \
             the filter.",
        ),
        _ => None,
    };
    let stop = match role {
        StreamRole::Output => " To stop trying, turn off Exclusive output (the window's Diagnostics > Latency, or --no-exclusive-output).",
        StreamRole::Mic => " To stop trying, turn off Exclusive microphone (the window's Diagnostics > Latency, or --no-exclusive-mic).",
        _ => "",
    };
    format!(
        "{} ({device}) runs in shared mode: {}. It works as usual, with more latency ({cost}).{}{stop}",
        role.noun(),
        describe_refusal(why),
        fix.map(|f| format!(" {f}")).unwrap_or_default()
    )
}

/// Why exclusive mode was refused, as a clause: the note's, and the stream's in the status and the reports' conditions.
pub(crate) fn describe_refusal(why: ExclusiveRefusal) -> String {
    match why {
        ExclusiveRefusal::NoFormat | ExclusiveRefusal::Refused(hr::AUDCLNT_E_UNSUPPORTED_FORMAT) => {
            "it takes none of the formats the filter can use in exclusive mode (48 kHz in 32-bit float, 24-bit or 16-bit)".to_owned()
        }
        ExclusiveRefusal::Refused(hr::AUDCLNT_E_DEVICE_IN_USE) => {
            "another app is using it, and exclusive mode needs it to itself".to_owned()
        }
        ExclusiveRefusal::Refused(hr::AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED) => {
            "exclusive control of it is turned off in Windows' sound settings".to_owned()
        }
        ExclusiveRefusal::Refused(
            code @ (hr::AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED | hr::AUDCLNT_E_INVALID_DEVICE_PERIOD | hr::AUDCLNT_E_BUFFER_SIZE_ERROR),
        ) => format!("it refused the short period (error 0x{:08X})", code as u32),
        ExclusiveRefusal::Refused(code) => format!("error 0x{:08X}", code as u32),
        ExclusiveRefusal::Periods { hr: code, shortest_hns, longest_hns } => {
            let ms = |hns: i64| format!("{:.2} ms", hns as f64 / 10_000.0);
            let tried = if shortest_hns == longest_hns {
                format!("the {} period", ms(shortest_hns))
            } else {
                format!("every period tried, {} to {}", ms(shortest_hns), ms(longest_hns))
            };
            format!("it refused {tried} (error 0x{:08X})", code as u32)
        }
        ExclusiveRefusal::DefaultPlayback => {
            "it is Windows' default playback device, and in exclusive mode no other app could play sound on it".to_owned()
        }
    }
}

/// Words the loss of the echo reference (or its keep-alive) while running: the filter carries on
/// without it (wasapi/B).
pub(crate) fn describe_reference_lost(f: Fault) -> String {
    let why = match f {
        Fault::Stream { role, hr: code } => describe_hr(role, code),
        Fault::MicStalled => "It stopped sending audio.".to_owned(),
    };
    format!(
        "Echo cancellation lost its reference: {why} The filter keeps running without echo cancellation, and tries the speakers again when they come back or change, and every so often."
    )
}

/// Words a fault that stopped a running engine.
pub(crate) fn describe_fault(f: Fault) -> String {
    match f {
        Fault::Stream { role, hr: code } => {
            let base = describe_hr(role, code);
            if f.device_related() { format!("{base} The filter restarts when it's back.") } else { base }
        }
        Fault::MicStalled => {
            "The microphone stopped sending audio for 2 seconds. Check that it's connected; the filter restarts when it's back.".to_owned()
        }
    }
}

/// The output can't be the echo reference: our own output would be cancelled as echo.
pub(crate) fn same_device_message() -> &'static str {
    "The filtered output and echo reference use the same playback endpoint. Echo cancellation would use the filter's own output \
     as its reference. Choose the speakers playing the far-end audio as the echo reference or send the filtered output to another cable."
}

/// The output isn't chosen.
pub(crate) fn no_output_message() -> &'static str {
    "Pick the virtual cable to send the cleaned mic to."
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_lowercase().chain(c).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faults_round_trip_through_a_u64() {
        let all = [
            Fault::Stream { role: StreamRole::Mic, hr: hr::AUDCLNT_E_DEVICE_INVALIDATED },
            Fault::Stream { role: StreamRole::Output, hr: -1 },
            Fault::Stream { role: StreamRole::Reference, hr: 0 },
            Fault::Stream { role: StreamRole::KeepAlive, hr: i32::MIN },
            Fault::MicStalled,
        ];
        for f in all {
            let v = f.pack();
            assert_ne!(v, 0);
            assert_eq!(Fault::unpack(v), Some(f));
        }
        assert_eq!(Fault::unpack(0), None);
    }

    #[test]
    fn device_faults_wait_for_the_device() {
        assert!(Fault::Stream { role: StreamRole::Mic, hr: hr::AUDCLNT_E_DEVICE_INVALIDATED }.device_related());
        assert!(Fault::MicStalled.device_related());
        assert!(!Fault::Stream { role: StreamRole::Mic, hr: hr::E_FAIL }.device_related());
    }

    #[test]
    fn messages_name_the_device_and_the_fix() {
        let m = describe_hr(StreamRole::Mic, hr::AUDCLNT_E_DEVICE_INVALIDATED);
        assert_eq!(m, "The microphone was disconnected or disabled.");
        let m = describe_hr(StreamRole::Mic, hr::E_ACCESSDENIED);
        assert!(m.starts_with("Windows blocked access to the microphone.") && m.contains("Privacy"), "{m}");
        let m = describe_hr(StreamRole::Output, hr::AUDCLNT_E_DEVICE_IN_USE);
        assert!(m.starts_with("The output device is held in exclusive mode by another app"), "{m}");
        let m = describe_hr(StreamRole::Mic, hr::AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED);
        assert!(m.starts_with("The microphone doesn't allow exclusive mode") && !m.contains("another app"), "{m}");
        let m = describe_hr(StreamRole::Output, 0x8889_0099_u32 as i32);
        assert_eq!(m, "The output device couldn't be used (error 0x88890099).");
        assert!(describe_start_failure(&StartFailure::NotFound(StreamRole::Output)).contains("pick it again"));
        let reference = describe_start_failure(&StartFailure::OutputIsReference);
        assert!(
            reference.contains("same playback endpoint") && reference.contains("echo reference or send the filtered output"),
            "{reference}"
        );
        assert!(describe_start_failure(&StartFailure::WrongDirection(StreamRole::Mic)).contains("recording device"));
        assert!(describe_start_failure(&StartFailure::Stage("boom".into())).ends_with("boom"));
        assert!(describe_fault(Fault::MicStalled).contains("2 seconds"));
        let f = describe_fault(Fault::Stream { role: StreamRole::Mic, hr: hr::AUDCLNT_E_DEVICE_INVALIDATED });
        assert!(f.ends_with("restarts when it's back."), "{f}");
        assert!(describe_warning(&Warning::Mmcss(1552)).contains("Pro Audio"));
        let w = describe_warning(&Warning::InferenceMmcss(1552));
        assert!(w.contains("inference thread") && w.contains("error 1552") && w.contains("miss blocks"), "{w}");
        let w = describe_warning(&Warning::ReferenceUnavailable(StartFailure::NotFound(StreamRole::Reference)));
        assert!(w.starts_with("Echo cancellation has no reference signal: No speakers"), "{w}");
        let w = describe_warning(&Warning::MicSharesOutputDevice("VB-Audio Virtual Cable".into()));
        assert!(w.contains("VB-Audio Virtual Cable") && w.contains("different device or send the filtered output"), "{w}");
        let m = describe_start_failure(&StartFailure::DefaultMicIsOutput("VB-Audio Virtual Cable".into()));
        assert!(m.contains("default microphone") && m.contains("VB-Audio Virtual Cable"), "{m}");
        let m = describe_start_failure(&StartFailure::MicIsOutput("ACX-LL-Cable A".into()));
        assert!(m.contains("routing loop on ACX-LL-Cable A") && m.contains("different device or send the filtered output"), "{m}");
        let r = describe_reference_lost(Fault::Stream { role: StreamRole::Reference, hr: hr::AUDCLNT_E_DEVICE_INVALIDATED });
        assert!(r.starts_with("Echo cancellation lost its reference:") && r.contains("keeps running"), "{r}");
        assert!(describe_hr(StreamRole::Mic, hr::AUDCLNT_E_ENDPOINT_CREATE_FAILED).contains("disconnected"));
        assert!(describe_hr(StreamRole::FarEnd, hr::AUDCLNT_E_DEVICE_INVALIDATED).starts_with("The far-end device was disconnected"));
        assert_eq!(StreamRole::from_u8(StreamRole::FarEnd as u8), StreamRole::FarEnd);
        // A UI thread's COM: said plainly; anything else keeps its code
        assert!(describe_com_failure(hr::RPC_E_CHANGED_MODE).contains("worker thread"));
        assert_eq!(describe_com_failure(hr::E_FAIL), "Windows audio couldn't be initialised (error 0x80004005).");
    }

    /// The exclusive period guard's note (control.rs): which stream, at what, why, and what it costs.
    #[test]
    fn a_stepped_up_period_is_a_note() {
        use crate::exclusive::PeriodFloor;
        let note = |role, floor| {
            let w = Warning::ExclusivePeriodRaised { role, device: "Speakers (USB)".into(), floor };
            assert!(w.is_note());
            describe_warning(&w)
        };
        assert_eq!(
            note(StreamRole::Output, PeriodFloor::AtLeast(160)),
            "The output device (Speakers (USB)) runs at an exclusive period of 3.33 ms or longer until the filter's app \
             restarts: at the shorter one the filter answered it late again and again, as the PC held the filter's audio \
             thread up past it (LatencyMon shows which driver does). That costs a little latency and saves the clicks."
        );
        assert_eq!(
            note(StreamRole::Mic, PeriodFloor::Shared),
            "The microphone (Speakers (USB)) runs in shared mode until the filter's app restarts: in exclusive mode it lost \
             samples again and again, as the PC held the filter's audio thread up past it (LatencyMon shows which driver \
             does). Shared mode has more latency and more room for that."
        );
    }

    /// Exclusive mode is the default (the owner, 2026-11-02), so a refusal is ordinary: a note, not a warning, that says which
    /// stream runs shared and why, what that costs, and what to do: the fix where there is one (the app holding the device, or
    /// Windows' "Allow applications to take exclusive control" box), and how to stop it trying.
    #[test]
    fn an_exclusive_refusal_is_a_note_with_its_cost_and_its_fix() {
        let note = |role, device: &str, why| {
            let w = Warning::ExclusiveUnavailable { role, device: device.into(), why };
            assert!(w.is_note());
            describe_warning(&w)
        };
        let out = |why| note(StreamRole::Output, "CABLE Input", why);
        let w = out(ExclusiveRefusal::Refused(hr::AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED));
        assert_eq!(
            w,
            "The output device (CABLE Input) runs in shared mode: exclusive control of it is turned off in Windows' sound settings. \
             It works as usual, with more latency (about 20-30 ms more on VB-CABLE). To get exclusive mode, tick \"Allow \
             applications to take exclusive control of this device\" in the Sound control panel (Settings > System > Sound > More \
             sound settings), under the device's Properties > Advanced, and restart the filter. To stop trying, turn off \
             Exclusive output (the window's Diagnostics > Latency, or --no-exclusive-output)."
        );
        let w = note(StreamRole::Mic, "Mic (USB)", ExclusiveRefusal::Refused(hr::AUDCLNT_E_DEVICE_IN_USE));
        assert_eq!(
            w,
            "The microphone (Mic (USB)) runs in shared mode: another app is using it, and exclusive mode needs it to itself. It \
             works as usual, with more latency (about 10-20 ms more on VB-CABLE). To get exclusive mode, close the app that holds \
             it and restart the filter (or start the filter before that app next time). To stop trying, turn off Exclusive \
             microphone (the window's Diagnostics > Latency, or --no-exclusive-mic)."
        );
        // Nothing to fix: only how to stop it trying.
        let w = out(ExclusiveRefusal::NoFormat);
        assert!(w.contains(": it takes none of the formats the filter can use in exclusive mode (48 kHz in 32-bit float"), "{w}");
        let stop = "To stop trying, turn off Exclusive output (the window's Diagnostics > Latency, or --no-exclusive-output).";
        assert!(w.ends_with(&format!("more on VB-CABLE). {stop}")), "{w}");
        assert_eq!(out(ExclusiveRefusal::Refused(hr::AUDCLNT_E_UNSUPPORTED_FORMAT)), w, "as no format");
        assert!(
            out(ExclusiveRefusal::Refused(hr::AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED))
                .contains(": it refused the short period (error 0x88890019).")
        );
        assert!(out(ExclusiveRefusal::Refused(hr::E_FAIL)).contains("shared mode: error 0x80004005. It works"));
        let periods = ExclusiveRefusal::Periods { hr: hr::AUDCLNT_E_INVALID_DEVICE_PERIOD, shortest_hns: 30_000, longest_hns: 100_000 };
        assert!(out(periods).contains(": it refused every period tried, 3.00 ms to 10.00 ms (error 0x88890020). It"), "{}", out(periods));
        let one = ExclusiveRefusal::Periods { hr: hr::E_INVALIDARG, shortest_hns: 20_000, longest_hns: 20_000 };
        assert!(out(one).contains(": it refused the 2.00 ms period (error 0x80070057)."), "{}", out(one));
        // The stream's own clause (the status, the reports' conditions) is the short why, without the fix.
        assert_eq!(
            describe_refusal(ExclusiveRefusal::Refused(hr::AUDCLNT_E_DEVICE_IN_USE)),
            "another app is using it, and exclusive mode needs it to itself"
        );
        // Every other start-up condition stays a warning.
        for w in [Warning::Mmcss(1), Warning::InferenceMmcss(1), Warning::KeepAliveUnavailable(0), Warning::OutputIsSpeakers {
            name: "x".into(),
            exclusive_withheld: true,
        }] {
            assert!(!w.is_note(), "{w:?}");
        }
    }

    /// The output on Windows' default playback device: the speakers, or a cable that then carries every app's sound. Either
    /// way it needs fixing, so a warning; asked for in exclusive mode, it also says the output runs shared until then, as
    /// exclusive mode there would keep every other app from playing sound (and the stream's own clause says why).
    #[test]
    fn the_default_playback_output_warns_and_says_it_runs_shared() {
        let speakers = |exclusive_withheld| {
            describe_warning(&Warning::OutputIsSpeakers { name: "Speakers (Realtek(R) Audio)".into(), exclusive_withheld })
        };
        let cable =
            |exclusive_withheld| describe_warning(&Warning::CableIsDefaultPlayback { name: "CABLE Input".into(), exclusive_withheld });
        let withheld = " Until then it runs in shared mode, with more latency: in exclusive mode the filter would have it to itself, \
                        and no other app could play sound on it.";
        assert!(speakers(false).contains("plays out loud") && speakers(false).ends_with("Pick the virtual cable as the output."));
        assert_eq!(speakers(true), format!("{}{withheld}", speakers(false)));
        assert!(cable(false).contains("every app's sound goes into your filtered microphone"), "{}", cable(false));
        assert!(cable(false).ends_with("the default playback device in Sound settings."), "{}", cable(false));
        assert_eq!(cable(true), format!("{}{withheld}", cable(false)));
        assert_eq!(
            describe_refusal(ExclusiveRefusal::DefaultPlayback),
            "it is Windows' default playback device, and in exclusive mode no other app could play sound on it"
        );
    }

    /// A start that fails on a device held in exclusive mode (with exclusive mode the default, most often another copy of the
    /// filter: the window and a CLI command on the same cable or mic) says so, and what unticking Windows' box would cost.
    #[test]
    fn a_device_held_by_another_app_names_the_filter_itself() {
        let m = describe_start_failure(&StartFailure::Open { role: StreamRole::Mic, hr: hr::AUDCLNT_E_DEVICE_IN_USE });
        assert!(m.starts_with("The microphone is held in exclusive mode by another app, so the filter can't open it."), "{m}");
        assert!(m.contains("Another copy of the filter counts (the window, or a run, latency or quality command"), "{m}");
        assert!(m.contains("close that app, or stop its filter"), "{m}");
        assert!(m.contains("the filter's own streams then shared too, with more latency"), "{m}");
        let f = describe_fault(Fault::Stream { role: StreamRole::Output, hr: hr::AUDCLNT_E_DEVICE_IN_USE });
        assert!(f.starts_with("The output device is held in exclusive mode") && f.ends_with("The filter restarts when it's back."), "{f}");
        // Streams the filter never takes exclusive (the reference, the lab's probe and capture): just another app, and
        // unticking the box costs the filter nothing there.
        let r = describe_hr(StreamRole::Reference, hr::AUDCLNT_E_DEVICE_IN_USE);
        assert!(r.contains("so the filter can't open it. Close that app. Unticking") && !r.contains("copy of the filter"), "{r}");
        assert!(r.ends_with("keeps every app from holding it."), "{r}");
    }

    /// The owner's ask (2026-10-31): a mic that Windows' privacy settings block offers a way to the setting;
    /// other failures, and other devices refused, don't.
    #[test]
    fn a_blocked_mic_points_to_the_privacy_settings() {
        let denied = |role| StartFailure::Open { role, hr: hr::E_ACCESSDENIED };
        assert_eq!(remedy_for_start(&denied(StreamRole::Mic)), Some(Remedy::MicPrivacy));
        assert_eq!(remedy_for_start(&denied(StreamRole::Capture)), Some(Remedy::MicPrivacy));
        assert_eq!(remedy_for_start(&denied(StreamRole::Output)), None);
        assert_eq!(remedy_for_start(&StartFailure::Open { role: StreamRole::Mic, hr: hr::AUDCLNT_E_DEVICE_IN_USE }), None);
        assert_eq!(remedy_for_start(&StartFailure::NotFound(StreamRole::Mic)), None);
        assert_eq!(remedy_for_fault(Fault::Stream { role: StreamRole::Mic, hr: hr::E_ACCESSDENIED }), Some(Remedy::MicPrivacy));
        assert_eq!(remedy_for_fault(Fault::MicStalled), None);
        assert_eq!(Remedy::MicPrivacy.settings_uri(), "ms-settings:privacy-microphone");
    }

    /// wasapi/E: only the virtual cables people install count as cables; a real card's mic and
    /// speakers share an interface name too.
    #[test]
    fn virtual_cables_are_recognised_by_name() {
        for n in [
            "VB-Audio Virtual Cable",
            "VB-Audio Cable A",
            "VB-Audio Voicemeeter VAIO",
            "Voicemeeter Out B1 (VB-Audio Voicemeeter VAIO)",
            "Virtual Audio Cable",
            "CABLE Input (VB-Audio Virtual Cable)",
            // Ours (TODO 19): each cable's device name, its endpoints', numbered or in any case; and the old driver's
            "ACX-LL-Cable A",
            "Speakers (ACX-LL-Cable B)",
            "Microphone (2- ACX-LL-Cable C)",
            "acx-ll-cable d",
            "AudioCodec Device",
            "Speakers (Noise Filter Cable B)",
        ] {
            assert!(is_virtual_cable(n), "{n}");
        }
        for n in ["Realtek(R) Audio", "USB Audio Device", "Speakers (Realtek(R) Audio)", "Steinberg UR22C", "Remote Audio", ""] {
            assert!(!is_virtual_cable(n), "{n}");
        }
    }

    /// wasapi/A: what the timed retry follows.
    #[test]
    fn transient_failures_are_retried() {
        let open = |hr| StartFailure::Open { role: StreamRole::Mic, hr };
        for f in [
            StartFailure::NotFound(StreamRole::Mic),
            StartFailure::DefaultMicIsOutput("x".into()),
            StartFailure::Timeout,
            StartFailure::ThreadDied,
            open(hr::AUDCLNT_E_DEVICE_INVALIDATED),
            open(hr::AUDCLNT_E_DEVICE_IN_USE),
            open(hr::AUDCLNT_E_RESOURCES_INVALIDATED),
            open(hr::AUDCLNT_E_SERVICE_NOT_RUNNING),
            open(hr::AUDCLNT_E_ENDPOINT_CREATE_FAILED),
            StartFailure::DriverBusy,
        ] {
            assert!(f.transient(), "{f:?}");
        }
        for f in [
            open(hr::AUDCLNT_E_UNSUPPORTED_FORMAT),
            open(hr::E_ACCESSDENIED),
            StartFailure::WrongDirection(StreamRole::Mic),
            StartFailure::OutputIsReference,
            StartFailure::MicIsOutput("x".into()),
            StartFailure::Stage("x".into()),
            StartFailure::Spawn,
            StartFailure::Com(0),
        ] {
            assert!(!f.transient(), "{f:?}");
        }
        // An exclusive-mode grab mid-run is retried too, and the message says so.
        let busy = Fault::Stream { role: StreamRole::Mic, hr: hr::AUDCLNT_E_DEVICE_IN_USE };
        assert!(busy.device_related());
        assert!(describe_fault(busy).ends_with("restarts when it's back."));
    }
}
