//! The sequences `update` and the migration run (TODO 19, the per-cable design, section 9): what is checked, disabled, waited
//! for, removed, installed, enabled and made, in which order, and what is put back when a step fails; and those of `disable`
//! and `enable` ([`run_disable`], [`run_enable`]) and of `add`'s repair in place ([`reinstall`]). They go through [`Ops`], the
//! seam between them and Windows ([`Real`]: SetupAPI, the configuration manager, the service control manager and the store),
//! so the tests run them against a fake and check the calls.
//!
//! The rules they keep:
//! - **Nothing changes until every check has passed:** the package's driver for each present cable, and (the migration) that
//!   the package makes each cable to be made, on a scratch element that is never registered.
//! - **No run leaves a node disabled.** A veto, a failed disable or the image still loaded after [`super::service::UNLOAD_WAIT`]
//!   enables what the run disabled, and nothing is installed. A node an earlier run disabled (not persistently: Ctrl+C, a
//!   killed tool) counts as this run's. After the image has unloaded, a step that fails (the migration's removal of an old
//!   node) enables every node it disabled and didn't remove; an install that fails still enables its node.
//! - **The migration converges.** The cables it is to make go to the store, pending, before anything is disabled; a run that
//!   stops later (a failure, a restart Windows needs, a killed tool) leaves them there, and the next run makes whatever is still
//!   missing. One it made and didn't record as made (stopped in between, or made by a run that ends needing a restart) is
//!   present with its record pending: the next run checks it (its identity on one node, working on the package, its endpoints
//!   there) and records it. A later run with the old driver's nodes gone and every cable on the package already disables and
//!   installs nothing.
//! - **Its endpoint map is whole at the end.** A cable's record keeps the old nodes it replaces once it is made. Every run maps
//!   the cables it makes or checks, and every present cable recorded as made whose record has them, however the run ends (a
//!   veto, a failure, a restart); the run that leaves no record pending drops them once its map is published
//!   ([`drop_replaces`]), so that run's map holds the whole migration, whatever its earlier attempts were. A drop that stops
//!   part way loses nothing: each record keeps them as `Replaced` ([`super::store`]), and while any keeps Replaces every run
//!   maps the cables of both ([`super::plan::mapped`]). A run that leaves a record pending may map less (a cable it couldn't
//!   make, or stopped before making); its `result` is then `error` or `restart`.
//! - **A cable is pending until Windows has made its endpoints.** One made now is recorded as made only once every endpoint of
//!   its microphones is there; one Windows needs a restart for stays pending, and the run after the restart checks it.
//! - **A restart Windows needs stops the run** (exit 7): an install's, a node's that needs one to start, or the removal of a
//!   node that failed to install; nothing more is made before it.

use windows::Win32::Devices::DeviceAndDriverInstallation::CM_PROB_NEED_RESTART;

use super::devices::{Bound, DRIVER_WAIT, Disabled, Installed, NewNode, judge_install};
use super::endpoint_map::{self, Gathered};
use super::endpoints::{self, ENDPOINTS_WAIT, Endpoint, EndpointCheck, Names};
use super::package::Candidate;
use super::plan::{Create, Entry, Finish, Health, MigratePlan, Node, PROBLEM_DISABLED, StatePlan, UpdatePlan};
use super::{
    Done, Made, NotInstalled, Present, SetupError, SystemClock, Want, choose_driver, entry_of, lacks, make_or_next, service, store,
};
use crate::identity::{CableInstance, Label, same_instance};
use crate::variant::Variant;
use crate::win_error::WinError;

/// What the sequences do to Windows, by index into the present nodes. [`Real`] is Windows; tests fake it.
pub trait Ops {
    /// A driver node from a node's compatible-driver list.
    type Driver: Clone;
    /// Present node `i`'s driver from the staged package (`published`), and its candidate; refused if the package isn't among
    /// the node's compatible drivers or lacks its variant's microphones. Changes nothing.
    fn staged_driver(&mut self, i: usize, published: &str, say: &mut dyn FnMut(&str)) -> Result<(Self::Driver, Candidate), SetupError>;
    /// Whether the staged package makes cable `label` with `variant`: its compatible drivers on a scratch element with the
    /// variant's hardware ID, never registered. Changes nothing.
    fn check_target(&mut self, label: &Label, variant: Variant, published: &str, say: &mut dyn FnMut(&str)) -> Result<(), SetupError>;
    /// CM_Disable_DevNode, polite and not persistent.
    fn disable(&mut self, i: usize) -> Disabled;
    fn enable(&mut self, i: usize) -> Result<(), WinError>;
    /// `disable` and `enable`'s persistent, global change (DIF_PROPERTYCHANGE: [`super::devices::DeviceSet::change_state`]);
    /// whether Windows needs a restart to finish it.
    fn change_state(&mut self, i: usize, enable: bool) -> Result<bool, WinError>;
    /// Node `i`'s state now, without waiting.
    fn bound(&mut self, i: usize) -> Bound;
    /// D1's start count deleted; whether there was one.
    fn clear_count(&mut self, i: usize) -> Result<bool, WinError>;
    /// DiInstallDevice; whether Windows asked for a restart.
    fn install(&mut self, i: usize, driver: &Self::Driver) -> Result<bool, WinError>;
    fn set_friendly_name(&mut self, i: usize, name: &str) -> Result<(), WinError>;
    /// DiUninstallDevice; whether Windows needs a restart to finish.
    fn uninstall(&mut self, i: usize) -> Result<bool, WinError>;
    /// Up to [`DRIVER_WAIT`] for node `i` to work; its state then.
    fn wait_working(&mut self, i: usize) -> Bound;
    /// Up to [`service::UNLOAD_WAIT`] for the driver's service to stop.
    fn wait_unloaded(&mut self) -> service::Unload;
    /// The endpoints before the run, NOTPRESENT ones too, and those of the nodes `also` names ([`endpoints::before`]).
    fn endpoints_before(&mut self, also: &[String]) -> Result<Vec<Endpoint>, WinError>;
    /// Up to [`ENDPOINTS_WAIT`] for a cable's endpoints ([`endpoints::wait_for`]).
    fn wait_endpoints(&mut self, cable: &CableInstance, variant: Variant, names: Names) -> (Option<EndpointCheck>, Option<WinError>);
    /// A new cable's node on the package `want` names ([`super::make_or_next`]).
    fn make(&mut self, create: &Create, want: Want, say: &mut dyn FnMut(&str)) -> Result<Made, SetupError>;
    /// The store's entry for a cable.
    fn record(&mut self, entry: &Entry) -> Result<(), WinError>;
}

/// Windows, over the present nodes read at the start of the run.
pub struct Real<'a> {
    pub present: &'a Present,
}

impl Ops for Real<'_> {
    type Driver = windows::Win32::Devices::DeviceAndDriverInstallation::SP_DRVINFO_DATA_V2_W;

    fn staged_driver(&mut self, i: usize, published: &str, say: &mut dyn FnMut(&str)) -> Result<(Self::Driver, Candidate), SetupError> {
        let n = &self.present.nodes[i];
        let mut data = self.present.data[i];
        let drivers = self.present.set.compatible(&mut data)?;
        let hw = n.model.hardware_id();
        let chosen = choose_driver(&drivers, Want::Published(published), hw, &n.instance_id, say)?;
        let (driver, candidate) = drivers[chosen].clone();
        if let Some(v) = n.variant() {
            lacks(&candidate, v, hw)?;
        }
        Ok((driver, candidate))
    }

    fn check_target(&mut self, label: &Label, variant: Variant, published: &str, say: &mut dyn FnMut(&str)) -> Result<(), SetupError> {
        let hw = variant.hardware_id();
        // Dropped at the end, which deletes the element: it was never registered
        let (set, mut data) = NewNode::scratch(hw)?;
        let drivers = set.compatible(&mut data)?;
        let chosen = choose_driver(&drivers, Want::Published(published), hw, &format!("the new {}", label.friendly_name()), say)?;
        lacks(&drivers[chosen].1, variant, hw)
    }

    fn disable(&mut self, i: usize) -> Disabled {
        self.present.set.disable(&self.present.data[i])
    }

    fn enable(&mut self, i: usize) -> Result<(), WinError> {
        self.present.set.enable(&self.present.data[i])
    }

    fn change_state(&mut self, i: usize, enable: bool) -> Result<bool, WinError> {
        self.present.set.change_state(&self.present.data[i], enable)
    }

    fn bound(&mut self, i: usize) -> Bound {
        self.present.set.bound(&self.present.data[i])
    }

    fn clear_count(&mut self, i: usize) -> Result<bool, WinError> {
        self.present.set.clear_start_attempts(&self.present.data[i])
    }

    fn install(&mut self, i: usize, driver: &Self::Driver) -> Result<bool, WinError> {
        self.present.set.install(&self.present.data[i], driver)
    }

    fn set_friendly_name(&mut self, i: usize, name: &str) -> Result<(), WinError> {
        let mut data = self.present.data[i];
        self.present.set.set_friendly_name(&mut data, name)
    }

    fn uninstall(&mut self, i: usize) -> Result<bool, WinError> {
        self.present.set.uninstall(&self.present.data[i])
    }

    fn wait_working(&mut self, i: usize) -> Bound {
        self.present.set.wait_working(&self.present.data[i], DRIVER_WAIT)
    }

    fn wait_unloaded(&mut self) -> service::Unload {
        service::wait_unloaded(service::UNLOAD_WAIT, &mut || service::state(service::SERVICE), &mut SystemClock::start())
    }

    fn endpoints_before(&mut self, also: &[String]) -> Result<Vec<Endpoint>, WinError> {
        endpoints::before(also)
    }

    fn wait_endpoints(&mut self, cable: &CableInstance, variant: Variant, names: Names) -> (Option<EndpointCheck>, Option<WinError>) {
        endpoints::wait_for(cable, variant, names, ENDPOINTS_WAIT, &mut endpoints::all, &mut SystemClock::start())
    }

    fn make(&mut self, create: &Create, want: Want, say: &mut dyn FnMut(&str)) -> Result<Made, SetupError> {
        make_or_next(self.present, create, want, say)
    }

    fn record(&mut self, entry: &Entry) -> Result<(), WinError> {
        store::write(entry)
    }
}

/// The endpoints before the run (with those of the nodes `also` names), for their names afterwards; `None` if they don't read
/// (said: then every name is checked against the expected one).
fn endpoints_before<O: Ops>(ops: &mut O, also: &[String], say: &mut dyn FnMut(&str)) -> Option<Vec<Endpoint>> {
    ops.endpoints_before(also)
        .inspect_err(|e| {
            say(&format!("  note: the endpoints couldn't be read before the run ({e}): their names are checked against the expected ones"))
        })
        .ok()
}

/// What a wait for a cable's endpoints found, each said; the checks that failed.
pub(super) fn endpoint_problems(
    cable: &CableInstance, (check, failed): (Option<EndpointCheck>, Option<WinError>), say: &mut dyn FnMut(&str),
) -> Vec<String> {
    let mut problems = Vec::new();
    match &check {
        Some(c) => {
            for (kind, e) in &c.found {
                let state = if e.state == endpoints::State::Active { String::new() } else { format!(" ({:?})", e.state) };
                say(&format!("  {}: \"{}\"{state}", kind.name(), e.name));
            }
            if !c.is_done() {
                problems.push(format!("{}: {c}", cable.label.friendly_name()));
            }
        }
        None => problems.push(format!("{}'s endpoints couldn't be read", cable.label.friendly_name())),
    }
    if let (Some(e), Some(c)) = (&failed, &check)
        && !c.is_done()
    {
        problems.push(format!("the last look at the endpoints failed: {e}"));
    }
    problems
}

fn wait_endpoints<O: Ops>(ops: &mut O, cable: &CableInstance, variant: Variant, names: Names, say: &mut dyn FnMut(&str)) -> Vec<String> {
    say(&format!("Waiting for Windows to make {}'s endpoints (up to {} s)", cable.label.friendly_name(), ENDPOINTS_WAIT.as_secs()));
    let found = ops.wait_endpoints(cable, variant, names);
    endpoint_problems(cable, found, say)
}

/// D1's start count deleted, said; a failure is said and costs nothing else (the driver would then refuse the node, which the
/// check after names).
fn clear_count<O: Ops>(ops: &mut O, nodes: &[Node], i: usize, say: &mut dyn FnMut(&str)) {
    match ops.clear_count(i) {
        Ok(true) => say(&format!("  cleared {}'s start count ({})", nodes[i].instance_id, super::devices::START_ATTEMPTS_VALUE)),
        Ok(false) => {}
        Err(e) => {
            say(&format!("  warning: {}'s start count couldn't be cleared ({e}): the driver may refuse it (Code 31)", nodes[i].instance_id))
        }
    }
}

/// Enables `which` again, said; those that wouldn't.
fn restore<O: Ops>(ops: &mut O, nodes: &[Node], which: &[usize], say: &mut dyn FnMut(&str)) -> Vec<(String, WinError)> {
    let mut failed = Vec::new();
    for &i in which {
        say(&format!("  enabling {} again", nodes[i].instance_id));
        if let Err(e) = ops.enable(i) {
            failed.push((nodes[i].instance_id.clone(), e));
        }
    }
    failed
}

/// Each node `which` given the staged package: its driver node and candidate, found before anything is disabled.
fn staged<O: Ops>(
    ops: &mut O, which: &[usize], published: &str, say: &mut dyn FnMut(&str),
) -> Result<Vec<(usize, O::Driver, Candidate)>, SetupError> {
    which.iter().map(|&i| ops.staged_driver(i, published, say).map(|(d, c)| (i, d, c))).collect()
}

/// Disables `disable` (polite, no persistence) and waits for the driver's image to unload; `reenable`, disabled by an earlier
/// run, counts as disabled by this one. On a veto, a failure or the timeout, enables every node it counts and stops: nothing
/// is installed. Returns the nodes to enable again.
pub fn unload<O: Ops>(
    ops: &mut O, nodes: &[Node], disable: &[usize], reenable: &[usize], say: &mut dyn FnMut(&str),
) -> Result<Vec<usize>, SetupError> {
    let mut disabled = reenable.to_vec();
    for &i in reenable {
        say(&format!("{} is disabled already, not persistently (an earlier run's, stopped): enabled with the rest", nodes[i].instance_id));
    }
    for &i in disable {
        let n = &nodes[i];
        say(&format!("Disabling {} (CM_Disable_DevNode, polite, not persistent)", n.instance_id));
        match ops.disable(i) {
            Disabled::Done => disabled.push(i),
            d => {
                let unrestored = restore(ops, nodes, &disabled, say);
                let (vetoed, error) = match d {
                    Disabled::Vetoed => (true, None),
                    Disabled::Failed(e) => (false, Some(e)),
                    Disabled::Done => unreachable!(),
                };
                return Err(SetupError::Disable { device: n.instance_id.clone(), vetoed, error, unrestored });
            }
        }
    }
    say(&format!(
        "Waiting for the driver's image to unload (the {} service, up to {} s)",
        service::SERVICE,
        service::UNLOAD_WAIT.as_secs()
    ));
    match ops.wait_unloaded() {
        service::Unload::Done(after) => {
            say(&format!("  unloaded after {:.1} s", after.as_secs_f64()));
            Ok(disabled)
        }
        stuck => {
            let last = match stuck {
                service::Unload::StillLoaded(s) => format!("{s:?}"),
                service::Unload::Unknown(e) => format!("unread, {e}"),
                service::Unload::Done(_) => unreachable!(),
            };
            let unrestored = restore(ops, nodes, &disabled, say);
            Err(SetupError::StillLoaded { last, unrestored })
        }
    }
}

/// Installs the staged package on each node of `drivers`, enables every node of `disabled` but the old driver's (whatever the
/// installs did), and checks each: working on the package, its endpoints there and named as `before` had them. The checks that
/// failed, and a node Windows needs a restart to finish (its install asked for one, or it shows Code 14 on the package).
pub fn install_all<O: Ops>(
    ops: &mut O, nodes: &[Node], drivers: &[(usize, O::Driver, Candidate)], disabled: &[usize], before: &[Endpoint],
    say: &mut dyn FnMut(&str),
) -> (Vec<String>, Option<String>) {
    let mut problems = Vec::new();
    let mut restart = None;
    for (i, driver, candidate) in drivers {
        let n = &nodes[*i];
        clear_count(ops, nodes, *i, say);
        say(&format!("Installing {} on {} (DiInstallDevice)", candidate.inf, n.instance_id));
        match ops.install(*i, driver) {
            Ok(true) => restart = Some(n.instance_id.clone()),
            Ok(false) => {}
            Err(e) => problems.push(format!("{}: {e}", n.instance_id)),
        }
        rename(ops, n, *i, say);
    }
    for &i in disabled.iter().filter(|&&i| !nodes[i].is_legacy()) {
        clear_count(ops, nodes, i, say);
        say(&format!("Enabling {}", nodes[i].instance_id));
        if let Err(e) = ops.enable(i) {
            problems.push(format!("{} couldn't be enabled again ({e}): enable it in Device Manager", nodes[i].instance_id));
        }
    }
    for (i, _, candidate) in drivers {
        let n = &nodes[*i];
        if !disabled.contains(i) {
            say(&format!("  {} was disabled before (by the user): left disabled, on the new package", n.instance_id));
            continue;
        }
        let now = ops.wait_working(*i);
        say(&format!("  {}: {}", n.instance_id, now.describe()));
        match judge_install(Ok(()), false, &now, candidate) {
            Ok(Installed::Working) => {
                if let (Some(c), Some(v)) = (&n.cable, n.variant()) {
                    problems.extend(wait_endpoints(ops, c, v, Names::Since(before), say));
                }
            }
            Ok(Installed::Restart) => {
                say("  Windows needs a restart to finish it: its endpoints come after it");
                restart.get_or_insert_with(|| n.instance_id.clone());
            }
            Err(failure) => problems.push(format!("{}: {failure}", n.instance_id)),
        }
    }
    (problems, restart)
}

/// `update`, once the package is staged and the plan made: the checks, the unload, the installs (the module's docs).
pub fn run_update<O: Ops>(
    ops: &mut O, nodes: &[Node], plan: &UpdatePlan, published: &str, say: &mut dyn FnMut(&str),
) -> Result<Done, SetupError> {
    let drivers = staged(ops, &plan.install, published, say)?;
    let before = endpoints_before(ops, &[], say).unwrap_or_default();
    let disabled = unload(ops, nodes, &plan.disable, &plan.reenable, say)?;
    let (problems, restart) = install_all(ops, nodes, &drivers, &disabled, &before, say);
    ended(restart, problems)
}

/// A run that changed something, by what is left: a node Windows needs a restart to finish (`restart`) and the checks that
/// failed (`problems`).
fn ended(restart: Option<String>, problems: Vec<String>) -> Result<Done, SetupError> {
    match restart {
        Some(device) if problems.is_empty() => Ok(Done::RestartPending { device }),
        // The restart first (exit 7): the run after it says what is still wrong
        Some(device) => Err(SetupError::Checks(problems).and_restart(Some(device))),
        None if problems.is_empty() => Ok(Done::Changed),
        None => Err(SetupError::Checks(problems)),
    }
}

/// A present cable's node `i` given the package again in place (`add`'s repair, [`super::repair`], once its driver is
/// chosen): D1's start count cleared, its friendly name re-applied, DiInstallDevice with `driver`, the node enabled if it is
/// disabled (before the install or after it: CM_Enable_DevNode, or for a persistent disable the global change that clears
/// it), then up to [`DRIVER_WAIT`] for it to work. What it came to ([`judge_install`]): working on `candidate`'s package, or a
/// restart Windows needs to finish. A restart the install asked for is kept whatever fails after it (Astra's review of the
/// first install, 2026-11-26): an enable that fails then is an error that needs the restart first (exit 7).
pub fn reinstall<O: Ops>(
    ops: &mut O, nodes: &[Node], i: usize, driver: &O::Driver, candidate: &Candidate, say: &mut dyn FnMut(&str),
) -> Result<Installed, SetupError> {
    let n = &nodes[i];
    clear_count(ops, nodes, i, say);
    rename(ops, n, i, say);
    say(&format!("Installing {} on {} (DiInstallDevice)", candidate.inf, n.instance_id));
    let call = ops.install(i, driver);
    let mut restart = matches!(call, Ok(true));
    if n.health == Health::Problem(PROBLEM_DISABLED) || ops.bound(i).health == Health::Problem(PROBLEM_DISABLED) {
        clear_count(ops, nodes, i, say);
        say("  enabling it");
        let enabled = if n.disabled_by_user() { ops.change_state(i, true).map(|r| restart |= r) } else { ops.enable(i) };
        if let Err(e) = enabled {
            return Err(SetupError::Windows(e).and_restart(restart.then(|| n.instance_id.clone())));
        }
    }
    let now = if restart { ops.bound(i) } else { ops.wait_working(i) };
    say(&format!("  {}: {}", n.instance_id, now.describe()));
    judge_install(call.as_ref().map(drop), restart, &now, candidate).map_err(|failure| {
        let not = NotInstalled { device: n.instance_id.clone(), chosen: candidate.describe(), failure, removed: None };
        SetupError::Install(Box::new(not))
    })
}

/// A cable node's friendly name re-applied, said if it fails.
fn rename<O: Ops>(ops: &mut O, n: &Node, i: usize, say: &mut dyn FnMut(&str)) {
    if let Some(c) = &n.cable
        && let Err(e) = ops.set_friendly_name(i, &c.label.friendly_name())
    {
        say(&format!("  warning: the friendly name \"{}\" couldn't be set again: {e}", c.label.friendly_name()));
    }
}

/// `disable <label>`, its plan made (the module's docs, [`super::disable`]): each node of `plan.change` disabled for good
/// (DIF_PROPERTYCHANGE, DICS_DISABLE in every hardware profile), so the cable stays off across restarts until `enable`. Nothing
/// is uninstalled, so its endpoints keep their ids (Windows shows them as not present while it is off). No other cable is
/// touched. A node Windows can't stop now is disabled at the next restart ([`Done::RestartPending`]: exit 7).
pub fn run_disable<O: Ops>(ops: &mut O, nodes: &[Node], plan: &StatePlan, say: &mut dyn FnMut(&str)) -> Result<Done, SetupError> {
    let name = plan.label.friendly_name();
    if plan.change.is_empty() {
        for &i in &plan.left {
            say(&format!("{name} is disabled already: {}", nodes[i].describe()));
        }
        return Ok(Done::Nothing);
    }
    let mut restart = None;
    let mut problems = Vec::new();
    for &i in &plan.change {
        let n = &nodes[i];
        say(&format!(
            "Disabling {} (DIF_PROPERTYCHANGE, DICS_DISABLE in every hardware profile: it stays off across restarts)",
            n.instance_id
        ));
        match ops.change_state(i, false) {
            Ok(true) => {
                say("  Windows finishes disabling it when it restarts (it couldn't stop the device now: something has it in use)");
                restart.get_or_insert_with(|| n.instance_id.clone());
            }
            Ok(false) => {
                let now = ops.bound(i);
                say(&format!("  {}: {}", n.instance_id, now.describe()));
                if now.health != Health::Problem(PROBLEM_DISABLED) {
                    problems.push(format!("{} isn't disabled after the disable: {}", n.instance_id, now.health.describe()));
                }
            }
            Err(e) => return Err(SetupError::Windows(e).and_restart(restart)),
        }
    }
    if restart.is_none() && problems.is_empty() {
        say(&format!(
            "{name} is off, across restarts too; its endpoints keep their ids (not present while it is off): `acx-ll-cable enable {}` \
             turns it back on",
            plan.label
        ));
    }
    ended(restart, problems)
}

/// `enable <label>`, its plan made ([`super::enable`]): the cable's node enabled (DIF_PROPERTYCHANGE, DICS_ENABLE in every
/// hardware profile, D1's start count cleared first), then up to [`DRIVER_WAIT`] for it to work and [`ENDPOINTS_WAIT`] for its
/// endpoints, each checked as an update checks them and against the endpoints before the enable: one Windows brought back with
/// an id it didn't have before ([`new_ids`]) is a failed check, since the cable is meant to keep its ids. No other cable is
/// touched. Windows needing a restart to finish it is [`Done::RestartPending`] (exit 7).
pub fn run_enable<O: Ops>(ops: &mut O, nodes: &[Node], plan: &StatePlan, say: &mut dyn FnMut(&str)) -> Result<Done, SetupError> {
    let name = plan.label.friendly_name();
    if plan.change.is_empty() {
        for &i in &plan.left {
            say(&format!("{name} isn't disabled: {}", nodes[i].describe()));
        }
        return Ok(Done::Nothing);
    }
    // The endpoints it had (NOTPRESENT while it is off), whose ids and names it keeps
    let before = ops
        .endpoints_before(&[])
        .inspect_err(|e| say(&format!("  note: the endpoints couldn't be read before the enable ({e}): their ids aren't compared")))
        .ok();
    let mut restart = None;
    let mut problems = Vec::new();
    for &i in &plan.change {
        let n = &nodes[i];
        clear_count(ops, nodes, i, say);
        say(&format!("Enabling {} (DIF_PROPERTYCHANGE, DICS_ENABLE in every hardware profile)", n.instance_id));
        match ops.change_state(i, true) {
            Ok(false) => {}
            Ok(true) => {
                say("  Windows needs a restart to finish enabling it: its endpoints come after it");
                restart.get_or_insert_with(|| n.instance_id.clone());
                continue;
            }
            Err(e) => return Err(SetupError::Windows(e).and_restart(restart)),
        }
        let now = ops.wait_working(i);
        say(&format!("  {}: {}", n.instance_id, now.describe()));
        match now.health {
            Health::Working => {
                let (Some(cable), Some(variant)) = (&n.cable, n.variant()) else { continue };
                say(&format!("Waiting for Windows to bring back {}'s endpoints (up to {} s)", name, ENDPOINTS_WAIT.as_secs()));
                let found = ops.wait_endpoints(cable, variant, Names::Since(before.as_deref().unwrap_or_default()));
                if let (Some(before), Some(check)) = (&before, &found.0) {
                    problems.extend(new_ids(cable, before, check));
                }
                problems.extend(endpoint_problems(cable, found, say));
            }
            Health::Problem(p) if p == CM_PROB_NEED_RESTART.0 => {
                say("  Windows needs a restart to start it: its endpoints come after it");
                restart.get_or_insert_with(|| n.instance_id.clone());
            }
            _ => problems.push(format!(
                "{} was enabled, but isn't working ({}): `acx-ll-cable add {}` repairs it",
                n.instance_id,
                now.describe(),
                plan.label
            )),
        }
    }
    ended(restart, problems)
}

/// The endpoints an enable brought back that Windows didn't have before it (`before`, NOTPRESENT ones included: a disabled
/// cable's), by MMDevice id: each a problem, since a disable is meant to keep them (an app that saved the old id finds the new
/// one by the cable and role: [`crate::roles`]). Pure.
pub fn new_ids(cable: &CableInstance, before: &[Endpoint], check: &EndpointCheck) -> Vec<String> {
    check
        .found
        .iter()
        .filter(|(_, e)| !before.iter().any(|b| b.id == e.id))
        .map(|(kind, _)| {
            format!(
                "{}'s {} came back as a new endpoint, not the one it had (an app finds it by the cable and role)",
                cable.label.friendly_name(),
                kind.name()
            )
        })
        .collect()
}

/// Nothing for the migration's unload to do: no node of the old driver's, none an earlier run left disabled, and every cable on
/// the staged package already, working or disabled by the user (a later run, after the old driver's nodes went). Then nothing
/// is disabled or installed: the cables to make go on the package the cables run, and those to finish are checked as they are.
fn settled<D>(nodes: &[Node], plan: &MigratePlan, drivers: &[(usize, D, Candidate)]) -> bool {
    plan.legacy.is_empty()
        && plan.update.reenable.is_empty()
        && drivers.iter().all(|(i, _, c)| {
            let n = &nodes[*i];
            n.running.as_ref().is_some_and(|r| r.runs(c)) && (n.healthy() || n.disabled_by_user())
        })
}

/// What [`check_finish`] found of a cable an earlier run made: recorded as made, or left pending.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Finished {
    /// Its identity on one node, working on the package, every endpoint of its microphones there: recorded. `misnamed`: the
    /// endpoints' names that aren't as expected, reported as the first run reports them, which doesn't hold the record back.
    Checked { cable: CableInstance, variant: Variant, misnamed: Vec<String> },
    /// Left pending: why.
    Pending(Vec<String>),
    /// Left pending: on the package, but Windows needs a restart to start it (Code 14), so the run stops there (exit 7).
    Restart(String),
}

/// What a wait for the endpoints of a cable to finish found (each said): `Ok` with the names that aren't as expected (reported as
/// for a cable made now, not holding its record back, so a retry can't stick on a name) once every endpoint of its microphones
/// is there; `Err` with why not. The migration's check ([`check_finish`]) and `add`'s of a migration's cable.
pub(crate) fn finish_verdict(
    cable: &CableInstance, found: (Option<EndpointCheck>, Option<WinError>), say: &mut dyn FnMut(&str),
) -> Result<Vec<String>, Vec<String>> {
    let there = found.0.as_ref().is_some_and(|c| c.missing.is_empty());
    let problems = endpoint_problems(cable, found, say);
    if there { Ok(problems) } else { Err(problems) }
}

/// A cable an earlier run made and didn't record ([`Finish`]): its identity on one node (the plan found it by its identity, so
/// its label and id are the record's), working on the package (`drivers`' candidate for it), its endpoints there.
fn check_finish<O: Ops>(
    ops: &mut O, nodes: &[Node], f: &Finish, drivers: &[(usize, O::Driver, Candidate)], before: &[Endpoint], say: &mut dyn FnMut(&str),
) -> Finished {
    let name = f.label.friendly_name();
    let [i] = f.nodes.as_slice() else {
        let ids: Vec<&str> = f.nodes.iter().map(|&i| nodes[i].instance_id.as_str()).collect();
        return Finished::Pending(vec![format!(
            "{name}'s identity is on {} nodes ({}): `acx-ll-cable add {}` repairs it, then run this again",
            ids.len(),
            ids.join(", "),
            f.label
        )]);
    };
    let n = &nodes[*i];
    let (Some(cable), Some(variant)) = (n.cable.clone(), n.variant()) else {
        return Finished::Pending(vec![format!("{}: not a cable's node", n.instance_id)]);
    };
    let Some((_, _, candidate)) = drivers.iter().find(|(j, ..)| j == i) else {
        return Finished::Pending(vec![format!("{name}: no driver of the package was found for it")]);
    };
    say(&format!("Checking {name} ({}): an earlier run made it and stopped before recording it", n.instance_id));
    let now = ops.wait_working(*i);
    say(&format!("  {}: {}", n.instance_id, now.describe()));
    match judge_install(Ok(()), false, &now, candidate) {
        Ok(Installed::Working) => {}
        Ok(Installed::Restart) => return Finished::Restart(n.instance_id.clone()),
        Err(failure) => return Finished::Pending(vec![format!("{}: {failure}", n.instance_id)]),
    }
    say(&format!("Waiting for Windows to make {name}'s endpoints (up to {} s)", ENDPOINTS_WAIT.as_secs()));
    let found = ops.wait_endpoints(&cable, variant, Names::Since(before));
    match finish_verdict(&cable, found, say) {
        Ok(misnamed) => Finished::Checked { cable, variant, misnamed },
        Err(problems) => Finished::Pending(problems),
    }
}

/// The migration, once the package is staged and the plan made (the module's docs): every check; the cables to make to the
/// store, pending; the old image unloaded; the old driver's nodes removed; the present cables given the package and enabled
/// (none of it when [`settled`]); the cables an earlier run made checked and recorded; the cables made, each recorded as made
/// (pending still if Windows needs a restart to finish it). `gathered` takes the endpoints before the run and the cables made,
/// finished, or made by an earlier run with their map not final ([`MigratePlan::mapped`]), for the endpoint map; the caller
/// writes it, then [`drop_replaces`]. Those an earlier run made go in however the run ends (a veto, a removal that fails, a
/// restart), so a run that leaves no record pending, and so may drop the old nodes' names once its map is published, maps
/// every one of them.
pub fn run_migrate<O: Ops>(
    ops: &mut O, nodes: &[Node], plan: &MigratePlan, published: &str, gathered: &mut Gathered, say: &mut dyn FnMut(&str),
) -> Result<Done, SetupError> {
    let r = migration(ops, nodes, plan, published, gathered, say);
    mapped_too(nodes, plan, gathered);
    r
}

/// [`run_migrate`]'s sequence, the cables an earlier run made not mapped yet.
fn migration<O: Ops>(
    ops: &mut O, nodes: &[Node], plan: &MigratePlan, published: &str, gathered: &mut Gathered, say: &mut dyn FnMut(&str),
) -> Result<Done, SetupError> {
    // 1. Every check, before anything changes
    let drivers = staged(ops, &plan.update.install, published, say)?;
    for t in &plan.add {
        say(&format!("Checking that the package makes {} ({})", t.label.friendly_name(), t.variant.describe()));
        ops.check_target(&t.label, t.variant, published, say)?;
    }
    // 2. The cables to make, to the store before the old devices go
    for t in &plan.add {
        ops.record(&t.entry(true)).map_err(SetupError::Store)?;
        say(&format!("  {} (id {}, n {}) recorded in the setup's store, to make", t.label.friendly_name(), t.id, t.n));
    }
    let to_make: Vec<Label> = plan.finish.iter().map(|f| f.label.clone()).chain(plan.add.iter().map(|t| t.label.clone())).collect();
    // The endpoints before: the old driver's nodes' too, and those of the nodes an earlier run removed (lingering, NOTPRESENT)
    let mut also: Vec<String> = Vec::new();
    let replaced = plan.add.iter().flat_map(|t| &t.replaces).chain(plan.finish.iter().chain(&plan.mapped).flat_map(|f| &f.replaces));
    for id in plan.legacy.iter().map(|&i| &nodes[i].instance_id).chain(replaced) {
        if !also.iter().any(|a| same_instance(a, id)) {
            also.push(id.clone());
        }
    }
    gathered.before = endpoints_before(ops, &also, say);
    let before = gathered.before.clone().unwrap_or_default();
    let mut problems = Vec::new();
    if settled(nodes, plan, &drivers) {
        say("No device of the old driver's is left and every cable runs the package: nothing to disable or install");
    } else {
        // 3. The old image unloaded: every node of ours off (put back by `unload` itself if it can't be)
        let disabled = unload(ops, nodes, &plan.update.disable, &plan.update.reenable, say)?;
        // 4. The old driver's nodes removed; a removal that fails, or that needs a restart, stops the run with every node it
        //    disabled and didn't remove enabled again, nothing installed
        let mut removed = Vec::new();
        for &i in &plan.legacy {
            say(&format!("Removing {} (DiUninstallDevice)", nodes[i].instance_id));
            let stopped = match ops.uninstall(i) {
                Ok(false) => {
                    say("  removed");
                    removed.push(i);
                    continue;
                }
                Ok(true) => {
                    say("  removed, but Windows needs a restart to finish");
                    removed.push(i);
                    SetupError::RestartFirst { device: nodes[i].instance_id.clone() }
                }
                Err(e) => SetupError::Windows(e),
            };
            let back: Vec<usize> = disabled.iter().copied().filter(|j| !removed.contains(j)).collect();
            let unrestored = restore(ops, nodes, &back, say);
            return Err(SetupError::Stopped { error: Box::new(stopped), unrestored, to_make });
        }
        // 5. The present cables to the package, and every node that is left enabled
        let restart;
        (problems, restart) = install_all(ops, nodes, &drivers, &disabled, &before, say);
        if let Some(device) = restart {
            // The new image can't load until Windows restarts: the cables wait for the next run, recorded
            return restart_due(device, problems, to_make, say);
        }
    }
    // 6. The cables an earlier run made and didn't record: checked, then recorded as made. One whose node doesn't work on the
    //    package, or whose endpoints don't come, stays pending; endpoint names that aren't as expected are reported, as for a
    //    cable made now, and don't hold the record back
    let mut unmade = Vec::new();
    let mut restart = None;
    for f in &plan.finish {
        match check_finish(ops, nodes, f, &drivers, &before, say) {
            Finished::Checked { cable, variant, misnamed } => {
                // The nodes it replaces stay until the migration's map is final
                let entry = entry_of(&cable, variant, &f.replaces, false);
                match ops.record(&entry) {
                    Ok(()) => say(&format!("  {} recorded as made", f.label.friendly_name())),
                    Err(e) => problems.push(format!("{} is made, but the setup's store couldn't record it ({e})", f.label.friendly_name())),
                }
                problems.extend(misnamed);
                gathered.cables.push(endpoint_map::Made::migration(cable, variant, &f.replaces));
            }
            Finished::Pending(why) => {
                problems.extend(why);
                unmade.push(f.label.clone());
            }
            Finished::Restart(device) => {
                say("  Windows needs a restart to start it: it stays pending, and the run after the restart checks it");
                unmade.push(f.label.clone());
                restart.get_or_insert(device);
            }
        }
    }
    if let Some(device) = restart {
        // Nothing is made before the restart Windows needs
        let rest: Vec<Label> = unmade.iter().chain(plan.add.iter().map(|t| &t.label)).cloned().collect();
        return restart_due(device, problems, rest, say);
    }
    // 7. The cables made; one that fails stays pending in the store, and the others are still made, but for a restart Windows
    //    needs (the failed node's removal, say): then the run stops there, nothing more made (exit 7)
    for (k, t) in plan.add.iter().enumerate() {
        say(&format!("Adding {} ({})", t.label.friendly_name(), t.variant.describe()));
        let made = match ops.make(&t.create(), Want::Published(published), say) {
            Ok(m) => m,
            Err(e) => {
                problems.push(format!("{}: {e}", t.label.friendly_name()));
                unmade.push(t.label.clone());
                if let Some(device) = e.restart_device() {
                    say("  Windows needs a restart to finish with it: nothing more is made before then");
                    let rest: Vec<Label> = unmade.iter().chain(plan.add[k + 1..].iter().map(|t| &t.label)).cloned().collect();
                    return restart_due(device.to_string(), problems, rest, say);
                }
                continue;
            }
        };
        gathered.cables.push(endpoint_map::Made::migration(made.cable.clone(), t.variant, &t.replaces));
        let not_recorded = |e| format!("{} was made, but the setup's store couldn't record it ({e})", t.label.friendly_name());
        if made.restart {
            // Pending still, its n recorded, so the run after the restart checks it
            if let Err(e) = ops.record(&Entry { n: made.cable.n, ..t.entry(true) }) {
                problems.push(not_recorded(e));
            }
            let rest: Vec<Label> = unmade.iter().chain(plan.add[k..].iter().map(|t| &t.label)).cloned().collect();
            return restart_due(made.cable.instance_id(), problems, rest, say);
        }
        // Recorded as made (its n, the old nodes it replaces kept) only once every endpoint of its microphones is there; else
        // it stays pending, for the next run's check
        say(&format!("Waiting for Windows to make {}'s endpoints (up to {} s)", t.label.friendly_name(), ENDPOINTS_WAIT.as_secs()));
        let found = ops.wait_endpoints(&made.cable, t.variant, Names::Since(&before));
        match finish_verdict(&made.cable, found, say) {
            Ok(misnamed) => {
                if let Err(e) = ops.record(&Entry { n: made.cable.n, ..t.entry(false) }) {
                    problems.push(not_recorded(e));
                }
                problems.extend(misnamed);
            }
            Err(why) => {
                say("  it stays pending in the setup's store: the next run checks it");
                problems.extend(why);
                unmade.push(t.label.clone());
                if made.cable.n != t.n
                    && let Err(e) = ops.record(&Entry { n: made.cable.n, ..t.entry(true) })
                {
                    problems.push(not_recorded(e));
                }
            }
        }
    }
    // A cable to finish is checked by the installs and again by its own check: each problem once
    let mut seen = std::collections::BTreeSet::new();
    problems.retain(|p| seen.insert(p.clone()));
    match (unmade.is_empty(), problems.is_empty()) {
        (true, true) => Ok(Done::Changed),
        (true, false) => Err(SetupError::Checks(problems)),
        (false, _) => Err(SetupError::Unfinished { problems, to_make: unmade }),
    }
}

/// The cables an earlier run made whose map isn't final ([`MigratePlan::mapped`]), as their newest nodes have them, into the
/// endpoint map's input.
fn mapped_too(nodes: &[Node], plan: &MigratePlan, gathered: &mut Gathered) {
    for m in &plan.mapped {
        let n = &nodes[m.nodes[0]];
        if let (Some(c), Some(v)) = (&n.cable, n.variant()) {
            gathered.cables.push(endpoint_map::Made::migration(c.clone(), v, &m.replaces));
        }
    }
}

/// The migration's end when Windows needs a restart to finish with `device` (an install, a node's start, a failed node's
/// removal): the cables still `to_make` wait for the next run (recorded in the store, pending), which it says. A restart and
/// nothing else wrong is [`Done::RestartPending`]; with other problems, an error that needs the restart first
/// ([`SetupError::needs_restart`]: exit 7).
fn restart_due(device: String, problems: Vec<String>, to_make: Vec<Label>, say: &mut dyn FnMut(&str)) -> Result<Done, SetupError> {
    let text = format!("Windows needs a restart to finish with {device}");
    if to_make.is_empty() {
        say(&format!("  {text}"));
    } else {
        say(&format!("  {text}; then run this again (Install-Driver.ps1) to finish {}, recorded in the setup's store", labels(&to_make)));
    }
    if problems.is_empty() {
        return Ok(Done::RestartPending { device });
    }
    let error = if to_make.is_empty() { SetupError::Checks(problems) } else { SetupError::Unfinished { problems, to_make } };
    Err(error.and_restart(Some(device)))
}

/// Once no record in `entries` (the store after the run) is pending, every record's Replaces dropped: the migration is done, and
/// the run's endpoint map, published before this, holds every cable they name. A record at a time: each keeps them as Replaced
/// ([`super::store::write`]), so a drop that stops part way leaves the next migration run mapping every cable of both (and
/// dropping the rest). A write that fails is said; that record keeps them, and the next migration run maps it again and drops
/// them.
pub fn drop_replaces<O: Ops>(ops: &mut O, entries: &[Entry], say: &mut dyn FnMut(&str)) {
    if entries.iter().any(|e| e.pending) {
        return;
    }
    for e in entries.iter().filter(|e| !e.replaces.is_empty()) {
        match ops.record(&Entry { replaces: Vec::new(), ..e.clone() }) {
            Ok(()) => say(&format!(
                "  {}: the migration is done; the old devices it replaced are dropped from its record",
                e.label.friendly_name()
            )),
            Err(err) => say(&format!(
                "  warning: {}'s record keeps the old devices it replaced ({err}): the next migration maps it again",
                e.label.friendly_name()
            )),
        }
    }
}

/// How [`make_recorded`] records the cable it makes: the nodes its record says it replaces (a migration's cable: kept as they
/// are, until the migration's map is final), and whether its record is still a migration's not finished (pending).
#[derive(Clone, Copy, Debug, Default)]
pub struct Recording<'a> {
    pub replaces: &'a [String],
    pub pending: bool,
}

/// A cable `add` or the variant switch makes (`create`, on the package `want`), recorded, its endpoints waited for (up to
/// [`ENDPOINTS_WAIT`], their names judged by `names`). The cable made, if it was, beside the result:
/// - the make fails: its error as it is, nothing recorded, nothing waited for (a restart Windows needs to finish removing a node
///   that failed to install among them: exit 7);
/// - made, and Windows needs a restart to finish it: recorded (a migration's cable still pending, so the run after the restart
///   checks it), [`Done::RestartPending`] (exit 7), its endpoints unchecked;
/// - made: a migration's cable (`rec.pending`) is recorded pending first (with its n), then as made only once every endpoint of
///   its microphones is there ([`finish_verdict`]; else [`SetupError::Unfinished`]: still pending); any other cable is
///   recorded, then its endpoints are checked ([`SetupError::Checks`]).
pub fn make_recorded<O: Ops>(
    ops: &mut O, create: &Create, want: Want, rec: Recording, names: Names, say: &mut dyn FnMut(&str),
) -> (Option<CableInstance>, Result<Done, SetupError>) {
    let made = match ops.make(create, want, say) {
        Ok(m) => m,
        Err(e) => return (None, Err(e)),
    };
    let cable = made.cable.clone();
    let r = recorded(ops, &made, create.variant, rec, names, say);
    (Some(cable), r)
}

/// [`make_recorded`] once the cable is made.
fn recorded<O: Ops>(
    ops: &mut O, made: &Made, variant: Variant, rec: Recording, names: Names, say: &mut dyn FnMut(&str),
) -> Result<Done, SetupError> {
    let cable = &made.cable;
    let record = |ops: &mut O, pending| ops.record(&entry_of(cable, variant, rec.replaces, pending)).map_err(SetupError::Store);
    if made.restart {
        let device = cable.instance_id();
        return record(ops, rec.pending).map(|()| Done::RestartPending { device: device.clone() }).map_err(|e| e.and_restart(Some(device)));
    }
    if !rec.pending {
        record(ops, false)?;
        let problems = wait_endpoints(ops, cable, variant, names, say);
        return if problems.is_empty() { Ok(Done::Changed) } else { Err(SetupError::Checks(problems)) };
    }
    // A migration's cable: its n recorded, still pending, so a run that stops during the wait checks the node made now
    record(ops, true)?;
    say(&format!("Waiting for Windows to make {}'s endpoints (up to {} s)", cable.label.friendly_name(), ENDPOINTS_WAIT.as_secs()));
    let found = ops.wait_endpoints(cable, variant, names);
    match finish_verdict(cable, found, say) {
        Ok(misnamed) => {
            record(ops, false)?;
            say("  recorded as made in the setup's store (its record was a migration's, still pending)");
            if misnamed.is_empty() { Ok(Done::Changed) } else { Err(SetupError::Checks(misnamed)) }
        }
        Err(problems) => {
            say("  it stays pending in the setup's store: the migration (Install-Driver.ps1) checks it again");
            Err(SetupError::Unfinished { problems, to_make: vec![cable.label.clone()] })
        }
    }
}

/// "A and B".
pub fn labels(labels: &[Label]) -> String {
    let names: Vec<String> = labels.iter().map(Label::friendly_name).collect();
    match names.as_slice() {
        [] => "nothing".into(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::*;
    use crate::cables::EndpointKind;
    use crate::setup::package::tests::candidate;
    use crate::setup::package::version_text;
    use crate::setup::plan::tests::{A_ID, B_ID, cid, label, legacy, node};
    use crate::setup::plan::{Health, Legacy, plan_migrate, plan_update};

    const PUBLISHED: &str = r"C:\Windows\INF\oem20.inf";
    const VERSION: u64 = (3 << 48) | (27 << 32) | (41 << 16) | 512;
    const NEW_A: &str = "a0a0a0a0-0000-4000-8000-0000000000a0";
    const NEW_B: &str = "b0b0b0b0-0000-4000-8000-0000000000b0";

    /// Windows as the tests script it: each call logged, the nodes disabled now and removed kept.
    #[derive(Default)]
    struct Fake {
        calls: Vec<String>,
        disabled: BTreeSet<usize>,
        removed: BTreeSet<usize>,
        veto: Option<usize>,
        still_loaded: bool,
        install_fails: Option<usize>,
        install_restart: Option<usize>,
        uninstall_fails: Option<usize>,
        uninstall_restart: Option<usize>,
        target_fails: Option<&'static str>,
        make_fails: Option<&'static str>,
        /// This cable's make fails, and Windows needs a restart to finish removing the node that failed (StillPresent).
        make_fails_restart: Option<&'static str>,
        made_restart: Option<&'static str>,
        /// This cable is made with n + 1 (`make_or_next`'s fallback, the node re-opened from a removed one's key having failed).
        made_next: Option<&'static str>,
        /// This node is on the package after its install or enable, but Windows needs a restart to start it (Code 14).
        start_restart: Option<usize>,
        /// This cable's endpoints never come.
        endpoints_missing: Option<&'static str>,
        /// This cable's endpoints come with Windows' "2- " numbering in their names.
        endpoints_numbered: Option<&'static str>,
        records: Vec<Entry>,
        /// CM_Enable_DevNode fails on this node.
        enable_fails: Option<usize>,
        /// The persistent change (DIF_PROPERTYCHANGE) fails on this node.
        state_fails: Option<usize>,
        /// The persistent change on this node needs a restart to finish (DI_NEEDRESTART): its state stays as it was.
        state_restart: Option<usize>,
        /// This node doesn't start after an enable (Code 10).
        enable_broken: Option<usize>,
        /// The endpoints before the run (endpoints_before's look).
        before: Vec<Endpoint>,
        /// The nodes disabled persistently (CONFIGFLAG_DISABLED), by the persistent change.
        persistent: BTreeSet<usize>,
    }

    impl Fake {
        /// A node disabled before the run.
        fn with_disabled(mut self, i: usize) -> Self {
            self.disabled.insert(i);
            self
        }

        fn log(&mut self, s: String) {
            self.calls.push(s);
        }

        /// The calls of these kinds, in order.
        fn only(&self, kinds: &[&str]) -> Vec<&str> {
            self.calls.iter().map(String::as_str).filter(|c| kinds.iter().any(|k| c.split(' ').next() == Some(k))).collect()
        }
    }

    fn failed(call: &'static str) -> WinError {
        WinError { call, code: 5 }
    }

    impl Ops for Fake {
        type Driver = ();

        fn staged_driver(&mut self, i: usize, _: &str, _: &mut dyn FnMut(&str)) -> Result<((), Candidate), SetupError> {
            self.log(format!("staged {i}"));
            Ok(((), candidate(PUBLISHED, "audiocodec.inf", 200, VERSION)))
        }

        fn check_target(&mut self, label: &Label, variant: Variant, published: &str, _: &mut dyn FnMut(&str)) -> Result<(), SetupError> {
            self.log(format!("check {label}"));
            if self.target_fails == Some(label.as_str()) {
                return Err(SetupError::NotCompatible {
                    device: format!("the new {}", label.friendly_name()),
                    hardware_id: variant.hardware_id().into(),
                    package: published.into(),
                });
            }
            Ok(())
        }

        fn disable(&mut self, i: usize) -> Disabled {
            self.log(format!("disable {i}"));
            if self.veto == Some(i) {
                return Disabled::Vetoed;
            }
            self.disabled.insert(i);
            Disabled::Done
        }

        fn enable(&mut self, i: usize) -> Result<(), WinError> {
            self.log(format!("enable {i}"));
            if self.enable_fails == Some(i) {
                return Err(failed("CM_Enable_DevNode"));
            }
            self.disabled.remove(&i);
            Ok(())
        }

        fn change_state(&mut self, i: usize, enable: bool) -> Result<bool, WinError> {
            self.log(format!("{} {i}", if enable { "state-on" } else { "state-off" }));
            if self.state_fails == Some(i) {
                return Err(failed("SetupDiCallClassInstaller(DIF_PROPERTYCHANGE)"));
            }
            if self.state_restart == Some(i) {
                return Ok(true);
            }
            if enable {
                self.disabled.remove(&i);
                self.persistent.remove(&i);
            } else {
                self.disabled.insert(i);
                self.persistent.insert(i);
            }
            Ok(false)
        }

        fn bound(&mut self, i: usize) -> Bound {
            self.wait_working(i)
        }

        fn clear_count(&mut self, i: usize) -> Result<bool, WinError> {
            self.log(format!("clear {i}"));
            Ok(false)
        }

        fn install(&mut self, i: usize, _: &()) -> Result<bool, WinError> {
            self.log(format!("install {i}"));
            if self.install_fails == Some(i) {
                return Err(failed("DiInstallDevice"));
            }
            Ok(self.install_restart == Some(i))
        }

        fn set_friendly_name(&mut self, i: usize, _: &str) -> Result<(), WinError> {
            self.log(format!("rename {i}"));
            Ok(())
        }

        fn uninstall(&mut self, i: usize) -> Result<bool, WinError> {
            self.log(format!("uninstall {i}"));
            if self.uninstall_fails == Some(i) {
                return Err(failed("DiUninstallDevice"));
            }
            self.removed.insert(i);
            self.disabled.remove(&i);
            Ok(self.uninstall_restart == Some(i))
        }

        fn wait_working(&mut self, i: usize) -> Bound {
            let health = if self.disabled.contains(&i) {
                Health::Problem(22)
            } else if self.start_restart == Some(i) {
                Health::Problem(14)
            } else if self.enable_broken == Some(i) {
                Health::Problem(10)
            } else {
                Health::Working
            };
            let installed_bad = self.install_fails == Some(i);
            Bound {
                health,
                inf: Some(if installed_bad { "oem12.inf" } else { "oem20.inf" }.into()),
                driver_version: Some(if installed_bad { "3.27.41.511".into() } else { version_text(VERSION) }),
            }
        }

        fn wait_unloaded(&mut self) -> service::Unload {
            self.log("wait".into());
            if self.still_loaded {
                service::Unload::StillLoaded(service::State::Loaded(4))
            } else {
                service::Unload::Done(Duration::from_millis(500))
            }
        }

        fn endpoints_before(&mut self, also: &[String]) -> Result<Vec<Endpoint>, WinError> {
            self.log(format!("endpoints {}", also.join(" ")));
            Ok(self.before.clone())
        }

        fn wait_endpoints(&mut self, cable: &CableInstance, variant: Variant, names: Names) -> (Option<EndpointCheck>, Option<WinError>) {
            self.log(format!("wait-endpoints {}", cable.label));
            if self.endpoints_missing == Some(cable.label.as_str()) {
                return (Some(endpoints::check(cable, variant, &[], names)), None);
            }
            let numbered = self.endpoints_numbered == Some(cable.label.as_str());
            let eps: Vec<Endpoint> = EndpointKind::of_variant(variant)
                .into_iter()
                .map(|kind| Endpoint {
                    id: format!("{}.{kind:?}", cable.instance_id()),
                    name: if numbered { format!("{} (2- ACX-LL-Cable)", kind.name()) } else { endpoints::expected_name(cable, kind) },
                    kind,
                    state: endpoints::State::Active,
                    instance_id: cable.instance_id().to_lowercase(),
                })
                .collect();
            (Some(endpoints::check(cable, variant, &eps, names)), None)
        }

        fn make(&mut self, create: &Create, _: Want, _: &mut dyn FnMut(&str)) -> Result<Made, SetupError> {
            let l = create.instance.label.as_str();
            self.log(format!("make {l}"));
            if self.make_fails == Some(l) {
                return Err(SetupError::Windows(failed("SetupDiCallClassInstaller(DIF_REGISTERDEVICE)")));
            }
            if self.make_fails_restart == Some(l) {
                return Err(SetupError::StillPresent {
                    device: create.instance.instance_id(),
                    why: "Windows needs a restart to finish removing it".into(),
                    restart: true,
                    error: Box::new(SetupError::Windows(failed("DiInstallDevice"))),
                });
            }
            let mut cable = create.instance.clone();
            if self.made_next == Some(l) {
                cable.n += 1;
            }
            Ok(Made { cable, restart: self.made_restart == Some(l) })
        }

        fn record(&mut self, entry: &Entry) -> Result<(), WinError> {
            self.log(format!("record {}{}", entry.label, if entry.pending { " pending" } else { "" }));
            self.records.push(entry.clone());
            Ok(())
        }
    }

    /// Three cables, A, B and C.
    fn three() -> Vec<Node> {
        vec![
            node("A", A_ID, 1, Variant::Both),
            node("B", B_ID, 1, Variant::LowLatency),
            node("C", "cccccccc-0000-4000-8000-00000000000c", 1, Variant::Call),
        ]
    }

    fn update(fake: &mut Fake, nodes: &[Node]) -> (Result<Done, SetupError>, Vec<String>) {
        let mut said = Vec::new();
        let plan = plan_update(nodes).unwrap();
        let r = run_update(fake, nodes, &plan, PUBLISHED, &mut |s: &str| said.push(s.to_string()));
        (r, said)
    }

    #[test]
    fn an_update_disables_every_cable_installs_and_enables_them() {
        let nodes = three();
        let mut fake = Fake::default();
        let (r, _) = update(&mut fake, &nodes);
        assert_eq!(r, Ok(Done::Changed));
        assert_eq!(fake.only(&["staged", "disable", "wait", "install", "enable"]), [
            "staged 0", "staged 1", "staged 2", "disable 0", "disable 1", "disable 2", "wait", "install 0", "install 1", "install 2",
            "enable 0", "enable 1", "enable 2"
        ]);
        // D1's start count cleared before each install and each enable
        let at = |c: &str| fake.calls.iter().position(|x| x == c).unwrap();
        assert!(at("clear 1") < at("install 1") && fake.calls[at("enable 1") - 1] == "clear 1");
        assert!(fake.disabled.is_empty());
    }

    /// A veto at the second of three cables: the first enabled again, nothing installed.
    #[test]
    fn a_veto_puts_back_what_the_update_disabled() {
        let nodes = three();
        let mut fake = Fake { veto: Some(1), ..Fake::default() };
        let (r, said) = update(&mut fake, &nodes);
        let Err(SetupError::Disable { device, vetoed: true, unrestored, .. }) = r else { panic!("{r:?}") };
        assert_eq!((device.as_str(), unrestored.len()), (nodes[1].instance_id.as_str(), 0));
        assert_eq!(fake.only(&["disable", "wait", "install", "enable", "uninstall"]), ["disable 0", "disable 1", "enable 0"]);
        assert!(fake.disabled.is_empty(), "nothing left disabled");
        assert!(said.iter().any(|s| s.contains("enabling") && s.contains(&nodes[0].instance_id)), "{said:?}");
    }

    /// The image still loaded after the wait: every cable enabled again, nothing installed.
    #[test]
    fn an_image_that_stays_loaded_puts_everything_back() {
        let nodes = three();
        let mut fake = Fake { still_loaded: true, ..Fake::default() };
        let (r, _) = update(&mut fake, &nodes);
        assert!(matches!(r, Err(SetupError::StillLoaded { ref unrestored, .. }) if unrestored.is_empty()), "{r:?}");
        assert_eq!(fake.only(&["disable", "wait", "install", "enable"]), [
            "disable 0", "disable 1", "disable 2", "wait", "enable 0", "enable 1", "enable 2"
        ]);
        assert!(fake.disabled.is_empty());
    }

    /// An install that fails on one cable: every cable still enabled, the failure named.
    #[test]
    fn a_failed_install_still_enables_every_cable() {
        let nodes = three();
        let mut fake = Fake { install_fails: Some(1), ..Fake::default() };
        let (r, _) = update(&mut fake, &nodes);
        let Err(SetupError::Checks(problems)) = r else { panic!("{r:?}") };
        assert!(problems.iter().all(|p| p.starts_with(&nodes[1].instance_id)), "{problems:?}");
        assert!(problems[0].contains("DiInstallDevice failed"), "{problems:?}");
        assert!(problems[1].contains("another package"), "{problems:?}");
        assert_eq!(fake.only(&["install", "enable"]), ["install 0", "install 1", "install 2", "enable 0", "enable 1", "enable 2"]);
        assert!(fake.disabled.is_empty());
    }

    /// A cable an earlier run left disabled (not persistently) is enabled with the rest, and on a veto too; the user's disable
    /// stays.
    #[test]
    fn an_earlier_runs_disable_is_put_right() {
        let mut nodes = three();
        nodes[1].health = Health::Problem(22);
        let mut fake = Fake::default().with_disabled(1);
        let (r, said) = update(&mut fake, &nodes);
        assert_eq!(r, Ok(Done::Changed));
        assert_eq!(fake.only(&["disable", "enable"]), ["disable 0", "disable 2", "enable 1", "enable 0", "enable 2"]);
        assert!(said[0].contains("an earlier run's"), "{said:?}");
        assert!(fake.disabled.is_empty());
        let mut fake = Fake { veto: Some(2), ..Fake::default() }.with_disabled(1);
        let (r, _) = update(&mut fake, &nodes);
        assert!(matches!(r, Err(SetupError::Disable { .. })));
        assert_eq!(fake.only(&["disable", "enable", "install"]), ["disable 0", "disable 2", "enable 1", "enable 0"]);
        assert!(fake.disabled.is_empty());
        // The user's (CONFIGFLAG_DISABLED): left disabled, though it gets the package
        nodes[1].config_flags = Some(1);
        let mut fake = Fake::default().with_disabled(1);
        let (r, said) = update(&mut fake, &nodes);
        assert_eq!(r, Ok(Done::Changed));
        assert_eq!(fake.only(&["install", "enable"]), ["install 0", "install 1", "install 2", "enable 0", "enable 2"]);
        assert!(said.iter().any(|s| s.contains("left disabled")), "{said:?}");
        assert_eq!(fake.disabled, BTreeSet::from([1]));
    }

    /// The box today, migrated: the old A and Cable-B.ps1's B, a cable C made since.
    fn todays_box() -> Vec<Node> {
        vec![
            legacy(r"ROOT\MEDIA\0003", Legacy::AudioCodec(Variant::Both)),
            legacy(r"ROOT\MEDIA\0004", Legacy::CableB),
            node("C", "cccccccc-0000-4000-8000-00000000000c", 1, Variant::Call),
        ]
    }

    fn migrate_gathering<O: Ops>(ops: &mut O, nodes: &[Node], store: &[Entry]) -> (Result<Done, SetupError>, Vec<String>, Gathered) {
        let mut said = Vec::new();
        let mut gathered = Gathered::default();
        let plan = plan_migrate(nodes, store, None, None, [cid(NEW_A), cid(NEW_B)]).unwrap();
        let r = run_migrate(ops, nodes, &plan, PUBLISHED, &mut gathered, &mut |s: &str| said.push(s.to_string()));
        (r, said, gathered)
    }

    fn migrate(fake: &mut Fake, nodes: &[Node], store: &[Entry]) -> (Result<Done, SetupError>, Vec<String>) {
        let (r, said, _) = migrate_gathering(fake, nodes, store);
        (r, said)
    }

    #[test]
    fn the_migration_checks_records_unloads_removes_installs_and_makes_in_that_order() {
        let nodes = todays_box();
        let mut fake = Fake::default();
        let (r, _, gathered) = migrate_gathering(&mut fake, &nodes, &[]);
        assert_eq!(r, Ok(Done::Changed));
        assert_eq!(fake.only(&["staged", "check", "record", "disable", "wait", "uninstall", "install", "enable", "make"]), [
            "staged 2", "check A", "check B", "record A pending", "record B pending", "disable 0", "disable 1", "disable 2", "wait",
            "uninstall 0", "uninstall 1", "install 2", "enable 2", "make A", "record A", "make B", "record B"
        ]);
        assert!(fake.disabled.is_empty());
        let last: Vec<(String, bool)> = fake.records.iter().map(|e| (e.label.to_string(), e.pending)).collect();
        assert_eq!(last[2..], [("A".to_string(), false), ("B".to_string(), false)]);
        assert_eq!(fake.records[2].id, cid(NEW_A), "made with the identity recorded first");
        // The old nodes each cable replaces go to the store with it (pending), and the look before the run takes their endpoints
        assert_eq!(
            (fake.records[0].replaces.as_slice(), fake.records[1].replaces.as_slice()),
            (&[r"ROOT\MEDIA\0003".to_string()][..], &[r"ROOT\MEDIA\0004".to_string()][..])
        );
        assert!(fake.records[2..].iter().all(|e| e.replaces.len() == 1), "kept once made: dropped after the map (drop_replaces)");
        let mut dropped = Fake::default();
        drop_replaces(&mut dropped, &fake.records[2..], &mut |_: &str| {});
        assert_eq!(dropped.only(&["record"]), ["record A", "record B"]);
        assert!(dropped.records.iter().all(|e| e.replaces.is_empty() && !e.pending));
        // Not while a record is pending: the run that finishes the migration maps them
        let mut kept = Fake::default();
        drop_replaces(&mut kept, &fake.records[..], &mut |_: &str| {});
        assert!(kept.records.is_empty());
        assert_eq!(fake.only(&["endpoints"]), [r"endpoints ROOT\MEDIA\0003 ROOT\MEDIA\0004"]);
        // The endpoint map's input: the endpoints before, and A and B in place of the old nodes
        assert_eq!(gathered.before, Some(vec![]));
        let made: Vec<(&str, Vec<endpoint_map::Old>)> = gathered.cables.iter().map(|m| (m.cable.label.as_str(), m.old.clone())).collect();
        assert_eq!(made, [
            ("A", vec![endpoint_map::Old::Instance(r"ROOT\MEDIA\0003".into()), endpoint_map::Old::OtherInstance { id: cid(NEW_A), n: 1 }]),
            ("B", vec![endpoint_map::Old::Instance(r"ROOT\MEDIA\0004".into()), endpoint_map::Old::OtherInstance { id: cid(NEW_B), n: 1 }]),
        ]);
    }

    /// The package in the folder doesn't have the per-cable hardware IDs (a build from before D1, as the box's
    /// driver\out\Release\AudioCodec holds today): refused before anything is recorded or disabled.
    #[test]
    fn a_package_without_the_cables_ids_stops_the_migration_before_anything_changes() {
        let nodes = todays_box();
        let mut fake = Fake { target_fails: Some("A"), ..Fake::default() };
        let (r, _) = migrate(&mut fake, &nodes, &[]);
        let Err(e @ SetupError::NotCompatible { .. }) = r else { panic!("{r:?}") };
        assert!(e.to_string().contains("nothing was changed"), "{e}");
        assert_eq!(fake.calls, ["staged 2", "check A"]);
        assert!(fake.records.is_empty() && fake.removed.is_empty() && fake.disabled.is_empty());
    }

    /// The old A's removal fails after the image unloaded: every node still there enabled again, nothing installed, A and B
    /// left pending in the store for the next run.
    #[test]
    fn a_failed_removal_puts_back_what_the_migration_disabled() {
        let nodes = todays_box();
        let mut fake = Fake { uninstall_fails: Some(1), ..Fake::default() };
        let (r, _) = migrate(&mut fake, &nodes, &[]);
        let Err(SetupError::Stopped { error, unrestored, to_make }) = &r else { panic!("{r:?}") };
        assert!(matches!(**error, SetupError::Windows(_)) && unrestored.is_empty());
        assert_eq!(to_make, &[label("A"), label("B")]);
        assert_eq!(fake.only(&["uninstall", "install", "enable", "make"]), ["uninstall 0", "uninstall 1", "enable 1", "enable 2"]);
        assert!(fake.disabled.is_empty(), "nothing left disabled: {:?}", fake.disabled);
        assert!(fake.records.iter().all(|e| e.pending));
        assert!(!r.as_ref().unwrap_err().needs_restart(), "a failure: exit 2");
        let text = r.unwrap_err().to_string();
        assert!(text.contains("ACX-LL-Cable A and ACX-LL-Cable B are recorded in the setup's store"), "{text}");
        // A removal that needs a restart stops it the same way, the node it removed not enabled
        let mut fake = Fake { uninstall_restart: Some(0), ..Fake::default() };
        let (r, _) = migrate(&mut fake, &nodes, &[]);
        let Err(e @ SetupError::Stopped { error, .. }) = &r else { panic!("{r:?}") };
        assert!(matches!(**error, SetupError::RestartFirst { .. }));
        assert!(e.needs_restart(), "exit 7: the harness stops there (Astra's review, 2026-11-25)");
        assert_eq!(fake.only(&["uninstall", "install", "enable", "make"]), ["uninstall 0", "enable 1", "enable 2"]);
        assert!(fake.disabled.is_empty());
    }

    /// An update's install that needs a restart, beside one that fails: an error that needs the restart first (exit 7).
    #[test]
    fn an_updates_restart_beside_a_failure_is_a_restart() {
        let nodes = three();
        let mut fake = Fake { install_restart: Some(0), install_fails: Some(1), ..Fake::default() };
        let (r, _) = update(&mut fake, &nodes);
        let Err(e @ SetupError::AndRestart { device, error }) = &r else { panic!("{r:?}") };
        assert_eq!(device, &nodes[0].instance_id);
        assert!(matches!(&**error, SetupError::Checks(p) if p.iter().all(|p| p.starts_with(&nodes[1].instance_id))), "{error:?}");
        assert!(e.needs_restart());
        assert!(fake.disabled.is_empty());
    }

    /// The migration's veto and timeout put back every node, the old driver's too, and change nothing else (the pending
    /// entries stay for the next run).
    #[test]
    fn a_veto_or_timeout_in_the_migration_changes_nothing() {
        let nodes = todays_box();
        for mut fake in [Fake { veto: Some(2), ..Fake::default() }, Fake { still_loaded: true, ..Fake::default() }] {
            let (r, _) = migrate(&mut fake, &nodes, &[]);
            assert!(matches!(r, Err(SetupError::Disable { .. } | SetupError::StillLoaded { .. })), "{r:?}");
            assert!(fake.only(&["uninstall", "install", "make"]).is_empty(), "{:?}", fake.calls);
            assert!(fake.disabled.is_empty());
        }
    }

    /// A cable that can't be made stays pending, and the other is still made.
    #[test]
    fn a_cable_that_isnt_made_stays_pending() {
        let nodes = todays_box();
        let mut fake = Fake { make_fails: Some("A"), ..Fake::default() };
        let (r, _) = migrate(&mut fake, &nodes, &[]);
        let Err(SetupError::Unfinished { problems, to_make }) = &r else { panic!("{r:?}") };
        assert_eq!((to_make.as_slice(), problems.len()), (&[label("A")][..], 1));
        assert_eq!(fake.only(&["make", "record"]), ["record A pending", "record B pending", "make A", "make B", "record B"]);
        assert!(r.unwrap_err().to_string().contains("run Install-Driver.ps1"));
        // The next run, A pending: A made with the identity the first run recorded
        let store: Vec<Entry> = vec![fake.records[0].clone(), Entry { pending: false, ..fake.records[1].clone() }];
        let now = vec![nodes[2].clone(), node("B", NEW_B, 1, Variant::Both)];
        let mut fake = Fake::default();
        let (r, _) = migrate(&mut fake, &now, &store);
        assert_eq!(r, Ok(Done::Changed));
        assert_eq!(fake.only(&["make", "record", "uninstall"]), ["record A pending", "make A", "record A"]);
        assert_eq!(fake.records[1].id, cid(NEW_A));
    }

    /// A cable made with n + 1 (`make_or_next`'s fallback) whose endpoints don't come within the wait: it stays pending, its
    /// record written again with the n it was made with, the nodes it replaces kept, and the other cable is still made.
    #[test]
    fn a_cable_made_with_the_next_n_whose_endpoints_are_late_stays_pending_with_that_n() {
        let nodes = todays_box();
        let mut fake = Fake { made_next: Some("A"), endpoints_missing: Some("A"), ..Fake::default() };
        let (r, _, gathered) = migrate_gathering(&mut fake, &nodes, &[]);
        let Err(e @ SetupError::Unfinished { problems, to_make }) = &r else { panic!("{r:?}") };
        assert_eq!((to_make.as_slice(), e.needs_restart()), (&[label("A")][..], false), "exit 2");
        assert!(problems.iter().any(|p| p.contains("Windows hasn't made")), "{problems:?}");
        assert_eq!(fake.only(&["make", "wait-endpoints", "record"]), [
            "record A pending", "record B pending", "wait-endpoints C", "make A", "wait-endpoints A", "record A pending", "make B",
            "wait-endpoints B", "record B"
        ]);
        let a: Vec<(u32, bool, usize)> =
            fake.records.iter().filter(|e| e.label.as_str() == "A").map(|e| (e.n, e.pending, e.replaces.len())).collect();
        assert_eq!(a, [(1, true, 1), (2, true, 1)], "pending first with the plan's n, then with the n it was made with");
        assert_eq!(gathered.cables[0].cable.n, 2, "mapped as made");
        // Made with the plan's n: its pending record already has it, so it isn't written again
        let mut fake = Fake { endpoints_missing: Some("A"), ..Fake::default() };
        let _ = migrate(&mut fake, &nodes, &[]);
        assert_eq!(fake.only(&["record"]), ["record A pending", "record B pending", "record B"]);
    }

    /// A restart Windows needs for an install stops the run before the cables are made (exit 7); they stay pending, and it
    /// says so.
    #[test]
    fn a_restart_leaves_the_cables_to_make_pending() {
        let nodes = todays_box();
        let mut fake = Fake { install_restart: Some(2), ..Fake::default() };
        let (r, said) = migrate(&mut fake, &nodes, &[]);
        assert_eq!(r, Ok(Done::RestartPending { device: nodes[2].instance_id.clone() }));
        assert!(fake.only(&["make"]).is_empty());
        assert!(fake.records.iter().all(|e| e.pending));
        let next = "then run this again (Install-Driver.ps1) to finish ACX-LL-Cable A and ACX-LL-Cable B, recorded in the setup's store";
        assert!(said.iter().any(|s| s.contains(next)), "{said:?}");
        assert!(fake.disabled.is_empty());
        // With a failure beside it: an error naming both, and the cables still to make, that needs the restart first (exit 7)
        let mut with_d = nodes.clone();
        with_d.push(node("D", "dddddddd-0000-4000-8000-00000000000d", 1, Variant::Both));
        let mut fake = Fake { install_restart: Some(2), install_fails: Some(3), ..Fake::default() };
        let (r, _) = migrate(&mut fake, &with_d, &[]);
        let Err(e @ SetupError::AndRestart { device, error }) = &r else { panic!("{r:?}") };
        assert!(e.needs_restart() && device == &with_d[2].instance_id);
        let SetupError::Unfinished { problems, to_make } = &**error else { panic!("{error:?}") };
        assert_eq!(to_make, &[label("A"), label("B")]);
        assert!(problems.iter().all(|p| p.starts_with(&with_d[3].instance_id)), "{problems:?}");
        let text = e.to_string();
        assert!(text.contains("are recorded in the setup's store") && text.contains("Windows needs a restart to finish with"), "{text}");
        assert!(fake.disabled.is_empty());
        // One for a cable made: it stays pending (its n recorded), so the run after the restart checks it; the rest wait too
        let mut fake = Fake { made_restart: Some("A"), ..Fake::default() };
        let (r, said, gathered) = migrate_gathering(&mut fake, &nodes, &[]);
        assert!(matches!(r, Ok(Done::RestartPending { .. })), "{r:?}");
        assert_eq!(fake.only(&["make"]), ["make A"]);
        assert!(said.iter().any(|s| s.contains("to finish ACX-LL-Cable A and ACX-LL-Cable B, recorded")), "{said:?}");
        assert!(fake.records.iter().all(|e| e.pending), "{:?}", fake.records);
        assert_eq!(fake.records.last().map(|e| (e.label.as_str(), e.replaces.len())), Some(("A", 1)), "A's n, and the node it replaces");
        assert_eq!(gathered.cables.iter().map(|m| m.cable.label.as_str()).collect::<Vec<_>>(), ["A"]);
    }

    /// A finished cable's endpoints: every one of its microphones there is a pass (the names said beside it); one missing isn't.
    #[test]
    fn a_cable_to_finish_passes_once_its_endpoints_are_there() {
        let b = CableInstance { label: label("B"), id: cid(B_ID), n: 1 };
        let mut fake = Fake::default();
        let quiet = &mut |_: &str| {};
        assert_eq!(finish_verdict(&b, fake.wait_endpoints(&b, Variant::Both, Names::Since(&[])), quiet), Ok(vec![]));
        fake.endpoints_numbered = Some("B");
        let misnamed = finish_verdict(&b, fake.wait_endpoints(&b, Variant::Both, Names::Since(&[])), quiet).unwrap();
        assert!(!misnamed.is_empty() && misnamed.iter().all(|p| p.contains("(2- ACX-LL-Cable)")), "{misnamed:?}");
        fake.endpoints_missing = Some("B");
        let why = finish_verdict(&b, fake.wait_endpoints(&b, Variant::Both, Names::Since(&[])), quiet).unwrap_err();
        assert!(why[0].contains("Windows hasn't made"), "{why:?}");
        assert!(finish_verdict(&b, (None, None), quiet).is_err(), "unread");
    }

    /// The box as the migration's runs leave it: the nodes (a run's own, by index, and those it makes after them), those removed,
    /// and the store. Every change counts against `budget`; once it is spent, each change fails and does nothing, so the run's
    /// world stays as a kill right after its last change would leave it (the calls after fail without effect). `make` is two
    /// changes, as on Windows: the node registered (present, no driver: Code 28), then installed. `restart_make`: that cable's
    /// install needs a restart (Code 14 until [`World::next`]). The endpoints: each working node's (the old A's three,
    /// Cable-B.ps1's B's Speakers and Microphone, a cable's variant's), a disabled node's and a removed old node's lingering
    /// (NOTPRESENT), as Windows keeps them; `late`, a cable whose endpoints don't come in this world (they do in the next).
    /// `map`: the endpoint map every run publishes, one file across runs (as `Install-Driver.ps1 -EndpointMap <file>` has it);
    /// its publish is a change.
    struct World {
        nodes: Vec<Node>,
        removed: BTreeSet<usize>,
        /// Old nodes earlier runs removed: their endpoints linger, NOTPRESENT.
        lingering: Vec<Node>,
        store: Vec<Entry>,
        budget: usize,
        changes: Vec<String>,
        restart_make: Option<&'static str>,
        /// This cable's make fails: its node registered, removed again, and Windows needs a restart to finish the removal.
        make_fails_restart: Option<&'static str>,
        late: Option<&'static str>,
        /// An app vetoes this cable's disable.
        veto: Option<&'static str>,
        map: Option<PathBuf>,
    }

    impl World {
        fn new(nodes: Vec<Node>, store: Vec<Entry>, budget: usize) -> Self {
            World {
                nodes,
                removed: BTreeSet::new(),
                lingering: Vec::new(),
                store,
                budget,
                changes: Vec::new(),
                restart_make: None,
                make_fails_restart: None,
                late: None,
                veto: None,
                map: None,
            }
        }

        /// The next run's world: what is left present, the store, no budget limit; a node waiting for a restart has had it, and
        /// the endpoints that were late have come. The map's file is the same.
        fn next(&self) -> World {
            let mut nodes: Vec<Node> =
                self.nodes.iter().enumerate().filter(|(i, _)| !self.removed.contains(i)).map(|(_, n)| n.clone()).collect();
            for n in nodes.iter_mut().filter(|n| n.health == Health::Problem(14)) {
                n.health = Health::Working;
            }
            let mut next = World::new(nodes, self.store.clone(), usize::MAX);
            next.lingering = self.lingering.clone();
            next.lingering.extend(self.removed.iter().map(|&i| self.nodes[i].clone()).filter(|n| n.is_legacy()));
            next.map = self.map.clone();
            next
        }

        /// The endpoints of node `n`, in `state`.
        fn endpoints_of(n: &Node, state: endpoints::State) -> Vec<Endpoint> {
            let kinds = match n.model {
                crate::setup::plan::Model::Cable(v) | crate::setup::plan::Model::Legacy(Legacy::AudioCodec(v)) => {
                    EndpointKind::of_variant(v)
                }
                crate::setup::plan::Model::Legacy(Legacy::CableB) => vec![EndpointKind::Speakers, EndpointKind::Microphone],
            };
            let name = |kind: EndpointKind| match &n.cable {
                Some(c) => endpoints::expected_name(c, kind),
                None => format!("{} (the old driver's)", kind.name()),
            };
            kinds
                .into_iter()
                .map(|kind| Endpoint {
                    id: endpoint_id(&n.instance_id, kind),
                    name: name(kind),
                    kind,
                    state,
                    instance_id: n.instance_id.to_lowercase(),
                })
                .collect()
        }

        /// The endpoints as a run reads them after it: every working cable's but the late one's.
        fn after(&self) -> Vec<Endpoint> {
            let late = |n: &Node| n.cable.as_ref().is_some_and(|c| self.late == Some(c.label.as_str()));
            (self.nodes.iter().enumerate())
                .filter(|(i, n)| !self.removed.contains(i) && n.cable.is_some() && n.health == Health::Working && !late(n))
                .flat_map(|(_, n)| World::endpoints_of(n, endpoints::State::Active))
                .collect()
        }

        fn spend(&mut self, change: String) -> bool {
            if self.budget == 0 {
                return false;
            }
            self.budget -= 1;
            self.changes.push(change);
            true
        }

        fn published() -> crate::setup::package::Running {
            crate::setup::package::Running { inf: "oem20.inf".into(), version: version_text(VERSION) }
        }

        fn killed() -> WinError {
            WinError { call: "(the tool was killed)", code: 1 }
        }
    }

    impl Ops for World {
        type Driver = ();

        fn staged_driver(&mut self, _: usize, _: &str, _: &mut dyn FnMut(&str)) -> Result<((), Candidate), SetupError> {
            Ok(((), candidate(PUBLISHED, "audiocodec.inf", 200, VERSION)))
        }

        fn check_target(&mut self, _: &Label, _: Variant, _: &str, _: &mut dyn FnMut(&str)) -> Result<(), SetupError> {
            Ok(())
        }

        fn disable(&mut self, i: usize) -> Disabled {
            if self.nodes[i].cable.as_ref().is_some_and(|c| self.veto == Some(c.label.as_str())) {
                return Disabled::Vetoed;
            }
            if !self.spend(format!("disable {}", self.nodes[i].instance_id)) {
                return Disabled::Failed(World::killed());
            }
            self.nodes[i].health = Health::Problem(22);
            self.nodes[i].config_flags = Some(0);
            Disabled::Done
        }

        fn enable(&mut self, i: usize) -> Result<(), WinError> {
            if !self.spend(format!("enable {}", self.nodes[i].instance_id)) {
                return Err(World::killed());
            }
            let n = &mut self.nodes[i];
            n.health = if n.running.is_some() { Health::Working } else { Health::Problem(28) };
            Ok(())
        }

        /// As a disable or enable that persists (the migration never makes one).
        fn change_state(&mut self, i: usize, enable: bool) -> Result<bool, WinError> {
            if !self.spend(format!("state {} {}", if enable { "on" } else { "off" }, self.nodes[i].instance_id)) {
                return Err(World::killed());
            }
            let n = &mut self.nodes[i];
            n.health = if !enable {
                Health::Problem(22)
            } else if n.running.is_some() {
                Health::Working
            } else {
                Health::Problem(28)
            };
            n.config_flags = Some(if enable { 0 } else { windows::Win32::Devices::DeviceAndDriverInstallation::CONFIGFLAG_DISABLED.0 });
            Ok(false)
        }

        fn bound(&mut self, i: usize) -> Bound {
            self.wait_working(i)
        }

        fn clear_count(&mut self, i: usize) -> Result<bool, WinError> {
            if !self.spend(format!("clear {}", self.nodes[i].instance_id)) {
                return Err(World::killed());
            }
            Ok(self.nodes[i].start_attempts.take().is_some())
        }

        fn install(&mut self, i: usize, _: &()) -> Result<bool, WinError> {
            if !self.spend(format!("install {}", self.nodes[i].instance_id)) {
                return Err(World::killed());
            }
            let n = &mut self.nodes[i];
            n.running = Some(World::published());
            if n.health != Health::Problem(22) {
                n.health = Health::Working;
            }
            Ok(false)
        }

        fn set_friendly_name(&mut self, i: usize, _: &str) -> Result<(), WinError> {
            if !self.spend(format!("rename {}", self.nodes[i].instance_id)) {
                return Err(World::killed());
            }
            Ok(())
        }

        fn uninstall(&mut self, i: usize) -> Result<bool, WinError> {
            if !self.spend(format!("uninstall {}", self.nodes[i].instance_id)) {
                return Err(World::killed());
            }
            self.removed.insert(i);
            Ok(false)
        }

        fn wait_working(&mut self, i: usize) -> Bound {
            let n = &self.nodes[i];
            Bound {
                health: n.health,
                inf: n.running.as_ref().map(|r| r.inf.clone()),
                driver_version: n.running.as_ref().map(|r| r.version.clone()),
            }
        }

        fn wait_unloaded(&mut self) -> service::Unload {
            service::Unload::Done(Duration::from_millis(500))
        }

        /// As [`endpoints::before`]: the cables' (a removed one's NOTPRESENT) and those of the nodes `also` names, lingering
        /// ones too.
        fn endpoints_before(&mut self, also: &[String]) -> Result<Vec<Endpoint>, WinError> {
            let wanted = |n: &Node| n.cable.is_some() || also.iter().any(|a| same_instance(a, &n.instance_id));
            let mut out = Vec::new();
            for (i, n) in self.nodes.iter().enumerate().filter(|(_, n)| wanted(n)) {
                if self.removed.contains(&i) || n.health == Health::Problem(22) {
                    out.extend(World::endpoints_of(n, endpoints::State::NotPresent));
                } else if n.health == Health::Working {
                    out.extend(World::endpoints_of(n, endpoints::State::Active));
                }
            }
            for n in self.lingering.iter().filter(|n| wanted(n)) {
                out.extend(World::endpoints_of(n, endpoints::State::NotPresent));
            }
            Ok(out)
        }

        fn wait_endpoints(&mut self, cable: &CableInstance, variant: Variant, names: Names) -> (Option<EndpointCheck>, Option<WinError>) {
            let endpoints_missing = self.late.filter(|l| *l == cable.label.as_str());
            Fake { endpoints_missing, ..Fake::default() }.wait_endpoints(cable, variant, names)
        }

        fn make(&mut self, create: &Create, _: Want, _: &mut dyn FnMut(&str)) -> Result<Made, SetupError> {
            let id = create.instance.instance_id();
            if !self.spend(format!("register {id}")) {
                return Err(SetupError::Windows(World::killed()));
            }
            if self.make_fails_restart == Some(create.instance.label.as_str()) {
                // Registered, its install failed, removed again; Windows finishes the removal at the restart
                return Err(SetupError::StillPresent {
                    device: id,
                    why: "Windows needs a restart to finish removing it".into(),
                    restart: true,
                    error: Box::new(SetupError::Windows(failed("DiInstallDevice"))),
                });
            }
            self.nodes.push(Node {
                instance_id: id.clone(),
                model: crate::setup::plan::Model::Cable(create.variant),
                cable: Some(create.instance.clone()),
                health: Health::Problem(28),
                running: None,
                start_attempts: None,
                problem_status: None,
                config_flags: Some(0),
            });
            if !self.spend(format!("install-new {id}")) {
                return Err(SetupError::Windows(World::killed()));
            }
            let restart = self.restart_make == Some(create.instance.label.as_str());
            let n = self.nodes.last_mut().unwrap();
            n.running = Some(World::published());
            n.health = if restart { Health::Problem(14) } else { Health::Working };
            Ok(Made { cable: create.instance.clone(), restart })
        }

        /// `record A pending`, `record A made` (the nodes it replaces kept), `record A done` (none kept).
        fn record(&mut self, entry: &Entry) -> Result<(), WinError> {
            let what = if entry.pending {
                "pending"
            } else if entry.replaces.is_empty() {
                "done"
            } else {
                "made"
            };
            if !self.spend(format!("record {} {what}", entry.label)) {
                return Err(World::killed());
            }
            // As store::write: Replaced takes every node Replaces names, and keeps them
            let mut entry = entry.clone();
            let kept = self.store.iter().find(|e| e.label == entry.label).map(|e| e.replaced.clone()).unwrap_or_default();
            entry.replaced = store::replaced_after(&kept, &entry).unwrap_or(kept);
            match self.store.iter_mut().find(|e| e.label == entry.label) {
                Some(e) => *e = entry,
                None => self.store.push(entry),
            }
            Ok(())
        }
    }

    const NEW_A2: &str = "a2a2a2a2-0000-4000-8000-0000000000a2";
    const NEW_B2: &str = "b2b2b2b2-0000-4000-8000-0000000000b2";

    /// One run on `world` (new identities `ids` if the store has none), as `migrate` runs it: the map's start check, the
    /// sequence, the map published (`world.map`; a change: a kill before it leaves the file as it was), then (published, or no
    /// map asked for) the old nodes' names dropped once nothing is pending; `None` if there is nothing to migrate (the file
    /// untouched).
    fn run_mapped(world: &mut World, ids: [&str; 2]) -> Option<(Result<Done, SetupError>, Gathered)> {
        let map = world.map.clone();
        endpoint_map::start(map.as_deref()).unwrap();
        let nodes = world.nodes.clone();
        let plan = match plan_migrate(&nodes, &world.store, None, None, [cid(ids[0]), cid(ids[1])]) {
            Ok(p) => p,
            Err(crate::setup::plan::PlanError::NothingToMigrate) => return None,
            Err(e) => panic!("{e}"),
        };
        let mut gathered = Gathered::default();
        let r = run_migrate(world, &nodes, &plan, PUBLISHED, &mut gathered, &mut |_: &str| {});
        let (r, mapped) = match &map {
            None => (r, true),
            Some(_) if !world.spend("publish map".into()) => (r, false),
            Some(path) => {
                let after = world.after();
                endpoint_map::finish(Some(path), "migrate", gathered.clone(), r, &mut || Ok(after.clone()), &mut |_: &str| {})
            }
        };
        if mapped {
            let store = world.store.clone();
            drop_replaces(world, &store, &mut |_: &str| {});
        }
        Some((r, gathered))
    }

    fn run_on(world: &mut World, ids: [&str; 2]) -> Option<Result<Done, SetupError>> {
        run_mapped(world, ids).map(|(r, _)| r)
    }

    /// The cables a run's endpoint map holds, by label, each with the old driver's nodes it replaces.
    fn mapped_of(g: &Gathered) -> Vec<(String, Vec<String>)> {
        let mut m: Vec<(String, Vec<String>)> =
            g.cables.iter().map(|m| (m.cable.label.to_string(), m.replaces().cloned().collect())).collect();
        m.sort();
        m
    }

    /// A and B, each with the old driver's node it replaces on today's box: the whole migration's map.
    fn whole_map() -> Vec<(String, Vec<String>)> {
        vec![("A".into(), vec![r"ROOT\MEDIA\0003".into()]), ("B".into(), vec![r"ROOT\MEDIA\0004".into()])]
    }

    /// The migration as S0a runs it: every cable of ours present, working on the package, A and B recorded as made, nothing
    /// disabled, no old node, nothing pending.
    fn assert_migrated(w: &World, with_c: bool, why: &str) {
        let labels: Vec<&str> = w.nodes.iter().map(|n| n.cable.as_ref().map_or("old", |c| c.label.as_str())).collect();
        assert_eq!(labels, if with_c { vec!["C", "A", "B"] } else { vec!["A", "B"] }, "{why}");
        for n in &w.nodes {
            assert_eq!((n.health, n.running.clone()), (Health::Working, Some(World::published())), "{why}: {}", n.instance_id);
        }
        assert!(w.store.iter().all(|e| !e.pending && e.replaces.is_empty()), "{why}: {:?}", w.store);
        let stored: Vec<&str> = w.store.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(stored, ["A", "B"], "{why}");
        for n in &w.nodes[usize::from(with_c)..] {
            let c = n.cable.as_ref().unwrap();
            assert!(w.store.iter().any(|e| e.label == c.label && e.id == c.id && e.n == c.n), "{why}: {} isn't the store's", n.instance_id);
        }
    }

    /// An endpoint's MMDevice id in the worlds: its node's instance ID and its role.
    fn endpoint_id(instance: &str, kind: EndpointKind) -> String {
        format!("{{{instance}}}.{kind:?}")
    }

    /// A cable's rows in an endpoint map as a reader takes them: role, old id, new id, match.
    type Rows = Vec<(String, Option<String>, Option<String>, String)>;

    /// A cable in an endpoint map as a reader takes it: its label, the old nodes it replaces, its rows.
    type MapCable = (String, Vec<String>, Rows);

    /// What a reader finds in the endpoint map's file, as s0a.sh's map.ps1 reads it: `None` if there is no file; else its
    /// `result`, and each cable (by label) with the old nodes it replaces and its rows. The file is always a whole map.
    fn read_map(path: &Path) -> Option<(String, Vec<MapCable>)> {
        let text = std::fs::read_to_string(path).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("not a whole map ({e}): {text}"));
        assert_eq!(
            (v["format"].as_str(), v["version"].as_u64(), v["command"].as_str()),
            (Some(endpoint_map::FORMAT), Some(1), Some("migrate"))
        );
        let text_of = |v: &serde_json::Value| v.as_str().unwrap().to_string();
        let id = |side: &serde_json::Value| side.get("id").map(text_of);
        let mut cables: Vec<MapCable> = (v["cables"].as_array().unwrap().iter())
            .map(|c| {
                let replaces = c["replaces"].as_array().unwrap().iter().map(text_of).collect();
                let rows = (c["endpoints"].as_array().unwrap().iter())
                    .map(|p| (text_of(&p["role"]), id(&p["old"]), id(&p["new"]), text_of(&p["match"])))
                    .collect();
                (text_of(&c["label"]), replaces, rows)
            })
            .collect();
        cables.sort();
        Some((text_of(&v["result"]), cables))
    }

    /// The whole migration's map for the cables A and B of `w`, as a reader should find it (the owner's decision with Astra,
    /// 2026-11-25): A's three endpoints moved from the old A's; B's Speakers moved from Cable-B.ps1's, and its one microphone to
    /// B's low-latency Microphone (so an app's saved choice keeps working), B's Call Microphone a newly added endpoint.
    fn whole_map_rows(w: &World) -> Vec<MapCable> {
        let inst = |l: &str| w.nodes.iter().find(|n| n.cable.as_ref().is_some_and(|c| c.label.as_str() == l)).unwrap().instance_id.clone();
        let (a, b, old_a, old_b) = (inst("A"), inst("B"), r"ROOT\MEDIA\0003", r"ROOT\MEDIA\0004");
        let id = |instance: &str, kind| Some(endpoint_id(instance, kind));
        let row = |kind: EndpointKind, old, new, m: &str| (kind.name().to_string(), old, new, m.to_string());
        let (spk, mic, call) = (EndpointKind::Speakers, EndpointKind::Microphone, EndpointKind::CallMicrophone);
        vec![
            ("A".into(), vec![old_a.into()], vec![
                row(spk, id(old_a, spk), id(&a, spk), "moved"),
                row(mic, id(old_a, mic), id(&a, mic), "moved"),
                row(call, id(old_a, call), id(&a, call), "moved"),
            ]),
            ("B".into(), vec![old_b.into()], vec![
                row(spk, id(old_b, spk), id(&b, spk), "moved"),
                row(mic, id(old_b, mic), id(&b, mic), "moved"),
                row(call, None, id(&b, call), "added"),
            ]),
        ]
    }

    /// Astra's review (2026-11-25): a migration stopped right after any change it makes (a killed tool, a crash) is finished by
    /// the next run, which never meets the stuck state the review found (a record pending that no plan acts on). Today's box
    /// (the old A and Cable-B.ps1's B), and the same with a cable C on an older package. Each run publishes its endpoint map to
    /// one file (Install-Driver.ps1 -EndpointMap): at every step boundary, the publish and each record's drop of the old nodes'
    /// names among them, a reader finds no map or the whole migration's after the stopped run, the whole migration's after the
    /// retry, and the same after a third run (nothing to do: the file untouched).
    #[test]
    fn a_migration_stopped_after_any_change_is_finished_by_the_next_run() {
        let dir = endpoint_map::tests::scratch("boundaries");
        let map = dir.join("endpoint-map.json");
        let mut c = node("C", "cccccccc-0000-4000-8000-00000000000c", 1, Variant::Call);
        c.running = Some(crate::setup::package::tests::running("oem12.inf", "3.27.41.511"));
        for with_c in [false, true] {
            let mut start = vec![legacy(r"ROOT\MEDIA\0003", Legacy::AudioCodec(Variant::Both)), legacy(r"ROOT\MEDIA\0004", Legacy::CableB)];
            if with_c {
                start.insert(0, c.clone());
            }
            let world = |budget| World { map: Some(map.clone()), ..World::new(start.clone(), vec![], budget) };
            let _ = std::fs::remove_file(&map);
            let mut whole = world(usize::MAX);
            assert_eq!(run_on(&mut whole, [NEW_A, NEW_B]), Some(Ok(Done::Changed)));
            assert_migrated(&whole.next(), with_c, "one run");
            assert_eq!(read_map(&map), Some(("done".into(), whole_map_rows(&whole))), "one run");
            // The step boundaries: each change below is one place a run can stop
            let kinds: Vec<&str> = whole.changes.iter().map(|c| c.split(' ').next().unwrap()).collect();
            // The last three: the map published, then the old nodes' names dropped from A's and B's records
            #[rustfmt::skip]
            let expected: &[&str] = if with_c {
                &["record", "record", "disable", "disable", "disable", "uninstall", "uninstall", "clear", "install", "rename", "clear",
                  "enable", "register", "install-new", "record", "register", "install-new", "record", "publish", "record", "record"]
            } else {
                &["record", "record", "disable", "disable", "uninstall", "uninstall", "register", "install-new", "record", "register",
                  "install-new", "record", "publish", "record", "record"]
            };
            assert_eq!(kinds, expected, "{:?}", whole.changes);
            let published_at = kinds.iter().position(|k| *k == "publish").unwrap();
            for k in 0..whole.changes.len() {
                let why = format!("stopped after {} (change {k}, C {with_c})", k.checked_sub(1).map_or("nothing", |j| &whole.changes[j]));
                let _ = std::fs::remove_file(&map);
                let mut first = world(k);
                let (_, first_map) = run_mapped(&mut first, [NEW_A, NEW_B]).unwrap();
                assert_eq!(first.changes, whole.changes[..k], "{why}");
                // What a reader finds after the stopped run: no map until the run published one, then the whole migration's
                let found = read_map(&map);
                if k > published_at {
                    assert_eq!(found, Some(("done".into(), whole_map_rows(&first))), "{why}");
                    assert_eq!(mapped_of(&first_map), whole_map(), "{why}");
                } else {
                    assert_eq!(found, None, "{why}: no map before the run published one");
                }
                let mut next = first.next();
                let (r, gathered) = run_mapped(&mut next, [NEW_A2, NEW_B2]).unwrap_or_else(|| panic!("{why}: nothing to do"));
                assert_eq!(r, Ok(Done::Changed), "{why}: {:?}", next.changes);
                // The retry maps the whole migration, the cables earlier runs made and recorded too, whether or not their records
                // still keep the old nodes' names (a drop cut short: A's dropped, B's kept), and publishes it whole
                assert_eq!(mapped_of(&gathered), whole_map(), "{why}");
                let done = next.next();
                assert_migrated(&done, with_c, &why);
                assert_eq!(read_map(&map), Some(("done".into(), whole_map_rows(&done))), "{why}: after the retry");
                let left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
                assert_eq!(left, ["endpoint-map.json"], "{why}: no temporary file left");
                let text = std::fs::read_to_string(&map).unwrap();
                let mut again = done.next();
                assert!(run_on(&mut again, [NEW_A2, NEW_B2]).is_none(), "{why}: a third run has nothing to do");
                assert_eq!(std::fs::read_to_string(&map).unwrap(), text, "{why}: the whole map stays");
                // A cable recorded before the stop keeps the identity it was recorded with
                for e in first.store.iter() {
                    assert!(done.store.iter().any(|d| d.label == e.label && d.id == e.id), "{why}: {} changed identity", e.label);
                }
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Astra's review (2026-11-25): a newly made cable stays pending while an expected endpoint is missing. B made, Windows not
    /// making its endpoints within the wait: its record stays pending (the run exits 2, the map says so), `list` says the
    /// migration hasn't finished it, and the next run checks it once they are there and finishes the migration. After a restart
    /// too: a cable made by a run that needs one stays pending through the run after it while its endpoints are late.
    #[test]
    fn a_cable_stays_pending_until_its_endpoints_are_there() {
        let dir = endpoint_map::tests::scratch("late");
        let map = dir.join("endpoint-map.json");
        let start = vec![legacy(r"ROOT\MEDIA\0003", Legacy::AudioCodec(Variant::Both)), legacy(r"ROOT\MEDIA\0004", Legacy::CableB)];
        let mut first = World { map: Some(map.clone()), late: Some("B"), ..World::new(start.clone(), vec![], usize::MAX) };
        let (r, _) = run_mapped(&mut first, [NEW_A, NEW_B]).unwrap();
        let Err(e @ SetupError::Unfinished { to_make, .. }) = &r else { panic!("{r:?}") };
        assert_eq!((to_make.as_slice(), e.needs_restart()), (&[label("B")][..], false), "exit 2");
        let b = first.store.iter().find(|e| e.label.as_str() == "B").unwrap();
        assert!(b.pending && b.replaces.len() == 1, "{b:?}");
        assert!(!first.changes.iter().any(|c| c == "record B made"), "{:?}", first.changes);
        // The map of that run: A whole; B's old endpoints have no new one yet
        let (result, cables) = read_map(&map).unwrap();
        assert_eq!(result, "error");
        assert_eq!(cables[0], whole_map_rows(&first)[0]);
        assert!(cables[1].2.iter().all(|(_, old, _, m)| old.is_some() && m == "gone"), "{:?}", cables[1]);
        // How pending shows
        let nodes = first.next().nodes;
        let report =
            crate::setup::Report { nodes, store: store::Store { entries: first.store.clone(), problems: vec![] }, endpoints: Ok(vec![]) };
        assert!(crate::setup::format_list(&report).contains("note: the migration hasn't finished it (its record is pending)"));
        // The next run: B checked in place, recorded, the whole map published
        let mut next = first.next();
        assert_eq!(run_on(&mut next, [NEW_A2, NEW_B2]), Some(Ok(Done::Changed)));
        assert!(!next.changes.iter().any(|c| c.starts_with("register")), "B isn't made again: {:?}", next.changes);
        assert_migrated(&next.next(), false, "B's endpoints late");
        assert_eq!(read_map(&map), Some(("done".into(), whole_map_rows(&next))));
        // Made by a run that needs a restart, its endpoints still late after the restart: pending until they come
        let mut first = World { map: Some(map.clone()), restart_make: Some("A"), ..World::new(start, vec![], usize::MAX) };
        assert!(matches!(run_on(&mut first, [NEW_A, NEW_B]), Some(Ok(Done::RestartPending { .. }))));
        let mut after_restart = World { late: Some("A"), ..first.next() };
        let r = run_on(&mut after_restart, [NEW_A2, NEW_B2]).unwrap();
        assert!(matches!(&r, Err(SetupError::Unfinished { to_make, .. }) if to_make == &[label("A")]), "{r:?}");
        assert!(after_restart.store.iter().any(|e| e.label.as_str() == "A" && e.pending), "{:?}", after_restart.store);
        let mut last = after_restart.next();
        assert_eq!(run_on(&mut last, [NEW_A2, NEW_B2]), Some(Ok(Done::Changed)));
        assert_migrated(&last.next(), false, "A's endpoints late after the restart");
        assert_eq!(read_map(&map), Some(("done".into(), whole_map_rows(&last))));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A run that leaves no record pending maps every cable the migration made, however it ends, since it drops the old nodes'
    /// names once its map is published. The run that made A and B and recorded them stopped before its publish (their records
    /// keep the names); the next finds A not working (Code 10), so it disables every cable to give A the package, and an app
    /// vetoes B's disable: that run fails (exit 2) with nothing pending, publishes its map, then drops the names. Its map holds
    /// A and B whole.
    #[test]
    fn a_run_stopped_before_its_installs_still_maps_the_migrations_cables() {
        let dir = endpoint_map::tests::scratch("veto");
        let map = dir.join("endpoint-map.json");
        let start = vec![legacy(r"ROOT\MEDIA\0003", Legacy::AudioCodec(Variant::Both)), legacy(r"ROOT\MEDIA\0004", Legacy::CableB)];
        let world = |budget| World { map: Some(map.clone()), ..World::new(start.clone(), vec![], budget) };
        let mut whole = world(usize::MAX);
        run_on(&mut whole, [NEW_A, NEW_B]);
        let publish = whole.changes.iter().position(|c| c == "publish map").unwrap();
        let _ = std::fs::remove_file(&map);
        let mut first = world(publish);
        let _ = run_on(&mut first, [NEW_A, NEW_B]);
        assert_eq!(read_map(&map), None);
        assert!(first.store.iter().all(|e| !e.pending && e.replaces.len() == 1), "{:?}", first.store);
        let mut next = World { veto: Some("B"), ..first.next() };
        let a = next.nodes.iter().position(|n| n.cable.as_ref().is_some_and(|c| c.label.as_str() == "A")).unwrap();
        next.nodes[a].health = Health::Problem(10);
        let (r, gathered) = run_mapped(&mut next, [NEW_A2, NEW_B2]).unwrap();
        let Err(e @ SetupError::Disable { vetoed: true, .. }) = &r else { panic!("{r:?}") };
        assert!(!e.needs_restart(), "exit 2");
        assert_eq!(mapped_of(&gathered), whole_map());
        assert_eq!(read_map(&map), Some(("error".into(), whole_map_rows(&next))), "the whole migration's map, then the names dropped");
        assert!(next.store.iter().all(|e| !e.pending && e.replaces.is_empty()), "{:?}", next.store);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Astra's review (2026-11-25): making A failed and Windows needs a restart to finish removing the failed node. The run
    /// keeps the restart (exit 7) and stops before B; after the restart the next run makes both.
    #[test]
    fn a_restart_to_remove_a_failed_cable_stops_the_migration_before_the_next() {
        let start = vec![legacy(r"ROOT\MEDIA\0003", Legacy::AudioCodec(Variant::Both)), legacy(r"ROOT\MEDIA\0004", Legacy::CableB)];
        let mut first = World { make_fails_restart: Some("A"), ..World::new(start, vec![], usize::MAX) };
        let r = run_on(&mut first, [NEW_A, NEW_B]).unwrap();
        let Err(e) = &r else { panic!("{r:?}") };
        assert!(e.needs_restart(), "exit 7: {e}");
        assert!(!first.changes.iter().any(|c| c.contains("ACXLLCABLE_B")), "B never attempted: {:?}", first.changes);
        assert!(first.store.iter().all(|e| e.pending), "A and B pending for the run after the restart: {:?}", first.store);
        let mut next = first.next();
        assert_eq!(run_on(&mut next, [NEW_A2, NEW_B2]), Some(Ok(Done::Changed)));
        assert_migrated(&next.next(), false, "after the restart");
    }

    /// The case the review found, and the one before it: stopped right after B was made (and before its record), the next run
    /// checks B and records it, changing nothing else; stopped right after B's node was created (registered, its driver not
    /// installed), the next run installs the package on it, enables it and records it, without making another.
    #[test]
    fn a_run_stopped_after_making_a_cable_is_finished_without_making_it_again() {
        let start = vec![legacy(r"ROOT\MEDIA\0003", Legacy::AudioCodec(Variant::Both)), legacy(r"ROOT\MEDIA\0004", Legacy::CableB)];
        let mut whole = World::new(start.clone(), vec![], usize::MAX);
        run_on(&mut whole, [NEW_A, NEW_B]);
        let at = |c: &str| whole.changes.iter().position(|x| x.starts_with(c)).unwrap();
        // After B was made, before its record
        let mut first = World::new(start.clone(), vec![], at("record B made"));
        let _ = run_on(&mut first, [NEW_A, NEW_B]);
        assert!(first.store.iter().any(|e| e.label.as_str() == "B" && e.pending), "{:?}", first.store);
        let mut next = first.next();
        let (r, gathered) = run_mapped(&mut next, [NEW_A2, NEW_B2]).unwrap();
        assert_eq!(r, Ok(Done::Changed));
        assert_eq!(next.changes, ["record B made", "record A done", "record B done"], "checked and recorded, nothing else");
        assert_eq!(mapped_of(&gathered), whole_map(), "A, made and recorded by the first run, is in the map too");
        // After B's node was created, before its install
        let mut first = World::new(start.clone(), vec![], at("install-new ROOT\\MEDIA\\ACXLLCABLE_B"));
        let _ = run_on(&mut first, [NEW_A, NEW_B]);
        assert_eq!(first.nodes.last().unwrap().health, Health::Problem(28));
        let mut next = first.next();
        assert_eq!(run_on(&mut next, [NEW_A2, NEW_B2]), Some(Ok(Done::Changed)));
        let kinds: Vec<&str> = next.changes.iter().map(|c| c.split(' ').next().unwrap()).collect();
        assert!(!kinds.contains(&"register"), "B isn't made again: {:?}", next.changes);
        let b = next.nodes.iter().find(|n| n.cable.as_ref().is_some_and(|c| c.label.as_str() == "B")).unwrap().instance_id.clone();
        for c in [format!("install {b}"), format!("enable {b}"), "record B made".into()] {
            assert!(next.changes.contains(&c), "{c}: {:?}", next.changes);
        }
        assert_migrated(&next.next(), false, "B registered, not installed");
    }

    /// A cable made by a run that ends needing a restart (exit 7) stays pending with its n, as do the cables still to make; the
    /// run after the restart checks it in place (not made again), makes the rest, and maps the whole migration.
    #[test]
    fn a_cable_made_before_a_restart_is_checked_after_it() {
        let start = vec![legacy(r"ROOT\MEDIA\0003", Legacy::AudioCodec(Variant::Both)), legacy(r"ROOT\MEDIA\0004", Legacy::CableB)];
        let mut first = World::new(start, vec![], usize::MAX);
        first.restart_make = Some("A");
        let (r, gathered) = run_mapped(&mut first, [NEW_A, NEW_B]).unwrap();
        assert!(matches!(r, Ok(Done::RestartPending { .. })), "{r:?}");
        assert!(first.store.iter().all(|e| e.pending && e.replaces.len() == 1), "{:?}", first.store);
        assert_eq!(mapped_of(&gathered), whole_map()[..1], "the restart's map: A so far");
        let mut next = first.next();
        let (r, gathered) = run_mapped(&mut next, [NEW_A2, NEW_B2]).unwrap();
        assert_eq!(r, Ok(Done::Changed));
        let kinds: Vec<&str> = next.changes.iter().map(String::as_str).collect();
        assert!(!kinds.iter().any(|c| c.starts_with(r"register ROOT\MEDIA\ACXLLCABLE_A")), "A isn't made again: {kinds:?}");
        assert!(kinds.contains(&"record A made") && kinds.iter().any(|c| c.starts_with(r"register ROOT\MEDIA\ACXLLCABLE_B")), "{kinds:?}");
        assert_eq!(mapped_of(&gathered), whole_map());
        assert_migrated(&next.next(), false, "a restart after A");
    }

    /// A finished cable that fails its checks stays pending, with the reason; one with its identity on two nodes isn't recorded.
    #[test]
    fn a_cable_to_finish_that_fails_its_checks_stays_pending() {
        let published = World::published();
        let mut a = node("A", A_ID, 1, Variant::Both);
        a.running = Some(published.clone());
        let mut b = node("B", B_ID, 1, Variant::Both);
        b.running = Some(published.clone());
        let pending = Entry { pending: true, ..crate::setup::plan::tests::entry("B", B_ID, 1, Some(Variant::Both)) };
        let store = vec![crate::setup::plan::tests::entry("A", A_ID, 1, Some(Variant::Both)), pending];
        // Settled (every cable on the package, nothing old): B checked in place; its endpoints don't come
        let mut fake = Fake { endpoints_missing: Some("B"), ..Fake::default() };
        let (r, _) = migrate(&mut fake, &[a.clone(), b.clone()], &store);
        let Err(SetupError::Unfinished { problems, to_make }) = &r else { panic!("{r:?}") };
        assert_eq!(to_make, &[label("B")]);
        assert!(problems[0].contains("Windows hasn't made"), "{problems:?}");
        assert!(fake.only(&["disable", "install", "enable", "make", "record"]).is_empty(), "{:?}", fake.calls);
        // Its endpoints there, named with Windows' numbering: recorded all the same (as a cable made now would be), the names
        // reported, so a retry can't stick on a name
        let mut fake = Fake { endpoints_numbered: Some("B"), ..Fake::default() };
        let (r, _) = migrate(&mut fake, &[a.clone(), b.clone()], &store);
        let Err(SetupError::Checks(problems)) = &r else { panic!("{r:?}") };
        assert!(problems.iter().all(|p| p.contains("(2- ACX-LL-Cable)")), "{problems:?}");
        assert_eq!(fake.only(&["record"]), ["record B"]);
        // Not working on the package (another one, say): pending, and the full sequence ran to put it on the package
        let mut old = b.clone();
        old.running = Some(crate::setup::package::tests::running("oem12.inf", "3.27.41.511"));
        let mut fake = Fake { install_fails: Some(1), ..Fake::default() };
        let (r, _) = migrate(&mut fake, &[a.clone(), old], &store);
        let Err(SetupError::Unfinished { problems, to_make }) = &r else { panic!("{r:?}") };
        assert_eq!(to_make, &[label("B")]);
        assert!(problems.iter().any(|p| p.contains("another package")), "{problems:?}");
        let unique: BTreeSet<&String> = problems.iter().collect();
        assert_eq!(unique.len(), problems.len(), "each problem once: {problems:?}");
        assert_eq!(fake.only(&["disable", "install", "enable"]), [
            "disable 0", "disable 1", "install 0", "install 1", "enable 0", "enable 1"
        ]);
        // Two nodes with B's identity
        let mut b2 = node("B", B_ID, 2, Variant::Both);
        b2.running = Some(published);
        let mut fake = Fake::default();
        let (r, _) = migrate(&mut fake, &[a, b, b2], &store);
        let Err(SetupError::Unfinished { problems, .. }) = &r else { panic!("{r:?}") };
        assert!(problems[0].contains("identity is on 2 nodes") && problems[0].contains("acx-ll-cable add B"), "{problems:?}");
        assert!(fake.records.is_empty());
    }

    /// Astra's review (2026-11-25): making A fails and Windows needs a restart to finish removing the node that failed. The
    /// migration keeps the restart (exit 7) and stops before B, which is never attempted; A and B stay pending, and the error
    /// says both and the restart.
    #[test]
    fn a_restart_after_a_failed_make_is_exit_7_and_stops_before_b() {
        let nodes = todays_box();
        let mut fake = Fake { make_fails_restart: Some("A"), ..Fake::default() };
        let (r, said, gathered) = migrate_gathering(&mut fake, &nodes, &[]);
        let Err(e) = &r else { panic!("{r:?}") };
        assert!(e.needs_restart(), "exit 7: {e:?}");
        let SetupError::AndRestart { device, error } = e else { panic!("{e:?}") };
        assert_eq!(device, &format!(r"ROOT\MEDIA\ACXLLCABLE_A_{}_1", NEW_A.to_uppercase()), "the failed node's");
        let SetupError::Unfinished { problems, to_make } = &**error else { panic!("{error:?}") };
        assert_eq!(to_make, &[label("A"), label("B")]);
        assert!(problems[0].starts_with("ACX-LL-Cable A: ") && problems[0].contains("isn't confirmed gone"), "{problems:?}");
        assert_eq!(fake.only(&["make"]), ["make A"], "B never attempted");
        assert!(!fake.calls.iter().any(|c| c.ends_with(" B") && !c.starts_with("check") && !c.starts_with("record")), "{:?}", fake.calls);
        assert!(fake.records.iter().all(|e| e.pending), "nothing recorded as made: {:?}", fake.records);
        assert!(fake.disabled.is_empty());
        assert!(said.iter().any(|s| s.contains("nothing more is made before then")), "{said:?}");
        let text = e.to_string();
        assert!(text.contains("ACX-LL-Cable A and ACX-LL-Cable B are recorded in the setup's store, pending"), "{text}");
        assert!(text.contains("Windows needs a restart to finish with"), "{text}");
        assert!(gathered.cables.is_empty(), "nothing made to map");
        // B's make failing that way after A was made: A recorded, the restart kept
        let mut fake = Fake { make_fails_restart: Some("B"), ..Fake::default() };
        let (r, _) = migrate(&mut fake, &nodes, &[]);
        assert!(r.as_ref().is_err_and(SetupError::needs_restart), "{r:?}");
        assert_eq!(fake.only(&["make", "record"]), ["record A pending", "record B pending", "make A", "record A", "make B"]);
        // An error without a restart still goes on to the next cable (exit 2)
        let mut fake = Fake { make_fails: Some("A"), ..Fake::default() };
        let (r, _) = migrate(&mut fake, &nodes, &[]);
        assert!(r.as_ref().is_err_and(|e| !e.needs_restart()) && fake.only(&["make"]) == ["make A", "make B"], "{r:?}");
    }

    /// A node on the package that Windows needs a restart to start (Code 14) after an install or an enable is a restart (exit
    /// 7), not a check that failed: `update` says so, and the migration stops before it makes or checks a cable.
    #[test]
    fn a_node_that_needs_a_restart_to_start_is_a_restart() {
        let nodes = three();
        let mut fake = Fake { start_restart: Some(1), ..Fake::default() };
        let (r, _) = update(&mut fake, &nodes);
        assert_eq!(r, Ok(Done::RestartPending { device: nodes[1].instance_id.clone() }));
        assert!(!fake.calls.iter().any(|c| c == "wait-endpoints B"), "its endpoints come after the restart: {:?}", fake.calls);
        // The migration: C needs the restart after its install, so A and B wait for it, pending
        let mut fake = Fake { start_restart: Some(2), ..Fake::default() };
        let (r, _) = migrate(&mut fake, &todays_box(), &[]);
        assert_eq!(r, Ok(Done::RestartPending { device: todays_box()[2].instance_id.clone() }));
        assert!(fake.only(&["make"]).is_empty() && fake.records.iter().all(|e| e.pending));
        // A cable an earlier run made and didn't record, Code 14 on the package: pending, nothing made before the restart
        let published = World::published();
        let mut b = node("B", B_ID, 1, Variant::Both);
        b.running = Some(published);
        let pending = |l: &str, id: &str| Entry { pending: true, ..crate::setup::plan::tests::entry(l, id, 1, Some(Variant::Both)) };
        let store = vec![pending("A", NEW_A), pending("B", B_ID)];
        let mut fake = Fake { start_restart: Some(0), ..Fake::default() };
        let (r, said) = migrate(&mut fake, &[b.clone()], &store);
        assert_eq!(r, Ok(Done::RestartPending { device: b.instance_id.clone() }));
        assert!(fake.only(&["make", "disable", "install"]).is_empty(), "{:?}", fake.calls);
        assert!(fake.only(&["record"]).iter().all(|c| c.ends_with("pending")), "{:?}", fake.calls);
        assert!(said.iter().any(|s| s.contains("to finish ACX-LL-Cable B and ACX-LL-Cable A, recorded")), "{said:?}");
    }

    fn a_create(n: u32) -> Create {
        Create {
            instance: CableInstance { label: label("A"), id: cid(A_ID), n },
            variant: Variant::Both,
            remove: vec![],
            identity: crate::setup::plan::Identity::Stored,
            same_instance: true,
        }
    }

    /// `add`'s and the variant switch's make ([`make_recorded`]), audited as the migration is (Astra's review, 2026-11-25): a
    /// make that fails with a restart Windows needs is that error (exit 7), nothing recorded or waited for; a cable made that
    /// needs one is recorded (a migration's still pending) and is a restart; a migration's cable is recorded as made only once
    /// its endpoints are there, else left pending (exit 2); any other cable is recorded, then checked.
    #[test]
    fn add_and_the_variant_switch_keep_a_restart_and_a_pending_record() {
        let run = |fake: &mut Fake, rec: Recording| {
            let mut said = Vec::new();
            let r = make_recorded(fake, &a_create(2), Want::Published(PUBLISHED), rec, Names::Since(&[]), &mut |s: &str| {
                said.push(s.to_string())
            });
            (r, said)
        };
        let replaces = [r"ROOT\MEDIA\0003".to_string()];
        let migrations = Recording { replaces: &replaces, pending: true };
        for rec in [Recording::default(), migrations] {
            // The make fails, Windows needing a restart to finish removing the failed node: exit 7, nothing else
            let mut fake = Fake { make_fails_restart: Some("A"), ..Fake::default() };
            let ((made, r), _) = run(&mut fake, rec);
            assert!(made.is_none() && r.as_ref().is_err_and(SetupError::needs_restart), "{r:?}");
            assert_eq!(fake.calls, ["make A"], "nothing recorded or waited for");
            // Made, Windows needing a restart to finish it: recorded (pending still, if it was), a restart, no wait
            let mut fake = Fake { made_restart: Some("A"), ..Fake::default() };
            let ((made, r), _) = run(&mut fake, rec);
            assert_eq!(made.map(|c| c.n), Some(2));
            assert!(matches!(r, Ok(Done::RestartPending { .. })), "{r:?}");
            assert_eq!(fake.only(&["record", "wait-endpoints"]), [if rec.pending { "record A pending" } else { "record A" }]);
            assert_eq!(fake.records[0].replaces, rec.replaces, "the nodes it replaces kept as they are");
            // Made, its endpoints not there: a migration's stays pending (exit 2); another is recorded, its check failed
            let mut fake = Fake { endpoints_missing: Some("A"), ..Fake::default() };
            let ((_, r), _) = run(&mut fake, rec);
            if rec.pending {
                assert!(matches!(&r, Err(SetupError::Unfinished { to_make, .. }) if to_make == &[label("A")]), "{r:?}");
                assert_eq!(fake.only(&["record", "wait-endpoints"]), ["record A pending", "wait-endpoints A"]);
                assert_eq!(fake.records[0].n, 2, "its n recorded");
            } else {
                assert!(matches!(&r, Err(SetupError::Checks(_))), "{r:?}");
                assert_eq!(fake.only(&["record", "wait-endpoints"]), ["record A", "wait-endpoints A"]);
            }
            assert!(!r.unwrap_err().needs_restart());
            // Made, its endpoints there: recorded as made
            let mut fake = Fake::default();
            let ((_, r), _) = run(&mut fake, rec);
            assert_eq!(r, Ok(Done::Changed));
            let recorded =
                if rec.pending { vec!["record A pending", "wait-endpoints A", "record A"] } else { vec!["record A", "wait-endpoints A"] };
            assert_eq!(fake.only(&["record", "wait-endpoints"]), recorded);
            assert!(fake.records.last().is_some_and(|e| !e.pending && e.n == 2));
        }
    }

    /// Astra's review of the first install (2026-11-26): `add`'s repair keeps the restart its install asked for when the enable
    /// after it fails (exit 7, the enable's failure said beside it); without the restart that failure is exit 2, as before. A
    /// persistent disable is cleared by the global change, not CM_Enable_DevNode.
    #[test]
    fn a_repairs_restart_is_kept_when_the_enable_after_it_fails() {
        use windows::Win32::Devices::DeviceAndDriverInstallation::CONFIGFLAG_DISABLED;
        let mut nodes = three();
        nodes[1].health = Health::Problem(PROBLEM_DISABLED);
        let c = candidate(PUBLISHED, "audiocodec.inf", 200, VERSION);
        let run = |fake: &mut Fake, nodes: &[Node]| reinstall(fake, nodes, 1, &(), &c, &mut |_: &str| {});
        let b = nodes[1].instance_id.clone();
        let mut fake = Fake { install_restart: Some(1), enable_fails: Some(1), ..Fake::default() }.with_disabled(1);
        let e = run(&mut fake, &nodes).unwrap_err();
        assert!(e.needs_restart(), "{e:?}");
        assert_eq!(e.restart_device(), Some(b.as_str()));
        let SetupError::AndRestart { error, .. } = &e else { panic!("{e:?}") };
        assert!(matches!(&**error, SetupError::Windows(w) if w.call == "CM_Enable_DevNode"), "{error:?}");
        let text = e.to_string();
        assert!(
            text.starts_with("CM_Enable_DevNode failed") && text.contains(&format!("Windows needs a restart to finish with {b}")),
            "{text}"
        );
        assert_eq!(fake.only(&["clear", "rename", "install", "enable"]), ["clear 1", "rename 1", "install 1", "clear 1", "enable 1"]);
        // The enable fails, no restart asked for: the error alone (exit 2)
        let mut fake = Fake { enable_fails: Some(1), ..Fake::default() }.with_disabled(1);
        let e = run(&mut fake, &nodes).unwrap_err();
        assert!(matches!(&e, SetupError::Windows(w) if w.call == "CM_Enable_DevNode") && !e.needs_restart(), "{e:?}");
        // The restart, the enable working: a restart; neither: working
        assert_eq!(run(&mut Fake { install_restart: Some(1), ..Fake::default() }.with_disabled(1), &nodes), Ok(Installed::Restart));
        assert_eq!(run(&mut Fake::default().with_disabled(1), &nodes), Ok(Installed::Working));
        // A working node isn't enabled
        let mut fake = Fake::default();
        assert_eq!(run(&mut fake, &three()), Ok(Installed::Working));
        assert!(fake.only(&["enable", "state-on"]).is_empty());
        // Disabled persistently (a duplicate's node `add` repairs): the global change, its failure kept with the restart too
        nodes[1].config_flags = Some(CONFIGFLAG_DISABLED.0);
        let mut fake = Fake { install_restart: Some(1), state_fails: Some(1), ..Fake::default() }.with_disabled(1);
        let e = run(&mut fake, &nodes).unwrap_err();
        assert!(e.needs_restart() && fake.only(&["enable", "state-on"]) == ["state-on 1"], "{e:?}");
        // The global change asking for a restart itself: a restart
        let mut fake = Fake { state_restart: Some(1), ..Fake::default() }.with_disabled(1);
        assert_eq!(run(&mut fake, &nodes), Ok(Installed::Restart));
    }

    /// `disable`: that cable's node off for good (the global change), nothing else touched; a restart Windows needs to finish it
    /// is exit 7; a cable disabled for good already is nothing to do.
    #[test]
    fn a_disable_turns_off_only_that_cable_for_good() {
        use windows::Win32::Devices::DeviceAndDriverInstallation::CONFIGFLAG_DISABLED;

        use crate::setup::plan::plan_disable;
        let nodes = three();
        let plan = plan_disable(&label("B"), &[], &nodes).unwrap();
        let disable = |fake: &mut Fake, nodes: &[Node], plan: &StatePlan| {
            let mut said = Vec::new();
            let r = run_disable(fake, nodes, plan, &mut |s: &str| said.push(s.to_string()));
            (r, said)
        };
        let mut fake = Fake::default();
        let (r, said) = disable(&mut fake, &nodes, &plan);
        assert_eq!(r, Ok(Done::Changed));
        assert_eq!(fake.calls, ["state-off 1"], "no other cable, nothing removed or installed");
        assert_eq!((fake.disabled.iter().collect::<Vec<_>>(), fake.persistent.contains(&1)), (vec![&1], true));
        assert!(said.iter().any(|s| s.starts_with("Disabling") && s.contains("every hardware profile: it stays off across restarts")));
        assert!(said.last().unwrap().starts_with("ACX-LL-Cable B is off, across restarts too; its endpoints keep their ids"), "{said:?}");
        // Windows couldn't stop it now: it finishes at the restart (exit 7)
        let mut fake = Fake { state_restart: Some(1), ..Fake::default() };
        let (r, said) = disable(&mut fake, &nodes, &plan);
        assert_eq!(r, Ok(Done::RestartPending { device: nodes[1].instance_id.clone() }));
        assert!(said.iter().any(|s| s.contains("Windows finishes disabling it when it restarts")), "{said:?}");
        // The change failed: its error (exit 2)
        let (r, _) = disable(&mut Fake { state_fails: Some(1), ..Fake::default() }, &nodes, &plan);
        assert!(matches!(&r, Err(SetupError::Windows(w)) if w.call.starts_with("SetupDiCallClassInstaller")), "{r:?}");
        // Disabled for good already: nothing to do, nothing called
        let mut off = nodes.clone();
        off[1].health = Health::Problem(PROBLEM_DISABLED);
        off[1].config_flags = Some(CONFIGFLAG_DISABLED.0);
        let mut fake = Fake::default();
        let (r, said) = disable(&mut fake, &off, &plan_disable(&label("B"), &[], &off).unwrap());
        assert_eq!(r, Ok(Done::Nothing));
        assert!(fake.calls.is_empty() && said[0].starts_with("ACX-LL-Cable B is disabled already: "), "{said:?}");
    }

    /// `enable`: the global change, D1's count cleared first, then the node working and its endpoints back with the ids Windows
    /// had for them (NOTPRESENT while it was off); one with a new id is a failed check. A restart Windows needs is exit 7; a node
    /// enabled that doesn't work is a failed check naming `add`.
    #[test]
    fn an_enable_brings_the_cable_back_with_its_endpoints() {
        use windows::Win32::Devices::DeviceAndDriverInstallation::CONFIGFLAG_DISABLED;

        use crate::setup::plan::plan_enable;
        let mut nodes = three();
        nodes[1].health = Health::Problem(PROBLEM_DISABLED);
        nodes[1].config_flags = Some(CONFIGFLAG_DISABLED.0);
        let b = nodes[1].cable.clone().unwrap();
        // B's endpoints while it is off, with the ids the fake's wait gives them back
        let kept: Vec<Endpoint> = EndpointKind::of_variant(Variant::LowLatency)
            .into_iter()
            .map(|kind| Endpoint {
                id: format!("{}.{kind:?}", b.instance_id()),
                name: endpoints::expected_name(&b, kind),
                kind,
                state: endpoints::State::NotPresent,
                instance_id: b.instance_id().to_lowercase(),
            })
            .collect();
        let plan = plan_enable(&label("B"), &[], &nodes).unwrap();
        let enable = |fake: &mut Fake, nodes: &[Node]| {
            let mut said = Vec::new();
            let r = run_enable(fake, nodes, &plan, &mut |s: &str| said.push(s.to_string()));
            (r, said)
        };
        let off = |fake: Fake| Fake { before: kept.clone(), ..fake }.with_disabled(1);
        let mut fake = off(Fake::default());
        let (r, said) = enable(&mut fake, &nodes);
        assert_eq!(r, Ok(Done::Changed), "{said:?}");
        let kinds = ["clear", "state-on", "state-off", "enable", "disable", "install", "uninstall", "wait-endpoints"];
        assert_eq!(fake.only(&kinds), ["clear 1", "state-on 1", "wait-endpoints B"], "no other cable touched");
        assert!(fake.disabled.is_empty() && !fake.persistent.contains(&1));
        // Windows made one anew (an id it didn't have before): a failed check, the others fine
        let mut fake = Fake { before: kept[1..].to_vec(), ..Fake::default() }.with_disabled(1);
        let (r, _) = enable(&mut fake, &nodes);
        assert_eq!(
            r,
            Err(SetupError::Checks(vec![
                "ACX-LL-Cable B's Speakers came back as a new endpoint, not the one it had (an app finds it by the cable and role)".into()
            ]))
        );
        // A restart to finish it, or to start it (Code 14): exit 7, its endpoints not waited for
        for fake in [Fake { state_restart: Some(1), ..Fake::default() }, Fake { start_restart: Some(1), ..Fake::default() }] {
            let mut fake = off(fake);
            let (r, _) = enable(&mut fake, &nodes);
            assert_eq!(r, Ok(Done::RestartPending { device: nodes[1].instance_id.clone() }));
            assert!(fake.only(&["wait-endpoints"]).is_empty());
        }
        // Enabled, not working: a failed check that names `add`
        let (r, _) = enable(&mut off(Fake { enable_broken: Some(1), ..Fake::default() }), &nodes);
        let Err(SetupError::Checks(p)) = r else { panic!("{r:?}") };
        assert!(p[0].contains("was enabled, but isn't working (a problem: Code 10") && p[0].ends_with("`acx-ll-cable add B` repairs it"));
        // The change failed: its error (exit 2)
        let (r, _) = enable(&mut off(Fake { state_fails: Some(1), ..Fake::default() }), &nodes);
        assert!(matches!(r, Err(SetupError::Windows(_))));
        // Working: nothing to do
        let mut fake = Fake::default();
        let mut said = Vec::new();
        let working = plan_enable(&label("B"), &[], &three()).unwrap();
        assert_eq!(run_enable(&mut fake, &three(), &working, &mut |s: &str| said.push(s.to_string())), Ok(Done::Nothing));
        assert!(fake.calls.is_empty() && said[0].starts_with("ACX-LL-Cable B isn't disabled: "), "{said:?}");
    }

    #[test]
    fn labels_read_as_a_list() {
        assert_eq!(labels(&[]), "nothing");
        assert_eq!(labels(&[label("A")]), "ACX-LL-Cable A");
        assert_eq!(labels(&[label("A"), label("B"), label("C")]), "ACX-LL-Cable A, ACX-LL-Cable B and ACX-LL-Cable C");
    }
}
