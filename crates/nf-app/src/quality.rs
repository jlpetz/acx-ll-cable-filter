//! The quality test's pure half (`noise-filter quality`, TODO #14: Lab phase 0's live half). [`signals`] builds the scene it
//! plays and [`analysis`] measures what comes back. Neither touches a device, so both are unit-tested here; the command
//! (`commands/quality.rs`) runs the engine, plays the scene and records the result.

pub mod analysis;
pub mod signals;

/// Which scene the quality test plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QualityTest {
    /// Near speech in noise: what the noise filter does to the noise in pauses, and to the speech.
    Noise,
    /// Near speech, the echo of a far voice and low noise, with the far voice on the far-end device (the engine's echo
    /// reference): what echo cancellation does.
    Echo,
}

impl QualityTest {
    pub fn name(self) -> &'static str {
        match self {
            QualityTest::Noise => "noise",
            QualityTest::Echo => "echo",
        }
    }
}
