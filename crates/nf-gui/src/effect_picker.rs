//! The shell's CLAP file picker. Run the modal dialog in its own STA so its message loop never re-enters Reactor's update.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR, FOS_PATHMUSTEXIST, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
};
use windows::core::{HRESULT, w};

/// The active window on the GUI thread. An HWND is valid across threads in this process.
pub(crate) fn owner() -> usize {
    // SAFETY: GetActiveWindow has no preconditions.
    unsafe { GetActiveWindow().0 as usize }
}

pub(crate) fn choose(owner: usize) -> Result<Option<PathBuf>, String> {
    std::thread::Builder::new()
        .name("nf-effect-picker".into())
        .spawn(move || {
            // SAFETY: this new thread has no COM apartment. Drop the shell interfaces before balancing the successful initialization.
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok()?;
            let path = show(owner);
            // SAFETY: this balances the successful call above on the same thread.
            unsafe { CoUninitialize() };
            path
        })
        .map_err(|e| e.to_string())?
        .join()
        .map_err(|_| "The effect picker stopped unexpectedly.".to_string())?
        .map_err(|e| e.to_string())
}

fn show(owner: usize) -> windows::core::Result<Option<PathBuf>> {
    // SAFETY: COM is initialized on this thread. Literal filter strings outlive the dialog, and the shell owns the returned
    // display name until we copy it and free its allocation. No COM pointer is passed to another thread.
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
        dialog.SetOptions(dialog.GetOptions()? | FOS_FILEMUSTEXIST | FOS_PATHMUSTEXIST | FOS_FORCEFILESYSTEM | FOS_NOCHANGEDIR)?;
        dialog.SetFileTypes(&[COMDLG_FILTERSPEC { pszName: w!("CLAP effects"), pszSpec: w!("*.clap") }])?;
        dialog.SetTitle(w!("Choose a CLAP effect"))?;
        match dialog.Show((owner != 0).then_some(HWND(owner as _))) {
            Err(e) if e.code() == HRESULT::from_win32(ERROR_CANCELLED.0) => return Ok(None),
            result => result?,
        }
        let name = dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?;
        let path = PathBuf::from(OsString::from_wide(name.as_wide()));
        CoTaskMemFree(Some(name.0.cast()));
        Ok(Some(path))
    }
}
