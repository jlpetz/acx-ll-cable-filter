//! In-memory signal graph for comparing the three editors against exactly the same session.
use std::collections::BTreeMap;

pub(super) const SOURCES: [&str; 3] = ["Studio mic", "Cable C playback", "Cable D playback"];
pub(super) const SOURCE_INFO: [&str; 3] = ["Microphone input", "What apps play to Cable C", "What apps play to Cable D"];
pub(super) const DESTS: [&str; 3] = ["Cable A", "Cable B", "Headphones"];
pub(super) const DEST_INFO: [&str; 3] = ["OBS recording", "Discord microphone", "Stereo monitor"];

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) enum Scope {
    Source(usize),
    Send(usize, usize),
    Output(usize),
}

impl Scope {
    pub(super) fn title(self) -> String {
        match self {
            Self::Source(s) => format!("{} input", SOURCES[s]),
            Self::Send(s, d) => format!("{} → {}", SOURCES[s], DESTS[d]),
            Self::Output(d) => format!("{} mix", DESTS[d]),
        }
    }

    pub(super) fn description(self) -> &'static str {
        match self {
            Self::Source(_) => "Shared input effects. Each send chooses where it takes the signal.",
            Self::Send(_, _) => "Only this copy of the signal passes through these stages.",
            Self::Output(_) => "Runs after the incoming sends have been mixed.",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Echo,
    Noise,
    Gate,
    Equalizer,
    Pan,
    Voice,
    Gain,
    Compressor,
}

impl Kind {
    pub(super) const ALL: [Self; 8] =
        [Self::Echo, Self::Noise, Self::Gate, Self::Equalizer, Self::Pan, Self::Voice, Self::Gain, Self::Compressor];

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Echo => "Echo cancellation",
            Self::Noise => "Noise reduction",
            Self::Gate => "Noise gate",
            Self::Equalizer => "Equalizer",
            Self::Pan => "Stereo pan",
            Self::Voice => "Voice tone",
            Self::Gain => "Gain",
            Self::Compressor => "Compressor",
        }
    }

    pub(super) fn family(self) -> &'static str {
        match self {
            Self::Echo => "Built-in · WebRTC AEC3",
            Self::Noise => "Built-in · RNNoise",
            Self::Gate => "Built-in · dynamics",
            Self::Pan | Self::Equalizer => "Built-in concept",
            Self::Voice | Self::Gain | Self::Compressor => "CLAP · example plug-in",
        }
    }

    pub(super) fn param(self) -> &'static str {
        match self {
            Self::Echo => "Echo reference",
            Self::Noise => "Suppression limit",
            Self::Gate | Self::Compressor => "Threshold",
            Self::Equalizer => "Low shelf at 160 Hz",
            Self::Pan => "Pan",
            Self::Voice => "Pitch",
            Self::Gain => "Gain",
        }
    }

    pub(super) fn range(self) -> (i32, i32) {
        match self {
            Self::Echo => (0, 2),
            Self::Noise => (0, 60),
            Self::Gate | Self::Compressor => (-80, 0),
            Self::Equalizer | Self::Voice => (-12, 12),
            Self::Pan => (-100, 100),
            Self::Gain => (-36, 12),
        }
    }

    fn default_value(self) -> i32 {
        match self {
            Self::Gate => -50,
            Self::Compressor => -18,
            Self::Noise => 36,
            Self::Voice => -3,
            _ => 0,
        }
    }

    pub(super) fn format(self, value: i32) -> String {
        match self {
            Self::Pan if value == 0 => "Centre".into(),
            Self::Pan => format!("{}% {}", value.abs(), if value < 0 { "left" } else { "right" }),
            Self::Voice => format!("{value:+} semitones"),
            Self::Gate | Self::Compressor => format!("{value} dBFS"),
            Self::Noise => format!("{value} dB"),
            _ => format!("{value:+} dB"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Stage {
    pub(super) id: u32,
    pub(super) kind: Kind,
    pub(super) enabled: bool,
    pub(super) value: i32,
}

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tap {
    Raw,
    Stage(u32),
    #[default]
    End,
}

#[derive(Default, Clone, Copy)]
pub(super) struct Send {
    pub(super) connected: bool,
    pub(super) tap: Tap,
    pub(super) gain_db: i32,
}

pub(super) struct Scene {
    pub(super) scope: Scope,
    pub(super) sends: [[Send; DESTS.len()]; SOURCES.len()],
    chains: BTreeMap<Scope, Vec<Stage>>,
    next_id: u32,
}

impl Default for Scene {
    fn default() -> Self {
        let mut scene = Self { scope: Scope::Send(0, 1), sends: Default::default(), chains: BTreeMap::new(), next_id: 1 };
        for (s, d) in [(0, 0), (0, 1), (1, 0)] {
            scene.sends[s][d].connected = true;
        }
        scene.sends[0][0].tap = Tap::Raw;
        scene.sends[1][0].gain_db = -9;
        scene.add(Kind::Echo);
        scene.add(Kind::Noise);
        scene
    }
}

impl Scene {
    pub(super) fn connected(&self, s: usize, d: usize) -> bool {
        self.sends[s][d].connected
    }

    pub(super) fn at(&self, scope: Scope) -> &[Stage] {
        self.chains.get(&scope).map_or(&[], Vec::as_slice)
    }

    pub(super) fn stages(&self) -> &[Stage] {
        self.at(self.scope)
    }

    pub(super) fn toggle(&mut self, s: usize, d: usize) {
        let send = &mut self.sends[s][d];
        send.connected = !send.connected;
        if !send.connected && self.scope == Scope::Send(s, d) {
            self.scope = Scope::Output(d);
        }
    }

    fn append(&mut self, kind: Kind, value: i32, enabled: bool) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.chains.entry(self.scope).or_default().push(Stage { id, kind, value, enabled });
        id
    }

    pub(super) fn add(&mut self, kind: Kind) -> u32 {
        self.append(kind, kind.default_value(), true)
    }

    pub(super) fn stage_mut(&mut self, id: u32) -> Option<&mut Stage> {
        self.chains.get_mut(&self.scope)?.iter_mut().find(|s| s.id == id)
    }

    pub(super) fn move_stage(&mut self, id: u32, step: i8) {
        if let Some(stages) = self.chains.get_mut(&self.scope)
            && let Some(index) = stages.iter().position(|s| s.id == id)
            && let Some(target) = index.checked_add_signed(isize::from(step)).filter(|i| *i < stages.len())
        {
            stages.swap(index, target);
        }
    }

    pub(super) fn remove(&mut self, id: u32) {
        if let Some(stages) = self.chains.get_mut(&self.scope)
            && let Some(index) = stages.iter().position(|s| s.id == id)
        {
            if let Scope::Source(source) = self.scope {
                // A send tapped after this stage stays at the preceding boundary if its stage is removed.
                let before = index.checked_sub(1).map_or(Tap::Raw, |i| Tap::Stage(stages[i].id));
                for send in &mut self.sends[source] {
                    if send.tap == Tap::Stage(id) {
                        send.tap = before;
                    }
                }
            }
            stages.remove(index);
        }
    }

    pub(super) fn load_stack(&mut self, saved: &[Stage]) {
        while let Some(stage) = self.stages().last().copied() {
            self.remove(stage.id);
        }
        for stage in saved {
            self.append(stage.kind, stage.value, stage.enabled);
        }
    }

    pub(super) fn taps(&self, s: usize) -> Vec<(Tap, String)> {
        let mut options = vec![(Tap::Raw, "Original input (dry)".into()), (Tap::End, "After all input stages".into())];
        options.extend(
            self.at(Scope::Source(s))
                .iter()
                .enumerate()
                .map(|(i, stage)| (Tap::Stage(stage.id), format!("After {} · {}", i + 1, stage.kind.name()))),
        );
        options
    }
}
