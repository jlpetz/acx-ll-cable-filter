//! The endpoint resolver by role (TODO 19, the per-cable design; the contract for the app, Astra's, after the owner's decision
//! of 2026-11-26): a cable's endpoint for a role, as Windows has it now.
//!
//! **Why.** Windows' endpoint ids belong to one installation of a cable's device: a full removal and re-add makes new ones (the
//! first install's trial: the old ones stay NOTPRESENT, never reused), while a disable and enable keeps them. So the app saves
//! a cable's identity with the endpoint's role ("B's Call Microphone": the [`CableId`] and an [`EndpointKind`]), never the
//! MMDevice id or the name, and resolves the current id each time it opens one ([`resolve`]).
//!
//! **How.** By identity, never by name (names are the user's to change): the cable's present device nodes, by the identity in
//! their instance IDs (`ROOT\MEDIA\ACXLLCABLE_<label>_<id>_<n>`, [`crate::setup::Present`]: SetupAPI, any user), and the
//! MMDevice endpoints on those nodes, by the KS filter path each is connected to: its instance ID and its circuit's reference
//! string (`Speaker0` the Speakers, `Microphone0` the Microphone, `Microphone1` the Call Microphone), as the setup tool's
//! endpoint code reads them ([`crate::setup::endpoints`]). An endpoint Windows keeps for a removed node or an earlier
//! installation (NOTPRESENT) is never returned, even with the same instance ID and pin: after a re-add the new endpoint is the
//! answer; with only the old one there, the answer is none. Among several (a setup error: one identity on two nodes), an active
//! one before one disabled in the Sound panel, then the newest node's.
//!
//! **What comes back** ([`Resolved`]): the endpoint's MMDevice id now, its state (active; disabled in the Sound panel;
//! unplugged), its name for display, and the node it's on. Or none ([`NoEndpoint`]), and why: no node with that identity is
//! present (removed, or never made here); the cable's microphones don't include the role (a one-microphone variant:
//! `acx-ll-cable variant <label> both` adds it); the cable is disabled (`acx-ll-cable enable <label>`: its endpoints come back
//! with their ids); its device isn't working (`acx-ll-cable add <label>` repairs it); or it works and Windows hasn't made that
//! endpoint (just after an add or an enable: look again).
//!
//! **Use.** On a control thread, never an audio thread: it enumerates the device tree and reads the endpoints' property stores
//! (COM, on a thread of its own in the MTA, so the caller's apartment doesn't matter), allocating as it goes. Any user, no
//! elevation. [`Snapshot`] reads once and resolves many (a settings page); [`resolve`] reads for one. The setup tool's
//! `acx-ll-cable endpoint <label|id> speakers|microphone|call-microphone` prints the answer (driver/README.md).

use std::fmt;

use crate::cables::EndpointKind;
use crate::identity::{CableId, CableInstance, Label, same_instance};
use crate::setup::Present;
use crate::setup::endpoints::{self, Endpoint, State};
use crate::setup::plan::{Health, Node, PROBLEM_DISABLED};
use crate::variant::Variant;
use crate::win_error::WinError;

/// A cable's endpoint for a role, as Windows has it now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleEndpoint {
    /// The MMDevice id (`{0.0.1.00000000}.{...}`): what WASAPI opens (`IMMDeviceEnumerator::GetDevice`).
    pub id: String,
    /// [`State::Active`], [`State::Disabled`] (in the Sound panel: the user's to enable) or [`State::Unplugged`]; never
    /// [`State::NotPresent`].
    pub state: State,
    /// As Windows names it now: for display only (the user may have renamed it).
    pub name: String,
    /// The device node it's on, as its KS filter path has it (lower case, usually).
    pub instance_id: String,
}

/// Why a cable has no endpoint for a role now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoEndpoint {
    /// No device node with the identity is present: removed (`acx-ll-cable add <label>` brings it back, with new endpoint
    /// ids), or never made on this machine.
    NotPresent(CableId),
    /// The cable's microphones don't include the role (a one-microphone variant).
    NoSuchRole { label: Label, variant: Variant, role: EndpointKind },
    /// The cable is disabled (`acx-ll-cable disable`, or Device Manager's): its endpoints are not present until it is enabled,
    /// then come back with their ids.
    Disabled { label: Label },
    /// The cable's device isn't working (`health`): `acx-ll-cable add <label>` repairs it.
    NotWorking { label: Label, health: Health },
    /// The cable works, but Windows shows no endpoint for the role: not made yet (just after an add or an enable), so look again.
    NotMade { label: Label, role: EndpointKind },
}

impl fmt::Display for NoEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NoEndpoint::NotPresent(id) => write!(f, "no cable with id {id} is present (removed, or never made here)"),
            NoEndpoint::NoSuchRole { label, variant, role } => write!(
                f,
                "{} has no {}: it has {} (`acx-ll-cable variant {label} both` gives it both microphones)",
                label.friendly_name(),
                role.port(),
                variant.describe()
            ),
            NoEndpoint::Disabled { label } => write!(
                f,
                "{} is disabled: its endpoints come back, with their ids, once it is enabled (`acx-ll-cable enable {label}`)",
                label.friendly_name()
            ),
            NoEndpoint::NotWorking { label, health } => {
                write!(f, "{} isn't working ({}): `acx-ll-cable add {label}` repairs it", label.friendly_name(), health.describe())
            }
            NoEndpoint::NotMade { label, role } => {
                write!(f, "{} works, but Windows shows no {} for it (yet): look again", label.friendly_name(), role.port())
            }
        }
    }
}

/// What [`resolve`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    Endpoint(RoleEndpoint),
    None(NoEndpoint),
}

impl Resolved {
    /// The endpoint, if there is one.
    pub fn endpoint(&self) -> Option<&RoleEndpoint> {
        match self {
            Resolved::Endpoint(e) => Some(e),
            Resolved::None(_) => None,
        }
    }
}

/// The device nodes of ours and the endpoints on our nodes, read once: any number of [`Snapshot::resolve`]s.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// The present nodes with one of our hardware IDs ([`Present`]).
    pub nodes: Vec<Node>,
    /// The endpoints on nodes of our instance-ID form, NOTPRESENT ones left out ([`endpoints::all`]).
    pub endpoints: Vec<Endpoint>,
}

impl Snapshot {
    /// The device tree and the endpoints now. Any user.
    pub fn read() -> Result<Self, WinError> {
        let nodes = Present::read()?.nodes;
        Ok(Snapshot { nodes, endpoints: endpoints::all()? })
    }

    /// Cable `id`'s endpoint for `role`, in this snapshot (the module's docs). Pure.
    pub fn resolve(&self, id: CableId, role: EndpointKind) -> Resolved {
        let own: Vec<&Node> = self.nodes.iter().filter(|n| n.id() == Some(id)).collect();
        let n_of = |instance: &str| CableInstance::parse(instance).map_or(0, |c| c.n);
        let found = self
            .endpoints
            .iter()
            .filter(|e| e.kind == role && e.state != State::NotPresent && own.iter().any(|n| same_instance(&n.instance_id, &e.instance_id)))
            .max_by_key(|e| (e.state == State::Active, n_of(&e.instance_id)));
        if let Some(e) = found {
            return Resolved::Endpoint(RoleEndpoint {
                id: e.id.clone(),
                state: e.state,
                name: e.name.clone(),
                instance_id: e.instance_id.clone(),
            });
        }
        // None: why, by the cable's node (a working one, else the newest)
        let Some(main) = own.iter().max_by_key(|n| (n.health == Health::Working, n.cable.as_ref().map_or(0, |c| c.n))) else {
            return Resolved::None(NoEndpoint::NotPresent(id));
        };
        let label = main.cable.as_ref().map_or_else(Label::a, |c| c.label.clone());
        Resolved::None(match (main.variant(), main.health) {
            (Some(variant), _) if !EndpointKind::of_variant(variant).contains(&role) => NoEndpoint::NoSuchRole { label, variant, role },
            (_, Health::Problem(PROBLEM_DISABLED)) => NoEndpoint::Disabled { label },
            (_, Health::Working) => NoEndpoint::NotMade { label, role },
            (_, health) => NoEndpoint::NotWorking { label, health },
        })
    }

    /// The identity of the present cable labelled `label` (`acx-ll-cable endpoint B ...`): `None` if no node is labelled so;
    /// the identities, if several are (a setup error `acx-ll-cable list` shows).
    pub fn cable_labelled(&self, label: &Label) -> Result<Option<CableId>, Vec<CableId>> {
        let mut ids: Vec<CableId> =
            self.nodes.iter().filter_map(|n| n.cable.as_ref().filter(|c| &c.label == label).map(|c| c.id)).collect();
        ids.sort_unstable();
        ids.dedup();
        match ids.as_slice() {
            [] => Ok(None),
            [id] => Ok(Some(*id)),
            _ => Err(ids),
        }
    }
}

/// Cable `id`'s endpoint for `role` now ([`Snapshot::read`], then [`Snapshot::resolve`]): the app's call when it opens a cable's
/// endpoint it saved by role. On a control thread; any user.
pub fn resolve(id: CableId, role: EndpointKind) -> Result<Resolved, WinError> {
    Ok(Snapshot::read()?.resolve(id, role))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::plan::tests::{A_ID, B_ID, cid, label, node};

    // Pure: fake lists of nodes and endpoints, as the setup's tests fake them.

    fn cable(l: &str, id: &str, n: u32) -> CableInstance {
        CableInstance { label: label(l), id: cid(id), n }
    }

    /// An endpoint on instance `c`, as its KS filter path has the instance ID (lower case).
    fn ep(c: &CableInstance, kind: EndpointKind, id: &str, state: State) -> Endpoint {
        Endpoint { id: id.into(), name: endpoints::expected_name(c, kind), kind, state, instance_id: c.instance_id().to_lowercase() }
    }

    fn found(r: &Resolved) -> Option<(&str, State)> {
        r.endpoint().map(|e| (e.id.as_str(), e.state))
    }

    /// Two cables with every role, A's and B's endpoints apart: each resolves to its own, by identity and pin, whatever the
    /// names say.
    #[test]
    fn a_cables_role_resolves_to_its_own_endpoint() {
        let (a, b) = (cable("A", A_ID, 1), cable("B", B_ID, 1));
        let mut renamed = ep(&b, EndpointKind::Speakers, "{0}.{b-spk}", State::Active);
        renamed.name = "Speakers (ACX-LL-Cable A)".into();
        let s = Snapshot {
            nodes: vec![node("A", A_ID, 1, Variant::Both), node("B", B_ID, 1, Variant::Both)],
            endpoints: vec![
                ep(&a, EndpointKind::Speakers, "{0}.{a-spk}", State::Active),
                ep(&a, EndpointKind::Microphone, "{1}.{a-mic}", State::Active),
                ep(&a, EndpointKind::CallMicrophone, "{1}.{a-call}", State::Active),
                renamed,
                ep(&b, EndpointKind::Microphone, "{1}.{b-mic}", State::Active),
                ep(&b, EndpointKind::CallMicrophone, "{1}.{b-call}", State::Disabled),
            ],
        };
        assert_eq!(found(&s.resolve(cid(A_ID), EndpointKind::Speakers)), Some(("{0}.{a-spk}", State::Active)));
        assert_eq!(found(&s.resolve(cid(B_ID), EndpointKind::Speakers)), Some(("{0}.{b-spk}", State::Active)), "not by its name");
        assert_eq!(found(&s.resolve(cid(B_ID), EndpointKind::CallMicrophone)), Some(("{1}.{b-call}", State::Disabled)));
        let r = s.resolve(cid(A_ID), EndpointKind::CallMicrophone);
        let e = r.endpoint().unwrap();
        assert_eq!((e.name.as_str(), e.instance_id.clone()), ("LLCable-A Out-5ms (ACX-LL-Cable A)", a.instance_id().to_lowercase()));
        assert_eq!(s.cable_labelled(&label("b")), Ok(Some(cid(B_ID))));
        assert_eq!(s.cable_labelled(&label("C")), Ok(None));
    }

    /// The trial's finding (2026-11-26): B removed and added again under the same instance ID gets new endpoints, Windows
    /// keeping the old ones NOTPRESENT with the same instance ID and pins. The role resolves to the new one; with only the old
    /// one there (Windows hasn't made the new one yet), to none, never the old id in its place.
    #[test]
    fn a_re_made_endpoint_resolves_to_its_new_id_never_the_old_one() {
        let b = cable("B", B_ID, 1);
        let old = |kind, id| ep(&b, kind, id, State::NotPresent);
        let nodes = vec![node("B", B_ID, 1, Variant::Both)];
        let s = Snapshot {
            nodes: nodes.clone(),
            endpoints: vec![
                old(EndpointKind::Speakers, "{0}.{old-spk}"),
                ep(&b, EndpointKind::Speakers, "{0}.{new-spk}", State::Active),
                ep(&b, EndpointKind::CallMicrophone, "{1}.{new-call}", State::Active),
                old(EndpointKind::CallMicrophone, "{1}.{old-call}"),
                old(EndpointKind::Microphone, "{1}.{old-mic}"),
            ],
        };
        assert_eq!(found(&s.resolve(cid(B_ID), EndpointKind::Speakers)), Some(("{0}.{new-spk}", State::Active)));
        assert_eq!(found(&s.resolve(cid(B_ID), EndpointKind::CallMicrophone)), Some(("{1}.{new-call}", State::Active)));
        // Only the old Microphone there: none, Windows not having made the new one yet
        let label_b = label("B");
        assert_eq!(
            s.resolve(cid(B_ID), EndpointKind::Microphone),
            Resolved::None(NoEndpoint::NotMade { label: label_b.clone(), role: EndpointKind::Microphone })
        );
        // B removed: its old endpoints linger, and none is B's
        let gone = Snapshot { nodes: vec![], endpoints: s.endpoints.clone() };
        assert_eq!(gone.resolve(cid(B_ID), EndpointKind::Speakers), Resolved::None(NoEndpoint::NotPresent(cid(B_ID))));
        // An earlier instance's endpoint (B before a variant switch, n 1) on a node no longer present: never B's now (n 2)
        let b2 = cable("B", B_ID, 2);
        let switched = Snapshot {
            nodes: vec![node("B", B_ID, 2, Variant::Call)],
            endpoints: vec![
                ep(&b, EndpointKind::Speakers, "{0}.{n1-spk}", State::Active),
                ep(&b2, EndpointKind::Speakers, "{0}.{n2-spk}", State::Disabled),
            ],
        };
        assert_eq!(found(&switched.resolve(cid(B_ID), EndpointKind::Speakers)), Some(("{0}.{n2-spk}", State::Disabled)));
    }

    /// None, and why: the role absent (a one-microphone variant), the cable disabled, not working, or not present.
    #[test]
    fn a_role_without_an_endpoint_says_why() {
        let b = cable("B", B_ID, 1);
        let mut low = node("B", B_ID, 1, Variant::LowLatency);
        let speakers = ep(&b, EndpointKind::Speakers, "{0}.{spk}", State::Active);
        let s = |n: &Node, eps: &[Endpoint]| Snapshot { nodes: vec![n.clone()], endpoints: eps.to_vec() };
        let r = s(&low, std::slice::from_ref(&speakers)).resolve(cid(B_ID), EndpointKind::CallMicrophone);
        let Resolved::None(why) = &r else { panic!("{r:?}") };
        assert_eq!(why, &NoEndpoint::NoSuchRole { label: label("B"), variant: Variant::LowLatency, role: EndpointKind::CallMicrophone });
        assert_eq!(
            why.to_string(),
            "ACX-LL-Cable B has no Out-5ms: it has Out only (`acx-ll-cable variant B both` gives it \
             both microphones)"
        );
        // Disabled: its endpoints NOTPRESENT (read without them), so none, by the disable
        low.health = Health::Problem(PROBLEM_DISABLED);
        let r = s(&low, &[]).resolve(cid(B_ID), EndpointKind::Speakers);
        assert_eq!(r, Resolved::None(NoEndpoint::Disabled { label: label("B") }));
        let Resolved::None(why) = r else { unreachable!() };
        assert!(why.to_string().ends_with("once it is enabled (`acx-ll-cable enable B`)"), "{why}");
        // The role it lacks is said first, whatever its state
        assert!(matches!(s(&low, &[]).resolve(cid(B_ID), EndpointKind::CallMicrophone), Resolved::None(NoEndpoint::NoSuchRole { .. })));
        // A failed node (the breaker's Code 31): not working
        low.health = Health::Problem(31);
        let r = s(&low, &[]).resolve(cid(B_ID), EndpointKind::Microphone);
        assert_eq!(r, Resolved::None(NoEndpoint::NotWorking { label: label("B"), health: Health::Problem(31) }));
        let Resolved::None(why) = r else { unreachable!() };
        assert!(
            why.to_string().contains("isn't working (a problem: Code 31") && why.to_string().ends_with("`acx-ll-cable add B` repairs it")
        );
        // Another cable present, this one not
        let a = Snapshot { nodes: vec![node("A", A_ID, 1, Variant::Both)], endpoints: vec![] };
        let r = a.resolve(cid(B_ID), EndpointKind::Speakers);
        assert_eq!(r, Resolved::None(NoEndpoint::NotPresent(cid(B_ID))));
        assert_eq!(r.endpoint(), None);
    }

    /// One identity on two nodes (a setup error): the endpoints of either, an active one first, then the newer node's; the
    /// reason for none from the working node, else the newest.
    #[test]
    fn a_duplicate_takes_the_active_endpoint_then_the_newest_nodes() {
        let (b1, b3) = (cable("B", B_ID, 1), cable("B", B_ID, 3));
        let mut old = node("B", B_ID, 1, Variant::Both);
        old.health = Health::Problem(28);
        let nodes = vec![old.clone(), node("B", B_ID, 3, Variant::Both)];
        let s = Snapshot {
            nodes: nodes.clone(),
            endpoints: vec![
                ep(&b1, EndpointKind::Speakers, "{0}.{1-spk}", State::Active),
                ep(&b3, EndpointKind::Speakers, "{0}.{3-spk}", State::Disabled),
                ep(&b1, EndpointKind::Microphone, "{1}.{1-mic}", State::Disabled),
                ep(&b3, EndpointKind::Microphone, "{1}.{3-mic}", State::Disabled),
            ],
        };
        assert_eq!(found(&s.resolve(cid(B_ID), EndpointKind::Speakers)), Some(("{0}.{1-spk}", State::Active)));
        assert_eq!(found(&s.resolve(cid(B_ID), EndpointKind::Microphone)), Some(("{1}.{3-mic}", State::Disabled)));
        assert_eq!(
            s.resolve(cid(B_ID), EndpointKind::CallMicrophone),
            Resolved::None(NoEndpoint::NotMade { label: label("B"), role: EndpointKind::CallMicrophone }),
            "the working node's reason"
        );
        // Two identities labelled B: no single answer for the label
        let other: CableId = "cccccccc-0000-4000-8000-00000000000c".parse().unwrap();
        let mut c = node("B", B_ID, 1, Variant::Both);
        c.cable = Some(CableInstance { label: label("B"), id: other, n: 1 });
        let two = Snapshot { nodes: vec![nodes[1].clone(), c], endpoints: vec![] };
        assert_eq!(two.cable_labelled(&label("B")), Err(vec![cid(B_ID), other]));
    }
}
