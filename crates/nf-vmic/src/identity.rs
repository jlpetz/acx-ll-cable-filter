//! A cable's names (TODO 19, the per-cable design, doc/todo/19-per-cable-design.md, section 4): its identity, a GUID that
//! outlives removal; its label, "D"; and the instance ID the setup tool gives its device node,
//! `ROOT\MEDIA\ACXLLCABLE_<label>_<id>_<n>`, from which the driver reads both at AddDevice (D1's `NfParseCableInstanceId`,
//! Device.cpp). Pure: parsing and formatting only.
//!
//! The parse is the driver's: only what follows the last `\` counts, without case: the prefix `ACXLLCABLE_`, a label of one
//! to eight of A-Z and 0-9, `_`, the GUID's 36 characters (8-4-4-4-12 hex, no braces), `_`, and n in decimal, 1 to
//! 4294967295, with no leading zero and nothing after it. The tool writes the whole ID in upper case; the control device's
//! name has the identity in lower case (`\\.\AcxLlCable_<id>`, nf_vmic.h's NF_VMIC_USER_PREFIX).

use std::fmt;
use std::str::FromStr;

use windows::core::GUID;

/// What every cable's instance ID has after its last `\` (Cable.h, NF_CABLE_INSTANCE_PREFIX).
pub const INSTANCE_PREFIX: &str = "ACXLLCABLE_";
/// Where the setup tool puts every cable's device node: the root enumerator's MEDIA class.
pub const ENUMERATOR: &str = r"ROOT\MEDIA";
/// A label's most characters (Cable.h, NF_CABLE_LABEL_MAX_CHARS).
pub const LABEL_MAX_CHARS: usize = 8;
/// An identity's characters: a GUID without braces (Cable.h, NF_CABLE_ID_CHARS).
pub const ID_CHARS: usize = 36;
/// A device instance ID's most characters, its NUL excluded (MAX_DEVICE_ID_LEN, cfgmgr32.h, is 200 with it).
pub const INSTANCE_ID_MAX_CHARS: usize = 199;
/// The start of each cable's control device's user-mode path (nf_vmic.h, NF_VMIC_USER_PREFIX).
pub const CONTROL_PREFIX: &str = r"\\.\AcxLlCable_";
/// Every cable's device description, and the start of its friendly name (D1's INF and `NfAssignFriendlyName`).
pub const PRODUCT: &str = "ACX-LL-Cable";

/// A cable's identity: a GUID, shown as its 36 lower-case characters without braces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
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

/// A GUID's 36 characters, either case, checked character by character as the driver's NfParseGuid36 does.
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

    /// The cable Install-Driver.ps1 makes first.
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
    /// `ROOT\MEDIA\ACXLLCABLE_D_<ID>_1`, upper case, as the tool creates it.
    pub fn instance_id(&self) -> String {
        format!(r"{ENUMERATOR}\{INSTANCE_PREFIX}{}_{}_{}", self.label, self.id.upper(), self.n)
    }

    /// The same cable's next instance (a variant switch).
    pub fn next(&self) -> Option<Self> {
        Some(Self { n: self.n.checked_add(1)?, ..self.clone() })
    }

    /// The instance ID's parts, as the driver reads them: what follows the last `\`, without case. `None` for anything else
    /// (a node made outside the tool, `ROOT\MEDIA\0003`, which the driver refuses with code 31).
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

/// `\\.\AcxLlCable_<id>`: a cable's control device, built from its identity (lower case, as the driver names it).
pub fn control_path(id: CableId) -> String {
    format!("{CONTROL_PREFIX}{id}")
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
        assert_eq!(CableId::from_guid(id().guid()), id());
        assert_eq!(id().guid(), GUID::from_u128(0x0f1e2d3c_4b5a_6978_8796_a5b4c3d2e1f0));
        for bad in [
            "", "{0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0}", "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f", "0f1e2d3c4b5a-6978-8796-a5b4c3d2e1f0-",
            "0f1e2d3g-4b5a-6978-8796-a5b4c3d2e1f0", " 0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f",
        ] {
            assert!(bad.parse::<CableId>().is_err(), "{bad:?}");
        }
        assert_eq!(control_path(id()), format!(r"\\.\AcxLlCable_{ID}"));
    }

    #[test]
    fn a_label_is_one_to_eight_letters_or_digits() {
        assert_eq!("d".parse::<Label>().unwrap().as_str(), "D");
        assert_eq!("Cab2".parse::<Label>().unwrap().as_str(), "CAB2");
        assert_eq!("ABCDEFGH".parse::<Label>().unwrap().as_str(), "ABCDEFGH");
        for bad in ["", "ABCDEFGHI", "A_B", "A-B", "Ä", " A"] {
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
        // Without case, and only what follows the last backslash (the driver doesn't check ROOT\MEDIA)
        assert_eq!(CableInstance::parse(&text.to_ascii_lowercase()), Some(c.clone()));
        assert_eq!(CableInstance::parse(&format!("acxllcable_d_{ID}_1")), Some(c.clone()));
        assert_eq!(CableInstance::parse(&format!(r"X\Y\ACXLLCABLE_D_{ID}_1")), Some(c.clone()));
        // The largest label and n
        let big = format!(r"ROOT\MEDIA\ACXLLCABLE_ABCDEFGH_{ID}_4294967295");
        assert_eq!(CableInstance::parse(&big).map(|c| (c.label.to_string(), c.n)), Some(("ABCDEFGH".into(), u32::MAX)));
        assert_eq!(c.next().unwrap().n, 2);
        assert_eq!(CableInstance { n: u32::MAX, ..c.clone() }.next(), None);
    }

    #[test]
    fn anything_else_has_no_identity() {
        #[rustfmt::skip]
        let bad = [
            r"ROOT\MEDIA\0003".to_string(),
            format!(r"ROOT\MEDIA\ACXLLCABLE__{ID}_1"),             // no label
            format!(r"ROOT\MEDIA\ACXLLCABLE_ABCDEFGHI_{ID}_1"),    // nine
            format!(r"ROOT\MEDIA\ACXLLCABLE_D-1_{ID}_1"),          // not A-Z0-9
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{{{ID}}}_1"),        // braces
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{ID}"),              // no n
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{ID}_"),
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{ID}_0"),            // n from 1
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{ID}_01"),           // no leading zero
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{ID}_4294967296"),   // too large
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{ID}_1x"),           // nothing after
            format!(r"ROOT\MEDIA\ACXLLCABLE_D_{ID}_1\"),           // the tail is empty
            format!(r"ROOT\MEDIA\ACXLLCABLEX_D_{ID}_1"),
            format!(r"ROOT\MEDIA\ACXLLCABL_D_{ID}_1"),
            String::new(),
        ];
        for b in &bad {
            assert_eq!(CableInstance::parse(b), None, "{b}");
        }
    }

    #[test]
    fn instance_ids_compare_without_case() {
        assert!(same_instance(r"ROOT\MEDIA\ACXLLCABLE_D_X_1", r"root\media\acxllcable_d_x_1"));
        assert!(!same_instance(r"ROOT\MEDIA\0003", r"ROOT\MEDIA\0004"));
    }
}
