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

/// The host STFT and its inverse, and the high-band policy, at the bin-mapped 48 kHz size (TODO 3).
#[test]
fn the_stft_and_the_high_band_policy_do_not_allocate() {
    use nf_core::config::HighBand;
    use nf_core::dsp::high_band::{MODEL_BAND, band_gain, fill};
    use nf_core::dsp::stft::{Complex32, Istft, Stft, hann};
    let w = hann(1536, true);
    let (mut stft, mut istft) = (Stft::new(&w, 768), Istft::new(&w, 768));
    let x: Vec<f32> = (0..768).map(|i| (i as f32 * 0.01).sin() * 0.1).collect();
    let mut y = vec![0.0f32; 768];
    let mix = vec![0.5f32; 514];
    let mut hi = vec![Complex32::new(0.0, 0.0); 512];
    let n = allocations_in(|| {
        for k in 0..50 {
            let spec = stft.push(&x);
            let g = band_gain(&mix, &mix, MODEL_BAND).unwrap_or(0.0);
            fill(HighBand::ALL[k % 3], g, &spec[257..], &mut hi);
            let s = istft.spectrum_mut();
            s.copy_from_slice(stft.spectrum());
            s[257..].copy_from_slice(&hi);
            istft.synthesize(&mut y);
            if k == 25 {
                stft.reset();
                istft.reset();
            }
        }
    });
    assert_eq!(n, 0);
    std::hint::black_box(&y);
}

/// The pipelined model's audio side (TODO 6): no allocation on the calling thread, inline or with a worker, at each depth,
/// through resets, a stalled worker (gaps and misses) and recovery. The worker's own allocations are its thread's, not
/// counted here.
#[test]
fn the_pipelined_models_audio_side_does_not_allocate() {
    use nf_core::noise::{DEFAULT_DEPTH, Executor, MAX_DEPTH, NoHooks, NoiseModel, Pipelined, RING};
    for depth in 1..=MAX_DEPTH {
        for exec in [0, 1, 2] {
            let make = Box::new(|| Ok(Box::new(DelayModel::new(768, 1536)) as Box<dyn NoiseModel>));
            let e = match exec {
                0 => Executor::Inline,
                w => Executor::Thread { hooks: Arc::new(NoHooks), wait_for_results: w == 2 },
            };
            let mut p = Pipelined::spawn(make, e, depth).expect("spawn");
            p.prepare().expect("prepare");
            let mut out = [0.0f32; BLOCK];
            let stats = p.stats();
            let n = allocations_in(|| {
                for blk in 0..200 {
                    let x = tone(blk);
                    std::hint::black_box(p.process_hop(&x, &mut out));
                    if blk % 60 == 59 {
                        p.reset();
                    }
                    let _ = stats.snapshot();
                }
            });
            assert_eq!(n, 0, "depth {depth}, executor {exec}: allocations on the audio side");
        }
    }
    // A worker that falls behind for longer than the ring holds: gaps, misses, late results, all without allocating on
    // this side.
    let make = Box::new(|| {
        struct Stall(DelayModel, u32);
        impl NoiseModel for Stall {
            fn info(&self) -> &nf_core::noise::ModelInfo {
                self.0.info()
            }
            fn prepare(&mut self) -> Result<(), StageError> {
                Ok(())
            }
            fn reset(&mut self) {
                self.0.reset();
            }
            fn process_hop(&mut self, i: &[f32], o: &mut [f32]) -> f32 {
                self.1 += 1;
                if self.1 % 40 == 0 {
                    // Longer than the ring: a block every 2 ms fills its 32 slots in 64 ms.
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                self.0.process_hop(i, o)
            }
        }
        Ok(Box::new(Stall(DelayModel::new(480, 960), 0)) as Box<dyn NoiseModel>)
    });
    assert_eq!(RING, 32);
    let exec = Executor::Thread { hooks: Arc::new(NoHooks), wait_for_results: false };
    let mut p = Pipelined::spawn(make, exec, DEFAULT_DEPTH).expect("spawn");
    let mut out = [0.0f32; BLOCK];
    let n = allocations_in(|| {
        for blk in 0..200 {
            p.process_hop(&tone(blk), &mut out);
            let t = std::time::Instant::now();
            while t.elapsed() < std::time::Duration::from_millis(2) {
                std::hint::spin_loop();
            }
        }
    });
    let s = p.stats().snapshot();
    assert_eq!(n, 0, "a stalling worker: {s:?}");
    assert!(s.misses > 0 && s.gaps > 0, "it did fall behind: {s:?}");
}

/// The worker's own counters (TODO 6): read 0 Rust allocations in its steady state. The baseline is taken after "ready",
/// whose model info (a pack's owned id and name) is cloned before it, so those two don't count against the jobs.
#[test]
fn the_worker_s_steady_state_counts_no_allocation() {
    use std::any::Any;

    use nf_core::noise::{DEFAULT_DEPTH, Executor, ModelInfo, NoiseModel, Pipelined, WorkerCounters, WorkerGuard, WorkerHooks};

    /// The worker's counters are this binary's per-thread allocation count, read on the worker.
    struct Counted;
    impl WorkerHooks for Counted {
        fn enter(&self) -> WorkerGuard {
            WorkerGuard::none()
        }
        fn steady(&self) -> Box<dyn Any> {
            Box::new(())
        }
        fn counters(&self) -> WorkerCounters {
            WorkerCounters { pool_allocs: ALLOCATIONS.with(Cell::get) as u64, ..WorkerCounters::default() }
        }
    }
    /// A delay model with owned text, as a pack's info is.
    struct Owned(DelayModel, ModelInfo);
    impl NoiseModel for Owned {
        fn info(&self) -> &ModelInfo {
            &self.1
        }
        fn prepare(&mut self) -> Result<(), StageError> {
            Ok(())
        }
        fn reset(&mut self) {
            self.0.reset();
        }
        fn process_hop(&mut self, i: &[f32], o: &mut [f32]) -> f32 {
            self.0.process_hop(i, o)
        }
    }
    let make = Box::new(|| {
        let d = DelayModel::new(768, 1536);
        let info = ModelInfo { id: Text::Owned("a-pack".into()), name: Text::Owned("A pack".into()), ..d.info().clone() };
        Ok(Box::new(Owned(d, info)) as Box<dyn NoiseModel>)
    });
    let exec = Executor::Thread { hooks: Arc::new(Counted), wait_for_results: true };
    let mut p = Pipelined::spawn(make, exec, DEFAULT_DEPTH).expect("spawn");
    let mut out = [0.0f32; BLOCK];
    for blk in 0..300 {
        p.process_hop(&tone(blk), &mut out);
    }
    let s = p.stats().snapshot();
    assert!(s.jobs >= 298, "{s:?}");
    assert_eq!((s.worker_pool_allocs, s.worker_fallbacks), (0, 0), "{s:?}");
}
