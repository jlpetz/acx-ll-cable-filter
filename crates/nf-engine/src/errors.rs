//! Engine failures as plain data, and their wording for the user.
//!
//! The audio thread never formats anything: it keeps a raw HRESULT (or a fault code) and the
//! control thread turns it into a sentence here. Everything in this module is pure.

/// HRESULTs we word specially (values from `windows::Win32::Media::Audio` / `Foundation`).
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
    pub(crate) const AUDCLNT_E_RESOURCES_INVALIDATED: i32 = 0x8889_0026_u32 as i32;
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
}

impl StreamRole {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => StreamRole::Mic,
            2 => StreamRole::Output,
            3 => StreamRole::Reference,
            4 => StreamRole::KeepAlive,
            5 => StreamRole::Probe,
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
    /// Opening or starting a stream failed with this HRESULT.
    Open { role: StreamRole, hr: i32 },
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

/// A non-fatal start-up condition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Warning {
    /// MMCSS registration failed with this Win32 error; the thread runs at a raised normal priority.
    Mmcss(u32),
    /// The echo reference couldn't be opened; AEC runs on silence.
    ReferenceUnavailable(StartFailure),
    /// The keep-alive stream couldn't be opened; the reference has gaps during silence.
    KeepAliveUnavailable(i32),
    /// The mic and the output are on the same device (this interface friendly name). If the mic is
    /// the output cable's recording side, the filter only hears its own output.
    MicSharesOutputDevice(String),
    /// The output is the default playback device and doesn't look like a virtual cable (this
    /// endpoint name): the cleaned mic plays out of the speakers instead of reaching other apps.
    OutputIsSpeakers(String),
    /// The output cable is also the default playback device (this endpoint name), so every app's
    /// sound goes into the filtered mic.
    CableIsDefaultPlayback(String),
}

impl StartFailure {
    /// Whether retrying later can fix it by itself: the device is missing, busy or being reset, or
    /// the start timed out. A timed retry with backoff follows these (wasapi/A).
    pub(crate) fn transient(&self) -> bool {
        match self {
            StartFailure::NotFound(_) | StartFailure::DefaultMicIsOutput(_) => true,
            StartFailure::Timeout | StartFailure::ThreadDied | StartFailure::DriverBusy => true,
            StartFailure::Open { hr, .. } => transient_hr(*hr),
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

/// Whether an endpoint or interface name is one of the virtual cables users install (VB-Audio's
/// cables and Voicemeeter, VAC). Both ends of one of these share a name, and its recording side
/// carries whatever is played into it; two ends of a real sound card share a name too, but are
/// a real mic and real speakers.
pub(crate) fn is_virtual_cable(name: &str) -> bool {
    let n = name.to_lowercase();
    ["vb-audio", "voicemeeter", "virtual audio cable", "virtual cable"].iter().any(|k| n.contains(k))
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
            Self::KIND_STREAM => {
                Some(Fault::Stream { role: StreamRole::from_u8((v >> 32) as u8), hr: v as u32 as i32 })
            }
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
        hr::AUDCLNT_E_DEVICE_IN_USE | hr::AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED => format!(
            "{noun} is in use by another app in exclusive mode. Close that app, or turn off \"Allow applications to take exclusive control\" for the device in Sound settings."
        ),
        hr::AUDCLNT_E_SERVICE_NOT_RUNNING => "The Windows Audio service isn't running. Start it (or restart the PC) and try again.".to_owned(),
        hr::AUDCLNT_E_UNSUPPORTED_FORMAT => format!(
            "{noun} doesn't accept a 48 kHz stream. Set its format to 48000 Hz in Sound settings > the device > Advanced."
        ),
        hr::E_ACCESSDENIED if role == StreamRole::Mic || role == StreamRole::Capture => format!(
            "Windows blocked access to {}. Turn on \"Let desktop apps access your microphone\" in Settings > Privacy & security > Microphone.",
            lower_first(noun)
        ),
        hr::E_ACCESSDENIED => format!("Windows denied access to {}.", lower_first(noun)),
        _ => format!("{noun} couldn't be used (error 0x{:08X}).", code as u32),
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
            "No speakers were found to take the echo reference from. Pick the speakers you listen on, or turn echo cancellation off.".to_owned()
        }
        StartFailure::NotFound(role) => describe_hr(*role, hr::E_NOTFOUND),
        StartFailure::WrongDirection(StreamRole::Mic) => {
            "The device picked as the microphone is a playback device. Pick a recording device.".to_owned()
        }
        StartFailure::WrongDirection(role) => format!(
            "{} must be a playback device, but a recording device was picked.",
            role.noun()
        ),
        StartFailure::OutputIsReference => same_device_message().to_owned(),
        StartFailure::DefaultMicIsOutput(adapter) => format!(
            "The default microphone is the recording side of the output cable ({adapter}), so the filter would only hear its own output. Pick your real microphone, or make it the default; the filter starts when the default changes."
        ),
        StartFailure::Open { role, hr } => describe_hr(*role, *hr),
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
        Warning::ReferenceUnavailable(f) => {
            format!("Echo cancellation has no reference signal: {}", describe_start_failure(f))
        }
        Warning::KeepAliveUnavailable(code) => format!(
            "The silent keep-alive stream on the speakers couldn't be opened (error 0x{:08X}); the echo reference may drop out during silence.",
            *code as u32
        ),
        Warning::MicSharesOutputDevice(adapter) => format!(
            "The microphone is on the same device as the output ({adapter}). If it is the output cable's recording side, the filter only hears its own output and your apps get silence. Pick your real microphone."
        ),
        Warning::OutputIsSpeakers(name) => format!(
            "The output is your default playback device ({name}), which doesn't look like a virtual cable: the cleaned microphone plays out loud instead of reaching your other apps. Pick the virtual cable as the output."
        ),
        Warning::CableIsDefaultPlayback(name) => format!(
            "The output cable ({name}) is also Windows' default playback device, so every app's sound goes into your filtered microphone. Make your speakers or headphones the default playback device in Sound settings."
        ),
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
        Fault::MicStalled => "The microphone stopped sending audio for 2 seconds. Check that it's connected; the filter restarts when it's back.".to_owned(),
    }
}

/// The output can't be the echo reference: our own output would be cancelled as echo.
pub(crate) fn same_device_message() -> &'static str {
    "The output and the echo reference are the same device, so the filter would hear its own output as echo. Pick the speakers you listen on as the reference."
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
        assert!(m.starts_with("The output device is in use"), "{m}");
        let m = describe_hr(StreamRole::Output, 0x8889_0099_u32 as i32);
        assert_eq!(m, "The output device couldn't be used (error 0x88890099).");
        assert!(describe_start_failure(&StartFailure::NotFound(StreamRole::Output)).contains("pick it again"));
        assert!(describe_start_failure(&StartFailure::OutputIsReference).contains("same device"));
        assert!(describe_start_failure(&StartFailure::WrongDirection(StreamRole::Mic)).contains("recording device"));
        assert!(describe_start_failure(&StartFailure::Stage("boom".into())).ends_with("boom"));
        assert!(describe_fault(Fault::MicStalled).contains("2 seconds"));
        let f = describe_fault(Fault::Stream { role: StreamRole::Mic, hr: hr::AUDCLNT_E_DEVICE_INVALIDATED });
        assert!(f.ends_with("restarts when it's back."), "{f}");
        assert!(describe_warning(&Warning::Mmcss(1552)).contains("Pro Audio"));
        let w = describe_warning(&Warning::ReferenceUnavailable(StartFailure::NotFound(StreamRole::Reference)));
        assert!(w.starts_with("Echo cancellation has no reference signal: No speakers"), "{w}");
        let w = describe_warning(&Warning::MicSharesOutputDevice("VB-Audio Virtual Cable".into()));
        assert!(w.contains("(VB-Audio Virtual Cable)") && w.contains("Pick your real microphone"), "{w}");
        let m = describe_start_failure(&StartFailure::DefaultMicIsOutput("VB-Audio Virtual Cable".into()));
        assert!(m.contains("default microphone") && m.contains("VB-Audio Virtual Cable"), "{m}");
        let r = describe_reference_lost(Fault::Stream { role: StreamRole::Reference, hr: hr::AUDCLNT_E_DEVICE_INVALIDATED });
        assert!(r.starts_with("Echo cancellation lost its reference:") && r.contains("keeps running"), "{r}");
        assert!(describe_warning(&Warning::OutputIsSpeakers("Speakers (Realtek(R) Audio)".into())).contains("plays out loud"));
        assert!(describe_warning(&Warning::CableIsDefaultPlayback("CABLE Input".into())).contains("default playback"));
        assert!(describe_hr(StreamRole::Mic, hr::AUDCLNT_E_ENDPOINT_CREATE_FAILED).contains("disconnected"));
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
