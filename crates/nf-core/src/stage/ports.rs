//! Audio ports (`clap.audio-ports`): one main input and one main output at index 0, plus at most one non-main input, the
//! sidechain. The v0 host feeds that one the speaker reference on the mic's clock, as the chain receives it: not delayed
//! by the slots upstream (see [`crate::chain`]). Mono only in v0. No in-place pairs.

use std::borrow::Cow;

use super::{Text, text};

/// CLAP's `CLAP_PORT_MONO`.
pub const PORT_MONO: &str = "mono";

/// One port (`clap_audio_port_info`, without `in_place_pair`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioPortInfo {
    /// Stable per direction.
    pub id: u32,
    pub name: Text,
    /// `CLAP_AUDIO_PORT_IS_MAIN`: only port 0 of each direction may be.
    pub main: bool,
    pub channel_count: u32,
    /// [`PORT_MONO`] in v0.
    pub port_type: Text,
}

impl AudioPortInfo {
    /// A main mono port.
    pub const fn main_mono(id: u32, name: &'static str) -> Self {
        Self { id, name: text(name), main: true, channel_count: 1, port_type: text(PORT_MONO) }
    }

    /// A non-main mono port.
    pub const fn aux_mono(id: u32, name: &'static str) -> Self {
        Self { id, name: text(name), main: false, channel_count: 1, port_type: text(PORT_MONO) }
    }
}

/// A stage's ports, inputs and outputs each in port order: borrowed from a table for our stages, owned for one whose ports
/// are known only at run time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioPorts {
    pub inputs: Cow<'static, [AudioPortInfo]>,
    pub outputs: Cow<'static, [AudioPortInfo]>,
}

impl AudioPorts {
    /// How many input blocks `process` gets: one per input port.
    pub fn input_count(&self) -> usize {
        self.inputs.len()
    }
}

/// One mono input, one mono output: a noise filter or an effect.
pub static MONO_IN_OUT: AudioPorts = AudioPorts {
    inputs: Cow::Borrowed(&[AudioPortInfo::main_mono(0, "Input")]),
    outputs: Cow::Borrowed(&[AudioPortInfo::main_mono(0, "Output")]),
};

/// An echo canceller's: the mic on the main input, the speaker reference on input 1 (non-main).
pub static MONO_WITH_REFERENCE: AudioPorts = AudioPorts {
    inputs: Cow::Borrowed(&[AudioPortInfo::main_mono(0, "Mic"), AudioPortInfo::aux_mono(1, "Speaker reference")]),
    outputs: Cow::Borrowed(&[AudioPortInfo::main_mono(0, "Echo-cancelled mic")]),
};
