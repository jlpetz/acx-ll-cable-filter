//! A cable's names (TODO 19, the per-cable design, doc/todo/19-per-cable-design.md, section 4): its identity, a GUID that
//! outlives removal; its label, "D"; and the instance ID the setup tool gives its device node,
//! `ROOT\MEDIA\ACXLLCABLE_<label>_<id>_<n>`, from which the driver reads both at AddDevice (D1's `NfParseCableInstanceId`,
//! Device.cpp). Pure: parsing and formatting only. The one parse of it in user mode: the setup ([`crate::setup`]), the cables
//! present ([`crate::cables`]) and the endpoints' KS filters ([`crate::sessions`]) all read instance IDs here, so the tools,
//! the app and the driver agree on which node is which cable.
//!
//! The parse is the driver's: only what follows the last `\` counts, without case: the prefix `ACXLLCABLE_`, a label of one
//! to eight of A-Z and 0-9, `_`, the GUID's 36 characters (8-4-4-4-12 hex, no braces), `_`, and n in decimal, 1 to
//! 4294967295, with no leading zero and nothing after it. The tool writes the whole ID in upper case; the control device's
//! name has the identity in lower case (`\\.\AcxLlCable_<id>`, [`crate::protocol::control_path`]).

use std::fmt;
use std::str::FromStr;

use windows::core::GUID;

/// What every cable's instance ID has after its last `\` (Cable.h, NF_CABLE_INSTANCE_PREFIX).
pub const INSTANCE_PREFIX: &str = "ACXLLCABLE_";
/// Where the setup tool puts every cable's device node: the root enumerator's MEDIA class. The driver doesn't check this part.
pub const ENUMERATOR: &str = r"ROOT\MEDIA";
/// A label's most characters (Cable.h, NF_CABLE_LABEL_MAX_CHARS).
pub const LABEL_MAX_CHARS: usize = 8;
/// An identity's characters: a GUID without braces (Cable.h, NF_CABLE_ID_CHARS).
pub const ID_CHARS: usize = 36;
/// A device instance ID's most characters, its NUL excluded (MAX_DEVICE_ID_LEN, cfgmgr32.h, is 200 with it).
pub const INSTANCE_ID_MAX_CHARS: usize = 199;
/// The product's short name (the owner, 2026-11-24): every cable's manufacturer and device description, and the start of its
/// friendly name, "ACX-LL-Cable D" (D1's INF and `NfAssignFriendlyName`), which Windows puts in its endpoints' names
/// ("Microphone (ACX-LL-Cable D)").
pub const PRODUCT: &str = "ACX-LL-Cable";

/// A cable's identity: a GUID, shown as its 36 lower-case characters without braces (the driver's form), read in either case
/// but only in that form.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CableId(pub u128);

impl CableId {
    pub fn from_guid(g: GUID) -> Self {
        Self(g.to_u128())
    }

    pub fn guid(self) -> GUID {
        GUID::from_u128(self.0)
    }

    /// The 36 characters in upper case, as the tool writes them into an instance ID.
    pub fn upper(self) -> String {
        self.to_string().to_ascii_uppercase()
    }
}

impl fmt::Display for CableId {
    /// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`, lower case: the control device's name, the store's `Id`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = self.0;
        write!(
            f,
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            (v >> 96) as u32,
            (v >> 80) as u16,
            (v >> 64) as u16,
            (v >> 48) as u16,
            v & 0xFFFF_FFFF_FFFF
        )
    }
}

impl fmt::Debug for CableId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CableId({self})")
    }
}

/// Why a name didn't parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

impl FromStr for CableId {
    type Err = ParseError;

    /// The 36 characters in either case, as the driver takes them (no braces, nothing around them).
    fn from_str(s: &str) -> Result<Self, ParseError> {
        parse_id36(s).ok_or_else(|| ParseError(format!("'{s}' isn't a cable id: 36 characters, xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx")))
    }
}

/// A GUID's 36 characters, either case, checked character by character as the driver's NfParseGuid36 does. The text's order
/// is the GUID's: Data1, Data2 and Data3 most significant first, then Data4's bytes, as `GUID::from_u128` reads them.
fn parse_id36(s: &str) -> Option<CableId> {
    let b = s.as_bytes();
    if b.len() != ID_CHARS {
        return None;
    }
    let mut v: u128 = 0;
    for (i, &c) in b.iter().enumerate() {
        if matches!(i, 8 | 13 | 18 | 23) {
            if c != b'-' {
                return None;
            }
            continue;
        }
        let digit = (c as char).to_digit(16)?;
        v = (v << 4) | u128::from(digit);
    }
    Some(CableId(v))
}

/// A cable's label: one to eight of A-Z and 0-9, kept in upper case ("D"). Letters are labels, not slots: gaps are allowed.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Label(String);

impl Label {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The cable Install-Driver.ps1 makes first, and the one a tool uses when it isn't told which ([`crate::cables`]).
    pub fn a() -> Self {
        Self("A".into())
    }

    /// "ACX-LL-Cable D": the device's friendly name, which Windows puts in its endpoints' names.
    pub fn friendly_name(&self) -> String {
        format!("{PRODUCT} {}", self.0)
    }
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Label {
    type Err = ParseError;

    /// Any case ("d" is "D").
    fn from_str(s: &str) -> Result<Self, ParseError> {
        let ok = !s.is_empty() && s.len() <= LABEL_MAX_CHARS && s.bytes().all(|c| c.is_ascii_alphanumeric());
        if ok {
            Ok(Self(s.to_ascii_uppercase()))
        } else {
            Err(ParseError(format!("'{s}' isn't a cable label: one to {LABEL_MAX_CHARS} of A-Z and 0-9")))
        }
    }
}

/// A cable's device node, by its instance ID's parts.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CableInstance {
    pub label: Label,
    pub id: CableId,
    /// The instance number, from 1: kept on re-adding, the next for a variant switch.
    pub n: u32,
}

impl CableInstance {
    /// `ROOT\MEDIA\ACXLLCABLE_D_<ID>_1`, upper case, as the tool creates it. Windows and the driver compare it without case.
    pub fn instance_id(&self) -> String {
        format!(r"{ENUMERATOR}\{INSTANCE_PREFIX}{}_{}_{}", self.label, self.id.upper(), self.n)
    }

    /// The same cable's next instance (a variant switch).
    pub fn next(&self) -> Option<Self> {
        Some(Self { n: self.n.checked_add(1)?, ..self.clone() })
    }

    /// The instance ID's parts, as the driver reads them: what follows the last `\` (the whole string if it has none), without
    /// case; the part before it isn't checked. `None` for anything else: a node made outside the tool (`ROOT\MEDIA\0003`),
    /// which the driver refuses with code 31. KS filter paths carry the instance ID in lower case
    /// (`root\media\acxllcable_d_..._1`, [`crate::sessions::parse_filter`]); the label comes back upper case.
    pub fn parse(instance_id: &str) -> Option<Self> {
        let tail = instance_id.rsplit('\\').next().unwrap_or(instance_id);
        let prefix = tail.get(..INSTANCE_PREFIX.len())?;
        if !prefix.eq_ignore_ascii_case(INSTANCE_PREFIX) {
            return None;
        }
        let rest = &tail[INSTANCE_PREFIX.len()..];
        let (label, rest) = rest.split_once('_')?;
        let label = Label::from_str(label).ok()?;
        let id = parse_id36(rest.get(..ID_CHARS)?)?;
        let n = rest.get(ID_CHARS..)?.strip_prefix('_')?;
        if n.is_empty() || n.len() > 10 || n.starts_with('0') || !n.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let n = u32::try_from(n.parse::<u64>().ok()?).ok()?;
        Some(Self { label, id, n })
    }
}

/// Whether two instance IDs name one device node: Windows compares them without case.
pub fn same_instance(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0";

    fn id() -> CableId {
        ID.parse().unwrap()
    }

    #[test]
    fn an_identity_reads_and_shows_as_its_36_characters() {
        assert_eq!(id().to_string(), ID);
        assert_eq!(id().upper(), ID.to_ascii_uppercase());
        assert_eq!(ID.to_ascii_uppercase().parse::<CableId>().unwrap(), id(), "either case");
        assert_eq!("0F1e2D3c-4b5A-6978-8796-A5b4c3d2e1F0".parse::<CableId>().unwrap(), id(), "mixed case");
        assert_eq!(format!("{:?}", id()), format!("CableId({ID})"));
        assert_eq!(CableId::from_guid(id().guid()), id());
        assert_eq!(id().guid(), GUID::from_u128(0x0f1e2d3c_4b5a_6978_8796_a5b4c3d2e1f0));
        // The GUID's fields in the text's order, as the driver's NfParseGuid36 fills them
        let g = id().guid();
        assert_eq!((g.data1, g.data2, g.data3), (0x0f1e_2d3c, 0x4b5a, 0x6978));
        assert_eq!(g.data4, [0x87, 0x96, 0xa5, 0xb4, 0xc3, 0xd2, 0xe1, 0xf0]);
        // Leading zeros are written, every field its width
        let small: CableId = "00000001-0002-0003-0004-000000000005".parse().unwrap();
        assert_eq!(small.to_string(), "00000001-0002-0003-0004-000000000005");
        for bad in [
            "",
            "{0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0}",
            "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f",
            "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f00",
            "0f1e2d3c4b5a-6978-8796-a5b4c3d2e1f0-",
            "0f1e2d3c_4b5a_6978_8796_a5b4c3d2e1f0",
            "0f1e2d3g-4b5a-6978-8796-a5b4c3d2e1f0",
            " 0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f",
            "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f\u{e9}",
            "+f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0",
        ] {
            let e = bad.parse::<CableId>().unwrap_err();
            assert!(e.to_string().contains("36 characters"), "{bad:?}: {e}");
        }
    }

    #[test]
    fn a_label_is_one_to_eight_letters_or_digits() {
        assert_eq!("d".parse::<Label>().unwrap().as_str(), "D");
        assert_eq!("Cab2".parse::<Label>().unwrap().as_str(), "CAB2");
        assert_eq!("ABCDEFGH".parse::<Label>().unwrap().as_str(), "ABCDEFGH");
        assert_eq!("0".parse::<Label>().unwrap().as_str(), "0");
        for bad in ["", "ABCDEFGHI", "A_B", "A-B", "A B", "\u{c4}", "\u{131}", " A"] {
            assert!(bad.parse::<Label>().is_err(), "{bad:?}");
        }
        assert_eq!(Label::a().friendly_name(), "ACX-LL-Cable A");
    }

    #[test]
    fn an_instance_id_is_formatted_in_upper_case_and_parsed_as_the_driver_does() {
        let c = CableInstance { label: "d".parse().unwrap(), id: id(), n: 1 };
        let text = c.instance_id();
        assert_eq!(text, format!(r"ROOT\MEDIA\ACXLLCABLE_D_{}_1", ID.to_ascii_uppercase()));
        assert!(text.len() <= INSTANCE_ID_MAX_CHARS);
        assert_eq!(CableInstance::parse(&text), Some(c.clone()));
        // Without case (a KS filter's path has it in lower case; the label comes back upper case), and only what follows the
        // last backslash (the driver doesn't check ROOT\MEDIA); none at all is the whole string
        assert_eq!(CableInstance::parse(&text.to_ascii_lowercase()), Some(c.clone()));
        assert_eq!(CableInstance::parse(&format!("acxllcable_d_{ID}_1")), Some(c.clone()));
        assert_eq!(CableInstance::parse(&format!(r"X\Y\ACXLLCABLE_D_{ID}_1")), Some(c.clone()));
        assert_eq!(CableInstance::parse(&format!(r"ROOT\OTHER\ACXLLCABLE_D_{ID}_1")), Some(c.clone()));
        // Labels and n at their limits
        let with = |label: &str, n: &str| CableInstance::parse(&format!(r"ROOT\MEDIA\ACXLLCABLE_{label}_{ID}_{n}"));
        let parts = |c: Option<CableInstance>| c.map(|c| (c.label.to_string(), c.n));
        assert_eq!(parts(with("ABCDEFGH", "4294967295")), Some(("ABCDEFGH".into(), u32::MAX)));
        assert_eq!(parts(with("CABLE12Z", "7")), Some(("CABLE12Z".into(), 7)));
        assert_eq!(parts(with("0", "1")), Some(("0".into(), 1)));
        assert_eq!(parts(with("b", "10")), Some(("B".into(), 10)));
        for label in ["", "ABCDEFGHI", "A-B", "A B", "\u{c4}", "\u{131}"] {
            assert_eq!(with(label, "1"), None, "label {label:?}");
        }
        // n: decimal from 1, no leading zero, nothing after it, at most 4294967295
        for n in ["", "0", "01", "4294967296", "99999999999", "1x", "1 ", "+1", "-1", "1_"] {
            assert_eq!(with("D", n), None, "n {n:?}");
        }
        assert_eq!(c.next().unwrap().n, 2);
        assert_eq!(CableInstance { n: u32::MAX, ..c.clone() }.next(), None);
    }

    #[test]
    fn anything_else_has_no_identity() {
        #[rustfmt::skip]
        let bad = [
            r"ROOT\MEDIA\0003".to_string(),
            r"ROOT\NoiseFilterCableB\0000".to_string(),
            format!(r"ROOT\MEDIA\ACXLLCABLE__{ID}_1"),             // no label
            format!(r"ROOT\MEDIA\ACXLLCABLE_ABCDEFGHI_{ID}_1"),    // nine
            format!(r"ROOT\MEDIA\ACXLLCABLE_D-1_{ID}_1"),          // not A-Z0-9
            format!(r"ROOT\MEDIA\ACXLLCABLE_D{ID}_1"),             // no underscore after the label
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{{{ID}}}_1"),        // braces
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{}_1", &ID[1..]),    // 35 characters
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{ID}1"),             // no underscore after the identity
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{ID}"),              // no n
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{ID}_"),
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{ID}_0"),            // n from 1
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{ID}_01"),           // no leading zero
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{ID}_4294967296"),   // too large
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{ID}_1x"),           // nothing after
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{ID}_1\"),           // the tail is empty
            format!(r"ROOT\MEDIA\ACXLLCABLEX_D_{ID}_1"),
            format!(r"ROOT\MEDIA\ACXLLCABL_D_{ID}_1"),
            format!(r"ROOT\MEDIA\ACXLLCABLED_{ID}_1"),
            format!(r"ROOT\MEDIA\XACXLLCABLE_D_{ID}_1"),
            String::new(),
            r"\".to_string(),
        ];
        for b in &bad {
            assert_eq!(CableInstance::parse(b), None, "{b}");
        }
        // A multi-byte character where the identity starts can't split a character
        assert_eq!(CableInstance::parse("ACXLLCABLE_D_\u{e9}"), None);
    }

    #[test]
    fn instance_ids_compare_without_case() {
        assert!(same_instance(r"ROOT\MEDIA\ACXLLCABLE_D_X_1", r"root\media\acxllcable_d_x_1"));
        assert!(!same_instance(r"ROOT\MEDIA\0003", r"ROOT\MEDIA\0004"));
    }

    const DRIVER: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../driver/src/");

    /// The names here are the driver's (step 1's contract, D1): Cable.h's instance-ID parts, the INF's manufacturer and
    /// description, the friendly name AddDevice gives the device (Device.cpp, NfAssignFriendlyName), and its control device's
    /// identity in lower case.
    #[test]
    fn the_names_are_the_drivers() {
        let h = std::fs::read_to_string(format!("{DRIVER}Shared/Cable.h")).unwrap();
        assert!(h.contains(&format!("#define NF_CABLE_INSTANCE_PREFIX        L\"{INSTANCE_PREFIX}\"")));
        assert!(h.contains(&format!("#define NF_CABLE_LABEL_MAX_CHARS        {LABEL_MAX_CHARS}u")));
        assert!(h.contains(&format!("#define NF_CABLE_ID_CHARS               {ID_CHARS}u")));
        assert!(h.contains(&format!("#define NF_CABLE_INSTANCE_ID_CHARS      {}u", INSTANCE_ID_MAX_CHARS + 1)));
        let inf = std::fs::read(format!("{DRIVER}AudioCodec/Driver/AudioCodec.inf")).unwrap();
        let inf = String::from_utf16_lossy(&crate::setup::devices::wide_of_bytes(&inf[2..]));
        assert!(inf.contains(&format!("StdMfg = \"{PRODUCT}\"")));
        assert!(inf.contains(&format!("AcxLlCable.DeviceDesc = \"{PRODUCT}\"")));
        let device = std::fs::read_to_string(format!("{DRIVER}AudioCodec/Driver/Device.cpp")).unwrap();
        assert!(device.contains(&format!("L\"{PRODUCT} %ws\", Cable->Label")), "the driver's friendly name: Label::friendly_name");
        // The control device's name: the identity's 36 lower-case characters, as CableId shows it
        assert!(device.contains(r#"L"%08lx-%04hx-%04hx-%02x%02x-%02x%02x%02x%02x%02x%02x""#));
    }
}
