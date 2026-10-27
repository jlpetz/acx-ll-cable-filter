//! Exit codes (app.md) and the failure a command returns, worded for the user.

/// The process's exit code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Ok = 0,
    /// The command line doesn't parse, or misses something it needs.
    Usage = 1,
    /// The engine, a device or a file failed.
    Error = 2,
    /// The latency test ran but found no valid repetition.
    NoResult = 3,
    /// The large-page helper (`setup-large-pages --elevated-helper`) started without elevation,
    /// e.g. with UAC turned off. Its caller reads this code (`setup::helper_result`).
    NotElevated = 4,
}

impl Exit {
    pub fn code(self) -> u8 {
        self as u8
    }
}

// The window's large-page button reads the helper's code through nf-mem's constant.
const _: () = assert!(Exit::NotElevated as u32 == nf_mem::privilege::HELPER_NOT_ELEVATED);

/// Why a command stopped, and with which code.
#[derive(Clone, Debug, PartialEq)]
pub struct Failure {
    pub exit: Exit,
    pub message: String,
}

impl Failure {
    pub fn usage(message: impl Into<String>) -> Self {
        Self { exit: Exit::Usage, message: message.into() }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self { exit: Exit::Error, message: message.into() }
    }

    pub fn no_result(message: impl Into<String>) -> Self {
        Self { exit: Exit::NoResult, message: message.into() }
    }

    pub fn not_elevated(message: impl Into<String>) -> Self {
        Self { exit: Exit::NotElevated, message: message.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_codes_are_app_mds() {
        assert_eq!([Exit::Ok, Exit::Usage, Exit::Error, Exit::NoResult, Exit::NotElevated].map(Exit::code), [0, 1, 2, 3, 4]);
        assert_eq!(Failure::usage("u").exit, Exit::Usage);
        assert_eq!(Failure::error("e").exit, Exit::Error);
        assert_eq!(Failure::no_result("n").exit, Exit::NoResult);
        assert_eq!(Failure::not_elevated("x").exit, Exit::NotElevated);
    }
}
