//! What each setup action does with the device nodes there now (TODO 19, the per-cable design, sections 4, 8 and 9). Pure:
//! the nodes, the store and the request in, a plan out; [`super`] carries it out.
//!
//! **Per cable.** A cable is its identity. [`resolve`] finds a label's identity (the store's, or, if the store has none, the
//! one its nodes' instance IDs carry) and the nodes with it; `add`, `variant`, `remove`, `disable` and `enable` act on those
//! nodes alone, so no other cable is touched. Only `update` and the migration act on every node of ours, as every cable runs
//! one image.
//!
//! **Instance numbers.** Re-adding a cable with the microphones it had keeps its instance ID (its n) (Astra's recommendation,
//! forwarded by the owner, 2026-11-25), though Windows makes it new endpoint ids all the same: the first install's trial
//! (2026-11-26) found the old ones kept NOTPRESENT and never reused, which the owner accepted, the app resolving a cable's
//! endpoints by role ([`crate::roles`]). A disable keeps them ([`plan_disable`]). One that is there but unhealthy is repaired
//! in place, the same node. A variant switch, and a re-add with other microphones, take n + 1: fresh endpoints, which Windows
//! makes with the variant's default period (TODO 22).

use std::fmt;

use windows::Win32::Devices::DeviceAndDriverInstallation::{CONFIGFLAG_DISABLED, DN_HAS_PROBLEM, DN_STARTED};

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

/// A device node's state, from CM_Get_DevNode_Status (or, for a node found by its instance ID, CM_Locate_DevNodeW:
/// `cables::present`).
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
    /// CM_Locate_DevNodeW failed with this CONFIGRET, other than CR_NO_SUCH_DEVNODE (not present): the node is listed, its state
    /// unread (`cables::present`).
    Unlocated(u32),
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
            Health::Unknown(cr) => unread("CM_Get_DevNode_Status", cr),
            Health::Unlocated(cr) => unread("CM_Locate_DevNodeW", cr),
        }
    }
}

/// "state unknown (CM_Get_DevNode_Status: CR_NO_SUCH_DEVNODE, 13)": a state `call` couldn't read, failing with CONFIGRET `cr`.
fn unread(call: &str, cr: u32) -> String {
    match configret_name(cr) {
        Some(name) => format!("state unknown ({call}: {name}, {cr})"),
        None => format!("state unknown ({call}: CONFIGRET {cr})"),
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
    /// SPDRP_CONFIGFLAGS, if it reads: CONFIGFLAG_DISABLED marks a disable that persists (Device Manager's, pnputil's).
    pub config_flags: Option<u32>,
}

impl Node {
    /// Disabled (Code 22) with CONFIGFLAG_DISABLED: a disable that persists across restarts, `acx-ll-cable disable`'s or the
    /// user's (Device Manager's), which `update` and `add` leave. `None`: the flags didn't read, so it is taken as the user's.
    pub fn disabled_by_user(&self) -> bool {
        self.health == Health::Problem(PROBLEM_DISABLED) && self.config_flags.is_none_or(|f| f & CONFIGFLAG_DISABLED.0 != 0)
    }

    /// Disabled (Code 22) without CONFIGFLAG_DISABLED: a disable that doesn't persist, as `update` and the migration make
    /// theirs. One there before a run is an earlier run's, stopped before it enabled the node again (Ctrl+C, a killed tool):
    /// the next run enables it with the rest. (Another tool's disable for the current hardware profile only, devcon's, looks
    /// the same: its CSConfigFlags read only through CM_Get_HW_Prof_Flags, deprecated since Windows 8 [V: Learn].)
    pub fn disabled_by_a_run(&self) -> bool {
        self.health == Health::Problem(PROBLEM_DISABLED) && !self.disabled_by_user()
    }

    /// Not disabled (no Code 22) though CONFIGFLAG_DISABLED is set: a disable in every hardware profile that Windows couldn't
    /// finish now (`acx-ll-cable disable`'s exit 7: something had the device in use), so the device runs until the next
    /// restart, which disables it (we expect: to confirm on the box). `enable` cancels it (its global DICS_ENABLE clears the
    /// flag); `disable` asks again.
    pub fn disable_pending(&self) -> bool {
        self.health != Health::Problem(PROBLEM_DISABLED) && self.config_flags.is_some_and(|f| f & CONFIGFLAG_DISABLED.0 != 0)
    }

    /// Off on purpose, now or from the next restart: disabled with CONFIGFLAG_DISABLED ([`Node::disabled_by_user`]), or that
    /// disable waiting for a restart ([`Node::disable_pending`]). `variant` refuses such a cable: its re-made device would be
    /// enabled.
    pub fn off_on_purpose(&self) -> bool {
        self.disabled_by_user() || self.disable_pending()
    }

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
        // A cable's disable said apart from a failure ("a problem: Code N"): off on purpose, and how it comes back; or a disable
        // waiting for a restart
        if let Some(c) = &self.cable {
            if self.disabled_by_user() {
                text.push_str(&format!(
                    "; it stays disabled across restarts until `acx-ll-cable enable {}`, its endpoints keeping their ids",
                    c.label
                ));
            } else if self.disabled_by_a_run() {
                text.push_str(&format!(
                    "; disabled, though not in every hardware profile (no CONFIGFLAG_DISABLED): a setup run's that stopped, which a \
                     restart undoes, or another tool's for this profile only (devcon's disable, which persists): `acx-ll-cable enable \
                     {}` turns it back on",
                    c.label
                ));
            } else if self.disable_pending() {
                text.push_str(&format!(
                    "; disabled at the next restart (CONFIGFLAG_DISABLED is set: a disable Windows couldn't finish while the device was \
                     in use): `acx-ll-cable enable {}` cancels it",
                    c.label
                ));
            }
        }
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
    /// The migration's, not finished yet: not made, or made and not checked and recorded ([`super::store`]'s `Pending`).
    pub pending: bool,
    /// A migration's cable: the old driver's nodes (instance IDs) it is made in place of, whose endpoints the endpoint map gives
    /// this cable's ([`super::endpoint_map`]), on a run after their removal too ([`super::store`]'s `Replaces`). Kept once it is
    /// made, until a run leaves no record pending: that run's map (published first) holds every cable that has them.
    pub replaces: Vec<String>,
    /// Every node `replaces` has named, kept for good (the store's `Replaced`, which its write takes `replaces` into): while
    /// another record keeps its `replaces`, the cable is still mapped with them ([`mapped`]), so a drop of the records'
    /// `replaces` that stops part way loses no cable from the migration's map. Read only: a write adds to it, never drops it.
    pub replaced: Vec<String>,
}

/// The packages the present cables run, the package rule's input ([`super::package::pick`]): one entry a node with an identity
/// and a driver. The old driver's nodes, and nodes made outside the tool (which the driver refuses at AddDevice, so they load
/// no image of ours), don't set the cables' package.
pub fn running(nodes: &[Node]) -> Vec<(String, Running)> {
    nodes.iter().filter(|n| n.cable.is_some()).filter_map(|n| Some((n.instance_id.clone(), n.running.clone()?))).collect()
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
    /// `disable` or `enable` (`action`) on a cable that isn't present.
    Absent { label: Label, action: &'static str },
    /// `enable` on a cable whose identity is on more than one node (a setup error): enabling them all would have two want one
    /// control device's name.
    OnSeveral { label: Label, nodes: Vec<String> },
    /// `enable` on a cable that isn't disabled, and isn't working either (`node`, described).
    NotDisabled { label: Label, node: String },
    /// `variant` on a cable off on purpose ([`Node::off_on_purpose`]): its device re-made would be enabled, undoing the disable.
    Disabled(Label),
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
            PlanError::Absent { label, action } => write!(
                f,
                "cable {label} isn't present, so there is nothing to {action} (`acx-ll-cable list` shows the cables; `acx-ll-cable add \
                 {label}` makes it). Nothing was changed."
            ),
            PlanError::OnSeveral { label, nodes } => write!(
                f,
                "cable {label}'s identity is on {} devices ({}): a setup error. `acx-ll-cable add {label}` keeps one and removes the \
                 others, enabling it. Nothing was changed.",
                nodes.len(),
                nodes.join(", ")
            ),
            PlanError::NotDisabled { label, node } => write!(
                f,
                "cable {label} isn't disabled, but it isn't working either ({node}): `acx-ll-cable add {label}` repairs it. Nothing was \
                 changed."
            ),
            PlanError::Disabled(label) => write!(
                f,
                "cable {label} is disabled, or disabled at the next restart (`acx-ll-cable disable`'s, or Device Manager's): a variant \
                 switch re-makes its device enabled, which would undo that. `acx-ll-cable enable {label}` first, then the switch \
                 (`acx-ll-cable disable {label}` after it keeps the cable off). Nothing was changed."
            ),
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
    /// The store has it as the migration's, not made yet: no instance of it has been there.
    pub pending: bool,
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
    let pending = entry.is_some_and(|e| e.pending);
    Ok(Some(Resolved { label: label.clone(), id, n, mics, nodes: own, stored: entry.is_some(), pending }))
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
    /// remove; `mics_differ`: microphones asked for that it doesn't have (`variant` switches); `pending`: its store record is
    /// still the migration's (a run that made it and stopped before recording it), so it is recorded as made.
    Nothing {
        node: usize,
        prune: Vec<usize>,
        mics_differ: Option<Variant>,
        identity: Identity,
        pending: bool,
    },
    /// Present and disabled on purpose, its only node (`acx-ll-cable disable`'s, or Device Manager's: it persists across
    /// restarts): left as it is, as `update` leaves it, `enable` turning it back on; recorded in the store if the store lacked it
    /// (`identity`). A migration's record pending stays so: its endpoints can't be checked while it is off. Its microphones stay
    /// (`mics_differ`: microphones asked for that it doesn't have; `variant` switches once it is enabled).
    Disabled {
        node: usize,
        mics_differ: Option<Variant>,
        identity: Identity,
    },
    /// Present but unhealthy (disabled for now, failing, refused by the crash breaker, or a duplicate): its newest node gets the
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
    // Its instance ID was there before (a removed cable's), unless the migration recorded it and never made it
    let had_instance = resolved.as_ref().is_some_and(|r| !r.pending);
    // The instance ID it had, for the microphones it had (its endpoint ids are new all the same: the module's docs). Other
    // microphones take n + 1, as a variant switch does: fresh endpoints, with the variant's default period (TODO 22)
    let reuse = |variant: Variant| -> Result<(CableInstance, bool), PlanError> {
        if had_instance && last_mics.is_some_and(|m| m != variant) {
            let next = n.checked_add(1).ok_or_else(|| PlanError::NoNextInstance(label.clone()))?;
            Ok((instance(next), false))
        } else {
            Ok((instance(n), had_instance))
        }
    };
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
        let (instance, same_instance) = reuse(variant)?;
        return Ok(AddPlan::Create(Create { instance, variant, remove: vec![i], identity, same_instance }));
    }
    let own = resolved.as_ref().map(|r| r.nodes.clone()).unwrap_or_default();
    let healthy: Vec<usize> = own.iter().copied().filter(|&i| nodes[i].healthy()).collect();
    if let [keep] = healthy.as_slice() {
        let prune = own.iter().copied().filter(|i| i != keep).collect();
        let mics_differ = mics.filter(|&m| nodes[*keep].variant() != Some(m));
        let pending = resolved.as_ref().is_some_and(|r| r.pending);
        return Ok(AddPlan::Nothing { node: *keep, prune, mics_differ, identity, pending });
    }
    // Disabled on purpose, and nothing else of it there: left so (the user's choice; `enable` undoes it)
    if let [only] = own.as_slice()
        && nodes[*only].disabled_by_user()
    {
        let mics_differ = mics.filter(|&m| nodes[*only].variant() != Some(m));
        return Ok(AddPlan::Disabled { node: *only, mics_differ, identity });
    }
    // Present but unhealthy (or several working: a setup error mended the same way): the newest node repaired in place
    if let Some(&newest) = own.iter().max_by_key(|&&i| nodes[i].cable.as_ref().map_or(0, |c| c.n)) {
        let prune = own.iter().copied().filter(|&i| i != newest).collect();
        let mics_differ = mics.filter(|&m| nodes[newest].variant() != Some(m));
        return Ok(AddPlan::Repair { node: newest, prune, mics_differ, identity });
    }
    // Absent: created with its identity (and, for the microphones it had, the instance ID it had), or new; both microphones
    // unless asked or remembered otherwise
    let variant = mics.or(last_mics).unwrap_or(Variant::Both);
    let (instance, same_instance) = reuse(variant)?;
    Ok(AddPlan::Create(Create { instance, variant, remove: Vec::new(), identity, same_instance }))
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

/// What `disable` or `enable` does to one cable's nodes (every other cable's are left alone).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatePlan {
    pub label: Label,
    /// Indices into the nodes: the cable's nodes to disable (persistently), or to enable. None: nothing to do.
    pub change: Vec<usize>,
    /// The cable's nodes left as they are: disabled persistently already (`disable`), or not disabled (`enable`: working).
    pub left: Vec<usize>,
}

/// The cable's present nodes, for `disable` or `enable` (`action`): by its identity ([`resolve`]); refused while the old
/// driver's nodes are there (the migration first), or if none is present.
fn present_nodes(label: &Label, action: &'static str, store: &[Entry], nodes: &[Node]) -> Result<Vec<usize>, PlanError> {
    refuse_legacy(nodes)?;
    match resolve(label, store, nodes)? {
        Some(r) if !r.nodes.is_empty() => Ok(r.nodes),
        _ => Err(PlanError::Absent { label: label.clone(), action }),
    }
}

/// `disable <label>`: each of the cable's nodes not disabled persistently already, so the cable stays off across restarts until
/// `enable`. One disabled but not in every hardware profile (a stopped run's, which a restart would bring back, or another
/// tool's for this profile only) is included, and so is one whose disable waits for a restart ([`Node::disable_pending`]:
/// asked again, as the device may stop now).
pub fn plan_disable(label: &Label, store: &[Entry], nodes: &[Node]) -> Result<StatePlan, PlanError> {
    let own = present_nodes(label, "disable", store, nodes)?;
    let (left, change) = own.into_iter().partition(|&i| nodes[i].disabled_by_user());
    Ok(StatePlan { label: label.clone(), change, left })
}

/// `enable <label>`: the cable's node, if it is disabled (persistently or not), or its disable waits for a restart
/// ([`Node::disable_pending`], its health whatever it is: the enable cancels the disable, so the restart doesn't make it). One
/// working is left (nothing to do); one with another problem is refused (`add` repairs it), as is an identity on several
/// nodes (a setup error `add` mends).
pub fn plan_enable(label: &Label, store: &[Entry], nodes: &[Node]) -> Result<StatePlan, PlanError> {
    let own = present_nodes(label, "enable", store, nodes)?;
    let [only] = own.as_slice() else {
        return Err(PlanError::OnSeveral { label: label.clone(), nodes: own.iter().map(|&i| nodes[i].instance_id.clone()).collect() });
    };
    let n = &nodes[*only];
    match n.health {
        _ if n.disable_pending() => Ok(StatePlan { label: label.clone(), change: own, left: Vec::new() }),
        Health::Problem(PROBLEM_DISABLED) => Ok(StatePlan { label: label.clone(), change: own, left: Vec::new() }),
        Health::Working => Ok(StatePlan { label: label.clone(), change: Vec::new(), left: own }),
        _ => Err(PlanError::NotDisabled { label: label.clone(), node: n.describe() }),
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
/// microphones didn't come). A cable off on purpose ([`Node::off_on_purpose`], any of its nodes) is refused: its device
/// re-made would be enabled, undoing the disable (`enable` first, `disable` after).
pub fn plan_variant(label: &Label, target: Variant, recreate: bool, store: &[Entry], nodes: &[Node]) -> Result<VariantPlan, PlanError> {
    refuse_legacy(nodes)?;
    let Some(r) = resolve(label, store, nodes)? else { return Err(PlanError::NotPresent(label.clone())) };
    if r.nodes.is_empty() {
        return Err(PlanError::NotPresent(label.clone()));
    }
    if r.nodes.iter().any(|&i| nodes[i].off_on_purpose()) {
        return Err(PlanError::Disabled(label.clone()));
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
    /// Disabled without persistence before the run (an earlier update's or migration's, stopped before it enabled them):
    /// enabled with the rest, and on a failure too, so no run leaves a node disabled.
    pub reenable: Vec<usize>,
    /// Disabled by the user (CONFIGFLAG_DISABLED, Device Manager's): left disabled, though its driver is updated.
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
        if n.disabled_by_user() {
            plan.left_disabled.push(i);
        } else if n.disabled_by_a_run() {
            plan.reenable.push(i);
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

/// A cable the migration makes: its label, identity, instance number and microphones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub label: Label,
    pub id: CableId,
    pub n: u32,
    pub variant: Variant,
    /// A new identity (the store had none for the label), written to the store, pending, before anything changes.
    pub new: bool,
    /// The old driver's nodes it is made in place of (instance IDs): those present now of its kind, and those an earlier run
    /// recorded. Their endpoints are the old side of its endpoint map.
    pub replaces: Vec<String>,
}

impl Target {
    pub fn instance(&self) -> CableInstance {
        CableInstance { label: self.label.clone(), id: self.id, n: self.n }
    }

    /// Its store entry: `pending` until it is made, with the nodes it replaces (kept once made, until the migration's map is
    /// final: [`Entry::replaces`]).
    pub fn entry(&self, pending: bool) -> Entry {
        let replaces = self.replaces.clone();
        Entry { label: self.label.clone(), id: self.id, n: self.n, mics: Some(self.variant), pending, replaces, replaced: Vec::new() }
    }

    /// The node to create (nothing to remove: the old driver's nodes go first).
    pub fn create(&self) -> Create {
        let identity = if self.new { Identity::New } else { Identity::Stored };
        Create { instance: self.instance(), variant: self.variant, remove: Vec::new(), identity, same_instance: false }
    }
}

/// The migration from the old driver's devices (TODO 19, S0a): what to remove, what to update, what to add.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigratePlan {
    /// The old driver's nodes: disabled, then (the image unloaded) removed.
    pub legacy: Vec<usize>,
    /// The other nodes of ours: disabled with them, so the old image can unload, then given the new package as `update` does.
    pub update: UpdatePlan,
    /// The cables to make in their place, each unless a cable of that label is present already: A for the old driver's cable
    /// (its variant, unless `a_mics`), B for Cable-B.ps1's (`b_mics`, else both), and every cable an earlier run recorded as
    /// pending and didn't make (its microphones as recorded, unless asked for others). Their entries go to the store, pending,
    /// before anything is disabled, so a run that stops after the old nodes are gone is finished by the next.
    pub add: Vec<Target>,
    /// The cables an earlier run made and stopped before recording as made: present, their records still pending. Each is
    /// checked (its identity on one node, working on the package, its endpoints there), then recorded as made.
    pub finish: Vec<Finish>,
    /// The cables an earlier run made and recorded, their records keeping the nodes they replace ([`mapped`]): in this run's
    /// endpoint map again, nothing else done to them.
    pub mapped: Vec<Finish>,
}

/// A cable to finish ([`MigratePlan::finish`]), or to map ([`MigratePlan::mapped`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finish {
    pub label: Label,
    pub id: CableId,
    /// Indices into the nodes: those with its identity, newest first (one, unless a setup error).
    pub nodes: Vec<usize>,
    /// The old driver's nodes it was made in place of, as the store recorded them: the old side of its endpoint map.
    pub replaces: Vec<String>,
}

/// The cables a migration made, present and recorded as made, to map again (but those labelled as in `except`): while any
/// record keeps the nodes it replaces ([`Entry::replaces`]: the migration's endpoint map isn't final yet), every cable whose
/// record has them or had them ([`map_old`]). Every migrate or add run maps them again, so the run that leaves no record
/// pending, and drops the replaced nodes once its map is published, maps the whole migration, whatever its earlier attempts were
/// (killed, stopped by a restart, a drop that stopped after some records: theirs are in [`Entry::replaced`]). None once no
/// record keeps them: the map is final.
pub fn mapped(store: &[Entry], nodes: &[Node], except: &[&Label]) -> Vec<Finish> {
    let mut out = Vec::new();
    for e in store.iter().filter(|e| !e.pending && !except.contains(&&e.label)) {
        let replaces = map_old(store, e);
        if replaces.is_empty() {
            continue;
        }
        let mut own: Vec<usize> = (0..nodes.len()).filter(|&i| nodes[i].id() == Some(e.id)).collect();
        own.sort_by_key(|&i| std::cmp::Reverse(nodes[i].cable.as_ref().map_or(0, |c| c.n)));
        if !own.is_empty() {
            out.push(Finish { label: e.label.clone(), id: e.id, nodes: own, replaces });
        }
    }
    out
}

/// The old nodes the migration's endpoint map gives record `e`'s cable: while any record of `store` keeps its
/// [`Entry::replaces`] (the map isn't final), `e`'s, with every node its record had ([`Entry::replaced`]); none once the map
/// is final.
pub fn map_old(store: &[Entry], e: &Entry) -> Vec<String> {
    if store.iter().any(|x| !x.replaces.is_empty()) { union(e.replaces.clone(), &e.replaced) } else { Vec::new() }
}

/// `a` and those of `b` not in it, without case.
pub(crate) fn union(mut a: Vec<String>, b: &[String]) -> Vec<String> {
    for s in b {
        if !a.iter().any(|x| same_instance(x, s)) {
            a.push(s.clone());
        }
    }
    a
}

/// The migration's plan. `new_ids` are the identities A and B get if the store has none for them.
pub fn plan_migrate(
    nodes: &[Node], store: &[Entry], a_mics: Option<Variant>, b_mics: Option<Variant>, new_ids: [CableId; 2],
) -> Result<MigratePlan, PlanError> {
    let legacy: Vec<usize> = (0..nodes.len()).filter(|&i| nodes[i].is_legacy()).collect();
    let pending = |label: &Label| store.iter().find(|e| &e.label == label && e.pending);
    let mut add: Vec<Target> = Vec::new();
    let mut finish: Vec<Finish> = Vec::new();
    // A label's cable. Present: nothing to make, but finished if its record is still pending (a run that made it and stopped
    // before recording it: the planner would otherwise find nothing to do while the store says there is). Absent: a target
    // with the store's identity and n for its label, if it has one, else `new_id` and n 1. (The old driver's nodes have no
    // identity, so they are no label's.)
    let want = |add: &mut Vec<Target>, finish: &mut Vec<Finish>, label: Label, variant: Variant, new_id: CableId, replaces| {
        match resolve(&label, store, nodes)? {
            Some(r) if !r.nodes.is_empty() => {
                if r.pending {
                    let mut own = r.nodes;
                    own.sort_by_key(|&i| std::cmp::Reverse(nodes[i].cable.as_ref().map_or(0, |c| c.n)));
                    finish.push(Finish { label, id: r.id, nodes: own, replaces });
                }
            }
            Some(r) => add.push(Target { label, id: r.id, n: r.n, variant, new: false, replaces }),
            None => add.push(Target { label, id: new_id, n: 1, variant, new: true, replaces }),
        }
        Ok::<(), PlanError>(())
    };
    // The old driver's nodes of one cable: Cable-B.ps1's B (`b`), else A's
    let old_of = |b: bool| -> Vec<String> {
        let of_b = |i: usize| nodes[i].model == Model::Legacy(Legacy::CableB);
        legacy.iter().filter(|&&i| of_b(i) == b).map(|&i| nodes[i].instance_id.clone()).collect()
    };
    // Today's A: the first working one's variant, else the first's
    let old_a = legacy
        .iter()
        .filter_map(|&i| match nodes[i].model {
            Model::Legacy(Legacy::AudioCodec(v)) => Some((nodes[i].health == Health::Working, v)),
            _ => None,
        })
        .min_by_key(|&(working, _)| !working)
        .map(|(_, v)| v);
    let old_b = legacy.iter().any(|&i| nodes[i].model == Model::Legacy(Legacy::CableB));
    let a = Label::a();
    let b: Label = "B".parse().expect("a label");
    for (label, old, asked, new_id, replaced) in
        [(a, old_a, a_mics, new_ids[0], old_of(false)), (b, old_b.then_some(Variant::Both), b_mics, new_ids[1], old_of(true))]
    {
        // Asked for, else as an earlier run recorded it, else the old device's
        let p = pending(&label);
        let variant = match (old, p) {
            (None, None) => continue,
            (old, p) => asked.or(p.and_then(|e| e.mics)).or(old).unwrap_or(Variant::Both),
        };
        let replaces = union(p.map(|e| e.replaces.clone()).unwrap_or_default(), &replaced);
        want(&mut add, &mut finish, label, variant, new_id, replaces)?;
    }
    // Any other cable an earlier run recorded as pending (none today: the migration makes A and B only)
    for e in store.iter().filter(|e| e.pending) {
        if !add.iter().any(|t| t.label == e.label) && !finish.iter().any(|f| f.label == e.label) {
            want(&mut add, &mut finish, e.label.clone(), e.mics.unwrap_or(Variant::Both), e.id, e.replaces.clone())?;
        }
    }
    let labels: Vec<&Label> = add.iter().map(|t| &t.label).chain(finish.iter().map(|f| &f.label)).collect();
    let mapped = mapped(store, nodes, &labels);
    // Records that keep the nodes they replace: the run maps them and drops those (an earlier run's map wasn't written, or the
    // drop didn't finish), even if none of their cables is present
    let unsettled = store.iter().any(|e| !e.replaces.is_empty());
    if legacy.is_empty() && add.is_empty() && finish.is_empty() && !unsettled {
        return Err(PlanError::NothingToMigrate);
    }
    let mut update = unload_and_install(nodes, |n| !n.is_legacy());
    // The old driver's nodes are disabled too (they hold the old image), and removed rather than installed
    for &i in &legacy {
        if nodes[i].disabled_by_a_run() {
            update.reenable.push(i);
        } else if !nodes[i].disabled_by_user() {
            update.disable.push(i);
        }
    }
    update.disable.sort_unstable();
    update.reenable.sort_unstable();
    Ok(MigratePlan { legacy, update, add, finish, mapped })
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
            config_flags: Some(0),
        }
    }

    pub(crate) fn legacy(instance: &str, l: Legacy) -> Node {
        Node {
            instance_id: instance.into(),
            model: Model::Legacy(l),
            cable: None,
            health: Health::Working,
            running: Some(running("oem7.inf", "1.0.0.0")),
            start_attempts: None,
            problem_status: None,
            config_flags: Some(0),
        }
    }

    pub(crate) fn entry(l: &str, id: &str, n: u32, mics: Option<Variant>) -> Entry {
        Entry { label: label(l), id: cid(id), n, mics, pending: false, replaces: vec![], replaced: vec![] }
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
        assert_eq!(p, AddPlan::Nothing { node: 1, prune: vec![], mics_differ: None, identity: Identity::Stored, pending: false });
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
        assert_eq!(p, AddPlan::Nothing { node: 1, prune: vec![2], mics_differ: None, identity: Identity::Stored, pending: false });
    }

    /// A cable disabled persistently (`disable`, Device Manager's: CONFIGFLAG_DISABLED) is left as it is by `add`, as `update`
    /// leaves it; one disabled for now (a stopped run's) is repaired, as before. Recorded if the store lacked it.
    #[test]
    fn add_leaves_a_disabled_cable_as_it_is() {
        let (store, mut nodes) = two();
        nodes[1].health = Health::Problem(PROBLEM_DISABLED);
        nodes[1].config_flags = Some(CONFIGFLAG_DISABLED.0);
        let p = plan_add(&label("B"), None, None, &store, &nodes, cid(NEW_ID)).unwrap();
        assert_eq!(p, AddPlan::Disabled { node: 1, mics_differ: None, identity: Identity::Stored });
        assert_eq!(plan_add(&label("B"), None, None, &store[..1], &nodes, cid(NEW_ID)).unwrap(), AddPlan::Disabled {
            node: 1,
            mics_differ: None,
            identity: Identity::FromNode
        });
        // Other microphones asked for: said, not made (B stays as it is; `variant` switches once it is enabled)
        assert_eq!(plan_add(&label("B"), Some(Variant::Call), None, &store, &nodes, cid(NEW_ID)).unwrap(), AddPlan::Disabled {
            node: 1,
            mics_differ: Some(Variant::Call),
            identity: Identity::Stored
        });
        assert_eq!(plan_add(&label("B"), Some(Variant::LowLatency), None, &store, &nodes, cid(NEW_ID)).unwrap(), AddPlan::Disabled {
            node: 1,
            mics_differ: None,
            identity: Identity::Stored
        });
        nodes[1].config_flags = Some(0);
        assert!(matches!(plan_add(&label("B"), None, None, &store, &nodes, cid(NEW_ID)).unwrap(), AddPlan::Repair { node: 1, .. }));
        // A's add is untouched by B's state
        assert!(matches!(plan_add(&label("A"), None, None, &store, &nodes, cid(NEW_ID)).unwrap(), AddPlan::Nothing { node: 0, .. }));
    }

    /// `disable` and `enable` act on that cable's nodes alone; each says when there is nothing to do, and refuses a cable that
    /// isn't present, the old driver's devices (the migration first), an enable of an identity on two nodes, and an enable of a
    /// cable that isn't disabled but failing.
    #[test]
    fn disable_and_enable_take_only_that_cables_nodes() {
        let (store, mut nodes) = two();
        let p = plan_disable(&label("b"), &store, &nodes).unwrap();
        assert_eq!(p, StatePlan { label: label("B"), change: vec![1], left: vec![] });
        // Disabled for now (a stopped run's, not persistent): disabled for good
        nodes[1].health = Health::Problem(PROBLEM_DISABLED);
        nodes[1].config_flags = Some(0);
        assert_eq!(plan_disable(&label("B"), &store, &nodes).unwrap().change, [1]);
        assert_eq!(plan_enable(&label("B"), &store, &nodes).unwrap().change, [1]);
        // Disabled for good already: nothing to disable; enable takes it
        nodes[1].config_flags = Some(CONFIGFLAG_DISABLED.0);
        assert_eq!(plan_disable(&label("B"), &store, &nodes).unwrap(), StatePlan { label: label("B"), change: vec![], left: vec![1] });
        assert_eq!(plan_enable(&label("B"), &store, &nodes).unwrap(), StatePlan { label: label("B"), change: vec![1], left: vec![] });
        // Working: nothing to enable
        assert_eq!(plan_enable(&label("A"), &store, &nodes).unwrap(), StatePlan { label: label("A"), change: vec![], left: vec![0] });
        // A disable waiting for a restart (Windows couldn't stop it: `disable`'s exit 7): working, CONFIGFLAG_DISABLED set. The
        // enable takes it (its global DICS_ENABLE clears the flag, so the restart doesn't disable it); a disable asks again
        nodes[1].health = Health::Working;
        assert!(nodes[1].disable_pending() && nodes[1].off_on_purpose() && !nodes[1].disabled_by_user());
        assert_eq!(plan_enable(&label("B"), &store, &nodes).unwrap(), StatePlan { label: label("B"), change: vec![1], left: vec![] });
        assert_eq!(plan_disable(&label("B"), &store, &nodes).unwrap(), StatePlan { label: label("B"), change: vec![1], left: vec![] });
        // Failing too, with that flag: the enable still cancels the disable (then says it isn't working: `add` repairs it)
        nodes[1].health = Health::Problem(PROBLEM_FAILED_ADD);
        assert_eq!(plan_enable(&label("B"), &store, &nodes).unwrap().change, [1]);
        // Working, the flags unread or clear: nothing pending, nothing to enable
        nodes[1].health = Health::Working;
        for flags in [None, Some(0)] {
            nodes[1].config_flags = flags;
            assert!(!nodes[1].disable_pending());
            assert_eq!(plan_enable(&label("B"), &store, &nodes).unwrap().left, [1]);
        }
        // Failing, not disabled: `add` repairs it
        nodes[1].health = Health::Problem(PROBLEM_FAILED_ADD);
        nodes[1].problem_status = Some(STATUS_START_BREAKER);
        let e = plan_enable(&label("B"), &store, &nodes).unwrap_err();
        assert!(matches!(&e, PlanError::NotDisabled { node, .. } if node.contains("Code 31")), "{e:?}");
        assert!(e.to_string().starts_with("cable B isn't disabled, but it isn't working either ("), "{e}");
        assert!(e.to_string().ends_with("`acx-ll-cable add B` repairs it. Nothing was changed."), "{e}");
        assert_eq!(plan_disable(&label("B"), &store, &nodes).unwrap().change, [1], "a failing cable can still be turned off");
        // Absent
        let e = plan_disable(&label("C"), &store, &nodes).unwrap_err();
        assert_eq!(e, PlanError::Absent { label: label("C"), action: "disable" });
        assert!(e.to_string().starts_with("cable C isn't present, so there is nothing to disable"), "{e}");
        let gone = plan_enable(&label("B"), &store, &nodes[..1]).unwrap_err();
        assert_eq!(gone, PlanError::Absent { label: label("B"), action: "enable" }, "B's record alone isn't B");
        // B's identity on two nodes: disabled together; not enabled together
        let mut two_b = nodes.clone();
        two_b[1].health = Health::Problem(PROBLEM_DISABLED);
        two_b[1].config_flags = Some(0);
        two_b.push(node("B", B_ID, 2, Variant::Both));
        assert_eq!(plan_disable(&label("B"), &store, &two_b).unwrap().change, [1, 2]);
        let e = plan_enable(&label("B"), &store, &two_b).unwrap_err();
        assert!(matches!(&e, PlanError::OnSeveral { nodes, .. } if nodes.len() == 2), "{e:?}");
        assert!(e.to_string().contains("`acx-ll-cable add B` keeps one and removes the others"), "{e}");
        // The old driver's devices: the migration first
        let mut old = nodes.clone();
        old.push(legacy(r"ROOT\MEDIA\0003", Legacy::AudioCodec(Variant::Both)));
        assert!(matches!(plan_disable(&label("A"), &store, &old), Err(PlanError::Legacy(_))));
        assert!(matches!(plan_enable(&label("A"), &store, &old), Err(PlanError::Legacy(_))));
    }

    /// `list` and every message say a cable's disable apart from a failure: off for good until `enable`, or not in every
    /// hardware profile (a stopped run's, or another tool's for this profile only), or waiting for a restart (working, with
    /// CONFIGFLAG_DISABLED); a failed node says its problem code only.
    #[test]
    fn a_disabled_node_is_described_apart_from_a_failed_one() {
        let mut b = node("B", B_ID, 1, Variant::Both);
        b.health = Health::Problem(PROBLEM_DISABLED);
        b.config_flags = Some(CONFIGFLAG_DISABLED.0);
        let text = b.describe();
        assert!(text.contains("both microphones (ROOT\\AcxLlCable), disabled (Code 22), driver oem12.inf"), "{text}");
        assert!(
            text.ends_with("; it stays disabled across restarts until `acx-ll-cable enable B`, its endpoints keeping their ids"),
            "{text}"
        );
        b.config_flags = Some(0);
        assert!(b.describe().ends_with(
            "; disabled, though not in every hardware profile (no CONFIGFLAG_DISABLED): a setup run's that stopped, which a restart \
             undoes, or another tool's for this profile only (devcon's disable, which persists): `acx-ll-cable enable B` turns it \
             back on"
        ));
        // Working, the disable waiting for a restart
        b.health = Health::Working;
        b.config_flags = Some(CONFIGFLAG_DISABLED.0);
        let pending = b.describe();
        assert!(pending.contains("both microphones (ROOT\\AcxLlCable), working, driver oem12.inf"), "{pending}");
        assert!(
            pending.ends_with(
                "; disabled at the next restart (CONFIGFLAG_DISABLED is set: a disable Windows couldn't finish while the device was \
                 in use): `acx-ll-cable enable B` cancels it"
            ),
            "{pending}"
        );
        b.health = Health::Problem(PROBLEM_FAILED_ADD);
        b.config_flags = Some(0);
        let failed = b.describe();
        assert!(failed.contains("a problem: Code 31 (CM_PROB_FAILED_ADD)") && !failed.contains("disabled"), "{failed}");
        b.health = Health::Working;
        assert!(!b.describe().contains("disabled"));
    }

    /// `variant` refuses a cable off on purpose (disabled, or its disable waiting for a restart): its device re-made would be
    /// enabled, undoing the disable, even a switch to the microphones it has. One disabled by a stopped run is switched as
    /// before; another cable's disable doesn't matter.
    #[test]
    fn a_variant_switch_refuses_a_cable_disabled_on_purpose() {
        let (store, mut nodes) = two();
        nodes[1].health = Health::Problem(PROBLEM_DISABLED);
        nodes[1].config_flags = Some(CONFIGFLAG_DISABLED.0);
        for (target, recreate) in [(Variant::Call, false), (Variant::LowLatency, false), (Variant::LowLatency, true)] {
            let e = plan_variant(&label("B"), target, recreate, &store, &nodes).unwrap_err();
            assert_eq!(e, PlanError::Disabled(label("B")), "{target:?} {recreate}");
            assert!(e.to_string().contains("`acx-ll-cable enable B` first, then the switch"), "{e}");
            assert!(e.to_string().ends_with("Nothing was changed."), "{e}");
        }
        // Its disable waiting for a restart: refused too
        nodes[1].health = Health::Working;
        assert_eq!(plan_variant(&label("B"), Variant::Call, false, &store, &nodes), Err(PlanError::Disabled(label("B"))));
        assert_eq!(plan_variant(&label("B"), Variant::LowLatency, false, &store, &nodes), Err(PlanError::Disabled(label("B"))));
        // A's switch is untouched by B's state
        assert!(matches!(plan_variant(&label("A"), Variant::Call, false, &store, &nodes), Ok(VariantPlan::Switch(_))));
        // Disabled by a stopped run (no CONFIGFLAG_DISABLED): switched as before
        nodes[1].health = Health::Problem(PROBLEM_DISABLED);
        nodes[1].config_flags = Some(0);
        assert!(matches!(plan_variant(&label("B"), Variant::Call, false, &store, &nodes), Ok(VariantPlan::Switch(_))));
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
        assert_eq!(p, UpdatePlan { disable: vec![0, 1], install: vec![0, 1], ..UpdatePlan::default() });
        // B disabled by the user (Device Manager's disable persists: CONFIGFLAG_DISABLED) stays so, but gets the package; a node
        // without an identity goes off and on with the rest
        nodes[1].health = Health::Problem(22);
        nodes[1].config_flags = Some(CONFIGFLAG_DISABLED.0);
        let mut outside = node("X", NEW_ID, 1, Variant::Call);
        outside.cable = None;
        nodes.push(outside);
        let p = plan_update(&nodes).unwrap();
        assert_eq!(p, UpdatePlan {
            disable: vec![0, 2],
            reenable: vec![],
            left_disabled: vec![1],
            install: vec![0, 1],
            unidentified: vec![2]
        });
        // B disabled without persistence: an earlier run's, stopped before it enabled B again (Ctrl+C), so enabled with the rest
        nodes[1].config_flags = Some(0);
        let p = plan_update(&nodes).unwrap();
        assert_eq!((p.disable, p.reenable, p.left_disabled), (vec![0, 2], vec![1], vec![]));
        // Flags that don't read: taken as the user's disable
        nodes[1].config_flags = None;
        assert_eq!(plan_update(&nodes).unwrap().left_disabled, [1]);
        assert_eq!(plan_update(&[]), Err(PlanError::NothingToUpdate));
    }

    const NEW_A: &str = "a0a0a0a0-0000-4000-8000-0000000000a0";
    const NEW_B: &str = "b0b0b0b0-0000-4000-8000-0000000000b0";

    fn migrate(nodes: &[Node], store: &[Entry], a: Option<Variant>, b: Option<Variant>) -> Result<MigratePlan, PlanError> {
        plan_migrate(nodes, store, a, b, [cid(NEW_A), cid(NEW_B)])
    }

    fn targets(p: &MigratePlan) -> Vec<(&str, Variant, u32, bool)> {
        p.add.iter().map(|t| (t.label.as_str(), t.variant, t.n, t.new)).collect()
    }

    /// The box today: the old driver's A (ROOT\AudioCodec) and Cable-B.ps1's B (ROOT\NoiseFilterCableB).
    #[test]
    fn the_migration_replaces_todays_a_and_b() {
        let nodes = vec![legacy(r"ROOT\MEDIA\0003", Legacy::AudioCodec(Variant::Both)), legacy(r"ROOT\MEDIA\0004", Legacy::CableB)];
        let p = migrate(&nodes, &[], None, None).unwrap();
        assert_eq!(p.legacy, [0, 1]);
        assert_eq!(p.update.disable, [0, 1]);
        assert!(p.update.install.is_empty());
        assert_eq!(targets(&p), [("A", Variant::Both, 1, true), ("B", Variant::Both, 1, true)]);
        assert_eq!((p.add[0].id, p.add[1].id), (cid(NEW_A), cid(NEW_B)));
        // What goes to the store before anything changes: pending; the node made from it carries the identity and n 1
        let replaces = vec![r"ROOT\MEDIA\0003".to_string()];
        assert_eq!(p.add[0].entry(true), Entry {
            label: label("A"),
            id: cid(NEW_A),
            n: 1,
            mics: Some(Variant::Both),
            pending: true,
            replaces,
            replaced: vec![],
        });
        // Made, its record keeps the old node it replaces (dropped once the migration's map is written)
        assert_eq!((p.add[1].replaces.as_slice(), p.add[1].entry(false).replaces.len()), (&[r"ROOT\MEDIA\0004".to_string()][..], 1));
        assert_eq!(p.add[1].create().instance.instance_id(), format!(r"ROOT\MEDIA\ACXLLCABLE_B_{}_1", NEW_B.to_uppercase()));
        // Install-Driver's -Variant for A, and -CableBMics for B
        let p = migrate(&nodes, &[], Some(Variant::Call), Some(Variant::LowLatency)).unwrap();
        assert_eq!(targets(&p), [("A", Variant::Call, 1, true), ("B", Variant::LowLatency, 1, true)]);
        // A takes the first working old A's variant, else the first's
        let mut two_a = vec![
            legacy(r"ROOT\MEDIA\0003", Legacy::AudioCodec(Variant::Call)),
            legacy(r"ROOT\MEDIA\0005", Legacy::AudioCodec(Variant::LowLatency)),
        ];
        two_a[0].health = Health::Problem(10);
        assert_eq!(targets(&migrate(&two_a, &[], None, None).unwrap()), [("A", Variant::LowLatency, 1, true)]);
        two_a[1].health = Health::Problem(28);
        assert_eq!(targets(&migrate(&two_a, &[], None, None).unwrap()), [("A", Variant::Call, 1, true)]);
        two_a.iter_mut().for_each(|n| n.health = Health::Working);
        assert_eq!(targets(&migrate(&two_a, &[], None, None).unwrap()), [("A", Variant::Call, 1, true)]);
        // A second run after a partial one: A made already, so only B is added; A is updated with the rest
        let mut partial = nodes.clone();
        partial.push(node("A", A_ID, 1, Variant::Both));
        let p = migrate(&partial, &[entry("A", A_ID, 1, None)], None, None).unwrap();
        assert_eq!(targets(&p), [("B", Variant::Both, 1, true)]);
        assert_eq!((p.update.disable, p.update.install), (vec![0, 1, 2], vec![2]));
        // Nothing of the old driver's, and nothing pending
        assert_eq!(migrate(&two().1, &two().0, None, None), Err(PlanError::NothingToMigrate));
    }

    /// The first run recorded A and B in the store (pending) before it removed anything, then stopped part way: the next run
    /// makes whatever is still missing, with the identities and microphones the first run gave them.
    #[test]
    fn a_stopped_migration_is_finished_by_the_next_run() {
        let pending = |l: &str, id: &str, mics| Entry { pending: true, ..entry(l, id, 1, Some(mics)) };
        let store = vec![pending("A", A_ID, Variant::Call), pending("B", B_ID, Variant::LowLatency)];
        // The old A gone (removed, or its removal waiting for a restart), the old B still there: both made, as recorded
        let old_b = vec![legacy(r"ROOT\MEDIA\0004", Legacy::CableB)];
        let p = migrate(&old_b, &store, None, None).unwrap();
        assert_eq!((p.legacy.clone(), p.update.disable.clone()), (vec![0], vec![0]));
        assert_eq!(targets(&p), [("A", Variant::Call, 1, false), ("B", Variant::LowLatency, 1, false)]);
        assert_eq!((p.add[0].id, p.add[1].id), (cid(A_ID), cid(B_ID)), "the first run's identities, not new ones");
        assert_eq!(p.add[0].create().identity, Identity::Stored);
        // Asked for other microphones this time: those
        assert_eq!(migrate(&old_b, &store, Some(Variant::Both), None).unwrap().add[0].variant, Variant::Both);
        // Both old devices gone and neither cable made: no old device left, and still both made
        let p = migrate(&[], &store, None, None).unwrap();
        assert!(p.legacy.is_empty() && p.update == UpdatePlan::default());
        assert_eq!(targets(&p), [("A", Variant::Call, 1, false), ("B", Variant::LowLatency, 1, false)]);
        // A made, B not: only B, and A updated with the rest
        let mut a_made = store.clone();
        a_made[0].pending = false;
        let p = migrate(&[node("A", A_ID, 1, Variant::Call)], &a_made, None, None).unwrap();
        assert_eq!(targets(&p), [("B", Variant::LowLatency, 1, false)]);
        assert_eq!((p.update.disable, p.update.install), (vec![0], vec![0]));
        // Every target made: nothing to do
        a_made[1].pending = false;
        let both = [node("A", A_ID, 1, Variant::Call), node("B", B_ID, 1, Variant::LowLatency)];
        assert_eq!(migrate(&both, &a_made, None, None), Err(PlanError::NothingToMigrate));
        // A cable the user removed (remembered, not pending) isn't the migration's
        assert_eq!(migrate(&[], &two().0, None, None), Err(PlanError::NothingToMigrate));
        // A pending target is a new instance to `add` too: its endpoints are new
        let AddPlan::Create(c) = plan_add(&label("B"), None, None, &store, &[], cid(NEW_ID)).unwrap() else { panic!() };
        assert_eq!((c.instance.id, c.identity, c.same_instance), (cid(B_ID), Identity::Stored, false));
    }

    /// Astra's review (2026-11-25): a run that made B and stopped before recording it left B present with its record pending.
    /// The planner found nothing to do (B present, no old node) while Install-Driver.ps1 saw pending work, so every retry
    /// stopped at NothingToMigrate. Now B is to finish: checked and recorded.
    #[test]
    fn a_cable_made_and_not_recorded_is_finished() {
        let pending = |l: &str, id: &str, mics| Entry { pending: true, ..entry(l, id, 1, Some(mics)) };
        let a_made = Entry { pending: false, ..pending("A", A_ID, Variant::Both) };
        let b_pending = Entry { replaces: vec![r"ROOT\MEDIA\0004".into()], ..pending("B", B_ID, Variant::Both) };
        let store = vec![a_made.clone(), b_pending.clone()];
        let both = [node("A", A_ID, 1, Variant::Both), node("B", B_ID, 1, Variant::Both)];
        let p = migrate(&both, &store, None, None).unwrap();
        assert!(p.legacy.is_empty() && p.add.is_empty(), "{p:?}");
        assert_eq!(p.finish, [Finish { label: label("B"), id: cid(B_ID), nodes: vec![1], replaces: vec![r"ROOT\MEDIA\0004".into()] }]);
        // The update sets as always (the run decides whether any of it is needed)
        assert_eq!(p.update.install, [0, 1]);
        // Made but unhealthy (registered, its install not done): still to finish, the run installs it
        let mut half = both.clone();
        half[1].health = Health::Problem(28);
        half[1].running = None;
        assert_eq!(migrate(&half, &store, None, None).unwrap().finish.len(), 1);
        // A made and not recorded, B not made: A to finish, B to make, with the identities the first run recorded
        let store2 = vec![pending("A", A_ID, Variant::Call), pending("B", B_ID, Variant::LowLatency)];
        let p = migrate(&both[..1], &store2, None, None).unwrap();
        assert_eq!((p.finish.len(), p.finish[0].label.as_str()), (1, "A"));
        assert_eq!(targets(&p), [("B", Variant::LowLatency, 1, false)]);
        // Old B still there beside a pending B that is present (made before its removal: not a sequence the run has, but
        // planned all the same): B finished, the old B removed
        let mut with_old = both.to_vec();
        with_old.push(legacy(r"ROOT\MEDIA\0004", Legacy::CableB));
        let p = migrate(&with_old, &store, None, None).unwrap();
        assert_eq!((p.legacy.clone(), p.finish.len(), p.add.len()), (vec![2], 1, 0));
        // Two nodes with B's identity: both listed, newest first (the run refuses to record it)
        let mut dup = both.to_vec();
        dup.push(node("B", B_ID, 2, Variant::Both));
        assert_eq!(migrate(&dup, &store, None, None).unwrap().finish[0].nodes, [2, 1]);
        // Recorded since, the old nodes' names dropped: nothing to do
        let done = vec![a_made.clone(), Entry { pending: false, replaces: vec![], ..b_pending.clone() }];
        assert_eq!(migrate(&both, &done, None, None), Err(PlanError::NothingToMigrate));
        // Recorded, and its record still keeps the old node (a run that stopped before its map was written, or before the drop):
        // mapped again, and nothing else
        let unsettled = vec![a_made, Entry { pending: false, ..b_pending }];
        let p = migrate(&both, &unsettled, None, None).unwrap();
        assert!(p.legacy.is_empty() && p.add.is_empty() && p.finish.is_empty(), "{p:?}");
        assert_eq!(p.mapped, [Finish { label: label("B"), id: cid(B_ID), nodes: vec![1], replaces: vec![r"ROOT\MEDIA\0004".into()] }]);
        // ... also when its cable is gone (removed by the user since): the run drops the names
        let p = migrate(&both[..1], &unsettled, None, None).unwrap();
        assert!(p.mapped.is_empty() && p.add.is_empty(), "{p:?}");
        // A drop that stopped part way (Astra's review, 2026-11-25): A's Replaces dropped, B's kept. A is still mapped, with
        // the node its record had (Replaced), so the retry's map holds the whole migration; once no record keeps Replaces,
        // nothing is mapped (the map is final)
        let a_dropped = Entry { replaced: vec![r"ROOT\MEDIA\0003".into()], ..entry("A", A_ID, 1, Some(Variant::Both)) };
        let b_kept = Entry { replaced: vec![r"ROOT\MEDIA\0004".into()], ..unsettled[1].clone() };
        let p = migrate(&both, &[a_dropped.clone(), b_kept.clone()], None, None).unwrap();
        let mapped: Vec<(&str, &[String])> = p.mapped.iter().map(|f| (f.label.as_str(), f.replaces.as_slice())).collect();
        assert_eq!(mapped, [("A", &[r"ROOT\MEDIA\0003".to_string()][..]), ("B", &[r"ROOT\MEDIA\0004".to_string()][..])]);
        let final_ = [a_dropped.clone(), Entry { replaces: vec![], ..b_kept.clone() }];
        assert_eq!(migrate(&both, &final_, None, None), Err(PlanError::NothingToMigrate));
        assert!(mapped_of(&final_, &both).is_empty() && map_old(&final_, &final_[0]).is_empty());
        assert_eq!(map_old(&[a_dropped.clone(), b_kept], &a_dropped), [r"ROOT\MEDIA\0003"]);
        // Beside a cable to make, a made one keeping its old node is mapped too
        let a_kept = Entry { replaces: vec![r"ROOT\MEDIA\0003".into()], ..entry("A", A_ID, 1, Some(Variant::Both)) };
        let b_to_make = Entry { pending: true, ..unsettled[1].clone() };
        let p = migrate(&both[..1], &[a_kept, b_to_make], None, None).unwrap();
        assert_eq!((p.mapped.len(), p.mapped[0].label.as_str()), (1, "A"));
        assert_eq!(targets(&p), [("B", Variant::Both, 1, false)]);
        // `add B` on the present B with its record pending: nothing to change, recorded as made
        let p = plan_add(&label("B"), None, None, &store, &both, cid(NEW_ID)).unwrap();
        assert!(matches!(p, AddPlan::Nothing { node: 1, pending: true, .. }), "{p:?}");
    }

    fn mapped_of(store: &[Entry], nodes: &[Node]) -> Vec<Finish> {
        mapped(store, nodes, &[])
    }

    /// A re-add keeps the identity, and the instance ID only with the microphones it had (its endpoint ids are new either way:
    /// the first install's trial): other microphones take n + 1, as a variant switch does (Astra's recommendation, 2026-11-25).
    #[test]
    fn a_re_add_with_other_microphones_takes_the_next_n() {
        let (mut store, nodes) = two();
        store[1].n = 4;
        let add = |mics, adopt: Option<&str>, nodes: &[Node]| plan_add(&label("B"), mics, adopt, &store, nodes, cid(NEW_ID)).unwrap();
        let AddPlan::Create(c) = add(Some(Variant::LowLatency), None, &nodes[..1]) else { panic!() };
        assert_eq!((c.instance.n, c.same_instance), (4, true), "the microphones it had");
        let AddPlan::Create(c) = add(Some(Variant::Both), None, &nodes[..1]) else { panic!() };
        assert_eq!((c.instance.id, c.instance.n, c.variant, c.same_instance), (cid(B_ID), 5, Variant::Both, false));
        // Adopted under its remembered label with other microphones: the next n too
        let mut outside = node("X", NEW_ID, 1, Variant::Call);
        outside.instance_id = r"ROOT\MEDIA\0007".into();
        outside.cable = None;
        let with = vec![nodes[0].clone(), outside];
        let AddPlan::Create(c) = add(None, Some(r"ROOT\MEDIA\0007"), &with) else { panic!() };
        assert_eq!((c.instance.n, c.variant, c.same_instance, c.remove.clone()), (5, Variant::Call, false, vec![1]));
        // A new label: both microphones by default
        let AddPlan::Create(c) = plan_add(&label("D"), None, None, &store, &nodes, cid(NEW_ID)).unwrap() else { panic!() };
        assert_eq!((c.instance.n, c.variant), (1, Variant::Both));
        let max = vec![Entry { n: u32::MAX, ..store[1].clone() }];
        assert_eq!(plan_add(&label("B"), Some(Variant::Call), None, &max, &[], cid(NEW_ID)), Err(PlanError::NoNextInstance(label("B"))));
    }

    /// The old driver's nodes disabled by an earlier run (no CONFIGFLAG_DISABLED) are enabled again on a failure; the user's
    /// disable is left (they are removed anyway).
    #[test]
    fn the_migration_disables_the_old_nodes_too() {
        let mut nodes = vec![legacy(r"ROOT\MEDIA\0003", Legacy::AudioCodec(Variant::Both)), legacy(r"ROOT\MEDIA\0004", Legacy::CableB)];
        nodes.push(node("C", NEW_ID, 1, Variant::Call));
        nodes[0].health = Health::Problem(22);
        nodes[1].health = Health::Problem(22);
        nodes[1].config_flags = Some(CONFIGFLAG_DISABLED.0);
        let p = migrate(&nodes, &[], None, None).unwrap();
        assert_eq!((p.update.disable, p.update.reenable, p.update.install), (vec![2], vec![0], vec![2]));
    }

    /// The package rule's input with two cables present: a node made outside the tool, on another package, doesn't count
    /// (the driver refuses it), nor does the old driver's.
    #[test]
    fn only_cables_set_the_package() {
        let (_, mut nodes) = two();
        let mut outside = node("X", NEW_ID, 1, Variant::Call);
        outside.instance_id = r"ROOT\MEDIA\0007".into();
        outside.cable = None;
        outside.running = Some(running("oem3.inf", "1.0.0.0"));
        nodes.push(outside);
        nodes.push(legacy(r"ROOT\MEDIA\0003", Legacy::AudioCodec(Variant::Both)));
        let r = super::running(&nodes);
        assert_eq!(r.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), [nodes[0].instance_id.as_str(), &nodes[1].instance_id]);
        let v = crate::setup::package::parse_version("3.27.41.512").unwrap();
        let ours = crate::setup::package::tests::candidate(r"C:\Windows\INF\oem12.inf", "audiocodec.inf", 200, v);
        let other = crate::setup::package::tests::candidate(r"C:\Windows\INF\oem3.inf", "audiocodec.inf", 100, 1 << 48);
        let pick = crate::setup::package::pick(&r, &[other, ours], r"ROOT\AcxLlCable");
        assert_eq!(pick, Ok(crate::setup::package::Pick::Running { candidate: 1, newer: None }), "not Mixed");
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
