//! Standalone cable-owned feed broker. Launched on an explicit control-panel or SDK approval.
#![windows_subsystem = "windows"]

fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(nf_vmic::broker::helper_entry().unwrap_or(2))
}
