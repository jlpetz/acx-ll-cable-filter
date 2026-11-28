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
    /// Output of a zero-based logical effect on an earlier bus, including effects sharing one block adapter.
    Stage(usize, usize),
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

    /// True when this instance can start on the calling audio thread without further setup. Checked on live edits only.
    /// Implementations must not allocate, block or initialise thread-local scratch in this query.
    fn ready_on_thread(&self) -> bool {
        false
    }

    /// A block-based effect can retain the meter at its native hop boundary. Otherwise the router measures packet peaks.
    fn set_meter(&mut self, _meter: &StageMeter) -> bool {
        false
    }

    /// During graph construction, two adjacent effects with the same block interface may share framing. Return `true`
    /// only after taking ownership of `next`'s processing state. A keyed `next` is always attached independently.
    fn append_compatible(&mut self, _next: &mut dyn PacketEffect) -> bool {
        false
    }

    /// A fused effect must make its intermediate stages available after a later live subscription.
    /// Returning true also requires `limit_stages` to preserve a shortened live prefix at the current packet position.
    fn supports_stage_taps(&self) -> bool {
        false
    }

    /// Send each intermediate mono span to `publish` with its zero-based stage and packet offset. The final stage writes `stereo`.
    fn process_tapped(&mut self, stereo: &mut [[f32; 2]], sidechain: Option<Input<'_>>, _publish: &mut dyn FnMut(usize, usize, &[f32])) {
        self.process(stereo, sidechain);
    }

    /// Shorten a fused group on the audio thread while preserving its current packet phase. Called only for a partial group.
    fn limit_stages(&mut self, _active: usize) {}

    /// `sidechain` is an aligned, read-only packet when this instance is routed as [`Effect::Sidechain`].
    fn process(&mut self, stereo: &mut [[f32; 2]], sidechain: Option<Input<'_>>);
}

pub enum Effect {
    Gain(f32),
    /// Optional trailing rack metadata. Execute only the first `n` logical stages, retaining their DSP history through live shrink.
    ActivePrefix(usize),
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

impl Bus {
    /// Ordered stages that produce audio. A retained rack can keep an inactive suffix until it is retired.
    pub fn active_effects(&self) -> &[Effect] {
        match self.effects.split_last() {
            Some((Effect::ActivePrefix(n), stages)) => &stages[..(*n).min(stages.len())],
            _ => &self.effects,
        }
    }
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
    IncompatibleStream,
    InvalidRetainedRack { old: usize, new: usize },
    EffectNeedsThreadSetup { bus: usize },
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
            Self::IncompatibleStream => f.write_str("a live edit requires the same sample rate, input count and packet capacity"),
            Self::InvalidRetainedRack { old, new } => {
                write!(f, "cannot retain rack {old} on bus {new}: check bus indexes, uniqueness and sidechain keys")
            }
            Self::EffectNeedsThreadSetup { bus } => {
                write!(f, "an effect on bus {bus} needs capture-thread setup. Stop and start audio to activate it")
            }
        }
    }
}

impl Error for RoutingError {}

struct SendMix {
    from: Tap,
    channels: [f32; 2],
}

enum Processor {
    Bypass,
    Gain(f32),
    Duck(Tap, Ducker),
    Custom(Option<Tap>, Box<dyn PacketEffect>),
    Observed(Box<Processor>, StageMeter),
}

impl Processor {
    fn key_mut(&mut self) -> Option<&mut Tap> {
        match self {
            Self::Duck(key, _) | Self::Custom(Some(key), _) => Some(key),
            Self::Observed(inner, _) => inner.key_mut(),
            _ => None,
        }
    }

    fn ready_on_thread(&self) -> bool {
        match self {
            Self::Custom(_, e) => e.ready_on_thread(),
            Self::Observed(inner, _) => inner.ready_on_thread(),
            _ => true,
        }
    }

    fn prepare_thread(&mut self) {
        match self {
            Self::Custom(_, e) => e.prepare_thread(),
            Self::Observed(inner, _) => inner.prepare_thread(),
            _ => {}
        }
    }

    fn process<'a>(&mut self, out: &mut [[f32; 2]], read: &impl Fn(Tap) -> Input<'a>) {
        match self {
            Self::Bypass => {}
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

struct Group {
    first: usize,
    end: usize,
    processor: Processor,
}

struct ActiveBus {
    sends: Vec<SendMix>,
    effects: Vec<Group>,
    packet: Vec<[f32; 2]>,
    stage_packets: Vec<Option<Vec<[f32; 2]>>>,
    active: usize,
    limit: Option<usize>,
    // A muted/no-op placeholder can declare the sidechain of a retained effect without compiling a processor.
    keys: Vec<(usize, Tap)>,
    // The opposite router's bus index during a packet-boundary edit. No allocation on the capture thread.
    retained_as: Option<usize>,
}

impl ActiveBus {
    fn remap_effect_keys(&mut self, earlier: &[ActiveBus], active: usize) {
        for g in self.effects.iter_mut().take_while(|g| g.first < active) {
            if let Some(Tap::Bus(i) | Tap::Stage(i, _)) = g.processor.key_mut() {
                *i = earlier[*i].retained_as.unwrap_or(*i);
            }
        }
        for (_, key) in self.keys.iter_mut().take_while(|(i, _)| *i < active) {
            if let Tap::Bus(i) | Tap::Stage(i, _) = key {
                *i = earlier[*i].retained_as.unwrap_or(*i);
            }
        }
    }

    fn can_expose(&self, boundary: usize) -> bool {
        self.effects
            .iter()
            .find(|g| boundary < g.end)
            .is_some_and(|g| boundary + 1 == g.end || matches!(&g.processor, Processor::Custom(_, effect) if effect.supports_stage_taps()))
    }

    fn set_active(&mut self, active: usize) {
        self.active = active;
        if let Some(g) = self.effects.iter_mut().find(|g| g.first < active && active < g.end)
            && let Processor::Custom(_, effect) = &mut g.processor
        {
            effect.limit_stages(active - g.first);
        }
    }

    fn run_effects<'a>(&mut self, frames: usize, read: &impl Fn(Tap) -> Input<'a>) {
        let out = &mut self.packet[..frames];
        for g in self.effects.iter_mut().take_while(|g| g.first < self.active) {
            let last = self.active.min(g.end) - 1;
            if self.stage_packets.is_empty() {
                g.processor.process(out, read);
                continue;
            }
            if g.first < last
                && self.stage_packets.get(g.first..last).is_some_and(|p| p.iter().any(Option::is_some))
                && let Processor::Custom(key, effect) = &mut g.processor
            {
                effect.process_tapped(out, key.map(read), &mut |stage, offset, mono| {
                    if let Some(Some(packet)) = self.stage_packets.get_mut(g.first + stage) {
                        for (frame, &sample) in packet[offset..offset + mono.len()].iter_mut().zip(mono) {
                            *frame = [sample; 2];
                        }
                    }
                });
            } else {
                g.processor.process(out, read);
            }
            if let Some(Some(packet)) = self.stage_packets.get_mut(last) {
                packet[..frames].copy_from_slice(out);
            }
        }
    }
}

pub struct Router {
    buses: Vec<ActiveBus>,
    sample_rate: u32,
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
        let mut subscribed: Vec<Vec<bool>> = buses.iter().map(|b| vec![false; b.effects.len()]).collect();
        for (bus, b) in buses.iter().enumerate() {
            for tap in b.sends.iter().map(|s| s.from).chain(b.active_effects().iter().filter_map(Effect::key)) {
                if let Tap::Stage(i, stage) = tap {
                    let slot = subscribed.get_mut(i).and_then(|s| s.get_mut(stage)).ok_or(RoutingError::InvalidTap { bus, tap })?;
                    *slot = true;
                }
            }
        }
        let mut active: Vec<ActiveBus> = Vec::with_capacity(buses.len());
        for (bus, mut spec) in buses.into_iter().enumerate() {
            let check = |tap| match tap {
                Tap::Input(i) if i < input_count => Ok(()),
                Tap::Bus(i) if i < bus => Ok(()),
                Tap::Stage(i, stage) if i < bus && stage < active[i].active => Ok(()),
                _ => Err(RoutingError::InvalidTap { bus, tap }),
            };
            let limit = match spec.effects.last() {
                Some(Effect::ActivePrefix(n)) => {
                    let n = *n;
                    spec.effects.pop();
                    if n > spec.effects.len() {
                        return Err(RoutingError::InvalidEffect { bus });
                    }
                    Some(n)
                }
                _ => None,
            };
            let count = limit.unwrap_or(spec.effects.len());
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
            let keys = spec.effects.iter().take(count).enumerate().filter_map(|(i, e)| e.key().map(|key| (i, key))).collect();
            let mut effects: Vec<Group> = Vec::with_capacity(spec.effects.len());
            for (index, effect) in spec.effects.into_iter().take(count).enumerate() {
                let (mut effect, mut meter) = (effect, None);
                while let Effect::Observed(inner, handle) = effect {
                    effect = *inner;
                    meter = Some(handle);
                }
                let mut processor = match effect {
                    Effect::Gain(g) if !g.is_finite() => return Err(RoutingError::InvalidEffect { bus }),
                    Effect::Gain(1.0) if meter.is_none() => Processor::Bypass,
                    Effect::Gain(g) => Processor::Gain(g),
                    Effect::Duck { key, settings } => {
                        check(key)?;
                        let duck = Ducker::new(settings, sample_rate).ok_or(RoutingError::InvalidEffect { bus })?;
                        if settings.attenuation_db == 0.0 && meter.is_none() { Processor::Bypass } else { Processor::Duck(key, duck) }
                    }
                    Effect::Custom(effect) => Processor::Custom(None, effect),
                    Effect::Sidechain { key, effect } => {
                        check(key)?;
                        Processor::Custom(Some(key), effect)
                    }
                    Effect::Observed(_, _) => unreachable!("unwrapped above"),
                    Effect::ActivePrefix(_) => return Err(RoutingError::InvalidEffect { bus }),
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
                    && let Some(group) = effects.last_mut()
                    && let Processor::Custom(_, previous) = &mut group.processor
                    && (previous.supports_stage_taps() || !subscribed[bus][group.first..index].iter().any(|&tap| tap))
                    && previous.append_compatible(next.as_mut())
                {
                    group.end = index + 1;
                    continue;
                }
                effects.push(Group { first: index, end: index + 1, processor });
            }
            let allocate_packet = || {
                let mut packet = Vec::new();
                packet.try_reserve_exact(max_frames).map_err(|_| RoutingError::InvalidCapacity)?;
                packet.resize(max_frames, [0.0; 2]);
                Ok(packet)
            };
            let packet = allocate_packet()?;
            let stage_packets = if subscribed[bus].iter().take(count).any(|&tap| tap) {
                subscribed[bus].iter().take(count).map(|&tap| tap.then(allocate_packet).transpose()).collect::<Result<_, RoutingError>>()?
            } else {
                Vec::new()
            };
            active.push(ActiveBus { sends, effects, packet, keys, stage_packets, active: count, limit, retained_as: None });
        }
        Ok(Self { buses: active, sample_rate, input_count, max_frames, frames: 0 })
    }

    /// Call on the host's processing thread before entering real time. Built-in routing effects need no preparation.
    pub fn prepare_thread(&mut self) {
        for bus in &mut self.buses {
            for stage in &mut bus.effects {
                stage.processor.prepare_thread();
            }
        }
    }

    /// Activate a replacement built for this stream at a packet boundary. Each `(old, new)` keeps the running rack and
    /// uses the replacement bus's sends. A sidechain follows its retained bus if its index moved. A malformed mapping, changed sidechain
    /// or new processor requiring thread setup refuses the whole edit. Destroy the displaced router on a control thread.
    pub fn activate_edit(&mut self, replacement: &mut Router, retained: &[(usize, usize)]) -> Result<(), RoutingError> {
        if self.sample_rate != replacement.sample_rate
            || self.max_frames != replacement.max_frames
            || self.input_count != replacement.input_count
        {
            return Err(RoutingError::IncompatibleStream);
        }
        for bus in self.buses.iter_mut().chain(&mut replacement.buses) {
            bus.retained_as = None;
        }
        for &(old, new) in retained {
            let error = RoutingError::InvalidRetainedRack { old, new };
            let (Some(from), Some(to)) = (self.buses.get_mut(old), replacement.buses.get_mut(new)) else { return Err(error) };
            if from.retained_as.is_some() || to.retained_as.is_some() {
                return Err(error);
            }
            from.retained_as = Some(new);
            to.retained_as = Some(old);
        }
        for &(old, new) in retained {
            let from = &self.buses[old];
            let to = &replacement.buses[new];
            let keep = to.limit.unwrap_or(from.active);
            let same_bus =
                |i: usize, j: usize| self.buses[i].retained_as.unwrap_or(i) == j && replacement.buses[j].retained_as.unwrap_or(j) == i;
            let same_key = |before: Tap, after: Tap| match (before, after) {
                (Tap::Bus(i), Tap::Bus(j)) => same_bus(i, j),
                (Tap::Stage(i, a), Tap::Stage(j, b)) => a == b && same_bus(i, j),
                _ => before == after,
            };
            let old_keys = &from.keys[..from.keys.partition_point(|(i, _)| *i < keep)];
            let new_keys = &to.keys[..to.keys.partition_point(|(i, _)| *i < to.active)];
            // Historical callers use sparse placeholders carrying just the sidechain edges. Publishing intermediate stages
            // requires full logical positions so that loop validation and the running processors describe the same prefix.
            let positional = !to.stage_packets.is_empty() || !from.stage_packets.is_empty() || to.limit.is_some();
            let keys_match = old_keys.len() == new_keys.len()
                && old_keys.iter().zip(new_keys).all(|((i, a), (j, b))| (!positional || i == j) && same_key(*a, *b));
            if !keys_match
                || (positional && to.active != keep)
                || keep > from.active
                || (keep < from.active && keep > 0 && !from.can_expose(keep - 1))
                || to.stage_packets.iter().enumerate().any(|(i, p)| p.is_some() && (i >= keep || !from.can_expose(i)))
            {
                return Err(RoutingError::InvalidRetainedRack { old, new });
            }
        }
        for (bus, b) in replacement.buses.iter().enumerate() {
            if b.retained_as.is_none() && !b.effects.iter().all(|g| g.processor.ready_on_thread()) {
                return Err(RoutingError::EffectNeedsThreadSetup { bus });
            }
        }
        for &(old, new) in retained {
            let (old_prefix, old_tail) = self.buses.split_at_mut(old);
            let (new_prefix, new_tail) = replacement.buses.split_at_mut(new);
            let (from, to) = (&mut old_tail[0], &mut new_tail[0]);
            let keep = to.limit.unwrap_or(from.active);
            from.remap_effect_keys(old_prefix, keep);
            to.remap_effect_keys(new_prefix, to.active);
            if keep < from.active {
                from.set_active(keep);
            }
            std::mem::swap(&mut from.keys, &mut to.keys);
            std::mem::swap(&mut from.active, &mut to.active);
            std::mem::swap(&mut from.effects, &mut to.effects);
        }
        std::mem::swap(self, replacement);
        Ok(())
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
                Tap::Stage(i, stage) => {
                    Input::Stereo(&prior[i].stage_packets[stage].as_ref().expect("subscribed stage has storage")[..frames])
                }
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
            bus.run_effects(frames, &read);
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
