//! LLCable's Windows control panel and live mixer. Opening either view only enumerates devices.
#![cfg_attr(not(test), windows_subsystem = "windows")]
#![warn(clippy::undocumented_unsafe_blocks)]

#[path = "../../nf-app/src/cpu_check.rs"]
mod cpu_check;
mod desk;
mod graph;
mod patch;
mod window;

use std::ffi::CStr;
use std::process::ExitCode;

use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxA};
use windows::core::PCSTR;
use windows_reactor::App;

fn show_error(text: &CStr) {
    // SAFETY: both strings are NUL-terminated and remain valid until the modal dialog returns.
    unsafe { MessageBoxA(None, PCSTR(text.as_ptr().cast()), windows::core::s!("LLCable Mixer"), MB_ICONERROR | MB_OK) };
}

fn main() -> ExitCode {
    if let Some(code) = cpu_check::gate(Some(show_error)) {
        return ExitCode::from(code);
    }
    app_main()
}

#[inline(never)]
fn app_main() -> ExitCode {
    let page = match std::env::args().nth(1).as_deref() {
        None | Some("--cables") => window::Page::Cables,
        Some("--mixer") => window::Page::Mixer,
        _ => {
            eprintln!("Usage: llcable-mixer [--cables | --mixer]");
            return ExitCode::FAILURE;
        }
    };
    match App::run_component::<window::Mixer>(page) {
        Ok(_) => ExitCode::SUCCESS,
        Err(err) => {
            if let Ok(text) = std::ffi::CString::new(err.to_string()) {
                show_error(&text);
            }
            ExitCode::FAILURE
        }
    }
}
