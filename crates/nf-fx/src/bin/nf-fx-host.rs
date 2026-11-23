//! Standalone worker for hosts other than noise-filter.exe, including integration tests.
fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(nf_fx::worker_entry().unwrap_or(2))
}
