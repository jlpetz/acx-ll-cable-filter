//! Imports cable choices from `acx-ll-cable migrate|add|update --endpoint-map`. Run once as the owner of the settings.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nf_vmic::cables::EndpointKind;
use nf_vmic::identity::CableId;
use nf_vmic::setup::endpoint_map::{self, Match};
use serde::Deserialize;
use serde_json::Value;

use crate::exit::Failure;
use crate::mutex::NamedMutex;

// Read only the fields that identify a choice. Names and instance paths in the map are diagnostic.
#[derive(Deserialize)]
struct Map {
    format: String,
    version: u32,
    command: String,
    result: String,
    cables: Vec<Cable>,
}

#[derive(Deserialize)]
struct Cable {
    id: String,
    endpoints: Vec<Row>,
}

#[derive(Deserialize)]
struct Row {
    role: String,
    flow: String,
    #[serde(rename = "match")]
    matched: Match,
    old: Option<Side>,
    new: Option<Side>,
}

#[derive(Deserialize)]
struct Side {
    id: String,
}

type Selection = (CableId, EndpointKind);

impl Map {
    /// One target per old Windows ID. A conflict or a row without a unique successor leaves the saved choice alone.
    fn index(&self) -> Result<HashMap<&str, Option<Selection>>, String> {
        if self.format != endpoint_map::FORMAT || self.version != endpoint_map::VERSION {
            return Err("Unrecognized endpoint map format or version.".into());
        }
        if !matches!(self.command.as_str(), "migrate" | "add" | "update") || !matches!(self.result.as_str(), "done" | "nothing") {
            return Err("Use an endpoint map from a completed migration, add or update.".into());
        }
        let mut by_old = HashMap::new();
        for cable in &self.cables {
            let id = cable.id.parse::<CableId>().map_err(|e| format!("Invalid cable ID in endpoint map: {e}"))?;
            for row in &cable.endpoints {
                let role = EndpointKind::from_arg(&row.role).ok_or_else(|| format!("Unknown endpoint role '{}'.", row.role))?;
                let flow = if role.is_render() { "render" } else { "capture" };
                if row.flow != flow {
                    return Err(format!("Invalid flow for endpoint role '{}'.", row.role));
                }
                let old = row.old.as_ref().map(|s| s.id.as_str());
                let new = row.new.as_ref().map(|s| s.id.as_str());
                let target = match (row.matched, old, new) {
                    (Match::Added, None, Some(n)) if !n.is_empty() => continue,
                    (Match::Same, Some(o), Some(n)) if !o.is_empty() && o == n => Some((id, role)),
                    (Match::Moved, Some(o), Some(n)) if !o.is_empty() && !n.is_empty() && o != n => Some((id, role)),
                    (Match::Gone | Match::Ambiguous, Some(o), None) if !o.is_empty() => None,
                    _ => return Err(format!("Invalid old/new endpoints for '{}' in endpoint map.", row.role)),
                };
                match by_old.entry(old.expect("a row with an old endpoint")) {
                    Entry::Vacant(entry) => {
                        entry.insert(target);
                    }
                    Entry::Occupied(mut entry) if *entry.get() != target => {
                        entry.insert(None);
                    }
                    _ => {}
                }
            }
        }
        Ok(by_old)
    }
}

#[derive(Default)]
struct Changes {
    fields: Vec<&'static str>,
    unresolved: Vec<&'static str>,
}

/// Edit just the selected device fields. Keep the file's version, UI preferences, and fields newer versions have added.
fn apply(map: &Map, settings: &mut Value) -> Result<Changes, String> {
    let index = map.index()?;
    if !settings.is_object() {
        return Err("Expected a settings JSON object.".into());
    }
    let wrapped = settings.get("engine").is_some() || settings.get("ui").is_some();
    let paths = if wrapped {
        &[
            ("/engine/devices/mic", false),
            ("/engine/devices/output", true),
            ("/engine/devices/reference", true),
            ("/ui/test_probe", true),
            ("/ui/test_capture", false),
        ][..]
    } else {
        &[("/devices/mic", false), ("/devices/output", true), ("/devices/reference", true)][..]
    };
    let mut changes = Changes::default();
    for &(path, render) in paths {
        let Some(field) = settings.pointer_mut(path) else { continue };
        let Some(target) = field.as_str().and_then(|id| index.get(id)) else { continue };
        match target {
            Some((id, role)) if role.is_render() == render => {
                *field = Value::String(nf_engine::cable_endpoint_selection(*id, *role));
                changes.fields.push(path);
            }
            _ => changes.unresolved.push(path),
        }
    }
    Ok(changes)
}

pub fn run(map_path: &Path, config: Option<&Path>) -> Result<(), Failure> {
    let path = config.map(PathBuf::from).or_else(nf_gui::default_settings_path).unwrap_or_else(|| PathBuf::from("settings.json"));
    let mutex = NamedMutex::open(crate::instance::MUTEX_NAME).map_err(|e| Failure::error(format!("Couldn't lock the settings: {e}")))?;
    let _guard = mutex.acquire(Duration::ZERO).ok_or_else(|| Failure::error("Close the noise-filter settings window and retry."))?;
    let read = |p: &Path| std::fs::read_to_string(p).map_err(|e| Failure::error(format!("Couldn't read {}: {e}", p.display())));
    let map: Map = serde_json::from_str(read(map_path)?.trim_start_matches('\u{feff}'))
        .map_err(|e| Failure::error(format!("Invalid endpoint map {}: {e}", map_path.display())))?;
    let mut settings = serde_json::from_str(read(&path)?.trim_start_matches('\u{feff}'))
        .map_err(|e| Failure::error(format!("Invalid settings in {}: {e}", path.display())))?;
    let changes = apply(&map, &mut settings).map_err(Failure::error)?;
    if !changes.fields.is_empty() {
        let json = serde_json::to_string_pretty(&settings).map_err(|e| Failure::error(e.to_string()))?;
        endpoint_map::publish(&path, &json).map_err(Failure::error)?;
    }
    say!("{} saved device choice(s) updated in {}.", changes.fields.len(), path.display());
    if !changes.unresolved.is_empty() {
        say!("No unambiguous endpoint of the required kind for {}. Select these in the settings window.", changes.unresolved.join(", "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const ID: &str = "aaaaaaaa-0000-4000-8000-00000000000a";

    fn map(rows: Value) -> Map {
        serde_json::from_value(json!({
            "format": endpoint_map::FORMAT, "version": endpoint_map::VERSION, "command": "migrate", "result": "done",
            "cables": [{"id":ID, "endpoints":rows, "future_field":2}]
        }))
        .unwrap()
    }

    fn row(role: EndpointKind, old: Option<&str>, new: Option<&str>, matched: Match) -> Value {
        let side = |id| json!({"id":id});
        json!({"role":role.name(), "flow":if role.is_render() {"render"} else {"capture"}, "old":old.map(side), "new":new.map(side), "match":matched})
    }

    #[test]
    fn settings_imports_only_known_choices_with_unique_targets_and_is_idempotent() {
        use EndpointKind::*;
        let map = map(json!([
            row(Speakers, Some("spk"), Some("spk"), Match::Same),
            row(Microphone, Some("mic"), Some("mic2"), Match::Moved),
            row(CallMicrophone, None, Some("call2"), Match::Added),
            row(CallMicrophone, Some("old-call"), None, Match::Gone),
            row(Speakers, Some("old-ref"), None, Match::Ambiguous)
        ]));
        let mut cfg = json!({
            "engine":{"devices":{"mic":"mic","output":"spk","reference":"old-ref","future":"mic"},"effect":{"name":"spk"}},
            "ui":{"test_probe":"spk","test_capture":"old-call"},"version":50,"future":["spk",99]
        });
        let before = cfg.clone();
        let changes = apply(&map, &mut cfg).unwrap();
        let key = |role| Value::String(nf_engine::cable_endpoint_selection(ID.parse().unwrap(), role));
        let mut expected = before;
        expected["engine"]["devices"]["mic"] = key(Microphone);
        expected["engine"]["devices"]["output"] = key(Speakers);
        expected["ui"]["test_probe"] = key(Speakers);
        assert_eq!(cfg, expected);
        assert_eq!(changes.fields.len(), 3);
        assert_eq!(changes.unresolved, ["/engine/devices/reference", "/ui/test_capture"]);
        assert!(apply(&map, &mut cfg).unwrap().fields.is_empty());
        let mut bare = json!({"devices":{"mic":"mic","output":"physical","reference":null}});
        assert_eq!(apply(&map, &mut bare).unwrap().fields, ["/devices/mic"]);
        assert_eq!(bare, json!({"devices":{"mic":key(Microphone),"output":"physical","reference":null}}));
    }

    #[test]
    fn conflicting_mappings_wrong_flows_and_incomplete_maps_do_not_retarget_choices() {
        use EndpointKind::*;
        let mut map = map(json!([
            row(Microphone, Some("mic"), Some("new-mic"), Match::Moved),
            row(CallMicrophone, Some("mic"), Some("new-call"), Match::Moved),
            row(Speakers, Some("speaker"), Some("new-speaker"), Match::Moved)
        ]));
        let mut settings = json!({"engine":{"devices":{"mic":"speaker"}},"ui":{"test_capture":"mic"}});
        let original = settings.clone();
        assert_eq!(apply(&map, &mut settings).unwrap().unresolved, ["/engine/devices/mic", "/ui/test_capture"]);
        assert_eq!(settings, original);
        map.result = "restart".into();
        assert!(apply(&map, &mut settings).is_err());
        map.result = "done".into();
        map.version += 1;
        assert!(apply(&map, &mut settings).is_err());
        assert_eq!(settings, original);
    }

    #[test]
    fn a_package_update_can_remap_choices_only_after_it_finished() {
        let mut map = map(json!([row(EndpointKind::Microphone, Some("mic-before-update"), Some("replacement"), Match::Moved)]));
        map.command = "update".into();
        let mut settings = json!({"devices": {"mic": "mic-before-update"}});
        for partial in ["error", "restart"] {
            map.result = partial.into();
            assert!(apply(&map, &mut settings).is_err());
            assert_eq!(settings["devices"]["mic"], "mic-before-update");
        }
        map.result = "done".into();
        assert_eq!(apply(&map, &mut settings).unwrap().fields, ["/devices/mic"]);
        assert_eq!(settings["devices"]["mic"], nf_engine::cable_endpoint_selection(ID.parse().unwrap(), EndpointKind::Microphone));
    }
}
