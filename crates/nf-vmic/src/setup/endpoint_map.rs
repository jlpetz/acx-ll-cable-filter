//! The endpoint map (TODO 19, the per-cable design, section 9; Astra's review, forwarded by the owner, 2026-11-25): which of a
//! cable's endpoints takes each old endpoint's place when `migrate` or `add` makes the cable in place of other device nodes, so
//! a harness can record the migration as a device change and the app's own saved settings can be remapped where the match is
//! unambiguous. `acx-ll-cable migrate|add ... --endpoint-map <file>` writes it, as JSON (UTF-8, no BOM).
//!
//! **Old and new.** A cable's old endpoints are those in the look before the run (NOTPRESENT ones too) on the nodes it replaces:
//! the old driver's nodes the migration makes it in place of (`ROOT\MEDIA\0003` for A, Cable-B.ps1's for B; recorded in the
//! store while pending, so a run after their removal still finds their lingering endpoints), a node `add --adopt` re-creates,
//! and every earlier instance of its identity (a re-add's, a repair's: the same node). Its new endpoints are those on its
//! instance after the run. Each old endpoint is matched to the new one of the same role (Speakers, render; Microphone and Call
//! Microphone, capture): the old driver's circuits have the cable's reference strings (Speaker0, Microphone0, Microphone1).
//!
//! **`match`:**
//! - `same`: Windows kept the endpoint (the same MMDevice id): nothing to remap.
//! - `moved`: one new endpoint of that role, with another id: remap the old id to it.
//! - `gone`: the cable has no endpoint of that role after the run (its microphones don't include it, or Windows hadn't made it
//!   when the run ended: a restart it needs, a failure). Not remapped.
//! - `ambiguous`: more than one endpoint of that role on the new instance, none with the old id. Not remapped.
//! - `added`: a new endpoint no old one maps to (`old` is null).
//!
//! The file is replaced at the start of the run (deleted, so a file there afterwards is this run's) and written at its end once
//! the run has read the endpoints before it; a run that stops earlier (its error says why, nothing was changed) leaves none.

use std::path::Path;

use serde::Serialize;

use super::endpoints::{Endpoint, State};
use super::{Done, SetupError};
use crate::cables::EndpointKind;
use crate::identity::{CableId, CableInstance, same_instance};
use crate::variant::Variant;

/// The file's `format`.
pub const FORMAT: &str = "acx-ll-cable endpoint map";
/// The file's `version`: a reader refuses one it doesn't know.
pub const VERSION: u32 = 1;

/// Where a cable's old endpoints were.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Old {
    /// A node by its instance ID: an old driver's node the migration replaces, or a node `add --adopt` re-creates.
    Instance(String),
    /// Any instance of the cable's identity: an earlier n, or the same one (a re-add with its instance ID, a repair).
    Identity(CableId),
}

impl Old {
    fn holds(&self, e: &Endpoint) -> bool {
        match self {
            Old::Instance(id) => same_instance(id, &e.instance_id),
            Old::Identity(id) => CableInstance::parse(&e.instance_id).is_some_and(|c| c.id == *id),
        }
    }
}

/// A cable a run made, finished or repaired, and where its endpoints were before.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Made {
    pub cable: CableInstance,
    pub variant: Variant,
    pub old: Vec<Old>,
}

impl Made {
    /// A cable made in place of the nodes `replaces` (instance IDs), and of its own identity's earlier instances.
    pub fn new(cable: CableInstance, variant: Variant, replaces: &[String]) -> Self {
        let mut old: Vec<Old> = replaces.iter().cloned().map(Old::Instance).collect();
        old.push(Old::Identity(cable.id));
        Self { cable, variant, old }
    }
}

/// What a run gathers for its map: the endpoints before it (`None`: not read), and the cables it made.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Gathered {
    pub before: Option<Vec<Endpoint>>,
    pub cables: Vec<Made>,
}

/// The file.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EndpointMap {
    pub format: &'static str,
    pub version: u32,
    /// `migrate` or `add`.
    pub command: &'static str,
    /// How the run ended: `done`, `nothing` (nothing needed doing), `restart` (Windows needs one to finish: the new endpoints may
    /// not be there yet), `error` (its message says what changed).
    pub result: &'static str,
    pub cables: Vec<CableMap>,
}

/// One cable's map.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CableMap {
    pub label: String,
    /// The identity, 36 lower-case characters.
    pub id: String,
    /// Its node after the run: `ROOT\MEDIA\ACXLLCABLE_<label>_<ID>_<n>`.
    pub instance_id: String,
    /// `both`, `call` or `low-latency`.
    pub mics: &'static str,
    /// The other nodes it took the place of (instance IDs): the old driver's, or an adopted one.
    pub replaces: Vec<String>,
    pub endpoints: Vec<Pair>,
}

/// An old endpoint and its new one, by role.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Pair {
    /// `Speakers`, `Microphone` or `Call Microphone`.
    pub role: &'static str,
    /// `render` or `capture`.
    pub flow: &'static str,
    pub old: Option<Side>,
    pub new: Option<Side>,
    #[serde(rename = "match")]
    pub matched: Match,
}

/// One endpoint, as Windows had it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Side {
    /// The MMDevice id, `{0.0.1.00000000}.{...}`: what the app and other apps save.
    pub id: String,
    pub name: String,
    /// The node it's on, as its KS filter path has it (lower case, usually).
    pub instance_id: String,
    /// `active`, `disabled`, `unplugged` or `notpresent`.
    pub state: &'static str,
}

/// How an old endpoint maps (the module's docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Match {
    Same,
    Moved,
    Gone,
    Ambiguous,
    Added,
}

impl Match {
    /// Whether a saved old id may be replaced by the new one.
    pub fn remaps(self) -> bool {
        matches!(self, Match::Same | Match::Moved)
    }
}

fn side(e: &Endpoint) -> Side {
    let state = match e.state {
        State::Active => "active",
        State::Disabled => "disabled",
        State::Unplugged => "unplugged",
        State::NotPresent => "notpresent",
    };
    Side { id: e.id.clone(), name: e.name.clone(), instance_id: e.instance_id.clone(), state }
}

/// One cable's map from the endpoints before and after the run (pure).
pub fn map_cable(made: &Made, before: &[Endpoint], after: &[Endpoint]) -> CableMap {
    let new_id = made.cable.instance_id();
    let news: Vec<&Endpoint> = after.iter().filter(|e| e.state != State::NotPresent && same_instance(&e.instance_id, &new_id)).collect();
    let mut olds: Vec<&Endpoint> = Vec::new();
    for e in before.iter().filter(|e| made.old.iter().any(|o| o.holds(e))) {
        if !olds.iter().any(|o| o.id == e.id) {
            olds.push(e);
        }
    }
    let mut pairs = Vec::new();
    let mut used: Vec<&str> = Vec::new();
    let kinds = [EndpointKind::Speakers, EndpointKind::Microphone, EndpointKind::CallMicrophone];
    for kind in kinds {
        let role = |old: Option<Side>, new: Option<Side>, matched| Pair {
            role: kind.name(),
            flow: if kind.is_render() { "render" } else { "capture" },
            old,
            new,
            matched,
        };
        let candidates: Vec<&&Endpoint> = news.iter().filter(|e| e.kind == kind).collect();
        for old in olds.iter().filter(|e| e.kind == kind) {
            let kept = candidates.iter().find(|n| n.id == old.id);
            let (new, matched) = match (kept, candidates.as_slice()) {
                (Some(n), _) => (Some(**n), Match::Same),
                (None, [only]) => (Some(**only), Match::Moved),
                (None, []) => (None, Match::Gone),
                (None, _) => (None, Match::Ambiguous),
            };
            if let Some(n) = new {
                used.push(&n.id);
            }
            pairs.push(role(Some(side(old)), new.map(side), matched));
        }
        for n in candidates.iter().filter(|n| !used.contains(&n.id.as_str())) {
            pairs.push(role(None, Some(side(n)), Match::Added));
        }
    }
    let replaces = made
        .old
        .iter()
        .filter_map(|o| match o {
            Old::Instance(id) => Some(id.clone()),
            Old::Identity(_) => None,
        })
        .collect();
    CableMap {
        label: made.cable.label.to_string(),
        id: made.cable.id.to_string(),
        instance_id: new_id,
        mics: made.variant.arg(),
        replaces,
        endpoints: pairs,
    }
}

/// The whole map (pure).
pub fn map(
    command: &'static str, result: &Result<Done, SetupError>, cables: &[Made], before: &[Endpoint], after: &[Endpoint],
) -> EndpointMap {
    let result = match result {
        Ok(Done::Changed) => "done",
        Ok(Done::Nothing) => "nothing",
        Ok(Done::RestartPending { .. }) => "restart",
        Err(_) => "error",
    };
    let cables = cables.iter().map(|m| map_cable(m, before, after)).collect();
    EndpointMap { format: FORMAT, version: VERSION, command, result, cables }
}

/// The map's file at the start of a run: an old one deleted, so a file there afterwards is this run's.
pub fn start(path: Option<&Path>) -> Result<(), SetupError> {
    let Some(path) = path else { return Ok(()) };
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(SetupError::EndpointMap(format!("{} is there and couldn't be deleted ({e}): nothing was changed", path.display()))),
    }
}

/// The map written to `path` at the end of a run (`gathered`, `result`), the endpoints after it read by `after`; said. A map
/// that can't be made or written turns a run that was done into [`SetupError::EndpointMap`]; any other result stands, the
/// failure said beside it.
pub fn finish(
    path: Option<&Path>, command: &'static str, gathered: Gathered, result: Result<Done, SetupError>,
    after: &mut dyn FnMut() -> Result<Vec<Endpoint>, crate::win_error::WinError>, say: &mut dyn FnMut(&str),
) -> Result<Done, SetupError> {
    let Some(path) = path else { return result };
    let mut failed = |why: String, result: Result<Done, SetupError>| match result {
        Ok(Done::Changed | Done::Nothing) => Err(SetupError::EndpointMap(format!("done, but {why}"))),
        other => {
            say(&format!("  warning: {why}"));
            other
        }
    };
    let Some(before) = gathered.before else {
        return failed(format!("no endpoint map was written to {}: the endpoints before the run weren't read", path.display()), result);
    };
    let after = match after() {
        Ok(a) => a,
        Err(e) => {
            return failed(
                format!("no endpoint map was written to {}: the endpoints after the run couldn't be read ({e})", path.display()),
                result,
            );
        }
    };
    let m = map(command, &result, &gathered.cables, &before, &after);
    let text = match serde_json::to_string_pretty(&m) {
        Ok(t) => t,
        Err(e) => return failed(format!("the endpoint map couldn't be made ({e})"), result),
    };
    if let Err(e) = std::fs::write(path, text + "\n") {
        return failed(format!("the endpoint map couldn't be written to {} ({e})", path.display()), result);
    }
    let remaps = m.cables.iter().flat_map(|c| &c.endpoints).filter(|p| p.old.is_some() && p.matched.remaps()).count();
    say(&format!("The endpoint map: {} ({} cable(s), {remaps} old endpoint(s) with a new one)", path.display(), m.cables.len()));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::plan::tests::{A_ID, B_ID, cid, label};

    fn cable(l: &str, id: &str, n: u32) -> CableInstance {
        CableInstance { label: label(l), id: cid(id), n }
    }

    fn ep(instance: &str, kind: EndpointKind, id: &str, name: &str, state: State) -> Endpoint {
        Endpoint { id: id.into(), name: name.into(), kind, state, instance_id: instance.to_lowercase() }
    }

    /// The migration: the old A (both microphones) and Cable-B.ps1's B (Speakers and Microphone) to A and B, both with both.
    #[test]
    fn the_migrations_old_endpoints_map_by_role() {
        let a = cable("A", A_ID, 1);
        let b = cable("B", B_ID, 1);
        let (ai, bi) = (a.instance_id(), b.instance_id());
        let before = vec![
            ep(r"ROOT\MEDIA\0003", EndpointKind::Speakers, "{0}.{a-spk}", "Speakers (AudioCodec Device)", State::Active),
            ep(r"ROOT\MEDIA\0003", EndpointKind::Microphone, "{1}.{a-mic}", "Microphone (AudioCodec Device)", State::Active),
            ep(r"ROOT\MEDIA\0003", EndpointKind::CallMicrophone, "{1}.{a-call}", "Call Microphone (AudioCodec Device)", State::Active),
            ep(r"ROOT\MEDIA\0004", EndpointKind::Speakers, "{0}.{b-spk}", "Speakers (Noise Filter Cable B)", State::NotPresent),
            ep(r"ROOT\MEDIA\0004", EndpointKind::Microphone, "{1}.{b-mic}", "Microphone (Noise Filter Cable B)", State::NotPresent),
        ];
        let after = vec![
            ep(&ai, EndpointKind::Speakers, "{0}.{A-spk}", "Speakers (ACX-LL-Cable A)", State::Active),
            ep(&ai, EndpointKind::Microphone, "{1}.{A-mic}", "Microphone (ACX-LL-Cable A)", State::Active),
            ep(&ai, EndpointKind::CallMicrophone, "{1}.{A-call}", "Call Microphone (ACX-LL-Cable A)", State::Active),
            ep(&bi, EndpointKind::Speakers, "{0}.{B-spk}", "Speakers (ACX-LL-Cable B)", State::Active),
            ep(&bi, EndpointKind::Microphone, "{1}.{B-mic}", "Microphone (ACX-LL-Cable B)", State::Active),
            ep(&bi, EndpointKind::CallMicrophone, "{1}.{B-call}", "Call Microphone (ACX-LL-Cable B)", State::Active),
        ];
        let made = [
            Made::new(a.clone(), Variant::Both, &[r"ROOT\MEDIA\0003".into()]),
            Made::new(b.clone(), Variant::Both, &[r"ROOT\MEDIA\0004".into()]),
        ];
        let m = map("migrate", &Ok(Done::Changed), &made, &before, &after);
        assert_eq!((m.format, m.version, m.command, m.result), (FORMAT, 1, "migrate", "done"));
        fn pairs(c: &CableMap) -> Vec<(&'static str, Option<&str>, Option<&str>, Match)> {
            c.endpoints
                .iter()
                .map(|p| (p.role, p.old.as_ref().map(|s| s.id.as_str()), p.new.as_ref().map(|s| s.id.as_str()), p.matched))
                .collect()
        }
        assert_eq!(pairs(&m.cables[0]), [
            ("Speakers", Some("{0}.{a-spk}"), Some("{0}.{A-spk}"), Match::Moved),
            ("Microphone", Some("{1}.{a-mic}"), Some("{1}.{A-mic}"), Match::Moved),
            ("Call Microphone", Some("{1}.{a-call}"), Some("{1}.{A-call}"), Match::Moved),
        ]);
        // B's old endpoints (lingering, its node removed by an earlier run) map; its new Call Microphone has no old one
        assert_eq!(pairs(&m.cables[1]), [
            ("Speakers", Some("{0}.{b-spk}"), Some("{0}.{B-spk}"), Match::Moved),
            ("Microphone", Some("{1}.{b-mic}"), Some("{1}.{B-mic}"), Match::Moved),
            ("Call Microphone", None, Some("{1}.{B-call}"), Match::Added),
        ]);
        let c = &m.cables[1];
        assert_eq!((c.label.as_str(), c.id.as_str(), c.instance_id.as_str(), c.mics), ("B", B_ID, bi.as_str(), "both"));
        assert_eq!(c.replaces, [r"ROOT\MEDIA\0004"]);
        assert_eq!(c.endpoints[0].old.as_ref().unwrap().state, "notpresent");
        assert_eq!((c.endpoints[0].flow, c.endpoints[1].flow), ("render", "capture"));
        // As JSON: the field names a reader takes
        let text = serde_json::to_string(&m).unwrap();
        for field in [
            r#""format":"acx-ll-cable endpoint map""#,
            r#""version":1"#,
            r#""match":"moved""#,
            r#""match":"added""#,
            r#""old":null"#,
            r#""role":"Call Microphone""#,
            r#""flow":"render""#,
            r#""mics":"both""#,
        ] {
            assert!(text.contains(field), "{field}: {text}");
        }
    }

    /// A re-add with its instance ID: the endpoints Windows kept are `same`; one it made anew is `moved`. A re-add with other
    /// microphones (n + 1): the old Call Microphone has no new one.
    #[test]
    fn a_re_adds_endpoints_are_kept_moved_or_gone() {
        let b1 = cable("B", B_ID, 1);
        let i1 = b1.instance_id();
        let before = vec![
            ep(&i1, EndpointKind::Speakers, "{0}.{spk}", "Studio out (ACX-LL-Cable B)", State::NotPresent),
            ep(&i1, EndpointKind::CallMicrophone, "{1}.{call}", "Call Microphone (ACX-LL-Cable B)", State::NotPresent),
            // Another cable's, never B's
            ep(&cable("A", A_ID, 1).instance_id(), EndpointKind::Speakers, "{0}.{a}", "Speakers (ACX-LL-Cable A)", State::Active),
        ];
        let kept = vec![
            ep(&i1, EndpointKind::Speakers, "{0}.{spk}", "Studio out (ACX-LL-Cable B)", State::Active),
            ep(&i1, EndpointKind::CallMicrophone, "{1}.{call2}", "Call Microphone (ACX-LL-Cable B)", State::Active),
        ];
        let made = Made::new(b1.clone(), Variant::Call, &[]);
        let c = map_cable(&made, &before, &kept);
        let got: Vec<(Option<&str>, Match)> = c.endpoints.iter().map(|p| (p.new.as_ref().map(|s| s.id.as_str()), p.matched)).collect();
        assert_eq!(got, [(Some("{0}.{spk}"), Match::Same), (Some("{1}.{call2}"), Match::Moved)]);
        assert!(c.replaces.is_empty());
        // n + 1 with the low-latency microphone: the old instance's endpoints map to the new instance's
        let b2 = cable("B", B_ID, 2);
        let after = vec![
            ep(&b2.instance_id(), EndpointKind::Speakers, "{0}.{spk2}", "Speakers (ACX-LL-Cable B)", State::Active),
            ep(&b2.instance_id(), EndpointKind::Microphone, "{1}.{mic2}", "Microphone (ACX-LL-Cable B)", State::Active),
        ];
        let c = map_cable(&Made::new(b2, Variant::LowLatency, &[]), &before, &after);
        let got: Vec<(&str, Match)> = c.endpoints.iter().map(|p| (p.role, p.matched)).collect();
        assert_eq!(got, [("Speakers", Match::Moved), ("Microphone", Match::Added), ("Call Microphone", Match::Gone)]);
        assert!(!Match::Gone.remaps() && Match::Moved.remaps() && Match::Same.remaps() && !Match::Ambiguous.remaps());
        // Two new endpoints of one role, neither with the old id: not remapped
        let mut two = after.clone();
        two.push(ep(
            &cable("B", B_ID, 2).instance_id(),
            EndpointKind::Speakers,
            "{0}.{spk3}",
            "Speakers (ACX-LL-Cable B)",
            State::Disabled,
        ));
        let c = map_cable(&Made::new(cable("B", B_ID, 2), Variant::LowLatency, &[]), &before, &two);
        assert_eq!((c.endpoints[0].matched, c.endpoints[0].new.as_ref()), (Match::Ambiguous, None));
        assert_eq!(c.endpoints.iter().filter(|p| p.matched == Match::Added).count(), 3, "both Speakers and the Microphone");
    }

    #[test]
    fn the_result_is_said_in_the_map() {
        let r = |r: Result<Done, SetupError>| map("add", &r, &[], &[], &[]).result;
        assert_eq!(r(Ok(Done::Nothing)), "nothing");
        assert_eq!(r(Ok(Done::RestartPending { device: "X".into() })), "restart");
        assert_eq!(r(Err(SetupError::Checks(vec![]))), "error");
    }

    /// The file: deleted at the start, written at the end; a map that can't be made fails a run that was done, and only says so
    /// beside one that wasn't.
    #[test]
    fn the_file_is_replaced_and_failures_are_said() {
        let dir = std::env::temp_dir().join(format!("nf-endpoint-map-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("map.json");
        std::fs::write(&path, "stale").unwrap();
        start(Some(&path)).unwrap();
        assert!(!path.exists());
        start(Some(&path)).unwrap();
        let mut said = Vec::new();
        let gathered = Gathered { before: Some(vec![]), cables: vec![] };
        let r = finish(Some(&path), "add", gathered, Ok(Done::Nothing), &mut || Ok(vec![]), &mut |s: &str| said.push(s.to_string()));
        assert_eq!(r, Ok(Done::Nothing));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""command": "add""#) && text.contains(r#""cables": []"#), "{text}");
        // The endpoints before weren't read: a done run fails, a failed one keeps its error
        let none = Gathered::default();
        let r = finish(Some(&path), "migrate", none.clone(), Ok(Done::Changed), &mut || Ok(vec![]), &mut |_: &str| {});
        assert!(matches!(&r, Err(SetupError::EndpointMap(why)) if why.starts_with("done, but no endpoint map")), "{r:?}");
        let mut said = Vec::new();
        let r = finish(Some(&path), "migrate", none, Err(SetupError::Checks(vec!["x".into()])), &mut || Ok(vec![]), &mut |s: &str| {
            said.push(s.to_string())
        });
        assert_eq!(r, Err(SetupError::Checks(vec!["x".into()])));
        assert!(said[0].starts_with("  warning: no endpoint map"), "{said:?}");
        // No option: nothing at all
        assert_eq!(finish(None, "add", Gathered::default(), Ok(Done::Changed), &mut || Ok(vec![]), &mut |_: &str| {}), Ok(Done::Changed));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
