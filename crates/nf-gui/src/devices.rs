//! Device lists as ComboBox items: the friendly names are shown, and the endpoint ids are kept in a
//! parallel Vec (gui.md, "Device lists").
//!
//! A selected device that isn't in the list (unplugged, or a list that failed to load) shows as
//! "(missing) <id>" and stays selected, so the config keeps it until the user picks another.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};

use llcable_control::Label;
use llcable_control::voice::{DeviceInfo, Flow};

/// One ComboBox's items. Choosing `labels[i]` stores `ids[i]` (`None` = the system default).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct DeviceChoices {
    pub labels: Vec<String>,
    pub ids: Vec<Option<String>>,
    /// Always a valid index into `labels`, or `None` (nothing chosen: the placeholder shows).
    pub selected: Option<usize>,
    /// Changes whenever `labels` does. The ComboBox is keyed by it, so a changed list gets a new
    /// native control with its items set before its index (windows-reactor.md, ComboBox).
    pub key: u64,
}

impl DeviceChoices {
    /// What choosing item `index` stores; `None` for an index outside the list (a stale event).
    pub fn id_at(&self, index: usize) -> Option<Option<String>> {
        self.ids.get(index).cloned()
    }
}

/// Direct targets are cable identities. An unplugged selection stays visible until the owner chooses another.
pub(crate) fn direct_choices(devices: &[DeviceInfo], selected: Option<&str>) -> DeviceChoices {
    let mut labels = vec!["Automatic (A or the only cable)".to_owned()];
    let mut ids = vec![None];
    for device in devices.iter().filter(|d| d.flow == Flow::Render) {
        if let Some(id) = device.cable_id() {
            labels.push(picker_name(device).to_owned());
            ids.push(Some(id.to_string()));
        }
    }
    let selected = ids.iter().position(|id| id.as_deref() == selected).unwrap_or_else(|| {
        labels.push(format!("(missing) {}", selected.unwrap_or_default()));
        ids.push(selected.map(str::to_owned));
        labels.len() - 1
    });
    let mut hasher = DefaultHasher::new();
    labels.hash(&mut hasher);
    ids.hash(&mut hasher);
    DeviceChoices { labels, ids, selected: Some(selected), key: hasher.finish() }
}
/// How one picker lists its devices.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Picker<'a> {
    pub flow: Flow,
    /// The first entry, stored as `None`, when the role has a system default ("Default microphone").
    pub default_label: Option<&'a str>,
    /// A device left out (the echo reference never lists the output cable), unless it's the one
    /// selected.
    pub exclude: Option<&'a str>,
}

impl Picker<'_> {
    pub const MIC: Picker<'static> = Picker { flow: Flow::Capture, default_label: Some("Default microphone"), exclude: None };
    pub const OUTPUT: Picker<'static> = Picker { flow: Flow::Render, default_label: None, exclude: None };
    pub const PROBE: Picker<'static> = Picker { flow: Flow::Render, default_label: None, exclude: None };
    pub const CAPTURE: Picker<'static> = Picker { flow: Flow::Capture, default_label: None, exclude: None };

    /// The echo reference: render endpoints minus the output, plus the default speakers.
    pub fn reference(output: Option<&str>) -> Picker<'_> {
        Picker { flow: Flow::Render, default_label: Some("Default speakers"), exclude: output }
    }
}

/// Shorten only the complete factory name on a known cable. A user alias or another manufacturer's device keeps its Windows name.
pub fn picker_name(device: &DeviceInfo) -> &str {
    if let Some(role) = device.cable_role()
        && let Some((short, adapter)) = device.name.strip_suffix(')').and_then(|s| s.rsplit_once(" ("))
        && let Some((_, label)) = adapter.rsplit_once(' ')
        && let Ok(label) = label.parse::<Label>()
        && device.name == role.friendly_name(&label)
    {
        short
    } else {
        &device.name
    }
}

/// The items for `picker` from `devices`, with `selected` (a stored endpoint id) chosen.
pub(crate) fn choices(devices: &[DeviceInfo], picker: Picker<'_>, selected: Option<&str>) -> DeviceChoices {
    let listed: Vec<&DeviceInfo> = devices
        .iter()
        .filter(|d| d.flow == picker.flow)
        .filter(|d| !picker.exclude.is_some_and(|s| d.matches_selection(s)) || selected.is_some_and(|s| d.matches_selection(s)))
        .collect();
    let mut labels = Vec::with_capacity(listed.len() + 2);
    let mut ids = Vec::with_capacity(listed.len() + 2);
    if let Some(label) = picker.default_label {
        // Name the current default too: "Default microphone (USB Mic)"
        let default = devices.iter().find(|d| d.flow == picker.flow && d.is_default);
        labels.push(match default {
            Some(d) => format!("{label} ({})", picker_name(d)),
            None => label.to_owned(),
        });
        ids.push(None);
    }
    let mut counts = HashMap::new();
    for &d in &listed {
        let name = picker_name(d);
        // Count the labels people see; a different device or a user alias can match a shortened cable name.
        let count = counts.entry(name).or_insert(0);
        *count += 1;
        labels.push(if *count == 1 { name.to_owned() } else { format!("{name} #{count}") });
        ids.push(Some(d.selection().to_owned()));
    }
    let selected = match selected {
        None => picker.default_label.map(|_| 0),
        Some(id) => match listed.iter().position(|d| d.matches_selection(id)) {
            Some(i) => Some(i + usize::from(picker.default_label.is_some())),
            None => {
                labels.push(format!("(missing) {id}"));
                ids.push(Some(id.to_owned()));
                Some(ids.len() - 1)
            }
        },
    };
    let mut hasher = DefaultHasher::new();
    labels.hash(&mut hasher);
    DeviceChoices { labels, ids, selected, key: hasher.finish() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, name: &str, flow: Flow, is_default: bool) -> DeviceInfo {
        DeviceInfo {
            id: id.to_owned(),
            cable_selection: None,
            name: name.to_owned(),
            flow,
            is_default,
            is_default_communications: false,
            sample_rate: 48_000,
            channels: 2,
        }
    }

    fn devices() -> Vec<DeviceInfo> {
        vec![
            device("mic", "Microphone (USB Mic)", Flow::Capture, true),
            device("cable-out", "CABLE Output (VB-Audio Virtual Cable)", Flow::Capture, false),
            device("speakers", "Speakers (Realtek)", Flow::Render, true),
            device("cable-in", "CABLE Input (VB-Audio Virtual Cable)", Flow::Render, false),
        ]
    }

    fn labels(c: &DeviceChoices) -> Vec<&str> {
        c.labels.iter().map(String::as_str).collect()
    }

    #[test]
    fn the_mic_list_has_the_default_first() {
        let c = choices(&devices(), Picker::MIC, None);
        assert_eq!(labels(&c), [
            "Default microphone (Microphone (USB Mic))",
            "Microphone (USB Mic)",
            "CABLE Output (VB-Audio Virtual Cable)"
        ]);
        assert_eq!(c.ids, [None, Some("mic".to_owned()), Some("cable-out".to_owned())]);
        assert_eq!(c.selected, Some(0), "None is the default entry");
        let c = choices(&devices(), Picker::MIC, Some("cable-out"));
        assert_eq!(c.selected, Some(2));
        assert_eq!(c.id_at(2), Some(Some("cable-out".to_owned())));
        assert_eq!(c.id_at(0), Some(None), "choosing the default stores None");
        assert_eq!(c.id_at(3), None, "an index past the list is ignored");
    }

    #[test]
    fn the_output_list_has_no_default() {
        let c = choices(&devices(), Picker::OUTPUT, None);
        assert_eq!(labels(&c), ["Speakers (Realtek)", "CABLE Input (VB-Audio Virtual Cable)"]);
        assert_eq!(c.selected, None, "not chosen: the placeholder shows");
        assert_eq!(choices(&devices(), Picker::OUTPUT, Some("cable-in")).selected, Some(1));
    }

    #[test]
    fn the_reference_list_leaves_out_the_output() {
        let c = choices(&devices(), Picker::reference(Some("cable-in")), None);
        assert_eq!(labels(&c), ["Default speakers (Speakers (Realtek))", "Speakers (Realtek)"]);
        assert_eq!(c.selected, Some(0));
        // ...unless it's the reference already chosen: the selection stays visible.
        let c = choices(&devices(), Picker::reference(Some("cable-in")), Some("cable-in"));
        assert_eq!(c.selected, Some(2));
        assert_eq!(c.labels[2], "CABLE Input (VB-Audio Virtual Cable)");
    }

    #[test]
    fn cable_selections_follow_the_role_across_endpoint_replacement() {
        let mut endpoint = device("old", "Studio", Flow::Render, false);
        let key = llcable_control::voice::cable_endpoint_selection(llcable_control::CableId(17), llcable_control::EndpointKind::Speakers);
        endpoint.cable_selection = Some(key.clone());
        let mut settings = crate::settings::Settings::default();
        settings.engine.devices.output = Some("old".into());
        settings.ui.test_probe = Some("old".into());
        settings.ui.test_capture = Some("another".into());
        assert!(settings.bind_cable_endpoints(std::slice::from_ref(&endpoint)));
        assert_eq!(settings.engine.devices.output.as_ref(), Some(&key));
        assert_eq!(settings.ui.test_probe.as_ref(), Some(&key));
        assert_eq!(settings.ui.test_capture.as_deref(), Some("another"));
        assert!(!settings.bind_cable_endpoints(std::slice::from_ref(&endpoint)));
        endpoint.id = "replacement".into();
        endpoint.name = "Renamed studio".into();
        let list = [endpoint];
        let c = choices(&list, Picker::OUTPUT, Some(&key));
        assert_eq!(c.selected, Some(0));
        assert_eq!(c.labels, ["Renamed studio"]);
        assert_eq!(c.ids, [Some(key.clone())]);
        assert_eq!(choices(&list, Picker::reference(Some(&key)), None).ids, [None]);
    }

    #[test]
    fn a_missing_device_is_shown_and_kept() {
        let c = choices(&devices(), Picker::MIC, Some("{0.0.1.00000000}.{gone}"));
        assert_eq!(c.labels.last().map(String::as_str), Some("(missing) {0.0.1.00000000}.{gone}"));
        assert_eq!(c.selected, Some(c.labels.len() - 1));
        assert_eq!(c.id_at(c.labels.len() - 1), Some(Some("{0.0.1.00000000}.{gone}".to_owned())));
        // With no list at all (enumeration failed): only the default and the missing entry.
        let c = choices(&[], Picker::OUTPUT, Some("cable-in"));
        assert_eq!(labels(&c), ["(missing) cable-in"]);
        assert_eq!(c.selected, Some(0));
        let c = choices(&[], Picker::MIC, None);
        assert_eq!((labels(&c), c.selected), (vec!["Default microphone"], Some(0)));
    }

    #[test]
    fn a_device_of_the_other_flow_is_missing() {
        let c = choices(&devices(), Picker::PROBE, Some("mic"));
        assert_eq!(c.labels.last().map(String::as_str), Some("(missing) mic"));
    }

    #[test]
    fn duplicate_names_are_numbered() {
        let mut list = devices();
        list.push(device("mic2", "Microphone (USB Mic)", Flow::Capture, false));
        let c = choices(&list, Picker::CAPTURE, Some("mic2"));
        assert_eq!(labels(&c), ["Microphone (USB Mic)", "CABLE Output (VB-Audio Virtual Cable)", "Microphone (USB Mic) #2"]);
        assert_eq!(c.selected, Some(2));
    }

    #[test]
    fn only_known_factory_cable_names_are_shortened_and_aliases_keep_their_ids() {
        use llcable_control::{CableId, EndpointKind};

        let id = CableId(17);
        let roles = [EndpointKind::Speakers, EndpointKind::Microphone, EndpointKind::CallMicrophone];
        for role in roles {
            let flow = if role.is_render() { Flow::Render } else { Flow::Capture };
            let short = format!("LLCable-STUDIO2 {}", role.port());
            let full = format!("{short} (ACX-LL-Cable STUDIO2)");
            let key = llcable_control::voice::cable_endpoint_selection(id, role);
            let mut ours = device("cable-mm-id", &full, flow, true);
            ours.cable_selection = Some(key.clone());
            let third_party = device("foreign-mm-id", &full, flow, false);
            let alias = device("alias-mm-id", &short, flow, false);
            let picker = Picker { flow, default_label: Some("Default"), exclude: None };
            let choices = choices(&[ours.clone(), third_party, alias], picker, Some("cable-mm-id"));
            assert_eq!(choices.labels, [format!("Default ({short})"), short.clone(), full, format!("{short} #2")]);
            assert_eq!(choices.selected, Some(1));
            assert_eq!(choices.id_at(1), Some(Some(key)));
            ours.name = "Studio voice (ACX-LL-Cable STUDIO2)".into();
            assert_eq!(picker_name(&ours), ours.name);
            ours.name = format!("{short} (ACX-LL-Cable B)");
            assert_eq!(picker_name(&ours), ours.name);
        }
    }

    #[test]
    fn direct_picker_keeps_destination_identity_separately_from_wasapi() {
        let mut list = devices();
        let id = llcable_control::CableId(4);
        list[3].cable_selection = Some(llcable_control::voice::cable_endpoint_selection(id, llcable_control::EndpointKind::Speakers));
        let choices = direct_choices(&list, Some(&id.to_string()));
        assert_eq!(choices.selected, Some(1));
        assert_eq!(choices.id_at(1), Some(Some(id.to_string())));
        assert_eq!(choices.id_at(0), Some(None));
        let missing = direct_choices(&[], Some(&id.to_string()));
        assert_eq!(missing.id_at(missing.selected.unwrap()), Some(Some(id.to_string())));
    }

    #[test]
    fn the_selection_is_always_in_range() {
        let list = devices();
        let ids = [None, Some("mic"), Some("speakers"), Some("cable-in"), Some("nope")];
        let pickers = [Picker::MIC, Picker::OUTPUT, Picker::PROBE, Picker::CAPTURE, Picker::reference(Some("cable-in"))];
        for picker in pickers {
            for id in ids {
                for list in [&list[..], &[][..]] {
                    let c = choices(list, picker, id);
                    assert_eq!(c.labels.len(), c.ids.len());
                    assert!(c.selected.is_none_or(|i| i < c.labels.len()), "{picker:?} {id:?}");
                }
            }
        }
    }

    #[test]
    fn the_key_follows_the_labels() {
        let a = choices(&devices(), Picker::MIC, None);
        assert_eq!(a.key, choices(&devices(), Picker::MIC, Some("mic")).key, "same items, other selection");
        assert_ne!(a.key, choices(&devices(), Picker::MIC, Some("gone")).key, "a missing entry was added");
        assert_ne!(a.key, choices(&devices()[1..], Picker::MIC, None).key, "a device went away");
    }
}
