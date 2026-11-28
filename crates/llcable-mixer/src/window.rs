mod monitor;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use llcable_audio::{RouteConfig, RouteStatus, Session};
use llcable_control::{CableId, CableInfo, CaptureEffects, EndpointKind, Port};
use monitor::{CableMonitor, Samples};
use nf_gui::mixer_meter::{MeterMode, MeterPalette};
use windows_reactor::*;

use crate::desk;
use crate::patch::{self, Channels, Inventory, Kind, Plugin, Preset, Settings, Stage, Theme};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Cables,
    Mixer,
    Wiring,
    Appearance,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Scope {
    Output(String),
    Source(String),
    Send(String, String),
}

pub enum StageEdit {
    Add(Kind),
    Remove(usize),
    Bypass(usize),
    Move(usize, i32),
    Value(usize, i16),
    Band(usize, usize, i16),
    Key(usize, String),
    Plugin(usize, Box<Plugin>),
    Param(usize, u32, f64),
}

pub enum Edit {
    Send(String, String),
    Mute(String),
    Level(String, String, i16),
    Balance(String, String, i16),
    Tap(String, String, Option<usize>),
    Stage(Scope, StageEdit),
    Forget(String),
}

pub enum Msg {
    Page(Page),
    Select(String),
    Scope(Scope),
    SendDetail(String),
    InspectCable(String),
    WireSource(Option<String>),
    WireDrag(Option<(f64, f64)>),
    Connect(String, String),
    WireRack(Scope, String),
    Edit(Edit),
    MeterMode(MeterMode),
    Palette(MeterPalette),
    Roomy(bool),
    Theme(Theme),
    Presets(bool),
    PresetName(String),
    SavePreset,
    LoadPreset,
    DeletePreset,
    Refresh,
    Monitor(bool),
    ResetCableCounters,
    CheckWindowsEffects(CableId),
    WindowsEffectsChecked(CableId, u64, EffectsCheck),
    Measure(CableId),
    CancelMeasurement,
    Measured(CableId, Result<String, String>),
    Discovered(Result<Inventory, String>),
    Start,
    Stop,
    CableEnabled(CableId, bool),
    Prepared(u64, Box<Result<patch::Plan, String>>),
    RouteChanged(u64, Arc<Mutex<RouteChange>>),
    Stopped,
    Authorize,
    Authorized(Result<(), String>),
    RouteTick,
    CableSamples(u64, Samples),
    OpenLegacy,
    ChoosePlugin(Scope, usize),
    Chosen(Scope, usize, Result<Option<Box<Plugin>>, String>),
    Width(f64),
}

pub enum EffectsCheck {
    Checking,
    Complete(Vec<(String, std::io::Result<CaptureEffects>)>),
    Unavailable,
}

pub fn recording_ports(cable: &CableInfo) -> impl Iterator<Item = &Port> {
    cable.ports.iter().filter(|p| p.active && p.role != EndpointKind::Speakers)
}

// Reactor can reject delivery after a background job has already run. Both completion and rejection take the same result,
// including the previous route if the executor rejected the job before it ran.
pub struct RouteChange<S = Arc<Session>> {
    hot: bool,
    previous: Option<S>,
    result: Option<Result<(S, Channels), String>>,
}

impl<S: Clone> RouteChange<S> {
    fn begin(current: &mut Option<S>, hot: bool) -> Self {
        Self { hot, previous: if hot { current.clone() } else { current.take() }, result: None }
    }

    fn run(
        &mut self, plan: patch::Plan, reconfigure: impl FnOnce(&S, RouteConfig, Vec<(usize, usize)>) -> Result<(), String>,
        start: impl FnOnce(RouteConfig) -> Result<S, String>,
    ) {
        let result = match self.previous.take() {
            Some(session) if self.hot => reconfigure(&session, plan.config, plan.retained).map(|()| session),
            previous => {
                drop(previous);
                start(plan.config)
            }
        };
        self.result = Some(result.map(|session| (session, plan.channels)));
    }

    fn finish(&mut self, current: &mut Option<S>, channels: &mut Channels) -> Result<(), String> {
        if let Some(old) = self.previous.take() {
            *current = Some(old);
        }
        let (session, next) = self.result.take().unwrap_or_else(|| Err("Couldn't schedule the audio update. Try again.".into()))?;
        *current = Some(session);
        *channels = next;
        Ok(())
    }
}

pub struct Mixer {
    pub page: Page,
    pub settings: Settings,
    path: Option<PathBuf>,
    pub inventory: Option<Arc<Inventory>>,
    pub monitor: CableMonitor,
    pub effects_checks: BTreeMap<CableId, EffectsCheck>,
    effects_generation: u64,
    pub status: Option<RouteStatus>,
    pub channels: Channels,
    session: Option<Arc<Session>>,
    generation: u64,
    saved_patch: Option<Settings>,
    pub scope: Option<Scope>,
    pub send_detail: Option<String>,
    pub wire_source: Option<String>,
    pub wire_drag: Option<(f64, f64)>,
    pub busy: bool,
    pub refreshing: bool,
    pub granting: bool,
    pub elevated: bool,
    pub choosing: bool,
    pub presets_open: bool,
    pub preset_name: String,
    pub notice: Option<Result<String, String>>,
    pub discovery_error: Option<String>,
    pub measuring: Option<(CableId, Arc<AtomicBool>)>,
    pub measured: Option<(CableId, Result<String, String>)>,
    pub width: f64,
    route_clock: Option<ComponentTask>,
}

impl Mixer {
    pub fn running(&self) -> bool {
        self.session.is_some()
    }

    pub fn cable_enabled(&self, id: CableId) -> Option<bool> {
        self.session.as_ref()?.cable_enabled(id)
    }

    pub fn changed(&self) -> bool {
        self.saved_patch.as_ref().is_some_and(|s| self.settings.mixes != s.mixes || self.settings.source_effects != s.source_effects)
    }

    fn persist(&mut self) {
        if let Some(path) = &self.path
            && let Err(e) = patch::save(path, &self.settings)
        {
            self.notice = Some(Err(format!("Could not save the route: {e}")));
        }
    }

    fn refresh(&mut self, cx: &ComponentContext<Self>) {
        if !self.refreshing {
            self.refreshing = true;
            // Even the same cable ID can have new Windows recording endpoints after a reinstall.
            self.effects_generation = self.effects_generation.wrapping_add(1);
            self.effects_checks.clear();
            cx.spawn_background_with_rejection(
                |_| Msg::Discovered(Inventory::read()),
                Msg::Discovered(Err("The device list is busy. Refresh to try again.".into())),
            );
        }
    }

    fn check_windows_effects(&mut self, id: CableId, cx: &ComponentContext<Self>) {
        if self.refreshing || matches!(self.effects_checks.get(&id), Some(EffectsCheck::Checking)) {
            return;
        }
        let Some(cable) = self.inventory.iter().flat_map(|i| &i.cables).find(|c| c.id == id) else { return };
        let ports: Vec<_> = recording_ports(cable).map(|p| (p.name.clone(), p.id.clone())).collect();
        if ports.is_empty() {
            return;
        }
        self.effects_checks.insert(id, EffectsCheck::Checking);
        let generation = self.effects_generation;
        cx.spawn_background_with_rejection(
            move |_| {
                let results = ports.into_iter().map(|(name, endpoint)| (name, llcable_control::capture_effects(&endpoint))).collect();
                Msg::WindowsEffectsChecked(id, generation, EffectsCheck::Complete(results))
            },
            Msg::WindowsEffectsChecked(id, generation, EffectsCheck::Unavailable),
        );
    }

    fn schedule_observations(&mut self, cx: &ComponentContext<Self>) {
        self.monitor.schedule(self.page == Page::Cables, self.inventory.as_deref(), cx);
        if self.session.is_some() && self.route_clock.is_none() {
            self.route_clock = Some(cx.spawn_background(|_| {
                std::thread::sleep(Duration::from_millis(120));
                Msg::RouteTick
            }));
        }
    }

    fn start(&mut self, cx: &ComponentContext<Self>) {
        let Some(inv) = self.inventory.clone() else { return };
        if self.busy || self.choosing || self.measuring.is_some() {
            return;
        }
        self.busy = true;
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let settings = self.settings.clone();
        let previous = self.status.as_ref().is_some_and(|s| s.running).then(|| self.channels.clone());
        self.notice = None;
        cx.spawn_background_with_rejection(
            move |_| {
                let plan = match previous {
                    Some(channels) => patch::plan_reusing(&settings, &inv, &channels),
                    None => patch::plan(&settings, &inv),
                };
                Msg::Prepared(generation, Box::new(plan))
            },
            Msg::Prepared(generation, Box::new(Err("Couldn't prepare the audio job. Try again.".into()))),
        );
    }

    fn apply_plan(&mut self, plan: patch::Plan, cx: &ComponentContext<Self>) {
        let hot = self.session.as_ref().is_some_and(|s| s.can_reconfigure(&plan.config));
        // A route can end while its replacement is being prepared. A sparse plan requires the live processors it refers to.
        let rebuild = (!hot && !plan.retained.is_empty()).then(|| (self.settings.clone(), self.inventory.clone()));
        let paused: Vec<_> = plan.config.cables.iter().copied().filter(|&id| self.cable_enabled(id) == Some(false)).collect();
        let work = Arc::new(Mutex::new(RouteChange::begin(&mut self.session, hot)));
        if !hot {
            self.status = None;
        }
        let rejected = Msg::RouteChanged(self.generation, work.clone());
        let generation = self.generation;
        cx.spawn_background_with_rejection(
            move |cancel| {
                let plan = match rebuild {
                    Some((settings, inv)) => {
                        drop(plan);
                        inv.as_deref()
                            .ok_or_else(|| "Refresh audio devices before starting the route.".into())
                            .and_then(|i| patch::plan(&settings, i))
                    }
                    None => Ok(plan),
                };
                {
                    let mut change = work.lock().unwrap();
                    match plan {
                        Ok(plan) if !cancel.is_cancelled() => change.run(
                            plan,
                            |s, config, retained| s.reconfigure_retaining(config, retained),
                            |config| Session::start_with_paused(config, llcable_effects::prepare_voice_thread, &paused).map(Arc::new),
                        ),
                        Err(e) => change.result = Some(Err(e)),
                        Ok(_) => change.result = Some(Err("The audio update was cancelled.".into())),
                    }
                }
                Msg::RouteChanged(generation, work)
            },
            rejected,
        );
    }

    fn edit(&mut self, change: Edit) {
        if self.busy || self.choosing {
            return;
        }
        match change {
            Edit::Send(id, source) => self.settings.toggle(&id, &source),
            Edit::Mute(id) => {
                let m = self.settings.mixes.entry(id).or_default();
                m.muted = !m.muted;
            }
            Edit::Forget(id) => {
                self.settings.mixes.remove(&id);
            }
            Edit::Stage(scope, e) => {
                if let Scope::Source(id) = &scope {
                    let len = self.settings.source_effects.get(id).map_or(0, Vec::len);
                    match &e {
                        StageEdit::Remove(i) if *i < len => self.settings.remap_taps(id, *i, None),
                        StageEdit::Move(i, d) if *i < len => {
                            if let Some(to) = i.checked_add_signed(*d as isize).filter(|&j| j < len) {
                                self.settings.remap_taps(id, *i, Some(to));
                            }
                        }
                        _ => {}
                    }
                }
                let stages = match scope {
                    Scope::Output(id) => &mut self.settings.mixes.entry(id).or_default().effects,
                    Scope::Source(id) => self.settings.source_effects.entry(id).or_default(),
                    Scope::Send(dest, key) => {
                        let Some(send) = self.settings.mixes.get_mut(&dest).and_then(|m| m.sends.iter_mut().find(|s| s.source == key))
                        else {
                            return;
                        };
                        &mut send.effects
                    }
                };
                match e {
                    StageEdit::Add(kind) => stages.push(Stage::new(kind)),
                    StageEdit::Remove(i) if i < stages.len() => {
                        stages.remove(i);
                    }
                    StageEdit::Bypass(i) if i < stages.len() => stages[i].enabled = !stages[i].enabled,
                    StageEdit::Move(i, d) if i < stages.len() => {
                        if let Some(to) = i.checked_add_signed(d as isize).filter(|&to| to < stages.len()) {
                            stages.swap(i, to);
                        }
                    }
                    StageEdit::Value(i, db) if i < stages.len() => stages[i].db = db,
                    StageEdit::Band(i, b, v)
                        if b < 3 && (-18..=18).contains(&v) && stages.get(i).is_some_and(|s| s.kind == Kind::Equalizer) =>
                    {
                        stages[i].eq_db[b] = v
                    }
                    StageEdit::Key(i, key) if i < stages.len() => stages[i].key = Some(key),
                    StageEdit::Plugin(i, plugin) if stages.get(i).is_some_and(|s| s.kind == Kind::Clap) => stages[i].clap = Some(*plugin),
                    StageEdit::Param(i, id, v) => {
                        if let Some(p) = stages.get_mut(i).and_then(|s| s.clap.as_mut())
                            && let Some(param) = p.info.params.iter().find(|p| p.id == id)
                            && v.is_finite()
                            && (param.min..=param.max).contains(&v)
                        {
                            let v = if param.stepped { v.round().clamp(param.min, param.max) } else { v };
                            p.params.insert(id, v);
                        }
                    }
                    _ => {}
                }
            }
            Edit::Level(ref id, ref source, _) | Edit::Balance(ref id, ref source, _) | Edit::Tap(ref id, ref source, _) => {
                if let Some(send) = self.settings.mixes.get_mut(id).and_then(|m| m.sends.iter_mut().find(|s| &s.source == source)) {
                    match change {
                        Edit::Level(_, _, v) => send.gain_db = v,
                        Edit::Balance(_, _, v) => send.balance = v,
                        Edit::Tap(_, _, at) => {
                            send.after_effects = at != Some(0);
                            send.boundary = at.filter(|n| *n != 0);
                        }
                        _ => unreachable!(),
                    }
                }
            }
        }
        self.persist();
    }

    fn open_legacy(&mut self) {
        let result =
            std::env::current_exe().and_then(|exe| std::process::Command::new(exe.with_file_name("noise-filter.exe")).arg("gui").spawn());
        if let Err(e) = result {
            self.notice = Some(Err(format!("Couldn't open the legacy app. Build noise-filter.exe alongside this executable. {e}")));
        }
    }
}

impl Component for Mixer {
    type Input = Page;
    type Message = Msg;

    fn create(page: &Page, cx: &ComponentContext<Self>) -> Self {
        let mut path = patch::path();
        let loaded =
            path.as_ref().map_or_else(|| Err("APPDATA is unavailable. Changes last for this session.".to_owned()), |p| patch::load(p));
        let (settings, notice) = match loaded {
            Ok(s) => (s, None),
            Err(e) => {
                path = None;
                (Settings::default(), Some(Err(e)))
            }
        };
        let mut app = Self {
            page: *page,
            settings,
            path,
            inventory: None,
            monitor: CableMonitor::default(),
            effects_checks: BTreeMap::new(),
            effects_generation: 0,
            status: None,
            channels: Channels::default(),
            session: None,
            generation: 0,
            saved_patch: None,
            scope: None,
            busy: false,
            refreshing: false,
            granting: false,
            elevated: nf_mem::privilege::is_elevated(),
            notice,
            discovery_error: None,
            send_detail: None,
            wire_source: None,
            wire_drag: None,
            choosing: false,
            measuring: None,
            measured: None,
            presets_open: false,
            preset_name: String::new(),
            width: 1000.0,
            route_clock: None,
        };
        app.refresh(cx);
        app
    }

    fn update(&mut self, msg: Msg, cx: &ComponentContext<Self>) {
        match msg {
            Msg::WireSource(source) => {
                self.wire_source = source;
                self.wire_drag = None;
            }
            Msg::WireDrag(point) => self.wire_drag = point,
            Msg::Connect(out, key) => {
                if self.inventory.as_deref().is_some_and(|inv| {
                    inv.outputs
                        .iter()
                        .find(|o| o.id == out)
                        .is_some_and(|o| inv.sources.iter().find(|s| s.id == key).is_some_and(|s| !o.receives_itself(s)))
                }) && !self.settings.mixes.get(&out).is_some_and(|m| m.sends.iter().any(|s| s.source == key))
                {
                    self.edit(Edit::Send(out, key));
                }
                self.wire_drag = None;
            }
            Msg::WireRack(scope, out) => {
                self.page = Page::Mixer;
                self.settings.focused = Some(out);
                self.send_detail = match &scope {
                    Scope::Send(_, key) => Some(key.clone()),
                    _ => None,
                };
                self.scope = Some(scope);
            }
            Msg::Page(page) => {
                self.page = page;
                self.scope = None;
                self.wire_drag = None;
            }
            Msg::Select(id) => {
                self.settings.focused = (self.settings.focused.as_ref() != Some(&id)).then_some(id);
                self.scope = None;
                self.send_detail = None;
                self.persist();
            }
            Msg::Presets(open) => self.presets_open = open,
            Msg::PresetName(name) => self.preset_name = name,
            Msg::SavePreset if !self.preset_name.trim().is_empty() => {
                self.settings.presets.insert(self.preset_name.trim().to_owned(), Preset {
                    mixes: self.settings.mixes.clone(),
                    source_effects: self.settings.source_effects.clone(),
                });
                self.persist();
            }
            Msg::LoadPreset if !self.busy && !self.choosing => {
                if let Some(p) = self.settings.presets.get(self.preset_name.trim()) {
                    self.settings.mixes = p.mixes.clone();
                    self.settings.source_effects = p.source_effects.clone();
                    self.scope = None;
                    self.send_detail = None;
                    self.persist();
                }
            }
            Msg::DeletePreset if !self.busy => {
                self.settings.presets.remove(self.preset_name.trim());
                self.persist();
            }
            Msg::SavePreset | Msg::LoadPreset | Msg::DeletePreset => {}
            Msg::Theme(theme) => {
                self.settings.theme = theme;
                self.persist();
            }
            Msg::SendDetail(id) => self.send_detail = (self.send_detail.as_ref() != Some(&id)).then_some(id),
            Msg::InspectCable(id) => {
                self.settings.focused = Some(id);
                self.page = Page::Cables;
            }
            Msg::Scope(scope) => self.scope = (self.scope.as_ref() != Some(&scope)).then_some(scope),
            Msg::Edit(change) => self.edit(change),
            Msg::Refresh => self.refresh(cx),
            Msg::Monitor(enabled) => self.monitor.set_enabled(enabled),
            Msg::ResetCableCounters => self.monitor.reset(),
            Msg::CheckWindowsEffects(id) => self.check_windows_effects(id, cx),
            Msg::WindowsEffectsChecked(id, generation, result) => {
                if generation == self.effects_generation {
                    self.effects_checks.insert(id, result);
                }
            }
            Msg::Measure(id) if !self.busy && self.measuring.is_none() && !self.running() => {
                use llcable_control::voice::{CableLatencyRequest, cable_latency_test};
                let Some(c) = self.inventory.iter().flat_map(|i| &i.cables).find(|c| c.id == id) else { return };
                let endpoint = |role| c.ports.iter().find(|p| p.active && p.role == role).map(|p| p.id.clone());
                let (Some(probe_device), Some(capture_device)) = (endpoint(EndpointKind::Speakers), endpoint(EndpointKind::Microphone))
                else {
                    return;
                };
                let cancel = Arc::new(AtomicBool::new(false));
                self.measuring = Some((id, cancel.clone()));
                self.measured = None;
                cx.spawn_background_with_rejection(
                    move |_| {
                        match llcable_control::diagnostics(id) {
                            Ok(d) if d.attached => {
                                return Msg::Measured(id, Err("Stop this cable’s direct feed before measuring latency.".into()));
                            }
                            Err(e) => return Msg::Measured(id, Err(format!("Could not check the cable before measuring latency: {e}"))),
                            Ok(_) => {}
                        }
                        let result = match cable_latency_test(&CableLatencyRequest {
                            probe_device,
                            capture_device,
                            repetitions: 3,
                            cancel: Some(&cancel),
                        }) {
                            Ok(r) if r.valid == 0 => Err(format!(
                                "No latency result (0 of {} sweeps matched). {} {}",
                                r.repetitions,
                                r.conditions,
                                r.warnings.join(" ")
                            )),
                            Ok(r) => Ok(format!(
                                "{} of {} sweeps matched · Median {:.2} ms · Spread {:.2} ms · {} {}",
                                r.valid,
                                r.repetitions,
                                r.cable_ms_median,
                                r.cable_ms_spread,
                                r.conditions,
                                r.warnings.join(" ")
                            )),
                            // CableLatencyRequest documents this cancellation response. Keep a concurrent device failure visible.
                            Err(e) if cancel.load(Ordering::Relaxed) && e.0 == "The test was cancelled." => Ok(e.0),
                            Err(e) => Err(e.0),
                        };
                        Msg::Measured(id, result)
                    },
                    Msg::Measured(id, Err("The latency job is busy. Try again.".into())),
                );
            }
            Msg::Measure(_) => {}
            Msg::CancelMeasurement => {
                if let Some((_, cancel)) = &self.measuring {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
            Msg::Measured(id, result) => {
                self.measuring = None;
                self.measured = Some((id, result));
            }
            Msg::Discovered(result) => {
                self.refreshing = false;
                match result {
                    Ok(inv) => {
                        self.discovery_error = None;
                        self.monitor.rediscovered(&inv);
                        self.inventory = Some(Arc::new(inv));
                    }
                    Err(e) => self.discovery_error = Some(e),
                }
            }
            Msg::MeterMode(mode) => {
                self.settings.meter_mode = mode;
                self.persist();
            }
            Msg::Palette(palette) => {
                self.settings.palette = palette;
                self.persist();
            }
            Msg::Roomy(roomy) => {
                self.settings.roomy = roomy;
                self.persist();
            }
            Msg::Start => self.start(cx),
            Msg::CableEnabled(id, enabled) => {
                if !self.busy
                    && let Some(session) = &self.session
                {
                    if let Err(e) = session.set_cable_enabled(id, enabled) {
                        self.notice = Some(Err(e));
                    }
                    self.status = Some(session.snapshot());
                }
            }
            Msg::Stop if !self.busy => {
                if let Some(session) = self.session.take() {
                    self.busy = true;
                    self.status = None;
                    self.generation = self.generation.wrapping_add(1);
                    cx.spawn_background_with_rejection(
                        move |_| {
                            drop(session);
                            Msg::Stopped
                        },
                        Msg::Stopped,
                    );
                }
            }
            Msg::Stop => {}
            Msg::Prepared(gen_id, res) if gen_id == self.generation => match *res {
                Err(e) => {
                    self.busy = false;
                    self.notice = Some(Err(e));
                }
                Ok(plan) => self.apply_plan(plan, cx),
            },
            Msg::Prepared(_, _) => {}
            Msg::RouteChanged(gen_id, work) if gen_id == self.generation => {
                self.busy = false;
                match work.lock().unwrap().finish(&mut self.session, &mut self.channels) {
                    Ok(()) => self.saved_patch = Some(self.settings.clone()),
                    Err(e) => {
                        self.notice = Some(Err(e));
                        if self.session.is_none() {
                            self.saved_patch = None;
                            self.channels = Channels::default();
                        }
                    }
                }
                self.status = self.session.as_ref().map(|s| s.snapshot());
            }
            Msg::RouteChanged(_, _) => {}
            Msg::Stopped => {
                self.busy = false;
                self.status = None;
                self.saved_patch = None;
                self.channels = Channels::default();
            }
            Msg::Authorize if !self.granting && !self.elevated => {
                self.granting = true;
                cx.spawn_background_with_rejection(
                    |_| Msg::Authorized(llcable_control::authorize().map_err(|e| e.to_string())),
                    Msg::Authorized(Err("Couldn't request access.".into())),
                );
            }
            Msg::Authorize => {}
            Msg::Authorized(result) => {
                self.granting = false;
                self.notice = Some(result.map(|_| "Direct feed is available for this session.".into()));
            }
            Msg::RouteTick => {
                self.route_clock = None;
                if let Some(session) = &self.session {
                    self.status = Some(session.snapshot());
                }
            }
            Msg::CableSamples(generation, samples) => self.monitor.received(generation, samples),
            Msg::OpenLegacy => self.open_legacy(),
            Msg::ChoosePlugin(scope, i) if !self.busy && !self.choosing => {
                self.choosing = true;
                let owner = nf_gui::effect_picker::owner();
                let fallback = scope.clone();
                cx.spawn_background_with_rejection(
                    move |_| {
                        let result = nf_gui::effect_picker::choose(owner).and_then(|path| {
                            path.map(|path| {
                                let info = llcable_effects::inspect_clap(&path, None)?;
                                Ok(Box::new(Plugin { path, info, params: Default::default() }))
                            })
                            .transpose()
                        });
                        Msg::Chosen(scope, i, result)
                    },
                    Msg::Chosen(fallback, i, Err("Could not open the plug-in picker.".into())),
                );
            }
            Msg::ChoosePlugin(_, _) => {}
            Msg::Chosen(scope, i, result) => {
                self.choosing = false;
                match result {
                    Ok(Some(p)) => self.edit(Edit::Stage(scope, StageEdit::Plugin(i, p))),
                    Err(e) => self.notice = Some(Err(e)),
                    Ok(None) => {}
                }
            }
            Msg::Width(w) => self.width = w,
        }
        self.schedule_observations(cx);
    }

    fn view(&self, _: &Page, cx: &mut ViewContext<Self>) -> View {
        cx.window_title("LLCable · Control panel & mixer");
        cx.window_visuals(
            WindowVisuals::new()
                .backdrop(WindowBackdrop::Mica)
                .theme(match self.settings.theme {
                    Theme::System => WindowTheme::System,
                    Theme::Light => WindowTheme::Light,
                    Theme::Dark => WindowTheme::Dark,
                })
                .client_size(1000.0, 680.0)
                .constraints(WindowConstraints { min_width: Some(560.0), min_height: Some(420.0), ..Default::default() }),
        );
        cx.on_window_size(cx.callback(|s: WindowSize| Msg::Width(s.width)));
        desk::view(self, cx)
    }
}

impl Drop for Mixer {
    fn drop(&mut self) {
        if let Some((_, cancel)) = &self.measuring {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use llcable_control::routing::{Router, Tap};

    use super::*;
    use crate::patch::{Destination, Source};

    #[test]
    fn applying_and_rejecting_an_edit_keeps_the_same_running_route_and_matching_meters() {
        let output = "play:speaker";
        let inv = Inventory {
            cables: vec![],
            sources: vec![Source { id: "mic".into(), label: "Mic".into(), channels: 2, owner: None }],
            outputs: vec![Destination { id: output.into(), name: "Speaker".into(), cable: None }],
        };
        let mut settings = Settings::default();
        settings.toggle(output, "mic");
        settings.source_effects.insert("mic".into(), vec![Stage::new(Kind::Gain)]);
        let first = patch::plan(&settings, &inv).unwrap();
        let mut channels = first.channels;
        let shared_meter = channels.meters[0].1.clone();
        let router = Rc::new(RefCell::new(Router::new(48000, 16, 1, first.config.buses).unwrap()));
        let mut running = Some(router.clone());
        let reconfigure = |active: &Rc<RefCell<Router>>, config: RouteConfig, retained: Vec<(usize, usize)>| {
            let mut replacement = Router::new(48000, 16, config.inputs.len(), config.buses).map_err(|e| e.to_string())?;
            active.borrow_mut().activate_edit(&mut replacement, &retained).map_err(|e| e.to_string())
        };
        let start = |_| panic!("a same-endpoint change must keep the active session");
        settings.mixes.get_mut(output).unwrap().effects.push(Stage { db: -6, ..Stage::new(Kind::Gain) });
        let applied = patch::plan_reusing(&settings, &inv, &channels).unwrap();
        let mut job = RouteChange::begin(&mut running, true);
        job.run(applied, reconfigure, start);
        job.finish(&mut running, &mut channels).unwrap();
        assert!(Rc::ptr_eq(running.as_ref().unwrap(), &router));

        settings.mixes.get_mut(output).unwrap().effects.clear();
        let mut bad = patch::plan_reusing(&settings, &inv, &channels).unwrap();
        bad.config.buses[1].sends[0].from = Tap::Bus(99);
        let mut job = RouteChange::begin(&mut running, true);
        job.run(bad, reconfigure, start);
        assert!(job.finish(&mut running, &mut channels).is_err());
        assert!(Rc::ptr_eq(running.as_ref().unwrap(), &router));
        let mut active = router.borrow_mut();
        active.process_stereo(&[&[[0.5; 2]; 4]]).unwrap();
        assert_eq!(channels.meters.len(), 2);
        assert_eq!(shared_meter.read().input_peak, Some(0.5));
        assert_eq!(channels.meters[0].1.read(), shared_meter.read());
        assert_eq!(channels.meters[1].1.read().output_peak, Some(active.output(1).unwrap()[0][0]));
        assert!((active.output(1).unwrap()[0][0] - 0.25059).abs() < 0.00001);

        let mut rejected = RouteChange::begin(&mut running, false);
        assert!(running.is_none());
        assert!(rejected.finish(&mut running, &mut channels).is_err());
        assert!(Rc::ptr_eq(running.as_ref().unwrap(), &router));
        assert_eq!(channels.meters.len(), 2);
    }
}
