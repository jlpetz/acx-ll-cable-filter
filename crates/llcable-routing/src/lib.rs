//! Route one aligned packet through any number of stereo buses. A bus can mix inputs, apply ordered effects and feed later buses.
//! An empty effect chain forwards the packet immediately. Mono sources feed both channels. Stereo balance attenuates the opposite
//! channel and leaves both channels unchanged at the centre. Mixing sums in floating point and leaves headroom and limiting to the caller.
//!
//! [`GraphSpec::build`] allocates all routing buffers on the control thread. [`Router::process`] and [`Router::process_stereo`]
//! accept varying packet lengths, use borrowed data, and perform no I/O, locking or allocation. The host aligns sources to a
//! common sample clock and writes the desired [`Router::output`] slices to devices. One input or bus can supply many destinations.

mod duck;
mod meter;

use std::error::Error;
use std::fmt;

use duck::Ducker;
pub use duck::Ducking;
pub use meter::{StageMeter, StageReading};
use serde::{Deserialize, Serialize};

/// A source index, or an earlier bus in the same graph. A forward bus reference (including feedback) is rejected at build time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tap {
    Input(usize),
    Bus(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Send {
    pub from: Tap,
    /// Linear amplitude. A negative value inverts polarity.
    pub gain: f32,
    /// Stereo balance, from -1 (left only) to +1 (right only). Centre preserves the original signal.
    pub pan: f32,
}

impl Send {
    pub const fn unity(from: Tap) -> Self {
        Self { from, gain: 1.0, pan: 0.0 }
    }
}

/// Serializable built-in effects. Place a ducker on the music bus and select the voice input as its key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum EffectSpec {
    Gain(f32),
    Duck { key: Tap, settings: Ducking },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BusSpec {
    pub sends: Vec<Send>,
    pub effects: Vec<EffectSpec>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GraphSpec {
    pub input_count: usize,
    pub buses: Vec<BusSpec>,
}

impl GraphSpec {
    /// Compile a stored patch for this stream; indexes in `buses` remain the indexes of the output packets.
    pub fn build(&self, sample_rate: u32, max_frames: usize) -> Result<Router, RoutingError> {
        let buses = self.buses.iter().map(|bus| Bus { sends: bus.sends.clone(), effects: bus.effects.iter().map(Effect::from).collect() });
        Router::new(sample_rate, max_frames, self.input_count, buses.collect())
    }
}

/// A caller-supplied packet effect. The host must prepare it before compiling the graph. `process` runs on the audio thread:
/// bounded work, no allocations, locks or blocking. Each instance sees the preceding stage's output.
pub trait PacketEffect: std::marker::Send + std::any::Any {
    /// Some voice processors operate only at one rate. The router verifies this before the first packet.
    fn required_sample_rate(&self) -> Option<u32> {
        None
    }

    /// Prepare any thread-local scratch after moving this processor to its final thread, before entering real time.
    fn prepare_thread(&mut self) {}

    /// A block-based effect can retain the meter at its native hop boundary. Otherwise the router measures packet peaks.
    fn set_meter(&mut self, _meter: &StageMeter) -> bool {
        false
    }

    /// During graph construction, two adjacent effects with the same block interface may share framing. Return `true`
    /// only after taking ownership of `next`'s processing state. A keyed `next` is always attached independently.
    fn append_compatible(&mut self, _next: &mut dyn PacketEffect) -> bool {
        false
    }

    /// `sidechain` is an aligned, read-only packet when this instance is routed as [`Effect::Sidechain`].
    fn process(&mut self, stereo: &mut [[f32; 2]], sidechain: Option<Input<'_>>);
}

pub enum Effect {
    Gain(f32),
    Duck {
        key: Tap,
        settings: Ducking,
    },
    Custom(Box<dyn PacketEffect>),
    Sidechain {
        key: Tap,
        effect: Box<dyn PacketEffect>,
    },
    #[doc(hidden)]
    Observed(Box<Effect>, StageMeter),
}

impl Effect {
    /// Attach a UI-readable meter during graph construction. Only this stage pays for the requested observations.
    pub fn observe(self) -> (Self, StageMeter) {
        if let Self::Observed(_, meter) = &self {
            let meter = meter.clone();
            return (self, meter);
        }
        let meter = StageMeter::default();
        (Self::Observed(Box::new(self), meter.clone()), meter)
    }

    /// The optional reference/sidechain for loop validation.
    pub fn key(&self) -> Option<Tap> {
        match self {
            Self::Duck { key, .. } | Self::Sidechain { key, .. } => Some(*key),
            Self::Observed(effect, _) => effect.key(),
            _ => None,
        }
    }
}

impl From<&EffectSpec> for Effect {
    fn from(spec: &EffectSpec) -> Self {
        match *spec {
            EffectSpec::Gain(gain) => Self::Gain(gain),
            EffectSpec::Duck { key, settings } => Self::Duck { key, settings },
        }
    }
}

/// Programmatic form of [`BusSpec`] for hosts that also provide their own effect instances.
#[derive(Default)]
pub struct Bus {
    pub sends: Vec<Send>,
    pub effects: Vec<Effect>,
}

#[derive(Clone, Copy)]
pub enum Input<'a> {
    Mono(&'a [f32]),
    Stereo(&'a [[f32; 2]]),
    /// Represents a disconnected source without allocating a zero packet.
    Silence(usize),
}

impl Input<'_> {
    fn frames(self) -> usize {
        match self {
            Input::Mono(x) => x.len(),
            Input::Stereo(x) => x.len(),
            Input::Silence(n) => n,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoutingError {
    InvalidCapacity,
    InvalidSampleRate,
    InvalidSend { bus: usize },
    InvalidEffect { bus: usize },
    EffectSampleRate { bus: usize, required: u32 },
    InvalidTap { bus: usize, tap: Tap },
    InputCount,
    UnequalFrames,
    TooManyFrames,
}

impl fmt::Display for RoutingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCapacity => f.write_str("routing packet capacity must be positive and fit in memory"),
            Self::InvalidSampleRate => f.write_str("routing sample rate must be positive"),
            Self::InvalidSend { bus } => write!(f, "bus {bus} has an invalid send gain or pan"),
            Self::InvalidEffect { bus } => write!(f, "bus {bus} has invalid effect parameters"),
            Self::EffectSampleRate { bus, required } => write!(f, "an effect on bus {bus} requires {required} Hz"),
            Self::InvalidTap { bus, tap } => write!(f, "bus {bus} references unavailable {tap:?}"),
            Self::InputCount => f.write_str("routing input count does not match the configured sources"),
            Self::UnequalFrames => f.write_str("routing inputs must have the same number of frames"),
            Self::TooManyFrames => f.write_str("routing packet exceeds the configured frame capacity"),
        }
    }
}

impl Error for RoutingError {}

struct SendMix {
    from: Tap,
    channels: [f32; 2],
}

enum Processor {
    Gain(f32),
    Duck(Tap, Ducker),
    Custom(Option<Tap>, Box<dyn PacketEffect>),
    Observed(Box<Processor>, StageMeter),
}

impl Processor {
    fn prepare_thread(&mut self) {
        match self {
            Self::Custom(_, e) => e.prepare_thread(),
            Self::Observed(inner, _) => inner.prepare_thread(),
            _ => {}
        }
    }

    fn process<'a>(&mut self, out: &mut [[f32; 2]], read: &impl Fn(Tap) -> Input<'a>) {
        match self {
            Self::Gain(gain) => {
                for frame in out {
                    frame[0] *= *gain;
                    frame[1] *= *gain;
                }
            }
            Self::Duck(key, duck) => duck.process(read(*key), out),
            Self::Custom(key, effect) => effect.process(out, key.map(read)),
            Self::Observed(inner, meter) => {
                let input = meter::peak(out);
                inner.process(out, read);
                let gain = match &**inner {
                    Self::Gain(g) => Some(*g),
                    Self::Duck(_, d) => Some(d.gain()),
                    _ => None,
                };
                meter.publish(StageReading { input_peak: Some(input), output_peak: Some(meter::peak(out)), gain, ..Default::default() });
            }
        }
    }
}

struct ActiveBus {
    sends: Vec<SendMix>,
    effects: Vec<Processor>,
    packet: Vec<[f32; 2]>,
}

pub struct Router {
    buses: Vec<ActiveBus>,
    input_count: usize,
    max_frames: usize,
    frames: usize,
}

impl Router {
    pub fn new(sample_rate: u32, max_frames: usize, input_count: usize, buses: Vec<Bus>) -> Result<Self, RoutingError> {
        if sample_rate == 0 {
            return Err(RoutingError::InvalidSampleRate);
        }
        if max_frames == 0 {
            return Err(RoutingError::InvalidCapacity);
        }
        let mut active = Vec::with_capacity(buses.len());
        for (bus, spec) in buses.into_iter().enumerate() {
            let check = |tap| match tap {
                Tap::Input(i) if i < input_count => Ok(()),
                Tap::Bus(i) if i < bus => Ok(()),
                _ => Err(RoutingError::InvalidTap { bus, tap }),
            };
            let mut sends = Vec::with_capacity(spec.sends.len());
            for send in spec.sends {
                check(send.from)?;
                if !(-1.0..=1.0).contains(&send.pan) || !send.gain.is_finite() {
                    return Err(RoutingError::InvalidSend { bus });
                }
                if send.gain != 0.0 {
                    sends.push(SendMix {
                        from: send.from,
                        channels: [send.gain * (1.0 - send.pan.max(0.0)), send.gain * (1.0 + send.pan.min(0.0))],
                    });
                }
            }
            let mut effects = Vec::with_capacity(spec.effects.len());
            for effect in spec.effects {
                let (mut effect, mut meter) = (effect, None);
                while let Effect::Observed(inner, handle) = effect {
                    effect = *inner;
                    meter = Some(handle);
                }
                let mut processor = match effect {
                    Effect::Gain(g) if !g.is_finite() => return Err(RoutingError::InvalidEffect { bus }),
                    Effect::Gain(1.0) if meter.is_none() => continue,
                    Effect::Gain(g) => Processor::Gain(g),
                    Effect::Duck { key, settings } => {
                        check(key)?;
                        let duck = Ducker::new(settings, sample_rate).ok_or(RoutingError::InvalidEffect { bus })?;
                        if settings.attenuation_db == 0.0 && meter.is_none() {
                            continue;
                        }
                        Processor::Duck(key, duck)
                    }
                    Effect::Custom(effect) => Processor::Custom(None, effect),
                    Effect::Sidechain { key, effect } => {
                        check(key)?;
                        Processor::Custom(Some(key), effect)
                    }
                    Effect::Observed(_, _) => unreachable!("unwrapped above"),
                };
                if let Processor::Custom(_, effect) = &processor
                    && let Some(required) = effect.required_sample_rate()
                    && required != sample_rate
                {
                    return Err(RoutingError::EffectSampleRate { bus, required });
                }
                if let Some(meter) = meter {
                    let internal = match &mut processor {
                        Processor::Custom(_, effect) => effect.set_meter(&meter),
                        _ => false,
                    };
                    if !internal {
                        processor = Processor::Observed(Box::new(processor), meter);
                    }
                }
                if let Processor::Custom(None, next) = &mut processor
                    && let Some(Processor::Custom(_, previous)) = effects.last_mut()
                    && previous.append_compatible(next.as_mut())
                {
                    continue;
                }
                effects.push(processor);
            }
            let mut packet = Vec::new();
            packet.try_reserve_exact(max_frames).map_err(|_| RoutingError::InvalidCapacity)?;
            packet.resize(max_frames, [0.0; 2]);
            active.push(ActiveBus { sends, effects, packet });
        }
        Ok(Self { buses: active, input_count, max_frames, frames: 0 })
    }

    /// Call on the host's processing thread before entering real time. Built-in routing effects need no preparation.
    pub fn prepare_thread(&mut self) {
        for bus in &mut self.buses {
            for stage in &mut bus.effects {
                stage.prepare_thread();
            }
        }
    }

    /// Resolve a packet of borrowed inputs; previous outputs and processor states survive validation errors.
    pub fn process(&mut self, inputs: &[Input<'_>]) -> Result<(), RoutingError> {
        self.process_with(inputs.len(), |index| inputs[index])
    }

    /// Convenience for stereo-only hosts; no array conversion or scratch allocation is needed.
    pub fn process_stereo(&mut self, inputs: &[&[[f32; 2]]]) -> Result<(), RoutingError> {
        self.process_with(inputs.len(), |index| Input::Stereo(inputs[index]))
    }

    fn process_with<'a>(&mut self, count: usize, input: impl Fn(usize) -> Input<'a>) -> Result<(), RoutingError> {
        if count != self.input_count {
            return Err(RoutingError::InputCount);
        }
        let frames = if count > 0 { input(0).frames() } else { 0 };
        if frames > self.max_frames {
            return Err(RoutingError::TooManyFrames);
        }
        if (1..count).any(|i| input(i).frames() != frames) {
            return Err(RoutingError::UnequalFrames);
        }
        for index in 0..self.buses.len() {
            let (prior, rest) = self.buses.split_at_mut(index);
            let bus = &mut rest[0];
            let read = |tap| match tap {
                Tap::Input(i) => input(i),
                Tap::Bus(i) => Input::Stereo(&prior[i].packet[..frames]),
            };
            let out = &mut bus.packet[..frames];
            if let Some((first, others)) = bus.sends.split_first() {
                mix::<false>(out, read(first.from), first.channels);
                for send in others {
                    mix::<true>(out, read(send.from), send.channels);
                }
            } else {
                out.fill([0.0; 2]);
            }
            for effect in &mut bus.effects {
                effect.process(out, &read);
            }
        }
        self.frames = frames;
        Ok(())
    }

    /// Frames for the last successful packet, in graph order. An intermediate bus is available to the caller too.
    pub fn output(&self, bus: usize) -> Option<&[[f32; 2]]> {
        self.buses.get(bus).map(|b| &b.packet[..self.frames])
    }
}

/// The first stereo send at unity is a direct memory copy, including signed zeros and subnormal samples. Subsequent
/// sends are summed. The const generic lets the compiler select the store or sum loop before processing any samples.
fn mix<const ADD: bool>(out: &mut [[f32; 2]], input: Input<'_>, channels: [f32; 2]) {
    if !ADD && channels == [1.0; 2] {
        match input {
            Input::Stereo(frames) => out.copy_from_slice(frames),
            Input::Mono(samples) => out.iter_mut().zip(samples).for_each(|(o, &x)| *o = [x; 2]),
            Input::Silence(_) => out.fill([0.0; 2]),
        }
        return;
    }
    let write = |out: &mut [f32; 2], input: [f32; 2]| {
        let left = channels[0] * input[0];
        let right = channels[1] * input[1];
        if ADD {
            out[0] += left;
            out[1] += right;
        } else {
            *out = [left, right];
        }
    };
    match input {
        Input::Stereo(frames) => out.iter_mut().zip(frames).for_each(|(o, &frame)| write(o, frame)),
        Input::Mono(samples) => out.iter_mut().zip(samples).for_each(|(o, &x)| write(o, [x; 2])),
        Input::Silence(_) if !ADD => out.fill([0.0; 2]),
        Input::Silence(_) => {}
    }
}
