mod monitor;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use llcable_audio::{RouteStatus, Session};
use llcable_control::CableId;
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
    Measure(CableId),
    CancelMeasurement,
    Measured(CableId, Result<String, String>),
    Discovered(Result<Inventory, String>),
    Start,
    Stop,
    Prepared(u64, Box<Result<patch::Plan, String>>),
    Started(u64, Box<Result<(Session, Channels), String>>),
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

pub struct Mixer {
    pub page: Page,
    pub settings: Settings,
    path: Option<PathBuf>,
    pub inventory: Option<Arc<Inventory>>,
    pub monitor: CableMonitor,
    pub status: Option<RouteStatus>,
    pub channels: Channels,
    session: Option<Session>,
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
            cx.spawn_background_with_rejection(
                |_| Msg::Discovered(Inventory::read()),
                Msg::Discovered(Err("The device list is busy. Refresh to try again.".into())),
            );
        }
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
        self.notice = None;
        cx.spawn_background_with_rejection(
            move |_| Msg::Prepared(generation, Box::new(patch::plan(&settings, &inv))),
            Msg::Prepared(generation, Box::new(Err("Couldn't prepare the audio job. Try again.".into()))),
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
            Msg::Measure(id) if !self.busy && self.measuring.is_none() && !self.running() => {
                use llcable_control::EndpointKind;
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
            Msg::Stop if !self.busy => {
                if let Some(session) = self.session.take() {
                    self.busy = true;
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
                Ok(plan) => {
                    let old = self.session.take();
                    cx.spawn_background_with_rejection(
                        move |cancel| {
                            drop(old);
                            if cancel.is_cancelled() {
                                return Msg::Stopped;
                            }
                            let result = Session::start(plan.config).map(|session| (session, plan.channels));
                            Msg::Started(gen_id, Box::new(result))
                        },
                        Msg::Started(gen_id, Box::new(Err("Couldn't start the audio job. Try again.".into()))),
                    );
                }
            },
            Msg::Prepared(_, _) => {}
            Msg::Started(gen_id, res) if gen_id == self.generation => {
                self.busy = false;
                self.status = None;
                match *res {
                    Ok((session, channels)) => {
                        self.status = Some(session.snapshot());
                        self.session = Some(session);
                        self.channels = channels;
                        self.saved_patch = Some(self.settings.clone());
                    }
                    Err(e) => {
                        self.notice = Some(Err(e));
                        self.saved_patch = None;
                        self.channels = Channels::default();
                    }
                }
            }
            Msg::Started(_, _) => {}
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
                if !self.busy {
                    self.status = self.session.as_ref().map(Session::snapshot);
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
