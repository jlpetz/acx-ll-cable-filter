//! Turning `run`'s options into an [`EngineConfig`]: the `--config` file, the processing overrides,
//! and device names resolved to endpoint ids. The pieces are pure so they're tested without
//! devices; [`engine_config`] puts them together with the live device list.

use std::path::Path;

use nf_engine::{DeviceInfo, EngineConfig, Flow, GateMode, ProcessingConfig};

use crate::cli::{EngineArgs, IoOptions, Processing};
use crate::exit::Failure;

/// Reads `--config FILE`.
pub fn load(path: &Path) -> Result<EngineConfig, Failure> {
    let text = std::fs::read_to_string(path).map_err(|e| Failure::error(format!("Couldn't read {}: {e}", path.display())))?;
    parse(&text).map_err(|e| Failure::error(format!("{} isn't a noise-filter config: {e}", path.display())))
}

/// The window's settings file (`{"engine": {...}, "ui": {...}}`) or a bare `EngineConfig`. Missing
/// fields take their defaults and unknown ones are ignored, as the window loads it; a UTF-8 BOM is
/// fine. The window's file is brought up to date by its version, as the window does
/// (`nf_gui::upgrade_saved_engine`); a bare config (`quality --save`'s `chain_config.json`) is taken
/// as it is.
pub fn parse(text: &str) -> Result<EngineConfig, String> {
    let value: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| e.to_string())?;
    let (engine, version) = match value {
        serde_json::Value::Object(mut map) if map.contains_key("engine") => {
            let version = map.get("version").and_then(serde_json::Value::as_u64).map_or(0, |v| u32::try_from(v).unwrap_or(u32::MAX));
            (map.remove("engine").unwrap_or_default(), Some(version))
        }
        serde_json::Value::Object(map) => (serde_json::Value::Object(map), None),
        _ => return Err("expected a JSON object".into()),
    };
    let mut cfg: EngineConfig = serde_json::from_value(engine).map_err(|e| e.to_string())?;
    if let Some(v) = version {
        nf_gui::upgrade_saved_engine(v, &mut cfg);
    }
    Ok(cfg)
}

impl Processing {
    /// Applies the options given over `p`. `--threshold` without `--gate` turns the static gate on.
    pub fn apply(&self, p: &mut ProcessingConfig) {
        if let Some(v) = self.aec {
            p.aec_enabled = v;
        }
        if let Some(v) = self.noise {
            p.noise_enabled = v;
        }
        if let Some(v) = self.floor_db {
            p.floor_db = v;
        }
        if let Some(v) = self.threshold_db {
            p.gate.threshold_db = v;
            p.gate.mode = GateMode::Static;
        }
        if let Some(v) = self.gate {
            p.gate.mode = v;
        }
        if let Some(v) = &self.model {
            p.noise_model = v.clone();
        }
        if let Some(v) = self.high_band {
            p.high_band = v;
        }
        if let Some(v) = self.model_depth {
            p.model_depth = v;
        }
    }
}

impl IoOptions {
    /// Applies the options given over `io`.
    pub fn apply(&self, io: &mut nf_engine::IoConfig) {
        if let Some(v) = self.output_exclusive {
            io.output_exclusive = v;
        }
        if let Some(v) = self.mic_exclusive {
            io.mic_exclusive = v;
        }
        if let Some(v) = self.adaptive_output {
            io.adaptive_output = v;
        }
    }
}

/// The endpoint id `query` names among `devices` of `flow`: an id, else a name equal to it (any
/// case), else the first name containing it. `what` names the option for the error.
pub fn resolve_device(devices: &[DeviceInfo], query: &str, flow: Flow, what: &str) -> Result<String, String> {
    let kind = |f: Flow| if f == Flow::Capture { "recording" } else { "playback" };
    if let Some(d) = devices.iter().find(|d| d.id == query) {
        return if d.flow == flow {
            Ok(d.id.clone())
        } else {
            Err(format!("{what}: \"{}\" is a {} device; {what} needs a {} device", d.name, kind(d.flow), kind(flow)))
        };
    }
    let q = query.to_lowercase();
    let of_flow = || devices.iter().filter(|d| d.flow == flow);
    of_flow()
        .find(|d| d.name.to_lowercase() == q)
        .or_else(|| of_flow().find(|d| d.name.to_lowercase().contains(&q)))
        .map(|d| d.id.clone())
        .ok_or_else(|| format!("{what}: no active {} device matches \"{query}\". `noise-filter devices` lists them", kind(flow)))
}

/// The config `run` and `latency` start the engine with: `--config` (or the defaults), the
/// processing overrides, and the devices given on the command line, resolved against
/// `list_devices` (called only if a device was given).
pub fn engine_config(args: &EngineArgs, list_devices: impl FnOnce() -> Result<Vec<DeviceInfo>, String>) -> Result<EngineConfig, Failure> {
    let mut cfg = match &args.config {
        Some(path) => load(path)?,
        None => EngineConfig::default(),
    };
    args.processing.apply(&mut cfg.processing);
    args.io.apply(&mut cfg.io);
    if args.mic.is_some() || args.output.is_some() || args.reference.is_some() {
        let devices = list_devices().map_err(|e| Failure::error(format!("Couldn't list the audio devices: {e}")))?;
        let resolve = |q: &Option<String>, flow, what| -> Result<Option<String>, Failure> {
            q.as_deref().map(|q| resolve_device(&devices, q, flow, what).map_err(Failure::error)).transpose()
        };
        if let Some(id) = resolve(&args.mic, Flow::Capture, "--mic")? {
            cfg.devices.mic = Some(id);
        }
        if let Some(id) = resolve(&args.output, Flow::Render, "--output")? {
            cfg.devices.output = Some(id);
        }
        if let Some(id) = resolve(&args.reference, Flow::Render, "--reference")? {
            cfg.devices.reference = Some(id);
        }
    }
    if cfg.devices.output.as_deref().is_none_or(str::is_empty) {
        return Err(Failure::usage(
            "Choose the playback device the filtered mic goes to with --output, e.g. --output \"CABLE Input\" \
             (`noise-filter devices` lists them)",
        ));
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit::Exit;

    fn dev(id: &str, name: &str, flow: Flow) -> DeviceInfo {
        DeviceInfo {
            id: id.into(),
            name: name.into(),
            flow,
            is_default: false,
            is_default_communications: false,
            sample_rate: 48_000,
            channels: 2,
        }
    }

    fn devices() -> Vec<DeviceInfo> {
        vec![
            dev("{cap-1}", "Microphone (Realtek Audio)", Flow::Capture),
            dev("{cap-2}", "CABLE Output (VB-Audio Virtual Cable)", Flow::Capture),
            dev("{ren-1}", "Speakers (Realtek Audio)", Flow::Render),
            dev("{ren-2}", "CABLE Input 2 (VB-Audio Virtual Cable)", Flow::Render),
            dev("{ren-3}", "CABLE Input", Flow::Render),
        ]
    }

    #[test]
    fn devices_resolve_by_id_then_exact_name_then_substring() {
        let d = devices();
        assert_eq!(resolve_device(&d, "{ren-1}", Flow::Render, "--output"), Ok("{ren-1}".into()));
        // An exact name beats an earlier substring match, in any case
        assert_eq!(resolve_device(&d, "cable input", Flow::Render, "--output"), Ok("{ren-3}".into()));
        // Otherwise the first name containing it
        assert_eq!(resolve_device(&d, "CABLE", Flow::Render, "--output"), Ok("{ren-2}".into()));
        assert_eq!(resolve_device(&d, "realtek", Flow::Capture, "--mic"), Ok("{cap-1}".into()));
        assert_eq!(resolve_device(&d, "realtek", Flow::Render, "--reference"), Ok("{ren-1}".into()));
        // Only the requested direction
        let e = resolve_device(&d, "CABLE Output", Flow::Render, "--output").unwrap_err();
        assert!(e.contains("no active playback device matches \"CABLE Output\""), "{e}");
        let e = resolve_device(&d, "{cap-2}", Flow::Render, "--output").unwrap_err();
        assert_eq!(e, "--output: \"CABLE Output (VB-Audio Virtual Cable)\" is a recording device; --output needs a playback device");
        assert!(resolve_device(&[], "x", Flow::Capture, "--mic").is_err());
    }

    #[test]
    fn config_files_are_the_windows_settings_or_a_bare_engine_config() {
        let gui = r#"{"engine": {"devices": {"output": "{ren-3}"}, "processing": {"aec_enabled": true}}, "ui": {"anything": 1}}"#;
        let c = parse(gui).unwrap();
        assert_eq!(c.devices.output.as_deref(), Some("{ren-3}"));
        assert!(c.processing.aec_enabled);
        // Missing fields take their defaults
        assert_eq!(c.processing.floor_db, ProcessingConfig::default().floor_db);
        let bare = "\u{feff}{\"processing\": {\"floor_db\": -12.0}, \"future_field\": true}";
        assert_eq!(parse(bare).unwrap().processing.floor_db, -12.0);
        assert_eq!(parse("{}").unwrap(), EngineConfig::default());
        assert!(parse("[1, 2]").is_err());
        assert!(parse("{\"processing\": {\"floor_db\": \"loud\"}}").is_err());
        assert!(parse("not json").is_err());
        // The window's file is brought up to date by its version (an older one's saved depth of 2 was the default then:
        // automatic now); a bare config keeps its depth, as `quality --save` wrote what ran.
        let depth = |json: &str| parse(json).unwrap().processing.model_depth;
        assert_eq!(depth(r#"{"engine": {"processing": {"model_depth": 2}}, "ui": {}}"#), nf_engine::AUTO_MODEL_DEPTH);
        assert_eq!(depth(r#"{"version": 1, "engine": {"processing": {"model_depth": 2}}}"#), 2);
        assert_eq!(depth(r#"{"processing": {"model_depth": 2}}"#), 2);
    }

    #[test]
    fn processing_options_override_the_config() {
        let mut p = ProcessingConfig::default();
        Processing::default().apply(&mut p);
        assert_eq!(p, ProcessingConfig::default());
        Processing { aec: Some(true), noise: Some(false), floor_db: Some(-20.0), threshold_db: Some(-45.0), ..Default::default() }
            .apply(&mut p);
        assert!(p.aec_enabled && !p.noise_enabled);
        assert_eq!((p.floor_db, p.gate.mode, p.gate.threshold_db), (-20.0, GateMode::Static, -45.0));
        // An explicit --gate wins over --threshold's implied static gate
        Processing { gate: Some(GateMode::Auto), threshold_db: Some(-40.0), ..Default::default() }.apply(&mut p);
        assert_eq!((p.gate.mode, p.gate.threshold_db), (GateMode::Auto, -40.0));
        assert_eq!((p.noise_model.as_str(), p.high_band), ("rnnoise", nf_engine::HighBand::Model), "kept unless given");
        Processing { model: Some("ulunas".into()), high_band: Some(nf_engine::HighBand::Pass), ..Default::default() }.apply(&mut p);
        assert_eq!(
            (p.noise_model.as_str(), p.high_band, p.model_depth),
            ("ulunas", nf_engine::HighBand::Pass, nf_engine::AUTO_MODEL_DEPTH)
        );
        Processing { model_depth: Some(1), ..Default::default() }.apply(&mut p);
        assert_eq!(p.model_depth, 1);
    }

    /// The latency options go over the config's: given ones win, the rest keep the file's, or the defaults (all on: the
    /// owner, 2026-11-02).
    #[test]
    fn the_latency_options_override_the_config() {
        let mut io = nf_engine::IoConfig::default();
        assert!(io.output_exclusive && io.mic_exclusive && io.adaptive_output, "on by default");
        IoOptions::default().apply(&mut io);
        assert_eq!(io, nf_engine::IoConfig::default(), "nothing given changes nothing");
        IoOptions { output_exclusive: Some(false), mic_exclusive: None, adaptive_output: Some(false) }.apply(&mut io);
        assert!(!io.output_exclusive && io.mic_exclusive && !io.adaptive_output);
        IoOptions { output_exclusive: Some(true), ..Default::default() }.apply(&mut io);
        assert!(io.output_exclusive && !io.adaptive_output);
        let with = |file: &str, io: IoOptions| {
            let dir = crate::test_dir("config-io");
            let path = dir.join("settings.json");
            std::fs::write(&path, file).unwrap();
            let args = EngineArgs { config: Some(path), output: Some("{ren-3}".into()), io, ..Default::default() };
            let c = engine_config(&args, || Ok(devices())).unwrap().io;
            let _ = std::fs::remove_dir_all(dir);
            (c.output_exclusive, c.mic_exclusive, c.adaptive_output)
        };
        // The defaults, through a file that names none of them (an older settings file or engine config) and with no flag
        assert_eq!(with(r#"{"engine": {"io": {"raw_mic": false}}}"#, IoOptions::default()), (true, true, true));
        assert_eq!(with("{}", IoOptions::default()), (true, true, true));
        // A file's false stays false, and a flag goes over the file either way
        let off = r#"{"engine": {"io": {"output_exclusive": false, "mic_exclusive": false, "adaptive_output": false}}}"#;
        assert_eq!(with(off, IoOptions::default()), (false, false, false));
        let on = IoOptions { output_exclusive: Some(true), mic_exclusive: Some(true), adaptive_output: None };
        assert_eq!(with(off, on), (true, true, false));
        let bare = r#"{"io": {"mic_exclusive": false}}"#;
        assert_eq!(with(bare, IoOptions::default()), (true, false, true), "a bare engine config");
        let no = IoOptions { output_exclusive: Some(false), mic_exclusive: None, adaptive_output: Some(false) };
        assert_eq!(with(bare, no.clone()), (false, false, false));
        // Without a file, the flags over the defaults
        let args = EngineArgs { output: Some("{ren-3}".into()), io: no, ..Default::default() };
        let c = engine_config(&args, || Ok(devices())).unwrap();
        assert!(!c.io.output_exclusive && c.io.mic_exclusive && !c.io.adaptive_output);
    }

    #[test]
    fn the_engine_config_resolves_only_given_devices_and_needs_an_output() {
        let list = || Ok(devices());
        let args = EngineArgs { output: Some("CABLE Input".into()), mic: Some("realtek".into()), ..Default::default() };
        let c = engine_config(&args, list).unwrap();
        assert_eq!((c.devices.output.as_deref(), c.devices.mic.as_deref(), c.devices.reference), (Some("{ren-3}"), Some("{cap-1}"), None));

        let none = EngineArgs::default();
        let e = engine_config(&none, || panic!("no device was given, so the list isn't needed")).unwrap_err();
        assert_eq!(e.exit, Exit::Usage);
        assert!(e.message.contains("--output"), "{}", e.message);

        let e = engine_config(&EngineArgs { output: Some("nothing".into()), ..Default::default() }, list).unwrap_err();
        assert_eq!(e.exit, Exit::Error);
        let e = engine_config(&EngineArgs { output: Some("x".into()), ..Default::default() }, || Err("COM failed".into())).unwrap_err();
        assert_eq!(e.message, "Couldn't list the audio devices: COM failed");
    }

    #[test]
    fn a_config_file_supplies_devices_and_settings() {
        let dir = crate::test_dir("config");
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"engine": {"devices": {"output": "{ren-2}"}, "processing": {"noise_enabled": false}}}"#).unwrap();
        let args = EngineArgs {
            config: Some(path.clone()),
            processing: Processing { aec: Some(true), ..Default::default() },
            ..Default::default()
        };
        let c = engine_config(&args, || panic!("no device was given")).unwrap();
        assert_eq!(c.devices.output.as_deref(), Some("{ren-2}"));
        assert!(!c.processing.noise_enabled && c.processing.aec_enabled);
        let e = load(&dir.join("missing.json")).unwrap_err();
        assert!(e.message.starts_with("Couldn't read"), "{}", e.message);
        std::fs::write(&path, "{").unwrap();
        assert!(load(&path).unwrap_err().message.contains("isn't a noise-filter config"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
