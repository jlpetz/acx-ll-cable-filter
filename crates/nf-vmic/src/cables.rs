//! The cables present and their endpoints' kinds (TODO 19, the per-cable design, doc/todo/19-per-cable-design.md, sections 3,
//! 4 and 10; this is N0's slice of it, the interim one).
//!
//! Every cable is a device node of the one driver package, and the setup tool gives each node a fixed instance ID,
//! `ROOT\MEDIA\ACXLLCABLE_<label>_<id>_<n>`: the label, the identity and an instance number n from 1 ([`crate::identity`], whose
//! [`CableInstance::parse`] parses it as the driver does). The driver names the cable's control device from the identity,
//! `\\.\AcxLlCable_<id>` ([`crate::protocol::control_path`]).
//!
//! - [`parse_control_path`]: the identity back from a control path (`nf-vmic-test --path`).
//! - [`EndpointKind`]: which of a cable's endpoints a KS filter's reference string names (Speaker0, Microphone0, Microphone1),
//!   and which a variant makes.
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
use windows::core::HSTRING;

use crate::identity::{CableId, CableInstance, ENUMERATOR, INSTANCE_PREFIX, LABEL_MAX_CHARS, Label, PRODUCT};
use crate::sessions::Mic;
use crate::setup::plan::Health;
use crate::variant::Variant;
use crate::win_error::WinError;

/// The low-latency microphone's KS filter, by its device interface's reference string: the INF's `KSNAME_Microphone`, the
/// driver's `captureCircuitName` (DriverSettings.h).
pub const MICROPHONE_FILTER: &str = "Microphone0";
/// The call microphone's KS filter: the INF's `KSNAME_CallMicrophone`, the driver's `capture2CircuitName`.
pub const CALL_MICROPHONE_FILTER: &str = "Microphone1";
/// The render circuit's KS filter: the INF's `KSNAME_Speaker`.
pub const SPEAKERS_FILTER: &str = "Speaker0";

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
    CableId::from_str(&p[prefix.len()..]).ok()
}

/// One of a cable's endpoints, by the KS filter its MMDevice endpoint is connected to: the circuits' reference strings
/// (`Speaker0`, `Microphone0`, `Microphone1`). A cable makes its Speakers and the microphones its variant has
/// ([`EndpointKind::of_variant`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
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
            EndpointKind::Speakers => SPEAKERS_FILTER,
            EndpointKind::Microphone => MICROPHONE_FILTER,
            EndpointKind::CallMicrophone => CALL_MICROPHONE_FILTER,
        }
    }

    /// The stable role string used by endpoint-map files and by packages with the original friendly names.
    pub fn name(self) -> &'static str {
        match self {
            EndpointKind::Speakers => "Speakers",
            EndpointKind::Microphone => "Microphone",
            EndpointKind::CallMicrophone => "Call Microphone",
        }
    }

    /// The endpoint's direction as seen from the cable. Out-5ms carries the same samples with a 5 ms default engine period.
    pub fn port(self) -> &'static str {
        match self {
            Self::Speakers => "In",
            Self::Microphone => "Out",
            Self::CallMicrophone => "Out-5ms",
        }
    }

    /// The endpoint's default display label, before Windows adds the adapter name in parentheses.
    pub fn display_name(self, cable: &Label) -> String {
        format!("LLCable-{cable} {}", self.port())
    }

    /// The complete default MMDevice name, with the existing cable adapter in parentheses.
    pub fn friendly_name(self, cable: &Label) -> String {
        format!("{} ({})", self.display_name(cable), cable.friendly_name())
    }

    /// `speakers`, `microphone`, `call-microphone`: the role on a command line (`acx-ll-cable endpoint`).
    pub fn arg(self) -> &'static str {
        match self {
            EndpointKind::Speakers => "speakers",
            EndpointKind::Microphone => "microphone",
            EndpointKind::CallMicrophone => "call-microphone",
        }
    }

    /// A role from the command line: [`EndpointKind::arg`]'s forms or [`EndpointKind::name`]'s ("Call Microphone"), in any
    /// case.
    pub fn from_arg(s: &str) -> Option<Self> {
        let s = s.trim();
        [EndpointKind::Speakers, EndpointKind::Microphone, EndpointKind::CallMicrophone]
            .into_iter()
            .find(|k| s.eq_ignore_ascii_case(k.arg()) || s.eq_ignore_ascii_case(k.name()) || s.eq_ignore_ascii_case(k.port()))
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

    /// A variant's endpoints: the Speakers, then its microphones.
    pub fn of_variant(v: Variant) -> Vec<Self> {
        let mut kinds = vec![EndpointKind::Speakers];
        kinds.extend(v.mics().iter().map(|m| match m {
            Mic::Call => EndpointKind::CallMicrophone,
            _ => EndpointKind::Microphone,
        }));
        kinds
    }
}

/// One of our cables' device nodes, present in the device tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresentCable {
    /// As Plug and Play keeps it (`ROOT\MEDIA\ACXLLCABLE_A_<ID>_1`).
    pub instance_id: String,
    pub cable: CableInstance,
    /// Started, disabled (Code 22), refused at AddDevice (Code 31: the boot crash breaker), and so on.
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

/// A device node's state ([`crate::setup::devices::health`]), or `None` if it isn't in the device tree (CR_NO_SUCH_DEVNODE from either
/// call: not present, or gone since the list). A search that fails otherwise is [`Health::Unlocated`], which names its call.
fn health_of(instance_id: &str) -> Option<Health> {
    let id = HSTRING::from(instance_id);
    let mut devinst = 0u32;
    // SAFETY: `id` is NUL-terminated and outlives the call; `devinst` is a live out-pointer.
    let cr = unsafe { CM_Locate_DevNodeW(&mut devinst, &id, CM_LOCATE_DEVNODE_NORMAL) };
    match cr {
        CR_SUCCESS => match crate::setup::devices::health(devinst) {
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
                "no {PRODUCT} is installed: no device node is {ENUMERATOR}\\{INSTANCE_PREFIX}<label>_<id>_<n> (the setup tool makes \
                 them; the old driver's ROOT\\MEDIA\\NNNN nodes aren't cables)"
            ),
            NoDefault::Ambiguous(cables) => {
                let list: Vec<String> = cables.iter().map(PresentCable::describe).collect();
                let a = cables.iter().filter(|c| c.cable.label == Label::a()).count();
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
    let labelled_a: Vec<&PresentCable> = cables.iter().filter(|c| c.cable.label == Label::a()).collect();
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
        for k in [EndpointKind::Speakers, EndpointKind::Microphone, EndpointKind::CallMicrophone] {
            assert_eq!(EndpointKind::from_arg(k.arg()), Some(k));
            assert_eq!(EndpointKind::from_arg(&k.name().to_uppercase()), Some(k));
            assert_eq!(EndpointKind::from_arg(&k.port().to_uppercase()), Some(k));
        }
        assert_eq!(EndpointKind::from_arg(" Call-Microphone "), Some(EndpointKind::CallMicrophone));
        for none in ["", "call", "mic", "speaker", "microphone0", "callmicrophone"] {
            assert_eq!(EndpointKind::from_arg(none), None, "{none:?}");
        }
        use EndpointKind::*;
        assert_eq!(EndpointKind::of_variant(Variant::Both), [Speakers, Microphone, CallMicrophone]);
        assert_eq!(EndpointKind::of_variant(Variant::Call), [Speakers, CallMicrophone]);
        assert_eq!(EndpointKind::of_variant(Variant::LowLatency), [Speakers, Microphone]);
    }

    fn node(label: &str, n: u32) -> PresentCable {
        let cable = CableInstance { label: label.parse().unwrap(), id: id(), n };
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
