//! The driver's three variants (TODO 19, the owner's decision of 2026-11-23), per cable (the per-cable design, sections 8 and
//! 13, S1). One INF, AudioCodec.inf, whose hardware ID picks a cable's microphones, shared by every cable: `ROOT\AcxLlCable`
//! has both ("Microphone (ACX-LL-Cable D)", low latency, IAudioClient3's small periods; and "Call Microphone (ACX-LL-Cable D)",
//! a 5 ms default period for apps that ask for none), `ROOT\AcxLlCableCall` the call microphone only,
//! `ROOT\AcxLlCableLowLatency` the low-latency one only. Every variant has "Speakers (ACX-LL-Cable D)". Windows reads a
//! microphone's default period only when it creates the endpoint (TODO 22), so a switch re-creates the cable's node, which
//! needs Administrator access.
//!
//! # The switch, per cable ([`switch`])
//! 0. Take the setup's lock ([`setup::lock`]), held to the end.
//! 1. Find the switched cable's nodes only: by its identity (the store's for its label, or its nodes' instance IDs:
//!    [`setup::plan::resolve`]). Every other cable is left alone.
//! 2. Already that variant, working, with its endpoints (by its instance ID, each name with the label, as "... (ACX-LL-Cable
//!    D)" or the user's rename of it): nothing. One whose endpoints didn't come, or came without the label, is re-created.
//! 3. Otherwise the new node: the cable's identity and label with the next instance number, n + 1 (`ROOT\MEDIA\
//!    ACXLLCABLE_B_<id>_<n+1>`), so its endpoints are new and Windows reads the variant's default period; the variant's hardware
//!    ID and the friendly name set on the element before registration; and the package the other cables run (the package
//!    rule, [`setup::package::pick`]). A package that doesn't make the variant's microphones, by its INF, stops it here, before
//!    anything is removed.
//! 4. Remove only that cable's old nodes, then register the new one, clear D1's start count, install, wait for it to work, and
//!    set the friendly name again ([`setup`]'s `make`). One that doesn't work on the chosen package is removed again.
//! 5. Record the new n in the store, then up to [`endpoints::ENDPOINTS_WAIT`] for its endpoints on the new instance; an endpoint of the
//!    cable's old instance still active is reported. A migration's cable not finished yet (its record pending) is recorded as
//!    made only once its endpoints are there ([`setup::steps::make_recorded`], as `add` has it). A restart Windows needs, to
//!    finish the new node or to finish removing one that failed, is the answer (exit 7), nothing else done.

use std::fmt::Write as _;

use crate::identity::Label;
use crate::sessions::Mic;
use crate::setup::endpoints::{self, Endpoint, EndpointCheck, Names};
use crate::setup::lock::{LOCK, LOCK_WAIT, SetupLock};
pub use crate::setup::package::INF_NAME;
pub use crate::setup::plan::Health;
use crate::setup::plan::{self, Entry, Node, PlanError, Resolved, VariantPlan};
use crate::setup::steps::{self, Real};
use crate::setup::{self, Done, Present, SetupError, Want};
use crate::win_error::WinError;

/// Cable A's low-latency microphone, by the name we expect Windows to give it.
pub const MICROPHONE: &str = "Microphone (ACX-LL-Cable A)";
/// Cable A's call microphone, by the name we expect Windows to give it.
pub const CALL_MICROPHONE: &str = "Call Microphone (ACX-LL-Cable A)";

/// One of the driver's three variants, by the hardware ID its cable's node has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Variant {
    /// `ROOT\AcxLlCable`: both microphones.
    Both,
    /// `ROOT\AcxLlCableCall`: the call microphone only.
    Call,
    /// `ROOT\AcxLlCableLowLatency`: the low-latency microphone only.
    LowLatency,
}

impl Variant {
    pub const ALL: [Variant; 3] = [Variant::Both, Variant::Call, Variant::LowLatency];

    /// The hardware ID the INF lists for it (D1's, shared by every cable).
    pub fn hardware_id(self) -> &'static str {
        match self {
            Variant::Both => r"ROOT\AcxLlCable",
            Variant::Call => r"ROOT\AcxLlCableCall",
            Variant::LowLatency => r"ROOT\AcxLlCableLowLatency",
        }
    }

    /// The variant a hardware ID names: the whole ID, any case.
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

    /// The microphones it makes, by kind.
    pub fn mics(self) -> &'static [Mic] {
        match self {
            Variant::Both => &[Mic::LowLatency, Mic::Call],
            Variant::Call => &[Mic::Call],
            Variant::LowLatency => &[Mic::LowLatency],
        }
    }

    /// Its microphones' names on cable `label`.
    pub fn microphones(self, label: &Label) -> Vec<String> {
        self.mics().iter().map(|&m| default_name(label, m)).collect()
    }

    /// `"Microphone (ACX-LL-Cable A)" and "Call Microphone (ACX-LL-Cable A)"`.
    pub fn microphones_text(self, label: &Label) -> String {
        quoted(self.microphones(label).iter().map(String::as_str))
    }
}

/// The name Windows gives a microphone of cable `label` (to confirm on the box; the user may rename it).
pub fn default_name(label: &Label, mic: Mic) -> String {
    match mic {
        Mic::LowLatency => format!("Microphone ({})", label.friendly_name()),
        Mic::Call => format!("Call Microphone ({})", label.friendly_name()),
        Mic::Other => label.friendly_name(),
    }
}

/// Names in quotes, joined by "and": `"a" and "b"`.
pub fn quoted<'a>(names: impl Iterator<Item = &'a str>) -> String {
    names.map(|n| format!("\"{n}\"")).collect::<Vec<_>>().join(" and ")
}

/// What [`switch`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The cable was that variant already, working, with its endpoints: nothing changed.
    Nothing,
    /// Switched: the new node works on the chosen package, and Windows has made its endpoints.
    Switched,
    /// Windows asked for a restart to finish installing `device`: it is left for it, its endpoints unchecked.
    RestartPending { device: String },
}

/// One cable as `mic variant` shows it (any user): its nodes, and its endpoints (or why they couldn't be read).
#[derive(Clone, Debug)]
pub struct Shown {
    pub label: Label,
    /// Its identity and nodes, if the store or a node knows the label.
    pub resolved: Option<Resolved>,
    pub nodes: Vec<Node>,
    pub endpoints: Result<Vec<Endpoint>, WinError>,
    /// The old driver's nodes, which wait for the migration.
    pub legacy: Vec<Node>,
}

/// Reads cable `label`: its nodes by identity, and the endpoints. Any user may call it.
pub fn present(label: &Label) -> Result<Shown, SetupError> {
    let r = setup::read_report()?;
    let resolved = plan::resolve(label, &r.store.entries, &r.nodes)?;
    let nodes = resolved.as_ref().map(|res| res.nodes.iter().map(|&i| r.nodes[i].clone()).collect()).unwrap_or_default();
    let legacy = r.nodes.iter().filter(|n| n.is_legacy()).cloned().collect();
    Ok(Shown { label: label.clone(), resolved, nodes, endpoints: r.endpoints, legacy })
}

/// What a switch to `target` would find of the cable, before any prompt (pure; the elevated switch decides again).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Precheck {
    /// That variant already, working, with its endpoints: what to say.
    Nothing(Vec<String>),
    /// Something to do; `note` says why, if it isn't plain.
    Switch(Option<String>),
    /// It can't be done (the cable isn't present, the old driver's devices wait for the migration, a setup error).
    Refused(PlanError),
    /// The endpoints couldn't be read here: the elevated switch reads them again.
    Unchecked(WinError),
}

/// The not-elevated side's look at a switch of cable `label` to `target` over `nodes` (every node of ours) and `store`.
pub fn precheck(label: &Label, target: Variant, store: &[Entry], nodes: &[Node], now: &Result<Vec<Endpoint>, WinError>) -> Precheck {
    match plan::plan_variant(label, target, false, store, nodes) {
        Err(e) => Precheck::Refused(e),
        Ok(VariantPlan::Switch(_)) => Precheck::Switch(None),
        Ok(VariantPlan::Keep { node }) => {
            let Some(cable) = &nodes[node].cable else { return Precheck::Switch(None) };
            match now {
                Err(e) => Precheck::Unchecked(e.clone()),
                Ok(eps) => {
                    let check = endpoints::check(cable, target, eps, Names::Any);
                    if check.is_done() {
                        let mut lines =
                            vec![format!("{} is {} already, working: nothing to do.", label.friendly_name(), target.describe())];
                        lines.extend(found_lines(&check));
                        Precheck::Nothing(lines)
                    } else {
                        Precheck::Switch(Some(format!(
                            "{} is {} already, but {check}: the switch re-creates it.",
                            label.friendly_name(),
                            target.describe()
                        )))
                    }
                }
            }
        }
    }
}

/// A check's endpoints, a line each (`  Call Microphone: "Call Microphone (ACX-LL-Cable A)"`), then a warning for each
/// disabled in the Sound panel.
pub fn found_lines(check: &EndpointCheck) -> Vec<String> {
    let mut lines: Vec<String> = check
        .found
        .iter()
        .map(|(kind, e)| {
            let off = if e.state == endpoints::State::Disabled { ", disabled in the Sound panel" } else { "" };
            format!("  {}: \"{}\"{off}", kind.name(), e.name)
        })
        .collect();
    lines.extend(
        check
            .found
            .iter()
            .filter(|(_, e)| e.state == endpoints::State::Disabled)
            .map(|(_, e)| format!("  warning: {}", disabled_note(&e.name))),
    );
    lines
}

/// What to do about an endpoint disabled in the Sound panel.
pub fn disabled_note(name: &str) -> String {
    format!(
        "\"{name}\" is disabled in the Sound panel, so apps don't see it: enable it there (mmsys.cpl: right-click it, Enable; \
         right-click the list, Show Disabled Devices, if it isn't listed)"
    )
}

/// `mic variant`'s view of one cable (pure).
pub fn format_shown(s: &Shown) -> String {
    let mut out = String::new();
    let name = s.label.friendly_name();
    if !s.legacy.is_empty() {
        let _ = writeln!(out, "The old driver's devices are present: migrate first (driver\\scripts\\Install-Driver.ps1).");
        for n in &s.legacy {
            let _ = writeln!(out, "  {}", n.describe());
        }
    }
    match s.nodes.as_slice() {
        [] => {
            let _ = writeln!(out, "{name} isn't present (`acx-ll-cable add {}` makes it).", s.label);
        }
        nodes => {
            for n in nodes {
                let v = n.variant().map_or("?", Variant::arg);
                let _ = writeln!(out, "{name}: variant {v}");
                let _ = writeln!(out, "  {}", n.describe());
                if let (Some(cable), Some(variant), Ok(eps)) = (&n.cable, n.variant(), &s.endpoints) {
                    let check = endpoints::check(cable, variant, eps, Names::Any);
                    for line in found_lines(&check) {
                        let _ = writeln!(out, "{line}");
                    }
                    if !check.is_done() && n.health == Health::Working {
                        let _ = writeln!(out, "  Missing or misnamed: {check}");
                    }
                }
            }
        }
    }
    if let Err(e) = &s.endpoints {
        let _ = writeln!(out, "(The endpoints couldn't be read: {e})");
    }
    let _ = writeln!(
        out,
        "Switch with `noise-filter mic variant both|call|low-latency`: both microphones, the call microphone only (a 5 ms default \
         period) or the low-latency one only. Windows asks for permission, as the switch re-creates the cable's device, with new \
         endpoints."
    );
    out
}

/// Switches cable `label` to `target` (the module's docs), reporting each step through `say`. Needs elevation: without it, the
/// lock refuses the caller ([`setup::lock::LockError::Denied`]).
pub fn switch(label: &Label, target: Variant, say: &mut dyn FnMut(&str)) -> Result<Outcome, SetupError> {
    let _lock = SetupLock::take(&LOCK, LOCK_WAIT, say)?;
    say(&format!("Looking for {} (its identity's nodes only)", label.friendly_name()));
    let present = Present::read()?;
    let entries = setup::store::read().map_err(SetupError::Store)?.entries;
    let mut plan = plan::plan_variant(label, target, false, &entries, &present.nodes)?;
    if let VariantPlan::Keep { node } = plan {
        let n = &present.nodes[node];
        say(&format!("  {}", n.describe()));
        let cable = n.cable.clone().expect("a kept node is a cable's");
        let now = endpoints::all().map_err(SetupError::Endpoints)?;
        let check = endpoints::check(&cable, target, &now, Names::Any);
        if check.is_done() {
            say(&format!("{} is {} already, working: nothing to do.", label.friendly_name(), target.describe()));
            for line in found_lines(&check) {
                say(&line);
            }
            return Ok(Outcome::Nothing);
        }
        say(&format!("  {check}: re-creating it"));
        plan = plan::plan_variant(label, target, true, &entries, &present.nodes)?;
    }
    let VariantPlan::Switch(create) = plan else { unreachable!("re-planned to switch") };
    let old: Vec<String> = create.remove.iter().map(|&i| present.nodes[i].instance_id.clone()).collect();
    for &i in &create.remove {
        say(&format!("  {}", present.nodes[i].describe()));
    }
    let running = present.running();
    // n + 1: endpoints Windows makes now, so every name the expected one. A make that fails is the answer as it is (a restart
    // Windows needs among them: exit 7); a migration's cable stays pending until its endpoints are there
    let rec = setup::recording_of(&entries, label);
    let (made, r) = steps::make_recorded(&mut Real { present: &present }, &create, Want::Rule(&running), rec, Names::Since(&[]), say);
    if let Ok(Done::RestartPending { device }) = r {
        say(&format!("  Windows needs a restart to finish installing {device}: its endpoints come after it."));
        return Ok(Outcome::RestartPending { device });
    }
    if made.is_some()
        && let Ok(now) = endpoints::all()
    {
        for e in endpoints::still_active(&old, &now) {
            say(&format!("  also still active: \"{}\", the cable's old instance's (going)", e.name));
        }
    }
    r.map(|_| Outcome::Switched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cables::{CALL_MICROPHONE_FILTER, EndpointKind, MICROPHONE_FILTER};
    use crate::identity::CableInstance;
    use crate::setup::plan::tests::{A_ID, B_ID, label, node};

    fn ep(c: &CableInstance, kind: EndpointKind, state: endpoints::State) -> Endpoint {
        Endpoint {
            id: format!("{kind:?}"),
            name: endpoints::expected_name(c, kind),
            kind,
            state,
            instance_id: c.instance_id().to_lowercase(),
        }
    }

    #[test]
    fn each_variant_has_its_hardware_id_and_word() {
        assert_eq!(Variant::Both.hardware_id(), r"ROOT\AcxLlCable");
        assert_eq!(Variant::Call.hardware_id(), r"ROOT\AcxLlCableCall");
        assert_eq!(Variant::LowLatency.hardware_id(), r"ROOT\AcxLlCableLowLatency");
        for v in Variant::ALL {
            assert_eq!(Variant::from_hardware_id(v.hardware_id()), Some(v));
            assert_eq!(Variant::from_arg(v.arg()), Some(v));
            assert_eq!(Variant::from_hardware_id(&v.hardware_id().to_uppercase()), Some(v));
        }
        assert_eq!(Variant::from_hardware_id(r"ROOT\AcxLlCableCallX"), None);
        assert_eq!(Variant::from_hardware_id(r"ROOT\AudioCodec"), None, "the old driver's: the migration's");
        assert_eq!(Variant::from_arg("Low-Latency"), Some(Variant::LowLatency));
        assert_eq!(Variant::from_arg("lowlatency"), None);
        let b = label("B");
        assert_eq!(Variant::Both.microphones(&b), ["Microphone (ACX-LL-Cable B)", "Call Microphone (ACX-LL-Cable B)"]);
        assert_eq!(Variant::Call.microphones_text(&b), "\"Call Microphone (ACX-LL-Cable B)\"");
        assert_eq!(Variant::Both.microphones(&Label::a()), [MICROPHONE, CALL_MICROPHONE]);
    }

    const DRIVER: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../driver/src/");

    /// The repo's INF, as text.
    fn inf_text() -> String {
        let inf = std::fs::read(format!("{DRIVER}AudioCodec/Driver/AudioCodec.inf")).unwrap();
        assert_eq!(inf[..2], [0xFF, 0xFE], "UTF-16LE, with a BOM");
        String::from_utf16_lossy(&crate::setup::devices::wide_of_bytes(&inf[2..]))
    }

    /// The INF and the driver agree on the microphones' filters and the hardware IDs (D1 and S1 in one tree since step 1's
    /// merge, so nothing here is skipped).
    #[test]
    fn the_microphones_filters_are_the_drivers() {
        let inf = inf_text();
        assert!(inf.contains(&format!("KSNAME_Microphone=\"{MICROPHONE_FILTER}\"")));
        assert!(inf.contains(&format!("KSNAME_CallMicrophone=\"{CALL_MICROPHONE_FILTER}\"")));
        assert!(inf.contains(&format!("KSNAME_Speaker=\"{}\"", crate::cables::SPEAKERS_FILTER)));
        let settings = std::fs::read_to_string(format!("{DRIVER}AudioCodec/Driver/DriverSettings.h")).unwrap();
        assert!(settings.contains(&format!("captureCircuitName, L\"{MICROPHONE_FILTER}\")")));
        assert!(settings.contains(&format!("capture2CircuitName, L\"{CALL_MICROPHONE_FILTER}\")")));
        // What AddDevice reads of the hardware key, which the INF's .HW AddReg writes
        let device = std::fs::read_to_string(format!("{DRIVER}AudioCodec/Driver/Device.cpp")).unwrap();
        assert!(device.contains("DECLARE_CONST_UNICODE_STRING(valueName, L\"NfMicrophones\")"));
        // D1's INF (the per-cable driver): each hardware ID's install section and NfMicrophones, and none of the old IDs
        for (v, section, mics) in [
            (Variant::Both, "Audio_Device", 3),
            (Variant::Call, "Audio_Device_Call", 2),
            (Variant::LowLatency, "Audio_Device_LowLatency", 1),
        ] {
            assert!(inf.contains(&format!("%AcxLlCable.DeviceDesc%={section}, {}", v.hardware_id())), "{v:?}");
            assert!(inf.contains(&format!("[{section}.HW.AddReg]\r\nHKR,,NfMicrophones,0x00010001,{mics}")), "{v:?}");
        }
        for old in crate::setup::plan::Legacy::ALL {
            assert!(!inf.contains(&format!("{}\r", old.hardware_id())) && !inf.contains(&format!("{},", old.hardware_id())), "{old:?}");
        }
    }

    /// The driver scripts' hardware IDs (common.ps1, which Install-Driver.ps1 decides between the migration, update and add
    /// by) are the setup's: the three variants', and the old driver's that the migration removes.
    #[test]
    fn the_scripts_hardware_ids_are_the_setups() {
        let common = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../driver/scripts/common.ps1")).unwrap();
        for v in Variant::ALL {
            assert!(common.contains(&format!("'{}'", v.hardware_id())), "{v:?}");
            assert!(common.contains(&format!("'{}'", v.arg())), "{v:?}");
        }
        let legacy: Vec<String> = crate::setup::plan::Legacy::ALL.iter().map(|l| format!("'{}'", l.hardware_id())).collect();
        assert!(common.contains(&format!("$script:LegacyHardwareIds = @({})", legacy.join(", "))), "{legacy:?}");
    }

    /// D1's Cable.h names what the setup mirrors: the instance ID's prefix, the start count's value, its limit and the status.
    #[test]
    fn the_setup_mirrors_cable_h() {
        let h = std::fs::read_to_string(format!("{DRIVER}Shared/Cable.h")).expect("D1's Cable.h, beside its INF");
        assert!(h.contains(&format!("#define NF_CABLE_INSTANCE_PREFIX        L\"{}\"", crate::identity::INSTANCE_PREFIX)));
        assert!(h.contains(&format!("#define NF_CABLE_START_ATTEMPTS_VALUE   L\"{}\"", crate::setup::devices::START_ATTEMPTS_VALUE)));
        assert!(h.contains(&format!("#define NF_CABLE_START_ATTEMPTS_LIMIT   {}u", plan::START_ATTEMPTS_LIMIT)));
        assert!(h.contains(&format!("#define NF_STATUS_START_BREAKER         ((NTSTATUS)0x{:08X}L)", plan::STATUS_START_BREAKER)));
        assert!(h.contains(&format!("#define NF_CABLE_LABEL_MAX_CHARS        {}u", crate::identity::LABEL_MAX_CHARS)));
    }

    /// The precheck with two cables present: B's switch looks at B alone.
    #[test]
    fn the_precheck_looks_at_the_switched_cable_only() {
        let a = node("A", A_ID, 1, Variant::Both);
        let b = node("B", B_ID, 1, Variant::LowLatency);
        let (ac, bc) = (a.cable.clone().unwrap(), b.cable.clone().unwrap());
        let nodes = vec![a, b];
        let mut eps: Vec<Endpoint> =
            EndpointKind::of_variant(Variant::Both).into_iter().map(|k| ep(&ac, k, endpoints::State::Active)).collect();
        // B's endpoints not there yet: a switch to its own variant re-creates it
        let Precheck::Switch(Some(note)) = precheck(&label("B"), Variant::LowLatency, &[], &nodes, &Ok(eps.clone())) else { panic!() };
        assert!(
            note.starts_with("ACX-LL-Cable B is the low-latency microphone only already, but Windows hasn't made Speakers, Microphone"),
            "{note}"
        );
        eps.extend(EndpointKind::of_variant(Variant::LowLatency).into_iter().map(|k| ep(&bc, k, endpoints::State::Active)));
        let Precheck::Nothing(lines) = precheck(&label("B"), Variant::LowLatency, &[], &nodes, &Ok(eps.clone())) else { panic!() };
        assert_eq!(lines, [
            "ACX-LL-Cable B is the low-latency microphone only already, working: nothing to do.",
            "  Speakers: \"Speakers (ACX-LL-Cable B)\"",
            "  Microphone: \"Microphone (ACX-LL-Cable B)\"",
        ]);
        assert_eq!(precheck(&label("B"), Variant::Call, &[], &nodes, &Ok(eps.clone())), Precheck::Switch(None));
        assert_eq!(precheck(&label("C"), Variant::Call, &[], &nodes, &Ok(eps)), Precheck::Refused(PlanError::NotPresent(label("C"))));
        let unread = WinError { call: "IMMDeviceEnumerator::EnumAudioEndpoints", code: 5 };
        assert_eq!(precheck(&label("A"), Variant::Both, &[], &nodes, &Err(unread.clone())), Precheck::Unchecked(unread));
        // B disabled on purpose (`acx-ll-cable disable`): refused before any prompt, so the app's switch doesn't undo the disable
        let mut off = nodes.clone();
        off[1].health = Health::Problem(plan::PROBLEM_DISABLED);
        off[1].config_flags = Some(windows::Win32::Devices::DeviceAndDriverInstallation::CONFIGFLAG_DISABLED.0);
        assert_eq!(precheck(&label("B"), Variant::Call, &[], &off, &Ok(vec![])), Precheck::Refused(PlanError::Disabled(label("B"))));
    }

    #[test]
    fn a_cable_shows_its_variant_and_endpoints() {
        let b = node("B", B_ID, 2, Variant::Call);
        let bc = b.cable.clone().unwrap();
        let mut eps: Vec<Endpoint> =
            EndpointKind::of_variant(Variant::Call).into_iter().map(|k| ep(&bc, k, endpoints::State::Active)).collect();
        eps[1].state = endpoints::State::Disabled;
        let shown = Shown { label: label("B"), resolved: None, nodes: vec![b], endpoints: Ok(eps), legacy: vec![] };
        let text = format_shown(&shown);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "ACX-LL-Cable B: variant call");
        assert!(lines[1].contains(r"the call microphone only (ROOT\AcxLlCableCall), working"), "{}", lines[1]);
        assert_eq!(lines[2], "  Speakers: \"Speakers (ACX-LL-Cable B)\"");
        assert_eq!(lines[3], "  Call Microphone: \"Call Microphone (ACX-LL-Cable B)\", disabled in the Sound panel");
        assert!(lines[4].starts_with("  warning: \"Call Microphone (ACX-LL-Cable B)\" is disabled in the Sound panel"), "{}", lines[4]);
        assert!(text.contains("Switch with `noise-filter mic variant both|call|low-latency`"));
        let none = Shown { label: label("C"), resolved: None, nodes: vec![], endpoints: Ok(vec![]), legacy: vec![] };
        assert!(format_shown(&none).starts_with("ACX-LL-Cable C isn't present (`acx-ll-cable add C` makes it)."));
    }
}
