//! The endpoint map (TODO 19, the per-cable design, section 9; Astra's review, forwarded by the owner, 2026-11-25): which of a
//! cable's endpoints takes each old endpoint's place when `migrate` or `add` makes the cable in place of other device nodes, so
//! a harness can record the migration as a device change and the app's own saved settings can be remapped where the match is
//! unambiguous. `acx-ll-cable migrate|add ... --endpoint-map <file>` writes it, as JSON (UTF-8, no BOM).
//!
//! **Old and new.** A cable's old endpoints are those in the look before the run (NOTPRESENT ones too) on the nodes it replaces:
//! the old driver's nodes the migration makes it in place of (`ROOT\MEDIA\0003` for A, Cable-B.ps1's for B; recorded in the
//! store until the migration's map is final, so a run after their removal still finds their lingering endpoints), a node `add
//! --adopt` re-creates, and every earlier instance of its identity (a re-add's, a repair's: the same node). A migration's cable
//! takes its identity's other instances only, never its own now: those endpoints are this run's or an earlier attempt's, so its
//! map has the same rows whichever attempt publishes it (B's Call Microphone `added` after a retry too; see "Across runs" for
//! what may differ). Its new endpoints are those on its instance after the run. Each old endpoint is matched to the new one of
//! the same role (Speakers, render; Microphone and Call Microphone, capture): the old driver's circuits have the cable's
//! reference strings (Speaker0, Microphone0, Microphone1).
//!
//! **`match`:**
//! - `same`: Windows kept the endpoint (the same MMDevice id): nothing to remap.
//! - `moved`: one new endpoint of that role, with another id: remap the old id to it.
//! - `gone`: the cable has no endpoint of that role after the run (its microphones don't include it, or Windows hadn't made it
//!   when the run ended: a restart it needs, a failure). Not remapped.
//! - `ambiguous`: more than one endpoint of that role on the new instance, none with the old id. Not remapped.
//! - `added`: a new endpoint no old one maps to (`old` is null).
//!
//! **Publishing** (Astra's review, 2026-11-25). The file is published whole at the end of a run that has read the endpoints
//! before it ([`publish`]): written to `<file>.tmp` beside it, flushed to disk, then renamed over the file. A reader finds the
//! file as it was or this run's map, never part of one, and never no file where there was one. A run that publishes none (it
//! stopped before reading the endpoints, or was killed, or its map couldn't be made) leaves the file as it was: an earlier
//! run's map, the whole migration's among them, stays until a run publishes another. `start` only checks, before anything
//! changes, that the file can be published (its folder there, a file beside it writable, the file itself replaceable and open
//! in no other process: it opens it exclusively). So a file there afterwards is this run's only if the run said so ("The
//! endpoint map: <file> ..."): a reader that needs this run's own map passes a path no earlier run used.
//!
//! **A reader closes the file before the run ends.** Windows doesn't rename over a file another process has open, whatever it
//! shares (ERROR_ACCESS_DENIED). The start's check and the publish each try again for about a second ([`BUSY`]: a scanner's
//! brief look); a reader that holds the file longer makes the start refuse the run (nothing changed), or the publish fail with
//! the file as it was (a run that was done: exit 2, "done, but the endpoint map wasn't published"; the old nodes' names kept,
//! so the next migrate or add run maps them again).
//!
//! **Across runs.** A migration's cable keeps the old nodes it replaces in its store record (`Replaces`) once it is made. Every
//! migrate or add run maps the cables it makes, finishes or repairs, and every present cable recorded as made whose record has
//! them, however the run ends (a veto, a failure, a restart); so the map of the run that leaves no record pending holds the
//! whole migration, whatever its earlier attempts did (killed, stopped by a restart: a cable made by a run that needs one stays
//! pending, and the run after the restart checks and maps it). That run drops them once its map is published, a record at a
//! time; each record keeps them for good as `Replaced`, so a drop that stops part way loses no cable: while any record keeps
//! Replaces, every run maps the cables of both ([`super::plan::mapped`]), and the map is final once none does. A map asked for
//! and not published keeps them, so the next migrate or add run maps them again. A map published while a record is pending may
//! lack a migration cable that isn't made or recorded yet (one whose make failed, or failed its check, or that a restart stopped
//! the run before), and its old endpoints with it; `result` is then `error` or `restart`. Whichever run publishes the whole
//! migration's map, its rows are the same: each cable, role, old and new MMDevice id and `match`. Not every field is: a single
//! run reads the old endpoints before it disables the old nodes (`active`, `disabled`), a retry after their removal finds them
//! lingering (`notpresent`).

use std::io::Write;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION};
use windows::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW};
use windows::core::HSTRING;

use super::endpoints::{Endpoint, State};
use super::{Done, SetupError};
use crate::cables::EndpointKind;
use crate::identity::{CableId, CableInstance, same_instance};
use crate::variant::Variant;
use crate::win_error::WinError;

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
    /// An instance of the cable's identity other than its own now (n): a migration's cable, whose own instance's endpoints in
    /// the look before are this run's or an earlier attempt's, so its map has the same rows whichever attempt publishes it (its
    /// Call Microphone `added`, not `same`, after a retry too).
    OtherInstance { id: CableId, n: u32 },
}

impl Old {
    fn holds(&self, e: &Endpoint) -> bool {
        match self {
            Old::Instance(id) => same_instance(id, &e.instance_id),
            Old::Identity(id) => CableInstance::parse(&e.instance_id).is_some_and(|c| c.id == *id),
            Old::OtherInstance { id, n } => CableInstance::parse(&e.instance_id).is_some_and(|c| c.id == *id && c.n != *n),
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
    /// A cable made in place of the nodes `replaces` (instance IDs), and of its own identity's earlier instances (a re-add's,
    /// a repair's: the same instance too).
    pub fn new(cable: CableInstance, variant: Variant, replaces: &[String]) -> Self {
        let mut old: Vec<Old> = replaces.iter().cloned().map(Old::Instance).collect();
        old.push(Old::Identity(cable.id));
        Self { cable, variant, old }
    }

    /// A migration's cable, made in place of the old driver's nodes `replaces`: its identity's other instances too, never its
    /// own now ([`Old::OtherInstance`]).
    pub fn migration(cable: CableInstance, variant: Variant, replaces: &[String]) -> Self {
        let mut old: Vec<Old> = replaces.iter().cloned().map(Old::Instance).collect();
        old.push(Old::OtherInstance { id: cable.id, n: cable.n });
        Self { cable, variant, old }
    }

    /// The other nodes it takes the place of (instance IDs).
    pub fn replaces(&self) -> impl Iterator<Item = &String> {
        self.old.iter().filter_map(|o| match o {
            Old::Instance(id) => Some(id),
            Old::Identity(_) | Old::OtherInstance { .. } => None,
        })
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
    let replaces = made.replaces().cloned().collect();
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
        Err(e) if e.needs_restart() => "restart",
        Err(_) => "error",
    };
    let cables = cables.iter().map(|m| map_cable(m, before, after)).collect();
    EndpointMap { format: FORMAT, version: VERSION, command, result, cables }
}

/// The temporary file a map is written to before it is renamed over `path`: `<file>.tmp`, beside it (a rename stays on its
/// volume). `None` if `path` names no file.
fn temporary(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?;
    let mut tmp = name.to_os_string();
    tmp.push(".tmp");
    Some(path.with_file_name(tmp))
}

/// How long the start's check and the publish wait out another process that has the file open (a scanner's brief look, a
/// reader not done yet): this many tries, this pause apart, about a second in all (the module's docs).
pub const BUSY: Busy = Busy { tries: 10, pause: Duration::from_millis(100) };

/// How often, and how far apart, a call on the map's file is tried while another process has the file open.
#[derive(Clone, Copy, Debug)]
pub struct Busy {
    pub tries: u32,
    pub pause: Duration,
}

impl Busy {
    /// `f` until it succeeds, fails for another reason, or the tries are spent: tried again while `code` of its error is a
    /// Win32 code of a file another process has open (ERROR_SHARING_VIOLATION; ERROR_ACCESS_DENIED, which a rename over an open
    /// file gives whatever the reader shares; a read-only file gives it too, and waits the tries out for nothing).
    fn retry<T, E>(self, code: impl Fn(&E) -> Option<u32>, mut f: impl FnMut() -> Result<T, E>) -> Result<T, E> {
        let busy = |c: u32| c == ERROR_SHARING_VIOLATION.0 || c == ERROR_ACCESS_DENIED.0;
        let mut left = self.tries.max(1);
        loop {
            match f() {
                Err(e) if left > 1 && code(&e).is_some_and(busy) => {
                    left -= 1;
                    std::thread::sleep(self.pause);
                }
                r => return r,
            }
        }
    }
}

/// The map's file at the start of a run, before anything changes: whether it can be published (the module's docs). It isn't
/// touched: a map an earlier run published stays until this run publishes its own.
pub fn start(path: Option<&Path>) -> Result<(), SetupError> {
    start_within(path, BUSY)
}

/// [`start`], another process's open waited out as `busy` says.
fn start_within(path: Option<&Path>, busy: Busy) -> Result<(), SetupError> {
    let Some(path) = path else { return Ok(()) };
    let refuse = |why: String| Err(SetupError::EndpointMap(format!("{why}: nothing was changed")));
    let shown = path.display();
    let Some(tmp) = temporary(path).filter(|_| !path.is_dir()) else { return refuse(format!("{shown} isn't a file's name")) };
    if path.exists() {
        // Exclusively (no sharing): it fails while any other process has the file open, as the publish's rename would
        let open = || std::fs::OpenOptions::new().write(true).share_mode(0).open(path);
        if let Err(e) = busy.retry(|e: &std::io::Error| e.raw_os_error().map(|c| c as u32), open) {
            return refuse(format!("{shown} is there and can't be replaced: open in another process, or read-only ({e})"));
        }
    }
    if let Err(e) = std::fs::File::create(&tmp) {
        return refuse(format!("{} can't be written beside it ({e})", tmp.display()));
    }
    let _ = std::fs::remove_file(&tmp);
    Ok(())
}

/// `text` published at `path` whole (Astra's review, 2026-11-25): written to `<file>.tmp` beside it (one a killed run left is
/// written over), flushed to disk (FlushFileBuffers), then renamed over `path` (MoveFileExW with MOVEFILE_REPLACE_EXISTING, and
/// MOVEFILE_WRITE_THROUGH: it returns once the rename is on disk), tried again for about a second while another process has
/// `path` open ([`BUSY`]). A reader finds `path` as it was, or `text`. On a failure the temporary file is deleted and `path` is
/// as it was.
pub fn publish(path: &Path, text: &str) -> Result<(), String> {
    publish_within(path, text, BUSY)
}

/// [`publish`], another process's open waited out as `busy` says.
fn publish_within(path: &Path, text: &str, busy: Busy) -> Result<(), String> {
    let tmp = temporary(path).ok_or_else(|| format!("{} isn't a file's name", path.display()))?;
    let written = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{} couldn't be written ({e})", tmp.display()));
    }
    let (from, to) = (HSTRING::from(tmp.as_path()), HSTRING::from(path));
    let rename = || {
        // SAFETY: two NUL-terminated paths that outlive the call; constant flags.
        unsafe { MoveFileExW(&from, &to, MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) }.map_err(WinError::of("MoveFileExW"))
    };
    if let Err(e) = busy.retry(|e: &WinError| Some(e.code), rename) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{} couldn't be renamed over {} ({e})", tmp.display(), path.display()));
    }
    Ok(())
}

/// The map published at `path` at the end of a run (`gathered`, `result`), the endpoints after it read by `after`; said. A map
/// that can't be made or published turns a run that was done into [`SetupError::EndpointMap`]; any other result stands, the
/// failure said beside it. With the result, whether this run published a map (or was asked for none): only then may its
/// records drop the nodes they replace (`super::settle`).
pub fn finish(
    path: Option<&Path>, command: &'static str, gathered: Gathered, result: Result<Done, SetupError>,
    after: &mut dyn FnMut() -> Result<Vec<Endpoint>, WinError>, say: &mut dyn FnMut(&str),
) -> (Result<Done, SetupError>, bool) {
    let Some(path) = path else { return (result, true) };
    let mut failed = |why: String, result: Result<Done, SetupError>| match result {
        Ok(Done::Changed | Done::Nothing) => (Err(SetupError::EndpointMap(format!("done, but {why}"))), false),
        other => {
            say(&format!("  warning: {why}"));
            (other, false)
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
    if let Err(why) = publish(path, &(text + "\n")) {
        return failed(format!("the endpoint map wasn't published ({why}): {} is as it was", path.display()), result);
    }
    let remaps = m.cables.iter().flat_map(|c| &c.endpoints).filter(|p| p.old.is_some() && p.matched.remaps()).count();
    say(&format!("The endpoint map: {} ({} cable(s), {remaps} old endpoint(s) with a new one)", path.display(), m.cables.len()));
    (result, true)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::setup::plan::tests::{A_ID, B_ID, cid, label};

    fn cable(l: &str, id: &str, n: u32) -> CableInstance {
        CableInstance { label: label(l), id: cid(id), n }
    }

    fn ep(instance: &str, kind: EndpointKind, id: &str, name: &str, state: State) -> Endpoint {
        Endpoint { id: id.into(), name: name.into(), kind, state, instance_id: instance.to_lowercase() }
    }

    /// The migration: the old A (both microphones) and Cable-B.ps1's B (Speakers and Microphone) to A and B, both with both.
    /// B's one microphone (the old driver's Microphone0, so a Microphone here) maps to B's low-latency Microphone, so an app's
    /// saved choice keeps working, and B's Call Microphone is a newly added endpoint (the owner's decision with Astra,
    /// 2026-11-25).
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
            Made::migration(a.clone(), Variant::Both, &[r"ROOT\MEDIA\0003".into()]),
            Made::migration(b.clone(), Variant::Both, &[r"ROOT\MEDIA\0004".into()]),
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
        // A retry's look before has the cables an earlier attempt made (A and B on their own instances, now): the same map, B's
        // Call Microphone still added. Another instance of A's identity (an n + 1 an earlier attempt made, say) still maps
        let mut retry = before.clone();
        retry.extend(after.iter().cloned());
        assert_eq!(map("migrate", &Ok(Done::Changed), &made, &retry, &after), m, "whichever attempt publishes it");
        retry.push(ep(
            &cable("A", A_ID, 7).instance_id(),
            EndpointKind::Speakers,
            "{0}.{A7}",
            "Speakers (ACX-LL-Cable A)",
            State::NotPresent,
        ));
        let a7 = map_cable(&made[0], &retry, &after);
        assert!(a7.endpoints.iter().any(|p| p.old.as_ref().is_some_and(|o| o.id == "{0}.{A7}") && p.matched == Match::Moved), "{a7:?}");
        // As a re-add is mapped instead (Made::new), its own instance's endpoints are `same`
        let readd = map_cable(&Made::new(b.clone(), Variant::Both, &[]), &retry, &after);
        assert!(readd.endpoints.iter().all(|p| p.matched == Match::Same), "{readd:?}");
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
        // An error that needs a restart first (exit 7): a restart, as the exit code says
        assert_eq!(r(Err(SetupError::Checks(vec![]).and_restart(Some("X".into())))), "restart");
    }

    /// A folder of the test's own under the system's temporary folder, empty.
    pub(crate) fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nf-endpoint-map-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The file: checked at the start and left as it is; published whole at the end. A map that can't be made fails a run that
    /// was done, and only says so beside one that wasn't; a run that publishes none leaves the file as it was.
    #[test]
    fn the_file_is_published_whole_and_failures_are_said() {
        let dir = scratch("file");
        let path = dir.join("map.json");
        std::fs::write(&path, "an earlier run's").unwrap();
        start(Some(&path)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "an earlier run's", "the start doesn't touch it");
        assert!(!dir.join("map.json.tmp").exists(), "the start's check leaves nothing beside it");
        // A temporary file a killed run left is written over
        std::fs::write(dir.join("map.json.tmp"), "a killed run's").unwrap();
        let mut said = Vec::new();
        let gathered = Gathered { before: Some(vec![]), cables: vec![] };
        let r = finish(Some(&path), "add", gathered, Ok(Done::Nothing), &mut || Ok(vec![]), &mut |s: &str| said.push(s.to_string()));
        assert_eq!(r, (Ok(Done::Nothing), true));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""command": "add""#) && text.contains(r#""cables": []"#), "{text}");
        assert!(!dir.join("map.json.tmp").exists());
        // The endpoints before weren't read: a done run fails, a failed one keeps its error; neither publishes, and the file
        // is as it was
        let none = Gathered::default();
        let r = finish(Some(&path), "migrate", none.clone(), Ok(Done::Changed), &mut || Ok(vec![]), &mut |_: &str| {});
        assert!(matches!(&r, (Err(SetupError::EndpointMap(why)), false) if why.starts_with("done, but no endpoint map")), "{r:?}");
        let mut said = Vec::new();
        let r = finish(Some(&path), "migrate", none, Err(SetupError::Checks(vec!["x".into()])), &mut || Ok(vec![]), &mut |s: &str| {
            said.push(s.to_string())
        });
        assert_eq!(r, (Err(SetupError::Checks(vec!["x".into()])), false));
        assert!(said[0].starts_with("  warning: no endpoint map"), "{said:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "the last published map stays");
        // No option: nothing at all, and nothing holds the records' drop back
        let r = finish(None, "add", Gathered::default(), Ok(Done::Changed), &mut || Ok(vec![]), &mut |_: &str| {});
        assert_eq!(r, (Ok(Done::Changed), true));
        // A path that can't be published is refused at the start, before anything changes
        let e = start(Some(&dir)).unwrap_err().to_string();
        assert!(e.contains("isn't a file's name") && e.ends_with("nothing was changed"), "{e}");
        let e = start(Some(&dir.join("no folder").join("map.json"))).unwrap_err().to_string();
        assert!(e.contains("map.json.tmp can't be written beside it"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The publish replaces the file in one rename, or not at all: the old file stays if the new one can't be written.
    #[test]
    fn a_publish_replaces_the_file_whole_or_not_at_all() {
        let dir = scratch("publish");
        let path = dir.join("map.json");
        publish(&path, "first").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first", "no file before: made");
        publish(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second", "renamed over the first");
        // The temporary file can't be made (a folder holds its name): nothing changes
        std::fs::create_dir(dir.join("map.json.tmp")).unwrap();
        let why = publish(&path, "third").unwrap_err();
        assert!(why.contains("map.json.tmp couldn't be written"), "{why}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        std::fs::remove_dir(dir.join("map.json.tmp")).unwrap();
        // The rename refused (the file is read-only): the file as it was, the temporary one gone
        let writable = std::fs::metadata(&path).unwrap().permissions();
        let mut read_only = writable.clone();
        read_only.set_readonly(true);
        std::fs::set_permissions(&path, read_only).unwrap();
        let why = publish_within(&path, "fourth", QUICK).unwrap_err();
        assert!(why.contains("couldn't be renamed over") && why.contains("MoveFileExW failed"), "{why}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        assert!(!dir.join("map.json.tmp").exists());
        assert!(start_within(Some(&path), QUICK).unwrap_err().to_string().contains("is there and can't be replaced"));
        std::fs::set_permissions(&path, writable).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The busy waits, short: where the wait isn't what is tested.
    const QUICK: Busy = Busy { tries: 2, pause: Duration::from_millis(10) };

    /// Astra's review (2026-11-25): a reader that has the file open blocks the rename, whatever it shares (std's open shares
    /// read, write and delete, the most a reader can). The start refuses the run while it is open; the publish waits for it
    /// (about a second: [`BUSY`]), then fails with the file as it was. A reader that closes it within the wait doesn't stop it.
    #[test]
    fn a_reader_with_the_file_open_is_waited_for_then_refused() {
        let dir = scratch("reader");
        let path = dir.join("map.json");
        publish(&path, "first").unwrap();
        let reader = std::fs::File::open(&path).unwrap();
        let e = start_within(Some(&path), QUICK).unwrap_err().to_string();
        assert!(e.contains("is there and can't be replaced: open in another process") && e.ends_with("nothing was changed"), "{e}");
        let why = publish_within(&path, "second", QUICK).unwrap_err();
        assert!(why.contains("couldn't be renamed over") && why.contains("MoveFileExW failed"), "{why}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first", "the file as it was");
        assert!(!dir.join("map.json.tmp").exists());
        // Closed within the wait: published
        let closer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(reader);
        });
        let waited = std::time::Instant::now();
        publish(&path, "third").unwrap();
        closer.join().unwrap();
        assert!(waited.elapsed() >= Duration::from_millis(100), "it waited for the reader: {:?}", waited.elapsed());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "third");
        start(Some(&path)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
