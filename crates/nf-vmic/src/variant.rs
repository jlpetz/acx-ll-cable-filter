//! The driver's three variants (TODO 19, the owner's decision of 2026-11-23): one INF, AudioCodec.inf, whose hardware ID
//! picks the microphones. `ROOT\AudioCodec` has both ("Microphone (AudioCodec Device)", low latency, IAudioClient3's
//! 2.67-10 ms periods; and "Call Microphone (AudioCodec Device)", a 5 ms default period for apps that ask for none),
//! `ROOT\AudioCodecCall` the call microphone only, `ROOT\AudioCodecLowLatency` the low-latency one only. Every variant has
//! "Speakers (AudioCodec Device)". Windows reads a microphone's default period only when it creates the endpoint (TODO 22),
//! so a switch removes the root device and creates a new one with the other hardware ID, which needs Administrator access.
//!
//! - [`present`] reads which of our devices are there, their state and driver (any user may).
//! - [`plan`] decides what a switch does (pure).
//! - [`switch`] does it, elevated: what `devcon remove` and `devcon install` do, with the driver taken from the driver store
//!   (the package must have been added to it, by `pnputil /add-driver` or the installer).
//!
//! # The switch, in order
//! 1. Find our devices: every present device (`SetupDiGetClassDevsW`, all classes) whose `SPDRP_HARDWAREID` lists one of the
//!    three IDs.
//! 2. The new device first, not yet registered (`SetupDiCreateDeviceInfoW` under class MEDIA with `DICD_GENERATE_ID`, as devcon
//!    does: `ROOT\MEDIA\NNNN`; the hardware ID by `SetupDiSetDeviceRegistryPropertyW`), and its driver: the compatible-driver
//!    search over the driver store (`SetupDiBuildDriverInfoList(SPDIT_COMPATDRIVER)`), which is how Plug and Play itself
//!    matches a hardware ID to an INF, keeping the nodes whose INF (`C:\Windows\INF\oemNN.inf`) has the original name
//!    `audiocodec.inf` (`SetupGetInfInformationW` + `SetupQueryInfOriginalFileInformationW`), the newest by date, then version.
//!    So a missing driver stops the switch before anything is removed. An element that isn't registered yet is a phantom:
//!    destroying the set deletes it. (That the compatible-driver search works on an element not yet registered is our
//!    inference, from the hardware ID being in its registry key; not yet run.)
//! 3. Remove the old devices (`DiUninstallDevice`, what `pnputil /remove-device` does). If Windows needs a restart to finish a
//!    removal, stop there: the old driver may still be loaded. The new device is registered only after this, so that its
//!    endpoints are made while the old ones are gone, and the driver never runs twice.
//! 4. Register the new device (`SetupDiCallClassInstaller(DIF_REGISTERDEVICE)`) and install the chosen driver on it
//!    (`DiInstallDevice`, documented for a driver already in the driver store). `UpdateDriverForPlugAndPlayDevicesW`, devcon's
//!    call, wants the package's own folder rather than a system one (its documentation), and would also touch every other
//!    device with that hardware ID. If the install fails, the new device is removed again (unless Plug and Play put the
//!    driver on it by itself meanwhile), so no device without a driver is left to read as that variant.
//!
//! A failure after the old devices are removed leaves none: the steps say which were removed, and a second run of the switch
//! creates the device afresh.

use std::fmt;
use std::mem::size_of;

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_DEVNODE_STATUS_FLAGS, CM_Get_DevNode_Status, CM_PROB, DICD_GENERATE_ID, DIF_REGISTERDEVICE, DIGCF_ALLCLASSES, DIGCF_PRESENT,
    DIIDFLAG_NOFINISHINSTALLUI, DN_HAS_PROBLEM, DN_STARTED, DiInstallDevice, DiUninstallDevice, GUID_DEVCLASS_MEDIA, HDEVINFO,
    INFINFO_INF_NAME_IS_ABSOLUTE, SETUP_DI_REGISTRY_PROPERTY, SP_DEVINFO_DATA, SP_DRVINFO_DATA_V2_W, SP_DRVINFO_DETAIL_DATA_W,
    SP_INF_INFORMATION, SP_ORIGINAL_FILE_INFO_W, SPDIT_COMPATDRIVER, SPDRP_HARDWAREID, SetupDiBuildDriverInfoList,
    SetupDiCallClassInstaller, SetupDiCreateDeviceInfoList, SetupDiCreateDeviceInfoW, SetupDiEnumDeviceInfo, SetupDiEnumDriverInfoW,
    SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW, SetupDiGetDevicePropertyW, SetupDiGetDeviceRegistryPropertyW,
    SetupDiGetDriverInfoDetailW, SetupDiSetDeviceRegistryPropertyW, SetupGetInfDriverStoreLocationW, SetupGetInfInformationW,
    SetupQueryInfOriginalFileInformationW,
};
use windows::Win32::Devices::Properties::{DEVPKEY_Device_DriverInfPath, DEVPKEY_Device_DriverVersion, DEVPROPTYPE};
use windows::Win32::Foundation::{DEVPROPKEY, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_ITEMS, HWND};
use windows::core::{BOOL, HSTRING, Owned, PCWSTR, w};

use crate::win_error::{WinError, configret_name, problem_name, win32_code};

/// What every endpoint of the driver has in its name ("Microphone (AudioCodec Device)"): the device's description.
pub const DEVICE_NAME: &str = "AudioCodec Device";
/// The low-latency microphone's endpoint (IAudioClient3's 2.67-10 ms periods).
pub const MICROPHONE: &str = "Microphone (AudioCodec Device)";
/// The call microphone's endpoint (a 5 ms default period, for apps that ask for none).
pub const CALL_MICROPHONE: &str = "Call Microphone (AudioCodec Device)";
/// The INF's file name as built; the driver store publishes it as `oemNN.inf` and remembers this one.
pub const INF_NAME: &str = "audiocodec.inf";

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
}

impl OurDevice {
    /// "ROOT\MEDIA\0003: both microphones (ROOT\AudioCodec), working, driver oem12.inf 1.0.0.0".
    pub fn describe(&self) -> String {
        let driver = match (&self.inf, &self.driver_version) {
            (Some(inf), Some(v)) => format!("driver {inf} {v}"),
            (Some(inf), None) => format!("driver {inf}"),
            (None, _) => "no driver".into(),
        };
        format!("{}: {} ({}), {}, {driver}", self.instance_id, self.variant.describe(), self.variant.hardware_id(), self.health.describe())
    }
}

/// The present devices with one of our hardware IDs. Any user may call it.
pub fn present() -> Result<Vec<OurDevice>, WinError> {
    let set = DeviceSet::all_present()?;
    Ok(set.ours()?.into_iter().map(|(_, d)| d).collect())
}

/// What a switch to `target` does with the devices there now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// A working device of the target variant, kept as it is.
    pub keep: Option<usize>,
    /// Every other device of ours, removed.
    pub remove: Vec<usize>,
    /// No working device of the target variant: create one.
    pub create: bool,
}

impl Plan {
    /// The target variant is already the only one there, and working.
    pub fn is_nothing(&self) -> bool {
        self.remove.is_empty() && !self.create
    }
}

/// Keeps the first working device of the target variant and removes the rest; creates one when none is working (a disabled
/// or broken device of the target variant is replaced, which also enables it).
pub fn plan(devices: &[OurDevice], target: Variant) -> Plan {
    let keep = devices.iter().position(|d| d.variant == target && d.health == Health::Working);
    let remove = (0..devices.len()).filter(|&i| Some(i) != keep).collect();
    Plan { keep, remove, create: keep.is_none() }
}

/// What [`switch`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The target variant was already the only one there: nothing changed.
    Nothing,
    /// Switched. `restart`: Windows asked for a restart to finish the install.
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
        }
    }
}

impl std::error::Error for SwitchError {}

/// Switches to `target` (see the module's docs for the steps), reporting each step and each Windows error through `say`, as
/// it happens. Needs elevation: without it, the first change fails with ERROR_ACCESS_DENIED.
pub fn switch(target: Variant, say: &mut dyn FnMut(&str)) -> Result<Outcome, SwitchError> {
    let ids: Vec<&str> = Variant::ALL.iter().map(|v| v.hardware_id()).collect();
    say(&format!("Looking for the AudioCodec device ({})", ids.join(", ")));
    let set = DeviceSet::all_present()?;
    let ours = set.ours()?;
    if ours.is_empty() {
        say("  none is installed");
    }
    for (_, d) in &ours {
        say(&format!("  {}", d.describe()));
    }
    let devices: Vec<OurDevice> = ours.iter().map(|(_, d)| d.clone()).collect();
    let plan = plan(&devices, target);
    if plan.is_nothing() {
        say(&format!("Already {} ({}): nothing to do.", target.describe(), target.hardware_id()));
        return Ok(Outcome::Nothing);
    }
    if let Some(k) = plan.keep {
        say(&format!("Keeping {}", devices[k].instance_id));
    }
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
    let restart = match pending {
        Some(p) => p.install(say)?,
        None => false,
    };
    Ok(Outcome::Switched { restart })
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
            let device = OurDevice {
                instance_id: self.instance_id(&data)?,
                variant,
                health: health(data.DevInst),
                inf: self.string_property(&data, &DEVPKEY_Device_DriverInfPath),
                driver_version: self.string_property(&data, &DEVPKEY_Device_DriverVersion),
            };
            out.push((data, device));
        }
        Ok(out)
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

/// The new device, not yet registered, with its driver chosen.
struct Pending {
    set: DeviceSet,
    data: SP_DEVINFO_DATA,
    node: SP_DRVINFO_DATA_V2_W,
    instance_id: String,
    inf: String,
    target: Variant,
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
        let inf = candidates[chosen].inf.clone();
        match store_location(&inf) {
            Ok(path) => say(&format!("  stored at {path}")),
            Err(e) => say(&format!("  (its driver-store folder is unknown: {e})")),
        }
        Ok(Self { set, data, node: nodes[chosen].0, instance_id, inf, target })
    }

    /// Registers the device and installs the chosen driver on it. `Ok(true)`: Windows wants a restart to finish.
    fn install(self, say: &mut dyn FnMut(&str)) -> Result<bool, SwitchError> {
        let Pending { set, data, node, instance_id, inf, target } = self;
        say(&format!("Creating {instance_id} (SetupDiCallClassInstaller(DIF_REGISTERDEVICE))"));
        // SAFETY: an element of our set.
        unsafe { SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set.get(), Some(&data)) }
            .map_err(WinError::of("SetupDiCallClassInstaller(DIF_REGISTERDEVICE)"))?;
        say(&format!("Installing the driver from {inf} on it (DiInstallDevice)"));
        let mut restart = BOOL(0);
        // No finish-install pages: the helper has no window to show them from.
        // SAFETY: a registered element of our set and a driver node from its own list; `restart` is a live out-pointer.
        let installed = unsafe { DiInstallDevice(None, set.get(), &data, Some(&node), DIIDFLAG_NOFINISHINSTALLUI, Some(&mut restart)) };
        let state = health(data.DevInst);
        if let Err(e) = installed {
            let e = WinError::new("DiInstallDevice", &e);
            say(&format!("  {e}"));
            // Plug and Play may have put the same driver on the new device by itself meanwhile
            if state != Health::Working {
                // A device without a driver would read as this variant next time: take it away again
                say(&format!("Removing {instance_id} again, as it is {}", state.describe()));
                // SAFETY: as above; no window, no restart flag wanted.
                match unsafe { DiUninstallDevice(HWND::default(), set.get(), &data, 0, None) } {
                    Ok(()) => say("  removed"),
                    Err(e2) => say(&format!("  {}", WinError::new("DiUninstallDevice", &e2))),
                }
                return Err(e.into());
            }
            say(&format!("  but {instance_id} is working: Windows installed its driver by itself"));
        }
        say(&format!("  installed: {instance_id} is {}", state.describe()));
        if restart.as_bool() {
            say("  Windows needs a restart to finish the install.");
        }
        let mics = target.microphones().iter().map(|m| format!("\"{m}\"")).collect::<Vec<_>>().join(" and ");
        say(&format!("Windows adds the endpoints in a few seconds: {mics}, and \"Speakers (AudioCodec Device)\"."));
        Ok(restart.as_bool())
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

    fn dev(variant: Variant, health: Health) -> OurDevice {
        OurDevice { instance_id: r"ROOT\MEDIA\0003".into(), variant, health, inf: Some("oem12.inf".into()), driver_version: None }
    }

    #[test]
    fn the_plan_keeps_one_working_target_and_removes_the_rest() {
        use Health::*;
        use Variant::*;
        // Already the only one, working: nothing
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
    }
}
