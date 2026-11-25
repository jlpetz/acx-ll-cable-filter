//! The sequences `update` and the migration run (TODO 19, the per-cable design, section 9): what is checked, disabled, waited
//! for, removed, installed, enabled and made, in which order, and what is put back when a step fails. They go through [`Ops`],
//! the seam between them and Windows ([`Real`]: SetupAPI, the configuration manager, the service control manager and the
//! store), so the tests run them against a fake and check the calls.
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
//!   missing. One it made and didn't record as made (stopped in between) is present with its record pending: the next run
//!   checks it (its identity on one node, working on the package, its endpoints there) and records it. A later run with the old
//!   driver's nodes gone and every cable on the package already disables and installs nothing.

use super::devices::{Bound, DRIVER_WAIT, Disabled, NewNode, judge_install};
use super::endpoint_map::{self, Gathered};
use super::endpoints::{self, ENDPOINTS_WAIT, Endpoint, EndpointCheck, Names};
use super::package::Candidate;
use super::plan::{Create, Entry, Finish, MigratePlan, Node, UpdatePlan};
use super::{Done, Made, Present, SetupError, SystemClock, Want, choose_driver, lacks, make_or_next, service, store};
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
    /// A new cable's node on the staged package ([`super::make_or_next`]).
    fn make(&mut self, create: &Create, published: &str, say: &mut dyn FnMut(&str)) -> Result<Made, SetupError>;
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

    fn make(&mut self, create: &Create, published: &str, say: &mut dyn FnMut(&str)) -> Result<Made, SetupError> {
        make_or_next(self.present, create, Want::Published(published), say)
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
/// failed, and a node Windows needs a restart to finish.
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
        if let Some(c) = &n.cable
            && let Err(e) = ops.set_friendly_name(*i, &c.label.friendly_name())
        {
            say(&format!("  warning: the friendly name \"{}\" couldn't be set again: {e}", c.label.friendly_name()));
        }
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
            Ok(_) => {
                if let (Some(c), Some(v)) = (&n.cable, n.variant()) {
                    problems.extend(wait_endpoints(ops, c, v, Names::Since(before), say));
                }
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
    let (mut problems, restart) = install_all(ops, nodes, &drivers, &disabled, &before, say);
    match restart {
        Some(device) if problems.is_empty() => Ok(Done::RestartPending { device }),
        Some(device) => {
            problems.push(format!("Windows needs a restart to finish installing {device}"));
            Err(SetupError::Checks(problems))
        }
        None if problems.is_empty() => Ok(Done::Changed),
        None => Err(SetupError::Checks(problems)),
    }
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
    if let Err(failure) = judge_install(Ok(()), false, &now, candidate) {
        return Finished::Pending(vec![format!("{}: {failure}", n.instance_id)]);
    }
    say(&format!("Waiting for Windows to make {name}'s endpoints (up to {} s)", ENDPOINTS_WAIT.as_secs()));
    let found = ops.wait_endpoints(&cable, variant, Names::Since(before));
    let there = found.0.as_ref().is_some_and(|c| c.missing.is_empty());
    let problems = endpoint_problems(&cable, found, say);
    if there { Finished::Checked { cable, variant, misnamed: problems } } else { Finished::Pending(problems) }
}

/// The migration, once the package is staged and the plan made (the module's docs): every check; the cables to make to the
/// store, pending; the old image unloaded; the old driver's nodes removed; the present cables given the package and enabled
/// (none of it when [`settled`]); the cables an earlier run made checked and recorded; the cables made, each recorded as made.
/// `gathered` takes the endpoints before the run and the cables made or finished, for the endpoint map.
pub fn run_migrate<O: Ops>(
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
    let replaced = plan.add.iter().flat_map(|t| &t.replaces).chain(plan.finish.iter().flat_map(|f| &f.replaces));
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
    for f in &plan.finish {
        match check_finish(ops, nodes, f, &drivers, &before, say) {
            Finished::Checked { cable, variant, misnamed } => {
                let entry =
                    Entry { label: f.label.clone(), id: f.id, n: cable.n, mics: Some(variant), pending: false, replaces: Vec::new() };
                match ops.record(&entry) {
                    Ok(()) => say(&format!("  {} recorded as made", f.label.friendly_name())),
                    Err(e) => problems.push(format!("{} is made, but the setup's store couldn't record it ({e})", f.label.friendly_name())),
                }
                problems.extend(misnamed);
                gathered.cables.push(endpoint_map::Made::new(cable, variant, &f.replaces));
            }
            Finished::Pending(why) => {
                problems.extend(why);
                unmade.push(f.label.clone());
            }
        }
    }
    // 7. The cables made; one that fails stays pending in the store, and the others are still made
    for (k, t) in plan.add.iter().enumerate() {
        say(&format!("Adding {} ({})", t.label.friendly_name(), t.variant.describe()));
        let made = match ops.make(&t.create(), published, say) {
            Ok(m) => m,
            Err(e) => {
                problems.push(format!("{}: {e}", t.label.friendly_name()));
                unmade.push(t.label.clone());
                continue;
            }
        };
        let entry = Entry { n: made.cable.n, ..t.entry(false) };
        if let Err(e) = ops.record(&entry) {
            problems.push(format!("{} was made, but the setup's store couldn't record it ({e})", t.label.friendly_name()));
        }
        gathered.cables.push(endpoint_map::Made::new(made.cable.clone(), t.variant, &t.replaces));
        if made.restart {
            let rest: Vec<Label> = unmade.iter().cloned().chain(plan.add[k + 1..].iter().map(|t| t.label.clone())).collect();
            return restart_due(made.cable.instance_id(), problems, rest, say);
        }
        problems.extend(wait_endpoints(ops, &made.cable, t.variant, Names::Since(&before), say));
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

/// The migration's end when Windows needs a restart to finish installing `device`: the cables still `to_make` wait for the
/// next run (recorded in the store, pending), which it says. A restart and nothing else wrong is [`Done::RestartPending`].
fn restart_due(device: String, mut problems: Vec<String>, to_make: Vec<Label>, say: &mut dyn FnMut(&str)) -> Result<Done, SetupError> {
    let text = format!("Windows needs a restart to finish installing {device}");
    if to_make.is_empty() {
        say(&format!("  {text}"));
    } else {
        say(&format!("  {text}; then run this again (Install-Driver.ps1) to make {}, recorded in the setup's store", labels(&to_make)));
    }
    if problems.is_empty() {
        return Ok(Done::RestartPending { device });
    }
    problems.push(text);
    Err(if to_make.is_empty() { SetupError::Checks(problems) } else { SetupError::Unfinished { problems, to_make } })
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
        made_restart: Option<&'static str>,
        /// This cable's endpoints never come.
        endpoints_missing: Option<&'static str>,
        /// This cable's endpoints come with Windows' "2- " numbering in their names.
        endpoints_numbered: Option<&'static str>,
        records: Vec<Entry>,
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
            self.disabled.remove(&i);
            Ok(())
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
            let health = if self.disabled.contains(&i) { Health::Problem(22) } else { Health::Working };
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
            Ok(Vec::new())
        }

        fn wait_endpoints(&mut self, cable: &CableInstance, variant: Variant, names: Names) -> (Option<EndpointCheck>, Option<WinError>) {
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

        fn make(&mut self, create: &Create, _: &str, _: &mut dyn FnMut(&str)) -> Result<Made, SetupError> {
            let l = create.instance.label.as_str();
            self.log(format!("make {l}"));
            if self.make_fails == Some(l) {
                return Err(SetupError::Windows(failed("SetupDiCallClassInstaller(DIF_REGISTERDEVICE)")));
            }
            Ok(Made { cable: create.instance.clone(), restart: self.made_restart == Some(l) })
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
        assert!(fake.records[2..].iter().all(|e| e.replaces.is_empty()), "dropped once made");
        assert_eq!(fake.only(&["endpoints"]), [r"endpoints ROOT\MEDIA\0003 ROOT\MEDIA\0004"]);
        // The endpoint map's input: the endpoints before, and A and B in place of the old nodes
        assert_eq!(gathered.before, Some(vec![]));
        let made: Vec<(&str, Vec<endpoint_map::Old>)> = gathered.cables.iter().map(|m| (m.cable.label.as_str(), m.old.clone())).collect();
        assert_eq!(made, [
            ("A", vec![endpoint_map::Old::Instance(r"ROOT\MEDIA\0003".into()), endpoint_map::Old::Identity(cid(NEW_A))]),
            ("B", vec![endpoint_map::Old::Instance(r"ROOT\MEDIA\0004".into()), endpoint_map::Old::Identity(cid(NEW_B))]),
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
        let text = r.unwrap_err().to_string();
        assert!(text.contains("ACX-LL-Cable A and ACX-LL-Cable B are recorded in the setup's store"), "{text}");
        // A removal that needs a restart stops it the same way, the node it removed not enabled
        let mut fake = Fake { uninstall_restart: Some(0), ..Fake::default() };
        let (r, _) = migrate(&mut fake, &nodes, &[]);
        let Err(SetupError::Stopped { error, .. }) = &r else { panic!("{r:?}") };
        assert!(matches!(**error, SetupError::RestartFirst { .. }));
        assert_eq!(fake.only(&["uninstall", "install", "enable", "make"]), ["uninstall 0", "enable 1", "enable 2"]);
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
        let next = "then run this again (Install-Driver.ps1) to make ACX-LL-Cable A and ACX-LL-Cable B, recorded in the setup's store";
        assert!(said.iter().any(|s| s.contains(next)), "{said:?}");
        assert!(fake.disabled.is_empty());
        // With a failure beside it: an error naming both, and the cables still to make
        let mut with_d = nodes.clone();
        with_d.push(node("D", "dddddddd-0000-4000-8000-00000000000d", 1, Variant::Both));
        let mut fake = Fake { install_restart: Some(2), install_fails: Some(3), ..Fake::default() };
        let (r, _) = migrate(&mut fake, &with_d, &[]);
        let Err(SetupError::Unfinished { problems, to_make }) = &r else { panic!("{r:?}") };
        assert_eq!(to_make, &[label("A"), label("B")]);
        assert!(problems.last().unwrap().starts_with("Windows needs a restart to finish installing"), "{problems:?}");
        assert!(fake.disabled.is_empty());
        // One for a cable made: the rest wait for the next run
        let mut fake = Fake { made_restart: Some("A"), ..Fake::default() };
        let (r, said) = migrate(&mut fake, &nodes, &[]);
        assert!(matches!(r, Ok(Done::RestartPending { .. })), "{r:?}");
        assert_eq!(fake.only(&["make"]), ["make A"]);
        assert!(said.iter().any(|s| s.contains("to make ACX-LL-Cable B, recorded")), "{said:?}");
        assert_eq!(fake.records.iter().filter(|e| !e.pending).map(|e| e.label.as_str()).collect::<Vec<_>>(), ["A"]);
    }

    /// The box as the migration's runs leave it: the nodes (a run's own, by index, and those it makes after them), those removed,
    /// and the store. Every change counts against `budget`; once it is spent, each change fails and does nothing, so the run's
    /// world stays as a kill right after its last change would leave it (the calls after fail without effect). `make` is two
    /// changes, as on Windows: the node registered (present, no driver: Code 28), then installed.
    struct World {
        nodes: Vec<Node>,
        removed: BTreeSet<usize>,
        store: Vec<Entry>,
        budget: usize,
        changes: Vec<String>,
    }

    impl World {
        fn new(nodes: Vec<Node>, store: Vec<Entry>, budget: usize) -> Self {
            World { nodes, removed: BTreeSet::new(), store, budget, changes: Vec::new() }
        }

        /// The next run's world: what is left present, the store, no budget limit.
        fn next(&self) -> World {
            let nodes = self.nodes.iter().enumerate().filter(|(i, _)| !self.removed.contains(i)).map(|(_, n)| n.clone()).collect();
            World::new(nodes, self.store.clone(), usize::MAX)
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

        fn endpoints_before(&mut self, _: &[String]) -> Result<Vec<Endpoint>, WinError> {
            Ok(Vec::new())
        }

        fn wait_endpoints(&mut self, cable: &CableInstance, variant: Variant, names: Names) -> (Option<EndpointCheck>, Option<WinError>) {
            Fake::default().wait_endpoints(cable, variant, names)
        }

        fn make(&mut self, create: &Create, _: &str, _: &mut dyn FnMut(&str)) -> Result<Made, SetupError> {
            let id = create.instance.instance_id();
            if !self.spend(format!("register {id}")) {
                return Err(SetupError::Windows(World::killed()));
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
            let n = self.nodes.last_mut().unwrap();
            n.running = Some(World::published());
            n.health = Health::Working;
            Ok(Made { cable: create.instance.clone(), restart: false })
        }

        fn record(&mut self, entry: &Entry) -> Result<(), WinError> {
            let what = if entry.pending { "pending" } else { "made" };
            if !self.spend(format!("record {} {what}", entry.label)) {
                return Err(World::killed());
            }
            match self.store.iter_mut().find(|e| e.label == entry.label) {
                Some(e) => *e = entry.clone(),
                None => self.store.push(entry.clone()),
            }
            Ok(())
        }
    }

    const NEW_A2: &str = "a2a2a2a2-0000-4000-8000-0000000000a2";
    const NEW_B2: &str = "b2b2b2b2-0000-4000-8000-0000000000b2";

    /// One run on `world` (new identities `ids` if the store has none); `None` if there is nothing to migrate.
    fn run_on(world: &mut World, ids: [&str; 2]) -> Option<Result<Done, SetupError>> {
        let nodes = world.nodes.clone();
        let plan = match plan_migrate(&nodes, &world.store, None, None, [cid(ids[0]), cid(ids[1])]) {
            Ok(p) => p,
            Err(crate::setup::plan::PlanError::NothingToMigrate) => return None,
            Err(e) => panic!("{e}"),
        };
        Some(run_migrate(world, &nodes, &plan, PUBLISHED, &mut Gathered::default(), &mut |_: &str| {}))
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

    /// Astra's review (2026-11-25): a migration stopped right after any change it makes (a killed tool, a crash) is finished by
    /// the next run, which never meets the stuck state the review found (a record pending that no plan acts on). Today's box
    /// (the old A and Cable-B.ps1's B), and the same with a cable C on an older package.
    #[test]
    fn a_migration_stopped_after_any_change_is_finished_by_the_next_run() {
        let mut c = node("C", "cccccccc-0000-4000-8000-00000000000c", 1, Variant::Call);
        c.running = Some(crate::setup::package::tests::running("oem12.inf", "3.27.41.511"));
        for with_c in [false, true] {
            let mut start = vec![legacy(r"ROOT\MEDIA\0003", Legacy::AudioCodec(Variant::Both)), legacy(r"ROOT\MEDIA\0004", Legacy::CableB)];
            if with_c {
                start.insert(0, c.clone());
            }
            let mut whole = World::new(start.clone(), vec![], usize::MAX);
            assert_eq!(run_on(&mut whole, [NEW_A, NEW_B]), Some(Ok(Done::Changed)));
            assert_migrated(&whole.next(), with_c, "one run");
            // The step boundaries: each change below is one place a run can stop
            let kinds: Vec<&str> = whole.changes.iter().map(|c| c.split(' ').next().unwrap()).collect();
            #[rustfmt::skip]
            let expected: &[&str] = if with_c {
                &["record", "record", "disable", "disable", "disable", "uninstall", "uninstall", "clear", "install", "rename", "clear",
                  "enable", "register", "install-new", "record", "register", "install-new", "record"]
            } else {
                &["record", "record", "disable", "disable", "uninstall", "uninstall", "register", "install-new", "record", "register",
                  "install-new", "record"]
            };
            assert_eq!(kinds, expected, "{:?}", whole.changes);
            for k in 0..whole.changes.len() {
                let why = format!("stopped after {} (change {k}, C {with_c})", k.checked_sub(1).map_or("nothing", |j| &whole.changes[j]));
                let mut first = World::new(start.clone(), vec![], k);
                let _ = run_on(&mut first, [NEW_A, NEW_B]);
                assert_eq!(first.changes, whole.changes[..k], "{why}");
                let mut next = first.next();
                assert_eq!(run_on(&mut next, [NEW_A2, NEW_B2]), Some(Ok(Done::Changed)), "{why}: {:?}", next.changes);
                let done = next.next();
                assert_migrated(&done, with_c, &why);
                let mut again = done.next();
                assert!(run_on(&mut again, [NEW_A2, NEW_B2]).is_none(), "{why}: a third run has nothing to do");
                // A cable recorded before the stop keeps the identity it was recorded with
                for e in first.store.iter() {
                    assert!(done.store.iter().any(|d| d.label == e.label && d.id == e.id), "{why}: {} changed identity", e.label);
                }
            }
        }
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
        assert_eq!(run_on(&mut next, [NEW_A2, NEW_B2]), Some(Ok(Done::Changed)));
        assert_eq!(next.changes, ["record B made"], "checked and recorded, nothing else");
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

    #[test]
    fn labels_read_as_a_list() {
        assert_eq!(labels(&[]), "nothing");
        assert_eq!(labels(&[label("A")]), "ACX-LL-Cable A");
        assert_eq!(labels(&[label("A"), label("B"), label("C")]), "ACX-LL-Cable A, ACX-LL-Cable B and ACX-LL-Cable C");
    }
}
