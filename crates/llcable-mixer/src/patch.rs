//! The session document contains stable endpoint selectors and cable identities. Device-list indexes are never saved.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use llcable_audio::RouteConfig;
use llcable_control::routing::{Bus, Ducking, Effect, Send as AudioSend, StageMeter, Tap};
use llcable_control::voice::{DeviceInfo, Flow};
use llcable_control::{CableId, CableInfo};
use nf_gui::mixer_meter::{MeterMode, MeterPalette};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SourceSend {
    pub source: String,
    pub gain_db: i16,
    pub balance: i16,
    #[serde(default)]
    pub after_effects: bool,
    /// Number of shared stages through the selected stage. None preserves the original/end choice in older scenes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary: Option<usize>,
    #[serde(default)]
    pub effects: Vec<Stage>,
}

impl SourceSend {
    pub fn depth(&self, len: usize) -> usize {
        self.boundary.unwrap_or(if self.after_effects { len } else { 0 }).min(len)
    }
    pub fn tap_name(&self, stages: &[Stage]) -> String {
        if self.boundary.is_some()
            && let Some(i) = self.depth(stages.len()).checked_sub(1)
        {
            return format!("After {}. {}", i + 1, stages[i].title());
        }
        if self.boundary.is_some() || !self.after_effects { "Original".into() } else { "After FX".into() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Kind {
    Gain,
    Equalizer,
    Rnnoise,
    Sonora,
    Gate,
    Duck,
    Aec,
    Clap,
}

impl Kind {
    pub const ALL: [Self; 8] = [Self::Gain, Self::Equalizer, Self::Rnnoise, Self::Sonora, Self::Gate, Self::Duck, Self::Aec, Self::Clap];

    pub fn title(self) -> &'static str {
        match self {
            Self::Gain => "Gain",
            Self::Equalizer => "Stereo equalizer",
            Self::Rnnoise => "RNNoise",
            Self::Sonora => "Sonora noise suppression",
            Self::Gate => "Noise gate",
            Self::Duck => "Sidechain ducking",
            Self::Aec => "Sonora echo cancellation",
            Self::Clap => "CLAP plug-in",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stage {
    pub kind: Kind,
    pub enabled: bool,
    pub db: i16,
    #[serde(default)]
    pub eq_db: [i16; 3],
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clap: Option<Plugin>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Plugin {
    pub path: PathBuf,
    pub info: llcable_effects::EffectInfo,
    pub params: BTreeMap<u32, f64>,
}

impl Stage {
    pub fn title(&self) -> &str {
        self.clap.as_ref().map_or(self.kind.title(), |plugin| &plugin.info.name)
    }

    pub fn new(kind: Kind) -> Self {
        let db = match kind {
            Kind::Gate => -50,
            Kind::Duck => 12,
            _ => 0,
        };
        Self { kind, enabled: true, db, eq_db: [0; 3], key: None, clap: None }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Mix {
    pub sends: Vec<SourceSend>,
    pub effects: Vec<Stage>,
    pub muted: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub disconnected: BTreeMap<String, SourceSend>,
}

#[derive(Clone, Default, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub mixes: BTreeMap<String, Mix>,
    pub source_effects: BTreeMap<String, Vec<Stage>>,
    pub focused: Option<String>,
    pub meter_mode: MeterMode,
    pub palette: MeterPalette,
    pub roomy: bool,
    pub presets: BTreeMap<String, Preset>,
    pub theme: Theme,
}

#[derive(Clone, Default, Debug, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    pub mixes: BTreeMap<String, Mix>,
    pub source_effects: BTreeMap<String, Vec<Stage>>,
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Serialize, Deserialize)]
pub enum Theme {
    #[default]
    System,
    Dark,
    Light,
}

impl Settings {
    pub fn toggle(&mut self, output: &str, source: &str) {
        let mix = self.mixes.entry(output.to_owned()).or_default();
        if let Some(index) = mix.sends.iter().position(|s| s.source == source) {
            mix.disconnected.insert(source.to_owned(), mix.sends.remove(index));
        } else {
            let send = mix.disconnected.remove(source).unwrap_or_else(|| SourceSend {
                source: source.to_owned(),
                gain_db: 0,
                balance: 0,
                after_effects: true,
                boundary: None,
                effects: Vec::new(),
            });
            mix.sends.push(send);
        }
    }

    /// Keep a tap attached to the same stage through a move. Removing its stage falls back to the preceding boundary.
    pub fn remap_taps(&mut self, source: &str, index: usize, moved_to: Option<usize>) {
        for m in self.mixes.values_mut() {
            for send in m.sends.iter_mut().chain(m.disconnected.values_mut()).filter(|s| s.source == source) {
                let Some(n) = send.boundary.and_then(|n| n.checked_sub(1)) else { continue };
                send.boundary = Some(match moved_to {
                    Some(to) if n == index => to + 1,
                    Some(_) if moved_to == Some(n) => index + 1,
                    None if n >= index => n,
                    _ => n + 1,
                });
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    pub id: String,
    pub label: String,
    pub owner: Option<CableId>,
    pub channels: u16,
}

#[derive(Clone, Debug)]
pub struct Destination {
    pub id: String,
    pub name: String,
    pub cable: Option<CableId>,
}

impl Destination {
    pub fn receives_itself(&self, source: &Source) -> bool {
        if let Some(cable) = self.cable {
            source.owner == Some(cable)
        } else {
            source.id.strip_prefix("loopback:") == self.id.strip_prefix("play:")
        }
    }
}

pub struct Inventory {
    pub cables: Vec<CableInfo>,
    pub outputs: Vec<Destination>,
    pub sources: Vec<Source>,
}

impl Inventory {
    pub fn read() -> Result<Self, String> {
        let cables = llcable_control::cables().map_err(|e| format!("Cable discovery failed: {e}"))?;
        let devices = llcable_control::voice::list_devices().map_err(|e| e.to_string())?;
        let sources = devices.iter().map(|d| Self::source(&cables, d)).collect();
        let mut outputs: Vec<_> = cables
            .iter()
            .map(|c| Destination {
                id: c.id.to_string(),
                name: c
                    .ports
                    .iter()
                    .find(|p| p.role == llcable_control::EndpointKind::Microphone)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| format!("LLCable-{} Out", c.label)),
                cable: Some(c.id),
            })
            .collect();
        outputs.extend(
            devices
                .iter()
                .filter(|d| d.flow == Flow::Render && !cables.iter().any(|c| c.ports.iter().any(|p| p.id.eq_ignore_ascii_case(&d.id))))
                .map(|d| Destination { id: format!("play:{}", d.selection()), name: nf_gui::picker_name(d).to_owned(), cable: None }),
        );
        Ok(Self { cables, sources, outputs })
    }

    fn source(cables: &[CableInfo], d: &DeviceInfo) -> Source {
        let owner = cables.iter().find(|cable| cable.ports.iter().any(|port| port.id.eq_ignore_ascii_case(&d.id))).map(|c| c.id);
        let (id, label) = if d.flow == Flow::Render {
            (format!("loopback:{}", d.selection()), format!("{} · playback monitor", nf_gui::picker_name(d)))
        } else {
            (d.selection().to_owned(), nf_gui::picker_name(d).to_owned())
        };
        Source { id, label, owner, channels: d.channels }
    }

    pub fn label(&self, id: &str) -> String {
        self.sources.iter().find(|s| s.id == id).map(|s| s.label.clone()).unwrap_or_else(|| format!("Unavailable source ({id})"))
    }
}

#[derive(Default)]
pub struct Channels {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub meters: Vec<(String, StageMeter)>,
}

pub struct Plan {
    pub config: RouteConfig,
    pub channels: Channels,
}

fn input_tap(inputs: &mut Vec<String>, published: &BTreeMap<CableId, usize>, id: &str, inventory: &Inventory) -> Result<Tap, String> {
    let source = inventory.sources.iter().find(|s| s.id == id).ok_or_else(|| format!("A selected source is unavailable: {id}"))?;
    // A cable Out already produced by this session is exactly the graph's output samples. A playback monitor
    // instead captures what other apps play to the cable's In, even while this graph feeds its Out.
    if !id.starts_with("loopback:")
        && let Some(bus) = source.owner.and_then(|owner| published.get(&owner))
    {
        return Ok(Tap::Bus(*bus));
    }
    let index = inputs.iter().position(|key| key == id).unwrap_or_else(|| {
        inputs.push(source.id.clone());
        inputs.len() - 1
    });
    Ok(Tap::Input(index))
}

/// Order cable sinks before all subscribers, including dependencies introduced by an effect's sidechain.
fn ordered_outputs<'a>(settings: &Settings, inv: &'a Inventory) -> Result<Vec<&'a Destination>, String> {
    let mut edges = BTreeMap::new();
    for (id, mix) in settings.mixes.iter().filter(|(_, mix)| !mix.sends.is_empty()) {
        let output = inv.outputs.iter().find(|c| c.id == *id).ok_or_else(|| format!("An output is missing: {id}"))?;
        let dependencies = edges.entry(output.id.as_str()).or_insert_with(Vec::new);
        let mut check = |key: &str| -> Result<(), String> {
            let source = inv.sources.iter().find(|s| s.id == key).ok_or_else(|| format!("A selected source is unavailable: {key}"))?;
            if output.receives_itself(source) {
                return Err(format!("{} would feed back into {}. Change a source or destination.", source.label, output.name));
            }
            // A recording on an LLCable may already be a bus. Monitoring a playback device is always an independent WASAPI input.
            let dependency = if let Some(monitor) = key.strip_prefix("loopback:") {
                inv.outputs.iter().find(|o| o.id.strip_prefix("play:") == Some(monitor))
            } else {
                source.owner.and_then(|owner| inv.outputs.iter().find(|o| o.cable == Some(owner)))
            };
            if let Some(dep) = dependency {
                dependencies.push(dep.id.as_str());
            }
            Ok(())
        };
        let mut stages: Vec<&Stage> = mix.effects.iter().collect();
        for send in &mix.sends {
            check(&send.source)?;
            stages.extend(&send.effects);
            if let Some(shared) = settings.source_effects.get(&send.source) {
                stages.extend(&shared[..send.depth(shared.len())]);
            }
        }
        for stage in stages.into_iter().filter(|s| s.enabled && matches!(s.kind, Kind::Duck | Kind::Aec)) {
            check(stage.key.as_deref().ok_or("Choose a reference source for the effect.")?)?;
        }
    }
    fn visit<'a>(
        id: &'a str, edges: &BTreeMap<&'a str, Vec<&'a str>>, visited: &mut BTreeMap<&'a str, bool>, sorted: &mut Vec<&'a str>,
    ) -> bool {
        let Some(dependencies) = edges.get(id) else { return true };
        if let Some(&finished) = visited.get(id) {
            return finished;
        }
        visited.insert(id, false);
        if dependencies.iter().any(|dependency| !visit(dependency, edges, visited, sorted)) {
            return false;
        }
        visited.insert(id, true);
        sorted.push(id);
        true
    }
    let mut visited = BTreeMap::new();
    let mut sorted = Vec::new();
    if !edges.keys().all(|id| visit(id, &edges, &mut visited, &mut sorted)) {
        return Err("These sends form a feedback loop. Disconnect one send or change its source.".into());
    }
    Ok(sorted.into_iter().filter_map(|id| inv.outputs.iter().find(|o| o.id == id)).collect())
}

fn build_effects(
    stages: &[Stage], offset: usize, input: &mut impl FnMut(&str) -> Result<Tap, String>, label: &str,
    meters: &mut Vec<(String, StageMeter)>,
) -> Result<Vec<Effect>, String> {
    stages
        .iter()
        .enumerate()
        .filter(|(_, s)| s.enabled)
        .map(|(index, stage)| {
            let effect = match stage.kind {
                Kind::Gain if (-60..=12).contains(&stage.db) => Effect::Gain(10_f32.powf(f32::from(stage.db) / 20.0)),
                Kind::Equalizer => {
                    let [low_db, mid_db, high_db] = stage.eq_db.map(f32::from);
                    llcable_effects::equalizer(llcable_effects::EqSettings { low_db, mid_db, high_db })?
                }
                Kind::Rnnoise => llcable_effects::noise_reduction(),
                Kind::Sonora => llcable_effects::sonora_noise_reduction()?,
                Kind::Aec => llcable_effects::echo_cancellation(input(
                    stage.key.as_deref().ok_or("Choose the captured playback reference for echo cancellation.")?,
                )?)?,
                Kind::Clap => {
                    let p = stage.clap.as_ref().ok_or("Choose a CLAP file in the effect rack.")?;
                    let params: Vec<_> = p.params.iter().map(|(&id, &v)| (id, v)).collect();
                    llcable_effects::clap_voice(&p.path, Some(&p.info.id), &params)?
                }
                Kind::Gate => llcable_effects::gate(f32::from(stage.db))?,
                Kind::Duck if (0..=60).contains(&stage.db) => Effect::Duck {
                    key: input(stage.key.as_deref().ok_or("Choose a source for the ducking sidechain.")?)?,
                    settings: Ducking { attenuation_db: f32::from(stage.db), ..Ducking::default() },
                },
                _ => return Err("The saved stage value is outside its range.".into()),
            };
            let (effect, meter) = effect.observe();
            meters.push((format!("{label} · {}. {}", offset + index + 1, stage.title()), meter));
            Ok(effect)
        })
        .collect()
}

pub fn plan(settings: &Settings, inventory: &Inventory) -> Result<Plan, String> {
    let mut inputs = Vec::new();
    let mut cables = Vec::new();
    let mut buses = Vec::new();
    let mut output_buses = Vec::new();
    let mut shared_buses: BTreeMap<&str, BTreeMap<usize, Tap>> = BTreeMap::new();
    let mut boundaries: BTreeMap<&str, BTreeSet<usize>> = BTreeMap::new();
    for send in settings.mixes.values().flat_map(|m| &m.sends) {
        let depth = send.depth(settings.source_effects.get(&send.source).map_or(0, Vec::len));
        if depth > 0 {
            boundaries.entry(&send.source).or_default().insert(depth);
        }
    }
    let mut meters = Vec::new();
    let mut published = BTreeMap::new();
    let mut playback = Vec::new();
    for out in ordered_outputs(settings, inventory)? {
        let mix = &settings.mixes[&out.id];
        let mut input = |key: &str| input_tap(&mut inputs, &published, key, inventory);
        let mut sends = Vec::new();
        for send in &mix.sends {
            if !(-60..=12).contains(&send.gain_db) || !(-100..=100).contains(&send.balance) {
                return Err("The saved send gain or balance is outside its range.".into());
            }
            let mut from = input(&send.source)?;
            if let Some(stages) = settings.source_effects.get(&send.source)
                && let Some(cuts) = boundaries.get(send.source.as_str())
            {
                let taps = shared_buses.entry(&send.source).or_default();
                let mut previous = 0;
                for &end in cuts.range(..=send.depth(stages.len())) {
                    if let Some(&tap) = taps.get(&end) {
                        from = tap;
                    } else {
                        let effects =
                            build_effects(&stages[previous..end], previous, &mut input, &inventory.label(&send.source), &mut meters)?;
                        if !effects.is_empty() {
                            let bus = buses.len();
                            buses.push(Bus { sends: vec![AudioSend::unity(from)], effects });
                            from = Tap::Bus(bus);
                        }
                        taps.insert(end, from);
                    }
                    previous = end;
                }
            }
            if send.effects.iter().any(|stage| stage.enabled) {
                let effects =
                    build_effects(&send.effects, 0, &mut input, &format!("{} → {}", inventory.label(&send.source), out.name), &mut meters)?;
                let bus = buses.len();
                buses.push(Bus { sends: vec![AudioSend::unity(from)], effects });
                from = Tap::Bus(bus);
            }
            sends.push(AudioSend { from, gain: 10_f32.powf(f32::from(send.gain_db) / 20.0), pan: f32::from(send.balance) / 100.0 });
        }
        let mut effects = build_effects(&mix.effects, 0, &mut input, &out.name, &mut meters)?;
        if mix.muted {
            effects.push(Effect::Gain(0.0));
        }
        if let Some(cable) = out.cable {
            published.insert(cable, buses.len());
            cables.push(cable);
            output_buses.push(buses.len());
        } else if let Some(device) = out.id.strip_prefix("play:") {
            playback.push((device.to_owned(), buses.len()));
        }
        buses.push(Bus { sends, effects });
    }
    if cables.is_empty() && playback.is_empty() {
        return Err("Select an output and add a source before starting audio.".into());
    }
    let channels = Channels {
        inputs: inputs.clone(),
        meters,
        outputs: cables.iter().map(ToString::to_string).chain(playback.iter().map(|(id, _)| format!("play:{id}"))).collect(),
    };
    Ok(Plan { config: RouteConfig { inputs, cables, buses, output_buses, playback }, channels })
}

pub fn path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("APPDATA")?).join("noise-filter").join("mixer-v2.json"))
}

pub fn load(path: &Path) -> Result<Settings, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("Couldn't read mixer settings from {}: {e}", path.display())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Settings::default()),
        Err(e) => Err(format!("Couldn't open {}: {e}", path.display())),
    }
}

pub fn save(path: &Path, settings: &Settings) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Err(io::Error::new(ErrorKind::InvalidInput, "missing settings directory"));
    };
    fs::create_dir_all(parent)?;
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let (file, mut fd) = loop {
        let file = path.with_extension(format!("{}.{}.tmp", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
        match fs::File::create_new(&file) {
            Ok(fd) => break (file, fd),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };
    let result = (|| {
        serde_json::to_writer_pretty(&mut fd, settings)?;
        fd.flush()?;
        drop(fd);
        fs::rename(&file, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(file);
    }
    result
}

#[cfg(test)]
mod tests {
    use llcable_control::routing::Router;

    use super::*;

    fn inventory() -> Inventory {
        let cables: Vec<_> =
            (1..=3).map(|n| CableInfo { id: CableId(n), label: n.to_string(), health: "Started".into(), ports: vec![] }).collect();
        let mut outputs: Vec<_> =
            cables.iter().map(|c| Destination { id: c.id.to_string(), name: c.label.clone(), cable: Some(c.id) }).collect();
        outputs.push(Destination { id: "play:speaker".into(), name: "Speakers".into(), cable: None });
        let sources = [("mic", None), ("other", None), ("loopback:speaker", None), ("out1", Some(CableId(1))), ("out2", Some(CableId(2)))]
            .into_iter()
            .map(|(id, owner)| Source { id: id.into(), label: id.into(), owner, channels: 2 })
            .collect();
        Inventory { cables, outputs, sources }
    }
    fn gain(db: i16) -> Stage {
        Stage { db, ..Stage::new(Kind::Gain) }
    }
    #[test]
    fn dry_intermediate_and_end_share_only_requested_prefixes_and_follow_the_tapped_stage_through_edits() {
        let inv = inventory();
        let [end, mid, dry] = [1, 2, 3].map(|n| CableId(n).to_string());
        let mut s = Settings::default();
        for id in [&end, &mid, &dry] {
            s.toggle(id, "mic");
        }
        s.source_effects.insert("mic".into(), vec![gain(3), gain(-6), gain(-12)]);
        s.mixes.get_mut(&mid).unwrap().sends[0].boundary = Some(1);
        s.mixes.get_mut(&dry).unwrap().sends[0].after_effects = false;
        let verify = |s: &Settings, expected: [f32; 3], count: usize| {
            let plan = plan(s, &inv).unwrap();
            assert_eq!(plan.config.inputs, ["mic"]);
            assert_eq!(plan.channels.meters.len(), count, "each source stage runs once across outputs");
            let mut r = Router::new(48000, 128, 1, plan.config.buses).unwrap();
            r.process_stereo(&[&[[0.1, -0.2]; 5]]).unwrap();
            for (&bus, gain_db) in plan.config.output_buses.iter().zip(expected) {
                let frame = r.output(bus).unwrap()[0];
                let g = 10f32.powf(gain_db / 20.0);
                assert!((frame[0] - 0.1 * g).abs() < 1e-5 && (frame[1] + 0.2 * g).abs() < 1e-5);
            }
        };
        verify(&s, [-15., 3., 0.], 3);
        s.remap_taps("mic", 0, Some(1));
        s.source_effects.get_mut("mic").unwrap().swap(0, 1);
        verify(&s, [-15., -3., 0.], 3);
        s.remap_taps("mic", 1, None);
        s.source_effects.get_mut("mic").unwrap().remove(1);
        verify(&s, [-18., -6., 0.], 2);
        s.remap_taps("mic", 0, None);
        s.source_effects.get_mut("mic").unwrap().remove(0);
        verify(&s, [-12., 0., 0.], 1);
        s.toggle(&end, "mic");
        let p = plan(&s, &inv).unwrap();
        assert!(p.channels.meters.is_empty(), "unused downstream stages stay inactive");
    }
    #[test]
    fn splitting_merging_and_inline_cable_chaining_preserve_original_stereo() {
        let inv = inventory();
        let [a, b, c] = [3, 2, 1].map(|n| CableId(n).to_string());
        let mut s = Settings::default();
        s.toggle(&a, "mic");
        s.mixes.get_mut(&a).unwrap().sends[0].after_effects = false;
        s.toggle(&b, "mic");
        s.toggle(&c, "out2");
        s.toggle(&c, "other");
        s.source_effects.insert("mic".into(), vec![gain(-6)]);
        let bsend = &mut s.mixes.get_mut(&b).unwrap().sends[0];
        bsend.after_effects = true;
        bsend.effects.push(gain(-6));
        let p = plan(&s, &inv).unwrap();
        assert!(!p.config.inputs.contains(&"out2".to_owned()));
        assert_eq!(p.config.inputs.len(), 2);
        let raw = [0.6, -0.2];
        let secondary = [0.03, 0.07];
        let packets: Vec<_> = p.config.inputs.iter().map(|id| [if id == "mic" { raw } else { secondary }; 17]).collect();
        let mut r = Router::new(48000, 128, packets.len(), p.config.buses).unwrap();
        r.process_stereo(&packets.iter().map(|p| p.as_slice()).collect::<Vec<_>>()).unwrap();
        let frames = |dest: &str| r.output(p.config.output_buses[p.channels.outputs.iter().position(|o| o == dest).unwrap()]).unwrap()[0];
        assert_eq!(frames(&a), raw);
        for i in 0..2 {
            assert!((frames(&b)[i] - raw[i] * 10_f32.powf(-12.0 / 20.0)).abs() < 1e-6);
            assert!((frames(&c)[i] - frames(&b)[i] - secondary[i]).abs() < 1e-6);
        }
    }
    #[test]
    fn shared_stages_only_run_once_and_key_source_is_captured_once_without_an_audible_send() {
        let inv = inventory();
        let mut s = Settings::default();
        let mut duck = Stage::new(Kind::Duck);
        duck.key = Some("other".into());
        s.source_effects.insert("mic".into(), vec![duck]);
        for n in [1, 2] {
            let id = CableId(n).to_string();
            s.toggle(&id, "mic");
            s.mixes.get_mut(&id).unwrap().sends[0].after_effects = true;
        }
        let p = plan(&s, &inv).unwrap();
        assert_eq!(p.channels.inputs, ["mic", "other"]);
        assert_eq!(p.channels.meters.len(), 1);
        assert_eq!(p.config.buses.len(), 3);
        for &b in &p.config.output_buses {
            assert_eq!(p.config.buses[b].sends.len(), 1);
            assert_eq!(p.config.buses[b].sends[0].from, Tap::Bus(0));
        }
    }
    #[test]
    fn direct_and_indirect_feedback_and_missing_source_are_refused_before_any_plugin_is_loaded() {
        let inv = inventory();
        let a = CableId(1).to_string();
        let b = CableId(2).to_string();
        let mut s = Settings::default();
        s.toggle(&a, "out1");
        assert!(plan(&s, &inv).err().unwrap().contains("feed back"));
        s.toggle(&a, "out1");
        s.toggle(&a, "out2");
        s.toggle(&b, "mic");
        let mut side = Stage::new(Kind::Duck);
        side.key = Some("out1".into());
        s.mixes.get_mut(&b).unwrap().effects.extend([Stage::new(Kind::Clap), side]);
        assert!(plan(&s, &inv).err().unwrap().contains("feedback loop"));
        s.toggle(&a, "out2");
        s.toggle(&a, "missing");
        assert!(plan(&s, &inv).is_err());
    }
    #[test]
    fn speakers_can_subscribe_to_an_inline_cable_but_cannot_feed_their_playback_monitor_back() {
        let inv = inventory();
        let a = CableId(1).to_string();
        let mut s = Settings::default();
        s.toggle(&a, "mic");
        s.toggle("play:speaker", "out1");
        let p = plan(&s, &inv).unwrap();
        assert_eq!(p.channels.inputs, ["mic"]);
        assert_eq!(p.channels.outputs, [a.as_str(), "play:speaker"]);
        assert_eq!(p.config.playback, [("speaker".into(), 1)]);
        s.toggle(&a, "mic");
        s.toggle(&a, "loopback:speaker");
        assert!(plan(&s, &inv).err().unwrap().contains("feedback loop"));
        s.toggle(&a, "loopback:speaker");
        s.toggle("play:speaker", "out1");
        s.toggle("play:speaker", "loopback:speaker");
        assert!(plan(&s, &inv).err().unwrap().contains("feed back"));
    }
    #[test]
    fn saved_route_round_trips_and_retains_missing_sources_until_reconnected() {
        let mut a = Settings::default();
        a.toggle("play:speaker", "mic");
        a.source_effects.insert("mic".into(), vec![gain(3)]);
        let file = std::env::temp_dir().join(format!("llcable-mixer-patch-{}.json", std::process::id()));
        save(&file, &a).unwrap();
        a.roomy = true;
        save(&file, &a).unwrap();
        assert_eq!(load(&file).unwrap(), a);
        std::fs::remove_file(file).unwrap();
    }
}
