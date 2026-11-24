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
//! 0. Take the lock ([`LOCK`]), held to the end, so two switches at once (two prompts approved, or an administrator's terminal
//!    beside the helper) can't both create a device: a mutex in a private namespace bounded by BUILTIN\Administrators
//!    (`CreateBoundaryDescriptorW` + `AddSIDToBoundaryDescriptor`, then `CreatePrivateNamespaceW`, or `OpenPrivateNamespaceW`
//!    when another switch made it), whose security descriptor lets Administrators and SYSTEM in. A process that isn't an
//!    elevated administrator's can neither make nor open it, so it can't hold the lock to block the switches, as any process
//!    could a `Global\` mutex; it gets [`SwitchError::LockDenied`] itself. A second switch waits up to [`LOCK_WAIT`], then
//!    stops; one whose holder's process ended mid-switch (an abandoned mutex) goes on, reading afresh.
//! 1. Find our devices: every present device (`SetupDiGetClassDevsW`, all classes) whose `SPDRP_HARDWAREID` lists one of the
//!    three IDs, and for each, the newest of our packages compatible with it (as in step 3, on the device itself). A device of
//!    the target variant is kept only if it is working and runs that package (its `DEVPKEY_Device_DriverInfPath` is the
//!    package's `oemNN.inf` and its `DEVPKEY_Device_DriverVersion` the package's). One on an older package is re-created: the
//!    single-microphone driver before the second capture circuit (f2e7d15) had `ROOT\AudioCodec` too, so a working old
//!    install reads as `both`. Re-created rather than updated in place, because Windows makes an endpoint, and reads the
//!    properties the INF gives it (the call microphone's default period), only when it creates it (TODO 22: of the changes
//!    tried, only uninstalling and reinstalling applied a new period), and a driver update keeps the device and its endpoints.
//! 2. A device kept is checked once, before anything changes ([`check_kept`]): it has been working, so Windows has made its
//!    endpoints, and a microphone of the variant without one is either one its package doesn't make or one Windows didn't
//!    make. Which microphones a package makes, for a hardware ID, its INF says ([`package_mics`]: the reference strings of the
//!    AddInterface lines in the install section that ID's models line names; the single-microphone driver's has the
//!    Microphone alone, and may be the newest package for `ROOT\AudioCodec`, as when it is the only one in the driver store).
//!    If the package makes it (a switch whose microphones didn't come in step 6, say), or its INF couldn't be read (ours, the
//!    owner's to confirm), the device is re-created ([`KeptCheck::Recreate`]). If not, the switch stops, the nothing-to-do one too
//!    ([`SwitchError::PackageLacks`]): it says to add the three variants' package, or, when one of ours that makes it is
//!    there but ranks below the newest, how DriverVer ranks them. A microphone disabled in the Sound panel passes, with a
//!    warning (the owner, 2026-11-23). With nothing else to do, the switch ends here.
//! 3. Note our capture endpoints (active or disabled) if a device is to be created, then the new device, not yet registered
//!    (`SetupDiCreateDeviceInfoW` under class MEDIA with `DICD_GENERATE_ID`, as devcon does: `ROOT\MEDIA\NNNN`; the hardware ID
//!    by `SetupDiSetDeviceRegistryPropertyW`), and its driver: the compatible-driver search over the driver store
//!    (`SetupDiBuildDriverInfoList(SPDIT_COMPATDRIVER)`), which is how Plug and Play itself matches a hardware ID to an INF,
//!    keeping the nodes whose INF (`C:\Windows\INF\oemNN.inf`) has the original name `audiocodec.inf`
//!    (`SetupGetInfInformationW` + `SetupQueryInfOriginalFileInformationW`), the newest by date, then version. So endpoints
//!    that can't be read, a missing driver, or one whose INF doesn't make the variant's microphones (as in step 2), stop the
//!    switch before anything is removed. An element that isn't registered yet is a phantom: destroying the set deletes it.
//! 4. Remove the old devices (`DiUninstallDevice`, what `pnputil /remove-device` does). If Windows needs a restart to finish a
//!    removal, stop there: the old driver may still be loaded. The new device is registered only after this, so that its
//!    endpoints are made while the old ones are gone, and the driver never runs twice.
//! 5. Register the new device (`SetupDiCallClassInstaller(DIF_REGISTERDEVICE)`) and install the chosen driver on it
//!    (`DiInstallDevice`, documented for a driver already in the driver store). `UpdateDriverForPlugAndPlayDevicesW`, devcon's
//!    call, wants the package's own folder rather than a system one (its documentation), and would also touch every other
//!    device with that hardware ID. Then up to [`DRIVER_WAIT`] for the device to be working, and [`judge_install`]: it must
//!    be working on the chosen package (its INF and version, as in step 1), whatever DiInstallDevice returned, as Plug and Play
//!    may have put a driver on it by itself meanwhile. Otherwise it is removed again, so no device without the right driver is
//!    left to read as that variant, and the switch fails. If Windows asked for a restart to finish, the device is left for it
//!    and the switch ends there ([`Outcome::RestartPending`]), its microphones unchecked.
//! 6. Up to [`MICS_WAIT`] for Windows to make the variant's microphones ([`check_mics`]): each there by its kind (the KS
//!    filter its endpoint is connected to, whatever the endpoint is named: [`sessions::identify`]), on the new device (or the
//!    one kept), active or disabled in the Sound panel, and not one of the endpoints noted in step 3; and no other endpoint of
//!    ours still active (a removed device's, going). A microphone that doesn't come is an error; another endpoint still there
//!    is reported.
//!
//! What a failure leaves: before step 4, nothing changed. One in step 4 leaves the devices not yet removed (the steps say
//! which were). One in step 5 leaves none of ours, the device that didn't install removed again, unless removing it failed
//! too (the error names it, to remove by hand); a second run of the switch creates the device afresh. (A restart Windows
//! asks for in step 5 isn't a failure: the new device is left for it.) One in step 6 leaves the new device, or the one kept,
//! working on the chosen package, without the microphones that didn't come; a second run re-creates it (step 2).

use std::fmt;
use std::mem::size_of;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_DEVNODE_STATUS_FLAGS, CM_Get_DevNode_Status, CM_PROB, CM_PROB_NEED_RESTART, DICD_GENERATE_ID, DIF_REGISTERDEVICE, DIGCF_ALLCLASSES,
    DIGCF_PRESENT, DIIDFLAG_NOFINISHINSTALLUI, DN_HAS_PROBLEM, DN_STARTED, DiInstallDevice, DiUninstallDevice, GUID_DEVCLASS_MEDIA,
    HDEVINFO, INF_STYLE_WIN4, INFCONTEXT, INFINFO_INF_NAME_IS_ABSOLUTE, SETUP_DI_REGISTRY_PROPERTY, SP_DEVINFO_DATA, SP_DRVINFO_DATA_V2_W,
    SP_DRVINFO_DETAIL_DATA_W, SP_INF_INFORMATION, SP_ORIGINAL_FILE_INFO_W, SPDIT_COMPATDRIVER, SPDRP_HARDWAREID, SetupCloseInfFile,
    SetupDiBuildDriverInfoList, SetupDiCallClassInstaller, SetupDiCreateDeviceInfoList, SetupDiCreateDeviceInfoW, SetupDiEnumDeviceInfo,
    SetupDiEnumDriverInfoW, SetupDiGetActualSectionToInstallW, SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW,
    SetupDiGetDevicePropertyW, SetupDiGetDeviceRegistryPropertyW, SetupDiGetDriverInfoDetailW, SetupDiSetDeviceRegistryPropertyW,
    SetupFindFirstLineW, SetupFindNextMatchLineW, SetupGetInfDriverStoreLocationW, SetupGetInfInformationW, SetupGetStringFieldW,
    SetupOpenInfFileW, SetupQueryInfOriginalFileInformationW,
};
use windows::Win32::Devices::Properties::{DEVPKEY_Device_DriverInfPath, DEVPKEY_Device_DriverVersion, DEVPROPTYPE};
use windows::Win32::Foundation::{
    DEVPROPKEY, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_ITEMS,
    ERROR_PATH_NOT_FOUND, GetLastError, HANDLE, HLOCAL, HWND, INVALID_HANDLE_VALUE, LocalFree, WAIT_ABANDONED, WAIT_EVENT, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{
    CreateWellKnownSid, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SECURITY_MAX_SID_SIZE, WELL_KNOWN_SID_TYPE,
    WinBuiltinAdministratorsSid,
};
use windows::Win32::System::Threading::{
    AddSIDToBoundaryDescriptor, ClosePrivateNamespace, CreateBoundaryDescriptorW, CreateMutexExW, CreatePrivateNamespaceW,
    DeleteBoundaryDescriptor, MUTEX_MODIFY_STATE, OpenPrivateNamespaceW, ReleaseMutex, SYNCHRONIZATION_SYNCHRONIZE, WaitForSingleObject,
};
use windows::core::{BOOL, HSTRING, Owned, PCWSTR, w};

use crate::sessions::{self, CaptureEndpoint, Mic};
use crate::win_error::{WinError, configret_name, problem_name, win32_code};

/// What every endpoint of the driver has in its name ("Microphone (AudioCodec Device)"): the device's description.
pub const DEVICE_NAME: &str = "AudioCodec Device";
/// The low-latency microphone's endpoint (IAudioClient3's 2.67-10 ms periods).
pub const MICROPHONE: &str = "Microphone (AudioCodec Device)";
/// The call microphone's endpoint (a 5 ms default period, for apps that ask for none).
pub const CALL_MICROPHONE: &str = "Call Microphone (AudioCodec Device)";
/// The low-latency microphone's KS filter, by its device interface's reference string: the INF's `KSNAME_Microphone`, the
/// driver's `captureCircuitName` (DriverSettings.h). The single-microphone driver's one microphone has it too.
pub const MICROPHONE_FILTER: &str = "Microphone0";
/// The call microphone's KS filter: the INF's `KSNAME_CallMicrophone`, the driver's `capture2CircuitName`.
pub const CALL_MICROPHONE_FILTER: &str = "Microphone1";
/// The INF's file name as built; the driver store publishes it as `oemNN.inf` and remembers this one.
pub const INF_NAME: &str = "audiocodec.inf";

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

    /// The microphones it makes, by kind, in [`microphones`](Self::microphones)' order.
    pub fn mics(self) -> &'static [Mic] {
        match self {
            Variant::Both => &[Mic::LowLatency, Mic::Call],
            Variant::Call => &[Mic::Call],
            Variant::LowLatency => &[Mic::LowLatency],
        }
    }
}

/// The name Windows gives a microphone of ours by default (it numbers a second device's, and the user may rename it).
pub fn default_name(mic: Mic) -> &'static str {
    match mic {
        Mic::LowLatency => MICROPHONE,
        Mic::Call => CALL_MICROPHONE,
        Mic::Other => DEVICE_NAME,
    }
}

/// Microphones by their default names, in quotes: `"Call Microphone (AudioCodec Device)"`.
pub fn mics_text(mics: &[Mic]) -> String {
    quoted(mics.iter().map(|&m| default_name(m)))
}

/// Names in quotes, joined by "and": `"a" and "b"`.
pub fn quoted<'a>(names: impl Iterator<Item = &'a str>) -> String {
    names.map(|n| format!("\"{n}\"")).collect::<Vec<_>>().join(" and ")
}

/// Our variant among a device's hardware IDs, if any.
pub fn variant_of(hardware_ids: &[String]) -> Option<Variant> {
    hardware_ids.iter().find_map(|id| Variant::from_hardware_id(id))
}

/// A device's state, from CM_Get_DevNode_Status (or, for a node found by its instance ID, CM_Locate_DevNodeW: `cables::present`).
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
    /// CM_Locate_DevNodeW failed with this CONFIGRET, other than CR_NO_SUCH_DEVNODE (not present): the node is listed, its state
    /// unread (`cables::present`).
    Unlocated(u32),
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
            Health::Unknown(cr) => unread("CM_Get_DevNode_Status", cr),
            Health::Unlocated(cr) => unread("CM_Locate_DevNodeW", cr),
        }
    }
}

/// "state unknown (CM_Get_DevNode_Status: CR_NO_SUCH_DEVNODE, 13)": a state `call` couldn't read, failing with CONFIGRET `cr`.
fn unread(call: &str, cr: u32) -> String {
    match configret_name(cr) {
        Some(name) => format!("state unknown ({call}: {name}, {cr})"),
        None => format!("state unknown ({call}: CONFIGRET {cr})"),
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
    /// Our other packages there for its hardware ID, ranked below the newest (none if the search failed).
    pub others: Vec<Candidate>,
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

    /// Re-creates the device it would keep: Windows lacks a microphone its package makes ([`KeptCheck::Recreate`]).
    pub fn recreate_kept(&mut self) {
        if let Some(k) = self.keep.take() {
            self.remove.push(k);
            self.remove.sort_unstable();
            self.create = true;
        }
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
    /// The target variant was already the only one there, working on the newest package, with its microphones: nothing
    /// changed.
    Nothing,
    /// Switched: the device works on the chosen package, and Windows has made its microphones.
    Switched,
    /// Windows asked for a restart to finish installing `device`: the device is left for it, its microphones unchecked.
    RestartPending { device: String },
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
    /// The lock's namespace or mutex refused this process (access denied): it isn't an elevated administrator's. Nothing
    /// was changed.
    LockDenied(WinError),
    /// Our capture endpoints couldn't be read, and the switch checks the microphones with them. Nothing was changed.
    NoEndpoints(WinError),
    /// The driver store's newest package for the target's hardware ID doesn't make some of its microphones, by its INF: the
    /// device that would be kept lacks them, or the new device's package would. Nothing was changed.
    PackageLacks(Box<Lacking>),
    /// The new device wasn't working on the chosen package, so it was removed again. The old devices were removed before it
    /// was made.
    Install(Box<NotKept>),
    /// The device works on the newest package, but Windows hadn't made these microphones after [`MICS_WAIT`]. `seen`: our
    /// endpoints at the last look; `read`: the last look's error, if it failed. The device stays.
    Mics { missing: Vec<Mic>, seen: Vec<String>, read: Option<WinError> },
}

/// The newest of our packages for a variant doesn't make some of its microphones ([`SwitchError::PackageLacks`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lacking {
    pub target: Variant,
    /// The device that would be kept, and its driver (`ROOT\MEDIA\0003`, "driver oem12.inf 1.0.0.0"); `None` when it is a new
    /// device's package.
    pub device: Option<(String, String)>,
    /// The newest package, described ([`Candidate::describe`]).
    pub newest: String,
    /// The target's microphones it doesn't make (and, for a device kept, Windows hasn't).
    pub missing: Vec<Mic>,
    /// The best ranked of our other packages for the hardware ID that makes all the target's microphones, described: staged,
    /// but ranked below the newest.
    pub ranked_below: Option<String>,
}

impl Lacking {
    fn it(&self) -> &'static str {
        if self.missing.len() == 1 { "it" } else { "them" }
    }

    /// "that package doesn't make it (its INF's interfaces for ROOT\AudioCodec; ...)".
    pub fn reason(&self) -> String {
        format!(
            "that package doesn't make {} (its INF's interfaces for {}; the single-microphone driver's has the Microphone alone)",
            self.it(),
            self.target.hardware_id()
        )
    }

    /// What to do: add the three variants' package, or, when one is there but ranks below the newest, how packages rank.
    pub fn advice(&self) -> String {
        let it = self.it();
        let arg = self.target.arg();
        match &self.ranked_below {
            None => format!(
                "No other AudioCodec package in the driver store makes {it}. Add the package with the three variants to the driver \
                 store (from an administrator prompt in its folder, driver\\out\\Release\\AudioCodec once \
                 driver\\scripts\\Build-Driver.ps1 has built it: pnputil /add-driver AudioCodec.inf), then run `noise-filter mic \
                 variant {arg}`: it re-creates the device with that package."
            ),
            Some(better) => format!(
                "{better} makes {it}, but ranks below the newest, {}: Plug and Play, and the switch, take the package whose \
                 DriverVer has the latest date, then the highest version. Build the three variants' package again \
                 (driver\\scripts\\Build-Driver.ps1 stamps DriverVer with the build's UTC date and time of day, so a new build \
                 ranks above the earlier ones; driver\\README.md has the exception, builds of 2026-11-23 from before e3026e5), add \
                 it (pnputil /add-driver AudioCodec.inf, from an administrator prompt in driver\\out\\Release\\AudioCodec), then \
                 run `noise-filter mic variant {arg}`.",
                self.newest
            ),
        }
    }
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
                "another switch has held the switch's lock for {} s: let it end, then run this one again (it reads the devices \
                 afresh). Nothing was changed.",
                LOCK_WAIT.as_secs()
            ),
            SwitchError::LockDenied(e) => write!(
                f,
                "the switch's lock refused this process ({e}): it is for elevated administrators, so run the switch from an \
                 administrator prompt, or approve its UAC prompt. Nothing was changed."
            ),
            SwitchError::NoEndpoints(e) => write!(
                f,
                "the audio endpoints couldn't be read ({e}), and the switch checks the microphones with them: nothing was \
                 changed. Is the Windows Audio Endpoint Builder service running?"
            ),
            SwitchError::PackageLacks(lacking) => {
                let missing = mics_text(&lacking.missing);
                match &lacking.device {
                    Some((device, driver)) => write!(
                        f,
                        "{device} works on the driver store's newest AudioCodec package ({driver}), but Windows has no {missing} on \
                         it: {}. {} Nothing was changed.",
                        lacking.reason(),
                        lacking.advice()
                    ),
                    None => write!(
                        f,
                        "the driver store's newest AudioCodec package for {}, {}, lacks {missing}: {}. {} Nothing was changed.",
                        lacking.target.hardware_id(),
                        lacking.newest,
                        lacking.reason(),
                        lacking.advice()
                    ),
                }
            }
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
                    mics_text(missing)
                )?;
                if let Some(e) = read {
                    write!(f, "; the last look at the endpoints failed: {e}")?;
                }
                write!(
                    f,
                    ". The device stays, and a second run of the switch re-creates it if they're still missing: `noise-filter mic \
                     variant` and the Sound panel's Recording tab show what's there now."
                )
            }
        }
    }
}

impl std::error::Error for SwitchError {}

/// Switches to `target` (see the module's docs for the steps), reporting each step and each Windows error through `say`, as
/// it happens. Needs elevation: without it, the lock refuses the caller ([`SwitchError::LockDenied`]).
pub fn switch(target: Variant, say: &mut dyn FnMut(&str)) -> Result<Outcome, SwitchError> {
    let _lock = SwitchLock::take(&LOCK, LOCK_WAIT, say)?;
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
    let mut plan = plan(&devices, target);
    if let Some(k) = plan.keep {
        // It has been working, so its endpoints are made: one missing is a microphone its package doesn't make (stop), or one
        // Windows didn't (re-create it). Checked before anything changes.
        match check_kept(target, &devices[k], &sessions::capture_endpoints())? {
            KeptCheck::Has(check) => {
                if plan.is_nothing() {
                    say(&format!("Already {} ({}) on the newest package: nothing to do.", target.describe(), target.hardware_id()));
                } else {
                    say(&format!("Keeping {}", devices[k].instance_id));
                }
                for line in found_lines(&check) {
                    say(&line);
                }
                if plan.is_nothing() {
                    return Ok(Outcome::Nothing);
                }
            }
            KeptCheck::Recreate { note, .. } => {
                say(&format!("{note}: re-creating it"));
                plan.recreate_kept();
            }
        }
    }
    // A new device's microphones are endpoints that weren't there before: read them first, before the new device's element
    // is prepared and anything is removed, as the check after needs them
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
    // The device whose microphones are waited for: the new one, or the one kept (plan.create is plan.keep.is_none())
    let device = match pending {
        Some(p) => {
            let id = p.instance_id.clone();
            if p.install(say)? == Installed::Restart {
                say(&format!(
                    "  Windows needs a restart to finish installing {id}: the microphones come after it (`noise-filter mic variant` \
                     checks them)."
                ));
                return Ok(Outcome::RestartPending { device: id });
            }
            id
        }
        None => plan.keep.map(|k| devices[k].instance_id.clone()).unwrap_or_default(),
    };
    let wanted = Wanted { target, device: &device, before: &before };
    wait_for_mics(&wanted, MICS_WAIT, &mut sessions::capture_endpoints, &mut SystemClock::start(), say)?;
    Ok(Outcome::Switched)
}

/// The ids of our capture endpoints there now, active or disabled.
fn capture_endpoint_ids() -> Result<Vec<String>, SwitchError> {
    let endpoints = sessions::capture_endpoints().map_err(SwitchError::NoEndpoints)?;
    Ok(endpoints.into_iter().map(|e| e.id).collect())
}

/// The waits' clock: the time since the wait began, and a sleep. Tests pass their own.
pub trait Clock {
    fn elapsed(&self) -> Duration;
    fn sleep(&mut self, d: Duration);
}

/// The system's clock, counting from [`SystemClock::start`].
pub struct SystemClock(Instant);

impl SystemClock {
    pub fn start() -> Self {
        Self(Instant::now())
    }
}

impl Clock for SystemClock {
    fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }

    fn sleep(&mut self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// Up to `wait` on `clock` for Windows to make `wanted`'s microphones ([`check_mics`]), reading our capture endpoints with
/// `read` every [`POLL`], and saying which came. Done once each is there and nothing else of ours is active; at the deadline,
/// the microphones alone will do (another of ours still active is reported). A read that fails is looked at again; the
/// last good look decides, and the last failure is reported with a microphone missing.
fn wait_for_mics(
    wanted: &Wanted, wait: Duration, read: &mut dyn FnMut() -> Result<Vec<CaptureEndpoint>, WinError>, clock: &mut dyn Clock,
    say: &mut dyn FnMut(&str),
) -> Result<(), SwitchError> {
    say(&format!("Waiting for Windows to make {} on {} (up to {} s)", wanted.target.microphones_text(), wanted.device, wait.as_secs()));
    // The last look that read the endpoints, and the last look's error, if it failed
    let mut last = None::<MicCheck>;
    let failed = loop {
        let failed = match read() {
            Ok(now) => {
                last = Some(check_mics(wanted, &now));
                None
            }
            Err(e) => Some(e),
        };
        let done = failed.is_none() && last.as_ref().is_some_and(MicCheck::is_done);
        if done || clock.elapsed() >= wait {
            break failed;
        }
        clock.sleep(POLL);
    };
    match last {
        Some(check) if check.missing.is_empty() => {
            for line in found_lines(&check) {
                say(&line);
            }
            if !check.extra.is_empty() {
                let extra = quoted(check.extra.iter().map(String::as_str));
                say(&format!("  also still active: {extra} (a removed device's, going, or another device's)"));
            }
            say(&format!("  after {:.1} s", clock.elapsed().as_secs_f64()));
            Ok(())
        }
        Some(check) => Err(SwitchError::Mics { missing: check.missing, seen: check.seen, read: failed }),
        None => Err(SwitchError::Mics { missing: wanted.target.mics().to_vec(), seen: Vec::new(), read: failed }),
    }
}

/// What a check waits for: `target`'s microphones on `device`, none of them one of `before`.
#[derive(Clone, Copy, Debug)]
pub struct Wanted<'a> {
    pub target: Variant,
    /// The instance ID of the device that makes them (`ROOT\MEDIA\0004`): the new one, or the one kept.
    pub device: &'a str,
    /// The ids of our endpoints before the change (a new device's are new); none for a device kept.
    pub before: &'a [String],
}

/// One of a variant's microphones, as a check found it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub mic: Mic,
    /// Its endpoint's name now ("Call Microphone (2- AudioCodec Device)", or the user's).
    pub name: String,
    /// Active; otherwise disabled in the Sound panel.
    pub active: bool,
}

/// What Windows has of a variant's microphones ([`check_mics`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MicCheck {
    /// Each microphone there.
    pub found: Vec<Found>,
    /// The microphones not there.
    pub missing: Vec<Mic>,
    /// Our other active endpoints: a removed device's (going), another device's, a second of the same kind, or one the
    /// variant doesn't have.
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

/// Judges our capture endpoints `now` against `wanted`: each of the target's microphones must be there, by its kind (its
/// filter's, or its name's: [`sessions::identify`]), whatever it's named; on the wanted device (an endpoint whose device
/// wasn't read may be on it), and not one of `before`. An active one is taken before a disabled one (disabled in the Sound
/// panel, it is there still; the owner, 2026-11-23: it passes, with a warning, [`found_lines`]). The endpoints left
/// over that are active are `extra`.
pub fn check_mics(wanted: &Wanted, now: &[CaptureEndpoint]) -> MicCheck {
    let eligible = |e: &CaptureEndpoint| {
        let new = !wanted.before.iter().any(|id| id.eq_ignore_ascii_case(&e.id));
        new && e.device.as_deref().is_none_or(|d| d.eq_ignore_ascii_case(wanted.device))
    };
    let seen = now.iter().map(|e| if e.active { e.name.clone() } else { format!("{} (disabled)", e.name) }).collect();
    let mut check = MicCheck { seen, ..MicCheck::default() };
    let mut used = vec![false; now.len()];
    for &mic in wanted.target.mics() {
        let find = |active: bool| (0..now.len()).find(|&i| !used[i] && now[i].active == active && now[i].mic == mic && eligible(&now[i]));
        match find(true).or_else(|| find(false)) {
            Some(i) => {
                used[i] = true;
                check.found.push(Found { mic, name: now[i].name.clone(), active: now[i].active });
            }
            None => check.missing.push(mic),
        }
    }
    check.extra = (0..now.len()).filter(|&i| !used[i] && now[i].active).map(|i| now[i].name.clone()).collect();
    check
}

/// What [`check_kept`] found of a device kept as it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeptCheck {
    /// It has the variant's microphones: kept.
    Has(MicCheck),
    /// Windows lacks `missing`, though the package it runs makes them (or its INF couldn't be read, so it may): a switch
    /// whose microphones didn't come, say. The switch re-creates it; `note` says why ("ROOT\MEDIA\0003 runs ... on it").
    Recreate { missing: Vec<Mic>, note: String },
}

/// One look at the microphones of a device kept as it is (`device`, of the target variant, working on the newest package):
/// its endpoints were made when it started, so a microphone missing is one its package doesn't make, by its INF
/// ([`SwitchError::PackageLacks`]: nothing to do but add a package), or one Windows didn't make ([`KeptCheck::Recreate`]).
/// `now`: our capture endpoints, as [`sessions::capture_endpoints`] read them.
pub fn check_kept(target: Variant, device: &OurDevice, now: &Result<Vec<CaptureEndpoint>, WinError>) -> Result<KeptCheck, SwitchError> {
    let now = now.as_ref().map_err(|e| SwitchError::NoEndpoints(e.clone()))?;
    let check = check_mics(&Wanted { target, device: &device.instance_id, before: &[] }, now);
    if check.missing.is_empty() {
        return Ok(KeptCheck::Has(check));
    }
    let id = &device.instance_id;
    let lacked = mics_text(&check.missing);
    // A device kept runs the newest package (plan); `None` here would be one that doesn't, which plan never keeps
    let newest = device.newest.as_ref().ok().and_then(Option::as_ref);
    let unmade: Vec<Mic> = match newest.map(|c| &c.mics) {
        Some(Err(why)) => {
            let note = format!(
                "{id} runs the newest AudioCodec package, but Windows has no {lacked} on it, and the package's INF couldn't be read \
                 to say whether it makes {} ({why})",
                if check.missing.len() == 1 { "it" } else { "them" }
            );
            return Ok(KeptCheck::Recreate { missing: check.missing, note });
        }
        Some(Ok(makes)) => check.missing.iter().filter(|m| !makes.contains(m)).copied().collect(),
        None => check.missing.clone(),
    };
    if unmade.is_empty() {
        let note = format!(
            "{id} runs the newest AudioCodec package, which makes {lacked} (its INF's interfaces for {}), but Windows has no \
             {lacked} on it (a switch whose microphones didn't come, say)",
            target.hardware_id()
        );
        return Ok(KeptCheck::Recreate { missing: check.missing, note });
    }
    Err(SwitchError::PackageLacks(Box::new(Lacking {
        target,
        device: Some((id.clone(), driver_text(device.inf.as_deref(), device.driver_version.as_deref()))),
        newest: newest.map(Candidate::describe).unwrap_or_default(),
        missing: unmade,
        ranked_below: best_with(target, &device.others).map(Candidate::describe),
    })))
}

/// The best ranked of `others` (ours, for the target's hardware ID) that makes all the target's microphones, by its INF.
fn best_with(target: Variant, others: &[Candidate]) -> Option<&Candidate> {
    others.iter().filter(|c| c.is_ours() && c.makes(target.mics()) == Some(true)).max_by_key(|c| (c.date, c.version))
}

/// A check's microphones, a line each (`  the Call Microphone: "Call Microphone (2- AudioCodec Device)"`), then a warning for
/// each disabled in the Sound panel.
pub fn found_lines(check: &MicCheck) -> Vec<String> {
    let mut lines: Vec<String> = check
        .found
        .iter()
        .map(|f| format!("  {}: \"{}\"{}", f.mic.describe(), f.name, if f.active { "" } else { ", disabled in the Sound panel" }))
        .collect();
    lines.extend(check.found.iter().filter(|f| !f.active).map(|f| format!("  warning: {}", disabled_note(&f.name))));
    lines
}

/// What to do about a microphone disabled in the Sound panel.
pub fn disabled_note(name: &str) -> String {
    format!(
        "\"{name}\" is disabled in the Sound panel, so apps don't see it: enable it there (mmsys.cpl, the Recording tab: right-click \
         it, Enable; right-click the list, Show Disabled Devices, if it isn't listed)"
    )
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

/// Where the switch's lock lives: a mutex in a private namespace (`CreatePrivateNamespaceW`), which only a caller within its
/// boundary may create or open (its token must hold the boundary's SID), so no other process can squat the name, as any could
/// a `Global\` one (creating a `Global\` mutex needs no privilege).
#[derive(Clone, Copy, Debug)]
pub struct LockSpace<'a> {
    /// The boundary descriptor's name: with [`sid`](Self::sid), what identifies the namespace.
    pub boundary: &'a str,
    /// The SID a caller's token must hold.
    pub sid: WELL_KNOWN_SID_TYPE,
    /// The namespace's and the mutex's security descriptor, in SDDL.
    pub sddl: &'a str,
    /// The start of the namespace's prefix in this process ([`fresh_alias`]): the mutex is `prefix\mutex`.
    pub alias: &'a str,
    pub mutex: &'a str,
}

/// The switch's lock: bounded by BUILTIN\Administrators (an elevated administrator, or SYSTEM, is within it), and open to
/// Administrators and SYSTEM, whichever account's switch made it.
pub const LOCK: LockSpace<'static> = LockSpace {
    boundary: "noise-filter-mic-variant",
    sid: WinBuiltinAdministratorsSid,
    sddl: "D:P(A;;GA;;;BA)(A;;GA;;;SY)",
    alias: "noise-filter",
    mutex: "mic-variant",
};

/// How often a switch looks for the namespace again when it went away between a create that found it and the open.
const NAMESPACE_TRIES: usize = 4;

/// A boundary descriptor, deleted on drop.
struct Boundary(HANDLE);

impl Drop for Boundary {
    fn drop(&mut self) {
        // SAFETY: our descriptor, deleted once.
        unsafe { DeleteBoundaryDescriptor(self.0) };
    }
}

/// An open private namespace, closed on drop but not destroyed (a switch waiting in it still uses it).
struct Namespace(HANDLE);

impl Drop for Namespace {
    fn drop(&mut self) {
        // SAFETY: our namespace handle, closed once; no PRIVATE_NAMESPACE_FLAG_DESTROY.
        let _ = unsafe { ClosePrivateNamespace(self.0, 0) };
    }
}

/// A security descriptor made from SDDL, freed on drop.
struct Descriptor(PSECURITY_DESCRIPTOR);

impl Descriptor {
    fn from_sddl(sddl: &str) -> Result<Self, WinError> {
        let sddl = HSTRING::from(sddl);
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: a NUL-terminated string; `sd` is a live out-pointer, freed by Drop.
        unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(&sddl, SDDL_REVISION_1, &mut sd, None) }
            .map_err(WinError::of("ConvertStringSecurityDescriptorToSecurityDescriptorW"))?;
        Ok(Self(sd))
    }

    /// Attributes that point at it: valid while it lives.
    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: false.into(),
        }
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: the descriptor ConvertStringSecurityDescriptorToSecurityDescriptorW allocated with LocalAlloc, freed once.
        let _ = unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
    }
}

/// A well-known SID, in a buffer aligned for one.
fn well_known_sid(kind: WELL_KNOWN_SID_TYPE) -> Result<Vec<u32>, WinError> {
    let mut sid = vec![0u32; (SECURITY_MAX_SID_SIZE as usize).div_ceil(size_of::<u32>())];
    let mut size = (sid.len() * size_of::<u32>()) as u32;
    // SAFETY: no domain; the buffer holds `size` bytes and is aligned for a SID; `size` is a live in-out pointer.
    unsafe { CreateWellKnownSid(kind, None, Some(PSID(sid.as_mut_ptr().cast())), &mut size) }
        .map_err(WinError::of("CreateWellKnownSid"))?;
    Ok(sid)
}

/// A lock call that failed with `code`: access denied is a caller outside the boundary, or one the descriptor keeps out.
fn lock_error(call: &'static str, code: u32) -> SwitchError {
    let e = WinError { call, code };
    if code == ERROR_ACCESS_DENIED.0 { SwitchError::LockDenied(e) } else { SwitchError::Windows(e) }
}

/// A namespace prefix this process hasn't used yet (`noise-filter-0`): Windows refuses a process a second open of a namespace
/// under a prefix it has open (ERROR_DUP_NAME), as two takes in one process would be (the tests' threads).
fn fresh_alias(start: &str) -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!("{start}-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// The last error, for a call that failed.
fn last_error() -> u32 {
    // SAFETY: no arguments; the callers read it straight after the failed call.
    unsafe { GetLastError() }.0
}

impl Namespace {
    /// Creates the namespace, or opens it if it's there (another switch's).
    fn open(space: &LockSpace, alias: &str, sd: &Descriptor) -> Result<Self, SwitchError> {
        let name = HSTRING::from(space.boundary);
        // SAFETY: a NUL-terminated name; no flags. Null on failure.
        let mut boundary = Boundary(unsafe { CreateBoundaryDescriptorW(&name, 0) });
        if boundary.0.is_invalid() {
            return Err(WinError { call: "CreateBoundaryDescriptorW", code: last_error() }.into());
        }
        let mut sid = well_known_sid(space.sid)?;
        // SAFETY: our descriptor, which the call may reallocate through the pointer; a SID that outlives the call (it's copied).
        unsafe { AddSIDToBoundaryDescriptor(&mut boundary.0, PSID(sid.as_mut_ptr().cast())) }
            .map_err(WinError::of("AddSIDToBoundaryDescriptor"))?;
        let alias = HSTRING::from(alias);
        let attributes = sd.attributes();
        let descriptor = boundary.0.0.cast_const();
        for _ in 0..NAMESPACE_TRIES {
            // SAFETY: attributes whose descriptor lives in `sd`; our boundary descriptor; a NUL-terminated prefix. Null on failure.
            let created = unsafe { CreatePrivateNamespaceW(Some(&attributes), descriptor, &alias) };
            if !created.is_invalid() {
                return Ok(Self(created));
            }
            let code = last_error();
            if code != ERROR_ALREADY_EXISTS.0 {
                return Err(lock_error("CreatePrivateNamespaceW", code));
            }
            // SAFETY: as above.
            let opened = unsafe { OpenPrivateNamespaceW(descriptor, &alias) };
            if !opened.is_invalid() {
                return Ok(Self(opened));
            }
            let code = last_error();
            // Gone between the two calls (its last holder closed it): create it again
            if code != ERROR_FILE_NOT_FOUND.0 && code != ERROR_PATH_NOT_FOUND.0 {
                return Err(lock_error("OpenPrivateNamespaceW", code));
            }
        }
        Err(WinError { call: "OpenPrivateNamespaceW", code: ERROR_FILE_NOT_FOUND.0 }.into())
    }
}

/// The switch's lock, a mutex in its private namespace; released when dropped.
struct SwitchLock {
    mutex: Owned<HANDLE>,
    /// Kept open while the mutex is held, so a switch that comes meanwhile opens this namespace rather than making another.
    _namespace: Namespace,
}

impl SwitchLock {
    /// Takes the mutex in `space`, waiting up to `wait` for another holder.
    fn take(space: &LockSpace, wait: Duration, say: &mut dyn FnMut(&str)) -> Result<Self, SwitchError> {
        let sd = Descriptor::from_sddl(space.sddl)?;
        let alias = fresh_alias(space.alias);
        let namespace = Namespace::open(space, &alias, &sd)?;
        let name = HSTRING::from(format!(r"{alias}\{}", space.mutex));
        // Enough to wait on it and release it
        let access = SYNCHRONIZATION_SYNCHRONIZE.0 | MUTEX_MODIFY_STATE.0;
        let attributes = sd.attributes();
        // SAFETY: attributes whose descriptor lives in `sd`; a NUL-terminated name in the namespace just opened; not owned on
        // creation, so it is waited for below whoever made it.
        let handle =
            unsafe { CreateMutexExW(Some(&attributes), &name, 0, access) }.map_err(|e| lock_error("CreateMutexExW", win32_code(&e)))?;
        // SAFETY: a valid handle that nothing else owns; Owned closes it once.
        let mutex = unsafe { Owned::new(handle) };
        // SAFETY: our handle; no wait.
        let mut event = unsafe { WaitForSingleObject(*mutex, 0) };
        if event == WAIT_TIMEOUT {
            say(&format!("Another switch is running: waiting for it to end (up to {} s)", wait.as_secs()));
            // SAFETY: as above.
            event = unsafe { WaitForSingleObject(*mutex, u32::try_from(wait.as_millis()).unwrap_or(u32::MAX - 1)) };
        }
        match lock_wait(event) {
            LockWait::Taken => Ok(Self { mutex, _namespace: namespace }),
            LockWait::Abandoned => {
                say("(The last switch's process ended in the middle of its switch: this one reads the devices afresh.)");
                Ok(Self { mutex, _namespace: namespace })
            }
            LockWait::Busy => Err(SwitchError::Busy),
            LockWait::Failed => Err(WinError { call: "WaitForSingleObject", code: last_error() }.into()),
        }
    }
}

impl Drop for SwitchLock {
    fn drop(&mut self) {
        // SAFETY: our handle, to a mutex this thread owns (take returns one only then). The fields drop after this: the
        // mutex's handle is closed, then the namespace's.
        let _ = unsafe { ReleaseMutex(*self.mutex) };
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
            let (newest, others) = rank(self.packages_for(&mut data));
            let device = OurDevice { instance_id: self.instance_id(&data)?, variant, health, inf, driver_version, newest, others };
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

    /// The drivers compatible with an element: the compatible-driver search on it (which only reads the driver store).
    fn packages_for(&self, data: &mut SP_DEVINFO_DATA) -> Result<Vec<Candidate>, WinError> {
        // SAFETY: an element of our set.
        unsafe { SetupDiBuildDriverInfoList(self.get(), Some(data), SPDIT_COMPATDRIVER) }
            .map_err(WinError::of("SetupDiBuildDriverInfoList"))?;
        Ok(self.compat_drivers(data)?.into_iter().map(|(_, c)| c).collect())
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
            let (inf, section) = self.driver_inf(data, &node)?;
            let original = original_inf_name(&inf).map_err(|e| e.to_string());
            let mics = package_mics(&inf, &section).map_err(|e| e.to_string());
            let candidate = Candidate {
                original,
                mics,
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

    /// The INF a driver node comes from (`C:\Windows\INF\oem12.inf`), and the install section its models line names for the
    /// hardware ID (`Audio_Device`).
    fn driver_inf(&self, data: &SP_DEVINFO_DATA, node: &SP_DRVINFO_DATA_V2_W) -> Result<(String, String), WinError> {
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
        // SAFETY: the call filled the fixed part; the fields are read by value, through the raw pointer.
        let (name, section) = unsafe { ((&raw const (*detail).InfFileName).read(), (&raw const (*detail).SectionName).read()) };
        Ok((from_wide(&name), from_wide(&section)))
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
    /// The microphones it makes for the hardware ID it was found for, by its INF ([`package_mics`]), or why they couldn't
    /// be read.
    pub mics: Result<Vec<Mic>, String>,
}

impl Candidate {
    /// Whether it's our INF (by its original name).
    pub fn is_ours(&self) -> bool {
        self.original.as_deref().is_ok_and(|n| n.eq_ignore_ascii_case(INF_NAME))
    }

    /// Whether it makes all of `mics`, by its INF: `None` if that couldn't be read.
    pub fn makes(&self, mics: &[Mic]) -> Option<bool> {
        self.mics.as_ref().ok().map(|has| mics.iter().all(|m| has.contains(m)))
    }

    /// "the Microphone and the Call Microphone", "no microphone", or why its INF couldn't be read.
    pub fn mics_note(&self) -> String {
        match &self.mics {
            Ok(mics) if mics.is_empty() => "makes no microphone".into(),
            Ok(mics) => format!("makes {}", mics.iter().map(|m| m.describe()).collect::<Vec<_>>().join(" and ")),
            Err(why) => format!("its microphones unread: {why}"),
        }
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

/// A search's drivers (or its error) as the newest of ours ([`choose`]) and the rest of ours, ranked below it.
pub fn rank(found: Result<Vec<Candidate>, WinError>) -> (Result<Option<Candidate>, WinError>, Vec<Candidate>) {
    match found {
        Err(e) => (Err(e), Vec::new()),
        Ok(all) => {
            let best = choose(&all);
            let others = all.iter().enumerate().filter(|&(i, c)| Some(i) != best && c.is_ours()).map(|(_, c)| c.clone()).collect();
            (Ok(best.map(|i| all[i].clone())), others)
        }
    }
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
            let mics = if c.is_ours() { format!(" ({})", c.mics_note()) } else { String::new() };
            say(&format!("  {}{mark}{mics}", c.describe()));
        }
        let Some(chosen) = chosen else { return Err(SwitchError::NoDriver { hardware_id: target.hardware_id() }) };
        // A package that doesn't make the variant's microphones, by its INF, would fail the check after the install: stop
        // before anything is removed (one whose INF couldn't be read is checked then)
        if let Some(lacking) = new_package_lacks(target, &candidates[chosen], &rank(Ok(candidates.clone())).1) {
            return Err(SwitchError::PackageLacks(Box::new(lacking)));
        }
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

/// What a new device's package (`newest`, chosen for `target`) lacks of the target's microphones, by its INF, and the best of
/// `others` (ours, ranked below it) that has them all; `None` if it has them, or its INF couldn't be read.
pub fn new_package_lacks(target: Variant, newest: &Candidate, others: &[Candidate]) -> Option<Lacking> {
    let makes = newest.mics.as_ref().ok()?;
    let missing: Vec<Mic> = target.mics().iter().filter(|m| !makes.contains(m)).copied().collect();
    (!missing.is_empty()).then(|| Lacking {
        target,
        device: None,
        newest: newest.describe(),
        missing,
        ranked_below: best_with(target, others).map(Candidate::describe),
    })
}

/// A devnode's state.
pub(crate) fn health(devinst: u32) -> Health {
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

/// The microphones a package makes for a hardware ID, by its INF (`inf_path`, `C:\Windows\INF\oem12.inf`) and the install
/// section the ID's models line names (`section`, the driver node's: `Audio_Device`): the reference strings of the
/// AddInterface lines in the section's `.Interfaces`, as the platform decorates it (`Audio_Device.NT.Interfaces`), each by
/// [`sessions::mic_of_filter`]; in [`Variant::mics`]' order. The single-microphone driver's `Audio_Device` has `Microphone0`
/// alone; the three variants' has both, its `Audio_Device_Call` `Microphone1`. Any user may read it.
pub fn package_mics(inf_path: &str, section: &str) -> Result<Vec<Mic>, WinError> {
    let inf = InfFile::open(inf_path)?;
    let interfaces = HSTRING::from(format!("{}.Interfaces", inf.actual_section(section)?));
    let key = w!("AddInterface");
    let mut found = Vec::new();
    let mut line = INFCONTEXT::default();
    // SAFETY: an open INF; NUL-terminated names; `line` is a live out-structure. It fails when there's no such line (or
    // section): no interfaces.
    let mut more = unsafe { SetupFindFirstLineW(inf.0, &interfaces, key, &mut line) }.is_ok();
    while more {
        // Field 2 of `AddInterface={guid},reference,section`: the reference string, the INF's %strings% substituted
        if let Ok(reference) = string_field(&line, 2) {
            found.push(sessions::mic_of_filter(&reference));
        }
        let current = line;
        // SAFETY: a line of the INF, which is still open; `line` is a live out-structure.
        more = unsafe { SetupFindNextMatchLineW(&current, key, &mut line) }.is_ok();
    }
    Ok([Mic::LowLatency, Mic::Call].into_iter().filter(|m| found.contains(m)).collect())
}

/// An INF opened with SetupOpenInfFileW, closed on drop.
struct InfFile(*mut std::ffi::c_void);

impl InfFile {
    fn open(path: &str) -> Result<Self, WinError> {
        let name = HSTRING::from(path);
        // SAFETY: a NUL-terminated path; no class to match; no error line wanted. INVALID_HANDLE_VALUE on failure.
        let handle = unsafe { SetupOpenInfFileW(&name, PCWSTR::null(), INF_STYLE_WIN4, None) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE.0 {
            return Err(WinError { call: "SetupOpenInfFileW", code: last_error() });
        }
        Ok(Self(handle))
    }

    /// The decorated install section the platform uses (`Audio_Device.NT` for `Audio_Device`).
    fn actual_section(&self, section: &str) -> Result<String, WinError> {
        let name = HSTRING::from(section);
        // Section names are at most 255 characters (MAX_INF_SECTION_NAME_LENGTH)
        let mut buf = vec![0u16; 256];
        // SAFETY: an open INF; a NUL-terminated name; the buffer's length goes with it.
        unsafe { SetupDiGetActualSectionToInstallW(self.0, &name, Some(&mut buf), None, None) }
            .map_err(WinError::of("SetupDiGetActualSectionToInstallW"))?;
        Ok(from_wide(&buf))
    }
}

impl Drop for InfFile {
    fn drop(&mut self) {
        // SAFETY: our handle, closed once.
        unsafe { SetupCloseInfFile(self.0) };
    }
}

/// A field of an INF line, its %strings% substituted.
fn string_field(line: &INFCONTEXT, index: u32) -> Result<String, WinError> {
    // An INF string is at most 4096 characters (MAX_INF_STRING_LENGTH), and its NUL
    let mut buf = vec![0u16; 4097];
    // SAFETY: a line of an open INF; the buffer's length goes with it.
    unsafe { SetupGetStringFieldW(line, index, Some(&mut buf), None) }.map_err(WinError::of("SetupGetStringFieldW"))?;
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
    use windows::Win32::Security::{
        CheckTokenMembership, CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, ImpersonateLoggedOnUser, RevertToSelf, SID_AND_ATTRIBUTES,
        TOKEN_DUPLICATE, TOKEN_IMPERSONATE, TOKEN_QUERY, WinAuthenticatedUserSid, WinWorldSid,
    };
    use windows::Win32::System::Threading::{CREATE_MUTEX_INITIAL_OWNER, GetCurrentProcess, OpenProcessToken};

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
            others: vec![],
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

    /// The new device's instance ID in these tests, as SetupDi gives it; its endpoints' filter paths have it in lower case.
    const NEW_ID: &str = r"ROOT\MEDIA\0005";
    const NEW: &str = r"root\media\0005";
    /// The old device's (the one [`dev`] and [`old`] make).
    const OLD: &str = r"root\media\0003";

    /// An endpoint of ours, of kind `mic`, on `device` (`""`: its filter wasn't read, so its device is unknown).
    fn ep(id: &str, name: &str, mic: Mic, device: &str, active: bool) -> CaptureEndpoint {
        CaptureEndpoint { id: id.into(), name: name.into(), active, mic, device: (!device.is_empty()).then(|| device.into()) }
    }

    fn found(mic: Mic, name: &str, active: bool) -> Found {
        Found { mic, name: name.into(), active }
    }

    fn wanted(target: Variant, before: &[String]) -> Wanted<'_> {
        Wanted { target, device: NEW_ID, before }
    }

    /// Both microphones of the new device, active, by their default names.
    fn both_new() -> Vec<CaptureEndpoint> {
        vec![ep("{n1}", MICROPHONE, Mic::LowLatency, NEW, true), ep("{n2}", CALL_MICROPHONE, Mic::Call, NEW, true)]
    }

    /// Astra's review: check the capabilities before declaring success. A new device's microphones are its endpoints, by kind,
    /// whatever they're named.
    #[test]
    fn the_microphones_are_the_devices_by_kind() {
        use Mic::*;
        let check = check_mics(&wanted(Variant::Both, &[]), &both_new());
        assert_eq!(check.found, [found(LowLatency, MICROPHONE, true), found(Call, CALL_MICROPHONE, true)]);
        assert!(check.is_done() && check.missing.is_empty() && check.extra.is_empty());
        // The upgrade from the single-microphone driver: its Microphone, still active on the old device, doesn't count, and is
        // waited out
        let before = ["{old}".to_string()];
        let midway = [ep("{old}", MICROPHONE, LowLatency, OLD, true), ep("{n1}", MICROPHONE, LowLatency, NEW, true)];
        let check = check_mics(&wanted(Variant::Both, &before), &midway);
        assert_eq!(
            (check.found, check.missing, check.extra),
            (vec![found(LowLatency, MICROPHONE, true)], vec![Call], vec![MICROPHONE.to_string()])
        );
        assert!(check_mics(&wanted(Variant::Both, &before), &both_new()).is_done());
        // Another device's, new or not, isn't this one's
        let other = [ep("{x}", MICROPHONE, LowLatency, OLD, true)];
        assert_eq!(check_mics(&wanted(Variant::LowLatency, &[]), &other).missing, [LowLatency]);
        // Its device unread: it counts if it's new, and an old id, in any case, isn't
        let unknown = [ep("{OLD}", MICROPHONE, LowLatency, "", true)];
        assert_eq!(check_mics(&wanted(Variant::LowLatency, &before), &unknown).missing, [LowLatency]);
        assert!(check_mics(&wanted(Variant::LowLatency, &[]), &unknown).is_done());
        // By kind, numbered or renamed in the Sound panel, and reported by the name it has
        let renamed = [
            ep("{n1}", "Studio mic (2- AudioCodec Device)", LowLatency, NEW, true),
            ep("{n2}", "Call Microphone (2- AudioCodec Device)", Call, NEW, true),
        ];
        let check = check_mics(&wanted(Variant::Both, &[]), &renamed);
        assert!(check.is_done(), "{check:?}");
        assert_eq!(check.found, [
            found(LowLatency, "Studio mic (2- AudioCodec Device)", true),
            found(Call, "Call Microphone (2- AudioCodec Device)", true)
        ]);
        // ... and a name doesn't make one the other kind
        let check = check_mics(&wanted(Variant::Call, &[]), &[ep("{n1}", CALL_MICROPHONE, LowLatency, NEW, true)]);
        assert_eq!((check.missing, check.extra), (vec![Call], vec![CALL_MICROPHONE.to_string()]));
        // One disabled in the Sound panel is there (the user's choice); an active one is taken before it
        let check = check_mics(&wanted(Variant::Call, &[]), &[ep("{n2}", CALL_MICROPHONE, Call, NEW, false)]);
        assert!(check.is_done() && check.found == [found(Call, CALL_MICROPHONE, false)]);
        assert_eq!(check.seen, [format!("{CALL_MICROPHONE} (disabled)")]);
        let two = [ep("{a}", CALL_MICROPHONE, Call, NEW, false), ep("{b}", "Calls (AudioCodec Device)", Call, NEW, true)];
        let check = check_mics(&wanted(Variant::Call, &[]), &two);
        assert!(check.is_done() && check.found == [found(Call, "Calls (AudioCodec Device)", true)], "{check:?}");
        // A second active one of the same kind, or one the variant doesn't have: waited out
        let two = [ep("{a}", CALL_MICROPHONE, Call, NEW, true), ep("{b}", CALL_MICROPHONE, Call, NEW, true)];
        let check = check_mics(&wanted(Variant::Call, &[]), &two);
        assert!(check.missing.is_empty() && check.extra == [CALL_MICROPHONE] && !check.is_done());
        let mut after = both_new();
        after.push(ep("{x}", "Line (AudioCodec Device)", Other, NEW, true));
        let check = check_mics(&wanted(Variant::LowLatency, &[]), &after);
        assert_eq!(check.found, [found(LowLatency, MICROPHONE, true)]);
        assert_eq!(check.extra, [CALL_MICROPHONE, "Line (AudioCodec Device)"]);
        // Nothing yet
        let check = check_mics(&wanted(Variant::Both, &[]), &[]);
        assert_eq!((check.missing, check.seen.len()), (vec![LowLatency, Call], 0));
    }

    /// Astra's review: with the single-microphone package alone in the driver store, its device runs the newest package, so
    /// `mic variant both` said there was nothing to do. A device kept is checked once, before anything changes.
    #[test]
    fn a_kept_device_must_have_the_variants_microphones() {
        use Mic::*;
        let only_low = Ok(vec![ep("{m}", MICROPHONE, LowLatency, OLD, true)]);
        // The single-microphone package alone in the driver store: no package makes the Call Microphone
        let d = OurDevice { newest: Ok(Some(single())), ..dev(Variant::Both, Health::Working) };
        let e = check_kept(Variant::Both, &d, &only_low).unwrap_err();
        assert_eq!(
            e,
            SwitchError::PackageLacks(Box::new(Lacking {
                target: Variant::Both,
                device: Some((r"ROOT\MEDIA\0003".into(), "driver oem12.inf 1.0.0.0".into())),
                newest: single().describe(),
                missing: vec![Call],
                ranked_below: None,
            }))
        );
        let text = e.to_string();
        assert!(
            text.starts_with(
                r#"ROOT\MEDIA\0003 works on the driver store's newest AudioCodec package (driver oem12.inf 1.0.0.0), but Windows has no "Call Microphone (AudioCodec Device)" on it: that package doesn't make it (its INF's interfaces for ROOT\AudioCodec; the single-microphone driver's has the Microphone alone). No other AudioCodec package in the driver store makes it. Add the package"#
            ),
            "{text}"
        );
        assert!(text.contains("pnputil /add-driver AudioCodec.inf), then run `noise-filter mic variant both`: it re-creates"), "{text}");
        assert!(text.ends_with("with that package. Nothing was changed."), "{text}");
        let none = check_kept(Variant::Both, &d, &Ok(vec![])).unwrap_err();
        let SwitchError::PackageLacks(lacking) = &none else { panic!("{none:?}") };
        assert_eq!(lacking.missing, [Call], "the Microphone, which the package makes, isn't what it lacks");
        assert!(none.to_string().contains(r#"no "Call Microphone (AudioCodec Device)" on it: that package doesn't make it"#), "{none}");

        // Astra's review of a3421c6: the three variants' package staged, but ranked below it by DriverVer: say how they rank
        let ranked = OurDevice { others: vec![staged()], ..d.clone() };
        let text = check_kept(Variant::Both, &ranked, &only_low).unwrap_err().to_string();
        let expected = format!(
            "{} makes it, but ranks below the newest, {}: Plug and Play, and the switch, take the package whose DriverVer has the \
             latest date, then the highest version.",
            staged().describe(),
            single().describe()
        );
        assert!(text.contains(&expected), "{text}");
        assert!(text.contains("Build the three variants' package again") && text.ends_with("Nothing was changed."), "{text}");
        // ... but not one that lacks it too
        let also_single = OurDevice { others: vec![Candidate { inf: r"C:\Windows\INF\oem2.inf".into(), ..single() }], ..d.clone() };
        assert!(check_kept(Variant::Both, &also_single, &only_low).unwrap_err().to_string().contains("No other AudioCodec package"));

        // Astra's review of a3421c6: the newest package makes it, but Windows didn't (a switch whose microphones didn't come):
        // re-created, not blamed on the package
        let d = dev(Variant::Both, Health::Working);
        let KeptCheck::Recreate { missing, note } = check_kept(Variant::Both, &d, &only_low).unwrap() else { panic!() };
        assert_eq!(missing, [Call]);
        assert_eq!(
            note,
            r#"ROOT\MEDIA\0003 runs the newest AudioCodec package, which makes "Call Microphone (AudioCodec Device)" (its INF's interfaces for ROOT\AudioCodec), but Windows has no "Call Microphone (AudioCodec Device)" on it (a switch whose microphones didn't come, say)"#
        );
        // Its INF unread: it may make it, so re-created too
        let unread_inf =
            OurDevice { newest: Ok(Some(Candidate { mics: Err("SetupOpenInfFileW failed".into()), ..newest() })), ..d.clone() };
        let KeptCheck::Recreate { note, .. } = check_kept(Variant::Both, &unread_inf, &only_low).unwrap() else { panic!() };
        assert!(note.ends_with("and the package's INF couldn't be read to say whether it makes it (SetupOpenInfFileW failed)"), "{note}");
        // The plan then re-creates it
        let mut p = plan(std::slice::from_ref(&d), Variant::Both);
        assert!(p.is_nothing());
        p.recreate_kept();
        assert_eq!(p, Plan { keep: None, remove: vec![0], create: true });

        // Both there: kept; one disabled passes, with a warning
        let both = Ok(vec![ep("{m}", MICROPHONE, LowLatency, OLD, true), ep("{c}", CALL_MICROPHONE, Call, OLD, false)]);
        let KeptCheck::Has(check) = check_kept(Variant::Both, &d, &both).unwrap() else { panic!() };
        let lines = found_lines(&check);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0], "  the Microphone: \"Microphone (AudioCodec Device)\"");
        assert_eq!(lines[1], "  the Call Microphone: \"Call Microphone (AudioCodec Device)\", disabled in the Sound panel");
        assert!(
            lines[2].starts_with(
                "  warning: \"Call Microphone (AudioCodec Device)\" is disabled in the Sound panel, so apps don't see it: enable it \
                 there"
            ),
            "{}",
            lines[2]
        );
        // Another device's microphone isn't the kept one's: it lacks it, so it is re-created (its package makes it)
        let call = dev(Variant::Call, Health::Working);
        let elsewhere = Ok(vec![ep("{c}", CALL_MICROPHONE, Call, NEW, true)]);
        assert!(matches!(check_kept(Variant::Call, &call, &elsewhere), Ok(KeptCheck::Recreate { .. })));
        // Unread: not checked, so it stops, nothing changed
        let unread = WinError { call: "IMMDeviceEnumerator::EnumAudioEndpoints", code: 5 };
        assert_eq!(check_kept(Variant::Both, &d, &Err(unread.clone())), Err(SwitchError::NoEndpoints(unread)));
    }

    /// A clock that moves only when slept on.
    struct FakeClock(Duration);

    impl Clock for FakeClock {
        fn elapsed(&self) -> Duration {
            self.0
        }

        fn sleep(&mut self, d: Duration) {
            self.0 += d;
        }
    }

    /// What [`wait_for_mics`] does for both microphones on the new device over `looks` (a read each, the last one again
    /// after the list), with 1 s to wait: its result, its lines, and the time it took.
    fn wait_over(looks: &[Result<Vec<CaptureEndpoint>, WinError>]) -> (Result<(), SwitchError>, Vec<String>, Duration) {
        let mut reads = 0;
        let mut read = || {
            reads += 1;
            looks[(reads - 1).min(looks.len() - 1)].clone()
        };
        let mut clock = FakeClock(Duration::ZERO);
        let mut lines = Vec::new();
        let result =
            wait_for_mics(&wanted(Variant::Both, &[]), Duration::from_secs(1), &mut read, &mut clock, &mut |l| lines.push(l.to_string()));
        (result, lines, clock.0)
    }

    /// Astra's review: the wait's loop had no test.
    #[test]
    fn the_wait_for_the_microphones_ends_as_it_should() {
        let low_only = vec![ep("{n1}", MICROPHONE, Mic::LowLatency, NEW, true)];
        let failed = WinError { call: "IMMDeviceEnumerator::EnumAudioEndpoints", code: 5 };
        // Done on the first look: no sleep
        let (result, lines, took) = wait_over(&[Ok(both_new())]);
        assert_eq!((result, took), (Ok(()), Duration::ZERO));
        assert_eq!(lines, [
            r#"Waiting for Windows to make "Microphone (AudioCodec Device)" and "Call Microphone (AudioCodec Device)" on ROOT\MEDIA\0005 (up to 1 s)"#,
            r#"  the Microphone: "Microphone (AudioCodec Device)""#,
            r#"  the Call Microphone: "Call Microphone (AudioCodec Device)""#,
            "  after 0.0 s",
        ]);
        // Done later, a look every POLL
        let (result, lines, took) = wait_over(&[Ok(vec![]), Ok(low_only.clone()), Ok(both_new())]);
        assert_eq!((result, took), (Ok(()), POLL * 2));
        assert_eq!(lines.last().unwrap(), "  after 0.5 s");
        // A failed read is looked at again
        let (result, _, took) = wait_over(&[Err(failed.clone()), Ok(both_new())]);
        assert_eq!((result, took), (Ok(()), POLL));
        // A failed last read, after a look that missed one: the last good look's, with the failure
        let (result, _, took) = wait_over(&[Ok(low_only.clone()), Err(failed.clone())]);
        assert_eq!(took, Duration::from_secs(1));
        assert_eq!(result, Err(SwitchError::Mics { missing: vec![Mic::Call], seen: vec![MICROPHONE.into()], read: Some(failed.clone()) }));
        // Every read failed
        let (result, _, _) = wait_over(&[Err(failed.clone())]);
        assert_eq!(result, Err(SwitchError::Mics { missing: vec![Mic::LowLatency, Mic::Call], seen: vec![], read: Some(failed.clone()) }));
        // At the deadline with the microphones there and an old one still active: done, and it says so
        let mut going = both_new();
        going.push(ep("{old}", MICROPHONE, Mic::LowLatency, OLD, true));
        let (result, lines, took) = wait_over(&[Ok(going.clone())]);
        assert_eq!((result, took), (Ok(()), Duration::from_secs(1)));
        assert!(
            lines.contains(
                &r#"  also still active: "Microphone (AudioCodec Device)" (a removed device's, going, or another device's)"#.into()
            ),
            "{lines:?}"
        );
        assert_eq!(lines.last().unwrap(), "  after 1.0 s");
        // ... the same when the last read failed after them
        let (result, _, _) = wait_over(&[Ok(going), Err(failed)]);
        assert_eq!(result, Ok(()));
        // A microphone that never comes: a timeout
        let (result, _, took) = wait_over(&[Ok(low_only)]);
        assert_eq!(took, Duration::from_secs(1));
        assert_eq!(result, Err(SwitchError::Mics { missing: vec![Mic::Call], seen: vec![MICROPHONE.into()], read: None }));
    }

    #[test]
    fn a_wait_for_the_lock_ends_as_windows_says() {
        assert_eq!(lock_wait(WAIT_OBJECT_0), LockWait::Taken);
        assert_eq!(lock_wait(WAIT_ABANDONED), LockWait::Abandoned);
        assert_eq!(lock_wait(WAIT_TIMEOUT), LockWait::Busy);
        assert_eq!(lock_wait(windows::Win32::Foundation::WAIT_FAILED), LockWait::Failed);
    }

    /// A lock of the test's own, so a real switch is never in the way: a boundary named for this process and `which`, with
    /// the Everyone SID (every token has it) and a descriptor that lets everyone in, so the test needn't be elevated. The
    /// switch's own ([`LOCK`]) differs in those alone.
    fn test_lock(which: &str) -> String {
        format!("noise-filter-test-{which}-{}", std::process::id())
    }

    fn space(boundary: &str) -> LockSpace<'_> {
        // The alias is the boundary's name: the tests run on threads of one process, each with its own namespace
        LockSpace { boundary, sid: WinWorldSid, sddl: "D:P(A;;GA;;;WD)", alias: boundary, mutex: "mic-variant" }
    }

    /// Takes the lock on another thread, as a second switch would: whether it got it, and what it said.
    fn take_elsewhere(boundary: &str, wait: Duration) -> (Result<(), SwitchError>, Vec<String>) {
        let boundary = boundary.to_string();
        std::thread::spawn(move || {
            let mut lines = Vec::new();
            let taken = SwitchLock::take(&space(&boundary), wait, &mut |l| lines.push(l.to_string())).map(drop);
            (taken, lines)
        })
        .join()
        .unwrap()
    }

    /// Astra's review: two switches at once could both create a device. Held, another thread waits and gives up; released,
    /// it takes it; abandoned by a thread that ended holding it, the next takes it and says so.
    #[test]
    fn the_lock_lets_one_switch_in_at_a_time() {
        let boundary = test_lock("one-at-a-time");
        let held = SwitchLock::take(&space(&boundary), Duration::ZERO, &mut |_| {}).unwrap();
        let (taken, lines) = take_elsewhere(&boundary, Duration::from_millis(50));
        assert_eq!(taken, Err(SwitchError::Busy));
        assert_eq!(lines, ["Another switch is running: waiting for it to end (up to 0 s)"]);
        drop(held);
        assert_eq!(take_elsewhere(&boundary, Duration::ZERO), (Ok(()), vec![]));
        let b = boundary.clone();
        std::thread::spawn(move || std::mem::forget(SwitchLock::take(&space(&b), Duration::ZERO, &mut |_| {}).unwrap())).join().unwrap();
        let (taken, lines) = take_elsewhere(&boundary, Duration::ZERO);
        assert_eq!(taken, Ok(()));
        assert_eq!(lines, ["(The last switch's process ended in the middle of its switch: this one reads the devices afresh.)"]);
    }

    /// The switch that made the namespace ends while another waits in it: the next still finds the waiter's lock (the
    /// namespace lives while a holder has it open), not a new one beside it.
    #[test]
    fn the_lock_outlives_the_switch_that_made_its_namespace() {
        use std::sync::mpsc;
        let boundary = test_lock("outlives");
        let first = SwitchLock::take(&space(&boundary), Duration::ZERO, &mut |_| {}).unwrap();
        let (waiting, waits) = mpsc::channel::<()>();
        let (taken, has_it) = mpsc::channel::<()>();
        let (release, released) = mpsc::channel::<()>();
        let b = boundary.clone();
        let second = std::thread::spawn(move || {
            let lock = SwitchLock::take(&space(&b), Duration::from_secs(10), &mut |_| waiting.send(()).unwrap()).unwrap();
            taken.send(()).unwrap();
            released.recv().unwrap();
            drop(lock);
        });
        waits.recv().unwrap();
        drop(first);
        has_it.recv().unwrap();
        assert_eq!(take_elsewhere(&boundary, Duration::ZERO).0, Err(SwitchError::Busy), "the second switch holds it");
        release.send(()).unwrap();
        second.join().unwrap();
        assert_eq!(take_elsewhere(&boundary, Duration::ZERO).0, Ok(()));
    }

    /// Astra's review: any process could squat `Global\noise-filter-mic-variant`. Neither a plain mutex of the name nor a
    /// namespace of the same boundary name with another SID (one a squatter could make) is the lock.
    #[test]
    fn a_squatted_name_isnt_the_lock() {
        let boundary = test_lock("squatted");
        let plain = HSTRING::from(format!(r"Local\{boundary}"));
        // SAFETY: no attributes; a NUL-terminated name; owned on creation.
        let squatter = unsafe { CreateMutexExW(None, &plain, CREATE_MUTEX_INITIAL_OWNER, SYNCHRONIZATION_SYNCHRONIZE.0) }.unwrap();
        // SAFETY: a valid handle that nothing else owns.
        let _squatter = unsafe { Owned::new(squatter) };
        // Another process's alias would be its own
        let alias = format!("{boundary}-squatter");
        let other_sid = LockSpace { sid: WinAuthenticatedUserSid, alias: &alias, ..space(&boundary) };
        let squatted = SwitchLock::take(&other_sid, Duration::ZERO, &mut |_| {}).unwrap();
        assert_eq!(take_elsewhere(&boundary, Duration::ZERO), (Ok(()), vec![]));
        drop(squatted);
    }

    /// This process's token with BUILTIN\Administrators deny-only, as a standard user's or an unelevated administrator's has
    /// it (or no member of it at all), and its privileges dropped.
    fn token_without_administrators() -> Owned<HANDLE> {
        let mut token = HANDLE::default();
        // SAFETY: this process's pseudo handle; `token` is a live out-pointer.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_IMPERSONATE, &mut token) }.unwrap();
        // SAFETY: a valid handle that nothing else owns; Owned closes it once.
        let token = unsafe { Owned::new(token) };
        let mut admins = well_known_sid(WinBuiltinAdministratorsSid).unwrap();
        let disable = [SID_AND_ATTRIBUTES { Sid: PSID(admins.as_mut_ptr().cast()), Attributes: 0 }];
        let mut restricted = HANDLE::default();
        // SAFETY: our token; a SID that outlives the call; `restricted` is a live out-pointer.
        unsafe { CreateRestrictedToken(*token, DISABLE_MAX_PRIVILEGE, Some(&disable), None, None, &mut restricted) }.unwrap();
        // SAFETY: as above.
        unsafe { Owned::new(restricted) }
    }

    /// Astra's review: any process could hold `Global\noise-filter-mic-variant`, and block every switch for 60 s. The switch's
    /// own lock refuses a caller without Administrators enabled, at once and by name: a thread impersonating this process's
    /// token with Administrators deny-only. Refused at the namespace (its boundary, or, if a switch made it meanwhile, its
    /// descriptor), so such a caller can't make the namespace first, with a descriptor of its own, either.
    #[test]
    fn the_switchs_lock_refuses_a_caller_that_isnt_an_elevated_administrator() {
        let refused = std::thread::spawn(|| {
            let token = token_without_administrators();
            // SAFETY: a valid token, for this thread alone, reverted below.
            unsafe { ImpersonateLoggedOnUser(*token) }.unwrap();
            let refused = SwitchLock::take(&LOCK, Duration::ZERO, &mut |_| {}).map(drop);
            // SAFETY: no arguments; ends this thread's impersonation.
            unsafe { RevertToSelf() }.unwrap();
            refused
        })
        .join()
        .unwrap();
        match &refused {
            Err(SwitchError::LockDenied(e)) => assert!(matches!(e.call, "CreatePrivateNamespaceW" | "OpenPrivateNamespaceW"), "{e}"),
            _ => panic!("{refused:?}"),
        }
        let text = refused.unwrap_err().to_string();
        assert!(text.starts_with("the switch's lock refused this process (") && text.contains("ERROR_ACCESS_DENIED (5)"), "{text}");
    }

    /// Whether this thread's token has BUILTIN\Administrators enabled: an elevated administrator's (or SYSTEM's).
    fn elevated() -> bool {
        let mut admins = well_known_sid(WinBuiltinAdministratorsSid).unwrap();
        let mut member = BOOL(0);
        // SAFETY: no token, so this thread's (or the process's); a SID that outlives the call; `member` is a live out-pointer.
        unsafe { CheckTokenMembership(None, PSID(admins.as_mut_ptr().cast()), &mut member) }.unwrap();
        member.as_bool()
    }

    /// Astra's review of a3421c6: the switch's own lock (its boundary and descriptor) was tested only refusing. Elevated, this
    /// process takes it, a second take (another thread, as another switch) is Busy, and once released it can be taken again.
    /// It holds the real lock for a moment: a switch that comes meanwhile waits for it.
    #[test]
    fn the_switchs_lock_lets_an_elevated_administrator_in() {
        if !elevated() {
            eprintln!("skipped: this process isn't an elevated administrator's, so the switch's lock refuses it (the test above)");
            return;
        }
        let take = || std::thread::spawn(|| SwitchLock::take(&LOCK, Duration::ZERO, &mut |_| {}).map(drop)).join().unwrap();
        let held = match SwitchLock::take(&LOCK, Duration::ZERO, &mut |_| {}) {
            Ok(lock) => lock,
            Err(SwitchError::Busy) => {
                eprintln!("skipped: a switch holds the lock now");
                return;
            }
            Err(e) => panic!("{e}"),
        };
        assert_eq!(take(), Err(SwitchError::Busy), "held");
        drop(held);
        match take() {
            Ok(()) => {}
            // A real switch that waited for this test's take has it now
            Err(SwitchError::Busy) => eprintln!("note: a switch took the lock as soon as it was released"),
            Err(e) => panic!("{e}"),
        }
    }

    /// The microphones' filters are the INF's and the driver's.
    #[test]
    fn the_microphones_filters_are_the_drivers() {
        let driver = concat!(env!("CARGO_MANIFEST_DIR"), "/../../driver/src/AudioCodec/Driver/");
        let inf = std::fs::read(format!("{driver}AudioCodec.inf")).unwrap();
        assert_eq!(inf[..2], [0xFF, 0xFE], "UTF-16LE, with a BOM");
        let inf = String::from_utf16_lossy(&wide_of_bytes(&inf[2..]));
        assert!(inf.contains(&format!("KSNAME_Microphone=\"{MICROPHONE_FILTER}\"")));
        assert!(inf.contains(&format!("KSNAME_CallMicrophone=\"{CALL_MICROPHONE_FILTER}\"")));
        let settings = std::fs::read_to_string(format!("{driver}DriverSettings.h")).unwrap();
        assert!(settings.contains(&format!("captureCircuitName, L\"{MICROPHONE_FILTER}\")")));
        assert!(settings.contains(&format!("capture2CircuitName, L\"{CALL_MICROPHONE_FILTER}\")")));
        for v in Variant::ALL {
            assert_eq!(v.mics().iter().map(|&m| default_name(m)).collect::<Vec<_>>(), v.microphones(), "{v:?}");
        }
        assert_eq!(mics_text(&[Mic::Call]), "\"Call Microphone (AudioCodec Device)\"");
    }

    /// Astra's review of a3421c6: which microphones a package makes comes from its INF, the install section the hardware ID
    /// names. The repo's INF for each variant, the single-microphone driver's, and what isn't there.
    #[test]
    fn a_packages_microphones_are_its_infs_interfaces() {
        use Mic::*;
        let inf = concat!(env!("CARGO_MANIFEST_DIR"), r"\..\..\driver\src\AudioCodec\Driver\AudioCodec.inf");
        assert_eq!(package_mics(inf, "Audio_Device").unwrap(), [LowLatency, Call]);
        assert_eq!(package_mics(inf, "Audio_Device_Call").unwrap(), [Call]);
        assert_eq!(package_mics(inf, "Audio_Device_LowLatency").unwrap(), [LowLatency]);
        assert_eq!(package_mics(inf, "No_Such_Section").unwrap(), []);
        // The single-microphone driver's INF (before f2e7d15), cut to what's read: the speaker and one microphone
        let single = std::env::temp_dir().join(format!("nf-vmic-test-single-{}.inf", std::process::id()));
        let text = [
            "[Version]",
            "Signature=\"$WINDOWS NT$\"",
            "Class=MEDIA",
            "[Audio_Device.NT]",
            "[Audio_Device.NT.Interfaces]",
            "AddInterface=%KSCATEGORY_AUDIO%,    %KSNAME_Speaker%,  Audio_Device.I.Speaker",
            "AddInterface=%KSCATEGORY_AUDIO%,    %KSNAME_Microphone%, Audio_Device.I.Microphone",
            "AddInterface=%KSCATEGORY_CAPTURE%,  %KSNAME_Microphone%, Audio_Device.I.Microphone",
            "[Strings]",
            "KSCATEGORY_AUDIO=\"{6994AD04-93EF-11D0-A3CC-00A0C9223196}\"",
            "KSCATEGORY_CAPTURE=\"{65E8773D-8F56-11D0-A3B9-00A0C9223196}\"",
            "KSNAME_Speaker=\"Speaker0\"",
            "KSNAME_Microphone=\"Microphone0\"",
        ]
        .join("\r\n");
        std::fs::write(&single, text).unwrap();
        let read = package_mics(single.to_str().unwrap(), "Audio_Device");
        let _ = std::fs::remove_file(&single);
        assert_eq!(read.unwrap(), [LowLatency]);
        let missing = package_mics(r"C:\no\such\folder\x.inf", "Audio_Device").unwrap_err();
        assert_eq!(missing.call, "SetupOpenInfFileW");
    }

    /// Astra's review of a3421c6: a switch to both with the single-microphone package newest in the driver store made a device
    /// without the Call Microphone, and failed 15 s later. It stops first, saying why, before anything is removed.
    #[test]
    fn a_new_devices_package_must_make_the_variants_microphones() {
        use Mic::*;
        assert_eq!(new_package_lacks(Variant::Both, &newest(), &[]), None);
        assert_eq!(new_package_lacks(Variant::LowLatency, &single(), &[]), None);
        let unread = Candidate { mics: Err("SetupOpenInfFileW failed".into()), ..newest() };
        assert_eq!(new_package_lacks(Variant::Both, &unread, &[]), None, "checked after the install instead");
        let lacking = new_package_lacks(Variant::Both, &single(), &[]).unwrap();
        let expected =
            Lacking { target: Variant::Both, device: None, newest: single().describe(), missing: vec![Call], ranked_below: None };
        assert_eq!(lacking, expected);
        let text = SwitchError::PackageLacks(Box::new(lacking)).to_string();
        assert!(
            text.starts_with(&format!(
                r#"the driver store's newest AudioCodec package for ROOT\AudioCodec, {}, lacks "Call Microphone (AudioCodec Device)": that package doesn't make it"#,
                single().describe()
            )),
            "{text}"
        );
        assert!(text.ends_with("it re-creates the device with that package. Nothing was changed."), "{text}");
        let ranked = new_package_lacks(Variant::Both, &single(), &[unread, staged()]).unwrap();
        assert_eq!(ranked.ranked_below, Some(staged().describe()));
        assert_eq!(new_package_lacks(Variant::Call, &single(), &[]).unwrap().missing, [Call]);
    }

    #[test]
    fn a_search_ranks_ours_below_the_newest() {
        let older = candidate(r"C:\Windows\INF\oem3.inf", "AudioCodec.inf", 100, V1);
        let newer = candidate(r"C:\Windows\INF\oem12.inf", "audiocodec.inf", 200, V1);
        let foreign = candidate(r"C:\Windows\INF\oem9.inf", "othercodec.inf", 900, V1);
        assert_eq!(rank(Ok(vec![older.clone(), newer.clone(), foreign.clone()])), (Ok(Some(newer)), vec![older]));
        assert_eq!(rank(Ok(vec![foreign])), (Ok(None), vec![]));
        let failed = WinError { call: "SetupDiBuildDriverInfoList", code: 5 };
        assert_eq!(rank(Err(failed.clone())), (Err(failed), vec![]));
        // The plan's re-creation of a device kept, beside one it removes
        let mut p = plan(&[dev(Variant::Both, Health::Working), dev(Variant::Call, Health::Working)], Variant::Call);
        assert_eq!(p, Plan { keep: Some(1), remove: vec![0], create: false });
        p.recreate_kept();
        assert_eq!(p, Plan { keep: None, remove: vec![0, 1], create: true });
        p.recreate_kept();
        assert_eq!(p, Plan { keep: None, remove: vec![0, 1], create: true }, "once");
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
        // A node whose search failed names that call, not the status's (cables::present)
        assert_eq!(Health::Unlocated(19).describe(), "state unknown (CM_Locate_DevNodeW: CR_FAILURE, 19)");
        assert_eq!(Health::Unlocated(30).describe(), "state unknown (CM_Locate_DevNodeW: CONFIGRET 30)");
        let d = OurDevice { driver_version: Some("1.0.0.0".into()), ..dev(Variant::Both, Health::Working) };
        assert_eq!(d.describe(), r"ROOT\MEDIA\0003: both microphones (ROOT\AudioCodec), working, driver oem12.inf 1.0.0.0");
        let bare = OurDevice { inf: None, ..dev(Variant::Call, Health::Problem(28)) };
        assert_eq!(
            bare.describe(),
            r"ROOT\MEDIA\0003: the call microphone only (ROOT\AudioCodecCall), no driver installed (Code 28), no driver"
        );
    }

    /// One of our packages (by its original name) with the three variants: for `ROOT\AudioCodec`, both microphones.
    fn candidate(inf: &str, original: &str, date: u64, version: u64) -> Candidate {
        Candidate {
            inf: inf.into(),
            original: Ok(original.into()),
            provider: "VS_Microsoft".into(),
            description: "AudioCodec Device".into(),
            date,
            version,
            mics: Ok(vec![Mic::LowLatency, Mic::Call]),
        }
    }

    /// The single-microphone driver's package (oem12.inf, the newest in [`dev`]), which makes the Microphone alone.
    fn single() -> Candidate {
        Candidate { mics: Ok(vec![Mic::LowLatency]), ..newest() }
    }

    /// The three variants' package, staged, but ranked below [`single`] (an older DriverVer date).
    fn staged() -> Candidate {
        candidate(r"C:\Windows\INF\oem15.inf", "audiocodec.inf", 100, (23 << 48) | 1)
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
        assert!(text.starts_with("another switch has held the switch's lock for 60 s: let it end"), "{text}");
        assert!(text.ends_with("Nothing was changed."));
        let text = SwitchError::LockDenied(WinError { call: "CreatePrivateNamespaceW", code: 5 }).to_string();
        assert!(
            text.starts_with("the switch's lock refused this process (CreatePrivateNamespaceW failed: ERROR_ACCESS_DENIED (5)"),
            "{text}"
        );
        assert!(text.contains("it is for elevated administrators") && text.ends_with("Nothing was changed."), "{text}");
        let text = SwitchError::NoEndpoints(WinError { call: "IMMDeviceEnumerator::EnumAudioEndpoints", code: 5 }).to_string();
        assert!(text.starts_with("the audio endpoints couldn't be read (IMMDeviceEnumerator::EnumAudioEndpoints failed"), "{text}");
        assert!(text.contains("nothing was changed"));

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

        let e = SwitchError::Mics { missing: vec![Mic::Call], seen: vec![MICROPHONE.into()], read: None };
        assert_eq!(
            e.to_string(),
            "the device is working on the newest package, but after 15 s Windows hasn't made \"Call Microphone (AudioCodec Device)\" \
             (ours there: \"Microphone (AudioCodec Device)\"). The device stays, and a second run of the switch re-creates it if \
             they're still missing: `noise-filter mic variant` and the Sound panel's Recording tab show what's there now."
        );
        let e = SwitchError::Mics { missing: vec![Mic::LowLatency, Mic::Call], seen: vec![], read: Some(WinError { call: "X", code: 5 }) };
        let text = e.to_string();
        assert!(
            text.contains("hasn't made \"Microphone (AudioCodec Device)\" and \"Call Microphone (AudioCodec Device)\" (ours there: none)")
        );
        assert!(text.contains("; the last look at the endpoints failed: X failed: ERROR_ACCESS_DENIED (5)"), "{text}");
        assert_eq!(Variant::Both.microphones_text(), "\"Microphone (AudioCodec Device)\" and \"Call Microphone (AudioCodec Device)\"");
        assert_eq!(Variant::Call.microphones_text(), "\"Call Microphone (AudioCodec Device)\"");
    }
}
