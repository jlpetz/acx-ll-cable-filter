//! The meters and the live latency readout (gui.md sections 4 and 5): a child component with its own
//! re-armed telemetry waiter, so a 20 Hz tick re-renders only this small view, not the whole window
//! (windows-reactor.md pitfall 6). Its `Input` only changes when a toggle or the window's width does.
//!
//! The waiter runs at 20 Hz while the engine runs and either section is shown, at 2 Hz while the
//! engine is stopped (to notice it start), and not at all while both are hidden: the window's own
//! 2 Hz status waiter is the clock then.

use std::sync::Arc;
use std::time::Duration;

use nf_engine::{Engine, EngineState, MeterSnapshot, TelemetrySnapshot};
use windows_reactor::*;

use crate::meter::{self, CLIP_RED};
use crate::widgets::{card, secondary, show_switch};
use crate::{SharedEngine, format, waiting};

/// 20 Hz: the meters' rate while the engine runs.
const FAST_TICK: Duration = Duration::from_millis(50);
/// 2 Hz while the engine isn't running: nothing moves, but a start shows within half a second.
const SLOW_TICK: Duration = Duration::from_millis(500);

/// What the window hands the live section. Compared on every window render: the engine by pointer,
/// the callbacks by identity (captureless mappers keep it across renders).
#[derive(Clone, PartialEq)]
pub(crate) struct LiveInput {
    pub engine: SharedEngine,
    /// `processing.meters_enabled`: the "Show meters" toggle.
    pub show_meters: bool,
    /// `processing.latency_readout_enabled`: the "Live readout" toggle.
    pub show_readout: bool,
    /// The meters' track width ([`crate::widgets::track_width`]).
    pub track: f64,
    pub on_show_meters: Callback<bool>,
    pub on_show_readout: Callback<bool>,
}

impl LiveInput {
    fn visible(&self) -> bool {
        self.show_meters || self.show_readout
    }
}

pub(crate) enum LiveMsg {
    /// A fresh snapshot, boxed: it's about 300 bytes, and the message is moved through the UI's queue (one small allocation
    /// a tick, on the UI side).
    Tick(Box<TelemetrySnapshot>),
    /// The CLIP badge was clicked.
    ResetClip,
}

pub(crate) struct Live {
    engine: Arc<Engine>,
    visible: bool,
    telemetry: TelemetrySnapshot,
    /// The waiter in flight; exactly one at a time while visible.
    feed: Option<ComponentTask>,
}

impl Live {
    /// Arms the waiter unless one is in flight or there's nothing to show. Called after every
    /// message and input change: a delivered tick re-arms here, and a chain that ended (a rejected
    /// spawn) restarts at the next event rather than never.
    fn ensure_feed(&mut self, context: &ComponentContext<Self>) {
        if !self.visible || waiting(self.feed.as_ref()) {
            return;
        }
        let tick = if self.telemetry.state == EngineState::Running { FAST_TICK } else { SLOW_TICK };
        let engine = Arc::clone(&self.engine);
        self.feed = Some(context.spawn_background(move |cancel| {
            // std's sleep is a high-resolution waitable timer on Windows: 50 ms isn't rounded up to the 15.6 ms tick
            std::thread::sleep(tick);
            // Cancelled when the window closes: the result is discarded, so skip the read
            LiveMsg::Tick(Box::new(if cancel.is_cancelled() { TelemetrySnapshot::default() } else { engine.telemetry() }))
        }));
    }

    fn meters(&self, track: f64, context: &ViewContext<Self>) -> View {
        let t = &self.telemetry;
        let running = t.state == EngineState::Running;
        // Not running: the atomics hold the last run's values, so show empty bars instead
        let shown = |m: MeterSnapshot| {
            if running { m } else { MeterSnapshot { peak_dbfs: f32::NEG_INFINITY, rms_dbfs: f32::NEG_INFINITY } }
        };
        let numbers = |m: MeterSnapshot| if running { format::meter_numbers(m) } else { format::DASH.to_owned() };
        let near_clip: View =
            if running && t.near_clip { secondary("Near clipping: turn the mic's gain down").into() } else { View::empty() };
        let output_extra: View = if running { secondary(format::reduction_and_speech(t)).into() } else { View::empty() };
        StackPanel::new().spacing(6.0).children((
            meter_header("Input", numbers(t.input), clip_badge(t.clip_latched, context), 0.0),
            meter::bar(shown(t.input), track),
            near_clip,
            meter_header("Output", numbers(t.output), View::empty(), 6.0),
            meter::bar(shown(t.output), track),
            output_extra,
        ))
    }

    fn readout(&self) -> View {
        let t = &self.telemetry;
        let note = secondary("Excludes the cable's own buffer and the receiving app.");
        if t.state != EngineState::Running {
            return StackPanel::new().spacing(4.0).children((secondary("Shown while the filter runs."), note));
        }
        StackPanel::new().spacing(4.0).children((
            TextBlock::new().text(format::pipeline(t)).font_weight(FontWeight::SEMI_BOLD),
            TextBlock::new().text(format::budget(t)).text_wrapping(TextWrapping::Wrap),
            TextBlock::new().text(format::clocks(t)).text_wrapping(TextWrapping::Wrap),
            TextBlock::new().text(format::streams(t)).text_wrapping(TextWrapping::Wrap),
            TextBlock::new().text(format::cpu(t)).text_wrapping(TextWrapping::Wrap),
            TextBlock::new().text(format::dropouts(t)).text_wrapping(TextWrapping::Wrap),
            TextBlock::new().text(format::allocations(t)).text_wrapping(TextWrapping::Wrap),
            note,
        ))
    }
}

/// A meter's header row: its name, its numbers, and the clip badge (input only) at the right.
/// `top` adds air above the second meter.
fn meter_header(name: &str, numbers: String, badge: View, top: f64) -> View {
    Grid::new()
        .columns([GridLength::Auto, GridLength::STAR, GridLength::Auto])
        .column_spacing(12.0)
        .margin(Thickness::new(0.0, top, 0.0, 0.0))
        .children((
            TextBlock::new().text(name).font_weight(FontWeight::SEMI_BOLD).vertical_alignment(VerticalAlignment::Center).grid_column(0),
            TextBlock::new()
                .text(numbers)
                .opacity(0.72)
                .text_trimming(TextTrimming::CharacterEllipsis)
                .vertical_alignment(VerticalAlignment::Center)
                .grid_column(1),
            Border::new().grid_column(2).vertical_alignment(VerticalAlignment::Center).content(badge),
        ))
}

/// The "CLIP" badge: red once the input has clipped, until clicked (which resets the engine's latch).
fn clip_badge(latched: bool, context: &ViewContext<Live>) -> View {
    let (fill, text, opacity): (Brush, Brush, f64) = if latched {
        (CLIP_RED.into(), Color::rgb(0xFF, 0xFF, 0xFF).into(), 1.0)
    } else {
        (Color::transparent().into(), ThemeBrush::PrimaryText.into(), 0.45)
    };
    Button::new()
        .style(ButtonStyle::Subtle)
        .on_click(context.callback(|()| LiveMsg::ResetClip))
        .content(
            Border::new()
                .background(fill)
                .border_brush(ThemeBrush::CardStroke)
                .border_thickness(1.0)
                .corner_radius(4.0)
                .padding(Thickness::xy(6.0, 1.0))
                .content(TextBlock::new().text("CLIP").font_size(11.0).font_weight(FontWeight::BOLD).foreground(text).opacity(opacity)),
        )
        .tooltip(if latched { "The input clipped. Click to reset." } else { "Lights when the input clips, until clicked." })
}

impl Component for Live {
    type Input = LiveInput;
    type Message = LiveMsg;

    fn create(input: &LiveInput, context: &ComponentContext<Self>) -> Self {
        let engine = Arc::clone(&input.engine.0);
        let telemetry = engine.telemetry();
        let mut live = Self { engine, visible: input.visible(), telemetry, feed: None };
        live.ensure_feed(context);
        live
    }

    fn input_changed(&mut self, input: &LiveInput, context: &ComponentContext<Self>) {
        self.engine = Arc::clone(&input.engine.0);
        if input.visible() && !self.visible {
            // Shown again: the snapshot is as old as the hiding, so don't draw it for a tick
            self.telemetry = self.engine.telemetry();
        }
        self.visible = input.visible();
        self.ensure_feed(context);
    }

    fn update(&mut self, message: LiveMsg, context: &ComponentContext<Self>) {
        match message {
            LiveMsg::Tick(telemetry) => self.telemetry = *telemetry,
            LiveMsg::ResetClip => {
                // Cheap: one atomic store. Cleared here too, so the badge doesn't wait for the next tick.
                self.engine.reset_clip();
                self.telemetry.clip_latched = false;
            }
        }
        self.ensure_feed(context);
    }

    fn view(&self, input: &LiveInput, context: &mut ViewContext<Self>) -> View {
        let meters = if input.show_meters { self.meters(input.track, context) } else { View::empty() };
        let readout = if input.show_readout { self.readout() } else { View::empty() };
        StackPanel::new().spacing(crate::widgets::SECTION_SPACING).children((
            card("Meters", Some(show_switch("Show meters", input.show_meters, input.on_show_meters.clone())), meters),
            card("Latency", Some(show_switch("Live readout", input.show_readout, input.on_show_readout.clone())), readout),
        ))
    }
}
