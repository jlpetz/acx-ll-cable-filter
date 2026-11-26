//! `devices`: the active recording and playback endpoints, with their ids for `--mic`/`--output`.

use std::fmt::Write as _;

use nf_engine::{DeviceInfo, Flow};

use crate::exit::Failure;

pub fn devices() -> Result<(), Failure> {
    let list = nf_engine::list_devices().map_err(|e| Failure::error(format!("Couldn't list the audio devices: {e}")))?;
    say!("{}", format_devices(&list).trim_end());
    Ok(())
}

/// Both lists: each device's name, mix format and default marks, then its id.
pub fn format_devices(list: &[DeviceInfo]) -> String {
    let mut s = String::new();
    for (flow, title) in [
        (Flow::Capture, "Recording devices (--mic, latency --capture)"),
        (Flow::Render, "Playback devices (--output, --reference, latency --probe)"),
    ] {
        let _ = writeln!(s, "{title}:");
        let mut any = false;
        for d in list.iter().filter(|d| d.flow == flow) {
            any = true;
            let format = if d.sample_rate == 0 { "format unknown".to_owned() } else { format!("{} Hz, {} ch", d.sample_rate, d.channels) };
            let marks: String = [(d.is_default, " [default]"), (d.is_default_communications, " [communications default]")]
                .iter()
                .filter(|(on, _)| *on)
                .map(|(_, mark)| *mark)
                .collect();
            let _ = writeln!(s, "  {}  ({format}){marks}", d.name);
            let _ = writeln!(s, "      {}", d.id);
        }
        if !any {
            let _ = writeln!(s, "  (none)");
        }
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_lists_show_names_formats_marks_and_ids() {
        let list = vec![
            DeviceInfo {
                id: "{0.0.1.00000000}.{mic}".into(),
                cable_selection: None,
                name: "Microphone (USB)".into(),
                flow: Flow::Capture,
                is_default: true,
                is_default_communications: true,
                sample_rate: 48_000,
                channels: 1,
            },
            DeviceInfo {
                id: "{0.0.0.00000000}.{cable}".into(),
                cable_selection: None,
                name: "CABLE Input".into(),
                flow: Flow::Render,
                is_default: false,
                is_default_communications: false,
                sample_rate: 0,
                channels: 0,
            },
        ];
        let text = format_devices(&list);
        assert_eq!(
            text,
            "Recording devices (--mic, latency --capture):\n  Microphone (USB)  (48000 Hz, 1 ch) [default] [communications default]\n      \
             {0.0.1.00000000}.{mic}\n\nPlayback devices (--output, --reference, latency --probe):\n  CABLE Input  (format unknown)\n      \
             {0.0.0.00000000}.{cable}\n\n"
        );
        let empty = format_devices(&[]);
        assert_eq!(empty.matches("(none)").count(), 2);
    }
}
