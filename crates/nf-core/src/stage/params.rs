//! Parameters (`clap.params`): what a stage declares ([`ParamInfo`]) and how a value is checked before it is stored
//! ([`ParamInfo::accept`]). No automation or modulation flags in v0.
//!
//! **Who sets them.** [`super::Stage::set_param`] (`&mut self`) is for whoever holds the stage, the in-process stand-in for
//! `CLAP_EVENT_PARAM_VALUE`: while the stage is active, the host calls it on the processing thread before a block's
//! `process`, where CLAP delivers its in-events, so it never runs during `process`. The chain owns its stages on the
//! audio thread, so no other thread can call it.
//!
//! **Values from another thread** (the settings window, the control thread) reach a stage through its host. In v0 that is
//! [`crate::controls::ChainControls`]: the noise stage's params are views of its atomics, which `ChainControls::apply`
//! writes from any thread (unclamped), so `param_value` sees the GUI's changes and `set_param` writes the same store.
//! A stage whose params are its own (an effect, a model pack's `controls[]`) has no such path yet. The plan (ours, for
//! TODO 15 or the first effect): a lock-free single-producer queue of (slot, id, value) that `Chain::process` drains into
//! each slot's `set_param` before running it.

use super::Text;

/// A parameter's stable id (`clap_id`).
pub type ParamId = u32;

/// `clap_param_info_flags`, with CLAP's bit values (`ext/params.h`; recheck before the `.clap` export).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ParamFlags(pub u32);

impl ParamFlags {
    pub const NONE: Self = Self(0);
    /// `CLAP_PARAM_IS_STEPPED`: integral values only.
    pub const STEPPED: Self = Self(1 << 0);
    /// `CLAP_PARAM_IS_HIDDEN`.
    pub const HIDDEN: Self = Self(1 << 2);
    /// `CLAP_PARAM_IS_READONLY`.
    pub const READONLY: Self = Self(1 << 3);
    /// `CLAP_PARAM_IS_ENUM`: each step is a named choice (with `STEPPED`).
    pub const ENUM: Self = Self(1 << 16);

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// One parameter (`clap_param_info`, plus a display unit; no cookie). Its text is [`Text`]: literals for our stages'
/// tables, owned for params known only at run time (a model pack's `controls[]`).
#[derive(Clone, Debug, PartialEq)]
pub struct ParamInfo {
    pub id: ParamId,
    pub name: Text,
    /// CLAP's module path for grouping, e.g. `"Gate"`; empty for none.
    pub module: Text,
    /// Display unit, e.g. `"dB"`; empty for none.
    pub unit: Text,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    pub flags: ParamFlags,
}

impl ParamInfo {
    /// The value to store for a requested `v`: NaN is refused, anything else clamped to the range, and a stepped value
    /// truncated toward zero. A read-only param refuses every value.
    #[inline]
    pub fn accept(&self, v: f64) -> Result<f64, ParamError> {
        if self.flags.contains(ParamFlags::READONLY) {
            return Err(ParamError::ReadOnly(self.id));
        }
        if v.is_nan() {
            return Err(ParamError::NotANumber(self.id));
        }
        let v = v.clamp(self.min, self.max);
        Ok(if self.flags.contains(ParamFlags::STEPPED) { v.trunc() } else { v })
    }
}

/// Why [`super::Stage::set_param`] refused a value. `Copy` and allocation-free, so the call stays real-time safe even when
/// it fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamError {
    UnknownId(ParamId),
    NotANumber(ParamId),
    ReadOnly(ParamId),
}

impl std::fmt::Display for ParamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParamError::UnknownId(id) => write!(f, "no param has id {id}"),
            ParamError::NotANumber(id) => write!(f, "param {id}: NaN is not a value"),
            ParamError::ReadOnly(id) => write!(f, "param {id} is read-only"),
        }
    }
}

impl std::error::Error for ParamError {}

impl From<ParamError> for super::StageError {
    fn from(e: ParamError) -> Self {
        super::StageError(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::super::text;
    use super::*;

    const fn info(min: f64, max: f64, flags: ParamFlags) -> ParamInfo {
        ParamInfo { id: 7, name: text("p"), module: text(""), unit: text(""), min, max, default: min, flags }
    }

    #[test]
    fn flags_have_clap_s_bits() {
        assert_eq!((ParamFlags::STEPPED.0, ParamFlags::HIDDEN.0, ParamFlags::READONLY.0, ParamFlags::ENUM.0), (1, 4, 8, 65536));
        let f = ParamFlags::STEPPED.union(ParamFlags::ENUM);
        assert!(f.contains(ParamFlags::STEPPED) && f.contains(ParamFlags::ENUM) && !f.contains(ParamFlags::HIDDEN));
        assert!(f.contains(ParamFlags::NONE));
    }

    #[test]
    fn a_value_is_clamped_truncated_or_refused() {
        let p = info(-120.0, 0.0, ParamFlags::NONE);
        assert_eq!((p.accept(-200.0), p.accept(3.0), p.accept(-6.5)), (Ok(-120.0), Ok(0.0), Ok(-6.5)));
        assert_eq!(p.accept(f64::NEG_INFINITY), Ok(-120.0));
        assert_eq!(p.accept(f64::NAN), Err(ParamError::NotANumber(7)));
        let s = info(0.0, 2.0, ParamFlags::STEPPED);
        assert_eq!((s.accept(1.7), s.accept(5.0), s.accept(-0.4)), (Ok(1.0), Ok(2.0), Ok(0.0)));
        let r = info(0.0, 1.0, ParamFlags::READONLY);
        assert_eq!(r.accept(0.5), Err(ParamError::ReadOnly(7)));
        assert_eq!(super::super::StageError::from(ParamError::UnknownId(3)).0, "no param has id 3");
    }
}
