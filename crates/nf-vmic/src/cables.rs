//! A cable's identity, the instance ID that carries it, and the cables present (TODO 19, the per-cable design,
//! doc/todo/19-per-cable-design.md, sections 3, 4 and 10; this is N0's slice of it, the interim one).
//!
//! Every cable is a device node of the one driver package, and the setup tool gives each node a fixed instance ID,
//! `ROOT\MEDIA\ACXLLCABLE_<label>_<id>_<n>`: the label (one to eight of A-Z and 0-9, "D"), the identity ([`CableId`], a GUID that
//! outlives removal) and an instance number n from 1. The driver parses it at AddDevice (D1, `Device.cpp`,
//! `NfParseCableInstanceId`) and names the cable's control device from the identity, `\\.\AcxLlCable_<id>`
//! ([`crate::protocol::control_path`]). [`CableInstance::parse`] parses it as the driver does, so the app, the tools and the
//! driver agree on which node is which cable.
//!
//! - [`CableId`]: the identity, written as its 36 lower-case characters (the driver's form), read in either case.
//! - [`CableInstance`]: what an instance ID names, parsed from it or written as the setup tool writes it.
//! - [`parse_control_path`]: the identity back from a control path (`nf-vmic-test --path`).
//! - [`EndpointKind`]: which of a cable's endpoints a KS filter's reference string names (Speaker0, Microphone0, Microphone1).
//! - [`present`] and [`choose_default`]: the cables whose device nodes are present, from Plug and Play's list of device
//!   instance IDs (CfgMgr32), and the one a tool uses when it isn't told which: the cable labelled A, else the only one. This
//!   is interim (N0): discovery through each cable's own device interface and its properties comes with D2 and N1, and the
//!   broker takes a cable's identity with B1.

use std::str::FromStr;
use std::{fmt, io};

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GETIDLIST_FILTER_ENUMERATOR, CM_Get_Device_ID_List_SizeW, CM_Get_Device_ID_ListW, CM_LOCATE_DEVNODE_NORMAL, CM_Locate_DevNodeW,
    CM_MapCrToWin32Err, CONFIGRET, CR_BUFFER_SMALL, CR_NO_SUCH_DEVNODE, CR_SUCCESS,
};
use windows::Win32::Foundation::{ERROR_AMBIGUOUS_SYSTEM_DEVICE, ERROR_FILE_NOT_FOUND, ERROR_GEN_FAILURE};
use windows::core::{GUID, HSTRING};

use crate::variant::Health;
use crate::win_error::WinError;

/// The product's short name (the owner, 2026-11-24): the driver's manufacturer and device description, and the start of each
/// cable's friendly name, "ACX-LL-Cable D", which Windows puts in its endpoints' names ("Microphone (ACX-LL-Cable D)").
pub const NAME: &str = "ACX-LL-Cable";
/// What follows the instance ID's last backslash starts with this (Cable.h's `NF_CABLE_INSTANCE_PREFIX`), in any case.
pub const INSTANCE_PREFIX: &str = "ACXLLCABLE_";
/// Where the setup tool makes the nodes: a root-enumerated device of the MEDIA class. The driver doesn't check this part.
pub const INSTANCE_PARENT: &str = r"ROOT\MEDIA";
/// A label is one to this many of A-Z and 0-9 (Cable.h's `NF_CABLE_LABEL_MAX_CHARS`).
pub const LABEL_MAX_CHARS: usize = 8;
/// An identity's characters, `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` (Cable.h's `NF_CABLE_ID_CHARS`).
pub const ID_CHARS: usize = 36;

/// A cable's identity: a GUID that outlives the cable's removal (the setup tool keeps it). Written as the driver writes it, 36
/// lower-case characters without braces; read in either case, but only in that form.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CableId(pub GUID);

impl CableId {
    /// The 36 characters `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`, hex digits in either case, hyphens where they go, nothing else
    /// (the driver's `NfParseGuid36`).
    fn parse36(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() != ID_CHARS {
            return None;
        }
        let mut value = 0u128;
        for (i, &b) in bytes.iter().enumerate() {
            if matches!(i, 8 | 13 | 18 | 23) {
                if b != b'-' {
                    return None;
                }
                continue;
            }
            value = value << 4 | u128::from((b as char).to_digit(16)?);
        }
        // The text's order is the GUID's: Data1, Data2 and Data3 most significant first, then Data4's bytes, as from_u128 reads.
        Some(Self(GUID::from_u128(value)))
    }
}

impl fmt::Display for CableId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = self.0.to_u128();
        write!(
            f,
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            v >> 96,
            (v >> 80) & 0xFFFF,
            (v >> 64) & 0xFFFF,
            (v >> 48) & 0xFFFF,
            v & 0xFFFF_FFFF_FFFF
        )
    }
}

impl fmt::Debug for CableId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CableId({self})")
    }
}

/// A string that isn't a cable's identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadCableId(pub String);

impl fmt::Display for BadCableId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} isn't a cable's identity: a GUID's {ID_CHARS} characters, xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx, no braces", self.0)
    }
}

impl std::error::Error for BadCableId {}

impl FromStr for CableId {
    type Err = BadCableId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse36(s).ok_or_else(|| BadCableId(s.to_string()))
    }
}

/// What a cable's instance ID names: `ROOT\MEDIA\ACXLLCABLE_<label>_<id>_<n>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CableInstance {
    /// One to eight of A-Z and 0-9, upper case: "D".
    pub label: String,
    pub id: CableId,
    /// The instance number, from 1: kept when a cable is re-added, the next for a variant switch (the design, section 4).
    pub n: u32,
}

impl CableInstance {
    /// Parses an instance ID as the driver does at AddDevice (D1, `NfParseCableInstanceId`): what follows its last backslash
    /// (the whole string if it has none), without case, must be the prefix `ACXLLCABLE_`, a label of one to eight of A-Z and
    /// 0-9, `_`, the identity's 36 characters, `_`, and n in decimal, 1 to 4294967295 with no leading zero and nothing after it.
    /// The part before the last backslash isn't checked. `None` for anything else: a node the setup tool didn't make
    /// (`ROOT\MEDIA\0004`), which the driver refuses (code 31). KS filter paths carry the instance ID in lower case
    /// (`root\media\acxllcable_d_..._1`, [`crate::sessions::parse_filter`]); the label comes back upper case.
    pub fn parse(instance_id: &str) -> Option<Self> {
        let tail = instance_id.rsplit('\\').next().unwrap_or(instance_id);
        if !tail.get(..INSTANCE_PREFIX.len())?.eq_ignore_ascii_case(INSTANCE_PREFIX) {
            return None;
        }
        let rest = &tail[INSTANCE_PREFIX.len()..];
        let (label, rest) = rest.split_once('_')?;
        if label.is_empty() || label.len() > LABEL_MAX_CHARS || !label.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        let (id, n) = (rest.get(..ID_CHARS)?, rest.get(ID_CHARS..)?);
        let id = CableId::parse36(id)?;
        let n = n.strip_prefix('_')?;
        if n.is_empty() || n.len() > 10 || n.starts_with('0') || !n.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(Self { label: label.to_ascii_uppercase(), id, n: n.parse().ok()? })
    }

    /// The instance ID as the setup tool writes it, upper case: `ROOT\MEDIA\ACXLLCABLE_D_<ID>_1`. Windows and the driver compare
    /// it without case.
    pub fn instance_id(&self) -> String {
        format!(r"{INSTANCE_PARENT}\{INSTANCE_PREFIX}{}_{}_{}", self.label, self.id.to_string().to_ascii_uppercase(), self.n)
    }

    /// "ACX-LL-Cable D": the friendly name the driver gives the cable's device, and the part of its endpoints' names in brackets.
    pub fn name(&self) -> String {
        format!("{NAME} {}", self.label)
    }
}

/// Whether `path` is one of the driver's old control-device names, `\\.\NoiseFilterVmic` or `\\.\NoiseFilterVmic<letter>`, which
/// the per-cable driver no longer makes (any case). The owner's aliases for them are served through discovery, N1's selector.
pub fn is_old_name(path: &str) -> bool {
    const OLD: &str = r"\\.\NoiseFilterVmic";
    let p = path.trim();
    p.get(..OLD.len()).is_some_and(|s| s.eq_ignore_ascii_case(OLD))
        && p.len() <= OLD.len() + LABEL_MAX_CHARS
        && p.bytes().skip(OLD.len()).all(|b| b.is_ascii_alphanumeric())
}

/// The identity a control path names: `\\.\AcxLlCable_<id>` ([`crate::protocol::control_path`]), the prefix and the identity in
/// either case (the object manager looks the name up without case), surrounding white space ignored, nothing else. The device
/// is then opened at the canonical path, `control_path` of what this returns.
pub fn parse_control_path(path: &str) -> Option<CableId> {
    let p = path.trim();
    let prefix = crate::protocol::USER_PREFIX;
    p.get(..prefix.len()).filter(|s| s.eq_ignore_ascii_case(prefix))?;
    CableId::parse36(&p[prefix.len()..])
}

/// One of a cable's endpoints, by the KS filter its MMDevice endpoint is connected to: the circuits' reference strings
/// (`Speaker0`, `Microphone0`, `Microphone1`). A cable makes its Speakers and the microphones its variant has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EndpointKind {
    /// Speaker0: "Speakers (ACX-LL-Cable D)", what is played into the cable.
    Speakers,
    /// Microphone0: "Microphone (ACX-LL-Cable D)", the low-latency one (IAudioClient3's small periods).
    Microphone,
    /// Microphone1: "Call Microphone (ACX-LL-Cable D)", a 5 ms default period for apps that ask for none.
    CallMicrophone,
}

impl EndpointKind {
    /// The circuit's reference string, as the INF and the driver write it.
    pub fn reference(self) -> &'static str {
        match self {
            EndpointKind::Speakers => "Speaker0",
            EndpointKind::Microphone => "Microphone0",
            EndpointKind::CallMicrophone => "Microphone1",
        }
    }

    /// "Speakers", "Microphone", "Call Microphone": the part of the endpoint's name before the brackets, as Windows names it.
    pub fn name(self) -> &'static str {
        match self {
            EndpointKind::Speakers => "Speakers",
            EndpointKind::Microphone => "Microphone",
            EndpointKind::CallMicrophone => "Call Microphone",
        }
    }

    /// Render (the Speakers) or capture (a microphone).
    pub fn is_render(self) -> bool {
        self == EndpointKind::Speakers
    }

    /// The endpoint a KS filter's reference string names, in any case, with or without a suffix after the name
    /// (`microphone0/00010001`: whatever follows its first character that isn't a letter or a digit), so `microphone01` and
    /// `speaker01` are none of them.
    pub fn of_reference(reference: &str) -> Option<Self> {
        let name = reference.trim_start().split(|c: char| !c.is_ascii_alphanumeric()).next().unwrap_or_default();
        [EndpointKind::Speakers, EndpointKind::Microphone, EndpointKind::CallMicrophone]
            .into_iter()
            .find(|k| name.eq_ignore_ascii_case(k.reference()))
    }
}

/// One of our cables' device nodes, present in the device tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresentCable {
    /// As Plug and Play keeps it (`ROOT\MEDIA\ACXLLCABLE_A_<ID>_1`).
    pub instance_id: String,
    pub cable: CableInstance,
    /// Started, disabled (Code 22), refused at AddDevice (Code 31: the boot crash breaker, or a duplicate's ...), and so on.
    pub health: Health,
}

impl PresentCable {
    /// "cable A (ROOT\MEDIA\ACXLLCABLE_A_..._1, working)".
    pub fn describe(&self) -> String {
        format!("cable {} ({}, {})", self.cable.label, self.instance_id, self.health.describe())
    }
}

/// The cables whose device nodes are present: every root-enumerated device instance ID Plug and Play has
/// (`CM_Get_Device_ID_ListW` with the enumerator `ROOT`) that [`CableInstance::parse`] reads, and that is in the device tree now
/// (`CM_Locate_DevNodeW`'s normal search: a removed node whose key remains isn't), with its state (`CM_Get_DevNode_Status`).
/// Any user may call it. In the order Plug and Play lists them. A node that goes away between the list and the search isn't
/// listed.
pub fn present() -> Result<Vec<PresentCable>, WinError> {
    let mut out = Vec::new();
    for instance_id in device_ids("ROOT")? {
        let Some(cable) = CableInstance::parse(&instance_id) else { continue };
        let Some(health) = health_of(&instance_id) else { continue };
        out.push(PresentCable { instance_id, cable, health });
    }
    Ok(out)
}

/// A CONFIGRET as a [`WinError`], by CfgMgr32's own mapping to a Win32 code (ERROR_GEN_FAILURE where it has none).
fn cr_error(call: &'static str, cr: CONFIGRET) -> WinError {
    // SAFETY: a pure mapping of two integers.
    WinError { call, code: unsafe { CM_MapCrToWin32Err(cr, ERROR_GEN_FAILURE.0) } }
}

/// The device instance IDs under the enumerator `enumerator`, present or not. Retried while the list grows between the size's
/// query and the list's (a device can arrive in between).
fn device_ids(enumerator: &str) -> Result<Vec<String>, WinError> {
    let filter = HSTRING::from(enumerator);
    for _ in 0..8 {
        let mut chars = 0u32;
        // SAFETY: `filter` is NUL-terminated and outlives the call; `chars` is a live out-pointer.
        let cr = unsafe { CM_Get_Device_ID_List_SizeW(&mut chars, &filter, CM_GETIDLIST_FILTER_ENUMERATOR) };
        if cr != CR_SUCCESS {
            return Err(cr_error("CM_Get_Device_ID_List_SizeW", cr));
        }
        let mut buffer = vec![0u16; chars as usize];
        // SAFETY: as above; the buffer's length is passed with it.
        let cr = unsafe { CM_Get_Device_ID_ListW(&filter, &mut buffer, CM_GETIDLIST_FILTER_ENUMERATOR) };
        if cr == CR_BUFFER_SMALL {
            continue;
        }
        if cr != CR_SUCCESS {
            return Err(cr_error("CM_Get_Device_ID_ListW", cr));
        }
        return Ok(multi_sz(&buffer));
    }
    Err(cr_error("CM_Get_Device_ID_ListW", CR_BUFFER_SMALL))
}

/// A list of NUL-terminated strings ending in an empty one (REG_MULTI_SZ's form, as CfgMgr32 lists IDs).
fn multi_sz(buffer: &[u16]) -> Vec<String> {
    buffer.split(|&c| c == 0).take_while(|s| !s.is_empty()).map(String::from_utf16_lossy).collect()
}

/// A device node's state ([`crate::variant::health`]), or `None` if it isn't in the device tree (CR_NO_SUCH_DEVNODE from either
/// call: not present, or gone since the list). A search that fails otherwise is [`Health::Unlocated`], which names its call.
fn health_of(instance_id: &str) -> Option<Health> {
    let id = HSTRING::from(instance_id);
    let mut devinst = 0u32;
    // SAFETY: `id` is NUL-terminated and outlives the call; `devinst` is a live out-pointer.
    let cr = unsafe { CM_Locate_DevNodeW(&mut devinst, &id, CM_LOCATE_DEVNODE_NORMAL) };
    match cr {
        CR_SUCCESS => match crate::variant::health(devinst) {
            Health::Unknown(cr) if cr == CR_NO_SUCH_DEVNODE.0 => None,
            health => Some(health),
        },
        CR_NO_SUCH_DEVNODE => None,
        _ => Some(Health::Unlocated(cr.0)),
    }
}

/// Why no cable was chosen by default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoDefault {
    /// No device node of ours is present.
    NoCable,
    /// Two or more nodes are labelled A (a setup error), or there's no A and more than one cable: these.
    Ambiguous(Vec<PresentCable>),
}

impl NoDefault {
    /// The Win32 code a caller that reports only a code gets (the broker's reply): ERROR_FILE_NOT_FOUND, as an open of a
    /// cable that isn't there gets; ERROR_AMBIGUOUS_SYSTEM_DEVICE for several.
    pub fn win32(&self) -> u32 {
        match self {
            NoDefault::NoCable => ERROR_FILE_NOT_FOUND.0,
            NoDefault::Ambiguous(_) => ERROR_AMBIGUOUS_SYSTEM_DEVICE.0,
        }
    }
}

impl fmt::Display for NoDefault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NoDefault::NoCable => write!(
                f,
                "no {NAME} is installed: no device node is {INSTANCE_PARENT}\\{INSTANCE_PREFIX}<label>_<id>_<n> (the setup tool makes \
                 them; the old driver's ROOT\\MEDIA\\NNNN nodes aren't cables)"
            ),
            NoDefault::Ambiguous(cables) => {
                let list: Vec<String> = cables.iter().map(PresentCable::describe).collect();
                let a = cables.iter().filter(|c| c.cable.label == "A").count();
                let why = if a > 1 { "more than one is labelled A" } else { "none is labelled A, and there are several" };
                write!(f, "which cable? {why}: {}", list.join("; "))
            }
        }
    }
}

impl std::error::Error for NoDefault {}

/// The cable a tool uses when it isn't told which (the design, section 10): the one labelled A, else the only cable. Whatever
/// its state: an open of a cable that isn't working says so.
pub fn choose_default(cables: &[PresentCable]) -> Result<&PresentCable, NoDefault> {
    let labelled_a: Vec<&PresentCable> = cables.iter().filter(|c| c.cable.label == "A").collect();
    match (labelled_a.as_slice(), cables) {
        ([a], _) => Ok(*a),
        ([], []) => Err(NoDefault::NoCable),
        ([], [only]) => Ok(only),
        ([], _) => Err(NoDefault::Ambiguous(cables.to_vec())),
        (_, _) => Err(NoDefault::Ambiguous(labelled_a.into_iter().cloned().collect())),
    }
}

/// The default cable's control path ([`choose_default`] over [`present`]), built from its identity. Its errors carry a Win32
/// code alone ([`NoDefault::win32`], or the failed call's), for callers that pass on only a code (the broker's helper).
pub fn default_control_path() -> io::Result<String> {
    let cables = present().map_err(|e| io::Error::from_raw_os_error(e.code as i32))?;
    let chosen = choose_default(&cables).map_err(|e| io::Error::from_raw_os_error(e.win32() as i32))?;
    Ok(crate::protocol::control_path(&chosen.cable.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure: nothing here asks Plug and Play or opens a device.

    const ID: &str = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0";

    fn id() -> CableId {
        ID.parse().unwrap()
    }

    #[test]
    fn an_identity_reads_either_case_and_writes_lower_case() {
        let id = id();
        assert_eq!(id.0, GUID::from_u128(0x0f1e2d3c_4b5a_6978_8796_a5b4c3d2e1f0));
        assert_eq!(id.to_string(), ID);
        assert_eq!(ID.to_ascii_uppercase().parse::<CableId>(), Ok(id));
        assert_eq!("0F1e2D3c-4b5A-6978-8796-A5b4c3d2e1F0".parse::<CableId>(), Ok(id));
        assert_eq!(format!("{id:?}"), format!("CableId({ID})"));
        // The GUID's fields in the text's order, as the driver's NfParseGuid36 fills them.
        assert_eq!((id.0.data1, id.0.data2, id.0.data3), (0x0f1e_2d3c, 0x4b5a, 0x6978));
        assert_eq!(id.0.data4, [0x87, 0x96, 0xa5, 0xb4, 0xc3, 0xd2, 0xe1, 0xf0]);
        // Leading zeros are written, every field its width.
        let small: CableId = "00000001-0002-0003-0004-000000000005".parse().unwrap();
        assert_eq!(small.to_string(), "00000001-0002-0003-0004-000000000005");
        for bad in [
            "",
            "{0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0}",
            "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f",
            "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f00",
            "0f1e2d3c4b5a-6978-8796-a5b4c3d2e1f0-",
            "0f1e2d3c_4b5a_6978_8796_a5b4c3d2e1f0",
            "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1fg",
            " 0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f",
            "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f\u{e9}",
            "+f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0",
        ] {
            assert_eq!(bad.parse::<CableId>(), Err(BadCableId(bad.into())), "{bad:?}");
        }
        assert!(BadCableId("x".into()).to_string().contains("36 characters"));
    }

    #[test]
    fn an_instance_id_is_parsed_as_the_driver_parses_it() {
        let want = CableInstance { label: "D".into(), id: id(), n: 1 };
        let upper = format!(r"ROOT\MEDIA\ACXLLCABLE_D_{}_1", ID.to_ascii_uppercase());
        assert_eq!(CableInstance::parse(&upper), Some(want.clone()));
        assert_eq!(want.instance_id(), upper, "the setup tool's form, upper case");
        assert_eq!(want.name(), "ACX-LL-Cable D");
        // As a KS filter's path carries it: lower case; the label comes back upper case.
        assert_eq!(CableInstance::parse(&format!(r"root\media\acxllcable_d_{ID}_1")), Some(want.clone()));
        // Only what follows the last backslash counts, as in the driver; none at all is the whole string.
        assert_eq!(CableInstance::parse(&format!(r"ROOT\OTHER\ACXLLCABLE_D_{ID}_1")), Some(want.clone()));
        assert_eq!(CableInstance::parse(&format!("ACXLLCABLE_D_{ID}_1")), Some(want.clone()));
        // Labels: one to eight of A-Z and 0-9.
        let with = |label: &str, n: &str| CableInstance::parse(&format!(r"ROOT\MEDIA\ACXLLCABLE_{label}_{ID}_{n}"));
        assert_eq!(with("CABLE12Z", "7").map(|c| (c.label, c.n)), Some(("CABLE12Z".into(), 7)));
        assert_eq!(with("0", "1").map(|c| c.label), Some("0".into()));
        assert_eq!(with("b", "4294967295").map(|c| (c.label, c.n)), Some(("B".into(), u32::MAX)));
        for label in ["", "ABCDEFGHI", "A-B", "A B", "\u{c4}", "\u{131}"] {
            assert_eq!(with(label, "1"), None, "label {label:?}");
        }
        // n: decimal from 1, no leading zero, nothing after it, at most 4294967295.
        for n in ["", "0", "01", "4294967296", "99999999999", "1x", "1 ", "+1", "-1", "1_"] {
            assert_eq!(with("D", n), None, "n {n:?}");
        }
        // The identity: its 36 characters, then an underscore.
        for bad in [
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{{{ID}}}_1"),
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{}_1", &ID[1..]),
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{ID}1"),
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{ID}"),
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{ID}_"),
            format!(r"ROOT\MEDIA\ACXLLCABLE_D{ID}_1"),
            format!(r"ROOT\MEDIA\ACXLLCABLE__{ID}_1"),
            format!(r"ROOT\MEDIA\ACXLLCABLED_{ID}_1"),
            format!(r"ROOT\MEDIA\XACXLLCABLE_D_{ID}_1"),
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{ID}_1\"),
            r"ROOT\MEDIA\0004".to_string(),
            r"ROOT\NoiseFilterCableB\0000".to_string(),
            String::new(),
            r"\".to_string(),
        ] {
            assert_eq!(CableInstance::parse(&bad), None, "{bad:?}");
        }
        // A multi-byte character where the identity starts can't split a character.
        assert_eq!(CableInstance::parse("ACXLLCABLE_D_\u{e9}"), None);
    }

    #[test]
    fn a_control_path_names_its_cable() {
        let id = id();
        let path = crate::protocol::control_path(&id);
        assert_eq!(path, format!(r"\\.\AcxLlCable_{ID}"));
        assert_eq!(parse_control_path(&path), Some(id));
        // Either case, surrounding white space ignored: the device is then opened at the canonical path.
        assert_eq!(parse_control_path(&format!(r" \\.\ACXLLCABLE_{} ", ID.to_ascii_uppercase())), Some(id));
        for bad in [
            String::new(),
            ID.to_string(),
            format!(r"\\.\AcxLlCable_{{{ID}}}"),
            format!(r"\\.\AcxLlCable_{ID}x"),
            format!(r"\\.\AcxLlCable{ID}"),
            format!(r"\\?\AcxLlCable_{ID}"),
            format!(r"\Device\AcxLlCable_{ID}"),
            format!(r"\\.\AcxLlCable_{}", &ID[..35]),
            r"\\.\NoiseFilterVmic".to_string(),
        ] {
            assert_eq!(parse_control_path(&bad), None, "{bad:?}");
        }
        // The old names are told apart, so a tool can say they've gone.
        for old in [r"\\.\NoiseFilterVmic", r"\\.\noisefiltervmic", r"\\.\NoiseFilterVmicB", r" \\.\NoiseFilterVmicC "] {
            assert!(is_old_name(old), "{old:?}");
        }
        for not_old in [r"\\.\NoiseFilterVmic_B", r"\\.\NoiseFilterVmicABCDEFGHI", r"\\.\NoiseFilterVmi", &path] {
            assert!(!is_old_name(not_old), "{not_old:?}");
        }
    }

    #[test]
    fn a_reference_string_names_an_endpoint() {
        assert_eq!(EndpointKind::of_reference("speaker0"), Some(EndpointKind::Speakers));
        assert_eq!(EndpointKind::of_reference("Speaker0"), Some(EndpointKind::Speakers));
        assert_eq!(EndpointKind::of_reference("MICROPHONE0"), Some(EndpointKind::Microphone));
        assert_eq!(EndpointKind::of_reference("microphone1/00010001"), Some(EndpointKind::CallMicrophone));
        for none in ["", "speaker01", "speaker", "microphone2", "pin3", "wave"] {
            assert_eq!(EndpointKind::of_reference(none), None, "{none:?}");
        }
        assert!(EndpointKind::Speakers.is_render() && !EndpointKind::Microphone.is_render() && !EndpointKind::CallMicrophone.is_render());
        assert_eq!(EndpointKind::CallMicrophone.name(), "Call Microphone");
    }

    fn node(label: &str, n: u32) -> PresentCable {
        let cable = CableInstance { label: label.into(), id: id(), n };
        PresentCable { instance_id: cable.instance_id(), cable, health: Health::Working }
    }

    #[test]
    fn the_default_cable_is_a_else_the_only_one() {
        assert_eq!(choose_default(&[]), Err(NoDefault::NoCable));
        let (a, b, c) = (node("A", 1), node("B", 1), node("C", 2));
        assert_eq!(choose_default(std::slice::from_ref(&b)), Ok(&b), "the only cable, whatever its label");
        assert_eq!(choose_default(&[b.clone(), a.clone(), c.clone()]), Ok(&a), "A among others");
        let none_a = [b.clone(), c.clone()];
        assert_eq!(choose_default(&none_a), Err(NoDefault::Ambiguous(none_a.to_vec())));
        let two_a = [a.clone(), b.clone(), node("A", 2)];
        assert_eq!(choose_default(&two_a), Err(NoDefault::Ambiguous(vec![a.clone(), node("A", 2)])));
        assert!(NoDefault::Ambiguous(vec![a.clone(), node("A", 2)]).to_string().contains("more than one is labelled A"));
        assert!(NoDefault::Ambiguous(none_a.to_vec()).to_string().contains("none is labelled A"));
        assert_eq!((NoDefault::NoCable.win32(), NoDefault::Ambiguous(vec![]).win32()), (2, 15250));
        assert_eq!(b.describe(), format!(r"cable B (ROOT\MEDIA\ACXLLCABLE_B_{}_1, working)", ID.to_ascii_uppercase()));
    }

    #[test]
    fn a_device_id_list_is_split_at_its_nuls() {
        let wide = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        assert_eq!(multi_sz(&wide("ROOT\\A\0ROOT\\B\0\0")), ["ROOT\\A", "ROOT\\B"]);
        assert_eq!(multi_sz(&wide("\0")), Vec::<String>::new());
        assert_eq!(multi_sz(&[]), Vec::<String>::new());
        assert_eq!(multi_sz(&wide("ROOT\\A")), ["ROOT\\A"], "a list cut short still yields what it has");
    }
}
