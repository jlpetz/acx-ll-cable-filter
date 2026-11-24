//! The cables' setup (TODO 19, the per-cable design, doc/todo/19-per-cable-design.md, sections 4, 8 and 9): one
//! implementation behind the `acx-ll-cable` tool, `driver\scripts\Cable.ps1`, `Install-Driver.ps1` and the variant switch
//! ([`crate::variant`]). Every action that changes a node takes the setup's lock ([`lock`]), which only an elevated
//! administrator can take, before it reads anything.
//!
//! - [`list`] (any user): the present nodes with one of our hardware IDs (the old driver's too, so unconfigured ones and ones
//!   with a problem show), the remembered absent cables, and setup errors.
//! - [`add`]: a cable by label. Present and healthy: nothing. Present but unhealthy: its node gets the package again in place
//!   (same identity, same instance ID), D1's start count cleared. Absent: made with the identity and n the store has, or new
//!   ones. The node is created with its fixed instance ID, `ROOT\MEDIA\ACXLLCABLE_<label>_<id>_<n>`, its hardware ID and
//!   friendly name set before registration; the package is the one the cables run ([`package::pick`]). `add` never restarts
//!   another cable.
//! - [`remove`]: a cable's nodes (by identity), or every node of ours (`--all`); the store keeps the identity unless forgotten.
//! - [`update`]: every node of ours to a package folder's driver. Every cable runs one service and one image, which reloads
//!   only once no device and no control-device handle holds it, so the update disables every node (politely, without
//!   persistence), waits for the driver's service to stop ([`service`]), installs, enables and checks each. If the image
//!   doesn't unload, or a node refuses the disable, it enables what it disabled and stops: the old image is still in use, so
//!   close the apps using a cable and run it again. Nothing is installed then.
//! - [`migrate`]: from the old driver's devices (`ROOT\AudioCodec*`, A; `ROOT\NoiseFilterCableB`, the B Cable-B.ps1 made) to
//!   cables: the package checked for every cable to make, A and B recorded in the store (pending), then as `update` to unload
//!   the old image, the old nodes removed and A and B made on the package given. A run that stops part way is finished by
//!   the next.
//!
//! The sequences of `update` and the migration are [`steps`], over a seam the tests fake. The variant switch, per cable, is
//! [`crate::variant::switch`], on the same pieces.

pub mod devices;
pub mod endpoints;
pub mod lock;
pub mod package;
pub mod plan;
pub mod service;
pub mod steps;
pub mod store;

use std::fmt;
use std::path::Path;
use std::time::{Duration, Instant};

use windows::Win32::Devices::DeviceAndDriverInstallation::{SP_DEVINFO_DATA, SP_DRVINFO_DATA_V2_W};
use windows::Win32::System::Com::CoCreateGuid;

use self::devices::{Bound, DRIVER_WAIT, DeviceSet, InstallFailure, Installed, NewNode, judge_install};
use self::endpoints::{ENDPOINTS_WAIT, Endpoint, EndpointCheck, Names};
use self::lock::{LOCK, LOCK_WAIT, LockError, SetupLock};
use self::package::{Candidate, INF_NAME, PackageError, Pick, Running, file_name};
use self::plan::{AddPlan, Create, Entry, Health, Identity, Node, PlanError, RemoveTarget, UpdatePlan};
use self::steps::Real;
use crate::identity::{CableId, CableInstance, Label};
use crate::variant::Variant;
use crate::win_error::WinError;

/// The package's INF in a package folder (Build-Driver.ps1's `driver\out\Release\AudioCodec`).
pub const PACKAGE_INF: &str = "AudioCodec.inf";
/// The build's identity beside the package (Build-Driver.ps1, from D2): read and shown by `update`; checked against each
/// cable's CABLE_INFO once D2 and N1 give it.
pub const BUILD_ID_FILE: &str = "BuildId.txt";

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

/// What an action did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Done {
    /// Nothing needed doing.
    Nothing,
    Changed,
    /// Windows needs a restart to finish with `device`.
    RestartPending {
        device: String,
    },
}

/// Why an action stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetupError {
    Lock(LockError),
    Plan(PlanError),
    Package(PackageError),
    Windows(WinError),
    /// The setup's store couldn't be read or written.
    Store(WinError),
    /// The folder has no package of ours.
    NotAPackage {
        folder: String,
        why: String,
    },
    /// `update`, the migration: the package given isn't among the drivers compatible with this node, or with the cable to be
    /// made. Nothing was changed.
    NotCompatible {
        device: String,
        hardware_id: String,
        package: String,
    },
    /// The package chosen doesn't make the variant's endpoints, by its INF. Nothing was changed.
    PackageLacks {
        hardware_id: String,
        package: String,
        missing: Vec<endpoints::Kind>,
    },
    /// Windows will finish removing `device` at the next restart, so nothing was created.
    RestartFirst {
        device: String,
    },
    /// A node didn't work on the chosen package.
    Install(Box<NotInstalled>),
    /// `update`, the migration: a node refused the disable (`vetoed`), or it failed; what this run disabled was enabled again.
    Disable {
        device: String,
        vetoed: bool,
        error: Option<WinError>,
        unrestored: Vec<(String, WinError)>,
    },
    /// `update`, the migration: the old image was still loaded after [`service::UNLOAD_WAIT`]; what this run disabled was
    /// enabled again. Nothing was installed.
    StillLoaded {
        last: String,
        unrestored: Vec<(String, WinError)>,
    },
    /// The audio endpoints couldn't be read, and the action checks them first. Nothing was changed.
    Endpoints(WinError),
    /// The action was done, but these checks failed; the nodes are left as they are.
    Checks(Vec<String>),
    /// The migration stopped after the old image unloaded (`error`: an old node's removal failed, or needs a restart): every
    /// node it disabled and didn't remove was enabled again, but `unrestored`; nothing was installed. The cables to make are in
    /// the store, pending, for the next run.
    Stopped {
        error: Box<SetupError>,
        unrestored: Vec<(String, WinError)>,
        to_make: Vec<Label>,
    },
    /// The migration's cables `to_make` weren't all made (`problems` says why): they stay in the store, pending, for the next run.
    Unfinished {
        problems: Vec<String>,
        to_make: Vec<Label>,
    },
}

/// A node that didn't work on the chosen package ([`SetupError::Install`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotInstalled {
    pub device: String,
    /// The package it was given, described.
    pub chosen: String,
    pub failure: InstallFailure,
    /// How removing it again went; `None`: a node repaired in place, left as it is.
    pub removed: Option<Result<(), WinError>>,
}

impl From<LockError> for SetupError {
    fn from(e: LockError) -> Self {
        SetupError::Lock(e)
    }
}

impl From<PlanError> for SetupError {
    fn from(e: PlanError) -> Self {
        SetupError::Plan(e)
    }
}

impl From<PackageError> for SetupError {
    fn from(e: PackageError) -> Self {
        SetupError::Package(e)
    }
}

impl From<WinError> for SetupError {
    fn from(e: WinError) -> Self {
        SetupError::Windows(e)
    }
}

/// "; ROOT\MEDIA\... couldn't be enabled again (...): enable it in Device Manager".
fn unrestored_text(unrestored: &[(String, WinError)]) -> String {
    unrestored.iter().map(|(d, e)| format!(" {d} couldn't be enabled again ({e}): enable it in Device Manager.")).collect()
}

/// " ACX-LL-Cable A and ACX-LL-Cable B are recorded in the setup's store: ...", or nothing.
fn to_make_text(to_make: &[Label]) -> String {
    let (is, them) = if to_make.len() == 1 { ("is", "it") } else { ("are", "them") };
    if to_make.is_empty() {
        String::new()
    } else {
        format!(
            " {} {is} recorded in the setup's store: run Install-Driver.ps1 (or `acx-ll-cable migrate <package folder>`) again to \
             make {them}.",
            steps::labels(to_make)
        )
    }
}

impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SetupError::Lock(e) => e.fmt(f),
            SetupError::Plan(e) => e.fmt(f),
            SetupError::Package(e) => e.fmt(f),
            SetupError::Windows(e) => e.fmt(f),
            SetupError::Store(e) => write!(f, "the setup's store (HKLM\\{}) couldn't be read or written: {e}", store::KEY),
            SetupError::NotAPackage { folder, why } => write!(f, "{folder} has no driver package of ours ({PACKAGE_INF}): {why}"),
            SetupError::NotCompatible { device, hardware_id, package } => write!(
                f,
                "{package} isn't among the drivers compatible with {device} ({hardware_id}): nothing was changed. Is it a build of \
                 the per-cable driver (driver\\scripts\\Build-Driver.ps1)?"
            ),
            SetupError::PackageLacks { hardware_id, package, missing } => write!(
                f,
                "{package} doesn't make {} for {hardware_id}, by its INF: nothing was changed. Build the package again \
                 (driver\\scripts\\Build-Driver.ps1) and move the cables to it (`acx-ll-cable update`).",
                missing.iter().map(|k| k.word()).collect::<Vec<_>>().join(" and ")
            ),
            SetupError::RestartFirst { device } => {
                write!(f, "Windows will finish removing {device} when it restarts: restart Windows, then run this again")
            }
            SetupError::Install(not) => {
                let NotInstalled { device, chosen, failure, removed } = not.as_ref();
                write!(f, "{device} isn't working on the chosen driver ({chosen}): {failure}.")?;
                match removed {
                    None => write!(f, " It is left as it is: `acx-ll-cable list` shows it."),
                    Some(Ok(())) => write!(f, " It was removed again: run this again, or see what's there with `acx-ll-cable list`."),
                    Some(Err(e)) => write!(
                        f,
                        " Removing it again failed too ({e}): remove it in Device Manager, or with pnputil /remove-device {device}."
                    ),
                }
            }
            SetupError::Disable { device, vetoed, error, unrestored } => {
                match (vetoed, error) {
                    (true, _) => write!(f, "{device} refused to be disabled (CR_REMOVE_VETOED: an app or service has it in use)")?,
                    (false, Some(e)) => write!(f, "{device} couldn't be disabled ({e})")?,
                    (false, None) => write!(f, "{device} couldn't be disabled")?,
                }
                write!(
                    f,
                    ". Every device this run disabled was enabled again, and nothing was installed: close the apps using the cables, \
                     then run this again.{}",
                    unrestored_text(unrestored)
                )
            }
            SetupError::StillLoaded { last, unrestored } => write!(
                f,
                "the driver's old image is still loaded {} s after every cable was disabled (the {} service: {last}): something \
                 still has a cable's control device open (a feed, nf-vmic-test, the app's direct driver ring, or a diagnostics \
                 reader). Every device this run disabled was enabled again, and nothing was installed: close the apps using the \
                 cables, then run this again.{}",
                service::UNLOAD_WAIT.as_secs(),
                service::SERVICE,
                unrestored_text(unrestored)
            ),
            SetupError::Endpoints(e) => write!(
                f,
                "the audio endpoints couldn't be read ({e}), and this checks them first: nothing was changed. Is the Windows Audio \
                 Endpoint Builder service running?"
            ),
            SetupError::Checks(failed) => write!(f, "done, but: {}", failed.join("; ")),
            SetupError::Stopped { error, unrestored, to_make } => write!(
                f,
                "{error}. Every device this run disabled and didn't remove was enabled again, and nothing was installed.{}{}",
                unrestored_text(unrestored),
                to_make_text(to_make)
            ),
            SetupError::Unfinished { problems, to_make } => write!(f, "not finished: {}.{}", problems.join("; "), to_make_text(to_make)),
        }
    }
}

impl std::error::Error for SetupError {}

/// The present nodes of ours, with their elements in one set.
pub struct Present {
    pub set: DeviceSet,
    pub data: Vec<SP_DEVINFO_DATA>,
    pub nodes: Vec<Node>,
}

impl Present {
    pub fn read() -> Result<Self, WinError> {
        let set = DeviceSet::all_present()?;
        let (data, nodes) = set.ours()?.into_iter().unzip();
        Ok(Self { set, data, nodes })
    }

    /// The packages the present cables run ([`plan::running`]: nodes with an identity only).
    pub fn running(&self) -> Vec<(String, Running)> {
        plan::running(&self.nodes)
    }
}

/// The store's entries, its problems said.
fn read_store(say: &mut dyn FnMut(&str)) -> Result<Vec<Entry>, SetupError> {
    let s = store::read().map_err(SetupError::Store)?;
    for p in &s.problems {
        say(&format!("  (the store: {p}; left alone)"));
    }
    Ok(s.entries)
}

/// A new cable identity.
pub fn new_identity() -> Result<CableId, WinError> {
    // SAFETY: no arguments.
    let g = unsafe { CoCreateGuid() }.map_err(WinError::of("CoCreateGuid"))?;
    Ok(CableId::from_guid(g))
}

/// The package the new node gets.
#[derive(Clone, Copy, Debug)]
enum Want<'a> {
    /// The package rule: the one the cables run.
    Rule(&'a [(String, Running)]),
    /// The package staged for `update` or the migration, by its published INF.
    Published(&'a str),
}

/// One of a node's compatible drivers, by the package wanted; its driver node and candidate.
fn choose_driver(
    drivers: &[(SP_DRVINFO_DATA_V2_W, Candidate)], want: Want, hardware_id: &str, device: &str, say: &mut dyn FnMut(&str),
) -> Result<usize, SetupError> {
    let candidates: Vec<Candidate> = drivers.iter().map(|(_, c)| c.clone()).collect();
    let chosen = match want {
        Want::Rule(running) => {
            let p = package::pick(running, &candidates, hardware_id)?;
            if let Pick::Running { newer: Some(n), .. } = p {
                say(&format!(
                    "  note: a newer package of ours is staged, {}: `acx-ll-cable update` moves every cable to it; this goes on with \
                     the one the cables run",
                    candidates[n].describe()
                ));
            }
            p.index()
        }
        Want::Published(inf) => candidates
            .iter()
            .position(|c| c.is_ours() && file_name(&c.inf).eq_ignore_ascii_case(file_name(inf)))
            .ok_or_else(|| SetupError::NotCompatible { device: device.into(), hardware_id: hardware_id.into(), package: inf.into() })?,
    };
    say(&format!("  the driver: {} ({})", candidates[chosen].describe(), candidates[chosen].mics_note()));
    Ok(chosen)
}

/// What the chosen package lacks of a variant's microphones, by its INF (unread: checked after the install instead).
fn lacks(c: &Candidate, variant: Variant, hardware_id: &str) -> Result<(), SetupError> {
    match c.makes(variant.mics()) {
        Some(false) => {
            let has = c.mics.as_deref().unwrap_or_default();
            let missing = endpoints::Kind::of_variant(variant)
                .into_iter()
                .filter(|k| match k {
                    endpoints::Kind::Speakers => false,
                    endpoints::Kind::Microphone => !has.contains(&crate::sessions::Mic::LowLatency),
                    endpoints::Kind::CallMicrophone => !has.contains(&crate::sessions::Mic::Call),
                })
                .collect();
            Err(SetupError::PackageLacks { hardware_id: hardware_id.into(), package: c.describe(), missing })
        }
        _ => Ok(()),
    }
}

/// Removes the nodes `which` of `present` (DiUninstallDevice), saying each. A restart Windows needs stops it there.
fn uninstall_nodes(present: &Present, which: &[usize], say: &mut dyn FnMut(&str)) -> Result<(), SetupError> {
    for &i in which {
        let n = &present.nodes[i];
        say(&format!("Removing {} (DiUninstallDevice)", n.instance_id));
        if present.set.uninstall(&present.data[i])? {
            say("  removed, but Windows needs a restart to finish");
            return Err(SetupError::RestartFirst { device: n.instance_id.clone() });
        }
        say("  removed");
    }
    Ok(())
}

/// D1's start count deleted before an install or an enable, said; a failure is said and costs nothing else (the driver would
/// then refuse the node, which the check after names).
fn clear_count(set: &DeviceSet, data: &SP_DEVINFO_DATA, device: &str, say: &mut dyn FnMut(&str)) {
    match set.clear_start_attempts(data) {
        Ok(true) => say(&format!("  cleared {}'s start count ({})", device, devices::START_ATTEMPTS_VALUE)),
        Ok(false) => {}
        Err(e) => say(&format!("  warning: {device}'s start count couldn't be cleared ({e}): the driver may refuse it (Code 31)")),
    }
}

/// The friendly name re-applied after an install, said if it fails.
fn rename(set: &DeviceSet, data: &mut SP_DEVINFO_DATA, cable: &CableInstance, say: &mut dyn FnMut(&str)) {
    if let Err(e) = set.set_friendly_name(data, &cable.label.friendly_name()) {
        say(&format!("  warning: the friendly name \"{}\" couldn't be set again: {e}", cable.label.friendly_name()));
    }
}

/// A node made: its instance ID, and whether Windows needs a restart to finish it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Made {
    pub cable: CableInstance,
    pub restart: bool,
}

/// Why [`make`] stopped: on a re-opened element (a removed node's key), which it removed again (the caller may try n + 1), or
/// otherwise.
enum MakeError {
    Reopened(SetupError),
    Other(SetupError),
}

impl<E: Into<SetupError>> From<E> for MakeError {
    fn from(e: E) -> Self {
        MakeError::Other(e.into())
    }
}

/// Creates `create`'s node (design section 9, `add`'s "Creating it"): the element with its fixed instance ID, its hardware
/// ID and friendly name; its driver (the package wanted); then the cable's old nodes removed; DIF_REGISTERDEVICE; the start
/// count cleared; DiInstallDevice; up to [`DRIVER_WAIT`] for it to work; the friendly name again. Nothing is written between
/// registration and install that the driver reads. A node that doesn't work on the chosen package is removed again; a
/// re-opened one so removed is [`MakeError::Reopened`], for [`make_or_next`]'s n + 1.
fn make(present: &Present, create: &Create, want: Want, say: &mut dyn FnMut(&str)) -> Result<Made, MakeError> {
    let hw = create.variant.hardware_id();
    let mut node = NewNode::prepare(&create.instance, hw)?;
    let id = node.instance_id.clone();
    say(&format!("Preparing {id} ({}, {hw}), not registered yet", create.variant.describe()));
    if node.reopened {
        say("  its key was still there (a removed node's): re-opened");
    }
    let drivers = node.set.compatible(&mut node.data)?;
    let chosen = choose_driver(&drivers, want, hw, &id, say)?;
    let (driver, candidate) = drivers[chosen].clone();
    lacks(&candidate, create.variant, hw)?;
    uninstall_nodes(present, &create.remove, say)?;
    say(&format!("Creating {id} (DIF_REGISTERDEVICE)"));
    if let Err(e) = node.register() {
        return Err(if node.reopened { MakeError::Reopened(e.into()) } else { MakeError::Other(e.into()) });
    }
    clear_count(&node.set, &node.data, &id, say);
    say(&format!("Installing {} on it (DiInstallDevice)", candidate.inf));
    let call = node.set.install(&node.data, &driver);
    if let Err(e) = &call {
        say(&format!("  {e}"));
        if node.reopened && node.set.bound(&node.data).inf.is_none() {
            let removed = node.set.uninstall(&node.data).map(drop);
            say(&format!("  removed again: {removed:?}"));
            return Err(MakeError::Reopened(e.clone().into()));
        }
    }
    let restart = matches!(call, Ok(true));
    let now = if restart { node.set.bound(&node.data) } else { node.set.wait_working(&node.data, DRIVER_WAIT) };
    say(&format!("  {id}: {}", now.describe()));
    match judge_install(call.as_ref().map(drop), restart, &now, &candidate) {
        Ok(Installed::Working) => {}
        Ok(Installed::Restart) => return Ok(Made { cable: create.instance.clone(), restart: true }),
        Err(failure) => {
            say(&format!("Removing {id} again: {failure}"));
            let removed = node.set.uninstall(&node.data).map(drop);
            let fell_back = node.reopened && removed.is_ok();
            let e =
                SetupError::Install(Box::new(NotInstalled { device: id, chosen: candidate.describe(), failure, removed: Some(removed) }));
            // A re-opened element that installed but never worked (what DiUninstallDevice left of a removed node, say): gone
            // again, so n + 1 can be tried
            return Err(if fell_back { MakeError::Reopened(e) } else { MakeError::Other(e) });
        }
    }
    rename(&node.set, &mut node.data, &create.instance, say);
    Ok(Made { cable: create.instance.clone(), restart: false })
}

/// [`make`], and once more with n + 1 if the element re-opened from a removed node's key couldn't be registered, couldn't take
/// the install, or took it and never worked (the design's fallback: the endpoints are new then).
fn make_or_next(present: &Present, create: &Create, want: Want, say: &mut dyn FnMut(&str)) -> Result<Made, SetupError> {
    match make(present, create, want, say) {
        Ok(m) => Ok(m),
        Err(MakeError::Other(e)) => Err(e),
        Err(MakeError::Reopened(e)) => {
            let Some(next) = create.instance.next() else { return Err(e) };
            say(&format!("  the re-opened node couldn't be installed ({e}): making {} instead, with new endpoints", next.instance_id()));
            // The old nodes are gone already, if there were any
            let again = Create { instance: next, remove: Vec::new(), same_instance: false, ..create.clone() };
            make(present, &again, want, say).map_err(|e| match e {
                MakeError::Reopened(e) | MakeError::Other(e) => e,
            })
        }
    }
}

/// Up to [`ENDPOINTS_WAIT`] for a cable's endpoints, said, their names judged by `names`; the checks that failed.
fn wait_endpoints(cable: &CableInstance, variant: Variant, names: Names, say: &mut dyn FnMut(&str)) -> Vec<String> {
    say(&format!("Waiting for Windows to make {}'s endpoints (up to {} s)", cable.label.friendly_name(), ENDPOINTS_WAIT.as_secs()));
    let found = endpoints::wait_for(cable, variant, names, ENDPOINTS_WAIT, &mut endpoints::all, &mut SystemClock::start());
    steps::endpoint_problems(cable, found, say)
}

/// The endpoints before an action (NOTPRESENT ones too), whose names they keep; none if they don't read (said: then every
/// name is checked against the expected one).
fn endpoints_before(say: &mut dyn FnMut(&str)) -> Vec<Endpoint> {
    endpoints::before().unwrap_or_else(|e| {
        say(&format!("  note: the endpoints couldn't be read before the run ({e}): their names are checked against the expected ones"));
        Vec::new()
    })
}

/// The store's record of a cable as made.
fn record(cable: &CableInstance, variant: Variant) -> Result<(), SetupError> {
    let entry = Entry { label: cable.label.clone(), id: cable.id, n: cable.n, mics: Some(variant), pending: false };
    store::write(&entry).map_err(SetupError::Store)
}

/// A cable's node made on the package the cables run (the variant switch's step 4, [`crate::variant::switch`]): `create`'s
/// old nodes removed once the new one's driver is found.
pub fn make_cable(
    present: &Present, create: &Create, running: &[(String, Running)], say: &mut dyn FnMut(&str),
) -> Result<Made, SetupError> {
    make_or_next(present, create, Want::Rule(running), say)
}

/// The store's record of a cable as made.
pub fn record_cable(cable: &CableInstance, variant: Variant) -> Result<(), SetupError> {
    record(cable, variant)
}

/// Up to [`ENDPOINTS_WAIT`] for a cable's endpoints, each said, their names judged by `names`; the checks that failed.
pub fn wait_cable_endpoints(cable: &CableInstance, variant: Variant, names: Names, say: &mut dyn FnMut(&str)) -> Vec<String> {
    wait_endpoints(cable, variant, names, say)
}

/// `add <label> [--mics m] [--adopt instance]` (the module's docs).
pub fn add(label: &Label, mics: Option<Variant>, adopt: Option<&str>, say: &mut dyn FnMut(&str)) -> Result<Done, SetupError> {
    let _lock = SetupLock::take(&LOCK, LOCK_WAIT, say)?;
    let present = Present::read()?;
    let entries = read_store(say)?;
    let plan = plan::plan_add(label, mics, adopt, &entries, &present.nodes, new_identity()?)?;
    // Before anything changes: the names a re-added or repaired cable's endpoints keep (the user's renames among them)
    let before = if matches!(plan, AddPlan::Nothing { .. }) { Vec::new() } else { endpoints_before(say) };
    match plan {
        AddPlan::Nothing { node, prune, mics_differ, identity } => {
            let n = &present.nodes[node];
            say(&format!("{} is present and working: {}", label.friendly_name(), n.describe()));
            if let Some(m) = mics_differ {
                say(&format!(
                    "  it has {}, not {}: `acx-ll-cable variant {label} {}` switches",
                    n.variant().map_or("?", Variant::arg),
                    m.arg(),
                    m.arg()
                ));
            }
            uninstall_nodes(&present, &prune, say)?;
            if identity == Identity::FromNode
                && let (Some(c), Some(v)) = (&n.cable, n.variant())
            {
                record(c, v)?;
                say("  recorded in the setup's store");
            }
            Ok(if prune.is_empty() { Done::Nothing } else { Done::Changed })
        }
        AddPlan::Repair { node, prune, mics_differ, .. } => repair(&present, node, &prune, mics_differ, &before, say),
        AddPlan::Create(create) => {
            say(&match create.identity {
                Identity::New => format!("Adding {} with a new identity", label.friendly_name()),
                _ if create.same_instance => format!("Adding {} again, with its identity and instance ID", label.friendly_name()),
                _ => format!("Adding {} with its identity", label.friendly_name()),
            });
            let running = present.running();
            let made = make_or_next(&present, &create, Want::Rule(&running), say)?;
            record(&made.cable, create.variant)?;
            if made.restart {
                return Ok(Done::RestartPending { device: made.cable.instance_id() });
            }
            // A re-added cable's endpoints come back with their names (we expect: to confirm on the box); a new one's are new
            let problems = wait_endpoints(&made.cable, create.variant, Names::Since(&before), say);
            if problems.is_empty() { Ok(Done::Changed) } else { Err(SetupError::Checks(problems)) }
        }
    }
}

/// A present but unhealthy cable's node given the package again in place (`add`'s repair); `before`, its endpoints before.
fn repair(
    present: &Present, node: usize, prune: &[usize], mics_differ: Option<Variant>, before: &[Endpoint], say: &mut dyn FnMut(&str),
) -> Result<Done, SetupError> {
    let n = &present.nodes[node];
    let (Some(cable), Some(variant)) = (n.cable.clone(), n.variant()) else { unreachable!("plan_add repairs cables only") };
    say(&format!("{} is present but not healthy: {}", cable.label.friendly_name(), n.describe()));
    say("  repairing it in place: its identity and instance ID stay");
    if let Some(m) = mics_differ {
        say(&format!("  it keeps {}; `acx-ll-cable variant {} {}` switches", variant.describe(), cable.label, m.arg()));
    }
    let mut data = present.data[node];
    let drivers = present.set.compatible(&mut data)?;
    let others: Vec<(String, Running)> =
        present.running().into_iter().filter(|(id, _)| !prune.iter().any(|&p| present.nodes[p].instance_id == *id)).collect();
    let chosen = choose_driver(&drivers, Want::Rule(&others), variant.hardware_id(), &n.instance_id, say)?;
    let (driver, candidate) = drivers[chosen].clone();
    lacks(&candidate, variant, variant.hardware_id())?;
    uninstall_nodes(present, prune, say)?;
    clear_count(&present.set, &data, &n.instance_id, say);
    rename(&present.set, &mut data, &cable, say);
    say(&format!("Installing {} on {} (DiInstallDevice)", candidate.inf, n.instance_id));
    let call = present.set.install(&data, &driver);
    if n.health == Health::Problem(plan::PROBLEM_DISABLED) || matches!(present.set.bound(&data).health, Health::Problem(22)) {
        clear_count(&present.set, &data, &n.instance_id, say);
        say("  enabling it");
        present.set.enable(&data)?;
    }
    let restart = matches!(call, Ok(true));
    let now: Bound = if restart { present.set.bound(&data) } else { present.set.wait_working(&data, DRIVER_WAIT) };
    say(&format!("  {}: {}", n.instance_id, now.describe()));
    match judge_install(call.as_ref().map(drop), restart, &now, &candidate) {
        Ok(Installed::Working) => {}
        Ok(Installed::Restart) => return Ok(Done::RestartPending { device: n.instance_id.clone() }),
        Err(failure) => {
            let not = NotInstalled { device: n.instance_id.clone(), chosen: candidate.describe(), failure, removed: None };
            return Err(SetupError::Install(Box::new(not)));
        }
    }
    rename(&present.set, &mut data, &cable, say);
    record(&cable, variant)?;
    let problems = wait_endpoints(&cable, variant, Names::Since(before), say);
    if problems.is_empty() { Ok(Done::Changed) } else { Err(SetupError::Checks(problems)) }
}

/// `remove <label> [--forget]`, `remove --all [--forget]`.
pub fn remove(target: &RemoveTarget, forget: bool, say: &mut dyn FnMut(&str)) -> Result<Done, SetupError> {
    let _lock = SetupLock::take(&LOCK, LOCK_WAIT, say)?;
    let present = Present::read()?;
    let entries = read_store(say)?;
    let plan = plan::plan_remove(target, forget, &entries, &present.nodes);
    if plan.remove.is_empty() {
        say(&match target {
            RemoveTarget::Label(l) => format!("{} isn't present: nothing to remove", l.friendly_name()),
            RemoveTarget::All => "no device of ours is present: nothing to remove".into(),
        });
    }
    let mut restart = None;
    for &i in &plan.remove {
        let n = &present.nodes[i];
        say(&format!("Removing {} (DiUninstallDevice)", n.describe()));
        if present.set.uninstall(&present.data[i])? {
            say("  removed, but Windows needs a restart to finish");
            restart = Some(n.instance_id.clone());
        } else {
            say("  removed");
        }
    }
    for label in &plan.forget {
        store::forget(label).map_err(SetupError::Store)?;
        say(&format!("Forgot {label}'s identity: an `add {label}` now makes a new cable, with new endpoints"));
    }
    if let RemoveTarget::Label(l) = target
        && !plan.remove.is_empty()
        && plan.forget.is_empty()
    {
        say(&format!("{l}'s identity stays in the setup's store: `add {l}` brings it back with its endpoints"));
    }
    Ok(match restart {
        Some(device) => Done::RestartPending { device },
        None if plan.remove.is_empty() && plan.forget.is_empty() => Done::Nothing,
        None => Done::Changed,
    })
}

/// A package folder's INF, staged in the driver store: its published INF. Ours only.
pub fn stage_package(folder: &Path, say: &mut dyn FnMut(&str)) -> Result<String, SetupError> {
    let inf = folder.join(PACKAGE_INF);
    let shown = folder.display().to_string();
    if !inf.is_file() {
        return Err(SetupError::NotAPackage { folder: shown, why: format!("no {PACKAGE_INF} there") });
    }
    let inf_text = inf.to_str().ok_or_else(|| SetupError::NotAPackage { folder: shown.clone(), why: "the path isn't Unicode".into() })?;
    say(&format!("Adding {inf_text} to the driver store (SetupCopyOEMInfW, no device installed)"));
    let published = devices::stage(inf_text)?;
    let original =
        devices::original_inf_name(&published).map_err(|e| SetupError::NotAPackage { folder: shown.clone(), why: e.to_string() })?;
    if !original.eq_ignore_ascii_case(INF_NAME) {
        return Err(SetupError::NotAPackage { folder: shown, why: format!("its INF was published as {published}, from {original}") });
    }
    say(&format!("  published as {published}"));
    match std::fs::read_to_string(folder.join(BUILD_ID_FILE)) {
        Ok(id) => say(&format!("  its build: {} (each cable's is checked once its CABLE_INFO reports one: D2)", id.trim())),
        Err(_) => say(&format!("  no {BUILD_ID_FILE} beside it (Build-Driver.ps1 writes it from D2): the build isn't checked")),
    }
    Ok(published)
}

/// `update <package folder>` (the module's docs; the sequence is [`steps::run_update`]).
pub fn update(folder: &Path, say: &mut dyn FnMut(&str)) -> Result<Done, SetupError> {
    let _lock = SetupLock::take(&LOCK, LOCK_WAIT, say)?;
    let present = Present::read()?;
    let plan: UpdatePlan = plan::plan_update(&present.nodes)?;
    for &i in &plan.unidentified {
        say(&format!("  {} has no identity: disabled and enabled with the rest, not installed", present.nodes[i].describe()));
    }
    for e in read_store(say)?.iter().filter(|e| e.pending) {
        say(&format!(
            "  note: {} is a migration's, not made yet: `acx-ll-cable migrate <package folder>` (Install-Driver.ps1) makes it",
            e.label.friendly_name()
        ));
    }
    let published = stage_package(folder, say)?;
    steps::run_update(&mut Real { present: &present }, &present.nodes, &plan, &published, say)
}

/// `migrate <package folder> [--a-mics m] [--b-mics m]`: today's devices (the old driver's A, Cable-B.ps1's B) to cables, or
/// the cables a stopped migration left to make (the module's docs; the sequence is [`steps::run_migrate`]). Written for S0a;
/// not run until the owner's go.
pub fn migrate(folder: &Path, a_mics: Option<Variant>, b_mics: Option<Variant>, say: &mut dyn FnMut(&str)) -> Result<Done, SetupError> {
    let _lock = SetupLock::take(&LOCK, LOCK_WAIT, say)?;
    let present = Present::read()?;
    let entries = read_store(say)?;
    let plan = plan::plan_migrate(&present.nodes, &entries, a_mics, b_mics, [new_identity()?, new_identity()?])?;
    for &i in &plan.legacy {
        say(&format!("The old driver's: {}", present.nodes[i].describe()));
    }
    for t in &plan.add {
        let from = if t.new { "a new identity" } else { "its identity from the setup's store" };
        say(&format!("To make: {} ({}), with {from}", t.label.friendly_name(), t.variant.describe()));
    }
    let published = stage_package(folder, say)?;
    steps::run_migrate(&mut Real { present: &present }, &present.nodes, &plan, &published, say)
}

/// What `list` reads (any user, no lock): the nodes, the store, and the endpoints (or why they couldn't be read).
pub struct Report {
    pub nodes: Vec<Node>,
    pub store: store::Store,
    pub endpoints: Result<Vec<Endpoint>, WinError>,
}

pub fn read_report() -> Result<Report, SetupError> {
    let present = Present::read()?;
    let store = store::read().map_err(SetupError::Store)?;
    Ok(Report { nodes: present.nodes, store, endpoints: endpoints::all() })
}

/// `list`'s text (pure).
pub fn format_list(r: &Report) -> String {
    let mut s = String::new();
    let l = plan::listing(&r.store.entries, &r.nodes);
    let mut line = |t: String| {
        s.push_str(&t);
        s.push('\n');
    };
    if l.cables.is_empty() {
        line("No cable is present.".into());
    }
    for c in &l.cables {
        line(format!("{} (id {})", c.label.friendly_name(), c.id));
        for &i in &c.nodes {
            let n = &r.nodes[i];
            line(format!("  {}", n.describe()));
            if let (Some(cable), Some(v), Ok(eps)) = (&n.cable, n.variant(), &r.endpoints) {
                let check: EndpointCheck = endpoints::check(cable, v, eps, Names::Any);
                for (kind, e) in &check.found {
                    let state = if e.state == endpoints::State::Active { String::new() } else { format!(", {:?}", e.state).to_lowercase() };
                    line(format!("    {}: \"{}\"{state}", kind.word(), e.name));
                }
                if !check.is_done() && n.health == Health::Working {
                    line(format!("    note: {check}"));
                }
            }
        }
        line(format!("  control device: {}", crate::identity::control_path(c.id)));
    }
    for e in &l.absent {
        let mics = e.mics.map(|m| format!(", {}", m.arg())).unwrap_or_default();
        line(if e.pending {
            format!(
                "{} (id {}): absent, the migration's to make (n {}{mics}): Install-Driver.ps1 (or `acx-ll-cable migrate <package \
                 folder>`) makes it",
                e.label.friendly_name(),
                e.id,
                e.n
            )
        } else {
            format!("{} (id {}): absent, remembered (n {}{mics}): `add {}` brings it back", e.label.friendly_name(), e.id, e.n, e.label)
        });
    }
    for &i in &l.legacy {
        line(format!("The old driver's device: {}", r.nodes[i].describe()));
    }
    if !l.legacy.is_empty() {
        line("  migrate with driver\\scripts\\Install-Driver.ps1 (or `acx-ll-cable migrate <package folder>`)".into());
    }
    for &i in &l.unidentified {
        line(format!("Not a cable: {}", r.nodes[i].describe()));
    }
    for p in l.problems.iter().chain(&r.store.problems) {
        line(format!("Setup error: {p}"));
    }
    if let Err(e) = &r.endpoints {
        line(format!("(The endpoints couldn't be read: {e})"));
    }
    s
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::setup::plan::tests::{A_ID, B_ID, node};

    /// A clock that moves only when slept on.
    pub(crate) struct FakeClock(pub Duration);

    impl Clock for FakeClock {
        fn elapsed(&self) -> Duration {
            self.0
        }

        fn sleep(&mut self, d: Duration) {
            self.0 += d;
        }
    }

    #[test]
    fn the_list_shows_each_cable_with_its_endpoints() {
        let a = node("A", A_ID, 1, Variant::Both);
        let mut b = node("B", B_ID, 2, Variant::LowLatency);
        b.health = Health::Problem(31);
        b.problem_status = Some(plan::STATUS_START_BREAKER);
        let ac = a.cable.clone().unwrap();
        let eps = endpoints::Kind::of_variant(Variant::Both)
            .into_iter()
            .map(|kind| Endpoint {
                id: format!("{kind:?}"),
                name: endpoints::expected_name(&ac, kind),
                kind,
                state: endpoints::State::Active,
                instance_id: ac.instance_id().to_lowercase(),
            })
            .collect();
        let gone = Entry {
            label: "C".parse().unwrap(),
            id: "cccccccc-0000-4000-8000-00000000000c".parse().unwrap(),
            n: 3,
            mics: Some(Variant::Call),
            pending: false,
        };
        let to_make = Entry {
            label: "E".parse().unwrap(),
            id: "eeeeeeee-0000-4000-8000-00000000000e".parse().unwrap(),
            n: 1,
            mics: Some(Variant::Both),
            pending: true,
        };
        let store = store::Store { entries: vec![gone, to_make], problems: vec![] };
        let r = Report { nodes: vec![a, b], store, endpoints: Ok(eps) };
        let text = format_list(&r);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], format!("ACX-LL-Cable A (id {A_ID})"));
        assert!(
            lines[1].starts_with(r"  ROOT\MEDIA\ACXLLCABLE_A_") && lines[1].contains("both microphones (ROOT\\AcxLlCable), working"),
            "{}",
            lines[1]
        );
        assert_eq!(lines[2], "    Speakers: \"Speakers (ACX-LL-Cable A)\"");
        assert_eq!(lines[4], "    Call Microphone: \"Call Microphone (ACX-LL-Cable A)\"");
        assert_eq!(lines[5], format!(r"  control device: \\.\AcxLlCable_{A_ID}"));
        assert_eq!(lines[6], format!("ACX-LL-Cable B (id {B_ID})"));
        assert!(lines[7].contains("refused by the driver's crash breaker"), "{}", lines[7]);
        assert!(
            text.contains(
                "ACX-LL-Cable C (id cccccccc-0000-4000-8000-00000000000c): absent, remembered (n 3, call): `add C` brings it back"
            ),
            "{text}"
        );
        assert!(text.contains("ACX-LL-Cable E (id eeeeeeee-0000-4000-8000-00000000000e): absent, the migration's to make (n 1, both)"));
    }

    #[test]
    fn setup_errors_say_what_to_do() {
        let text = SetupError::StillLoaded { last: "Loaded(4)".into(), unrestored: vec![] }.to_string();
        assert!(
            text.starts_with("the driver's old image is still loaded 10 s after every cable was disabled (the AudioCodec service"),
            "{text}"
        );
        assert!(text.contains("close the apps using the cables") && text.contains("nothing was installed"), "{text}");
        let text = SetupError::Disable {
            device: "X".into(),
            vetoed: true,
            error: None,
            unrestored: vec![("Y".into(), WinError { call: "CM_Enable_DevNode", code: 5 })],
        }
        .to_string();
        assert!(text.starts_with("X refused to be disabled (CR_REMOVE_VETOED"), "{text}");
        assert!(text.contains("Every device this run disabled was enabled again, and nothing was installed"), "{text}");
        assert!(text.contains(" Y couldn't be enabled again (CM_Enable_DevNode failed: ERROR_ACCESS_DENIED (5)"), "{text}");
        assert!(text.ends_with("): enable it in Device Manager."), "{text}");
        let text = SetupError::Checks(vec!["a".into(), "b".into()]).to_string();
        assert_eq!(text, "done, but: a; b");
    }
}
