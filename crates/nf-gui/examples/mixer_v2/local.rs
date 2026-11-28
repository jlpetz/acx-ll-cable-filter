//! The preview owns only its appearance file. Opening the live app is a separate, user-triggered action.
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use super::appearance::Appearance;

fn appearance_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").filter(|p| !p.is_empty()).map(|p| PathBuf::from(p).join("noise-filter/mixer-v2/appearance.json"))
}

fn read_appearance(path: &Path) -> Appearance {
    fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

pub(super) fn load_appearance() -> Appearance {
    appearance_path().as_deref().map(read_appearance).unwrap_or_default()
}

pub(super) fn save_appearance(prefs: Appearance) -> Result<(), String> {
    let path = appearance_path().ok_or("Couldn't save appearance because LOCALAPPDATA is missing.")?;
    write_appearance(&path, prefs).map_err(|e| format!("Couldn't save appearance to {} ({e}).", path.display()))
}

fn write_appearance(path: &Path, prefs: Appearance) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(&prefs).map_err(io::Error::other)?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    // A same-directory rename publishes the complete file. Two open previews must never share a temporary file.
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let (temp, mut file) = loop {
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let temp = path.with_extension(format!("{}.{}.tmp", std::process::id(), nonce));
        match File::create_new(&temp) {
            Ok(file) => break (temp, file),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };
    let flushed = file.write_all(&json).and_then(|()| file.sync_all());
    drop(file);
    flushed.and_then(|()| fs::rename(&temp, path)).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

fn live_path(preview: &Path) -> Option<PathBuf> {
    let dir = preview.parent()?;
    // Cargo builds examples in <profile>/examples; a distributed preview can sit beside the main executable.
    let dir = if dir.ends_with("examples") { dir.parent()? } else { dir };
    Some(dir.join("noise-filter.exe"))
}

pub(super) fn open_live() -> Result<(), String> {
    let preview = std::env::current_exe().map_err(|e| format!("Couldn't locate the running prototype ({e})."))?;
    let exe = live_path(&preview).ok_or("Couldn't locate the live app beside the prototype.")?;
    Command::new(&exe).arg("gui").spawn().map(drop).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => {
            format!("{} is missing. Run `cargo run -p nf-app -- gui` from the project to open live settings.", exe.display())
        }
        _ => format!("Couldn't open {} ({e}).", exe.display()),
    })
}

#[cfg(test)]
mod tests {
    use std::os::windows::fs::OpenOptionsExt;

    use super::*;
    use crate::appearance::ThemeChoice;
    use crate::meters::{MeterMode, MeterPalette};

    #[test]
    fn saves_replace_atomically_and_a_locked_destination_keeps_its_previous_preferences() {
        let mut number = 0;
        let dir = loop {
            let dir = std::env::temp_dir().join(format!("nf-mixer-v2-{}-{number}", std::process::id()));
            match fs::create_dir(&dir) {
                Ok(()) => break dir,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => number += 1,
                Err(e) => panic!("create test directory: {e}"),
            }
        };
        let path = dir.join("appearance.json");
        assert_eq!(read_appearance(&path), Appearance::default());
        fs::write(&path, br#"{"compact":false}"#).unwrap();
        assert_eq!(read_appearance(&path), Appearance { compact: false, ..Default::default() });
        fs::write(&path, br#"{"palette":"classic"}"#).unwrap();
        assert_eq!(read_appearance(&path), Appearance { palette: MeterPalette::Classic, ..Default::default() });
        fs::write(&path, b"partial JSON").unwrap();
        assert_eq!(read_appearance(&path), Appearance::default());

        let chosen = Appearance { theme: ThemeChoice::Dark, compact: true, mode: MeterMode::Stereo, palette: MeterPalette::BlueFloor };
        write_appearance(&path, chosen).unwrap();
        assert_eq!(read_appearance(&path), chosen);
        let locked = File::options().read(true).share_mode(0).open(&path).unwrap();
        assert!(write_appearance(&path, Appearance::default()).is_err());
        drop(locked);
        assert_eq!(read_appearance(&path), chosen);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_file(path).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn launch_uses_the_matching_cargo_profile_or_a_colocated_app() {
        let target = Path::new("build").join("custom-target").join("debug");
        assert_eq!(live_path(&target.join("examples/mixer_v2.exe")), Some(target.join("noise-filter.exe")));
        let portable = Path::new("apps").join("audio");
        assert_eq!(live_path(&portable.join("mixer_v2.exe")), Some(portable.join("noise-filter.exe")));
    }
}
