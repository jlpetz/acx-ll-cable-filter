//! Enforces the real-time rule (CLAUDE.md): after construction and warm-up, every per-block call
//! that runs on the audio thread makes no heap allocation. A counting global allocator (this test
//! binary only) counts allocations per thread, so tests running in parallel don't disturb each other.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use nf_core::chain::Chain;
use nf_core::config::{GateMode, ProcessingConfig};
use nf_core::controls::ChainControls;
use nf_core::dsp::delay::DelayLine;
use nf_core::dsp::drift::DriftController;
use nf_core::dsp::fade::crossfade;
use nf_core::dsp::gate::Gate;
use nf_core::dsp::meter::Meter;
use nf_core::dsp::mix::FloorMix;
use nf_core::dsp::noise_post::NoisePost;
use nf_core::dsp::reblock::Reblocker;
use nf_core::dsp::slip::{Slip, SlipResampler};
use nf_core::latency::stats::RollingStats;
use nf_core::noise::{DelayModel, NoiseStage, param as noise_param};
use nf_core::stage::{
    Activation, AudioPorts, MONO_IN_OUT, MONO_WITH_REFERENCE, ProcessIo, Stage, StageDescriptor, StageError, Text, VENDOR, features, text,
};
use nf_core::{BLOCK, Block};

struct Counting;

thread_local! {
    // Const-initialised and without a destructor, so reading it never allocates.
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

fn count() {
    let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
}

// SAFETY: every method forwards to `System` with the caller's arguments unchanged, so `System`'s
// guarantees hold; the only addition is a thread-local counter that doesn't allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: forwarded as received; the caller upholds `alloc`'s contract.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: as above.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: as above; `ptr` came from this allocator, which is `System` underneath.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as above.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Allocations made on this thread while `f` runs.
fn allocations_in(f: impl FnOnce()) -> usize {
    let before = ALLOCATIONS.with(Cell::get);
    f();
    ALLOCATIONS.with(Cell::get) - before
}

fn tone(blk: usize) -> Block {
    std::array::from_fn(|i| 0.2 * ((blk * BLOCK + i) as f32 * 0.03).sin())
}

#[test]
fn the_counter_sees_allocations() {
    assert!(allocations_in(|| drop(std::hint::black_box(vec![1u8; 64]))) >= 1);
}

#[test]
fn dsp_blocks_do_not_allocate() {
    let cfg = ProcessingConfig::default();
    let controls = ChainControls::new(&cfg);
    let mut gate = Gate::new();
    let mut mix = FloorMix::new();
    let mut post = NoisePost::new(480);
    let mut delay = DelayLine::new(4096, 480);
    let mut meter = Meter::new();
    let mut slip = SlipResampler::new();
    let mut drift = DriftController::new(960.0);
    let mut stats = RollingStats::<100>::new();
    let mut out = [0.0f32; BLOCK];
    let mut long = [0.0f32; BLOCK + 1];
    let mut short = [0.0f32; BLOCK - 1];
    let input_long = [0.1f32; BLOCK + 1];
    let input_short = [0.1f32; BLOCK - 1];

    let n = allocations_in(|| {
        for blk in 0..200 {
            let x = tone(blk);
            // Every gate mode, and parameter changes that rebuild the coefficient tables.
            let mut p = controls.noise.gate.load();
            p.mode = [GateMode::Off, GateMode::Static, GateMode::Auto][blk % 3];
            p.attack_ms = 1.0 + (blk % 7) as f32;
            p.release_ms = 50.0 + (blk % 5) as f32;
            p.range_db = -30.0 - (blk % 4) as f32;
            let mut g = x;
            gate.process(&mut g, &p, if blk % 2 == 0 { 0.9 } else { f32::NAN });

            let mut w = x;
            mix.process(&tone(blk + 1), &mut w, -20.0 - (blk % 3) as f32);
            let mut w = x;
            controls.noise.floor_db.store(-10.0 - (blk % 9) as f32);
            post.process(&tone(blk + 1), &mut w, &controls.noise, 0.5);

            delay.process(&x, &mut out);
            delay.set_delay(blk * 13 % 4096);
            meter.measure(&x);
            let old = out;
            crossfade(&old, &x, &mut out);

            slip.resample(&x, &mut long);
            slip.resample(&x, &mut short);
            slip.resample(&input_long, &mut out);
            slip.resample(&input_short, &mut out);
            slip.resample(&x, &mut out);

            let s = drift.update(960.0 + (blk % 50) as f32);
            let _ = (s.produced_len(), s.consumed_len(), drift.ppm(), drift.filtered_fill());
            stats.push(blk as f32);
            let _ = (stats.percentile(0.95), stats.median(), stats.max());
        }
    });
    assert_eq!(n, 0, "allocations on the audio path");
    let _ = Slip::None;
}

/// A stage that only scales (it must not allocate either, or the chain test would blame the chain), CLAP-shaped: in the
/// AEC slot with the echo-cancel role and a reference port (which it mixes in a little), in the noise slot as a denoiser.
struct Scale {
    gain: f32,
    descriptor: StageDescriptor,
    ports: AudioPorts,
}

impl Scale {
    fn descriptor(features: &[Text]) -> StageDescriptor {
        let t = text;
        let (id, name, url, version, description) = (t("test.scale"), t("scale"), t(""), t("0"), t(""));
        StageDescriptor { id, name, vendor: VENDOR, url, version, description, features: features.to_vec().into() }
    }

    fn aec(gain: f32) -> Self {
        Self { gain, descriptor: Self::descriptor(&[features::ECHO_CANCEL]), ports: MONO_WITH_REFERENCE.clone() }
    }

    fn noise(gain: f32) -> Self {
        Self { gain, descriptor: Self::descriptor(&[features::DENOISE]), ports: MONO_IN_OUT.clone() }
    }
}

impl Stage for Scale {
    fn descriptor(&self) -> &StageDescriptor {
        &self.descriptor
    }
    fn audio_ports(&self) -> &AudioPorts {
        &self.ports
    }
    fn activate(&mut self, a: &Activation) -> Result<(), StageError> {
        a.require_host("scale")
    }
    fn latency(&self) -> u32 {
        480
    }
    fn process(&mut self, io: ProcessIo<'_>) {
        let (x, r) = (io.main(), io.aux(1).unwrap_or(&[0.0; BLOCK]));
        for ((o, i), r) in io.output.iter_mut().zip(x).zip(r) {
            *o = *i * self.gain - 0.1 * r;
        }
    }
    fn reset(&mut self) {}
}

#[test]
fn the_chain_does_not_allocate_through_toggles_and_passthrough() {
    let controls = Arc::new(ChainControls::new(&ProcessingConfig::default()));
    let mut chain = Chain::new(controls.clone(), Some(Box::new(Scale::aec(0.9))), Some(Box::new(Scale::noise(0.5))));
    chain.activate().expect("activate");
    let reference = [0.0f32; BLOCK];
    let mut out = [0.0f32; BLOCK];
    chain.process(&tone(0), &reference, &mut out); // warm-up

    let n = allocations_in(|| {
        let mut ticks = 0u64;
        for blk in 1..300 {
            controls.aec.enabled.store(blk % 40 < 25, Relaxed);
            controls.noise.enabled.store(blk % 30 < 20, Relaxed);
            controls.passthrough.store(blk % 70 > 50, Relaxed);
            controls.meters_enabled.store(blk % 11 != 0, Relaxed);
            let r = chain.process_timed(&tone(blk), &reference, &mut out, || {
                ticks += 1;
                ticks
            });
            let _ = (r, chain.latency(), chain.aec_stats());
        }
    });
    assert_eq!(n, 0, "allocations in Chain::process");
}

/// The noise stage around a model of every hop the re-blocker handles, with and without lookahead: once built and
/// activated, its blocks, resets, extensions and params allocate nothing, whatever the gate and floor do.
#[test]
fn the_noise_stage_does_not_allocate_at_any_hop() {
    for hop in [160usize, 240, 256, 480, 512, 768, 960, 1024] {
        for lookahead in [0, 1] {
            let controls = Arc::new(ChainControls::new(&ProcessingConfig::default()));
            let model = DelayModel::new(hop, ((2 + lookahead) * hop) as u32).with_vad();
            let mut s = NoiseStage::new(controls.clone(), Box::new(model)).expect("stage");
            s.activate(&Activation::HOST).expect("activate");
            let mut out = [0.0f32; BLOCK];
            let n = allocations_in(|| {
                let mut sum = 0.0f64;
                for blk in 0..300 {
                    let x = tone(blk);
                    s.process(ProcessIo::new(&[&x], &mut out));
                    let sp = s.speech_probability().map_or(f32::NAN, |e| e.speech_probability());
                    let gr = s.gain_adjustment().map_or(f32::NAN, |e| e.gain_adjustment_db());
                    sum += f64::from(sp) + f64::from(gr);
                    // Every param, written and read back, one a block; and a bad value or id, refused without a String.
                    let id = (blk % 9) as u32 + 1;
                    let info = &s.params()[id as usize - 1];
                    let v = info.min + (info.max - info.min) * ((blk % 7) as f64 / 6.0);
                    let _ = s.set_param(id, if id == noise_param::GATE_MODE { (blk % 3) as f64 } else { v });
                    sum += s.param_value(id).unwrap_or(0.0);
                    let _ = (s.set_param(id, f64::NAN), s.set_param(99, 0.0));
                    if blk % 50 == 49 {
                        s.reset();
                        s.reset(); // the second is a no-op
                    }
                    controls.noise.floor_db.store(-60.0 + (blk / 40 % 4) as f32 * 15.0);
                }
                std::hint::black_box((sum, &out));
            });
            assert_eq!(n, 0, "hop {hop}, lookahead {lookahead}: allocations in the noise stage");
        }
    }
}

#[test]
fn the_reblocker_does_not_allocate() {
    for hop in [160usize, 256, 512, 768, 1024] {
        let mut r = Reblocker::new(hop);
        let mut out = [0.0f32; BLOCK];
        let n = allocations_in(|| {
            for blk in 0..100 {
                r.process(&tone(blk), &mut out, |i, o| o.copy_from_slice(i));
                if blk == 50 {
                    r.reset();
                }
            }
        });
        assert_eq!(n, 0, "hop {hop}");
    }
}
