//! Diagnostics for the design preview. Levels follow the example mix. The comparison run is recorded driver evidence.
use windows_reactor::*;

use super::{DESTS, Kind, Mixer, Msg, SOURCES, Scope, Stage, model_level};

fn text(value: impl Into<String>) -> TextBlock {
    TextBlock::new().text(value).text_wrapping(TextWrapping::Wrap)
}

fn section(title: &str) -> TextBlock {
    text(title).font_size(16.0).font_weight(FontWeight::SEMI_BOLD)
}

fn reading(label: &str, value: impl Into<String>) -> View {
    Grid::new()
        .columns([GridLength::Star(0.9), GridLength::Star(1.3)])
        .column_spacing(12.0)
        .children((text(label).opacity(0.78), text(value).grid_column(1)))
}

fn peak_label(levels: &[f32]) -> String {
    match levels.iter().copied().filter(|db| db.is_finite()).reduce(f32::max) {
        Some(db) if db >= 0.0 => format!("{db:+.1} dBFS · full scale"),
        Some(db) if db > -0.05 => ">−0.1 dBFS".into(),
        Some(db) => format!("{db:.1} dBFS"),
        None => "−∞ dBFS · silence".into(),
    }
}

// A copy taken before a shared stage never reaches that stage. Branch stages always precede send gain;
// output stages see the summed sends before the output mute. Use the same level model as the Mixer.
struct StageReading {
    scope: Scope,
    stage: Stage,
    input_db: Vec<f32>,
}

fn stage_readings(app: &Mixer) -> Vec<StageReading> {
    let d = app.output;
    let mut result = Vec::new();
    let mut visit = |scope, stages: &[Stage], levels: &mut [f32]| {
        for stage in stages {
            if matches!(stage.kind, Kind::Noise | Kind::Gate) {
                result.push(StageReading { scope, stage: *stage, input_db: levels.to_vec() });
            }
            for level in levels.iter_mut() {
                *level = model_level(*level, std::slice::from_ref(stage));
            }
        }
    };
    for (s, _) in SOURCES.iter().enumerate().filter(|(s, _)| app.scene.connected(*s, d)) {
        let mut levels = app.input_levels(s);
        visit(Scope::Source(s), app.scene.source_stages_for_send(s, d), &mut levels);
        visit(Scope::Send(s, d), app.scene.at(Scope::Send(s, d)), &mut levels);
    }
    visit(Scope::Output(d), app.scene.at(Scope::Output(d)), &mut app.mix_levels_before_output(d));
    result
}

impl StageReading {
    fn gate_status(&self) -> String {
        let n = self.input_db.len();
        let open = self.input_db.iter().filter(|&&db| db.is_finite() && db >= self.stage.value as f32).count();
        match (open, n) {
            (_, 0) => "no channel data".into(),
            (0, _) => "closed".into(),
            _ if open == n => "open".into(),
            _ => format!("{open} of {n} channels open"),
        }
    }

    fn view(&self, cx: &ViewContext<Mixer>) -> View {
        let Self { scope, stage, .. } = *self;
        let name = if stage.kind == Kind::Noise { "RNNoise" } else { "Noise gate" };
        let input = peak_label(&self.input_db);
        let setting = format!("{} {}", stage.kind.param(), stage.kind.format(stage.value));
        let status = match (stage.enabled, stage.kind) {
            (false, _) => format!("Bypassed  ·  {setting}"),
            (true, Kind::Gate) => format!("Demo gate {}  ·  {setting}  ·  Input {input}", self.gate_status()),
            _ => format!("Enabled  ·  {setting}  ·  Demo input {input}"),
        };
        let id = stage.id;
        let edit = Button::new()
            .grid_column(1)
            .vertical_alignment(VerticalAlignment::Center)
            .on_click(cx.callback(move |()| Msg::EditStage(scope, id)))
            .automation_name(format!("Edit {name} on {}", scope.title()))
            .content("Edit");
        let detail = StackPanel::new()
            .spacing(2.0)
            .children((text(format!("{}  ·  {name}", scope.title())).font_weight(FontWeight::SEMI_BOLD), text(status).font_size(12.0)));
        Grid::new().columns([GridLength::STAR, GridLength::Auto]).column_spacing(8.0).children((detail, edit))
    }
}

pub(super) fn view(app: &Mixer, cx: &ViewContext<Mixer>) -> View {
    let d = app.output;
    let sender = cx.sender();
    let choose_output = ComboBox::new()
        .items_source(DESTS)
        .selected_index(Some(d))
        .on_selection_changed(move |i: Option<usize>| {
            if let Some(next) = i.filter(|&next| next < DESTS.len() && next != d) {
                sender.send(Msg::Output(next));
            }
        })
        .automation_name("Inspect output")
        .slot(ComboBoxSlot::Header, "Inspect output");
    let tools = Grid::new().columns([GridLength::STAR, GridLength::Auto]).column_spacing(12.0).children((
        Border::new().horizontal_alignment(HorizontalAlignment::Left).content(choose_output),
        Button::new()
            .grid_column(1)
            .vertical_alignment(VerticalAlignment::Bottom)
            .on_click(cx.callback(|()| Msg::OpenLive))
            .content("Open live settings"),
    ));
    let mut signal = vec![section("Simulated signal").into()];
    for (s, name) in SOURCES.iter().enumerate().filter(|(s, _)| app.scene.connected(*s, d)) {
        let peaks = app.input_levels(s);
        let gain = app.scene.sends[s][d].gain_db;
        signal.push(reading(name, format!("{} input peak  ·  {gain:+} dB send  ·  {}", peak_label(&peaks), app.tap_name(s, d))));
    }
    if signal.len() == 1 {
        signal.push(text("No sources connected. Select this output in Mixer and add a source.").into());
    }
    signal.push(reading(if app.muted[d] { "Output peak · muted" } else { "Output peak" }, peak_label(&app.output_levels(d))));

    let mut stages = vec![section("Denoisers and gates on this path").into()];
    let readings = stage_readings(app);
    if readings.is_empty() {
        stages.push(text("This path contains no denoiser or gate.").into());
    }
    for stage in &readings {
        stages.push(stage.view(cx));
    }
    if readings.iter().any(|r| r.stage.enabled && r.stage.kind == Kind::Gate) {
        stages.push(
            text("The demo gate switches at each channel’s peak. The live gate uses block RMS, hold and gain ramps.")
                .font_size(12.0)
                .into(),
        );
    }
    if readings.iter().any(|r| r.stage.enabled && r.stage.kind == Kind::Noise) {
        stages.push(text("RNNoise reduction and speech probability require live audio.").font_size(12.0).into());
    }

    // TODO 19, “A marker from publication to the recording client”, 2026-11-20: one shared RAW reader on the dev box.
    let marker = StackPanel::new().spacing(4.0).children((
        section("Recorded driver test"),
        text("Dev box  ·  20 Nov 2026  ·  30 s  ·  shared RAW capture").font_size(12.0).opacity(0.8),
        reading("Recorder period", "3.33 ms  ·  160 frames at 48 kHz"),
        reading("Ring fill target", "1 packet  ·  160 frames"),
        reading("Ring write to recorder arrival", "3.11 ms median  ·  3.67 ms p99"),
        reading("Driver underruns", "0 during this test"),
    ));
    let live = StackPanel::new().spacing(6.0).children((
        section("Live app"),
        reading("Meters → Output", "Reduction, Gate attenuation and Speech probability when available. Reduction includes a gate placed immediately after the denoiser."),
        reading("Processing", "Adjust Max suppression. Gate settings has the threshold or auto margin, attack, hold, release and closed range."),
        reading("Latency", "Live readout shows sample timing, queue delay, noise CPU p99 and dropouts. Measure latency runs a sweep."),
        text(r"App log   %LOCALAPPDATA%\noise-filter\noise-filter.log").font_size(12.0).is_text_selection_enabled(true),
        text("To trace a timing glitch from the CLI, noise-filter run --flight saves triggered traces in the flight folder beside the log.")
            .font_size(12.0).is_text_selection_enabled(true),
    ));
    StackPanel::new().spacing(14.0).children((
        tools,
        StackPanel::new().spacing(4.0).keyed_children(signal.into_iter().enumerate()),
        StackPanel::new().spacing(5.0).keyed_children(stages.into_iter().enumerate()),
        marker,
        live,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appearance::Appearance;
    use crate::{Scene, Tap};

    #[test]
    fn dry_and_partial_taps_exclude_later_shared_stages_and_other_outputs() {
        let mut app = Mixer::new(Appearance::default());
        app.scene = Scene::empty(Scope::Source(0));
        app.scene.toggle(0, 0);
        app.scene.toggle(0, 1);
        app.output = 0;
        let noise = app.scene.add(Kind::Noise);
        let gate = app.scene.add(Kind::Gate);
        app.scene.sends[0][0].tap = Tap::Raw;
        app.scene.scope = Scope::Send(0, 0);
        let branch = app.scene.add(Kind::Gate);
        app.scene.stage_mut(branch).unwrap().enabled = false;
        app.scene.scope = Scope::Send(0, 1);
        app.scene.add(Kind::Noise);
        app.scene.scope = Scope::Output(0);
        let output = app.scene.add(Kind::Gate);
        let ids = |app: &Mixer| stage_readings(app).iter().map(|s| s.stage.id).collect::<Vec<_>>();
        assert_eq!(ids(&app), [branch, output]);
        assert!(!stage_readings(&app)[0].stage.enabled);
        app.scene.sends[0][0].tap = Tap::Stage(noise);
        assert_eq!(ids(&app), [noise, branch, output]);
        app.scene.sends[0][0].tap = Tap::End;
        assert_eq!(ids(&app), [noise, gate, branch, output]);
        app.scene.toggle(0, 0);
        assert_eq!(ids(&app), [output]);
        assert_eq!(stage_readings(&app)[0].gate_status(), "closed");
    }

    #[test]
    fn stage_input_readings_follow_upstream_gain_and_per_channel_gate_decisions() {
        let mut app = Mixer::new(Appearance::default());
        app.scene = Scene::empty(Scope::Source(1));
        app.scene.toggle(1, 0);
        app.output = 0;
        let gain = app.scene.add(Kind::Gain);
        app.scene.stage_mut(gain).unwrap().value = -6;
        app.scene.scope = Scope::Send(1, 0);
        let gate = app.scene.add(Kind::Gate);
        app.scene.stage_mut(gate).unwrap().value = -20;
        app.scene.sends[1][0].gain_db = -12;
        app.scene.scope = Scope::Output(0);
        let noise = app.scene.add(Kind::Noise);
        let readings = stage_readings(&app);
        let input = app.input_levels(1);
        assert_eq!(readings[0].input_db, [input[0] - 6.0, input[1] - 6.0]);
        assert_eq!(readings[0].gate_status(), "1 of 2 channels open");
        assert_eq!(readings[1].stage.id, noise);
        assert_eq!(readings[1].input_db[0], f32::NEG_INFINITY);
        assert!((readings[1].input_db[1] - (input[1] - 18.0)).abs() < 0.001);
        app.demo_signal = false;
        assert_eq!(stage_readings(&app)[0].gate_status(), "closed");
    }
}
