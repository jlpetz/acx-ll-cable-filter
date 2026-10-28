//! nf-gui: the Reactor (WinUI 3) settings window (TODO #15) and settings persistence.
//!
//! - [`run`] loads the settings, opens the window and blocks until it closes; then it flushes the
//!   settings and stops the engine (gui.md; `doc/dev/api/windows-reactor.md` for the API and its
//!   pitfalls).
//! - `window.rs` is the window and its state, `live.rs` the meters and the live readout (a child
//!   component with its own 20 Hz waiter), `settings.rs` the JSON file and its saver thread,
//!   `devices.rs` the device pickers' items, `format.rs` every label, `meter.rs` the level bars,
//!   `screen.rs` the window's size against the monitor's work area, and `widgets.rs` the shared
//!   view pieces.
//!
//! Threads: the window runs on the caller's thread, which `App::run` makes an STA. Blocking engine
//! calls run on the Windows thread pool (`spawn_background`), and the settings are written by an
//! `nf-settings` thread. Nothing here runs on, or waits for, an audio thread: the engine's
//! telemetry is read from its atomics.

#![cfg_attr(not(test), warn(clippy::unwrap_used))]

mod devices;
mod format;
mod live;
mod meter;
mod screen;
mod settings;
mod widgets;
mod window;

use std::path::PathBuf;
use std::sync::Arc;

use nf_engine::Engine;
use windows_reactor::{App, ComponentTask, ComponentTaskStatus};

/// Opens the window and runs until it closes. `settings_path` is the JSON settings file (created on
/// first save). The engine is owned by the caller; the window starts and stops it.
///
/// Call it on a thread with no COM apartment, or an STA: WinUI fails on an MTA thread
/// (windows-reactor.md pitfall 3). The error is Reactor's, as text (e.g. a missing Windows App
/// Runtime, for which Reactor has already shown its own download prompt).
pub fn run(engine: Arc<Engine>, settings_path: PathBuf) -> Result<(), String> {
    let loaded = settings::load(&settings_path);
    if let Some(warning) = &loaded.warning {
        log::warn!("{warning}");
    }
    // A file that's there but unreadable must survive the session: saving would replace it
    let saver = if loaded.read_only {
        settings::Saver::disabled(settings_path)
    } else {
        settings::Saver::spawn(settings_path, settings::SAVE_DEBOUNCE)
    };
    let start = window::Start {
        engine: SharedEngine(Arc::clone(&engine)),
        settings: loaded.settings,
        warning: loaded.warning,
        saver: saver.handle(),
    };
    let shown = App::run_component::<window::Window>(start).map_err(|e| e.to_string());
    // The last change is written before anything that could hang
    saver.finish();
    // Reactor has no close-cancel, and a waiter may still hold an `Arc<Engine>` on a pool thread, so
    // the last reference may drop there: stop explicitly rather than rely on `Drop` (pitfall 8).
    engine.stop();
    shown
}

/// The settings file's usual place: `%APPDATA%\noise-filter\settings.json`. `None` if `APPDATA`
/// isn't set.
pub fn default_settings_path() -> Option<PathBuf> {
    let app_data = std::env::var_os("APPDATA").filter(|v| !v.is_empty())?;
    Some(PathBuf::from(app_data).join("noise-filter").join("settings.json"))
}

/// The engine handle in a component's `Input`, which must be `PartialEq`: equal when it's the same
/// engine.
#[derive(Clone)]
pub(crate) struct SharedEngine(pub(crate) Arc<Engine>);

impl PartialEq for SharedEngine {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Whether a waiter is still in flight. A delivered, cancelled or rejected one has ended its chain,
/// and nothing arrives to re-arm it (windows-reactor.md pitfall 7).
pub(crate) fn waiting(task: Option<&ComponentTask>) -> bool {
    task.is_some_and(|t| matches!(t.status(), ComponentTaskStatus::Running | ComponentTaskStatus::Queued))
}

// `spawn_background` needs `Send` messages: the whole enum crosses from the thread pool.
const _: () = {
    const fn send<T: Send>() {}
    send::<window::Msg>();
    send::<live::LiveMsg>();
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_settings_path_is_under_appdata() {
        // %APPDATA% is set in any interactive or service session on Windows
        if let Some(path) = default_settings_path() {
            assert!(path.ends_with(r"noise-filter\settings.json"), "{}", path.display());
            assert_eq!(path.parent().and_then(|p| p.parent()), std::env::var_os("APPDATA").as_deref().map(std::path::Path::new));
        }
    }
}
