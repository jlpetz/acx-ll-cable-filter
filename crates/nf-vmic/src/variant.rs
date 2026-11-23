//! The driver's three variants (TODO 19, the owner's decision of 2026-11-23): one INF, AudioCodec.inf, whose hardware ID
//! picks the microphones. `ROOT\AudioCodec` has both ("Microphone (AudioCodec Device)", low latency, IAudioClient3's
//! 2.67-10 ms periods; and "Call Microphone (AudioCodec Device)", a 5 ms default period for apps that ask for none),
//! `ROOT\AudioCodecCall` the call microphone only, `ROOT\AudioCodecLowLatency` the low-latency one only. Every variant has
//! "Speakers (AudioCodec Device)". Windows reads a microphone's default period only when it creates the endpoint (TODO 22),
//! so a switch removes the root device and creates a new one with the other hardware ID, which needs Administrator access.
//!
//! - [`present`] reads which of our devices are there, their state and driver, and the newest of our packages the driver store
//!   has for each (any user may; the read of the driver store is the compatible-driver search on the device itself).
//! - [`plan`] decides what a switch does (pure).
//! - [`switch`] does it, elevated: what `devcon remove` and `devcon install` do, with the driver taken from the driver store
//!   (the package must have been added to it, by `pnputil /add-driver` or the installer).
//!
//! # The switch, in order
//! 0. Take the lock, `Global\noise-filter-mic-variant` (a named mutex), held to the end, so two switches at once (two prompts
//!    approved, or an administrator's terminal beside the helper) can't both create a device. A second switch waits up to
//!    [`LOCK_WAIT`], then stops; one whose holder's process ended mid-switch (an abandoned mutex) goes on, reading afresh.
//! 1. Find our devices: every present device (`SetupDiGetClassDevsW`, all classes) whose `SPDRP_HARDWAREID` lists one of the
//!    three IDs, and for each, the newest of our packages compatible with it (as in step 2, on the device itself). A device of
//!    the target variant is kept only if it is working and runs that package (its `DEVPKEY_Device_DriverInfPath` is the
//!    package's `oemNN.inf` and its `DEVPKEY_Device_DriverVersion` the package's). One on an older package is re-created: the
//!    single-microphone driver before the second capture circuit (f2e7d15) had `ROOT\AudioCodec` too, so a working old
//!    install reads as `both`. Re-created rather than updated in place, because Windows makes an endpoint, and reads the
//!    properties the INF gives it (the call microphone's default period), only when it creates it (TODO 22: of the changes
//!    tried, only uninstalling and reinstalling applied a new period), and a driver update keeps the device and its endpoints.
//! 2. The new device first, not yet registered (`SetupDiCreateDeviceInfoW` under class MEDIA with `DICD_GENERATE_ID`, as devcon
//!    does: `ROOT\MEDIA\NNNN`; the hardware ID by `SetupDiSetDeviceRegistryPropertyW`), and its driver: the compatible-driver
//!    search over the driver store (`SetupDiBuildDriverInfoList(SPDIT_COMPATDRIVER)`), which is how Plug and Play itself
//!    matches a hardware ID to an INF, keeping the nodes whose INF (`C:\Windows\INF\oemNN.inf`) has the original name
//!    `audiocodec.inf` (`SetupGetInfInformationW` + `SetupQueryInfOriginalFileInformationW`), the newest by date, then version.
//!    So a missing driver stops the switch before anything is removed. An element that isn't registered yet is a phantom:
//!    destroying the set deletes it.
//! 3. Note our capture endpoints (active or disabled), then remove the old devices (`DiUninstallDevice`, what
//!    `pnputil /remove-device` does). If Windows needs a restart to finish a removal, stop there: the old driver may still be
//!    loaded. The new device is registered only after this, so that its endpoints are made while the old ones are gone, and
//!    the driver never runs twice.
//! 4. Register the new device (`SetupDiCallClassInstaller(DIF_REGISTERDEVICE)`) and install the chosen driver on it
//!    (`DiInstallDevice`, documented for a driver already in the driver store). `UpdateDriverForPlugAndPlayDevicesW`, devcon's
//!    call, wants the package's own folder rather than a system one (its documentation), and would also touch every other
//!    device with that hardware ID. Then up to [`DRIVER_WAIT`] for the device to be working, and [`judge_install`]: it must
//!    be working on the chosen package (its INF and version, as in step 1), whatever DiInstallDevice returned, as Plug and Play
//!    may have put a driver on it by itself meanwhile. Otherwise it is removed again, so no device without the right driver is
//!    left to read as that variant, and the switch fails; unless Windows asked for a restart to finish, which leaves it.
//! 5. Up to [`MICS_WAIT`] for Windows to make the variant's microphones, by name ([`check_mics`]): each there (active, or
//!    disabled in the Sound panel) and not one of the endpoints noted in step 3, and no other endpoint of ours still active
//!    (a removed device's, going). A microphone that doesn't come is an error; another endpoint still there is reported.
//!    A switch that only removes devices (it keeps one) checks the same, with nothing noted.
//!
//! A failure after the old devices are removed leaves none: the steps say which were removed, and a second run of the switch
//! creates the device afresh.

use std::fmt;
use std::mem::size_of;
use std::time::{Duration, Instant};

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_DEVNODE_STATUS_FLAGS, CM_Get_DevNode_Status, CM_PROB, CM_PROB_NEED_RESTART, DICD_GENERATE_ID, DIF_REGISTERDEVICE, DIGCF_ALLCLASSES,
    DIGCF_PRESENT, DIIDFLAG_NOFINISHINSTALLUI, DN_HAS_PROBLEM, DN_STARTED, DiInstallDevice, DiUninstallDevice, GUID_DEVCLASS_MEDIA,
    HDEVINFO, INFINFO_INF_NAME_IS_ABSOLUTE, SETUP_DI_REGISTRY_PROPERTY, SP_DEVINFO_DATA, SP_DRVINFO_DATA_V2_W, SP_DRVINFO_DETAIL_DATA_W,
    SP_INF_INFORMATION, SP_ORIGINAL_FILE_INFO_W, SPDIT_COMPATDRIVER, SPDRP_HARDWAREID, SetupDiBuildDriverInfoList,
    SetupDiCallClassInstaller, SetupDiCreateDeviceInfoList, SetupDiCreateDeviceInfoW, SetupDiEnumDeviceInfo, SetupDiEnumDriverInfoW,
    SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW, SetupDiGetDevicePropertyW, SetupDiGetDeviceRegistryPropertyW,
    SetupDiGetDriverInfoDetailW, SetupDiSetDeviceRegistryPropertyW, SetupGetInfDriverStoreLocationW, SetupGetInfInformationW,
    SetupQueryInfOriginalFileInformationW,
};
use windows::Win32::Devices::Properties::{DEVPKEY_Device_DriverInfPath, DEVPKEY_Device_DriverVersion, DEVPROPTYPE};
use windows::Win32::Foundation::{
    DEVPROPKEY, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_ITEMS, GetLastError, HANDLE, HWND, WAIT_ABANDONED, WAIT_EVENT, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows::Win32::System::Threading::{
    CreateMutexExW, MUTEX_MODIFY_STATE, ReleaseMutex, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
};
use windows::core::{BOOL, HSTRING, Owned, PCWSTR, w};

use crate::sessions::{self, CaptureEndpoint};
use crate::win_error::{WinError, configret_name, problem_name, win32_code};

/// What every endpoint of the driver has in its name ("Microphone (AudioCodec Device)"): the device's description.
pub const DEVICE_NAME: &str = "AudioCodec Device";
/// The low-latency microphone's endpoint (IAudioClient3's 2.67-10 ms periods).
pub const MICROPHONE: &str = "Microphone (AudioCodec Device)";
/// The call microphone's endpoint (a 5 ms default period, for apps that ask for none).
pub const CALL_MICROPHONE: &str = "Call Microphone (AudioCodec Device)";
/// The INF's file name as built; the driver store publishes it as `oemNN.inf` and remembers this one.
pub const INF_NAME: &str = "audiocodec.inf";

/// The named mutex a switch holds from start to end, across sessions.
pub const LOCK_NAME: &str = r"Global\noise-filter-mic-variant";
/// How long a switch waits for another to end: a switch takes a few seconds, and at most [`DRIVER_WAIT`] and [`MICS_WAIT`]
/// more.
pub const LOCK_WAIT: Duration = Duration::from_secs(60);
/// How long a new device has, after DiInstallDevice, to be working.
pub const DRIVER_WAIT: Duration = Duration::from_secs(5);
/// How long Windows has, once the device works, to make its microphones.
pub const MICS_WAIT: Duration = Duration::from_secs(15);
/// How often the two waits look.
const POLL: Duration = Duration::from_millis(250);

/// One of the driver's three variants, by the hardware ID its root device has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Variant {
    /// `ROOT\AudioCodec`: both microphones.
    Both,
    /// `ROOT\AudioCodecCall`: the call microphone only.
    Call,
    /// `ROOT\AudioCodecLowLatency`: the low-latency microphone only.
    LowLatency,
}

impl Variant {
    pub const ALL: [Variant; 3] = [Variant::Both, Variant::Call, Variant::LowLatency];

    /// The hardware ID the INF lists for it.
    pub fn hardware_id(self) -> &'static str {
        match self {
            Variant::Both => r"ROOT\AudioCodec",
            Variant::Call => r"ROOT\AudioCodecCall",
            Variant::LowLatency => r"ROOT\AudioCodecLowLatency",
        }
    }

    /// The variant a hardware ID names: the whole ID, any case (Windows compares hardware IDs case-insensitively).
    pub fn from_hardware_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.hardware_id().eq_ignore_ascii_case(id.trim()))
    }

    /// Its command-line word: `both`, `call`, `low-latency`.
    pub fn arg(self) -> &'static str {
        match self {
            Variant::Both => "both",
            Variant::Call => "call",
            Variant::LowLatency => "low-latency",
        }
    }

    /// The variant a command-line word names, any case.
    pub fn from_arg(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.arg().eq_ignore_ascii_case(s.trim()))
    }

    /// "both microphones", "the call microphone only", "the low-latency microphone only".
    pub fn describe(self) -> &'static str {
        match self {
            Variant::Both => "both microphones",
            Variant::Call => "the call microphone only",
            Variant::LowLatency => "the low-latency microphone only",
        }
    }

    /// The microphone endpoints it makes.
    pub fn microphones(self) -> &'static [&'static str] {
        match self {
            Variant::Both => &[MICROPHONE, CALL_MICROPHONE],
            Variant::Call => &[CALL_MICROPHONE],
            Variant::LowLatency => &[MICROPHONE],
        }
    }

    /// `"Microphone (AudioCodec Device)" and "Call Microphone (AudioCodec Device)"`.
    pub fn microphones_text(self) -> String {
        quoted(self.microphones().iter().copied())
    }
}

/// Names in quotes, joined by "and": `"a" and "b"`.
pub fn quoted<'a>(names: impl Iterator<Item = &'a str>) -> String {
    names.map(|n| format!("\"{n}\"")).collect::<Vec<_>>().join(" and ")
}

/// Our variant among a device's hardware IDs, if any.
pub fn variant_of(hardware_ids: &[String]) -> Option<Variant> {
    hardware_ids.iter().find_map(|id| Variant::from_hardware_id(id))
}

/// A device's state, from CM_Get_DevNode_Status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    /// Started, no problem.
    Working,
    /// No problem code, but not started (yet).
    NotStarted,
    /// Device Manager's "Code N" (`CM_PROB_*`): 22 is disabled, 28 no driver installed.
    Problem(u32),
    /// CM_Get_DevNode_Status itself failed with this CONFIGRET.
    Unknown(u32),
}

impl Health {
    fn of(configret: u32, status: u32, problem: u32) -> Self {
        if configret != 0 {
            Health::Unknown(configret)
        } else if status & DN_HAS_PROBLEM.0 != 0 {
            Health::Problem(problem)
        } else if status & DN_STARTED.0 != 0 {
            Health::Working
        } else {
            Health::NotStarted
        }
    }

    pub fn describe(self) -> String {
        match self {
            Health::Working => "working".into(),
            Health::NotStarted => "not started".into(),
            Health::Problem(22) => "disabled (Code 22)".into(),
            Health::Problem(28) => "no driver installed (Code 28)".into(),
            Health::Problem(p) => match problem_name(p) {
                Some(name) => format!("a problem: Code {p} ({name})"),
                None => format!("a problem: Code {p}"),
            },
            Health::Unknown(cr) => match configret_name(cr) {
                Some(name) => format!("state unknown (CM_Get_DevNode_Status: {name}, {cr})"),
                None => format!("state unknown (CM_Get_DevNode_Status: CONFIGRET {cr})"),
            },
        }
    }
}

/// One of our root devices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OurDevice {
    /// `ROOT\MEDIA\0003`.
    pub instance_id: String,
    pub variant: Variant,
    pub health: Health,
    /// The driver's published INF (`oem12.inf`), if it has a driver.
    pub inf: Option<String>,
    pub driver_version: Option<String>,
    /// The newest of our packages in the driver store for its hardware ID (the compatible-driver search on the device, as
    /// [`choose`] ranks them): `None` if there's none; the error if the search failed.
    pub newest: Result<Option<Candidate>, WinError>,
}

impl OurDevice {
    /// "ROOT\MEDIA\0003: both microphones (ROOT\AudioCodec), working, driver oem12.inf 1.0.0.0".
    pub fn describe(&self) -> String {
        format!(
            "{}: {} ({}), {}, {}",
            self.instance_id,
            self.variant.describe(),
            self.variant.hardware_id(),
            self.health.describe(),
            driver_text(self.inf.as_deref(), self.driver_version.as_deref())
        )
    }

    /// Whether it runs the package `c` (see [`runs`]).
    pub fn runs(&self, c: &Candidate) -> bool {
        runs(self.inf.as_deref(), self.driver_version.as_deref(), c)
    }

    /// Whether it runs the newest of our packages in the driver store.
    pub fn is_current(&self) -> bool {
        matches!(&self.newest, Ok(Some(c)) if self.runs(c))
    }

    /// Its driver against the driver store's newest: "it runs the newest AudioCodec package in the driver store", or what the
    /// newest is.
    pub fn package_note(&self) -> String {
        match &self.newest {
            Ok(Some(c)) if self.runs(c) => "it runs the newest AudioCodec package in the driver store".into(),
            Ok(Some(c)) => format!("it doesn't run the newest AudioCodec package in the driver store, {}", c.describe()),
            Ok(None) => format!("no AudioCodec package in the driver store has {}", self.variant.hardware_id()),
            Err(e) => format!("the driver store's packages for it couldn't be read: {e}"),
        }
    }
}

/// "driver oem12.inf 1.0.0.0", "no driver".
fn driver_text(inf: Option<&str>, version: Option<&str>) -> String {
    match (inf, version) {
        (Some(inf), Some(v)) => format!("driver {inf} {v}"),
        (Some(inf), None) => format!("driver {inf}"),
        (None, _) => "no driver".into(),
    }
}

/// Whether a device whose driver Windows reports as `inf` (`DEVPKEY_Device_DriverInfPath`: `oem12.inf`) and `version`
/// (`DEVPKEY_Device_DriverVersion`: "12.40.56.342") runs the package `c`: the same published INF, any case, and the same
/// version. The version too, because the driver store reuses an `oemNN.inf` name once its package is deleted.
pub fn runs(inf: Option<&str>, version: Option<&str>, c: &Candidate) -> bool {
    let (Some(inf), Some(version)) = (inf, version) else { return false };
    file_name(inf.trim()).eq_ignore_ascii_case(file_name(&c.inf)) && parse_version(version) == Some(c.version)
}

/// A path's file name: what follows the last `\` or `/`.
fn file_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// The present devices with one of our hardware IDs, each with the driver store's newest package for it. Any user may call
/// it.
pub fn present() -> Result<Vec<OurDevice>, WinError> {
    let set = DeviceSet::all_present()?;
    Ok(set.ours()?.into_iter().map(|(_, d)| d).collect())
}

/// What a switch to `target` does with the devices there now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// A device of the target variant, working on the newest package, kept as it is.
    pub keep: Option<usize>,
    /// Every other device of ours, removed.
    pub remove: Vec<usize>,
    /// No such device to keep: create one.
    pub create: bool,
}

impl Plan {
    /// The target variant is already the only one there, working on the newest package.
    pub fn is_nothing(&self) -> bool {
        self.remove.is_empty() && !self.create
    }
}

/// Keeps the first device of the target variant that is working and runs the newest of our packages in the driver store,
/// and removes the rest; creates one when there's none such. A disabled or broken device of the target variant is replaced
/// (which also enables it), and so is one on an older package (an upgrade), or one whose packages couldn't be read.
pub fn plan(devices: &[OurDevice], target: Variant) -> Plan {
    let keep = devices.iter().position(|d| d.variant == target && d.health == Health::Working && d.is_current());
    let remove = (0..devices.len()).filter(|&i| Some(i) != keep).collect();
    Plan { keep, remove, create: keep.is_none() }
}

/// What [`switch`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The target variant was already the only one there, working on the newest package: nothing changed.
    Nothing,
    /// Switched. `restart`: Windows asked for a restart to finish the install, so the microphones weren't waited for.
    Switched { restart: bool },
}

/// Why [`switch`] stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SwitchError {
    /// A Windows call failed.
    Windows(WinError),
    /// No driver in the driver store has the target's hardware ID with our INF's name. Nothing was removed.
    NoDriver { hardware_id: &'static str },
    /// Windows will finish removing `device` at the next restart, so the new device wasn't created.
    RestartFirst { device: String },
    /// Another switch held the lock for [`LOCK_WAIT`]. Nothing was changed.
    Busy,
    /// Our capture endpoints couldn't be read before the change, and the check after it needs them. Nothing was removed.
    NoEndpoints(WinError),
    /// The new device wasn't working on the chosen package, so it was removed again. The old devices were removed before it
    /// was made.
    Install(Box<NotKept>),
    /// The device works on the newest package, but Windows hadn't made these microphones after [`MICS_WAIT`]. `seen`: our
    /// endpoints at the last look; `read`: the last look's error, if it failed.
    Mics { missing: Vec<&'static str>, seen: Vec<String>, read: Option<WinError> },
}

/// A new device that wasn't kept ([`SwitchError::Install`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotKept {
    /// `ROOT\MEDIA\0004`.
    pub device: String,
    /// The package it was given, described.
    pub chosen: String,
    pub failure: InstallFailure,
    /// How removing it again went.
    pub removed: Result<(), WinError>,
}

impl From<WinError> for SwitchError {
    fn from(e: WinError) -> Self {
        SwitchError::Windows(e)
    }
}

impl fmt::Display for SwitchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SwitchError::Windows(e) => e.fmt(f),
            SwitchError::NoDriver { hardware_id } => write!(
                f,
                "no driver in the driver store has {hardware_id} with the INF {INF_NAME}: install the driver package that has the \
                 three variants (pnputil /add-driver AudioCodec.inf, from an administrator prompt; pnputil /enum-drivers lists \
                 what's there). Nothing was removed."
            ),
            SwitchError::RestartFirst { device } => {
                write!(f, "Windows will finish removing {device} when it restarts: restart Windows, then run the switch again")
            }
            SwitchError::Busy => write!(
                f,
                "another switch has held {LOCK_NAME} for {} s: let it end, then run this one again (it reads the devices afresh). \
                 Nothing was changed.",
                LOCK_WAIT.as_secs()
            ),
            SwitchError::NoEndpoints(e) => write!(
                f,
                "the audio endpoints couldn't be read ({e}), and the switch checks the new microphones with them: nothing was \
                 removed. Is the Windows Audio Endpoint Builder service running?"
            ),
            SwitchError::Install(not_kept) => {
                let NotKept { device, chosen, failure, removed } = not_kept.as_ref();
                write!(f, "the new device {device} isn't working on the chosen driver ({chosen}): {failure}. ")?;
                match removed {
                    Ok(()) => write!(
                        f,
                        "It was removed again, and the old devices before it, so none of ours is installed now: run the switch \
                         again, or see what's there with `noise-filter mic variant`."
                    ),
                    Err(e) => write!(
                        f,
                        "Removing it again failed too ({e}): remove it in Device Manager, or with pnputil /remove-device {device}, \
                         then run the switch again."
                    ),
                }
            }
            SwitchError::Mics { missing, seen, read } => {
                let seen = if seen.is_empty() { "none".to_string() } else { quoted(seen.iter().map(String::as_str)) };
                write!(
                    f,
                    "the device is working on the newest package, but after {} s Windows hasn't made {} (ours there: {seen})",
                    MICS_WAIT.as_secs(),
                    quoted(missing.iter().copied())
                )?;
                if let Some(e) = read {
                    write!(f, "; the last look at the endpoints failed: {e}")?;
                }
                write!(f, ". The Sound panel's Recording tab and `noise-filter mic variant` show what's there now.")
            }
        }
    }
}

impl std::error::Error for SwitchError {}

/// Switches to `target` (see the module's docs for the steps), reporting each step and each Windows error through `say`, as
/// it happens. Needs elevation: without it, the lock or the first change fails with ERROR_ACCESS_DENIED.
pub fn switch(target: Variant, say: &mut dyn FnMut(&str)) -> Result<Outcome, SwitchError> {
    let _lock = SwitchLock::take(LOCK_NAME, LOCK_WAIT, say)?;
    let ids: Vec<&str> = Variant::ALL.iter().map(|v| v.hardware_id()).collect();
    say(&format!("Looking for the AudioCodec device ({})", ids.join(", ")));
    let set = DeviceSet::all_present()?;
    let ours = set.ours()?;
    if ours.is_empty() {
        say("  none is installed");
    }
    for (_, d) in &ours {
        say(&format!("  {}", d.describe()));
        say(&format!("    ({})", d.package_note()));
    }
    let devices: Vec<OurDevice> = ours.iter().map(|(_, d)| d.clone()).collect();
    let plan = plan(&devices, target);
    if plan.is_nothing() {
        say(&format!("Already {} ({}) on the newest package: nothing to do.", target.describe(), target.hardware_id()));
        return Ok(Outcome::Nothing);
    }
    if let Some(k) = plan.keep {
        say(&format!("Keeping {}", devices[k].instance_id));
    }
    // A new device's microphones are endpoints that weren't there before; read them before anything changes, as the check
    // after needs them
    let before = if plan.create { capture_endpoint_ids()? } else { Vec::new() };
    // The new device and its driver before anything is removed: no driver, no change
    let pending = if plan.create { Some(Pending::prepare(target, say)?) } else { None };
    for &i in &plan.remove {
        let (data, d) = &ours[i];
        say(&format!("Removing {} ({}) with DiUninstallDevice", d.instance_id, d.variant.describe()));
        let mut restart = BOOL(0);
        // SAFETY: `data` is an element of `set`, which is alive; `restart` is a live out-pointer. No window.
        unsafe { DiUninstallDevice(HWND::default(), set.get(), data, 0, Some(&mut restart)) }.map_err(WinError::of("DiUninstallDevice"))?;
        if restart.as_bool() {
            say("  removed, but Windows needs a restart to finish");
            return Err(SwitchError::RestartFirst { device: d.instance_id.clone() });
        }
        say("  removed");
    }
    if let Some(p) = pending
        && p.install(say)? == Installed::Restart
    {
        say("  Windows needs a restart to finish the install: the microphones come after it (`noise-filter mic variant` shows them).");
        return Ok(Outcome::Switched { restart: true });
    }
    wait_for_mics(target, &before, say)?;
    Ok(Outcome::Switched { restart: false })
}

/// The ids of our capture endpoints there now, active or disabled.
fn capture_endpoint_ids() -> Result<Vec<String>, SwitchError> {
    let endpoints = sessions::capture_endpoints().map_err(SwitchError::NoEndpoints)?;
    Ok(endpoints.into_iter().map(|e| e.id).collect())
}

/// Up to [`MICS_WAIT`] for Windows to make `target`'s microphones ([`check_mics`]; `before`: the endpoints there before the
/// change), saying which came.
fn wait_for_mics(target: Variant, before: &[String], say: &mut dyn FnMut(&str)) -> Result<(), SwitchError> {
    say(&format!("Waiting for Windows to make {} (up to {} s)", target.microphones_text(), MICS_WAIT.as_secs()));
    let start = Instant::now();
    // The last look that read the endpoints, and the last look's error, if it failed
    let mut last = None::<MicCheck>;
    let failed = loop {
        let failed = match sessions::capture_endpoints() {
            Ok(now) => {
                last = Some(check_mics(target, &now, before));
                None
            }
            Err(e) => Some(e),
        };
        let done = failed.is_none() && last.as_ref().is_some_and(MicCheck::is_done);
        if done || start.elapsed() >= MICS_WAIT {
            break failed;
        }
        std::thread::sleep(POLL);
    };
    match last {
        Some(check) if check.missing.is_empty() => {
            for &(mic, active) in &check.found {
                say(&format!("  \"{mic}\": {}", if active { "there" } else { "there, disabled in the Sound panel's Recording tab" }));
            }
            if !check.extra.is_empty() {
                let extra = quoted(check.extra.iter().map(String::as_str));
                say(&format!("  also still active: {extra} (a removed device's, going, or another device of the same name)"));
            }
            say(&format!("  after {:.1} s", start.elapsed().as_secs_f64()));
            Ok(())
        }
        Some(check) => Err(SwitchError::Mics { missing: check.missing, seen: check.seen, read: failed }),
        None => Err(SwitchError::Mics { missing: target.microphones().to_vec(), seen: Vec::new(), read: failed }),
    }
}

/// What Windows has of a variant's microphones after a switch ([`check_mics`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MicCheck {
    /// Each microphone there, and whether it's active (if not, it's disabled in the Sound panel).
    pub found: Vec<(&'static str, bool)>,
    /// The microphones not there.
    pub missing: Vec<&'static str>,
    /// Our other active endpoints: one there before the change (a removed device's, going), a second of the same name, or one
    /// the variant doesn't have.
    pub extra: Vec<String>,
    /// Every endpoint of ours, by name ("(disabled)" after a disabled one's).
    pub seen: Vec<String>,
}

impl MicCheck {
    /// Every microphone there, and nothing else of ours active.
    pub fn is_done(&self) -> bool {
        self.missing.is_empty() && self.extra.is_empty()
    }
}

/// Judges our capture endpoints `now` against `target`'s microphones: each must be there, by its name (any case), and not
/// one of `before` (the ids there before the change: a new device's endpoints are new); an active one is taken before a
/// disabled one (disabled in the Sound panel, it is there still). The endpoints left over that are active are `extra`.
pub fn check_mics(target: Variant, now: &[CaptureEndpoint], before: &[String]) -> MicCheck {
    let is_new = |e: &CaptureEndpoint| !before.iter().any(|id| id.eq_ignore_ascii_case(&e.id));
    let seen = now.iter().map(|e| if e.active { e.name.clone() } else { format!("{} (disabled)", e.name) }).collect();
    let mut check = MicCheck { seen, ..MicCheck::default() };
    let mut used = vec![false; now.len()];
    for &mic in target.microphones() {
        let find = |active: bool| {
            (0..now.len()).find(|&i| !used[i] && now[i].active == active && is_new(&now[i]) && now[i].name.trim().eq_ignore_ascii_case(mic))
        };
        let pick = find(true).or_else(|| find(false));
        match pick {
            Some(i) => {
                used[i] = true;
                check.found.push((mic, now[i].active));
            }
            None => check.missing.push(mic),
        }
    }
    check.extra = (0..now.len()).filter(|&i| !used[i] && now[i].active).map(|i| now[i].name.clone()).collect();
    check
}

/// How a wait for the lock ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LockWait {
    Taken,
    /// Its last holder's thread ended without releasing it: ours now, as if taken.
    Abandoned,
    /// Still held by another at the end of the wait.
    Busy,
    Failed,
}

fn lock_wait(event: WAIT_EVENT) -> LockWait {
    match event {
        e if e == WAIT_OBJECT_0 => LockWait::Taken,
        e if e == WAIT_ABANDONED => LockWait::Abandoned,
        e if e == WAIT_TIMEOUT => LockWait::Busy,
        _ => LockWait::Failed,
    }
}

/// The switch's lock, a named mutex; released when dropped.
struct SwitchLock(Owned<HANDLE>);

impl SwitchLock {
    /// Takes the mutex `name`, waiting up to `wait` for another holder.
    fn take(name: &str, wait: Duration, say: &mut dyn FnMut(&str)) -> Result<Self, SwitchError> {
        let name = HSTRING::from(name);
        // Enough to wait on it and release it
        let access = SYNCHRONIZATION_SYNCHRONIZE.0 | MUTEX_MODIFY_STATE.0;
        // SAFETY: no security attributes, so the creator's default DACL (an elevated administrator's lets Administrators and
        // SYSTEM in); a NUL-terminated name; not owned on creation, so it is waited for below whoever made it.
        let handle = unsafe { CreateMutexExW(None, &name, 0, access) }.map_err(WinError::of("CreateMutexExW"))?;
        // SAFETY: a valid handle that nothing else owns; Owned closes it once.
        let handle = unsafe { Owned::new(handle) };
        // SAFETY: our handle; no wait.
        let mut event = unsafe { WaitForSingleObject(*handle, 0) };
        if event == WAIT_TIMEOUT {
            say(&format!("Another switch is running: waiting for it to end (up to {} s)", wait.as_secs()));
            // SAFETY: as above.
            event = unsafe { WaitForSingleObject(*handle, u32::try_from(wait.as_millis()).unwrap_or(u32::MAX - 1)) };
        }
        match lock_wait(event) {
            LockWait::Taken => Ok(Self(handle)),
            LockWait::Abandoned => {
                say("(The last switch's process ended in the middle of its switch: this one reads the devices afresh.)");
                Ok(Self(handle))
            }
            LockWait::Busy => Err(SwitchError::Busy),
            // SAFETY: no arguments; read straight after the failed wait.
            LockWait::Failed => Err(WinError { call: "WaitForSingleObject", code: unsafe { GetLastError() }.0 }.into()),
        }
    }
}

impl Drop for SwitchLock {
    fn drop(&mut self) {
        // SAFETY: our handle, to a mutex this thread owns (take returns one only then); Owned closes the handle after this.
        let _ = unsafe { ReleaseMutex(*self.0) };
    }
}

/// A device information set, destroyed on drop (which also deletes an element in it that was never registered).
struct DeviceSet(Owned<HDEVINFO>);

impl DeviceSet {
    /// Every present device, of every class.
    fn all_present() -> Result<Self, WinError> {
        // SAFETY: no class, enumerator or window; the set returned is ours.
        let set = unsafe { SetupDiGetClassDevsW(None, PCWSTR::null(), None, DIGCF_ALLCLASSES | DIGCF_PRESENT) }
            .map_err(WinError::of("SetupDiGetClassDevsW"))?;
        // SAFETY: a valid set that nothing else owns; Owned destroys it once.
        Ok(Self(unsafe { Owned::new(set) }))
    }

    /// An empty set for class MEDIA, for a new device.
    fn new_media() -> Result<Self, WinError> {
        // SAFETY: the class GUID is a constant that outlives the call; no window.
        let set = unsafe { SetupDiCreateDeviceInfoList(Some(&GUID_DEVCLASS_MEDIA), None) }
            .map_err(WinError::of("SetupDiCreateDeviceInfoList"))?;
        // SAFETY: as in all_present.
        Ok(Self(unsafe { Owned::new(set) }))
    }

    fn get(&self) -> HDEVINFO {
        *self.0
    }

    /// The devices in the set with one of our hardware IDs, with their element data.
    fn ours(&self) -> Result<Vec<(SP_DEVINFO_DATA, OurDevice)>, WinError> {
        let mut out = Vec::new();
        for index in 0.. {
            let mut data = SP_DEVINFO_DATA { cbSize: size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
            // SAFETY: our set; `data` is a live out-structure with its size set.
            match unsafe { SetupDiEnumDeviceInfo(self.get(), index, &mut data) } {
                Ok(()) => {}
                Err(e) if win32_code(&e) == ERROR_NO_MORE_ITEMS.0 => break,
                Err(e) => return Err(WinError::new("SetupDiEnumDeviceInfo", &e)),
            }
            // A device whose IDs can't be read isn't one we could switch anyway
            let Some(variant) = self.registry_multi_sz(&data, SPDRP_HARDWAREID).ok().and_then(|ids| variant_of(&ids)) else {
                continue;
            };
            let Bound { health, inf, driver_version } = self.bound(&data);
            let device = OurDevice {
                instance_id: self.instance_id(&data)?,
                variant,
                health,
                inf,
                driver_version,
                newest: self.newest_for(&mut data),
            };
            out.push((data, device));
        }
        Ok(out)
    }

    /// An element's state and driver, as Windows reports them now.
    fn bound(&self, data: &SP_DEVINFO_DATA) -> Bound {
        Bound {
            health: health(data.DevInst),
            inf: self.string_property(data, &DEVPKEY_Device_DriverInfPath),
            driver_version: self.string_property(data, &DEVPKEY_Device_DriverVersion),
        }
    }

    /// The newest of our packages compatible with an element: the compatible-driver search on it (which only reads the
    /// driver store), then [`choose`].
    fn newest_for(&self, data: &mut SP_DEVINFO_DATA) -> Result<Option<Candidate>, WinError> {
        // SAFETY: an element of our set.
        unsafe { SetupDiBuildDriverInfoList(self.get(), Some(data), SPDIT_COMPATDRIVER) }
            .map_err(WinError::of("SetupDiBuildDriverInfoList"))?;
        let candidates: Vec<Candidate> = self.compat_drivers(data)?.into_iter().map(|(_, c)| c).collect();
        Ok(choose(&candidates).map(|i| candidates[i].clone()))
    }

    fn instance_id(&self, data: &SP_DEVINFO_DATA) -> Result<String, WinError> {
        // Device instance IDs are at most 200 characters (MAX_DEVICE_ID_LEN)
        let mut buf = [0u16; 512];
        // SAFETY: an element of our set; the buffer's length goes with it.
        unsafe { SetupDiGetDeviceInstanceIdW(self.get(), data, Some(&mut buf[..]), None) }
            .map_err(WinError::of("SetupDiGetDeviceInstanceIdW"))?;
        Ok(from_wide(&buf))
    }

    /// A REG_MULTI_SZ registry property (`SPDRP_HARDWAREID`).
    fn registry_multi_sz(&self, data: &SP_DEVINFO_DATA, property: SETUP_DI_REGISTRY_PROPERTY) -> Result<Vec<String>, WinError> {
        let mut buf = vec![0u8; 512];
        loop {
            let mut needed = 0u32;
            // SAFETY: an element of our set; the buffer's length goes with it; `needed` is a live out-pointer.
            match unsafe {
                SetupDiGetDeviceRegistryPropertyW(self.get(), data, property, None, Some(buf.as_mut_slice()), Some(&mut needed))
            } {
                Ok(()) => return Ok(parse_multi_sz(&wide_of_bytes(&buf))),
                Err(e) if win32_code(&e) == ERROR_INSUFFICIENT_BUFFER.0 && needed as usize > buf.len() => buf.resize(needed as usize, 0),
                Err(e) => return Err(WinError::new("SetupDiGetDeviceRegistryPropertyW", &e)),
            }
        }
    }

    /// A string device property, `None` when it isn't set (a device without a driver has no INF path).
    fn string_property(&self, data: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Option<String> {
        let mut buf = vec![0u8; 1024];
        let mut kind = DEVPROPTYPE::default();
        // SAFETY: an element of our set; `key` and `kind` outlive the call; the buffer's length goes with it.
        unsafe { SetupDiGetDevicePropertyW(self.get(), data, key, &mut kind, Some(buf.as_mut_slice()), None, 0) }.ok()?;
        let s = from_wide(&wide_of_bytes(&buf));
        (!s.is_empty()).then_some(s)
    }

    /// The compatible drivers found for `data` (after SetupDiBuildDriverInfoList), with each one's INF.
    fn compat_drivers(&self, data: &SP_DEVINFO_DATA) -> Result<Vec<(SP_DRVINFO_DATA_V2_W, Candidate)>, WinError> {
        let mut out = Vec::new();
        for index in 0.. {
            // SAFETY: a plain C structure, for which all zeros is a valid value; its size is set next.
            let mut node: SP_DRVINFO_DATA_V2_W = unsafe { std::mem::zeroed() };
            node.cbSize = size_of::<SP_DRVINFO_DATA_V2_W>() as u32;
            // SAFETY: an element of our set with a compatible-driver list; `node` is a live out-structure with its size set.
            match unsafe { SetupDiEnumDriverInfoW(self.get(), Some(data), SPDIT_COMPATDRIVER, index, &mut node) } {
                Ok(()) => {}
                Err(e) if win32_code(&e) == ERROR_NO_MORE_ITEMS.0 => break,
                Err(e) => return Err(WinError::new("SetupDiEnumDriverInfoW", &e)),
            }
            let inf = self.driver_inf(data, &node)?;
            let original = original_inf_name(&inf).map_err(|e| e.to_string());
            let candidate = Candidate {
                original,
                inf,
                provider: from_wide(&node.ProviderName),
                description: from_wide(&node.Description),
                date: (u64::from(node.DriverDate.dwHighDateTime) << 32) | u64::from(node.DriverDate.dwLowDateTime),
                version: node.DriverVersion,
            };
            out.push((node, candidate));
        }
        Ok(out)
    }

    /// The INF a driver node comes from (`C:\Windows\INF\oem12.inf`).
    fn driver_inf(&self, data: &SP_DEVINFO_DATA, node: &SP_DRVINFO_DATA_V2_W) -> Result<String, WinError> {
        // 8 KB, 8-byte aligned: the fixed part and the node's hardware IDs. Only the fixed part is read; if the IDs don't fit, the
        // call fills the fixed part and fails with ERROR_INSUFFICIENT_BUFFER.
        let mut buf = vec![0u64; 1024];
        let detail = buf.as_mut_ptr().cast::<SP_DRVINFO_DETAIL_DATA_W>();
        // SAFETY: the buffer is larger than the structure and aligned for it.
        unsafe { (*detail).cbSize = size_of::<SP_DRVINFO_DETAIL_DATA_W>() as u32 };
        let bytes = (buf.len() * size_of::<u64>()) as u32;
        // SAFETY: our set, element and node; `detail` points at `bytes` writable bytes.
        match unsafe { SetupDiGetDriverInfoDetailW(self.get(), Some(data), node, Some(detail), bytes, None) } {
            Ok(()) => {}
            Err(e) if win32_code(&e) == ERROR_INSUFFICIENT_BUFFER.0 => {}
            Err(e) => return Err(WinError::new("SetupDiGetDriverInfoDetailW", &e)),
        }
        // SAFETY: the call filled the fixed part; the field is read by value, through the raw pointer.
        let name = unsafe { (&raw const (*detail).InfFileName).read() };
        Ok(from_wide(&name))
    }
}

/// A compatible driver the search found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The published INF: `C:\Windows\INF\oem12.inf`.
    pub inf: String,
    /// The INF's name before the driver store published it (`audiocodec.inf`), or why it couldn't be read.
    pub original: Result<String, String>,
    pub provider: String,
    pub description: String,
    /// The INF's DriverVer date, as a FILETIME's 100 ns ticks since 1601.
    pub date: u64,
    /// The INF's DriverVer version, four 16-bit parts.
    pub version: u64,
}

impl Candidate {
    /// Whether it's our INF (by its original name).
    pub fn is_ours(&self) -> bool {
        self.original.as_deref().is_ok_and(|n| n.eq_ignore_ascii_case(INF_NAME))
    }

    /// "C:\Windows\INF\oem12.inf (audiocodec.inf), VS_Microsoft, 1.0.0.0 of 2026-11-23".
    pub fn describe(&self) -> String {
        let original = match &self.original {
            Ok(name) => name.clone(),
            Err(why) => format!("original name unknown: {why}"),
        };
        format!("{} ({original}), {}, {} of {}", self.inf, self.provider, version_text(self.version), date_text(self.date))
    }
}

/// Our driver among the candidates: the newest by date, then by version (as Plug and Play ranks equal matches); the first of
/// those on a tie.
pub fn choose(candidates: &[Candidate]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, c) in candidates.iter().enumerate().filter(|(_, c)| c.is_ours()) {
        if best.is_none_or(|b| (c.date, c.version) > (candidates[b].date, candidates[b].version)) {
            best = Some(i);
        }
    }
    best
}

/// A device's state and driver, as Windows reports them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bound {
    pub health: Health,
    /// `DEVPKEY_Device_DriverInfPath`: `oem12.inf`.
    pub inf: Option<String>,
    /// `DEVPKEY_Device_DriverVersion`: "12.40.56.342".
    pub driver_version: Option<String>,
}

impl Bound {
    /// "working, driver oem12.inf 12.40.56.342".
    pub fn describe(&self) -> String {
        format!("{}, {}", self.health.describe(), driver_text(self.inf.as_deref(), self.driver_version.as_deref()))
    }
}

/// What a new device's install came to, when it's kept ([`judge_install`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Installed {
    /// Working, on the chosen package.
    Working,
    /// Windows asked for a restart to finish: the device is left for it.
    Restart,
}

/// Why a new device isn't kept ([`judge_install`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallFailure {
    /// DiInstallDevice failed, and the device (`now`) isn't working on the chosen package.
    Call { error: WinError, now: Bound },
    /// DiInstallDevice succeeded, but the device (`now`) wasn't working [`DRIVER_WAIT`] later.
    NotWorking { now: Bound },
    /// The device (`now`) is working, on another package than the chosen one: Plug and Play's own choice, say, after
    /// DiInstallDevice failed (`error`).
    OtherPackage { error: Option<WinError>, now: Bound },
}

impl fmt::Display for InstallFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InstallFailure::Call { error, now } => write!(f, "{error}; the device now: {}", now.describe()),
            InstallFailure::NotWorking { now } => {
                write!(f, "{} s after DiInstallDevice, the device: {}", DRIVER_WAIT.as_secs(), now.describe())
            }
            InstallFailure::OtherPackage { error, now } => {
                if let Some(e) = error {
                    write!(f, "{e}; ")?;
                }
                write!(f, "the device now: {}, another package (Plug and Play's own choice)", now.describe())
            }
        }
    }
}

/// Judges a new device's install: `call` is what DiInstallDevice returned, `restart` whether it asked for a restart, `now`
/// the device afterwards, `chosen` the package it was given. Kept only when working on the chosen package (whatever the call
/// returned: Plug and Play may have put the same driver on it by itself), or when Windows asked for a restart to finish.
pub fn judge_install(call: Result<(), &WinError>, restart: bool, now: &Bound, chosen: &Candidate) -> Result<Installed, InstallFailure> {
    let on_chosen = runs(now.inf.as_deref(), now.driver_version.as_deref(), chosen);
    let needs_restart = restart || (on_chosen && now.health == Health::Problem(CM_PROB_NEED_RESTART.0));
    match call {
        Ok(()) if needs_restart => Ok(Installed::Restart),
        _ if now.health == Health::Working && on_chosen => Ok(Installed::Working),
        _ if now.health == Health::Working => Err(InstallFailure::OtherPackage { error: call.err().cloned(), now: now.clone() }),
        Err(e) => Err(InstallFailure::Call { error: e.clone(), now: now.clone() }),
        Ok(()) => Err(InstallFailure::NotWorking { now: now.clone() }),
    }
}

/// The new device, not yet registered, with its driver chosen.
struct Pending {
    set: DeviceSet,
    data: SP_DEVINFO_DATA,
    node: SP_DRVINFO_DATA_V2_W,
    instance_id: String,
    chosen: Candidate,
}

impl Pending {
    /// Creates the element (a phantom until registered), sets its hardware ID and finds its driver.
    fn prepare(target: Variant, say: &mut dyn FnMut(&str)) -> Result<Self, SwitchError> {
        let set = DeviceSet::new_media()?;
        let mut data = SP_DEVINFO_DATA { cbSize: size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
        // SAFETY: our set; "MEDIA" and the class GUID are constants; no description or window; `data` is a live out-structure
        // with its size set.
        unsafe {
            SetupDiCreateDeviceInfoW(set.get(), w!("MEDIA"), &GUID_DEVCLASS_MEDIA, PCWSTR::null(), None, DICD_GENERATE_ID, Some(&mut data))
        }
        .map_err(WinError::of("SetupDiCreateDeviceInfoW"))?;
        let instance_id = set.instance_id(&data)?;
        say(&format!("Preparing the new device {instance_id} with the hardware ID {} (not registered yet)", target.hardware_id()));
        let ids = multi_sz_bytes(&[target.hardware_id()]);
        // SAFETY: an element of our set; the buffer's length goes with it.
        unsafe { SetupDiSetDeviceRegistryPropertyW(set.get(), &mut data, SPDRP_HARDWAREID, Some(ids.as_slice())) }
            .map_err(WinError::of("SetupDiSetDeviceRegistryPropertyW(SPDRP_HARDWAREID)"))?;

        say(&format!(
            "Looking for its driver in the driver store (SetupDiBuildDriverInfoList, the drivers compatible with {})",
            target.hardware_id()
        ));
        // SAFETY: an element of our set.
        unsafe { SetupDiBuildDriverInfoList(set.get(), Some(&mut data), SPDIT_COMPATDRIVER) }
            .map_err(WinError::of("SetupDiBuildDriverInfoList"))?;
        let nodes = set.compat_drivers(&data)?;
        if nodes.is_empty() {
            say("  none found");
        }
        let candidates: Vec<Candidate> = nodes.iter().map(|(_, c)| c.clone()).collect();
        let chosen = choose(&candidates);
        for (i, c) in candidates.iter().enumerate() {
            let mark = if Some(i) == chosen {
                ": chosen"
            } else if c.is_ours() {
                ": an older one"
            } else {
                ": not ours"
            };
            say(&format!("  {}{mark}", c.describe()));
        }
        let Some(chosen) = chosen else { return Err(SwitchError::NoDriver { hardware_id: target.hardware_id() }) };
        match store_location(&candidates[chosen].inf) {
            Ok(path) => say(&format!("  stored at {path}")),
            Err(e) => say(&format!("  (its driver-store folder is unknown: {e})")),
        }
        Ok(Self { set, data, node: nodes[chosen].0, instance_id, chosen: candidates[chosen].clone() })
    }

    /// Registers the device, installs the chosen driver on it, and judges the result ([`judge_install`]). A device that
    /// isn't kept is removed again: one without the right driver would read as this variant next time.
    fn install(self, say: &mut dyn FnMut(&str)) -> Result<Installed, SwitchError> {
        let Pending { set, data, node, instance_id, chosen } = self;
        say(&format!("Creating {instance_id} (SetupDiCallClassInstaller(DIF_REGISTERDEVICE))"));
        // SAFETY: an element of our set.
        unsafe { SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set.get(), Some(&data)) }
            .map_err(WinError::of("SetupDiCallClassInstaller(DIF_REGISTERDEVICE)"))?;
        say(&format!("Installing the driver from {} on it (DiInstallDevice)", chosen.inf));
        let mut restart = BOOL(0);
        // No finish-install pages: the helper has no window to show them from.
        // SAFETY: a registered element of our set and a driver node from its own list; `restart` is a live out-pointer.
        let call = unsafe { DiInstallDevice(None, set.get(), &data, Some(&node), DIIDFLAG_NOFINISHINSTALLUI, Some(&mut restart)) }
            .map_err(|e| WinError::new("DiInstallDevice", &e));
        if let Err(e) = &call {
            say(&format!("  {e}"));
        }
        let restart = call.is_ok() && restart.as_bool();
        // Plug and Play may still be starting it, or, after a failed call, putting a driver on it by itself
        let deadline = Instant::now() + DRIVER_WAIT;
        let mut now = set.bound(&data);
        while !restart && now.health != Health::Working && Instant::now() < deadline {
            std::thread::sleep(POLL);
            now = set.bound(&data);
        }
        say(&format!("  {instance_id}: {}", now.describe()));
        match judge_install(call.as_ref().map(|_| ()), restart, &now, &chosen) {
            Ok(Installed::Working) => {
                if call.is_err() {
                    say("  on the chosen package all the same: Plug and Play installed it by itself");
                }
                Ok(Installed::Working)
            }
            Ok(Installed::Restart) => Ok(Installed::Restart),
            Err(failure) => {
                say(&format!("Removing {instance_id} again: {failure}"));
                // SAFETY: as above; no window, no restart flag wanted.
                let removed = unsafe { DiUninstallDevice(HWND::default(), set.get(), &data, 0, None) }
                    .map_err(|e| WinError::new("DiUninstallDevice", &e));
                match &removed {
                    Ok(()) => say("  removed"),
                    Err(e) => say(&format!("  {e}")),
                }
                Err(SwitchError::Install(Box::new(NotKept { device: instance_id, chosen: chosen.describe(), failure, removed })))
            }
        }
    }
}

/// A devnode's state.
fn health(devinst: u32) -> Health {
    let (mut status, mut problem) = (CM_DEVNODE_STATUS_FLAGS(0), CM_PROB(0));
    // SAFETY: two live out-pointers; `devinst` came from SetupDi (a stale one only makes the call fail).
    let cr = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, devinst, 0) };
    Health::of(cr.0, status.0, problem.0)
}

/// The name an INF had before the driver store published it as `oemNN.inf`: `audiocodec.inf`. An inbox INF gives its own name.
pub fn original_inf_name(inf_path: &str) -> Result<String, WinError> {
    let path = HSTRING::from(inf_path);
    let spec = path.as_ptr().cast();
    let mut needed = 0u32;
    // SAFETY: a NUL-terminated absolute path; no buffer, so the call only says how big one must be.
    match unsafe { SetupGetInfInformationW(spec, INFINFO_INF_NAME_IS_ABSOLUTE, None, 0, Some(&mut needed)) } {
        Ok(()) => {}
        Err(e) if win32_code(&e) == ERROR_INSUFFICIENT_BUFFER.0 => {}
        Err(e) => return Err(WinError::new("SetupGetInfInformationW", &e)),
    }
    let mut buf = vec![0u64; (needed as usize).div_ceil(size_of::<u64>()).max(4)];
    let info = buf.as_mut_ptr().cast::<SP_INF_INFORMATION>();
    let bytes = (buf.len() * size_of::<u64>()) as u32;
    // SAFETY: as above; `info` points at `bytes` writable bytes, aligned for the structure.
    unsafe { SetupGetInfInformationW(spec, INFINFO_INF_NAME_IS_ABSOLUTE, Some(info), bytes, None) }
        .map_err(WinError::of("SetupGetInfInformationW"))?;
    // SAFETY: a plain C structure, for which all zeros is a valid value; its size is set next.
    let mut original: SP_ORIGINAL_FILE_INFO_W = unsafe { std::mem::zeroed() };
    original.cbSize = size_of::<SP_ORIGINAL_FILE_INFO_W>() as u32;
    // SAFETY: `info` was filled by the call above and lives in `buf`; `original` is a live out-structure with its size set.
    unsafe { SetupQueryInfOriginalFileInformationW(info, 0, None, &mut original) }
        .map_err(WinError::of("SetupQueryInfOriginalFileInformationW"))?;
    Ok(from_wide(&original.OriginalInfName))
}

/// A published INF's folder in the driver store (`...\DriverStore\FileRepository\audiocodec.inf_amd64_...\audiocodec.inf`).
pub fn store_location(inf_path: &str) -> Result<String, WinError> {
    let path = HSTRING::from(inf_path);
    let mut buf = vec![0u16; 1024];
    // SAFETY: a NUL-terminated path; no platform or locale; the buffer's length goes with it.
    unsafe { SetupGetInfDriverStoreLocationW(&path, None, PCWSTR::null(), &mut buf, None) }
        .map_err(WinError::of("SetupGetInfDriverStoreLocationW"))?;
    Ok(from_wide(&buf))
}

/// A UTF-16 string up to its first NUL.
fn from_wide(s: &[u16]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}

/// Little-endian bytes as UTF-16 units (a registry or device property's buffer); an odd last byte is dropped.
fn wide_of_bytes(bytes: &[u8]) -> Vec<u16> {
    bytes.as_chunks::<2>().0.iter().map(|&pair| u16::from_le_bytes(pair)).collect()
}

/// The strings of a REG_MULTI_SZ: NUL-separated, ending at an empty one (or the buffer's end).
pub fn parse_multi_sz(s: &[u16]) -> Vec<String> {
    s.split(|&c| c == 0).take_while(|part| !part.is_empty()).map(String::from_utf16_lossy).collect()
}

/// A REG_MULTI_SZ's bytes: each string NUL-terminated, then one more NUL.
pub fn multi_sz_bytes(strings: &[&str]) -> Vec<u8> {
    let mut units: Vec<u16> = Vec::new();
    for s in strings {
        units.extend(s.encode_utf16());
        units.push(0);
    }
    units.push(0);
    units.iter().flat_map(|u| u.to_le_bytes()).collect()
}

/// A DriverVer version's four parts: "1.0.0.0".
pub fn version_text(v: u64) -> String {
    format!("{}.{}.{}.{}", v >> 48, (v >> 32) & 0xFFFF, (v >> 16) & 0xFFFF, v & 0xFFFF)
}

/// A driver version as Windows gives it ("12.40.56.342"): one to four parts of 0-65535, the missing ones 0, as DriverVer's
/// four 16-bit parts. Compared as numbers, so "1.02" is 1.2.0.0.
pub fn parse_version(s: &str) -> Option<u64> {
    let mut parts = s.trim().split('.');
    let mut v = 0u64;
    for i in 0..4 {
        let part = match parts.next() {
            Some(p) => p.trim().parse::<u16>().ok()?,
            None if i > 0 => 0,
            None => return None,
        };
        v = (v << 16) | u64::from(part);
    }
    parts.next().is_none().then_some(v)
}

/// A FILETIME's date (100 ns ticks since 1601-01-01 UTC) as "2026-11-23".
pub fn date_text(filetime: u64) -> String {
    const TICKS_PER_DAY: u64 = 864_000_000_000;
    // 1601-01-01 to 1970-01-01
    const DAYS_1601_TO_1970: i64 = 134_774;
    let days = (filetime / TICKS_PER_DAY) as i64 - DAYS_1601_TO_1970;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Nothing here touches a device or the driver store: the mapping, the plan and the formats only.

    #[test]
    fn each_variant_has_its_hardware_id_and_word() {
        assert_eq!(Variant::Both.hardware_id(), r"ROOT\AudioCodec");
        assert_eq!(Variant::Call.hardware_id(), r"ROOT\AudioCodecCall");
        assert_eq!(Variant::LowLatency.hardware_id(), r"ROOT\AudioCodecLowLatency");
        for v in Variant::ALL {
            assert_eq!(Variant::from_hardware_id(v.hardware_id()), Some(v));
            assert_eq!(Variant::from_arg(v.arg()), Some(v));
            assert_eq!(Variant::from_hardware_id(&v.hardware_id().to_uppercase()), Some(v), "hardware IDs compare in any case");
        }
        assert_eq!(Variant::from_hardware_id(r"root\audiocodeccall"), Some(Variant::Call));
        // The whole ID, not a prefix: ROOT\AudioCodec is a prefix of the other two
        assert_eq!(Variant::from_hardware_id(r"ROOT\AudioCodecCallX"), None);
        assert_eq!(Variant::from_hardware_id(r"ROOT\AudioCode"), None);
        assert_eq!(Variant::from_hardware_id(r"ROOT\MEDIA\0003"), None);
        assert_eq!(Variant::from_arg("Low-Latency"), Some(Variant::LowLatency));
        assert_eq!(Variant::from_arg("lowlatency"), None);
        assert_eq!(Variant::from_arg(""), None);
        assert_eq!(Variant::Both.microphones(), [MICROPHONE, CALL_MICROPHONE]);
        assert_eq!(Variant::Call.microphones(), [CALL_MICROPHONE]);
        assert_eq!(Variant::LowLatency.microphones(), [MICROPHONE]);
        assert!(MICROPHONE.contains(DEVICE_NAME) && CALL_MICROPHONE.contains(DEVICE_NAME));
    }

    #[test]
    fn a_devices_variant_comes_from_its_hardware_ids() {
        let ids = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(variant_of(&ids(&[r"ROOT\AudioCodecLowLatency"])), Some(Variant::LowLatency));
        assert_eq!(variant_of(&ids(&[r"PCI\VEN_8086", r"ROOT\AudioCodec"])), Some(Variant::Both));
        assert_eq!(variant_of(&ids(&[r"ROOT\VBAudioVACWDM"])), None);
        assert_eq!(variant_of(&[]), None);
    }

    #[test]
    fn multi_sz_round_trips() {
        let bytes = multi_sz_bytes(&[r"ROOT\AudioCodecCall"]);
        assert_eq!(bytes.len(), (r"ROOT\AudioCodecCall".len() + 2) * 2, "the string, its NUL and the list's NUL");
        assert_eq!(&bytes[bytes.len() - 4..], [0, 0, 0, 0]);
        assert_eq!(parse_multi_sz(&wide_of_bytes(&bytes)), [r"ROOT\AudioCodecCall"]);
        let two = multi_sz_bytes(&["a", "bc"]);
        assert_eq!(parse_multi_sz(&wide_of_bytes(&two)), ["a", "bc"]);
        // A buffer longer than the list (the property's buffer), and one without its final NUL
        let mut padded = wide_of_bytes(&two);
        padded.extend([0, 0, 0, 7]);
        assert_eq!(parse_multi_sz(&padded), ["a", "bc"]);
        assert_eq!(parse_multi_sz(&[b'x' as u16]), ["x"]);
        assert!(parse_multi_sz(&[]).is_empty());
        assert_eq!(from_wide(&[b'h' as u16, b'i' as u16, 0, b'x' as u16]), "hi");
    }

    const V1: u64 = 1 << 48;

    /// The driver store's newest package in these tests: oem12.inf, 1.0.0.0.
    fn newest() -> Candidate {
        candidate(r"C:\Windows\INF\oem12.inf", "audiocodec.inf", 200, V1)
    }

    /// A device on the newest package.
    fn dev(variant: Variant, health: Health) -> OurDevice {
        OurDevice {
            instance_id: r"ROOT\MEDIA\0003".into(),
            variant,
            health,
            inf: Some("oem12.inf".into()),
            driver_version: Some("1.0.0.0".into()),
            newest: Ok(Some(newest())),
        }
    }

    /// A device on an older package than the newest: the single-microphone driver, say, which had `ROOT\AudioCodec` too.
    fn old(variant: Variant, health: Health) -> OurDevice {
        OurDevice { inf: Some("oem3.inf".into()), driver_version: Some("0.9.0.300".into()), ..dev(variant, health) }
    }

    #[test]
    fn the_plan_keeps_one_working_target_and_removes_the_rest() {
        use Health::*;
        use Variant::*;
        // Already the only one, working on the newest package: nothing
        let p = plan(&[dev(Call, Working)], Call);
        assert_eq!(p, Plan { keep: Some(0), remove: vec![], create: false });
        assert!(p.is_nothing());
        // Another variant: remove it, create the target
        assert_eq!(plan(&[dev(Both, Working)], Call), Plan { keep: None, remove: vec![0], create: true });
        // None installed: create
        let p = plan(&[], LowLatency);
        assert_eq!(p, Plan { keep: None, remove: vec![], create: true });
        assert!(!p.is_nothing());
        // The target there beside another: keep it, remove the other, create nothing
        assert_eq!(plan(&[dev(Both, Working), dev(Call, Working)], Call), Plan { keep: Some(1), remove: vec![0], create: false });
        // Two of the target: keep the first
        assert_eq!(plan(&[dev(Call, Working), dev(Call, Working)], Call), Plan { keep: Some(0), remove: vec![1], create: false });
        // The target disabled, without a driver or failing: replace it
        for broken in [Problem(22), Problem(28), Problem(10), NotStarted, Unknown(13)] {
            assert_eq!(plan(&[dev(Call, broken)], Call), Plan { keep: None, remove: vec![0], create: true }, "{broken:?}");
        }
        assert_eq!(plan(&[dev(Call, Problem(28)), dev(Call, Working)], Call), Plan { keep: Some(1), remove: vec![0], create: false });
    }

    /// Astra's review: a working single-microphone install has `ROOT\AudioCodec`, so it read as `both` and the switch did
    /// nothing. Only a device on the driver store's newest package is kept.
    #[test]
    fn the_plan_replaces_a_device_on_an_older_package() {
        use Health::*;
        use Variant::*;
        let replace = Plan { keep: None, remove: vec![0], create: true };
        assert_eq!(plan(&[old(Both, Working)], Both), replace);
        assert!(!plan(&[old(Both, Working)], Both).is_nothing());
        // The same published INF with another version: the driver store reused the name for a newer package
        let reused = OurDevice { driver_version: Some("0.9.0.300".into()), ..dev(Both, Working) };
        assert_eq!(plan(&[reused], Both), replace);
        // The driver store's packages unread, or none of ours there: not known to be current, so replaced (the new device's
        // own search then says whether there is a driver, before anything is removed)
        let unread = OurDevice { newest: Err(WinError { call: "SetupDiBuildDriverInfoList", code: 5 }), ..dev(Both, Working) };
        assert_eq!(plan(&[unread], Both), replace);
        assert_eq!(plan(&[OurDevice { newest: Ok(None), ..dev(Both, Working) }], Both), replace);
        // An old one beside a current one of the target: keep the current one
        assert_eq!(plan(&[old(Both, Working), dev(Both, Working)], Both), Plan { keep: Some(1), remove: vec![0], create: false });
        // The INF's name in any case, or as a path, is the same INF
        let upper = OurDevice { inf: Some(r"C:\WINDOWS\INF\OEM12.INF".into()), ..dev(Call, Working) };
        assert!(upper.is_current());
        assert!(plan(&[upper], Call).is_nothing());
    }

    #[test]
    fn a_device_runs_a_package_by_its_inf_and_version() {
        let c = newest();
        assert!(runs(Some("oem12.inf"), Some("1.0.0.0"), &c));
        assert!(runs(Some(" OEM12.inf "), Some("1.0"), &c), "any case; the version as numbers");
        assert!(!runs(Some("oem12.inf"), Some("1.0.0.300"), &c));
        assert!(!runs(Some("oem1.inf"), Some("1.0.0.0"), &c), "the whole name, not a prefix");
        assert!(!runs(Some("oem12.inf"), None, &c) && !runs(None, Some("1.0.0.0"), &c) && !runs(None, None, &c));
        assert!(!runs(Some("oem12.inf"), Some("one"), &c));
        let d = dev(Variant::Both, Health::Working);
        assert!(d.runs(&c) && d.is_current());
        assert!(!old(Variant::Both, Health::Working).is_current());
    }

    #[test]
    fn the_package_note_says_what_the_device_runs() {
        assert_eq!(dev(Variant::Both, Health::Working).package_note(), "it runs the newest AudioCodec package in the driver store");
        let note = old(Variant::Both, Health::Working).package_note();
        assert!(
            note.starts_with(
                r"it doesn't run the newest AudioCodec package in the driver store, C:\Windows\INF\oem12.inf (audiocodec.inf)"
            ),
            "{note}"
        );
        let none = OurDevice { newest: Ok(None), ..dev(Variant::Call, Health::Working) };
        assert_eq!(none.package_note(), r"no AudioCodec package in the driver store has ROOT\AudioCodecCall");
        let unread =
            OurDevice { newest: Err(WinError { call: "SetupDiBuildDriverInfoList", code: 5 }), ..dev(Variant::Both, Health::Working) };
        let note = unread.package_note();
        assert!(
            note.starts_with(
                "the driver store's packages for it couldn't be read: SetupDiBuildDriverInfoList failed: ERROR_ACCESS_DENIED (5)"
            ),
            "{note}"
        );
    }

    #[test]
    fn versions_parse_as_windows_gives_them() {
        assert_eq!(parse_version("12.40.56.342"), Some((12 << 48) | (40 << 32) | (56 << 16) | 342));
        assert_eq!(parse_version(" 1.2.3.4 "), Some((1 << 48) | (2 << 32) | (3 << 16) | 4));
        assert_eq!(parse_version("1.02.0.0"), Some((1 << 48) | (2 << 32)));
        assert_eq!(parse_version("1"), Some(1 << 48));
        assert_eq!(parse_version("1.0"), Some(1 << 48));
        assert_eq!(parse_version("65535.0.0.65535"), Some((65_535 << 48) | 65_535));
        for bad in ["", " ", "1.2.3.4.5", "65536.0.0.0", "1..2", "a.b", "-1.0", "1.0."] {
            assert_eq!(parse_version(bad), None, "{bad:?}");
        }
        for v in [0, V1, (12 << 48) | (40 << 32) | (56 << 16) | 342, u64::MAX] {
            assert_eq!(parse_version(&version_text(v)), Some(v));
        }
    }

    fn bound(health: Health, inf: &str, version: &str) -> Bound {
        Bound { health, inf: Some(inf.into()), driver_version: Some(version.into()) }
    }

    /// Astra's review: the install accepted a failed bind if Windows started the device with some driver.
    #[test]
    fn an_install_is_kept_only_when_working_on_the_chosen_package() {
        use Health::*;
        let c = newest();
        let failed = WinError { call: "DiInstallDevice", code: 0xE000_022F };
        let on_chosen = bound(Working, "oem12.inf", "1.0.0.0");
        assert_eq!(judge_install(Ok(()), false, &on_chosen, &c), Ok(Installed::Working));
        // The call failed, but Plug and Play put the chosen package on it by itself
        assert_eq!(judge_install(Err(&failed), false, &on_chosen, &c), Ok(Installed::Working));
        // Working on another package, the call failed or not: not kept
        let other = bound(Working, "oem3.inf", "0.9.0.300");
        assert_eq!(
            judge_install(Err(&failed), false, &other, &c),
            Err(InstallFailure::OtherPackage { error: Some(failed.clone()), now: other.clone() })
        );
        assert_eq!(judge_install(Ok(()), false, &other, &c), Err(InstallFailure::OtherPackage { error: None, now: other.clone() }));
        let reused = bound(Working, "oem12.inf", "0.9.0.300");
        assert!(matches!(judge_install(Ok(()), false, &reused, &c), Err(InstallFailure::OtherPackage { .. })));
        // Not working
        let bare = Bound { health: Problem(28), inf: None, driver_version: None };
        assert_eq!(judge_install(Err(&failed), false, &bare, &c), Err(InstallFailure::Call { error: failed.clone(), now: bare.clone() }));
        for state in [NotStarted, Problem(10), Problem(28), Unknown(13)] {
            let now = Bound { health: state, ..on_chosen.clone() };
            assert_eq!(judge_install(Ok(()), false, &now, &c), Err(InstallFailure::NotWorking { now: now.clone() }), "{state:?}");
        }
        // A restart to finish: left for it
        assert_eq!(judge_install(Ok(()), true, &bare, &c), Ok(Installed::Restart));
        assert_eq!(judge_install(Ok(()), false, &Bound { health: Problem(14), ..on_chosen.clone() }, &c), Ok(Installed::Restart));
        // ... but not when the call failed, or Code 14 on another package
        assert!(matches!(judge_install(Err(&failed), true, &bare, &c), Err(InstallFailure::Call { .. })));
        assert!(matches!(
            judge_install(Ok(()), false, &Bound { health: Problem(14), ..other.clone() }, &c),
            Err(InstallFailure::NotWorking { .. })
        ));
    }

    #[test]
    fn install_failures_say_what_the_device_is() {
        let failed = WinError { call: "DiInstallDevice", code: 5 };
        let other = bound(Health::Working, "oem3.inf", "0.9.0.300");
        let text = InstallFailure::OtherPackage { error: Some(failed.clone()), now: other.clone() }.to_string();
        assert!(text.starts_with("DiInstallDevice failed: ERROR_ACCESS_DENIED (5)"), "{text}");
        assert!(
            text.ends_with("; the device now: working, driver oem3.inf 0.9.0.300, another package (Plug and Play's own choice)"),
            "{text}"
        );
        let text = InstallFailure::OtherPackage { error: None, now: other }.to_string();
        assert_eq!(text, "the device now: working, driver oem3.inf 0.9.0.300, another package (Plug and Play's own choice)");
        let bare = Bound { health: Health::Problem(28), inf: None, driver_version: None };
        let text = InstallFailure::Call { error: failed, now: bare.clone() }.to_string();
        assert!(text.ends_with("; the device now: no driver installed (Code 28), no driver"), "{text}");
        let text = InstallFailure::NotWorking { now: Bound { health: Health::NotStarted, ..bare } }.to_string();
        assert_eq!(text, "5 s after DiInstallDevice, the device: not started, no driver");
    }

    fn ep(id: &str, name: &str, active: bool) -> CaptureEndpoint {
        CaptureEndpoint { id: id.into(), name: name.into(), active }
    }

    /// Astra's review: check the capabilities before declaring success. A new device's microphones are new endpoints.
    #[test]
    fn the_microphones_are_new_endpoints_of_the_variants_names() {
        use Variant::*;
        let both_new = [ep("{n1}", MICROPHONE, true), ep("{n2}", CALL_MICROPHONE, true)];
        let check = check_mics(Both, &both_new, &[]);
        assert_eq!(check.found, [(MICROPHONE, true), (CALL_MICROPHONE, true)]);
        assert!(check.is_done() && check.missing.is_empty() && check.extra.is_empty());
        // The upgrade from the single-microphone driver: its Microphone, still active, doesn't count, and is waited out
        let before = ["{old}".to_string()];
        let midway = [ep("{old}", MICROPHONE, true), ep("{n1}", MICROPHONE, true)];
        let check = check_mics(Both, &midway, &before);
        assert_eq!(
            (check.found, check.missing, check.extra),
            (vec![(MICROPHONE, true)], vec![CALL_MICROPHONE], vec![MICROPHONE.to_string()])
        );
        let check = check_mics(Both, &[ep("{OLD}", MICROPHONE, true)], &before);
        assert_eq!(check.missing, [MICROPHONE, CALL_MICROPHONE], "an old endpoint, its id in any case, isn't the new device's");
        let mut after: Vec<CaptureEndpoint> = both_new.to_vec();
        assert!(check_mics(Both, &after, &before).is_done());
        // One disabled in the Sound panel is there (the user's choice); an active one is taken before it
        let check = check_mics(Call, &[ep("{n2}", CALL_MICROPHONE, false)], &[]);
        assert!(check.is_done() && check.found == [(CALL_MICROPHONE, false)]);
        assert_eq!(check.seen, [format!("{CALL_MICROPHONE} (disabled)")]);
        let check = check_mics(Call, &[ep("{a}", CALL_MICROPHONE, false), ep("{b}", CALL_MICROPHONE, true)], &[]);
        assert!(check.is_done() && check.found == [(CALL_MICROPHONE, true)], "{check:?}");
        // A second active one of the same name, or one the variant doesn't have: waited out
        let check = check_mics(Call, &[ep("{a}", CALL_MICROPHONE, true), ep("{b}", CALL_MICROPHONE, true)], &[]);
        assert!(check.missing.is_empty() && check.extra == [CALL_MICROPHONE] && !check.is_done());
        after.push(ep("{x}", "Speakers (AudioCodec Device)", true));
        let check = check_mics(LowLatency, &after, &[]);
        assert_eq!(check.found, [(MICROPHONE, true)]);
        assert_eq!(check.extra, [CALL_MICROPHONE, "Speakers (AudioCodec Device)"]);
        // Names in any case; a numbered or renamed one isn't the microphone
        assert!(check_mics(Call, &[ep("{a}", "call microphone (audiocodec device)", true)], &[]).is_done());
        let check = check_mics(LowLatency, &[ep("{a}", "Microphone (2- AudioCodec Device)", true)], &[]);
        assert_eq!((check.missing, check.extra), (vec![MICROPHONE], vec!["Microphone (2- AudioCodec Device)".to_string()]));
        // Nothing yet
        let check = check_mics(Both, &[], &[]);
        assert_eq!((check.missing, check.seen.len()), (vec![MICROPHONE, CALL_MICROPHONE], 0));
    }

    #[test]
    fn a_wait_for_the_lock_ends_as_windows_says() {
        assert_eq!(lock_wait(WAIT_OBJECT_0), LockWait::Taken);
        assert_eq!(lock_wait(WAIT_ABANDONED), LockWait::Abandoned);
        assert_eq!(lock_wait(WAIT_TIMEOUT), LockWait::Busy);
        assert_eq!(lock_wait(windows::Win32::Foundation::WAIT_FAILED), LockWait::Failed);
    }

    /// Astra's review: two switches at once could both create a device. The lock, on a mutex of the test's own (Local\, this
    /// process's id), so a real switch is never in the way: held, another thread waits and gives up; released, it takes it;
    /// abandoned by a thread that ended holding it, the next takes it and says so.
    #[test]
    fn the_lock_lets_one_switch_in_at_a_time() {
        let name = format!(r"Local\noise-filter-test-mic-variant-{}", std::process::id());
        let take_elsewhere = |wait: Duration| {
            let name = name.clone();
            std::thread::spawn(move || {
                let mut lines = Vec::new();
                let taken = SwitchLock::take(&name, wait, &mut |l| lines.push(l.to_string())).map(drop);
                (taken, lines)
            })
            .join()
            .unwrap()
        };
        let held = SwitchLock::take(&name, Duration::ZERO, &mut |_| {}).unwrap();
        let (taken, lines) = take_elsewhere(Duration::from_millis(50));
        assert_eq!(taken, Err(SwitchError::Busy));
        assert_eq!(lines, ["Another switch is running: waiting for it to end (up to 0 s)"]);
        drop(held);
        assert_eq!(take_elsewhere(Duration::ZERO), (Ok(()), vec![]));
        let n = name.clone();
        std::thread::spawn(move || std::mem::forget(SwitchLock::take(&n, Duration::ZERO, &mut |_| {}).unwrap())).join().unwrap();
        let (taken, lines) = take_elsewhere(Duration::ZERO);
        assert_eq!(taken, Ok(()));
        assert_eq!(lines, ["(The last switch's process ended in the middle of its switch: this one reads the devices afresh.)"]);
    }

    #[test]
    fn health_reads_the_status_flags() {
        assert_eq!(Health::of(0, DN_STARTED.0, 0), Health::Working);
        assert_eq!(Health::of(0, 0, 0), Health::NotStarted);
        assert_eq!(Health::of(0, DN_HAS_PROBLEM.0, 22), Health::Problem(22));
        assert_eq!(Health::of(0, DN_HAS_PROBLEM.0 | DN_STARTED.0, 10), Health::Problem(10));
        assert_eq!(Health::of(13, DN_STARTED.0, 0), Health::Unknown(13));
        assert_eq!(Health::Problem(22).describe(), "disabled (Code 22)");
        assert_eq!(Health::Problem(28).describe(), "no driver installed (Code 28)");
        assert_eq!(Health::Problem(10).describe(), "a problem: Code 10 (CM_PROB_FAILED_START)");
        assert_eq!(Health::Problem(99).describe(), "a problem: Code 99");
        assert_eq!(Health::Unknown(13).describe(), "state unknown (CM_Get_DevNode_Status: CR_NO_SUCH_DEVNODE, 13)");
        let d = OurDevice { driver_version: Some("1.0.0.0".into()), ..dev(Variant::Both, Health::Working) };
        assert_eq!(d.describe(), r"ROOT\MEDIA\0003: both microphones (ROOT\AudioCodec), working, driver oem12.inf 1.0.0.0");
        let bare = OurDevice { inf: None, ..dev(Variant::Call, Health::Problem(28)) };
        assert_eq!(
            bare.describe(),
            r"ROOT\MEDIA\0003: the call microphone only (ROOT\AudioCodecCall), no driver installed (Code 28), no driver"
        );
    }

    fn candidate(inf: &str, original: &str, date: u64, version: u64) -> Candidate {
        Candidate {
            inf: inf.into(),
            original: Ok(original.into()),
            provider: "VS_Microsoft".into(),
            description: "AudioCodec Device".into(),
            date,
            version,
        }
    }

    #[test]
    fn our_newest_driver_is_chosen() {
        let v1 = 1u64 << 48;
        let older = candidate(r"C:\Windows\INF\oem3.inf", "AudioCodec.inf", 100, v1);
        let newer = candidate(r"C:\Windows\INF\oem12.inf", "audiocodec.inf", 200, v1);
        let foreign = candidate(r"C:\Windows\INF\oem9.inf", "othercodec.inf", 900, v1);
        assert_eq!(choose(&[older.clone(), newer.clone(), foreign.clone()]), Some(1));
        assert_eq!(choose(&[newer.clone(), older.clone()]), Some(0));
        // The same date: the higher version
        let bumped = candidate(r"C:\Windows\INF\oem13.inf", "audiocodec.inf", 200, v1 + 1);
        assert_eq!(choose(&[newer.clone(), bumped.clone()]), Some(1));
        // A tie: the first
        assert_eq!(choose(&[newer.clone(), newer.clone()]), Some(0));
        // Nothing of ours, or nothing at all
        assert_eq!(choose(std::slice::from_ref(&foreign)), None);
        assert_eq!(choose(&[]), None);
        // An INF whose original name couldn't be read isn't taken for ours
        let unknown = Candidate { original: Err("SetupGetInfInformationW failed".into()), ..newer.clone() };
        assert!(!unknown.is_ours());
        assert_eq!(choose(&[unknown]), None);
    }

    #[test]
    fn versions_and_dates_read_as_windows_shows_them() {
        assert_eq!(version_text((1 << 48) | (2 << 32) | (3 << 16) | 4), "1.2.3.4");
        assert_eq!(version_text(0), "0.0.0.0");
        // 1601-01-01, 1970-01-01, a leap day and the day this was written (FILETIME ticks)
        assert_eq!(date_text(0), "1601-01-01");
        assert_eq!(date_text(116_444_736_000_000_000), "1970-01-01");
        assert_eq!(date_text(116_444_736_000_000_000 + (19_782 * 864_000_000_000)), "2024-02-29");
        assert_eq!(date_text(116_444_736_000_000_000 + (20_780 * 864_000_000_000) + 1), "2026-11-23");
        let c = candidate(r"C:\Windows\INF\oem12.inf", "audiocodec.inf", 116_444_736_000_000_000, 1 << 48);
        assert_eq!(c.describe(), r"C:\Windows\INF\oem12.inf (audiocodec.inf), VS_Microsoft, 1.0.0.0 of 1970-01-01");
    }

    #[test]
    fn switch_errors_say_what_to_do() {
        let e = SwitchError::NoDriver { hardware_id: Variant::Call.hardware_id() };
        let text = e.to_string();
        assert!(text.starts_with(r"no driver in the driver store has ROOT\AudioCodecCall with the INF audiocodec.inf"), "{text}");
        assert!(text.contains("pnputil /add-driver") && text.ends_with("Nothing was removed."), "{text}");
        let e = SwitchError::RestartFirst { device: r"ROOT\MEDIA\0003".into() };
        assert!(e.to_string().contains(r"finish removing ROOT\MEDIA\0003 when it restarts"));
        let e = SwitchError::from(WinError { call: "DiInstallDevice", code: 5 });
        assert!(e.to_string().starts_with("DiInstallDevice failed: ERROR_ACCESS_DENIED (5)"));
        let text = SwitchError::Busy.to_string();
        assert!(text.starts_with(r"another switch has held Global\noise-filter-mic-variant for 60 s: let it end"), "{text}");
        assert!(text.ends_with("Nothing was changed."));
        let text = SwitchError::NoEndpoints(WinError { call: "IMMDeviceEnumerator::EnumAudioEndpoints", code: 5 }).to_string();
        assert!(text.starts_with("the audio endpoints couldn't be read (IMMDeviceEnumerator::EnumAudioEndpoints failed"), "{text}");
        assert!(text.contains("nothing was removed"));

        let failure = InstallFailure::NotWorking { now: Bound { health: Health::NotStarted, inf: None, driver_version: None } };
        let install = |removed| {
            SwitchError::Install(Box::new(NotKept {
                device: r"ROOT\MEDIA\0004".into(),
                chosen: newest().describe(),
                failure: failure.clone(),
                removed,
            }))
        };
        let text = install(Ok(())).to_string();
        assert!(
            text.starts_with(
                r"the new device ROOT\MEDIA\0004 isn't working on the chosen driver (C:\Windows\INF\oem12.inf (audiocodec.inf)"
            ),
            "{text}"
        );
        assert!(text.contains("): 5 s after DiInstallDevice, the device: not started, no driver. It was removed again"), "{text}");
        assert!(
            text.ends_with("none of ours is installed now: run the switch again, or see what's there with `noise-filter mic variant`.")
        );
        let text = install(Err(WinError { call: "DiUninstallDevice", code: 5 })).to_string();
        assert!(text.contains(r"Removing it again failed too (DiUninstallDevice failed: ERROR_ACCESS_DENIED (5)"), "{text}");
        assert!(text.contains(r"pnputil /remove-device ROOT\MEDIA\0004"), "{text}");

        let e = SwitchError::Mics { missing: vec![CALL_MICROPHONE], seen: vec![MICROPHONE.into()], read: None };
        assert_eq!(
            e.to_string(),
            "the device is working on the newest package, but after 15 s Windows hasn't made \"Call Microphone (AudioCodec Device)\" \
             (ours there: \"Microphone (AudioCodec Device)\"). The Sound panel's Recording tab and `noise-filter mic variant` show \
             what's there now."
        );
        let e = SwitchError::Mics { missing: vec![MICROPHONE, CALL_MICROPHONE], seen: vec![], read: Some(WinError { call: "X", code: 5 }) };
        let text = e.to_string();
        assert!(
            text.contains("hasn't made \"Microphone (AudioCodec Device)\" and \"Call Microphone (AudioCodec Device)\" (ours there: none)")
        );
        assert!(text.contains("; the last look at the endpoints failed: X failed: ERROR_ACCESS_DENIED (5)"), "{text}");
        assert_eq!(Variant::Both.microphones_text(), "\"Microphone (AudioCodec Device)\" and \"Call Microphone (AudioCodec Device)\"");
        assert_eq!(Variant::Call.microphones_text(), "\"Call Microphone (AudioCodec Device)\"");
    }
}
