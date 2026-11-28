//! The session document contains stable endpoint selectors and cable identities. Device-list indexes are never saved.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, ErrorKind, Write};
use std::ops::Range;
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

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum RackId {
    Source(String, usize),       // Shared rack starting at this stage boundary.
    Send(String, String, usize), // Destination, source and occurrence among sends from that source.
    Output(String),
}

#[derive(Clone, PartialEq)]
struct RackEffects {
    stages: Vec<(Stage, Option<Tap>)>,
    muted: bool,
}

impl RackEffects {
    fn reusable_as(&self, next: &Self, retained: &[(usize, usize)], may_shorten: bool) -> bool {
        self.muted == next.muted
            && (self.stages.len() == next.stages.len() || may_shorten && self.stages.len() > next.stages.len())
            && self.stages.iter().zip(&next.stages).all(|((old, old_key), (new, new_key))| {
                let same_key = match (*old_key, *new_key) {
                    (Some(Tap::Bus(i)), Some(Tap::Bus(j))) => {
                        retained.iter().find(|&&(from, to)| from == i || to == j).map_or(i == j, |&pair| pair == (i, j))
                    }
                    _ => old_key == new_key,
                };
                old == new && same_key
            })
    }
}

#[derive(Clone)]
struct ActiveRack {
    bus: usize,
    effects: RackEffects,
    meters: Range<usize>,
    // Logical stages in the compiled processor, including a previously used tail that was deactivated live.
    width: usize,
}

#[derive(Clone, Default)]
pub struct Channels {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub meters: Vec<(String, StageMeter)>,
    racks: BTreeMap<RackId, ActiveRack>,
    source_cuts: BTreeMap<String, BTreeSet<usize>>,
}

pub struct Plan {
    pub config: RouteConfig,
    pub channels: Channels,
    /// Move the compiled effects from the old bus to the new bus. The new bus always supplies its own sends.
    pub retained: Vec<(usize, usize)>,
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

fn build_effect(stage: &Stage, key: Option<Tap>) -> Result<Effect, String> {
    match stage.kind {
        Kind::Gain if (-60..=12).contains(&stage.db) => Ok(Effect::Gain(10_f32.powf(f32::from(stage.db) / 20.0))),
        Kind::Equalizer => {
            let [low_db, mid_db, high_db] = stage.eq_db.map(f32::from);
            llcable_effects::equalizer(llcable_effects::EqSettings { low_db, mid_db, high_db })
        }
        Kind::Rnnoise => Ok(llcable_effects::noise_reduction()),
        Kind::Sonora => llcable_effects::sonora_noise_reduction(),
        Kind::Aec => llcable_effects::echo_cancellation(key.ok_or("Choose the captured playback reference for echo cancellation.")?),
        Kind::Clap => {
            let p = stage.clap.as_ref().ok_or("Choose a CLAP file in the effect rack.")?;
            let params: Vec<_> = p.params.iter().map(|(&id, &v)| (id, v)).collect();
            llcable_effects::clap_voice(&p.path, Some(&p.info.id), &params)
        }
        Kind::Gate => llcable_effects::gate(f32::from(stage.db)),
        Kind::Duck if (0..=60).contains(&stage.db) => Ok(Effect::Duck {
            key: key.ok_or("Choose a source for the ducking sidechain.")?,
            settings: Ducking { attenuation_db: f32::from(stage.db), ..Ducking::default() },
        }),
        _ => Err("The saved stage value is outside its range.".into()),
    }
}

struct PlannedBus {
    id: RackId,
    sends: Vec<AudioSend>,
    effects: RackEffects,
    labels: Vec<String>,
}

impl PlannedBus {
    fn new(
        id: RackId, sends: Vec<AudioSend>, stages: &[Stage], label: &str, offset: usize, muted: bool,
        input: &mut impl FnMut(&str) -> Result<Tap, String>,
    ) -> Result<Self, String> {
        let mut labels = Vec::new();
        let mut active = Vec::new();
        for (index, stage) in stages.iter().enumerate().filter(|(_, stage)| stage.enabled) {
            let key = if matches!(stage.kind, Kind::Duck | Kind::Aec) {
                Some(input(stage.key.as_deref().ok_or("Choose a reference source for the effect.")?)?)
            } else {
                None
            };
            active.push((stage.clone(), key));
            labels.push(format!("{label} · {}. {}", offset + index + 1, stage.title()));
        }
        Ok(Self { id, sends, effects: RackEffects { stages: active, muted }, labels })
    }

    fn compile(
        self, bus: usize, previous: Option<&Channels>, channels: &mut Channels, retained: &mut Vec<(usize, usize)>,
    ) -> Result<Bus, String> {
        let start = channels.meters.len();
        let mut effects = Vec::new();
        let active = self.effects.stages.len();
        let mut width = active;
        if let Some(old) = previous
            && let Some(rack) = old.racks.get(&self.id)
            && rack.effects.reusable_as(&self.effects, retained, matches!(self.id, RackId::Source(..)))
        {
            retained.push((rack.bus, bus));
            width = rack.width;
            // The live router owns the processors. Keep their logical stage indexes and active sidechains in this sparse plan.
            for (_, key) in &self.effects.stages {
                effects.push(match *key {
                    Some(key) => Effect::Duck { key, settings: Ducking { attenuation_db: 0.0, ..Ducking::default() } },
                    None => Effect::Gain(1.0),
                });
            }
            if self.effects.muted {
                effects.push(Effect::Gain(1.0));
            }
            if active < width {
                effects.resize_with(width, || Effect::Gain(1.0));
                effects.push(Effect::ActivePrefix(active));
            }
            channels.meters.extend(self.labels.into_iter().zip(old.meters[rack.meters.clone()].iter().map(|(_, meter)| meter.clone())));
        } else {
            for ((stage, key), label) in self.effects.stages.iter().zip(self.labels) {
                let (effect, meter) = build_effect(stage, *key)?.observe();
                effects.push(effect);
                channels.meters.push((label, meter));
            }
            if self.effects.muted {
                effects.push(Effect::Gain(0.0));
            }
        }
        channels.racks.insert(self.id, ActiveRack { bus, effects: self.effects, meters: start..channels.meters.len(), width });
        Ok(Bus { sends: self.sends, effects })
    }
}

pub fn plan(settings: &Settings, inventory: &Inventory) -> Result<Plan, String> {
    prepare(settings, inventory, None)
}

/// With the same capture sources, keep the clock, surviving destination order and unchanged processors through destination edits.
/// A different capture set produces a complete plan for a new session.
pub fn plan_reusing(settings: &Settings, inventory: &Inventory, previous: &Channels) -> Result<Plan, String> {
    prepare(settings, inventory, Some(previous))
}

fn source_segments(settings: &Settings, inventory: &Inventory, previous: Option<&Channels>) -> BTreeMap<String, BTreeSet<usize>> {
    let mut depths = BTreeMap::<&str, usize>::new();
    for send in settings.mixes.values().flat_map(|m| &m.sends) {
        let depth = send.depth(settings.source_effects.get(&send.source).map_or(0, Vec::len));
        if depth > 0 {
            let max = depths.entry(&send.source).or_default();
            *max = (*max).max(depth);
        }
    }
    let active_outputs: BTreeSet<_> = inventory
        .outputs
        .iter()
        .filter(|o| settings.mixes.get(&o.id).is_some_and(|m| !m.sends.is_empty()))
        .filter_map(|o| o.cable)
        .collect();
    depths
        .into_iter()
        .map(|(id, depth)| {
            // A source that is already running can grow a new tail without replacing its warmed prefix. Subscriber taps
            // inside an existing segment use its intermediate audio; they do not introduce new processing boundaries.
            let mut cuts: BTreeSet<_> =
                previous.and_then(|p| p.source_cuts.get(id)).into_iter().flat_map(|c| c.range(..depth).copied()).collect();
            cuts.insert(depth);
            for (i, stage) in settings.source_effects[id][..depth].iter().enumerate().skip(1) {
                if stage.enabled
                    && matches!(stage.kind, Kind::Aec | Kind::Duck)
                    && let Some(key) = &stage.key
                    && !key.starts_with("loopback:")
                    && inventory.sources.iter().find(|s| s.id == *key).and_then(|s| s.owner).is_some_and(|c| active_outputs.contains(&c))
                {
                    // An earlier tap can even feed the cable that supplies this key. Publish it before this stage.
                    cuts.insert(i);
                }
            }
            (id.to_owned(), cuts)
        })
        .collect()
}

fn prepare(settings: &Settings, inventory: &Inventory, previous: Option<&Channels>) -> Result<Plan, String> {
    let mut inputs = Vec::new();
    let mut buses = Vec::new();
    let mut outputs = Vec::new();
    let mut shared_buses: BTreeMap<&str, BTreeMap<usize, usize>> = BTreeMap::new();
    let boundaries = source_segments(settings, inventory, previous);
    let mut published = BTreeMap::new();
    for out in ordered_outputs(settings, inventory)? {
        let mix = &settings.mixes[&out.id];
        let mut input = |key: &str| input_tap(&mut inputs, &published, key, inventory);
        let mut sends = Vec::new();
        for (send_index, send) in mix.sends.iter().enumerate() {
            if !(-60..=12).contains(&send.gain_db) || !(-100..=100).contains(&send.balance) {
                return Err("The saved send gain or balance is outside its range.".into());
            }
            let mut from = input(&send.source)?;
            if let Some(stages) = settings.source_effects.get(&send.source)
                && let Some(cuts) = boundaries.get(send.source.as_str())
            {
                let taps = shared_buses.entry(&send.source).or_default();
                let mut start = 0;
                let depth = send.depth(stages.len());
                for &end in cuts {
                    if start >= depth {
                        break;
                    }
                    let segment = &stages[start..end];
                    let count = segment.iter().filter(|s| s.enabled).count();
                    let take = stages[start..end.min(depth)].iter().filter(|s| s.enabled).count();
                    if take == 0 {
                        start = end;
                        continue;
                    }
                    let bus = if let Some(&bus) = taps.get(&end) {
                        bus
                    } else {
                        let bus = buses.len();
                        buses.push(PlannedBus::new(
                            RackId::Source(send.source.clone(), start),
                            vec![AudioSend::unity(from)],
                            segment,
                            &inventory.label(&send.source),
                            start,
                            false,
                            &mut input,
                        )?);
                        taps.insert(end, bus);
                        bus
                    };
                    from = if take == count { Tap::Bus(bus) } else { Tap::Stage(bus, take - 1) };
                    start = end;
                }
            }
            if send.effects.iter().any(|stage| stage.enabled) {
                let occurrence = mix.sends[..send_index].iter().filter(|s| s.source == send.source).count();
                let bus = buses.len();
                buses.push(PlannedBus::new(
                    RackId::Send(out.id.clone(), send.source.clone(), occurrence),
                    vec![AudioSend::unity(from)],
                    &send.effects,
                    &format!("{} → {}", inventory.label(&send.source), out.name),
                    0,
                    false,
                    &mut input,
                )?);
                from = Tap::Bus(bus);
            }
            sends.push(AudioSend { from, gain: 10_f32.powf(f32::from(send.gain_db) / 20.0), pan: f32::from(send.balance) / 100.0 });
        }
        let index = buses.len();
        buses.push(PlannedBus::new(RackId::Output(out.id.clone()), sends, &mix.effects, &out.name, 0, mix.muted, &mut input)?);
        if let Some(cable) = out.cable {
            published.insert(cable, index);
        }
        outputs.push((out, index));
    }
    if outputs.is_empty() {
        return Err("Select an output and add a source before starting audio.".into());
    }
    if let Some(old) = previous {
        if old.inputs.len() != inputs.len() || !inputs.iter().all(|id| old.inputs.contains(id)) {
            return prepare(settings, inventory, None);
        }
        let indices: Vec<_> = inputs.iter().map(|id| old.inputs.iter().position(|key| key == id).unwrap()).collect();
        for b in &mut buses {
            for from in b.sends.iter_mut().map(|s| &mut s.from).chain(b.effects.stages.iter_mut().filter_map(|(_, key)| key.as_mut())) {
                if let Tap::Input(i) = from {
                    *i = indices[*i];
                }
            }
        }
        inputs.clone_from(&old.inputs);
        outputs.sort_by_cached_key(|(out, _)| old.outputs.iter().position(|id| id == &out.id).unwrap_or(usize::MAX));
    }
    let mut cables = Vec::new();
    let mut output_buses = Vec::new();
    let mut playback = Vec::new();
    for (out, bus) in outputs {
        if let Some(cable) = out.cable {
            cables.push(cable);
            output_buses.push(bus);
        } else {
            let device = out.id.strip_prefix("play:").ok_or("A playback destination is missing its device selector.")?;
            playback.push((device.to_owned(), bus));
        }
    }
    let mut channels = Channels {
        inputs: inputs.clone(),
        outputs: cables.iter().map(ToString::to_string).chain(playback.iter().map(|(id, _)| format!("play:{id}"))).collect(),
        source_cuts: boundaries,
        ..Channels::default()
    };
    let mut retained = Vec::new();
    let buses =
        buses.into_iter().enumerate().map(|(i, b)| b.compile(i, previous, &mut channels, &mut retained)).collect::<Result<_, _>>()?;
    Ok(Plan { config: RouteConfig { inputs, cables, buses, output_buses, playback }, channels, retained })
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
    fn live_apply_keeps_the_shared_rack_and_the_other_outputs_processors_and_meter_handles() {
        let inv = inventory();
        let [edited, untouched] = [1, 2].map(|n| CableId(n).to_string());
        let mut s = Settings::default();
        s.toggle(&edited, "mic");
        s.toggle(&untouched, "mic");
        s.source_effects.insert("mic".into(), vec![gain(-3)]);
        s.mixes.get_mut(&edited).unwrap().effects.push(gain(-6));
        let mix = s.mixes.get_mut(&untouched).unwrap();
        mix.sends[0].effects.push(gain(-9));
        mix.effects.push(Stage { key: Some("other".into()), ..Stage::new(Kind::Duck) });
        let old = plan(&s, &inv).unwrap();
        let mut running = Router::new(48000, 128, 2, old.config.buses).unwrap();
        let mix = s.mixes.get_mut(&edited).unwrap();
        mix.sends[0].effects.push(gain(6));
        mix.effects[0].db = -12;
        let next = plan_reusing(&s, &inv, &old.channels).unwrap();
        assert_eq!(next.retained, [(0, 0), (2, 3), (3, 4)]);
        assert_eq!(next.config.buses[4].sends[0].from, Tap::Bus(3));
        assert_eq!(next.config.buses[4].effects.iter().filter_map(Effect::key).collect::<Vec<_>>(), [Tap::Input(1)]);
        // An update arriving while the old route is running reaches the new UI handles only for the retained racks.
        running.process_stereo(&[&[[0.5, -0.25]; 5], &[[0.; 2]; 5]]).unwrap();
        for (old_index, new_index) in [(0, 0), (2, 3), (3, 4)] {
            let active = old.channels.meters[old_index].1.read();
            assert!(active.input_peak.is_some());
            assert_eq!(active, next.channels.meters[new_index].1.read());
        }
        assert!(next.channels.meters[1..3].iter().all(|(_, meter)| meter.read().input_peak.is_none()));
    }

    #[test]
    fn a_live_plan_keeps_the_clock_and_tracks_the_retained_sidechain_producer() {
        let inv = inventory();
        let [a, b] = [1, 2].map(|n| CableId(n).to_string());
        let mut s = Settings::default();
        s.toggle(&a, "mic");
        s.toggle(&b, "other");
        s.mixes.get_mut(&b).unwrap().effects.push(Stage { key: Some("out1".into()), ..Stage::new(Kind::Duck) });
        let old = plan(&s, &inv).unwrap();
        assert_eq!(old.channels.inputs, ["mic", "other"]);
        assert_eq!(old.config.buses[1].effects[0].key(), Some(Tap::Bus(0)));
        s.toggle(&a, "other");
        let mix = s.mixes.get_mut(&a).unwrap();
        mix.sends.swap(0, 1);
        mix.sends[0].effects.push(gain(6));
        let next = plan_reusing(&s, &inv, &old.channels).unwrap();
        assert_eq!(next.channels.inputs, old.channels.inputs);
        assert_eq!(next.config.buses[0].sends[0].from, Tap::Input(1));
        assert_eq!(next.config.buses[1].sends[1].from, Tap::Input(0));
        assert_eq!(next.config.buses[2].effects[0].key(), Some(Tap::Bus(1)));
        assert!(next.retained.contains(&(1, 2)));

        s.toggle(&a, "other");
        s.toggle(&b, "other");
        let restart = plan_reusing(&s, &inv, &next.channels).unwrap();
        assert!(restart.retained.is_empty());
        assert_eq!(restart.channels.inputs, ["mic"]);
        assert_eq!(restart.channels.outputs, [a]);
    }

    #[test]
    fn opening_another_capture_rebuilds_the_compact_source_rack() {
        let inv = inventory();
        let [a, b] = [1, 2].map(|n| CableId(n).to_string());
        let mut s = Settings::default();
        s.toggle(&a, "mic");
        s.mixes.get_mut(&a).unwrap().sends[0].boundary = Some(1);
        s.source_effects.insert("mic".into(), vec![gain(-3), gain(-6)]);
        let short = plan(&s, &inv).unwrap();
        s.toggle(&b, "mic");
        let grown = plan_reusing(&s, &inv, &short.channels).unwrap();
        assert_eq!(grown.channels.source_cuts["mic"], BTreeSet::from([1, 2]));
        s.toggle(&b, "other");
        let reopened = plan_reusing(&s, &inv, &grown.channels).unwrap();
        assert_eq!(reopened.channels.source_cuts["mic"], BTreeSet::from([2]));
        assert!(reopened.retained.is_empty());
        assert_eq!(reopened.config.buses[0].effects.len(), 2);
        assert_eq!(reopened.config.buses[1].sends[0].from, Tap::Stage(0, 0));
    }

    #[test]
    fn adding_and_removing_cable_and_playback_outputs_preserves_existing_duck_history_and_meter() {
        let inv = inventory();
        let [a, b, c] = [1, 2, 3].map(|n| CableId(n).to_string());
        let mut s = Settings::default();
        s.toggle(&b, "mic");
        s.toggle(&c, "mic");
        s.mixes.get_mut(&c).unwrap().effects.push(Stage { key: Some("out2".into()), ..Stage::new(Kind::Duck) });
        let old = plan(&s, &inv).unwrap();
        let mut running = Router::new(48000, 128, 1, old.config.buses).unwrap();
        let mut reference = Router::new(48000, 128, 1, plan(&s, &inv).unwrap().config.buses).unwrap();
        let input = [&[[0.75, -0.5]; 128][..]];
        running.process_stereo(&input).unwrap();
        reference.process_stereo(&input).unwrap();
        let old_meter = &old.channels.meters[0].1;
        let before = old_meter.read();

        s.toggle(&a, "mic");
        s.mixes.get_mut(&a).unwrap().muted = true;
        s.toggle("play:speaker", "mic");
        let added = plan_reusing(&s, &inv, &old.channels).unwrap();
        assert_eq!(added.channels.outputs, [b.as_str(), c.as_str(), a.as_str(), "play:speaker"]);
        assert_eq!(added.config.output_buses, [1, 2, 0]);
        assert_eq!(added.config.playback, [("speaker".into(), 3)]);
        assert_eq!(added.retained, [(0, 1), (1, 2)]);
        let mut replacement = Router::new(48000, 128, 1, added.config.buses).unwrap();
        running.activate_edit(&mut replacement, &added.retained).unwrap();
        running.process_stereo(&input).unwrap();
        reference.process_stereo(&input).unwrap();
        assert_eq!(running.output(0).unwrap(), [[0.; 2]; 128]);
        assert_eq!(running.output(2), reference.output(1));
        assert_ne!(old_meter.read(), before, "the old handle still receives the running duck's envelope");
        assert_eq!(old_meter.read(), added.channels.meters[0].1.read());

        s.toggle(&a, "mic");
        s.toggle("play:speaker", "mic");
        let removed = plan_reusing(&s, &inv, &added.channels).unwrap();
        assert_eq!(removed.channels.outputs, [b, c]);
        assert_eq!(removed.retained, [(1, 0), (2, 1)]);
        let mut replacement = Router::new(48000, 128, 1, removed.config.buses).unwrap();
        running.activate_edit(&mut replacement, &removed.retained).unwrap();
        running.process_stereo(&input).unwrap();
        reference.process_stereo(&input).unwrap();
        assert_eq!(running.output(1), reference.output(1));
        assert_eq!(old_meter.read(), removed.channels.meters[0].1.read());
    }

    #[test]
    fn a_new_subscriber_between_aec_and_rnnoise_preserves_both_running_models_and_hears_the_clean_aec_output() {
        let inv = inventory();
        let [a, b] = [1, 2].map(|n| CableId(n).to_string());
        let mut settings = Settings::default();
        settings.toggle(&a, "mic");
        let aec = Stage { key: Some("other".into()), ..Stage::new(Kind::Aec) };
        settings.source_effects.insert("mic".into(), vec![aec.clone(), Stage::new(Kind::Rnnoise)]);
        let build = |s: &Settings| {
            let p = plan(s, &inv).unwrap();
            let mut router = Router::new(48_000, 191, 2, p.config.buses).unwrap();
            router.prepare_thread();
            (router, p.channels)
        };
        let (mut running, mut channels) = build(&settings);
        let meters = channels.meters.clone();
        let (mut full, _) = build(&settings);
        let mut short = settings.clone();
        short.source_effects.insert("mic".into(), vec![aec]);
        let (mut prefix, _) = build(&short);
        let mut frame = 0;
        let mut different = false;
        for packet in 0..130 {
            // Add at an irregular packet boundary within a 480-frame hop, remove, then retire just the unused RNNoise tail.
            if [63, 83, 94, 105, 118].contains(&packet) {
                assert_ne!(frame % 480, 0);
                if packet < 105 {
                    settings.toggle(&b, "mic");
                    if let Some(send) = settings.mixes.get_mut(&b).unwrap().sends.first_mut() {
                        send.boundary = Some(1);
                    }
                } else {
                    settings.toggle(&a, "mic");
                }
                let updated = plan_reusing(&settings, &inv, &channels).unwrap();
                assert_eq!(updated.config.inputs, ["mic", "other"]);
                let mut new = Router::new(48_000, 191, 2, updated.config.buses).unwrap();
                running.activate_edit(&mut new, &updated.retained).unwrap();
                channels = updated.channels;
            }
            let n = [83, 191, 79, 157][packet % 4];
            let reference: Vec<_> = (frame..frame + n).map(|i| [(i as f32 * 0.11).sin() * 0.1; 2]).collect();
            let mic: Vec<_> =
                (frame..frame + n).map(|i| [(i as f32 * 0.034).sin() * 0.21 + ((i as f32 - 17.) * 0.11).sin() * 0.06; 2]).collect();
            let inputs = [mic.as_slice(), reference.as_slice()];
            for r in [&mut running, &mut full, &mut prefix] {
                r.process_stereo(&inputs).unwrap();
            }
            let bus = |id: &str| channels.racks.get(&RackId::Output(id.into())).map(|r| r.bus);
            if let Some(i) = bus(&a)
                && packet < 105
            {
                assert!(running.output(i) == full.output(1), "sample mismatch on uninterrupted full chain at packet {packet}");
                for (old, active) in meters.iter().zip(&channels.meters) {
                    assert_eq!(old.1.read().output_peak, active.1.read().output_peak);
                }
            }
            if let Some(i) = bus(&b) {
                assert!(running.output(i) == prefix.output(1), "sample mismatch on AEC-only branch at packet {packet}");
                assert_eq!(meters[0].1.read().output_peak, channels.meters[0].1.read().output_peak);
                different |= running.output(i) != full.output(1);
            }
            if (105..118).contains(&packet) {
                assert_eq!(channels.meters.len(), 1, "only AEC is active");
            }
            frame += n;
        }
        assert!(different, "the intermediate output must be distinguishable from the final denoised output");
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
    fn a_shorter_source_tap_can_feed_the_key_of_a_later_shared_stage_without_feedback() {
        let inv = inventory();
        let [a, b] = [1, 2].map(|n| CableId(n).to_string());
        let mut s = Settings::default();
        s.toggle(&a, "mic");
        s.toggle(&b, "mic");
        s.mixes.get_mut(&a).unwrap().sends[0].boundary = Some(1);
        s.source_effects.insert("mic".into(), vec![gain(-6), Stage { key: Some("out1".into()), ..Stage::new(Kind::Duck) }]);
        let p = plan(&s, &inv).unwrap();
        assert_eq!(p.config.inputs, ["mic"]);
        assert_eq!(p.config.output_buses, [1, 3]);
        assert_eq!(p.config.buses[2].sends[0].from, Tap::Bus(0));
        assert_eq!(p.config.buses[2].effects[0].key(), Some(Tap::Bus(1)));
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
