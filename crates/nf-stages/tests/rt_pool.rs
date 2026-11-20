//! RT-1: easyfft builds its FFT planners and scratch once per thread and leaks them by design. The
//! audio thread therefore calls `nf_stages::warm_up_fft_thread()` before it enters its real-time
//! scope (nf-engine's `audio::thread_main`), so that state lands on the system heap. Each start then
//! builds, activates, runs and drops its noise stage inside the scope and gives the locked pool
//! back in full; without the warm-up every start would keep about 22 KB of the pool for good.
//!
//! This binary's global allocator is nf-mem's `RtAllocator`, as in the app, over a pool of 4 KB
//! unlocked pages (tests never take the large-page path, CLAUDE.md). The pool's counters are
//! process-wide, so every test holds `SERIAL` and checks deltas, and nothing formats or prints
//! inside a scope.

use std::sync::{Arc, Mutex, MutexGuard};

use nf_core::config::{GateConfig, GateMode, ProcessingConfig};
use nf_core::controls::ChainControls;
use nf_core::stage::{Activation, ProcessIo, Stage};
use nf_core::{BLOCK, SAMPLE_RATE};
use nf_mem::{PageKind, PagePolicy, RtAllocator, RtScope};
use nf_stages::synth::{Voice, add, pink_noise, speech_like};
use nf_stages::{build_chain, rnnoise_stage};

#[global_allocator]
static GLOBAL: RtAllocator = RtAllocator::new();

const POOL: usize = 8 << 20;
/// Fresh threads, one start each.
const STARTS: usize = 8;

static SERIAL: Mutex<()> = Mutex::new(());

/// Installs the pool once and serialises the tests.
fn setup() -> MutexGuard<'static, ()> {
    let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let s = nf_mem::install_pool(PagePolicy::SmallUnlocked, POOL);
    assert_eq!(s.kind, PageKind::SmallUnlocked, "{}", s.describe());
    g
}

/// One second of noisy speech, as whole blocks, built outside any scope.
fn input() -> Arc<Vec<f32>> {
    let len = SAMPLE_RATE as usize;
    let sp = speech_like(len, Voice::LOW, 3);
    Arc::new(add(&sp.samples, &pink_noise(len, 0.01, 4)))
}

/// One start on a fresh thread, as on the engine's audio thread: the warm-up if `warm`, then
/// `start` inside the real-time scope. Returns the thread's scope allocations and fallbacks.
fn start_on_a_fresh_thread(warm: bool, start: impl FnOnce() + Send + 'static) -> (u64, u64) {
    std::thread::spawn(move || {
        if warm {
            nf_stages::warm_up_fft_thread();
        }
        let (a0, f0) = (nf_mem::thread_scope_allocs(), nf_mem::thread_scope_fallbacks());
        {
            let _scope = RtScope::enter();
            start();
        }
        (nf_mem::thread_scope_allocs() - a0, nf_mem::thread_scope_fallbacks() - f0)
    })
    .join()
    .expect("the start's thread")
}

/// Builds, activates, runs `input` through and drops the noise stage. The gate is on, so all of
/// NoisePost runs. Two resets on the way swap in RNNoise's spare states, and its spare maker (a
/// thread outside the scope) frees the used ones, from the pool, and builds new ones on the heap.
fn noise_stage_start(controls: &Arc<ChainControls>, input: &Arc<Vec<f32>>) -> impl FnOnce() + Send + 'static {
    let (controls, input) = (controls.clone(), input.clone());
    move || {
        let ok = match rnnoise_stage(controls) {
            Ok(mut stage) => {
                let ok = stage.activate(&Activation::HOST).is_ok();
                let mut y = [0.0f32; BLOCK];
                for (b, x) in input.as_chunks::<BLOCK>().0.iter().enumerate() {
                    if b == 30 || b == 60 {
                        stage.reset();
                        std::thread::sleep(nf_stages::MAKER_POLL + std::time::Duration::from_millis(50));
                    }
                    stage.process(ProcessIo::new(&[x], &mut y));
                }
                std::hint::black_box(&y);
                drop(stage);
                ok
            }
            Err(_) => false,
        };
        assert!(ok, "the stage failed to build or activate"); // formats only on failure
    }
}

fn controls() -> Arc<ChainControls> {
    let gate = GateConfig { mode: GateMode::Auto, ..GateConfig::default() };
    Arc::new(ChainControls::new(&ProcessingConfig { gate, ..ProcessingConfig::default() }))
}

#[test]
fn warmed_up_threads_give_the_pool_back_after_every_start() {
    let _g = setup();
    let (controls, input) = (controls(), input());
    let c0 = nf_mem::counters();
    for k in 0..STARTS {
        let (allocs, fallbacks) = start_on_a_fresh_thread(true, noise_stage_start(&controls, &input));
        let c = nf_mem::counters();
        // The stage's two model states and the warm-up's throwaway (ten allocations each) and
        // NoisePost's buffers come from the pool...
        assert!(allocs >= 30, "start {k}: only {allocs} allocations in the scope");
        assert_eq!(fallbacks, 0, "start {k}: the pool should serve the whole start");
        // ...and all of it goes back.
        assert_eq!(
            c.pool_bytes_in_use,
            c0.pool_bytes_in_use,
            "start {k}: the pool kept {} bytes",
            c.pool_bytes_in_use.wrapping_sub(c0.pool_bytes_in_use)
        );
        assert_eq!(c.pool_allocs - c0.pool_allocs, c.pool_frees - c0.pool_frees, "start {k}: live pool blocks left");
    }
    let c = nf_mem::counters();
    eprintln!(
        "{STARTS} warmed-up starts: {} pool allocations, all freed; peak {} KB in use",
        c.pool_allocs - c0.pool_allocs,
        c.pool_peak_bytes / 1024
    );
}

#[test]
fn a_thread_that_skips_the_warm_up_leaks_easyfft_state_into_the_pool() {
    // The control: why the warm-up has to come before the scope. `activate` still warms up, but
    // inside the scope, so easyfft's per-thread state comes from the pool and is never freed.
    let _g = setup();
    let (controls, input) = (controls(), input());
    let c0 = nf_mem::counters();
    let (allocs, fallbacks) = start_on_a_fresh_thread(false, noise_stage_start(&controls, &input));
    let c = nf_mem::counters();
    let kept = c.pool_bytes_in_use - c0.pool_bytes_in_use;
    eprintln!("a start without the warm-up kept {kept} bytes of the pool ({allocs} allocations)");
    assert_eq!(fallbacks, 0);
    assert!(kept >= 4096, "only {kept} bytes kept: has easyfft stopped leaking per thread? Then the warm-up can go");
}

#[test]
fn a_whole_chain_start_with_aec3_gives_the_pool_back_too() {
    // What the engine builds on each start: AEC3 (about 300 KB; sonora 0.3.0 allocates nothing per
    // block, 0.2.0 made Vecs) and RNNoise, run with a far end and its echo. sonora keeps no
    // per-thread state.
    let _g = setup();
    let controls = controls();
    let mic = input();
    let far: Arc<Vec<f32>> = Arc::new(speech_like(SAMPLE_RATE as usize, Voice::HIGH, 5).samples);
    let c0 = nf_mem::counters();
    for k in 0..2 {
        let (controls, mic, far) = (controls.clone(), mic.clone(), far.clone());
        let (allocs, fallbacks) = start_on_a_fresh_thread(true, move || {
            let built = build_chain(controls, true);
            let ok = built.is_ok();
            if let Ok(mut chain) = built {
                let mut y = [0.0f32; BLOCK];
                for (m, r) in mic.as_chunks::<BLOCK>().0.iter().zip(far.as_chunks::<BLOCK>().0) {
                    std::hint::black_box(chain.process(m, r, &mut y));
                }
                std::hint::black_box(&y);
            }
            assert!(ok, "build_chain failed");
        });
        let c = nf_mem::counters();
        assert!(allocs > 1000, "start {k}: only {allocs} allocations in the scope");
        assert_eq!(fallbacks, 0, "start {k}: the pool should serve the whole start");
        assert_eq!(
            c.pool_bytes_in_use,
            c0.pool_bytes_in_use,
            "start {k}: the pool kept {} bytes",
            c.pool_bytes_in_use.wrapping_sub(c0.pool_bytes_in_use)
        );
        eprintln!("chain start {k}: {allocs} scope allocations, all freed");
    }
}
