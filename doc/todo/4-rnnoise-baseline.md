# 4. RNNoise baseline (`nnnoiseless`)

Status: chosen as the baseline (owner, 2026-10-26). It runs live in v0, and since 2026-11-02 it is the first model
through the offline harness (#2), as a model inside the CLAP-shaped noise stage (#17, #3). The research behind the
choice is `doc/research-2026-10.md` (Noise models); the API facts are `doc/dev/api/sonora-nnnoiseless.md`, Part 2.

## Built (2026-11-02)
- **A model, not a stage:** `nf_stages::RnnoiseModel` implements nf-core's `NoiseModel`:
  - hop 480, `latency_samples` 960 (the 960 window, no lookahead), a VAD;
  - `nf_stages::rnnoise_stage` wraps it in `NoiseStage` (`noise-filter.denoise`), which sanitises, clamps, and runs
    the floor and gate.
- **Unchanged behaviour:**
  - the scale is ×32768 in and ÷32768 out;
  - the stream offset is 480;
  - a reset swaps in a pristine spare state (then: one spare, and a second reset rebuilt the state in place, ten
    allocations; now two spares kept off the audio thread, below);
  - the stage's activate warms up easyfft on the processing thread;
  - the output is bit for bit the old `RnnoiseStage`'s (#2's refactor guard).
- **Measured** (#2's harness):
  - the impulse offset is 480 at every block phase; GCC-PHAT on speech gives 479.987;
  - the model's offset hop by hop is 480 = 960 - 480;
  - bit-equal to nnnoiseless driven directly;
  - on the 10 fetched speech takes in cafe noise at 10 dB SNR: the offset is 479.99-480.00, and the noise is cut by
    11.7-34.1 dB in pauses.
- **CPU** (dev VM, release, p50): the model alone 34.5-35.6 µs a block, the whole noise stage 34.1-34.3 µs, the floor
  and gate 0.4-0.5 µs.
- **Allocation:** none per block once warm, and none at a reset (2026-11-02, below). Before that, the first reset after
  an activate swapped in the spare (none), and each later one until the next activate rebuilt the state in place, ten
  allocations on the audio thread from the engine's real-time pool: each passthrough end and each noise toggle after the
  first.
- **Huge input:** a finite sample of about 1e17 or more overflowed the ×32768 to inf, and easyfft aborted on the NaN
  it became (undefined behaviour in release; already so before #17). The noise stage now clamps its input to ±1e4
  first. Checked with `f32::MAX` and 1e17 samples, through the stage and the chain.

## Spares off the audio thread (built 2026-11-02, ours)
nnnoiseless has no reset, and `DenoiseState::new` costs ten allocations, so a reset swaps in a state built beforehand.
`RnnoiseModel` now keeps two pristine states on a lock-free ring (`rtrb`, `SPARES`), and a thread of its own, the spare
maker (`nf-rnnoise-spares`, normal priority), keeps the ring full and frees the states the resets retired, which come back
on a second ring:
- **At activate** (`prepare`): the ring is filled on the calling thread (inside the engine's real-time scope, from its
  pool, at start-up), then the maker starts with its side. A second activate keeps the maker it has.
- **At a reset** (audio thread): pop a spare, swap it in, push the used state onto the retired ring. No allocation, free,
  lock or system call: the maker isn't woken, it looks every 100 ms (`MAKER_POLL`). Two spares cover two real resets in
  100 ms; only a third within 100 ms of two others would find none and rebuild in place (no control toggles that fast):
  the one known exception to the reset rule, kept in its text (nf-core `lib.rs`, `stage/mod.rs`, `NoiseModel::reset`),
  ten allocations from the pool, counted (`RnnoiseModel::rebuilds`), and the used state is retired as at any reset, so
  nothing is freed on the audio thread. The retired ring has room for every state in flight (six), and a parking slot
  behind it.
- **Why it polls (ours, after review):** waking the maker at a reset (`Thread::unpark`, as `Pipelined` wakes its worker)
  would shrink the exception's window from 100 ms to the maker's few, but it is a system call, which the stage rule keeps
  out of `reset`; a thread that looks at two rings ten times a second costs nothing beside the audio thread's 100-500
  wakes. The states built with the model and at `prepare` come from the real-time pool, and the maker frees them when
  they're retired: nf-mem's pool takes frees from any thread (by address, under its spin lock, held only for a TLSF free;
  a contended locker yields every 128 spins), and the audio thread takes that lock only when it allocates, at a start and
  in the exception.
- **Freed off the audio thread:** the maker frees retired states (pool memory goes back to the pool from any thread) and
  builds new ones on the system heap (it's outside any real-time scope; `DenoiseState::new` runs no FFT, so easyfft's
  per-thread state isn't built there). Dropping the model stops and joins the maker, which hands its ring back, so
  everything is freed by the time the drop returns, by whoever drops it: the engine's pool comes back in full at each start
  (RT-1).
- **Tests:** `tests/rt_no_alloc.rs` `rnnoise_resets_never_allocate` (twelve resets through the chain, the noise toggle and
  passthrough in turn, then two back to back and a re-activate: 0 allocations on the processing thread) and
  `resetting_an_unused_noise_stage_keeps_its_spare`; `rnnoise.rs` `the_maker_replaces_each_spare_a_reset_takes` (every
  reset pristine, bit for bit a fresh model's, none rebuilt; a model never prepared spends its two spares, then rebuilds
  in place, counted);
  `tests/rt_pool.rs` (the pool back in full after starts with resets); nf-engine `audio.rs`
  `a_warmed_thread_builds_the_noise_stage_without_leaking` (the maker's own spawn accounted, as a bare spawn's).
- **Not done:** the fallback's count reaching telemetry (`RnnoiseModel::rebuilds` is the model's; the stage doesn't
  report it).

## Next
- Stays the bring-up model and later the eco mode. The candidates (#5) go through the same harness and the same
  noise stage, as packs (`doc/spec/model-pack-v0.md`) or built in.
- It can't load RNNoise 0.2 models (nnnoiseless 0.5.2).
