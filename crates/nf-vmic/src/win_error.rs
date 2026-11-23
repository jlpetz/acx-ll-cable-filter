//! A Windows call that failed, named for the user: the call, the error's constant name and code, and the system's message
//! ("DiInstallDevice failed: ERROR_NO_CATALOG_FOR_OEM_INF (0xE000022F): ..."). The names are a table of the codes the
//! device-install and audio-session paths ([`crate::variant`], [`crate::sessions`]) are known to return, built from the
//! `windows` crate's own constants so no value can be mistyped; another code shows as its number alone.

use std::fmt;

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_PROB_DEVLOADER_NOT_FOUND, CM_PROB_DISABLED, CM_PROB_DRIVER_BLOCKED, CM_PROB_DRIVER_FAILED_LOAD, CM_PROB_DRIVER_SERVICE_KEY_INVALID,
    CM_PROB_FAILED_ADD, CM_PROB_FAILED_DRIVER_ENTRY, CM_PROB_FAILED_INSTALL, CM_PROB_FAILED_POST_START, CM_PROB_FAILED_START,
    CM_PROB_NEED_RESTART, CM_PROB_NOT_CONFIGURED, CM_PROB_NOT_VERIFIED, CM_PROB_PHANTOM, CM_PROB_REINSTALL, CM_PROB_UNSIGNED_DRIVER,
    CM_PROB_WILL_BE_REMOVED, CR_ACCESS_DENIED, CR_FAILURE, CR_INVALID_DEVNODE, CR_INVALID_POINTER, CR_NO_SUCH_DEVNODE, CR_REMOVE_VETOED,
};
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_AUTHENTICODE_TRUST_NOT_ESTABLISHED, ERROR_BAD_PATHNAME, ERROR_CANCELLED, ERROR_DEVICE_IN_USE,
    ERROR_DEVICE_INSTALLER_NOT_READY, ERROR_DEVICE_INTERFACE_ACTIVE, ERROR_DEVICE_NOT_CONNECTED, ERROR_DEVICE_REMOVED,
    ERROR_DEVINST_ALREADY_EXISTS, ERROR_DI_BAD_PATH, ERROR_DI_DO_DEFAULT, ERROR_DI_DONT_INSTALL, ERROR_DRIVER_INSTALL_BLOCKED,
    ERROR_DRIVER_NONNATIVE, ERROR_DRIVER_STORE_ADD_FAILED, ERROR_ELEVATION_REQUIRED, ERROR_FILE_HASH_NOT_IN_CATALOG, ERROR_FILE_NOT_FOUND,
    ERROR_GEN_FAILURE, ERROR_IN_WOW64, ERROR_INF_IN_USE_BY_DEVICES, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_CLASS, ERROR_INVALID_DATA,
    ERROR_INVALID_DEVINST_NAME, ERROR_INVALID_FUNCTION, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, ERROR_INVALID_REG_PROPERTY,
    ERROR_LINE_NOT_FOUND, ERROR_NO_ASSOCIATED_SERVICE, ERROR_NO_CATALOG_FOR_OEM_INF, ERROR_NO_COMPAT_DRIVERS, ERROR_NO_DRIVER_SELECTED,
    ERROR_NO_INF, ERROR_NO_MORE_ITEMS, ERROR_NO_SUCH_DEVINST, ERROR_NOT_AN_INSTALLED_OEM_INF, ERROR_NOT_ENOUGH_MEMORY, ERROR_NOT_FOUND,
    ERROR_NOT_INSTALLED, ERROR_NOT_SUPPORTED, ERROR_ONLY_VALIDATE_VIA_AUTHENTICODE, ERROR_OUTOFMEMORY, ERROR_PATH_NOT_FOUND,
    ERROR_PNP_REGISTRY_ERROR, ERROR_SECTION_NOT_FOUND, ERROR_SERVICE_DISABLED, ERROR_SHARING_VIOLATION,
    ERROR_SIGNATURE_OSATTRIBUTE_MISMATCH, ERROR_SUCCESS_REBOOT_REQUIRED, ERROR_TIMEOUT, ERROR_UNKNOWN_EXCEPTION, ERROR_WRONG_INF_STYLE,
    WIN32_ERROR,
};
use windows::core::HRESULT;

/// `(code, "NAME")` pairs from the constants themselves.
macro_rules! named {
    ($($c:ident),* $(,)?) => { &[$(($c.0, stringify!($c))),*] };
}

/// Win32 and SetupAPI codes (SetupAPI's are 0xE000xxxx, which `HRESULT::from_win32` keeps as they are).
#[rustfmt::skip]
const ERRORS: &[(u32, &str)] = named![
    ERROR_INVALID_FUNCTION, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_ACCESS_DENIED, ERROR_INVALID_HANDLE,
    ERROR_NOT_ENOUGH_MEMORY, ERROR_INVALID_DATA, ERROR_OUTOFMEMORY, ERROR_GEN_FAILURE, ERROR_SHARING_VIOLATION,
    ERROR_NOT_SUPPORTED, ERROR_INVALID_PARAMETER, ERROR_INSUFFICIENT_BUFFER, ERROR_BAD_PATHNAME, ERROR_NO_MORE_ITEMS,
    ERROR_ELEVATION_REQUIRED, ERROR_SERVICE_DISABLED, ERROR_DEVICE_NOT_CONNECTED, ERROR_NOT_FOUND, ERROR_CANCELLED,
    ERROR_TIMEOUT, ERROR_DEVICE_REMOVED, ERROR_DEVICE_IN_USE, ERROR_SUCCESS_REBOOT_REQUIRED,
    ERROR_WRONG_INF_STYLE, ERROR_SECTION_NOT_FOUND, ERROR_LINE_NOT_FOUND,
    ERROR_NO_DRIVER_SELECTED, ERROR_INVALID_DEVINST_NAME, ERROR_INVALID_CLASS, ERROR_DEVINST_ALREADY_EXISTS,
    ERROR_INVALID_REG_PROPERTY, ERROR_NO_INF, ERROR_NO_SUCH_DEVINST, ERROR_DI_DO_DEFAULT, ERROR_DI_BAD_PATH,
    ERROR_NO_ASSOCIATED_SERVICE, ERROR_DEVICE_INTERFACE_ACTIVE, ERROR_NO_COMPAT_DRIVERS, ERROR_DI_DONT_INSTALL,
    ERROR_NO_CATALOG_FOR_OEM_INF, ERROR_DRIVER_NONNATIVE, ERROR_IN_WOW64, ERROR_PNP_REGISTRY_ERROR, ERROR_UNKNOWN_EXCEPTION,
    ERROR_NOT_AN_INSTALLED_OEM_INF, ERROR_INF_IN_USE_BY_DEVICES, ERROR_SIGNATURE_OSATTRIBUTE_MISMATCH,
    ERROR_ONLY_VALIDATE_VIA_AUTHENTICODE, ERROR_DEVICE_INSTALLER_NOT_READY, ERROR_DRIVER_STORE_ADD_FAILED,
    ERROR_DRIVER_INSTALL_BLOCKED, ERROR_FILE_HASH_NOT_IN_CATALOG, ERROR_AUTHENTICODE_TRUST_NOT_ESTABLISHED, ERROR_NOT_INSTALLED,
];

/// Device Manager's problem codes ("Code 28"), from CM_Get_DevNode_Status.
#[rustfmt::skip]
const PROBLEMS: &[(u32, &str)] = named![
    CM_PROB_NOT_CONFIGURED, CM_PROB_DEVLOADER_NOT_FOUND, CM_PROB_FAILED_START, CM_PROB_NOT_VERIFIED, CM_PROB_NEED_RESTART,
    CM_PROB_REINSTALL, CM_PROB_WILL_BE_REMOVED, CM_PROB_DISABLED, CM_PROB_FAILED_INSTALL, CM_PROB_FAILED_ADD,
    CM_PROB_FAILED_DRIVER_ENTRY, CM_PROB_DRIVER_FAILED_LOAD, CM_PROB_DRIVER_SERVICE_KEY_INVALID, CM_PROB_FAILED_POST_START,
    CM_PROB_PHANTOM, CM_PROB_DRIVER_BLOCKED, CM_PROB_UNSIGNED_DRIVER,
];

/// The configuration manager's CONFIGRET codes.
#[rustfmt::skip]
const CONFIGRETS: &[(u32, &str)] = named![
    CR_INVALID_POINTER, CR_INVALID_DEVNODE, CR_NO_SUCH_DEVNODE, CR_FAILURE, CR_REMOVE_VETOED, CR_ACCESS_DENIED,
];

fn lookup(table: &[(u32, &'static str)], code: u32) -> Option<&'static str> {
    table.iter().find(|&&(c, _)| c == code).map(|&(_, name)| name)
}

/// The constant's name for a Win32 or SetupAPI error code, if it's one the table knows.
pub fn error_name(code: u32) -> Option<&'static str> {
    lookup(ERRORS, code)
}

/// The `CM_PROB_*` name for a Device Manager problem code.
pub fn problem_name(code: u32) -> Option<&'static str> {
    lookup(PROBLEMS, code)
}

/// The `CR_*` name for a CONFIGRET.
pub fn configret_name(code: u32) -> Option<&'static str> {
    lookup(CONFIGRETS, code)
}

/// The Win32 code inside a `windows` error: the low word of a FACILITY_WIN32 HRESULT, else the HRESULT's bits (a SetupAPI code
/// such as 0xE0000228 is its own HRESULT).
pub fn win32_code(e: &windows::core::Error) -> u32 {
    WIN32_ERROR::from_error(e).map_or(e.code().0 as u32, |w| w.0)
}

/// "NAME (code)" for a Win32 or SetupAPI code: decimal below 0x10000, hexadecimal above (SetupAPI's and HRESULTs).
pub fn code_text(code: u32) -> String {
    let number = if code < 0x1_0000 { code.to_string() } else { format!("0x{code:08X}") };
    match error_name(code) {
        Some(name) => format!("{name} ({number})"),
        None => format!("error {number}"),
    }
}

/// A Windows call that failed: which, and the error code it gave.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WinError {
    /// The call, as the user may look it up: `DiInstallDevice`, `SetupDiCallClassInstaller(DIF_REGISTERDEVICE)`.
    pub call: &'static str,
    /// The Win32 or SetupAPI code, or an HRESULT's bits for a COM call.
    pub code: u32,
}

impl WinError {
    pub fn new(call: &'static str, e: &windows::core::Error) -> Self {
        Self { call, code: win32_code(e) }
    }

    /// For `map_err`: `.map_err(WinError::of("SetupDiGetClassDevsW"))`.
    pub fn of(call: &'static str) -> impl Fn(windows::core::Error) -> Self {
        move |e| Self::new(call, &e)
    }

    /// The system's message for the code, if it has one.
    pub fn message(&self) -> String {
        HRESULT::from_win32(self.code).message().trim().to_owned()
    }
}

impl fmt::Display for WinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed: {}", self.call, code_text(self.code))?;
        let message = self.message();
        if message.is_empty() { Ok(()) } else { write!(f, ": {message}") }
    }
}

impl std::error::Error for WinError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_named() {
        assert_eq!(error_name(5), Some("ERROR_ACCESS_DENIED"));
        assert_eq!(error_name(1223), Some("ERROR_CANCELLED"));
        assert_eq!(error_name(0xE000_0228), Some("ERROR_NO_COMPAT_DRIVERS"));
        assert_eq!(error_name(0xE000_022F), Some("ERROR_NO_CATALOG_FOR_OEM_INF"));
        assert_eq!(error_name(0xE000_0235), Some("ERROR_IN_WOW64"));
        assert_eq!(error_name(4242), None);
        assert_eq!(problem_name(28), Some("CM_PROB_FAILED_INSTALL"));
        assert_eq!(problem_name(22), Some("CM_PROB_DISABLED"));
        assert_eq!(configret_name(13), Some("CR_NO_SUCH_DEVNODE"));
        assert_eq!(code_text(5), "ERROR_ACCESS_DENIED (5)");
        assert_eq!(code_text(0xE000_0228), "ERROR_NO_COMPAT_DRIVERS (0xE0000228)");
        assert_eq!(code_text(4242), "error 4242");
        assert_eq!(code_text(0x8889_0004), "error 0x88890004");
    }

    #[test]
    fn errors_keep_their_codes_and_name_the_call() {
        // A SetupAPI code stays itself through HRESULT::from_win32 and back
        let e = windows::core::Error::from(ERROR_NO_COMPAT_DRIVERS);
        assert_eq!(win32_code(&e), 0xE000_0228);
        let e = windows::core::Error::from(ERROR_ACCESS_DENIED);
        let w = WinError::new("DiUninstallDevice", &e);
        assert_eq!(w.code, 5);
        let text = w.to_string();
        assert!(text.starts_with("DiUninstallDevice failed: ERROR_ACCESS_DENIED (5)"), "{text}");
        assert!(text.len() > "DiUninstallDevice failed: ERROR_ACCESS_DENIED (5)".len(), "the system's message follows: {text}");
        assert_eq!(WinError::of("X")(e), WinError { call: "X", code: 5 });
    }
}
