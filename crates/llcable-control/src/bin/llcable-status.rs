//! Read-only cable control-panel CLI.
fn main() -> std::process::ExitCode {
    let cables = match llcable_control::cables() {
        Ok(cables) => cables,
        Err(e) => {
            eprintln!("Cable discovery failed: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    if cables.is_empty() {
        println!("No LLCables installed.");
    }
    let mut status = std::process::ExitCode::SUCCESS;
    for cable in cables {
        println!("LLCable-{} ({})", cable.label, cable.health);
        for port in &cable.ports {
            println!("  {}{}", port.name, if port.active { "" } else { " (inactive)" });
        }
        if cable.ports.iter().any(|p| p.active) {
            match llcable_control::diagnostics(cable.id) {
                Ok(d) => println!(
                    "  feed {}, {} capture streams open, direct reader {} frames, worst capture timer {} µs\n  Capture underrun {} frames · Missing playback packets {}",
                    if d.attached { "attached" } else { "idle" },
                    d.capture_streams,
                    d.packet_frames,
                    d.timer_late_max_us,
                    d.underrun_frames,
                    d.render_lost_packets.map_or_else(|| "unavailable on this driver".to_owned(), |n| n.to_string())
                ),
                Err(e) => {
                    eprintln!("  Diagnostics unavailable: {e}");
                    status = std::process::ExitCode::FAILURE;
                }
            }
        }
    }
    status
}
