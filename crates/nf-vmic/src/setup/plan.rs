//! What each setup action does with the device nodes there now (TODO 19, the per-cable design, sections 4, 8 and 9). Pure:
//! the nodes, the store and the request in, a plan out; [`super`] carries it out.
//!
//! **Per cable.** A cable is its identity. [`resolve`] finds a label's identity (the store's, or, if the store has none, the
//! one its nodes' instance IDs carry) and the nodes with it; `add`, `variant` and `remove` act on those nodes alone, so no
//! other cable is touched. Only `update` and the migration act on every node of ours, as every cable runs one image.
//!
//! **Instance numbers.** Re-adding a cable keeps its instance ID (its n), so (we expect, to confirm on the box) Windows brings
//! back its endpoints; one that is there but unhealthy is repaired in place, the same node. A variant switch takes n + 1:
//! fresh endpoints, which Windows makes with the variant's default period (TODO 22).

use std::fmt;

use windows::Win32::Devices::DeviceAndDriverInstallation::{DN_HAS_PROBLEM, DN_STARTED};

use crate::identity::{CableId, CableInstance, Label, same_instance};
use crate::setup::package::Running;
use crate::variant::Variant;
use crate::win_error::{configret_name, problem_name};

/// The start attempts at which D1's driver refuses a node (Cable.h, NF_CABLE_START_ATTEMPTS_LIMIT): one start that never
/// completed.
pub const START_ATTEMPTS_LIMIT: u32 = 1;
/// The problem status D1's driver refuses such a node with (Cable.h, NF_STATUS_START_BREAKER), beside Code 31.
pub const STATUS_START_BREAKER: u32 = 0xE0AC_0001;
/// Code 31, CM_PROB_FAILED_ADD: what a node without an identity, or one the breaker refused, shows.
pub const PROBLEM_FAILED_ADD: u32 = 31;
/// Code 22, CM_PROB_DISABLED.
pub const PROBLEM_DISABLED: u32 = 22;

/// A device node's state, from CM_Get_DevNode_Status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Health {
    /// Started, no problem.
    Working,
    /// No problem code, but not started (yet).
    NotStarted,
    /// Device Manager's "Code N" (`CM_PROB_*`): 22 is disabled, 28 no driver installed, 31 a failed AddDevice.
    Problem(u32),
    /// CM_Get_DevNode_Status itself failed with this CONFIGRET.
    Unknown(u32),
}

impl Health {
    pub fn of(configret: u32, status: u32, problem: u32) -> Self {
        if configret != 0 {
            Health::Unknown(configret)
        } else if status & DN_HAS_PROBLEM.0 != 0 {
            Health::Problem(problem)
        } else if status & DN_STARTED.0 != 0 {
            Health::Working
        } else {
            Health::NotStarted
        }
    }

    pub fn describe(self) -> String {
        match self {
            Health::Working => "working".into(),
            Health::NotStarted => "not started".into(),
            Health::Problem(22) => "disabled (Code 22)".into(),
            Health::Problem(28) => "no driver installed (Code 28)".into(),
            Health::Problem(p) => match problem_name(p) {
                Some(name) => format!("a problem: Code {p} ({name})"),
                None => format!("a problem: Code {p}"),
            },
            Health::Unknown(cr) => match configret_name(cr) {
                Some(name) => format!("state unknown (CM_Get_DevNode_Status: {name}, {cr})"),
                None => format!("state unknown (CM_Get_DevNode_Status: CONFIGRET {cr})"),
            },
        }
    }
}

/// The hardware IDs the old driver had, before the per-cable driver (D1) dropped them from the INF.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Legacy {
    /// `ROOT\AudioCodec`, `ROOT\AudioCodecCall`, `ROOT\AudioCodecLowLatency`: today's cable A, with that variant's microphones.
    AudioCodec(Variant),
    /// `ROOT\NoiseFilterCableB`: the cable B that Cable-B.ps1 made (its Speakers and its Microphone).
    CableB,
}

impl Legacy {
    pub const ALL: [Legacy; 4] =
        [Legacy::AudioCodec(Variant::Both), Legacy::AudioCodec(Variant::Call), Legacy::AudioCodec(Variant::LowLatency), Legacy::CableB];

    pub fn hardware_id(self) -> &'static str {
        match self {
            Legacy::AudioCodec(Variant::Both) => r"ROOT\AudioCodec",
            Legacy::AudioCodec(Variant::Call) => r"ROOT\AudioCodecCall",
            Legacy::AudioCodec(Variant::LowLatency) => r"ROOT\AudioCodecLowLatency",
            Legacy::CableB => r"ROOT\NoiseFilterCableB",
        }
    }
}

/// Which of our hardware IDs a node has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Model {
    /// One of the per-cable driver's three, shared by every cable.
    Cable(Variant),
    /// One of the old driver's.
    Legacy(Legacy),
}

impl Model {
    /// Ours among a node's hardware IDs: the whole ID, without case.
    pub fn of(hardware_ids: &[String]) -> Option<Self> {
        hardware_ids.iter().find_map(|id| {
            let id = id.trim();
            Variant::ALL
                .into_iter()
                .find(|v| v.hardware_id().eq_ignore_ascii_case(id))
                .map(Model::Cable)
                .or_else(|| Legacy::ALL.into_iter().find(|l| l.hardware_id().eq_ignore_ascii_case(id)).map(Model::Legacy))
        })
    }

    pub fn hardware_id(self) -> &'static str {
        match self {
            Model::Cable(v) => v.hardware_id(),
            Model::Legacy(l) => l.hardware_id(),
        }
    }
}

/// What the boot crash breaker (D1, Device.cpp) makes of a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Breaker {
    Clear,
    /// Its last start never completed (NfStartAttempts at the limit): its next start will be refused.
    Armed(u32),
    /// Refused now: Code 31 with NF_STATUS_START_BREAKER.
    Refused,
}

/// One present device node with one of our hardware IDs, as the setup reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// `ROOT\MEDIA\ACXLLCABLE_B_<id>_1`, or a node made outside the tool (`ROOT\MEDIA\0003`).
    pub instance_id: String,
    pub model: Model,
    /// The label, identity and n its instance ID carries; `None` for a node made outside the tool.
    pub cable: Option<CableInstance>,
    pub health: Health,
    /// The package it runs, if it has a driver.
    pub running: Option<Running>,
    /// NfStartAttempts in its hardware key, if there.
    pub start_attempts: Option<u32>,
    /// DEVPKEY_Device_ProblemStatus, if there.
    pub problem_status: Option<u32>,
}

impl Node {
    pub fn breaker(&self) -> Breaker {
        if self.health == Health::Problem(PROBLEM_FAILED_ADD) && self.problem_status == Some(STATUS_START_BREAKER) {
            Breaker::Refused
        } else {
            match self.start_attempts {
                Some(n) if n >= START_ATTEMPTS_LIMIT && self.health != Health::Working => Breaker::Armed(n),
                _ => Breaker::Clear,
            }
        }
    }

    /// Working, and the breaker clear: a cable that needs nothing.
    pub fn healthy(&self) -> bool {
        self.health == Health::Working && self.breaker() == Breaker::Clear
    }

    pub fn is_legacy(&self) -> bool {
        matches!(self.model, Model::Legacy(_))
    }

    pub fn variant(&self) -> Option<Variant> {
        match self.model {
            Model::Cable(v) => Some(v),
            Model::Legacy(_) => None,
        }
    }

    pub fn id(&self) -> Option<CableId> {
        self.cable.as_ref().map(|c| c.id)
    }

    /// "ROOT\MEDIA\ACXLLCABLE_B_..._1: both microphones (ROOT\AcxLlCable), working, driver oem12.inf 1.0.0.0".
    pub fn describe(&self) -> String {
        let what = match self.model {
            Model::Cable(v) => v.describe().to_string(),
            Model::Legacy(Legacy::AudioCodec(v)) => format!("the old driver's cable, {}", v.describe()),
            Model::Legacy(Legacy::CableB) => "the old driver's cable B".to_string(),
        };
        let driver = match &self.running {
            Some(r) => format!("driver {r}"),
            None => "no driver".into(),
        };
        let mut text = format!("{}: {what} ({}), {}, {driver}", self.instance_id, self.model.hardware_id(), self.health.describe());
        match self.breaker() {
            Breaker::Clear => {}
            Breaker::Armed(n) => text.push_str(&format!(
                "; its last start never completed (NfStartAttempts {n}), so the driver refuses its next one: add or update clears it"
            )),
            Breaker::Refused => text.push_str(
                "; refused by the driver's crash breaker (its last start never completed): add or update clears it and starts it again",
            ),
        }
        if self.cable.is_none() && !self.is_legacy() {
            text.push_str("; made outside the setup tool, so it has no identity and the driver refuses it (Code 31): `add <label> --adopt` re-creates it");
        }
        text
    }
}

/// The setup's record of a cable, kept across its removal (`HKLM\SOFTWARE\ACX-LL-Cable\Cables\<label>`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub label: Label,
    pub id: CableId,
    /// The instance number its node had last.
    pub n: u32,
    /// The microphones it had last.
    pub mics: Option<Variant>,
}

/// Why an action can't go ahead. Nothing was changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// Nodes with the old driver's hardware IDs are present: the migration comes first.
    Legacy(Vec<String>),
    /// More than one identity is labelled this: the nodes' instance IDs, or the store and a node, disagree.
    Duplicate { label: Label, nodes: Vec<String> },
    /// The store gives this cable's identity to another label too.
    StoreDuplicate { label: Label, other: Label, id: CableId },
    /// A node carries the identity the store has for `label`, under another label.
    Mislabelled { label: Label, node: String },
    /// `variant` on a cable that isn't present.
    NotPresent(Label),
    /// `--adopt`: no present node of ours has that instance ID.
    AdoptNotFound(String),
    /// `--adopt`: that node already has an identity (it is a cable already).
    AdoptHasIdentity(String),
    /// `--adopt`: the old driver's node: the migration's.
    AdoptLegacy(String),
    /// `--adopt`, while the cable is present.
    AdoptPresent { label: Label, node: String },
    /// The instance number can't grow any more.
    NoNextInstance(Label),
    /// `update`: no node of ours is present.
    NothingToUpdate,
    /// The migration: no node of the old driver is present.
    NothingToMigrate,
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::Legacy(ids) => write!(
                f,
                "the old driver's devices are present ({}): migrate first (driver\\scripts\\Install-Driver.ps1, or `acx-ll-cable migrate \
                 <package folder>`). Nothing was changed.",
                ids.join(", ")
            ),
            PlanError::Duplicate { label, nodes } => write!(
                f,
                "more than one cable is labelled {label} ({}): a setup error. `acx-ll-cable remove {label}` removes every one, then `add \
                 {label}` makes it again. Nothing was changed.",
                nodes.join(", ")
            ),
            PlanError::StoreDuplicate { label, other, id } => write!(
                f,
                "the setup's store gives {label}'s identity ({id}) to {other} too: `acx-ll-cable remove {other} --forget` drops \
                 {other}'s record. Nothing was changed."
            ),
            PlanError::Mislabelled { label, node } => write!(
                f,
                "{node} carries {label}'s identity under another label: remove it (`acx-ll-cable remove {label}`), then add {label} \
                 again. Nothing was changed."
            ),
            PlanError::NotPresent(label) => {
                write!(f, "cable {label} isn't present: `acx-ll-cable add {label} --mics <m>` makes it with those microphones")
            }
            PlanError::AdoptNotFound(id) => write!(f, "no present device of ours is {id}: `acx-ll-cable list` shows them"),
            PlanError::AdoptHasIdentity(id) => write!(f, "{id} is a cable already (its instance ID has an identity): nothing to adopt"),
            PlanError::AdoptLegacy(id) => {
                write!(f, "{id} is the old driver's device: the migration replaces it (`acx-ll-cable migrate <package folder>`)")
            }
            PlanError::AdoptPresent { label, node } => {
                write!(f, "cable {label} is present ({node}): remove it first, or adopt the device under another label")
            }
            PlanError::NoNextInstance(label) => write!(f, "cable {label}'s instance number can't grow any more"),
            PlanError::NothingToUpdate => {
                write!(f, "no device of ours is present: `acx-ll-cable add A` makes the first cable (Install-Driver.ps1 does)")
            }
            PlanError::NothingToMigrate => write!(f, "no device of the old driver is present: nothing to migrate"),
        }
    }
}

impl std::error::Error for PlanError {}

/// A label's cable: its identity, the highest instance number it has had, its microphones as last set, and its present nodes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub label: Label,
    pub id: CableId,
    pub n: u32,
    pub mics: Option<Variant>,
    /// Indices into the nodes: those with this identity.
    pub nodes: Vec<usize>,
    /// Whether the store had it; if not, it came from a node, and the store is written afresh.
    pub stored: bool,
}

/// The legacy nodes, if any: every per-cable action but `remove --all` waits for the migration.
fn refuse_legacy(nodes: &[Node]) -> Result<(), PlanError> {
    let legacy: Vec<String> = nodes.iter().filter(|n| n.is_legacy()).map(|n| n.instance_id.clone()).collect();
    if legacy.is_empty() { Ok(()) } else { Err(PlanError::Legacy(legacy)) }
}

/// The per-cable filter: `label`'s identity and its nodes. The store's identity when it has one; else the one the nodes
/// labelled so carry. `None`: neither knows the label.
pub fn resolve(label: &Label, store: &[Entry], nodes: &[Node]) -> Result<Option<Resolved>, PlanError> {
    let entry = store.iter().find(|e| &e.label == label);
    if let Some(e) = entry
        && let Some(other) = store.iter().find(|o| o.id == e.id && &o.label != label)
    {
        return Err(PlanError::StoreDuplicate { label: label.clone(), other: other.label.clone(), id: e.id });
    }
    let labelled: Vec<usize> = (0..nodes.len()).filter(|&i| nodes[i].cable.as_ref().is_some_and(|c| &c.label == label)).collect();
    let id = match entry {
        Some(e) => {
            // A node named for this label with another identity, or with this identity under another label
            let others: Vec<String> =
                labelled.iter().filter(|&&i| nodes[i].id() != Some(e.id)).map(|&i| nodes[i].instance_id.clone()).collect();
            if !others.is_empty() {
                let mut all = vec![format!("{label}'s own, {}", e.id)];
                all.extend(others);
                return Err(PlanError::Duplicate { label: label.clone(), nodes: all });
            }
            if let Some(n) = nodes.iter().find(|n| n.cable.as_ref().is_some_and(|c| c.id == e.id && &c.label != label)) {
                return Err(PlanError::Mislabelled { label: label.clone(), node: n.instance_id.clone() });
            }
            e.id
        }
        None => {
            let mut ids: Vec<CableId> = labelled.iter().filter_map(|&i| nodes[i].id()).collect();
            ids.sort_unstable();
            ids.dedup();
            match ids.as_slice() {
                [] => return Ok(None),
                [id] => {
                    // ... and no store entry gives that identity to another label
                    if let Some(other) = store.iter().find(|o| o.id == *id) {
                        return Err(PlanError::StoreDuplicate { label: label.clone(), other: other.label.clone(), id: *id });
                    }
                    *id
                }
                _ => {
                    let all = labelled.iter().map(|&i| nodes[i].instance_id.clone()).collect();
                    return Err(PlanError::Duplicate { label: label.clone(), nodes: all });
                }
            }
        }
    };
    let own: Vec<usize> = (0..nodes.len()).filter(|&i| nodes[i].id() == Some(id)).collect();
    let n_nodes = own.iter().filter_map(|&i| nodes[i].cable.as_ref().map(|c| c.n)).max();
    let n = entry.map(|e| e.n).into_iter().chain(n_nodes).max().unwrap_or(1);
    // The microphones as last set: the newest node's, else the store's
    let newest = own.iter().copied().max_by_key(|&i| nodes[i].cable.as_ref().map_or(0, |c| c.n));
    let mics = newest.and_then(|i| nodes[i].variant()).or(entry.and_then(|e| e.mics));
    Ok(Some(Resolved { label: label.clone(), id, n, mics, nodes: own, stored: entry.is_some() }))
}

/// Where a new node's identity comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Identity {
    /// The store's.
    Stored,
    /// A present node's, the store lacking it: written to the store afresh.
    FromNode,
    /// New: the label was never seen.
    New,
}

/// A node to create: the cable's instance, its variant, and the cable's own nodes to remove first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Create {
    pub instance: CableInstance,
    pub variant: Variant,
    /// Indices into the nodes; all of this cable's (or the adopted node).
    pub remove: Vec<usize>,
    pub identity: Identity,
    /// The instance ID this cable had before (re-added or re-created): its endpoints should come back. Otherwise new.
    pub same_instance: bool,
}

/// What `add` does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddPlan {
    /// Present and healthy: nothing, so `add` is idempotent. `prune`: the cable's other nodes, not working (a leftover), to
    /// remove; `mics_differ`: microphones asked for that it doesn't have (`variant` switches).
    Nothing {
        node: usize,
        prune: Vec<usize>,
        mics_differ: Option<Variant>,
        identity: Identity,
    },
    /// Present but unhealthy (disabled, failing, refused by the crash breaker, or a duplicate): its newest node gets the
    /// package again in place, so it keeps its identity and instance ID (and its endpoints), its start count cleared; the
    /// cable's other nodes pruned. Its microphones stay (`mics_differ`: `variant` switches).
    Repair {
        node: usize,
        prune: Vec<usize>,
        mics_differ: Option<Variant>,
        identity: Identity,
    },
    Create(Create),
}

/// `add <label> [--mics m] [--adopt instance]`. `new_id` is the identity a cable never seen gets.
pub fn plan_add(
    label: &Label, mics: Option<Variant>, adopt: Option<&str>, store: &[Entry], nodes: &[Node], new_id: CableId,
) -> Result<AddPlan, PlanError> {
    if let Some(a) = adopt
        && let Some(n) = nodes.iter().find(|n| same_instance(&n.instance_id, a))
        && n.is_legacy()
    {
        return Err(PlanError::AdoptLegacy(n.instance_id.clone()));
    }
    refuse_legacy(nodes)?;
    let resolved = resolve(label, store, nodes)?;
    let identity = match &resolved {
        Some(r) if r.stored => Identity::Stored,
        Some(_) => Identity::FromNode,
        None => Identity::New,
    };
    let (id, n, last_mics) = match &resolved {
        Some(r) => (r.id, r.n, r.mics),
        None => (new_id, 1, None),
    };
    let instance = |n| CableInstance { label: label.clone(), id, n };
    if let Some(a) = adopt {
        let Some(i) = nodes.iter().position(|n| same_instance(&n.instance_id, a)) else {
            return Err(PlanError::AdoptNotFound(a.into()));
        };
        if nodes[i].cable.is_some() {
            return Err(PlanError::AdoptHasIdentity(nodes[i].instance_id.clone()));
        }
        if let Some(&p) = resolved.as_ref().and_then(|r| r.nodes.first()) {
            return Err(PlanError::AdoptPresent { label: label.clone(), node: nodes[p].instance_id.clone() });
        }
        let variant = mics.or(nodes[i].variant()).unwrap_or(Variant::Both);
        return Ok(AddPlan::Create(Create {
            instance: instance(n),
            variant,
            remove: vec![i],
            identity,
            same_instance: resolved.is_some(),
        }));
    }
    let own = resolved.as_ref().map(|r| r.nodes.clone()).unwrap_or_default();
    let healthy: Vec<usize> = own.iter().copied().filter(|&i| nodes[i].healthy()).collect();
    if let [keep] = healthy.as_slice() {
        let prune = own.iter().copied().filter(|i| i != keep).collect();
        let mics_differ = mics.filter(|&m| nodes[*keep].variant() != Some(m));
        return Ok(AddPlan::Nothing { node: *keep, prune, mics_differ, identity });
    }
    // Present but unhealthy (or several working: a setup error mended the same way): the newest node repaired in place
    if let Some(&newest) = own.iter().max_by_key(|&&i| nodes[i].cable.as_ref().map_or(0, |c| c.n)) {
        let prune = own.iter().copied().filter(|&i| i != newest).collect();
        let mics_differ = mics.filter(|&m| nodes[newest].variant() != Some(m));
        return Ok(AddPlan::Repair { node: newest, prune, mics_differ, identity });
    }
    // Absent: created with its identity and the instance ID it had, or new
    let variant = mics.or(last_mics).unwrap_or(Variant::Both);
    Ok(AddPlan::Create(Create { instance: instance(n), variant, remove: Vec::new(), identity, same_instance: resolved.is_some() }))
}

/// What `remove` does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemovePlan {
    /// Indices into the nodes.
    pub remove: Vec<usize>,
    /// Store records to drop (`--forget`).
    pub forget: Vec<Label>,
}

/// What `remove` acts on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoveTarget {
    /// One cable: the nodes with its identity, and any other node labelled so (a duplicate).
    Label(Label),
    /// Every node of ours, the old driver's and those without an identity too.
    All,
}

pub fn plan_remove(target: &RemoveTarget, forget: bool, store: &[Entry], nodes: &[Node]) -> RemovePlan {
    match target {
        RemoveTarget::All => RemovePlan {
            remove: (0..nodes.len()).collect(),
            forget: if forget { store.iter().map(|e| e.label.clone()).collect() } else { Vec::new() },
        },
        RemoveTarget::Label(label) => {
            let stored = store.iter().find(|e| &e.label == label).map(|e| e.id);
            let remove =
                (0..nodes.len()).filter(|&i| nodes[i].cable.as_ref().is_some_and(|c| &c.label == label || Some(c.id) == stored)).collect();
            let forget = if forget && stored.is_some() { vec![label.clone()] } else { Vec::new() };
            RemovePlan { remove, forget }
        }
    }
}

/// What `variant` does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VariantPlan {
    /// The cable is that variant already, working: kept (its microphones are checked next).
    Keep { node: usize },
    /// Re-created with n + 1 and the variant's hardware ID; its old nodes removed.
    Switch(Create),
}

/// `variant <label> <mics>`: only that cable's nodes. `recreate` re-creates a cable that is that variant already (its
/// microphones didn't come).
pub fn plan_variant(label: &Label, target: Variant, recreate: bool, store: &[Entry], nodes: &[Node]) -> Result<VariantPlan, PlanError> {
    refuse_legacy(nodes)?;
    let Some(r) = resolve(label, store, nodes)? else { return Err(PlanError::NotPresent(label.clone())) };
    if r.nodes.is_empty() {
        return Err(PlanError::NotPresent(label.clone()));
    }
    if let [only] = r.nodes.as_slice()
        && !recreate
        && nodes[*only].healthy()
        && nodes[*only].variant() == Some(target)
    {
        return Ok(VariantPlan::Keep { node: *only });
    }
    let n = r.n.checked_add(1).ok_or_else(|| PlanError::NoNextInstance(label.clone()))?;
    let identity = if r.stored { Identity::Stored } else { Identity::FromNode };
    Ok(VariantPlan::Switch(Create {
        instance: CableInstance { label: label.clone(), id: r.id, n },
        variant: target,
        remove: r.nodes,
        identity,
        same_instance: false,
    }))
}

/// What `update` does: every node of ours, since every cable runs one image.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UpdatePlan {
    /// Disabled (without persistence) so the image unloads, then enabled again: every node not disabled already.
    pub disable: Vec<usize>,
    /// Disabled before the update (the user's): left disabled, though its driver is updated.
    pub left_disabled: Vec<usize>,
    /// Given the new package: every node with an identity.
    pub install: Vec<usize>,
    /// Without an identity (made outside the tool): disabled and enabled with the rest, not installed (the driver refuses
    /// them; `add --adopt` makes them cables).
    pub unidentified: Vec<usize>,
}

pub fn plan_update(nodes: &[Node]) -> Result<UpdatePlan, PlanError> {
    refuse_legacy(nodes)?;
    if nodes.is_empty() {
        return Err(PlanError::NothingToUpdate);
    }
    Ok(unload_and_install(nodes, |_| true))
}

/// The disable, install and re-enable sets over the nodes `include` takes.
fn unload_and_install(nodes: &[Node], include: impl Fn(&Node) -> bool) -> UpdatePlan {
    let mut plan = UpdatePlan::default();
    for (i, n) in nodes.iter().enumerate().filter(|(_, n)| include(n)) {
        if n.health == Health::Problem(PROBLEM_DISABLED) {
            plan.left_disabled.push(i);
        } else {
            plan.disable.push(i);
        }
        if n.cable.is_some() {
            plan.install.push(i);
        } else {
            plan.unidentified.push(i);
        }
    }
    plan
}

/// The migration from the old driver's devices (TODO 19, S0a): what to remove, what to update, what to add.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigratePlan {
    /// The old driver's nodes: disabled, then (the image unloaded) removed.
    pub legacy: Vec<usize>,
    /// The other nodes of ours: disabled with them, so the old image can unload, then given the new package as `update` does.
    pub update: UpdatePlan,
    /// The cables to make in their place: A for the old driver's cable (its variant, unless `a_mics`), B for Cable-B.ps1's
    /// (`b_mics`), each unless a cable of that label is present already.
    pub add: Vec<(Label, Variant)>,
}

pub fn plan_migrate(nodes: &[Node], store: &[Entry], a_mics: Option<Variant>, b_mics: Variant) -> Result<MigratePlan, PlanError> {
    let legacy: Vec<usize> = (0..nodes.len()).filter(|&i| nodes[i].is_legacy()).collect();
    if legacy.is_empty() {
        return Err(PlanError::NothingToMigrate);
    }
    let rest: Vec<Node> = nodes.iter().filter(|n| !n.is_legacy()).cloned().collect();
    let present = |label: &Label| -> Result<bool, PlanError> { Ok(resolve(label, store, &rest)?.is_some_and(|r| !r.nodes.is_empty())) };
    let mut add = Vec::new();
    // Today's A: the first working one's variant, else the first's
    let old_a = legacy
        .iter()
        .filter_map(|&i| match nodes[i].model {
            Model::Legacy(Legacy::AudioCodec(v)) => Some((nodes[i].health == Health::Working, v)),
            _ => None,
        })
        .max_by_key(|&(working, _)| working);
    if let Some((_, v)) = old_a
        && !present(&Label::a())?
    {
        add.push((Label::a(), a_mics.unwrap_or(v)));
    }
    let b: Label = "B".parse().expect("a label");
    if legacy.iter().any(|&i| nodes[i].model == Model::Legacy(Legacy::CableB)) && !present(&b)? {
        add.push((b, b_mics));
    }
    let mut update = unload_and_install(nodes, |n| !n.is_legacy());
    // The old driver's nodes are disabled too (they hold the old image), and removed rather than installed
    for &i in &legacy {
        if nodes[i].health != Health::Problem(PROBLEM_DISABLED) {
            update.disable.push(i);
        }
    }
    update.disable.sort_unstable();
    Ok(MigratePlan { legacy, update, add })
}

/// `list`'s view (pure): the present cables by identity, the remembered absent ones, and what isn't a working cable.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    pub cables: Vec<Listed>,
    /// In the store, no node present.
    pub absent: Vec<Entry>,
    pub legacy: Vec<usize>,
    /// Our hardware ID, no identity in its instance ID.
    pub unidentified: Vec<usize>,
    /// Setup errors: an identity on two nodes, or a label on two identities.
    pub problems: Vec<String>,
}

/// One present cable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub label: Label,
    pub id: CableId,
    pub nodes: Vec<usize>,
    pub stored: Option<Entry>,
}

pub fn listing(store: &[Entry], nodes: &[Node]) -> Listing {
    let mut out = Listing::default();
    for (i, n) in nodes.iter().enumerate() {
        match &n.cable {
            None if n.is_legacy() => out.legacy.push(i),
            None => out.unidentified.push(i),
            Some(c) => match out.cables.iter_mut().find(|l| l.id == c.id) {
                Some(l) => l.nodes.push(i),
                None => out.cables.push(Listed {
                    label: c.label.clone(),
                    id: c.id,
                    nodes: vec![i],
                    stored: store.iter().find(|e| e.id == c.id).cloned(),
                }),
            },
        }
    }
    out.cables.sort_by(|a, b| a.label.cmp(&b.label).then(a.id.cmp(&b.id)));
    for c in &out.cables {
        if c.nodes.len() > 1 {
            let ids: Vec<&str> = c.nodes.iter().map(|&i| nodes[i].instance_id.as_str()).collect();
            out.problems.push(format!(
                "cable {}'s identity is on {} nodes ({}): only one gets a control device",
                c.label,
                ids.len(),
                ids.join(", ")
            ));
        }
        if c.nodes.iter().any(|&i| nodes[i].cable.as_ref().is_some_and(|x| x.label != c.label)) {
            out.problems.push(format!("cable {}'s identity is on nodes with another label", c.label));
        }
        if let Some(e) = &c.stored
            && e.label != c.label
        {
            out.problems.push(format!("the store has {}'s identity as {}", c.label, e.label));
        }
    }
    for w in out.cables.windows(2) {
        if w[0].label == w[1].label {
            out.problems.push(format!("more than one cable is labelled {} ({} and {})", w[0].label, w[0].id, w[1].id));
        }
    }
    out.absent = store.iter().filter(|e| !out.cables.iter().any(|c| c.id == e.id)).cloned().collect();
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::setup::package::tests::running;

    pub(crate) const A_ID: &str = "aaaaaaaa-0000-4000-8000-00000000000a";
    pub(crate) const B_ID: &str = "bbbbbbbb-0000-4000-8000-00000000000b";
    const NEW_ID: &str = "dddddddd-0000-4000-8000-00000000000d";

    pub(crate) fn label(s: &str) -> Label {
        s.parse().unwrap()
    }

    pub(crate) fn cid(s: &str) -> CableId {
        s.parse().unwrap()
    }

    /// A working cable node of the given label, identity, n and variant, on oem12.inf.
    pub(crate) fn node(l: &str, id: &str, n: u32, v: Variant) -> Node {
        let cable = CableInstance { label: label(l), id: cid(id), n };
        Node {
            instance_id: cable.instance_id(),
            model: Model::Cable(v),
            cable: Some(cable),
            health: Health::Working,
            running: Some(running("oem12.inf", "3.27.41.512")),
            start_attempts: None,
            problem_status: None,
        }
    }

    fn legacy(instance: &str, l: Legacy) -> Node {
        Node {
            instance_id: instance.into(),
            model: Model::Legacy(l),
            cable: None,
            health: Health::Working,
            running: Some(running("oem7.inf", "1.0.0.0")),
            start_attempts: None,
            problem_status: None,
        }
    }

    fn entry(l: &str, id: &str, n: u32, mics: Option<Variant>) -> Entry {
        Entry { label: label(l), id: cid(id), n, mics }
    }

    /// Two cables present, A (both) and B (low-latency), both in the store.
    fn two() -> (Vec<Entry>, Vec<Node>) {
        let store = vec![entry("A", A_ID, 1, Some(Variant::Both)), entry("B", B_ID, 1, Some(Variant::LowLatency))];
        (store, vec![node("A", A_ID, 1, Variant::Both), node("B", B_ID, 1, Variant::LowLatency)])
    }

    #[test]
    fn our_hardware_ids_name_the_model() {
        let ids = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(Model::of(&ids(&[r"ROOT\AcxLlCable"])), Some(Model::Cable(Variant::Both)));
        assert_eq!(Model::of(&ids(&[r"root\acxllcablecall"])), Some(Model::Cable(Variant::Call)));
        assert_eq!(Model::of(&ids(&[r"ROOT\AcxLlCableLowLatency"])), Some(Model::Cable(Variant::LowLatency)));
        assert_eq!(Model::of(&ids(&[r"ROOT\AudioCodec"])), Some(Model::Legacy(Legacy::AudioCodec(Variant::Both))));
        assert_eq!(Model::of(&ids(&[r"ROOT\AudioCodecCall"])), Some(Model::Legacy(Legacy::AudioCodec(Variant::Call))));
        assert_eq!(Model::of(&ids(&[r"PCI\X", r"ROOT\NoiseFilterCableB"])), Some(Model::Legacy(Legacy::CableB)));
        // The whole ID: ROOT\AcxLlCable is a prefix of the other two
        assert_eq!(Model::of(&ids(&[r"ROOT\AcxLlCableX"])), None);
        assert_eq!(Model::of(&ids(&[r"ROOT\VBAudioVACWDM"])), None);
        assert_eq!(Model::of(&[]), None);
    }

    #[test]
    fn the_breaker_reads_as_d1_sets_it() {
        let mut n = node("A", A_ID, 1, Variant::Both);
        assert_eq!(n.breaker(), Breaker::Clear);
        n.start_attempts = Some(1);
        assert_eq!(n.breaker(), Breaker::Clear, "counted while working: a start in progress, cleared once it completes");
        n.health = Health::NotStarted;
        assert_eq!(n.breaker(), Breaker::Armed(1));
        assert!(!n.healthy());
        n.health = Health::Problem(31);
        n.problem_status = Some(STATUS_START_BREAKER);
        assert_eq!(n.breaker(), Breaker::Refused);
        assert!(n.describe().contains("refused by the driver's crash breaker"), "{}", n.describe());
        n.problem_status = Some(0xC000_0182);
        assert_eq!(n.breaker(), Breaker::Armed(1), "Code 31 for another reason, the count still at the limit");
    }

    /// The per-cable filter: a label's identity and only its nodes, with two cables present.
    #[test]
    fn a_label_resolves_to_its_own_nodes() {
        let (store, nodes) = two();
        let a = resolve(&label("A"), &store, &nodes).unwrap().unwrap();
        assert_eq!((a.id, a.nodes.clone(), a.n, a.mics, a.stored), (cid(A_ID), vec![0], 1, Some(Variant::Both), true));
        let b = resolve(&label("b"), &store, &nodes).unwrap().unwrap();
        assert_eq!((b.id, b.nodes), (cid(B_ID), vec![1]));
        assert_eq!(resolve(&label("C"), &store, &nodes), Ok(None));
        // Absent but remembered: the store's identity and n, no nodes
        let gone = resolve(&label("B"), &store, &nodes[..1]).unwrap().unwrap();
        assert_eq!((gone.id, gone.nodes.len(), gone.n), (cid(B_ID), 0, 1));
        // Not in the store: from the nodes' instance IDs
        let b = resolve(&label("B"), &store[..1], &nodes).unwrap().unwrap();
        assert_eq!((b.id, b.stored), (cid(B_ID), false));
        // n: the higher of the store's and the nodes'
        let store3 = vec![entry("B", B_ID, 3, None)];
        assert_eq!(resolve(&label("B"), &store3, &nodes).unwrap().unwrap().n, 3);
        assert_eq!(resolve(&label("B"), &[entry("B", B_ID, 1, None)], &[node("B", B_ID, 5, Variant::Call)]).unwrap().unwrap().n, 5);
    }

    #[test]
    fn duplicate_identities_and_labels_are_refused() {
        let (store, mut nodes) = two();
        // Another node labelled B with another identity
        nodes.push(node("B", NEW_ID, 1, Variant::Both));
        assert!(matches!(resolve(&label("B"), &store, &nodes), Err(PlanError::Duplicate { .. })));
        assert!(matches!(resolve(&label("B"), &store[..1], &nodes), Err(PlanError::Duplicate { .. })), "no store entry either");
        // A is untouched by B's trouble
        assert!(resolve(&label("A"), &store, &nodes).is_ok());
        // A's identity under the label C
        let (store, mut nodes) = two();
        nodes.push(node("C", A_ID, 2, Variant::Both));
        assert!(matches!(resolve(&label("A"), &store, &nodes), Err(PlanError::Mislabelled { .. })));
        // The store gives one identity to two labels
        let store = vec![entry("A", A_ID, 1, None), entry("Z", A_ID, 1, None)];
        assert!(matches!(resolve(&label("A"), &store, &[]), Err(PlanError::StoreDuplicate { .. })));
        let text = resolve(&label("A"), &store, &[]).unwrap_err().to_string();
        assert!(text.contains("remove Z --forget"), "{text}");
    }

    #[test]
    fn add_is_idempotent_for_a_healthy_cable_and_leaves_the_other_alone() {
        let (store, nodes) = two();
        let p = plan_add(&label("B"), None, None, &store, &nodes, cid(NEW_ID)).unwrap();
        assert_eq!(p, AddPlan::Nothing { node: 1, prune: vec![], mics_differ: None, identity: Identity::Stored });
        // Other microphones asked for: no change, and it says `variant` switches
        let p = plan_add(&label("B"), Some(Variant::Call), None, &store, &nodes, cid(NEW_ID)).unwrap();
        assert!(matches!(p, AddPlan::Nothing { mics_differ: Some(Variant::Call), .. }));
    }

    #[test]
    fn add_makes_a_new_cable_beside_two_others() {
        let (store, nodes) = two();
        let AddPlan::Create(c) = plan_add(&label("D"), None, None, &store, &nodes, cid(NEW_ID)).unwrap() else { panic!() };
        assert_eq!(c.instance, CableInstance { label: label("D"), id: cid(NEW_ID), n: 1 });
        assert_eq!((c.variant, c.remove.clone(), c.identity, c.same_instance), (Variant::Both, vec![], Identity::New, false));
        let AddPlan::Create(c) = plan_add(&label("D"), Some(Variant::Call), None, &store, &nodes, cid(NEW_ID)).unwrap() else { panic!() };
        assert_eq!(c.variant, Variant::Call);
    }

    #[test]
    fn re_adding_a_removed_cable_keeps_its_identity_and_instance() {
        let (mut store, nodes) = two();
        store[1].n = 4;
        // B removed: the store keeps it
        let AddPlan::Create(c) = plan_add(&label("B"), None, None, &store, &nodes[..1], cid(NEW_ID)).unwrap() else { panic!() };
        assert_eq!(c.instance, CableInstance { label: label("B"), id: cid(B_ID), n: 4 });
        assert_eq!((c.variant, c.identity, c.same_instance, c.remove.len()), (Variant::LowLatency, Identity::Stored, true, 0));
        // Forgotten, then added: new
        let AddPlan::Create(c) = plan_add(&label("B"), None, None, &store[..1], &nodes[..1], cid(NEW_ID)).unwrap() else { panic!() };
        assert_eq!((c.instance.id, c.instance.n, c.identity, c.variant), (cid(NEW_ID), 1, Identity::New, Variant::Both));
    }

    #[test]
    fn add_repairs_an_unhealthy_cable_in_place() {
        let (store, mut nodes) = two();
        for (health, attempts, status) in [
            (Health::Problem(22), None, None),
            (Health::Problem(31), Some(1), Some(STATUS_START_BREAKER)),
            (Health::NotStarted, Some(1), None),
        ] {
            nodes[1].health = health;
            nodes[1].start_attempts = attempts;
            nodes[1].problem_status = status;
            let p = plan_add(&label("B"), None, None, &store, &nodes, cid(NEW_ID)).unwrap();
            assert_eq!(p, AddPlan::Repair { node: 1, prune: vec![], mics_differ: None, identity: Identity::Stored }, "{health:?}");
        }
        // Its microphones stay; asked for others, it says `variant` switches
        let p = plan_add(&label("B"), Some(Variant::Call), None, &store, &nodes, cid(NEW_ID)).unwrap();
        assert!(matches!(p, AddPlan::Repair { node: 1, mics_differ: Some(Variant::Call), .. }));
        // Two nodes with B's identity, neither healthy: the newest repaired, the other pruned
        let mut older = node("B", B_ID, 1, Variant::Both);
        older.health = Health::Problem(28);
        let mut newer = node("B", B_ID, 3, Variant::Call);
        newer.health = Health::Problem(10);
        let nodes2 = vec![nodes[0].clone(), older, newer];
        let p = plan_add(&label("B"), None, None, &store, &nodes2, cid(NEW_ID)).unwrap();
        assert_eq!(p, AddPlan::Repair { node: 2, prune: vec![1], mics_differ: None, identity: Identity::Stored });
        // A leftover beside the working node: kept, the leftover pruned
        let (store, mut nodes) = two();
        let mut stale = node("B", B_ID, 2, Variant::Call);
        stale.health = Health::Problem(28);
        nodes.push(stale);
        let p = plan_add(&label("B"), None, None, &store, &nodes, cid(NEW_ID)).unwrap();
        assert_eq!(p, AddPlan::Nothing { node: 1, prune: vec![2], mics_differ: None, identity: Identity::Stored });
    }

    #[test]
    fn add_adopts_a_node_made_outside_the_tool() {
        let (store, mut nodes) = two();
        let mut outside = node("X", NEW_ID, 1, Variant::Call);
        outside.instance_id = r"ROOT\MEDIA\0007".into();
        outside.cable = None;
        outside.health = Health::Problem(31);
        nodes.push(outside);
        let AddPlan::Create(c) = plan_add(&label("C"), None, Some(r"root\media\0007"), &store, &nodes, cid(NEW_ID)).unwrap() else {
            panic!()
        };
        assert_eq!((c.instance.label.as_str(), c.instance.n, c.variant, c.remove.clone()), ("C", 1, Variant::Call, vec![2]));
        let err = |adopt, l: &str| plan_add(&label(l), None, Some(adopt), &store, &nodes, cid(NEW_ID)).unwrap_err();
        assert!(matches!(err(r"ROOT\MEDIA\0099", "C"), PlanError::AdoptNotFound(_)));
        assert!(matches!(err(nodes[0].instance_id.as_str(), "C"), PlanError::AdoptHasIdentity(_)));
        assert!(matches!(err(r"ROOT\MEDIA\0007", "B"), PlanError::AdoptPresent { .. }));
    }

    #[test]
    fn per_cable_actions_wait_for_the_migration() {
        let (store, mut nodes) = two();
        nodes.push(legacy(r"ROOT\MEDIA\0003", Legacy::AudioCodec(Variant::Both)));
        assert!(matches!(plan_add(&label("D"), None, None, &store, &nodes, cid(NEW_ID)), Err(PlanError::Legacy(_))));
        assert!(matches!(plan_variant(&label("B"), Variant::Call, false, &store, &nodes), Err(PlanError::Legacy(_))));
        assert!(matches!(plan_update(&nodes), Err(PlanError::Legacy(_))));
        assert!(matches!(
            plan_add(&label("D"), None, Some(r"ROOT\MEDIA\0003"), &store, &nodes, cid(NEW_ID)),
            Err(PlanError::AdoptLegacy(_))
        ));
        assert!(PlanError::Legacy(vec![r"ROOT\MEDIA\0003".into()]).to_string().contains("acx-ll-cable migrate"));
    }

    #[test]
    fn remove_takes_only_that_cables_nodes() {
        let (store, nodes) = two();
        assert_eq!(plan_remove(&RemoveTarget::Label(label("B")), false, &store, &nodes), RemovePlan { remove: vec![1], forget: vec![] });
        assert_eq!(plan_remove(&RemoveTarget::Label(label("B")), true, &store, &nodes), RemovePlan {
            remove: vec![1],
            forget: vec![label("B")]
        });
        assert_eq!(plan_remove(&RemoveTarget::Label(label("C")), true, &store, &nodes), RemovePlan::default());
        // A duplicate labelled B goes with it; A stays
        let mut nodes2 = nodes.clone();
        nodes2.push(node("B", NEW_ID, 1, Variant::Both));
        assert_eq!(plan_remove(&RemoveTarget::Label(label("B")), false, &store, &nodes2).remove, [1, 2]);
        // --all: every node of ours, the old driver's and unidentified ones too
        let mut nodes3 = nodes.clone();
        nodes3.push(legacy(r"ROOT\MEDIA\0004", Legacy::CableB));
        let all = plan_remove(&RemoveTarget::All, true, &store, &nodes3);
        assert_eq!((all.remove, all.forget), (vec![0, 1, 2], vec![label("A"), label("B")]));
    }

    #[test]
    fn a_variant_switch_re_creates_only_that_cable_with_the_next_n() {
        let (store, nodes) = two();
        let VariantPlan::Switch(c) = plan_variant(&label("B"), Variant::Call, false, &store, &nodes).unwrap() else { panic!() };
        assert_eq!(c.instance, CableInstance { label: label("B"), id: cid(B_ID), n: 2 });
        assert_eq!((c.variant, c.remove.clone(), c.same_instance), (Variant::Call, vec![1], false));
        // Already that variant, working: kept; asked to re-create, n + 1 again
        assert_eq!(plan_variant(&label("B"), Variant::LowLatency, false, &store, &nodes), Ok(VariantPlan::Keep { node: 1 }));
        let VariantPlan::Switch(c) = plan_variant(&label("B"), Variant::LowLatency, true, &store, &nodes).unwrap() else { panic!() };
        assert_eq!(c.instance.n, 2);
        // The next n is above every n the cable had, the store's included
        let mut store5 = store.clone();
        store5[1].n = 5;
        let VariantPlan::Switch(c) = plan_variant(&label("B"), Variant::Call, false, &store5, &nodes).unwrap() else { panic!() };
        assert_eq!(c.instance.n, 6);
        // Absent: add
        assert_eq!(plan_variant(&label("B"), Variant::Call, false, &store, &nodes[..1]), Err(PlanError::NotPresent(label("B"))));
        assert_eq!(plan_variant(&label("C"), Variant::Call, false, &store, &nodes), Err(PlanError::NotPresent(label("C"))));
        let max = vec![entry("B", B_ID, u32::MAX, None)];
        assert_eq!(plan_variant(&label("B"), Variant::Call, false, &max, &nodes), Err(PlanError::NoNextInstance(label("B"))));
    }

    #[test]
    fn an_update_disables_installs_and_enables_every_cable() {
        let (_, mut nodes) = two();
        let p = plan_update(&nodes).unwrap();
        assert_eq!(p, UpdatePlan { disable: vec![0, 1], left_disabled: vec![], install: vec![0, 1], unidentified: vec![] });
        // B disabled by the user stays so, but gets the package; a node without an identity goes off and on with the rest
        nodes[1].health = Health::Problem(22);
        let mut outside = node("X", NEW_ID, 1, Variant::Call);
        outside.cable = None;
        nodes.push(outside);
        let p = plan_update(&nodes).unwrap();
        assert_eq!(p, UpdatePlan { disable: vec![0, 2], left_disabled: vec![1], install: vec![0, 1], unidentified: vec![2] });
        assert_eq!(plan_update(&[]), Err(PlanError::NothingToUpdate));
    }

    /// The box today: the old driver's A (ROOT\AudioCodec) and Cable-B.ps1's B (ROOT\NoiseFilterCableB).
    #[test]
    fn the_migration_replaces_todays_a_and_b() {
        let nodes = vec![legacy(r"ROOT\MEDIA\0003", Legacy::AudioCodec(Variant::Both)), legacy(r"ROOT\MEDIA\0004", Legacy::CableB)];
        let p = plan_migrate(&nodes, &[], None, Variant::Both).unwrap();
        assert_eq!(p.legacy, [0, 1]);
        assert_eq!(p.update.disable, [0, 1]);
        assert!(p.update.install.is_empty());
        assert_eq!(p.add, [(label("A"), Variant::Both), (label("B"), Variant::Both)]);
        // Install-Driver's -Variant for A, and B's microphones as Cable-B.ps1 made them
        let p = plan_migrate(&nodes, &[], Some(Variant::Call), Variant::LowLatency).unwrap();
        assert_eq!(p.add, [(label("A"), Variant::Call), (label("B"), Variant::LowLatency)]);
        // A second run after a partial one: A made already, so only B is added; A is updated with the rest
        let mut partial = nodes.clone();
        partial.push(node("A", A_ID, 1, Variant::Both));
        let p = plan_migrate(&partial, &[entry("A", A_ID, 1, None)], None, Variant::Both).unwrap();
        assert_eq!(p.add, [(label("B"), Variant::Both)]);
        assert_eq!((p.update.disable, p.update.install), (vec![0, 1, 2], vec![2]));
        // Nothing of the old driver's
        assert_eq!(plan_migrate(&two().1, &[], None, Variant::Both), Err(PlanError::NothingToMigrate));
    }

    #[test]
    fn the_listing_groups_nodes_by_identity() {
        let (store, mut nodes) = two();
        let gone = entry("C", NEW_ID, 2, Some(Variant::Call));
        let mut store = store;
        store.push(gone.clone());
        nodes.push(legacy(r"ROOT\MEDIA\0003", Legacy::AudioCodec(Variant::Both)));
        let mut outside = node("X", NEW_ID, 1, Variant::Call);
        outside.cable = None;
        nodes.push(outside);
        nodes.push(node("B", B_ID, 2, Variant::Call));
        let l = listing(&store, &nodes);
        assert_eq!(l.cables.iter().map(|c| (c.label.as_str(), c.nodes.clone())).collect::<Vec<_>>(), [("A", vec![0]), ("B", vec![1, 4])]);
        assert_eq!((l.absent, l.legacy, l.unidentified), (vec![gone], vec![2], vec![3]));
        assert_eq!(l.problems.len(), 1);
        assert!(l.problems[0].starts_with("cable B's identity is on 2 nodes"), "{:?}", l.problems);
    }
}
