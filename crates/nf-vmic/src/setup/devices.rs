//! The device nodes, through SetupAPI and the configuration manager (TODO 19, the per-cable design, section 9; extracted from
//! variant.rs, which did this for the one device before cables). Reading is open to any user; everything that changes a node
//! needs an elevated administrator, and the caller holds the setup's lock ([`super::lock`]).
//!
//! - [`DeviceSet::ours`]: every present node, of any class, whose hardware IDs include one of ours ([`Model::of`]), with its
//!   state, its driver, its instance ID's cable, D1's start count, its problem status and its config flags (whether a disable
//!   persists: the user's, or a run's that was stopped).
//! - [`NewNode`]: a cable's node made with its fixed instance ID (`SetupDiCreateDeviceInfoW` with the full ID and without
//!   `DICD_GENERATE_ID`), its hardware ID and friendly name set before `DIF_REGISTERDEVICE`, so nothing the driver reads is
//!   written after Plug and Play could start it. If the ID's key is still there (ERROR_DEVINST_ALREADY_EXISTS), the element is
//!   opened instead. [`NewNode::scratch`]: an element never registered, for a compatible-driver search before anything changes.
//! - [`DeviceSet::clear_start_attempts`]: D1's boot crash breaker counts a node's unfinished starts in its hardware key
//!   (`NfStartAttempts`, Cable.h); every install path deletes the count before it installs or enables (D1's contract).
//! - [`DeviceSet::disable`] and [`DeviceSet::enable`]: `CM_Disable_DevNode`, polite and without `CM_DISABLE_PERSIST` (a crash
//!   or a reboot mid-update brings every cable back).

use std::fmt;
use std::mem::size_of;
use std::time::{Duration, Instant};

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_DEVNODE_STATUS_FLAGS, CM_DISABLE_POLITE, CM_DISABLE_UI_NOT_OK, CM_Disable_DevNode, CM_Enable_DevNode, CM_Get_DevNode_Status,
    CM_MapCrToWin32Err, CM_PROB, CM_PROB_NEED_RESTART, CONFIGRET, CR_SUCCESS, DICD_GENERATE_ID, DICS_FLAG_GLOBAL, DIF_REGISTERDEVICE,
    DIGCF_ALLCLASSES, DIGCF_PRESENT, DIIDFLAG_NOFINISHINSTALLUI, DIREG_DEV, DiInstallDevice, DiUninstallDevice, GUID_DEVCLASS_MEDIA,
    HDEVINFO, INF_STYLE_WIN4, INFCONTEXT, INFINFO_INF_NAME_IS_ABSOLUTE, SETUP_DI_REGISTRY_PROPERTY, SP_COPY_STYLE, SP_DEVINFO_DATA,
    SP_DRVINFO_DATA_V2_W, SP_DRVINFO_DETAIL_DATA_W, SP_INF_INFORMATION, SP_ORIGINAL_FILE_INFO_W, SPDIT_COMPATDRIVER, SPDRP_CONFIGFLAGS,
    SPDRP_FRIENDLYNAME, SPDRP_HARDWAREID, SPOST_PATH, SetupCloseInfFile, SetupCopyOEMInfW, SetupDiBuildDriverInfoList,
    SetupDiCallClassInstaller, SetupDiCreateDeviceInfoList, SetupDiCreateDeviceInfoW, SetupDiEnumDeviceInfo, SetupDiEnumDriverInfoW,
    SetupDiGetActualSectionToInstallW, SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW, SetupDiGetDevicePropertyW,
    SetupDiGetDeviceRegistryPropertyW, SetupDiGetDriverInfoDetailW, SetupDiOpenDevRegKey, SetupDiOpenDeviceInfoW,
    SetupDiSetDeviceRegistryPropertyW, SetupFindFirstLineW, SetupFindNextMatchLineW, SetupGetInfDriverStoreLocationW,
    SetupGetInfInformationW, SetupGetStringFieldW, SetupOpenInfFileW, SetupQueryInfOriginalFileInformationW,
};
use windows::Win32::Devices::Properties::{
    DEVPKEY_Device_DriverInfPath, DEVPKEY_Device_DriverVersion, DEVPKEY_Device_ProblemStatus, DEVPROPTYPE,
};
use windows::Win32::Foundation::{
    DEVPROPKEY, ERROR_DEVINST_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_GEN_FAILURE, ERROR_INSUFFICIENT_BUFFER, ERROR_KEY_DOES_NOT_EXIST,
    ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, GetLastError, HWND, INVALID_HANDLE_VALUE,
};
use windows::Win32::System::Registry::{
    HKEY, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_DWORD, REG_VALUE_TYPE, RegCloseKey, RegDeleteValueW, RegQueryValueExW,
};
use windows::core::{BOOL, HSTRING, Owned, PCWSTR, w};

use super::package::{Candidate, Running, runs};
use super::plan::{Health, Model, Node};
use crate::identity::{CableInstance, PRODUCT};
use crate::sessions::{self, Mic};
use crate::win_error::{WinError, win32_code};

/// D1's start count's value in a node's hardware key (Cable.h, NF_CABLE_START_ATTEMPTS_VALUE).
pub const START_ATTEMPTS_VALUE: &str = "NfStartAttempts";
/// How long a node has, after DiInstallDevice or an enable, to be working.
pub const DRIVER_WAIT: Duration = Duration::from_secs(5);
/// How often the waits look.
pub const POLL: Duration = Duration::from_millis(250);

/// A device node's state and driver, as Windows reports them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bound {
    pub health: Health,
    /// `DEVPKEY_Device_DriverInfPath`: `oem12.inf`.
    pub inf: Option<String>,
    /// `DEVPKEY_Device_DriverVersion`: "12.40.56.342".
    pub driver_version: Option<String>,
}

impl Bound {
    pub fn running(&self) -> Option<Running> {
        Some(Running { inf: self.inf.clone()?, version: self.driver_version.clone()? })
    }

    /// "working, driver oem12.inf 12.40.56.342".
    pub fn describe(&self) -> String {
        format!("{}, {}", self.health.describe(), driver_text(self.inf.as_deref(), self.driver_version.as_deref()))
    }
}

/// "driver oem12.inf 1.0.0.0", "no driver".
pub fn driver_text(inf: Option<&str>, version: Option<&str>) -> String {
    match (inf, version) {
        (Some(inf), Some(v)) => format!("driver {inf} {v}"),
        (Some(inf), None) => format!("driver {inf}"),
        (None, _) => "no driver".into(),
    }
}

/// What an install came to, when it's kept ([`judge_install`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Installed {
    /// Working, on the chosen package.
    Working,
    /// Windows asked for a restart to finish: the node is left for it.
    Restart,
}

/// Why an install isn't kept ([`judge_install`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallFailure {
    /// DiInstallDevice failed, and the node (`now`) isn't working on the chosen package.
    Call { error: WinError, now: Bound },
    /// DiInstallDevice succeeded, but the node (`now`) wasn't working [`DRIVER_WAIT`] later.
    NotWorking { now: Bound },
    /// The node (`now`) is working, on another package than the chosen one (Plug and Play's own choice, say).
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

/// Judges an install: `call` is what DiInstallDevice returned, `restart` whether it asked for a restart, `now` the node
/// afterwards, `chosen` the package it was given. Kept only when working on the chosen package (whatever the call returned),
/// or when Windows asked for a restart to finish.
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

/// How a polite disable went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Disabled {
    Done,
    /// CR_REMOVE_VETOED: an app or service refused it.
    Vetoed,
    Failed(WinError),
}

/// A device information set, destroyed on drop (which also deletes an element in it that was never registered).
pub struct DeviceSet(Owned<HDEVINFO>);

impl DeviceSet {
    /// Every present device, of every class.
    pub fn all_present() -> Result<Self, WinError> {
        // SAFETY: no class, enumerator or window; the set returned is ours.
        let set = unsafe { SetupDiGetClassDevsW(None, PCWSTR::null(), None, DIGCF_ALLCLASSES | DIGCF_PRESENT) }
            .map_err(WinError::of("SetupDiGetClassDevsW"))?;
        // SAFETY: a valid set that nothing else owns; Owned destroys it once.
        Ok(Self(unsafe { Owned::new(set) }))
    }

    /// An empty set for class MEDIA, for a new node.
    pub fn new_media() -> Result<Self, WinError> {
        // SAFETY: the class GUID is a constant that outlives the call; no window.
        let set = unsafe { SetupDiCreateDeviceInfoList(Some(&GUID_DEVCLASS_MEDIA), None) }
            .map_err(WinError::of("SetupDiCreateDeviceInfoList"))?;
        // SAFETY: as in all_present.
        Ok(Self(unsafe { Owned::new(set) }))
    }

    pub fn get(&self) -> HDEVINFO {
        *self.0
    }

    /// The nodes in the set with one of our hardware IDs, with their element data.
    pub fn ours(&self) -> Result<Vec<(SP_DEVINFO_DATA, Node)>, WinError> {
        let mut out = Vec::new();
        for index in 0.. {
            let mut data = SP_DEVINFO_DATA { cbSize: size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
            // SAFETY: our set; `data` is a live out-structure with its size set.
            match unsafe { SetupDiEnumDeviceInfo(self.get(), index, &mut data) } {
                Ok(()) => {}
                Err(e) if win32_code(&e) == ERROR_NO_MORE_ITEMS.0 => break,
                Err(e) => return Err(WinError::new("SetupDiEnumDeviceInfo", &e)),
            }
            // A node whose IDs can't be read isn't one we could act on anyway
            let Some(model) = self.registry_multi_sz(&data, SPDRP_HARDWAREID).ok().and_then(|ids| Model::of(&ids)) else { continue };
            let instance_id = self.instance_id(&data)?;
            let bound = self.bound(&data);
            let node = Node {
                cable: CableInstance::parse(&instance_id),
                instance_id,
                model,
                health: bound.health,
                running: bound.running(),
                start_attempts: self.start_attempts(&data),
                problem_status: self.u32_property(&data, &DEVPKEY_Device_ProblemStatus),
                config_flags: self.registry_u32(&data, SPDRP_CONFIGFLAGS),
            };
            out.push((data, node));
        }
        Ok(out)
    }

    /// An element's state and driver, as Windows reports them now.
    pub fn bound(&self, data: &SP_DEVINFO_DATA) -> Bound {
        Bound {
            health: health(data.DevInst),
            inf: self.string_property(data, &DEVPKEY_Device_DriverInfPath),
            driver_version: self.string_property(data, &DEVPKEY_Device_DriverVersion),
        }
    }

    /// Up to `wait` for an element to be working; its state at the end.
    pub fn wait_working(&self, data: &SP_DEVINFO_DATA, wait: Duration) -> Bound {
        let deadline = Instant::now() + wait;
        let mut now = self.bound(data);
        while now.health != Health::Working && Instant::now() < deadline {
            std::thread::sleep(POLL);
            now = self.bound(data);
        }
        now
    }

    /// The drivers compatible with an element: the compatible-driver search on it (which only reads the driver store), with
    /// each one's driver node.
    pub fn compatible(&self, data: &mut SP_DEVINFO_DATA) -> Result<Vec<(SP_DRVINFO_DATA_V2_W, Candidate)>, WinError> {
        // SAFETY: an element of our set.
        unsafe { SetupDiBuildDriverInfoList(self.get(), Some(data), SPDIT_COMPATDRIVER) }
            .map_err(WinError::of("SetupDiBuildDriverInfoList"))?;
        self.compat_drivers(data)
    }

    pub fn instance_id(&self, data: &SP_DEVINFO_DATA) -> Result<String, WinError> {
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

    /// A REG_DWORD registry property (`SPDRP_CONFIGFLAGS`), `None` when it doesn't read.
    fn registry_u32(&self, data: &SP_DEVINFO_DATA, property: SETUP_DI_REGISTRY_PROPERTY) -> Option<u32> {
        let mut buf = [0u8; 4];
        let mut needed = 0u32;
        // SAFETY: an element of our set; the buffer's length goes with it; `needed` is a live out-pointer.
        unsafe { SetupDiGetDeviceRegistryPropertyW(self.get(), data, property, None, Some(&mut buf), Some(&mut needed)) }.ok()?;
        (needed == 4).then(|| u32::from_le_bytes(buf))
    }

    /// A string device property, `None` when it isn't set.
    fn string_property(&self, data: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Option<String> {
        let mut buf = vec![0u8; 1024];
        let mut kind = DEVPROPTYPE::default();
        // SAFETY: an element of our set; `key` and `kind` outlive the call; the buffer's length goes with it.
        unsafe { SetupDiGetDevicePropertyW(self.get(), data, key, &mut kind, Some(buf.as_mut_slice()), None, 0) }.ok()?;
        let s = from_wide(&wide_of_bytes(&buf));
        (!s.is_empty()).then_some(s)
    }

    /// A 32-bit device property (DEVPROP_TYPE_NTSTATUS, DEVPROP_TYPE_UINT32), `None` when it isn't set.
    fn u32_property(&self, data: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Option<u32> {
        let mut buf = [0u8; 4];
        let mut kind = DEVPROPTYPE::default();
        let mut size = 0u32;
        // SAFETY: an element of our set; `key` and `kind` outlive the call; the buffer's length goes with it.
        unsafe { SetupDiGetDevicePropertyW(self.get(), data, key, &mut kind, Some(&mut buf), Some(&mut size), 0) }.ok()?;
        (size == 4).then(|| u32::from_le_bytes(buf))
    }

    /// The node's hardware key (Device Parameters), opened for `access`; `None` if it doesn't exist yet.
    fn hardware_key(&self, data: &SP_DEVINFO_DATA, access: u32) -> Result<Option<RegKey>, WinError> {
        // SAFETY: an element of our set; the global scope, profile 0, the hardware key.
        match unsafe { SetupDiOpenDevRegKey(self.get(), data, DICS_FLAG_GLOBAL.0, 0, DIREG_DEV, access) } {
            Ok(key) => Ok(Some(RegKey(key))),
            Err(e) if matches!(win32_code(&e), c if c == ERROR_KEY_DOES_NOT_EXIST.0 || c == ERROR_FILE_NOT_FOUND.0) => Ok(None),
            Err(e) => Err(WinError::new("SetupDiOpenDevRegKey", &e)),
        }
    }

    /// D1's start count, if the node's hardware key has it.
    fn start_attempts(&self, data: &SP_DEVINFO_DATA) -> Option<u32> {
        let key = self.hardware_key(data, KEY_QUERY_VALUE.0).ok()??;
        let name = HSTRING::from(START_ATTEMPTS_VALUE);
        let (mut kind, mut value, mut len) = (REG_VALUE_TYPE::default(), [0u8; 4], 4u32);
        // SAFETY: an open key; a NUL-terminated name; a 4-byte buffer and its length; live out-pointers.
        let e = unsafe { RegQueryValueExW(key.0, &name, None, Some(&mut kind), Some(value.as_mut_ptr()), Some(&mut len)) };
        (e == ERROR_SUCCESS && kind == REG_DWORD && len == 4).then(|| u32::from_le_bytes(value))
    }

    /// Deletes D1's start count from the node's hardware key (D1's contract: on every install path, before installing or
    /// enabling). No key, or no value, is nothing to clear.
    pub fn clear_start_attempts(&self, data: &SP_DEVINFO_DATA) -> Result<bool, WinError> {
        let Some(key) = self.hardware_key(data, KEY_SET_VALUE.0)? else { return Ok(false) };
        let name = HSTRING::from(START_ATTEMPTS_VALUE);
        // SAFETY: an open key; a NUL-terminated name.
        match unsafe { RegDeleteValueW(key.0, &name) } {
            ERROR_SUCCESS => Ok(true),
            ERROR_FILE_NOT_FOUND => Ok(false),
            e => Err(WinError { call: "RegDeleteValueW(NfStartAttempts)", code: e.0 }),
        }
    }

    /// The node's friendly name, "ACX-LL-Cable D" (SPDRP_FRIENDLYNAME): before registration, and again after each install.
    pub fn set_friendly_name(&self, data: &mut SP_DEVINFO_DATA, name: &str) -> Result<(), WinError> {
        let bytes = sz_bytes(name);
        // SAFETY: an element of our set; the buffer's length goes with it.
        unsafe { SetupDiSetDeviceRegistryPropertyW(self.get(), data, SPDRP_FRIENDLYNAME, Some(&bytes)) }
            .map_err(WinError::of("SetupDiSetDeviceRegistryPropertyW(SPDRP_FRIENDLYNAME)"))
    }

    /// DiInstallDevice with `node` from the element's own compatible-driver list; whether Windows asked for a restart.
    pub fn install(&self, data: &SP_DEVINFO_DATA, node: &SP_DRVINFO_DATA_V2_W) -> Result<bool, WinError> {
        let mut restart = BOOL(0);
        // No finish-install pages: the tool may have no window to show them from.
        // SAFETY: a registered element of our set and a driver node from its own list; `restart` is a live out-pointer.
        unsafe { DiInstallDevice(None, self.get(), data, Some(node), DIIDFLAG_NOFINISHINSTALLUI, Some(&mut restart)) }
            .map_err(WinError::of("DiInstallDevice"))?;
        Ok(restart.as_bool())
    }

    /// DiUninstallDevice (what `pnputil /remove-device` does); whether Windows needs a restart to finish.
    pub fn uninstall(&self, data: &SP_DEVINFO_DATA) -> Result<bool, WinError> {
        let mut restart = BOOL(0);
        // SAFETY: an element of our set; no window; `restart` is a live out-pointer.
        unsafe { DiUninstallDevice(HWND::default(), self.get(), data, 0, Some(&mut restart)) }
            .map_err(WinError::of("DiUninstallDevice"))?;
        Ok(restart.as_bool())
    }

    /// Disables a node politely, without persistence: an app may veto it, and a restart brings it back.
    pub fn disable(&self, data: &SP_DEVINFO_DATA) -> Disabled {
        // SAFETY: a devnode from our set (a stale one only makes the call fail).
        let cr = unsafe { CM_Disable_DevNode(data.DevInst, CM_DISABLE_POLITE | CM_DISABLE_UI_NOT_OK) };
        match cr {
            CR_SUCCESS => Disabled::Done,
            c if c == windows::Win32::Devices::DeviceAndDriverInstallation::CR_REMOVE_VETOED => Disabled::Vetoed,
            c => Disabled::Failed(WinError { call: "CM_Disable_DevNode", code: cr_win32(c) }),
        }
    }

    /// Enables a node a run disabled.
    pub fn enable(&self, data: &SP_DEVINFO_DATA) -> Result<(), WinError> {
        // SAFETY: as in disable.
        let cr = unsafe { CM_Enable_DevNode(data.DevInst, 0) };
        if cr == CR_SUCCESS { Ok(()) } else { Err(WinError { call: "CM_Enable_DevNode", code: cr_win32(cr) }) }
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
            let candidate = Candidate {
                original: original_inf_name(&inf).map_err(|e| e.to_string()),
                mics: package_mics(&inf, &section).map_err(|e| e.to_string()),
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

/// A registry key from SetupAPI, closed on drop.
struct RegKey(HKEY);

impl Drop for RegKey {
    fn drop(&mut self) {
        // SAFETY: our key, closed once.
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

/// A cable's node being made: an element of its own set, with its fixed instance ID.
pub struct NewNode {
    pub set: DeviceSet,
    pub data: SP_DEVINFO_DATA,
    pub instance_id: String,
    /// The ID's key was still there (a removed node's), so the element was opened rather than created.
    pub reopened: bool,
}

impl NewNode {
    /// The element (a phantom until registered), its hardware ID and friendly name set. Nothing is registered yet: dropping
    /// it deletes a created element.
    pub fn prepare(cable: &CableInstance, hardware_id: &str) -> Result<Self, WinError> {
        let set = DeviceSet::new_media()?;
        let instance_id = cable.instance_id();
        let name = HSTRING::from(instance_id.as_str());
        let description = HSTRING::from(PRODUCT);
        let mut data = SP_DEVINFO_DATA { cbSize: size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
        // A full device instance ID, without DICD_GENERATE_ID (Learn, SetupDiCreateDeviceInfoW)
        // SAFETY: our set; NUL-terminated strings; the class GUID is a constant; no window; `data` is a live out-structure with
        // its size set.
        let created = unsafe {
            SetupDiCreateDeviceInfoW(set.get(), &name, &GUID_DEVCLASS_MEDIA, &description, None, Default::default(), Some(&mut data))
        };
        let reopened = match created {
            Ok(()) => false,
            Err(e) if win32_code(&e) == ERROR_DEVINST_ALREADY_EXISTS.0 => {
                // SAFETY: as above; no flags.
                unsafe { SetupDiOpenDeviceInfoW(set.get(), &name, None, 0, Some(&mut data)) }
                    .map_err(WinError::of("SetupDiOpenDeviceInfoW"))?;
                true
            }
            Err(e) => return Err(WinError::new("SetupDiCreateDeviceInfoW", &e)),
        };
        let ids = multi_sz_bytes(&[hardware_id]);
        // SAFETY: an element of our set; the buffer's length goes with it.
        unsafe { SetupDiSetDeviceRegistryPropertyW(set.get(), &mut data, SPDRP_HARDWAREID, Some(ids.as_slice())) }
            .map_err(WinError::of("SetupDiSetDeviceRegistryPropertyW(SPDRP_HARDWAREID)"))?;
        set.set_friendly_name(&mut data, &cable.label.friendly_name())?;
        Ok(Self { set, data, instance_id, reopened })
    }

    /// A scratch element for a compatible-driver search with `hardware_id`, before anything changes (the migration's check of
    /// the cables it will make): `ROOT\MEDIA\NNNN` (DICD_GENERATE_ID, as devcon makes one), never registered, so dropping it
    /// deletes it. Never one of the cables' fixed instance IDs, whose key a removed cable may still have.
    pub fn scratch(hardware_id: &str) -> Result<(DeviceSet, SP_DEVINFO_DATA), WinError> {
        let set = DeviceSet::new_media()?;
        let mut data = SP_DEVINFO_DATA { cbSize: size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
        let description = HSTRING::from(PRODUCT);
        // SAFETY: our set; NUL-terminated strings; the class GUID is a constant; no window; `data` is a live out-structure with
        // its size set.
        unsafe {
            SetupDiCreateDeviceInfoW(set.get(), w!("MEDIA"), &GUID_DEVCLASS_MEDIA, &description, None, DICD_GENERATE_ID, Some(&mut data))
        }
        .map_err(WinError::of("SetupDiCreateDeviceInfoW(DICD_GENERATE_ID)"))?;
        let ids = multi_sz_bytes(&[hardware_id]);
        // SAFETY: an element of our set; the buffer's length goes with it.
        unsafe { SetupDiSetDeviceRegistryPropertyW(set.get(), &mut data, SPDRP_HARDWAREID, Some(ids.as_slice())) }
            .map_err(WinError::of("SetupDiSetDeviceRegistryPropertyW(SPDRP_HARDWAREID)"))?;
        Ok((set, data))
    }

    /// DIF_REGISTERDEVICE. A reopened element may be registered already, which isn't an error.
    pub fn register(&self) -> Result<(), WinError> {
        // SAFETY: an element of our set.
        match unsafe { SetupDiCallClassInstaller(DIF_REGISTERDEVICE, self.set.get(), Some(&self.data)) } {
            Ok(()) => Ok(()),
            Err(e) if self.reopened && win32_code(&e) == ERROR_DEVINST_ALREADY_EXISTS.0 => Ok(()),
            Err(e) => Err(WinError::new("SetupDiCallClassInstaller(DIF_REGISTERDEVICE)", &e)),
        }
    }
}

/// A CONFIGRET as the Win32 code it stands for (CM_MapCrToWin32Err), so a [`WinError`] names it; ERROR_GEN_FAILURE for one
/// without.
fn cr_win32(cr: CONFIGRET) -> u32 {
    // SAFETY: plain values.
    unsafe { CM_MapCrToWin32Err(cr, ERROR_GEN_FAILURE.0) }
}

/// A devnode's state.
pub fn health(devinst: u32) -> Health {
    let (mut status, mut problem) = (CM_DEVNODE_STATUS_FLAGS(0), CM_PROB(0));
    // SAFETY: two live out-pointers; `devinst` came from SetupDi (a stale one only makes the call fail).
    let cr = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, devinst, 0) };
    Health::of(cr.0, status.0, problem.0)
}

/// Adds a package to the driver store (`SetupCopyOEMInfW`, as `pnputil /add-driver` does, without installing it on any
/// device) and returns its published INF (`C:\Windows\INF\oem12.inf`). One already there by a binary compare gives the name
/// it has.
pub fn stage(inf_path: &str) -> Result<String, WinError> {
    let path = HSTRING::from(inf_path);
    let mut dest = vec![0u16; 1024];
    // SAFETY: a NUL-terminated path; no media location (the INF's own folder); the buffer's length goes with it.
    unsafe { SetupCopyOEMInfW(&path, PCWSTR::null(), SPOST_PATH, SP_COPY_STYLE(0), Some(&mut dest), None, None) }
        .map_err(WinError::of("SetupCopyOEMInfW"))?;
    Ok(from_wide(&dest))
}

/// The name an INF had before the driver store published it as `oemNN.inf`: `audiocodec.inf`.
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

/// A published INF's folder in the driver store.
pub fn store_location(inf_path: &str) -> Result<String, WinError> {
    let path = HSTRING::from(inf_path);
    let mut buf = vec![0u16; 1024];
    // SAFETY: a NUL-terminated path; no platform or locale; the buffer's length goes with it.
    unsafe { SetupGetInfDriverStoreLocationW(&path, None, PCWSTR::null(), &mut buf, None) }
        .map_err(WinError::of("SetupGetInfDriverStoreLocationW"))?;
    Ok(from_wide(&buf))
}

/// The microphones a package makes for a hardware ID, by its INF and the install section the ID's models line names: the
/// reference strings of the AddInterface lines in the section's `.Interfaces`, as the platform decorates it, each by
/// [`sessions::mic_of_filter`]; in [`crate::variant::Variant::mics`]' order. Any user may read it.
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
            // SAFETY: no arguments; read straight after the failed call.
            return Err(WinError { call: "SetupOpenInfFileW", code: unsafe { GetLastError() }.0 });
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
pub fn from_wide(s: &[u16]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}

/// Little-endian bytes as UTF-16 units (a registry or device property's buffer); an odd last byte is dropped.
pub fn wide_of_bytes(bytes: &[u8]) -> Vec<u16> {
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

/// A REG_SZ's bytes, NUL-terminated.
pub fn sz_bytes(s: &str) -> Vec<u8> {
    s.encode_utf16().chain([0]).flat_map(|u| u.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::package::tests::{V1, candidate};

    // Nothing here touches a device or the driver store: the formats, the install's judgement, and the repo's INF.

    #[test]
    fn multi_sz_round_trips() {
        let bytes = multi_sz_bytes(&[r"ROOT\AcxLlCableCall"]);
        assert_eq!(bytes.len(), (r"ROOT\AcxLlCableCall".len() + 2) * 2, "the string, its NUL and the list's NUL");
        assert_eq!(parse_multi_sz(&wide_of_bytes(&bytes)), [r"ROOT\AcxLlCableCall"]);
        let two = multi_sz_bytes(&["a", "bc"]);
        let mut padded = wide_of_bytes(&two);
        padded.extend([0, 0, 0, 7]);
        assert_eq!(parse_multi_sz(&padded), ["a", "bc"]);
        assert!(parse_multi_sz(&[]).is_empty());
        assert_eq!(from_wide(&[b'h' as u16, b'i' as u16, 0, b'x' as u16]), "hi");
        assert_eq!(sz_bytes("ACX-LL-Cable D").len(), ("ACX-LL-Cable D".len() + 1) * 2);
    }

    fn bound(health: Health, inf: &str, version: &str) -> Bound {
        Bound { health, inf: Some(inf.into()), driver_version: Some(version.into()) }
    }

    /// Astra's review: the install accepted a failed bind if Windows started the device with some driver.
    #[test]
    fn an_install_is_kept_only_when_working_on_the_chosen_package() {
        use Health::*;
        let c = candidate(r"C:\Windows\INF\oem12.inf", "audiocodec.inf", 200, V1);
        let failed = WinError { call: "DiInstallDevice", code: 0xE000_022F };
        let on_chosen = bound(Working, "oem12.inf", "1.0.0.0");
        assert_eq!(judge_install(Ok(()), false, &on_chosen, &c), Ok(Installed::Working));
        assert_eq!(judge_install(Err(&failed), false, &on_chosen, &c), Ok(Installed::Working));
        let other = bound(Working, "oem3.inf", "0.9.0.300");
        assert_eq!(
            judge_install(Err(&failed), false, &other, &c),
            Err(InstallFailure::OtherPackage { error: Some(failed.clone()), now: other.clone() })
        );
        let bare = Bound { health: Problem(28), inf: None, driver_version: None };
        assert_eq!(judge_install(Err(&failed), false, &bare, &c), Err(InstallFailure::Call { error: failed.clone(), now: bare.clone() }));
        for state in [NotStarted, Problem(10), Problem(31), Unknown(13)] {
            let now = Bound { health: state, ..on_chosen.clone() };
            assert_eq!(judge_install(Ok(()), false, &now, &c), Err(InstallFailure::NotWorking { now: now.clone() }), "{state:?}");
        }
        assert_eq!(judge_install(Ok(()), true, &bare, &c), Ok(Installed::Restart));
        assert_eq!(judge_install(Ok(()), false, &Bound { health: Problem(14), ..on_chosen.clone() }, &c), Ok(Installed::Restart));
        assert!(matches!(judge_install(Err(&failed), true, &bare, &c), Err(InstallFailure::Call { .. })));
        let text = InstallFailure::NotWorking { now: Bound { health: NotStarted, ..bare } }.to_string();
        assert_eq!(text, "5 s after DiInstallDevice, the device: not started, no driver");
        assert_eq!(on_chosen.running(), Some(Running { inf: "oem12.inf".into(), version: "1.0.0.0".into() }));
    }

    #[test]
    fn health_reads_the_status_flags() {
        use windows::Win32::Devices::DeviceAndDriverInstallation::{DN_HAS_PROBLEM, DN_STARTED};
        assert_eq!(Health::of(0, DN_STARTED.0, 0), Health::Working);
        assert_eq!(Health::of(0, 0, 0), Health::NotStarted);
        assert_eq!(Health::of(0, DN_HAS_PROBLEM.0, 22), Health::Problem(22));
        assert_eq!(Health::of(13, DN_STARTED.0, 0), Health::Unknown(13));
        assert_eq!(Health::Problem(31).describe(), "a problem: Code 31 (CM_PROB_FAILED_ADD)");
        assert_eq!(Health::Unknown(13).describe(), "state unknown (CM_Get_DevNode_Status: CR_NO_SUCH_DEVNODE, 13)");
        // A node whose search failed names that call, not the status's (cables::present)
        assert_eq!(Health::Unlocated(19).describe(), "state unknown (CM_Locate_DevNodeW: CR_FAILURE, 19)");
        assert_eq!(Health::Unlocated(30).describe(), "state unknown (CM_Locate_DevNodeW: CONFIGRET 30)");
    }

    /// Which microphones a package makes comes from its INF, the install section the hardware ID names: the repo's INF (D1
    /// keeps the three sections' names), and one cut to the single-microphone driver's.
    #[test]
    fn a_packages_microphones_are_its_infs_interfaces() {
        use Mic::*;
        let inf = concat!(env!("CARGO_MANIFEST_DIR"), r"\..\..\driver\src\AudioCodec\Driver\AudioCodec.inf");
        assert_eq!(package_mics(inf, "Audio_Device").unwrap(), [LowLatency, Call]);
        assert_eq!(package_mics(inf, "Audio_Device_Call").unwrap(), [Call]);
        assert_eq!(package_mics(inf, "Audio_Device_LowLatency").unwrap(), [LowLatency]);
        assert_eq!(package_mics(inf, "No_Such_Section").unwrap(), []);
        let missing = package_mics(r"C:\no\such\folder\x.inf", "Audio_Device").unwrap_err();
        assert_eq!(missing.call, "SetupOpenInfFileW");
    }
}
