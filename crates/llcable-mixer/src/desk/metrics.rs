use llcable_audio::Level;
use llcable_control::Diagnostics;
use llcable_control::routing::StageReading;
use windows_reactor::*;

use super::{caption, col, title};

fn count(n: u64) -> String {
    let mut text = n.to_string();
    let mut left = text.len();
    while left > 3 {
        left -= 3;
        text.insert(left, ',');
    }
    text
}

fn label(text: impl Into<String>) -> TextBlock {
    TextBlock::new().text(text).font_size(13.0).text_wrapping(TextWrapping::Wrap)
}

fn heading(text: impl Into<String>) -> TextBlock {
    label(text).font_weight(FontWeight::SEMI_BOLD)
}

fn value(subject: &str, metric: &str, text: String, alert: bool) -> TextBlock {
    let name = if alert { format!("{subject}, {metric}, error, {text}") } else { format!("{subject}, {metric}, {text}") };
    let cell = if alert {
        label(format!("! {text}")).foreground(ThemeBrush::SystemCritical).font_weight(FontWeight::SEMI_BOLD)
    } else {
        label(text)
    };
    cell.horizontal_alignment(HorizontalAlignment::Right).automation_name(name)
}

/// All rows use the same tracks. Only the window width determines their size, so a counter gaining a digit cannot move its neighbours.
fn table(columns: impl IntoIterator<Item = GridLength>, rows: Vec<Vec<TextBlock>>) -> View {
    let height = rows.len();
    let cells = rows.into_iter().enumerate().flat_map(|(row, cells)| {
        cells.into_iter().enumerate().map(move |(column, cell)| cell.grid_column(column as i32).grid_row(row as i32))
    });
    Grid::new()
        .columns(columns)
        .rows(std::iter::repeat_n(GridLength::Auto, height))
        .row_spacing(3.0)
        .column_spacing(12.0)
        .keyed_children(cells.enumerate())
}

pub(super) fn cable_properties(cable: &str, d: &Diagnostics) -> View {
    let period = if d.packet_frames == 0 {
        "Idle".to_owned()
    } else {
        format!("{} frames · {:.2} ms", count(d.packet_frames.into()), d.packet_frames as f32 / 48.0)
    };
    let properties = [
        ("Open capture streams", count(d.capture_streams.into())),
        ("Reader period", period),
        ("Worst timer lateness (lifetime)", format!("{} µs", count(d.timer_late_max_us))),
    ];
    let headers = properties.iter().map(|(name, _)| heading(*name).horizontal_alignment(HorizontalAlignment::Right)).collect();
    let cells = properties.into_iter().map(|(name, text)| value(cable, name, text, false)).collect();
    table([GridLength::STAR; 3], vec![headers, cells])
        .tooltip("Worst timer lateness is the highest capture timer delay since this cable device started.")
}

pub(super) fn cable_losses(cable: &str, d: &Diagnostics, losses: (u64, Option<u64>)) -> View {
    let mut rows = vec![vec![
        heading("Cable losses"),
        heading("Since reset").horizontal_alignment(HorizontalAlignment::Right),
        heading("Device total").horizontal_alignment(HorizontalAlignment::Right),
    ]];
    for (name, recent, lifetime) in [
        ("Capture underrun frames", Some(losses.0), Some(d.underrun_frames)),
        ("Missing playback packets", losses.1, d.render_lost_packets),
    ] {
        let mut row = vec![label(name)];
        for (period, n, highlight_loss) in [("since reset", recent, true), ("device total", lifetime, false)] {
            let metric = format!("{name}, {period}");
            row.push(match n {
                Some(n) => value(cable, &metric, count(n), highlight_loss && n > 0),
                None => value(cable, &metric, "Unavailable".into(), false),
            });
        }
        rows.push(row);
    }
    table([GridLength::Star(1.2), GridLength::STAR, GridLength::STAR], rows)
}

pub(super) fn route_levels(channels: &[(&str, &str, &Level, bool)], width: f64) -> View {
    if channels.is_empty() {
        return View::empty();
    }
    // Leave space for the card, scroll bar and metric labels. Extra endpoints continue in a new group at narrow window widths.
    let per_group = (((width - 210.0) / 180.0) as usize).clamp(1, channels.len());
    let groups = channels.chunks(per_group).map(|group| {
        let names: Vec<_> = group.iter().map(|(role, name, _, _)| format!("{role} · {name}")).collect();
        let mut header = vec![heading("Frames")];
        for ((_, _, _, offline), name) in group.iter().zip(&names) {
            let cell = if *offline {
                heading(format!("! {name} · Offline")).foreground(ThemeBrush::SystemCritical).automation_name(format!("{name}, offline"))
            } else {
                heading(name)
            };
            header.push(cell.horizontal_alignment(HorizontalAlignment::Right));
        }
        let metric_row = |metric: &str, get: fn(&Level) -> u64, highlight_loss: bool| {
            let mut row = vec![label(metric)];
            let metric = format!("{metric} frames");
            for ((_, _, level, _), name) in group.iter().zip(&names) {
                let n = get(level);
                row.push(value(name, &metric, count(n), highlight_loss && n > 0));
            }
            row
        };
        let rows = vec![
            header,
            metric_row("Packet size", |l| l.period_frames.into(), false),
            metric_row("Processed", |l| l.frames, false),
            metric_row("Startup silence", |l| l.startup_silent_frames, false),
            metric_row("Dropped", |l| l.dropped_frames, true),
            metric_row("Underrun", |l| l.underrun_frames, true),
        ];
        let columns = std::iter::once(GridLength::Pixel(122.0)).chain(std::iter::repeat_n(GridLength::STAR, per_group));
        table(columns, rows)
    });
    col(groups.collect())
}

pub(super) fn effect(stage: &str, reading: StageReading) -> View {
    let mut rows = Vec::new();
    let mut add = |name: &str, text: String, alert| rows.push(vec![label(name), value(stage, name, text, alert)]);
    for (name, level, unit) in
        [("Input peak", reading.input_peak, "dBFS"), ("Output peak", reading.output_peak, "dBFS"), ("Gain", reading.gain, "dB")]
    {
        if let Some(v) = level {
            let db = if v > 0.0 { format!("{:.1}", 20.0 * v.log10()) } else { "−∞".into() };
            add(name, format!("{db} {unit}"), false);
        }
    }
    if let Some(open) = reading.gate_open {
        add("Gate", if open { "Open" } else { "Closed" }.into(), false);
    }
    if let Some(v) = reading.speech_probability {
        add("Speech probability", format!("{:.0}%", v * 100.0), false);
    }
    if let Some(v) = reading.worker_misses {
        add("Worker misses", count(v), v > 0);
    }
    if let Some(v) = reading.echo_delay_ms {
        add("Echo delay", format!("{v:.1} ms"), false);
    }
    if let Some(v) = reading.erle_db {
        add("Echo reduction", format!("{v:.1} dB"), false);
    }
    col(vec![title(stage, 14.0), if rows.is_empty() { caption("Waiting for samples") } else { table([GridLength::STAR; 2], rows) }])
}
