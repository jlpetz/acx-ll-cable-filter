# API reference: `sonora` 0.2.0 (AEC3) and `nnnoiseless` 0.5.2 (RNNoise)

Written from the registry sources on 2026-10-26. Path prefixes used below (all under
`C:\Users\Administrator\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\`):

| Prefix | Crate | Role for us |
|---|---|---|
| `S/` | `sonora-0.2.0/src` | public API (`AudioProcessing`, `Config`, ...) |
| `A/` | `sonora-aec3-0.2.0/src` | AEC3 internals (not configurable through `sonora` 0.2.0) |
| `C/` | `sonora-common-audio-0.2.0/src` | float/S16 conversion, biquads, resamplers |
| `M/` | `sonora-simd-0.2.0/src` | runtime SIMD dispatch (`SimdBackend`) |
| `N/` | `nnnoiseless-0.5.2/src` | RNNoise |
| `E/` | `easyfft-0.4.2/src` | FFT wrapper nnnoiseless uses |
| `G/` | `generic_singleton-0.5.3/src` | easyfft's thread-local storage |

`sonora` 0.2.0 depends on `sonora-aec3`, `sonora-agc2`, `sonora-common-audio`, `sonora-ns`,
`sonora-simd` (all `0.2.0`) and `tracing 0.1` (`sonora-0.2.0/Cargo.toml`). `sonora-fft 0.2.0` comes in
through `sonora-aec3`. All seven are built from the same git commit `a024d6ef` (`.cargo_vcs_info.json`).
Our pins: `sonora = "=0.3.0"` since 2026-11-20 (Part 1 describes 0.2.0, the pin until then; 1.11 says what 0.3.0
changed), `nnnoiseless = { version = "=0.5.2", default-features = false }`.

Statements marked **(inferred)** come from reading the code, not from a test or measurement.
Statements marked **(GitHub)** come from the upstream repo (issue or PR text), not from the 0.2.0 sources.

---

## Part 1: sonora 0.2.0

### 1.1 Public items (`S/lib.rs:21-29`)

```rust
pub mod config; pub mod high_pass_filter; pub mod stats; pub mod three_band_filter_bank;
pub use audio_processing::{AudioProcessing, AudioProcessingBuilder, Error};
pub use config::Config;
pub use stats::AudioProcessingStats;
pub use stream_config::StreamConfig;
```

Name clash: `sonora::config::HighPassFilter` is the **config** struct. `sonora::high_pass_filter::HighPassFilter`
is the DSP filter (`new(sample_rate_hz: i32, num_channels: usize)`, which panics for anything other
than 16/32/48 kHz, `S/high_pass_filter.rs:63-71`). Import the config one.

### 1.2 Config structs (`S/config.rs`)

| Item | Definition | Line |
|---|---|---|
| `Config` | `#[derive(Debug, Clone, Default)] pub struct Config { pub pipeline: Pipeline, pub pre_amplifier: Option<PreAmplifier>, pub capture_level_adjustment: Option<CaptureLevelAdjustment>, pub high_pass_filter: Option<HighPassFilter>, pub echo_canceller: Option<EchoCanceller>, pub noise_suppression: Option<NoiseSuppression>, pub gain_controller2: Option<GainController2> }`. Every component defaults to `None` (off). | 31-51 |
| `MaxProcessingRate` | `enum { Rate32kHz, Rate48kHz }`, `fn as_hz(self) -> u32` | 54-70 |
| `Pipeline` | `{ pub maximum_internal_processing_rate: MaxProcessingRate, pub multi_channel_render: bool, pub multi_channel_capture: bool, pub capture_downmix_method: DownmixMethod }` | 73-83 |
| `Pipeline::default()` | `Rate32kHz`, `true`, `true`, `AverageChannels`. **The default is 32 kHz, so set `Rate48kHz` explicitly.** | 94-103 |
| `HighPassFilter` (config) | `{ pub apply_in_full_band: bool }`, default `true` | 158-171 |
| `EchoCanceller` | `{ pub enforce_high_pass_filtering: bool, pub transparent_mode: TransparentModeType }`, default `true`, `Legacy` | 174-194 |
| `TransparentModeType` | re-export of `sonora_aec3::config::TransparentModeType { Legacy (default), Hmm }` | 196; `A/config.rs:703-714` |
| `NoiseSuppression` / `GainController2` | leave as `None` (WebRTC NS / AGC2 off) | 199-247 |

There is **no way to pass a custom `EchoCanceller3Config` in 0.2.0.** `initialize_echo_controller` always
uses `EchoCanceller3Config::default()` plus `transparent_mode`, with
`create_default_multichannel_config()` as the multichannel config (`S/audio_processing_impl.rs:1269-1291`).
Delay window, filter length (13 blocks = 52 ms, `A/config.rs:536`), `num_filters` (5) and
`use_external_delay_estimator` (false) are therefore fixed. (Upstream PR #41, open, adds this. **(GitHub)**)

**High-pass filter semantics** (`S/audio_processing_impl.rs:1229-1267`, phases at 468-477 and 562-572):

| Goal | `high_pass_filter` | `echo_canceller.enforce_high_pass_filtering` | Result |
|---|---|---|---|
| HPF off | `None` | `false` | no HPF |
| HPF on, WebRTC default | `None` | `true` | 3-biquad HPF on split band 0 (0-8 kHz, run at 16 kHz), after band split |
| HPF on, full band | `Some(HighPassFilter { apply_in_full_band: true })` | either | HPF at 48 kHz before band split |

The doc comment "Has no effect in mobile mode" (`S/config.rs:176-177`) refers to a mode sonora doesn't have.
RNNoise applies its own DC-blocking biquad no matter what (see 2.4), so sonora's HPF mostly matters for AEC3.

### 1.3 Builder and construction (`S/audio_processing.rs`)

```rust
impl AudioProcessingBuilder {                                    // struct at 310-316
    pub fn config(self, config: Config) -> Self;                 // 334
    pub fn capture_config(self, config: StreamConfig) -> Self;   // 342 (used as capture input AND output)
    pub fn render_config(self, config: StreamConfig) -> Self;    // 350 (render input AND output)
    pub fn echo_detector(self, enabled: bool) -> Self;           // 359 (residual echo detector, opt-in)
    pub fn build(self) -> AudioProcessing;                       // 365-388 (infallible)
}
impl AudioProcessing {
    pub fn new() -> Self;                                        // 439 (16 kHz mono, nothing enabled)
    pub fn builder() -> AudioProcessingBuilder;                  // 444
}
impl StreamConfig {                                              // S/stream_config.rs:6-44
    pub const fn new(sample_rate_hz: u32, num_channels: u16) -> Self;
    pub fn num_frames(&self) -> usize;                           // sample_rate_hz / 100  -> 480 at 48 kHz
}
```

Builder defaults: `Config::default()`, 16 kHz mono capture and render, no echo detector (`S/audio_processing.rs:318-330`).

**Construction cost (inferred, not measured):**
- `build()` initializes **twice.** `AudioProcessingImpl::with_config` calls `initialize()` with the default
  16 kHz formats (`S/audio_processing_impl.rs:172-199`), then `build()` calls
  `initialize_with_config` with the real formats (`S/audio_processing.rs:372-379`). Each pass builds
  a full `EchoCanceller3` (`S/audio_processing_impl.rs:1134-1140`), and the 16 kHz one is dropped.
- For 48 kHz mono, the AEC3 render delay buffer alone is 167 blocks (`A/common.rs`,
  `get_render_delay_buffer_size(4, 5, 13)`), held as three ring buffers: `BlockBuffer` with one
  `Vec<f32>` of 192 floats per block (~128 KB), `SpectrumBuffer` (~43 KB) and `FftBuffer` (~87 KB).
  That's roughly 300 KB and several hundred separate allocations per AEC3 instance, from nested
  `Vec<Vec<..>>` (`A/block_buffer.rs`, `A/block.rs:22-27`).
- **Rule:** build on a control thread, then move it to the DSP thread (`AudioProcessing: Send`).
  Never build or rebuild on the audio thread.

### 1.4 Minimal 48 kHz mono config: AEC3 on, NS off, AGC off, HPF switchable

```rust
use sonora::config::{EchoCanceller, HighPassFilter as HpfConfig, MaxProcessingRate, Pipeline};
use sonora::{AudioProcessing, Config, StreamConfig};

pub fn build_aec3(hpf: bool) -> AudioProcessing {
    let config = Config {
        pipeline: Pipeline {
            maximum_internal_processing_rate: MaxProcessingRate::Rate48kHz, // default is 32 kHz
            ..Pipeline::default()
        },
        echo_canceller: Some(EchoCanceller {
            enforce_high_pass_filtering: hpf, // false is required to really turn the HPF off
            ..EchoCanceller::default()
        }),
        high_pass_filter: hpf.then(|| HpfConfig { apply_in_full_band: true }),
        noise_suppression: None,
        gain_controller2: None,
        ..Config::default()
    };
    let mono48 = StreamConfig::new(48_000, 1);
    AudioProcessing::builder()
        .config(config)
        .capture_config(mono48)
        .render_config(mono48)
        .build()
}
```

With `Rate48kHz` and 48 kHz I/O, the internal rate is 48 kHz (`suitable_process_rate`,
`S/audio_processing_impl.rs:1431-1450`), so there's no resampler and no separate full-band buffer
(`S/audio_processing_impl.rs:1116-1130`). With `Rate32kHz` at 48 kHz I/O, a full-band buffer
appears, and `copy_to_buffer` then allocates a `vec!` on every frame (`S/audio_buffer.rs:322`).

### 1.5 Per-10 ms calls

```rust
pub fn process_render_f32(&mut self, src: &[&[f32]], dest: &mut [&mut [f32]]) -> Result<(), Error>;  // S/audio_processing.rs:643
pub fn process_capture_f32(&mut self, src: &[&[f32]], dest: &mut [&mut [f32]]) -> Result<(), Error>; // :615
pub fn process_render_f32_with_config(&mut self, src: &[&[f32]], input_config: &StreamConfig,
    output_config: &StreamConfig, dest: &mut [&mut [f32]]) -> Result<(), Error>;                      // :653
pub fn process_capture_f32_with_config(/* same shape */) -> Result<(), Error>;                       // :627
// i16 interleaved variants: process_capture_i16 :669, process_render_i16 :691 (do not use, see below)
```

- **Layout:** deinterleaved, one slice per channel. For mono that's `&[&buf[..]]`. The outer slice
  types coerce from stack arrays, so building the arguments doesn't allocate.
- **Length:** each channel slice needs at least `StreamConfig::num_frames()` = 480 samples. Code slices
  `&sd[..480]` and panics if a slice is shorter; extra samples are ignored
  (`S/audio_buffer.rs:269-272`, `:296-299`).
- **Range:** `[-1, 1]`. Input is **clamped** to [-1, 1] and scaled to FloatS16 (×32768) inside, and
  output is converted back and clamped (`C/audio_util.rs:46-54`). Anything above 0 dBFS clips.
- **Render `dest` is required.** For 48 kHz mono it's an exact copy of `src`, because the port has no
  render pre-processor (`S/audio_processing_impl.rs:366-386`, `S/submodule_states.rs:88-92`).
  Pass a 480-sample scratch buffer.
- **`Result`:** `Err` only comes from format validation (`S/audio_processing.rs:140-238`: rate outside
  8 k-384 k, 0 channels, output channel count not 1 or equal to input). With the fixed builder configs
  above the calls always return `Ok`. A format different from the builder's triggers a full
  re-initialization with allocation (`maybe_initialize_capture`/`_render`, `S/audio_processing_impl.rs:953-995`).
- **Avoid the i16 API:** it allocates a `vec!` per call (`S/audio_buffer.rs:365, 381, 410, 447, 473`).
- **Order and ratio:** each block, call render first and then capture, 1:1.
  - Render frames are queued (`SwapQueue`, 100 frames) and only consumed inside the next capture call
    (`S/echo_canceller3.rs:518, 653-698`). When the queue is full, frames are dropped silently
    (`S/echo_canceller3.rs:151`).
  - Until the first render block arrives, AEC3 does **not** process capture (`A/block_processor.rs:147`).
  - Persistent excess render triggers `RenderOverrun` and a buffer reset; more capture than render
    triggers `RenderUnderrun`, which shrinks the delay and resets the delay controller
    (`A/render_delay_buffer.rs:161-259`, `A/block_processor.rs:155-171`).
  - So feed exactly one reference block, zero- or dither-filled when the loopback had a gap, before
    every mic block.
- **Internals:** a 10 ms frame becomes 3 bands × 160 samples, two 80-sample sub-frames, then 64-sample
  (4 ms) AEC3 blocks, 2 or 3 per frame (`S/echo_canceller3.rs:486-545`, `A/common.rs` `BLOCK_SIZE`).

### 1.6 Delay hint, stats, reset

```rust
pub fn set_stream_delay_ms(&mut self, delay: i32) -> Result<(), Error>; // S/audio_processing.rs:580-599
pub fn stream_delay_ms(&self) -> i32;                                   // :602
pub fn statistics(&self) -> &AudioProcessingStats;                      // :537
pub fn apply_config(&mut self, config: Config);                         // :452
pub fn initialize(&mut self, input: StreamConfig, output: StreamConfig,
                  reverse_input: StreamConfig, reverse_output: StreamConfig); // :467-483
pub fn set_playout_volume(&mut self, volume: i32);                      // :512
pub fn set_capture_output_used(&mut self, used: bool);                  // :531
pub fn proc_sample_rate_hz(&self) -> usize;                             // :607
```

**`set_stream_delay_ms`**
- Values are clamped to `[0, 500]`. A clamp returns `Err(StreamParameterClamped)`, but the clamped
  value is still applied.
- The docs say it "must be called if echo cancellation is enabled"; the code treats it as optional.
- It's forwarded every frame (`S/audio_processing_impl.rs:593-595`) and converted to 4 ms blocks
  (`A/render_delay_buffer.rs:318-325`), but it's only used as the initial alignment, minus 2 blocks of
  headroom, when the render buffer resets (`A/render_delay_buffer.rs:143-158`).
- The internal delay estimator always runs (`use_external_delay_estimator` is false and can't be set).
- If it's never called, AEC3 starts from `default_delay` (5 blocks).

**`AudioProcessingStats`** (`S/stats.rs:10-39`): all fields are `Option`.

| Field | Type | Set in 0.2.0? |
|---|---|---|
| `echo_return_loss` | `Option<f64>` dB | yes, every capture call: `-10·log10(erl_time_domain)`, range about -30 to +20 dB (`A/echo_remover.rs:190-196`, `A/erl_estimator.rs:13-14`) |
| `echo_return_loss_enhancement` | `Option<f64>` dB | yes (fullband ERLE) |
| `delay_ms` | `Option<i32>` | yes: render buffer delay × 4 ms, a multiple of 4 (`A/block_processor.rs:109-116`) |
| `residual_echo_likelihood`, `_recent_max` | `Option<f64>` | only with `.echo_detector(true)` |
| `divergent_filter_fraction`, `delay_median_ms`, `delay_standard_deviation_ms` | | **never set: always `None`**, although the docs describe their aggregation |

Stats are written in phase 10 of each capture call (`S/audio_processing_impl.rs:742-749`). Read them on
the DSP thread after `process_capture_f32` and publish them through atomics, e.g. `f32::to_bits` into
an `AtomicU32`. AEC3's clock-drift flag isn't exposed.

**Reset and reinit: there is no `reset()`.**
- `initialize(..)` rebuilds every buffer and the whole AEC3, with allocation.
- `apply_config` rebuilds what changed. Toggling `echo_canceller` or `high_pass_filter` between `Some`
  and `None` changes the submodule state and forces a full reinit, rebuilding AEC3 too
  (`S/audio_processing_impl.rs:220-285`, `S/submodule_states.rs:33-59`).
- The doc says "Call `apply_config` from any thread", but it takes `&mut self`. Calling it from
  another thread would need a lock on the audio thread.
- **Pattern:** to reset, re-tune or toggle AEC, build a fresh instance off-thread, hand it over lock-free
  (rtrb), and send the old one back to be dropped off-thread. To bypass AEC, stop calling it, and
  remember the AEC latency (430 samples, 1.8) goes away with it.

**Docs vs code, other cases:**
- `set_playout_audio_device` says it's applied at the next `process_render_*`. It's actually queued on
  the capture queue and ignored (`S/audio_processing.rs:520-525`, `S/audio_processing_impl.rs:288-290, 928-930`).
- Runtime setters (`set_playout_volume`, ...) `push_back` into a `VecDeque`, which allocates on first
  growth (`S/audio_processing_impl.rs:288-290`).
- `set_playout_volume` with a changed value flags an echo-path gain change for AEC3
  (`S/audio_processing_impl.rs:518-521`). Use it when the OS speaker volume changes, since the loopback
  is captured before the volume is applied.

### 1.7 Where sonora allocates (inferred from a scan of the non-test code)

**Construction and reinit:** everything (1.3).

**Every 10 ms frame, 48 kHz mono, AEC3 only, steady state:**

| Site | Allocations |
|---|---|
| `S/echo_canceller3.rs:564-568` (`process_capture_sub_frame`) | `Vec<Vec<&[f32]>>`: 1 + 3 per sub-frame, ×2 = **8** |
| `S/echo_canceller3.rs:682-686` (`empty_render_queue`) | same shape, **8 per render frame** drained |
| `A/echo_remover.rs:225-237` (`process_capture`) | **11 `Vec`s per 4 ms block**, ~5.7 KB in total; ×2.5 blocks ≈ 27 per frame. Only once render has started. |
| `A/filter_analyzer.rs:377`, `A/adaptive_fir_filter.rs:271, 371` | `resize`; grows only when the filter length changes (12 to 13 partitions), then keeps its capacity |

That's about 43 allocations and frees per frame, with all-Rust `Vec`s through the global allocator.
The render call itself doesn't allocate. Upstream PR #40 (open) makes steady-state AEC allocation-free.
**(GitHub)** Until then, sonora on the DSP thread depends on our TLSF global-allocator router.
**0.3.0 (released 2026-11-07; our pin since 2026-11-20) ships sonora #40: no allocation per block, measured (1.11).**

Other per-call allocations, all off for us: the stereo downmix (`S/audio_buffer.rs:230`), AGC2
(`S/gain_controller2.rs:110-111, 264, 313-331`), the residual echo detector queue
(`S/residual_echo_detector.rs:82`), and `AudioConverter::convert` (`S/audio_converter.rs:206-224`,
used only when render in and out formats differ).

**Logging:** `tracing::error!` only on misconfiguration (`S/audio_processing_impl.rs:269`,
`S/echo_canceller3.rs:497`) and `tracing::trace!` on an unreachable render-setting path (:939).
Nothing logs on the normal hot path.

### 1.8 Latency added by sonora: 430 samples (8.96 ms), measured

- **Measured:** with AEC3 on at 48 kHz, the capture output lags the mic by **430 samples (8.96 ms)**.
  That is `nf_stages::AEC3_LATENCY` (`crates/nf-stages/src/aec3.rs:38-42`), the stage's
  `Stage::latency`. The `stream_offset_is_aec3_latency` test (`crates/nf-stages/tests/aec3.rs`) finds
  it with an impulse at four block phases and a silent reference, and GCC-PHAT on white noise agrees
  to within 0.1 sample. It is the same with the high-pass off, and 0 with AEC3 off.
- On voiced speech a plain cross-correlation reads about 30 samples late, because the high-pass delays
  the strong low harmonics. Measure with an impulse or GCC-PHAT.
- Add the 430 samples to the latency readout's constant part when AEC is active, on top of the 10 ms
  block.
- **The code reading underestimated it (inferred):** it found two sources, together about 4.5 ms (216
  samples). It doesn't account for the other 214 samples measured.
  - **AEC3 block framing:** `BlockFramer` starts with 64 zeros per band (`A/block_framer.rs:22-29`),
    so the capture output lags by 64 split-band samples = 4 ms (192 samples at 48 kHz). This applies
    whenever AEC is on, including before render starts.
  - **Three-band split and merge:** the crate's own test comment says "fixed delay of 24 samples and
    ~9.5 dB SNR" (`S/three_band_filter_bank.rs:327`). That's 0.5 ms, from the comment only, not
    verified.

### 1.9 Threading, Send/Sync, SIMD

- `AudioProcessing` is `Send + Sync`, statically asserted (`S/audio_processing.rs:725-730`).
- Every processing method takes `&mut self`, and there are no internal threads or locks. The
  `SwapQueue` atomics are single-owner (`S/swap_queue.rs:11-21`). One DSP thread owns the instance and
  calls render then capture.
- Vector kernels dispatch at run time: AVX2+FMA, then SSE2, then scalar. `detect_backend()` is cached
  through `cpufeatures` (`M/lib.rs:368-385`) and called in constructors (`A/block_processor.rs:53`).
- **`f32::mul_add` without `+fma`:**
  - The scalar biquad (`C/cascaded_biquad_filter.rs:76-112, 145-149`, used by the HPF and the AEC3
    decimator), the three-band filter bank (`S/three_band_filter_bank.rs:82-121`) and `fft4g`
    (`sonora-fft-0.2.0/src/fft4g.rs:509-525`) use `f32::mul_add` with no `#[target_feature]`.
  - Without FMA each `mul_add` is a call to the UCRT `fmaf` in per-sample loops. PR #37 (merged to
    main on 2026-10-09, **not in 0.2.0**) explains this and adds that under `+crt-static` UCRT's `fmaf`
    ignores FTZ/DAZ. **(GitHub)**
  - **Our build has FMA (2026-10-28):** `.cargo/config.toml` is `target-cpu=x86-64-v3`, so each
    `mul_add` is one `vfmadd` instruction and the exes import no `fmaf`; nf-stages' `tests/aec3.rs`
    (`the_build_has_fma_for_sonoras_mul_add`) doesn't build if the baseline loses FMA.
  - Before, at x86-64-v2, sonora and sonora-common-audio 0.2.0 were vendored (2026-10-26 to
    2026-10-28) with the AEC path's `mul_add`s written as plain arithmetic: AEC3 went from about 91
    to 55 µs a block on the dev VM. At v3, with crates.io's 0.2.0, it is about 50
    (`doc/dev/build-report.md` §11).

### 1.10 Denormals (sonora issue #34) and FTZ/DAZ

- **Issue #34 (open, GitHub):** with an all-zero render reference on an i9-14900K, render+capture took
  0.45-2.1 ms mean per 10 ms frame, worst about 15 ms. With FTZ/DAZ or ±1e-9 render dither it was
  44.8 / 45.4 µs mean.
  - The cause given: filter states decay into subnormals. WebRTC wraps its APM entry points in
    `DenormalDisabler`.
  - **0.2.0 has no denormal handling anywhere.** Grep for mxcsr, flush, denorm or subnormal finds
    nothing in any `sonora-*` crate.
  - PR #38 (open) adds the guard. **(GitHub)** 0.3.0 has one, upstream's `DenormalDisabler` (1.11).
- **Mitigation 1, defined behaviour:** add about ±1e-9 (−180 dBFS) of noise to every render sample,
  including zero-filled gaps, before `process_render_f32`. In FloatS16 that's about 3e-5, a normal float.
  Use a cheap xorshift with no allocation (skeleton in 3.1).
- **Mitigation 2, FTZ|DAZ:** set MXCSR bits 15 and 6 (`| 0x8040`) on the DSP thread.
  - The toolchain docs for `_mm_setcsr` (deprecated) say that changing the denormals-are-zero or
    rounding flags and then running Rust code is "immediate Undefined Behavior", and that inline asm
    must restore MXCSR before the block ends (`rust/library/stdarch/crates/core_arch/src/x86/sse.rs:1542-1550`
    in the installed nightly).
  - It's common practice in audio code, but formally outside Rust's model, and per #37 it doesn't cover
    the `fmaf` path under `+crt-static`.
  - **Our proposal:** dither always, FTZ/DAZ only if the owner accepts the caveat.
- **The "divergent filter" and stall/panic fixes in 0.2.0, according to the commit log
  (`v0.1.0...sonora-v0.2.0`)** **(GitHub)**:
  - **#15 "prevent panic when shrinking adaptive FIR filter" (issue #14).** The panic was
    `slice index starts at 13 but ends at 12` in `adaptive_fir_filter.rs`. 0.2.0 has
    `zero_filter(old, new)` returning early when shrinking, plus a `saturating_sub` clamp on
    `partition_to_constrain` (`A/adaptive_fir_filter.rs:137-146, 253-267, 324-344`) and a regression
    test (`:455-467`).
  - **"Port three post-M145 upstream fixes".** One is AEC3 choosing the refined or coarse filter once
    across all channels (`A/echo_remover.rs:287-294`); for mono that's a no-op.
  - Divergence detection itself is upstream logic (`A/subtractor_output_analyzer.rs:29-65`).
  - No release notes exist (the GitHub release body is empty). **0.2.0 contains no fix for the
    denormal stall.**

### 1.11 What 0.3.0 changed (2026-11-20)

0.3.0 (released 2026-11-07, git `1632d47c`; all seven `sonora-*` crates at 0.3.0) is the M145 port plus 15 later upstream
changes up to M156. Our code compiled unchanged. Paths below are under `sonora-0.3.0/src` and `sonora-aec3-0.3.0/src`.

- **Allocations (1.7):** sonora #40 makes echo cancellation allocation-free in steady state. Measured: AEC3 alone and the
  chain with it make **0 allocations** from the first block after construction, over 12 s (`crates/nf-stages/tests/rt_no_alloc.rs`;
  0.2.0: 43.5 a block). The filters' frequency and impulse responses are now sized for the longest filter at construction
  (`subtractor.rs:150-164`), so the `resize`s in `adaptive_fir_filter.rs:271, 371` don't allocate. **(inferred)** for the
  sizing; the count is measured.
- **Denormals (1.10):** each `process_*` call holds a `DenormalDisabler` (`denormal_disabler.rs`, used in
  `audio_processing_impl.rs:350, 388, 429, 478`): it sets MXCSR FTZ and DAZ unless both are already set and restores the saved word on
  drop, also when unwinding. Its own docs state the caveats: formally undefined behaviour in Rust, LLVM may move float
  operations across the guard, and code reached from inside the call (the global allocator, `tracing` subscribers, the
  panic hook) runs with FTZ. Measured: the undithered zero-reference row went from 674 to 46 µs a block; the thread's
  MXCSR is the same after the stage's blocks (`crates/nf-stages/tests/aec3.rs`, `aec3_leaves_the_thread_s_mxcsr_as_it_found_it`).
- **`mul_add` (1.9):** without native FMA (`sonora_simd::NATIVE_FMA`, false at x86-64-v2) the biquads, the three-band filter
  bank and `fft4g` use plain expressions (sonora #37). At our v3 baseline it is `true` and the code is unchanged.
- **Render buffer:** the headroom fix for render-buffer underruns (upstream d460e60e19). We call render then capture once
  each per block, so offline it never underruns.
- **Config:** `AudioProcessingBuilder::echo_canceller3_config` (sonora #41, #47), with `EchoCanceller3Config` re-exported
  from `sonora::config`; invalid configs are clamped with a `tracing::warn!` (`audio_processing_impl.rs:1327`). Unused by us.
- **Logging:** besides 1.7's, `tracing::info!("Denormal disabler unsupported")` when the processor is built on a target
  without FTZ (not ours). Nothing logs on the hot path.
- **Output, measured:** bit-identical to 0.2.0 on our config (`tests/refactor_parity.rs`' 20 s golden, with and without AEC3),
  the latency still 430 samples, and AEC3 p50 49.5 → 47.0 µs a block (`doc/dev/build-report.md` §12).

---

## Part 2: nnnoiseless 0.5.2

### 2.1 API

```rust
pub struct DenoiseState<'model> { /* lastg, rnn: RnnState<'model>, feat: DenoiseFeatures */ } // N/denoise.rs:36-42, #[derive(Clone)]
impl DenoiseState<'static> {
    pub const FRAME_SIZE: usize = FRAME_SIZE;                       // :46  (= 480)
    pub fn new() -> Box<DenoiseState<'static>>;                     // :53-55 (built-in RNNoise weights)
    pub fn from_model(model: RnnModel) -> Box<DenoiseState<'static>>; // :61
}
impl<'model> DenoiseState<'model> {
    pub fn with_model(model: &'model RnnModel) -> Box<DenoiseState<'model>>; // :72
    pub fn process_frame(&mut self, output: &mut [f32], input: &[f32]) -> f32; // :95-116
}
pub const FRAME_SIZE: usize = 120 << FRAME_SIZE_SHIFT;  // N/lib.rs:36-38, #[doc(hidden)]; prefer DenoiseState::FRAME_SIZE
impl RnnModel {
    pub fn from_bytes(bytes: &[u8]) -> Option<RnnModel>;               // N/rnn.rs:75 (copies)
    pub fn from_static_bytes(bytes: &'static [u8]) -> Option<RnnModel>; // :92 (borrows, no alloc)
}
```

There is **no reset method.** `default()` is `pub(crate)` (`N/denoise.rs:48`).

### 2.2 `process_frame` contract

- **Lengths:**
  - `input.len()` must be exactly 480, enforced by `assert!` (`N/features.rs:98`).
  - `output` needs at least 480; the code writes `out[0..480]` (`N/features.rs:271-274`).
  - The borrow rules stop `input` and `output` aliasing, so in-place use needs a copy.
- **Scale:** "in the range `[-32768.0, 32767.0]` instead of [-1, 1]" (`N/denoise.rs:86-90`). The crate's
  own dasp adapter multiplies by 32768 on the way in, and divides by 32768 and clamps on the way out
  (`N/signal.rs:97-98, 125`).
  - The output isn't clamped. Clamp after scaling back.
  - **Feeding [-1, 1] silently disables the denoiser:** a frame counts as "silent" when its summed band
    energy is below 0.04 (`N/features.rs:160-166`). **(inferred)** That's roughly RMS < 0.2 in i16
    units, so at [-1, 1] scale anything quieter than about −14 dBFS RMS is treated as silent.
- **Return value:** the VAD probability, from a sigmoid output in [0, 1].
  - The model is `dense 42→24 tanh`, VAD `GRU 24` (relu), noise `GRU 48`, denoise `GRU 96`, then
    `dense 96→22` sigmoid gains and `dense 24→1` sigmoid VAD (parsed from `N/weights.rnn`; layer
    order in `N/rnn.rs:189-194`).
  - On a "silent" frame it returns **0.0**. The RNN isn't run and its state isn't advanced, the gains
    stay 1.0 (pass-through), and `lastg` isn't updated (`N/denoise.rs:96-115`).
- **Always applied:** the input goes through a fixed DC-blocking biquad, double zero at DC,
  `BIQUAD_HP` (`N/util.rs:68-71`, `N/features.rs:97-104`). The output is high-passed even at gain 1.
- **Post-gain smoothing:** `g[i] = max(g[i], 0.6 * lastg[i])`, so gains can fall by at most a factor of
  0.6 per frame (`N/denoise.rs:106-109`).
- **NaN/Inf input is undefined behaviour (inferred):**
  1. NaN in the features makes `rf` NaN in `pitch_filter` (`N/features.rs:235`).
  2. The DC bin's imaginary part becomes `0·NaN`.
  3. realfft's C2R returns `Err(InputValues)` for a non-zero imaginary part at DC
     (`realfft-3.5.0/src/lib.rs:671-677, 752-754`; the odd-length path is at 589-591).
  4. easyfft calls `unwrap_unchecked()` on that `Err` (`E/dyn_size/realfft.rs:122, 168`).

  Sanitize the input first (replace non-finite values with 0).

### 2.3 Latency: one frame (480 samples, 10 ms), shown from the code

1. `shift_and_filter_input` shifts `input_mem` (length 1728) left by 480 and writes the new frame
   at the end (`N/features.rs:97-104`).
2. `transform_input(.., lag 0)` windows the last 960 samples, `[frame k-1 | frame k]`
   (`N/features.rs:281-290`).
3. `frame_synthesis` computes `out[i] = ifft_half1[i] + synthesis_mem[i]` and stores the second half in
   `synthesis_mem` (`N/features.rs:263-275`).
   - The first half of window k covers the time span of frame k-1.
   - `synthesis_mem` is the second half of window k-1 = `[k-2 | k-1]`, which also covers frame k-1.
4. So **output block k is the overlap-add reconstruction of input block k-1.** With unit gains it
   reconstructs exactly: the window is power-complementary, the 1/Σw² factor is 1/480, and the
   unnormalized IFFT of 960 points with the `/2` gives ×1. **(inferred)**
5. Evidence in the crate: its test and doc example throw away the first output and then match the
   reference sample for sample (`N/lib.rs:196-213`, `N/denoise.rs:24-34`), and `DenoiseSignal` calls
   `discard_first_frame()` (`N/signal.rs:83`).

**First frame:** `input_mem` and `synthesis_mem` start at zero, so the first output is about zero, with
small "fade-in artifacts" (`N/denoise.rs:92-94`). In real time we keep it, and the pipeline simply carries
10 ms of latency.

**Consequences:**
- **The dry/wet floor must mix with the input delayed by one block** (keep the previous `rnn_in`).
  Mixing with the current input creates a 10 ms comb filter.
- The VAD returned by call k comes from the window `[k-1 | k]`. Use it to gate that call's output.

### 2.4 Allocation, threading, size

- **Construction:** `Box` of the state, plus 3 GRU state `Vec`s (`N/rnn.rs:332-334`), 2 `DynRealDft`
  bin buffers made with `concat` (`E/dyn_size/realfft.rs:233-244`) and 4 `PitchFinder` `Vec`s
  (`N/pitch.rs:26-29`).
  - The built-in weights are `include_bytes!` and borrowed, so they aren't allocated (`N/rnn.rs:235-240`).
  - `Clone` (derived) allocates the same set again.
- **Steady state:** `process_frame` doesn't allocate. Gains are applied in place through `MulAssign`
  (`E/dyn_size/realfft.rs:323-356`), and the stack buffers are fixed arrays.
- **First call on each thread allocates, and the allocation is leaked:**
  - Each FFT goes through `generic_singleton::get_or_init_thread_local!` (`E/lib.rs:57-99`). That
    creates a per-thread `RealFftPlanner` plus per-length scratch and input-copy `HashMap<usize, Box<[_]>>`
    (`E/dyn_size/realfft.rs:93-175`).
  - Each value lives in a `Box::leak`ed `OnceCell` that is never dropped
    (`G/thread_local_static_anymap.rs:24-42`).
  - One `process_frame` call exercises both the forward FFT (twice) and the inverse FFT, even on a
    silent frame. So call it once with zeros **on the DSP thread itself** before going live.
  - A zero frame leaves the RNN state untouched (silence path).
  - Each new DSP thread repeats the setup and leaks again. Under the TLSF router those blocks
    permanently occupy the locked arena, so keep the DSP thread long-lived.
  - The global `COMMON` window/DCT table is a `once_cell::sync::OnceCell`, filled on first use, with no
    heap (`N/lib.rs:99-136`).
- **Steady-state FFT overhead:** each of the 3 FFTs per frame does 3 TLS/AnyMap lookups, 2
  `HashMap<usize,_>` lookups, `RefCell` borrows, an `Arc` clone and drop (atomic), and copies its input
  first (`E/dyn_size/realfft.rs:112-116, 156-161`). The cost is small but worth measuring.
- **SIMD:** rustfft picks AVX/SSE at run time. The RNN's `i8` mat-vec (`N/rnn.rs:396-410`) is plain
  Rust, so it only uses what `target-cpu=x86-64-v3` allows (AVX2 since 2026-10-28; RNNoise went from
  about 40 to 36 µs a block, `doc/dev/build-report.md` §11). That's where hot-loop work would pay
  off, but it needs a fork.
- **Send/Sync:** `DenoiseState: Send + Sync`, statically asserted (`N/denoise.rs:123-125`), so
  `Box<DenoiseState<'static>>` can be built anywhere and moved. The easyfft TLS state is per thread and
  doesn't move with it.
- **Reset:** build a new `DenoiseState::new()` off-thread and swap it in. Don't `clone()` on the audio
  thread.
- **`size_of::<DenoiseState>()` is about 14.6 KB inline (inferred, not measured):** `input_mem` 1728,
  `window_buf` 960, `synthesis_mem` 480 and `cepstral_mem` 176 `f32`s, plus about 1 KB of small fields.
  - Heap adds about 7.7 KB (2 × 481 `Complex32`), about 8.7 KB of pitch buffers and 672 B of GRU state.
  - `new()` builds the value on the stack before boxing, so it needs a normal thread stack.
  - Check with `size_of` in a test.
- **`default-features = false`** drops `bin` (`anyhow`, `clap`, `dasp_interpolate`, `dasp_ring_buffer`,
  `hound`) and `dasp` (`DenoiseSignal` and the `pub use dasp`). That leaves `easyfft` and `once_cell`
  (`nnnoiseless-0.5.2/Cargo.toml` `[features]`). `capi` and `train` are off by default anyway.
- **Cost:** research measured about 0.36% of a core at p50 and 0.77% at p99 (7800X3D, Windows, LTO;
  `doc/research-2026-10.md:29`). Not re-measured.

---

## Part 3: skeletons

### 3.1 One 10 ms block: AEC3 (reference + mic), then RNNoise

```rust
use nnnoiseless::DenoiseState;
use sonora::AudioProcessing;

pub const BLOCK: usize = 480; // == DenoiseState::FRAME_SIZE == StreamConfig::new(48_000, 1).num_frames()
const S16: f32 = 32768.0;
const INV_S16: f32 = 1.0 / 32768.0;

pub struct AecRnn {
    apm: AudioProcessing,              // built off-thread by build_aec3(), then moved here
    rnn: Box<DenoiseState<'static>>,   // DenoiseState::new() off-thread; warm_up() on this thread
    ref_buf: [f32; BLOCK],
    render_scratch: [f32; BLOCK],      // process_render_f32 copies src here; contents unused
    aec_out: [f32; BLOCK],
    rnn_in: [f32; BLOCK],              // i16 scale
    rnn_out: [f32; BLOCK],             // i16 scale, = rnn_in of the PREVIOUS block, denoised
    dry_prev: [f32; BLOCK],            // previous rnn_in: the dry signal aligned with rnn_out
    rng: u32,                          // xorshift state, non-zero
}

#[inline(always)]
fn dither(state: &mut u32) -> f32 {
    let mut x = *state;
    x ^= x << 13; x ^= x >> 17; x ^= x << 5;
    *state = x;
    (x as i32 as f32) * (1.0 / 2_147_483_648.0) // [-1, 1)
}

impl AecRnn {
    /// Call once on the DSP thread after MMCSS registration, before the streams start.
    /// It does the easyfft per-thread setup (allocates and leaks it once) and starts AEC3's render path.
    pub fn warm_up(&mut self) {
        let silence = [0.0f32; BLOCK];
        let mut out = [0.0f32; BLOCK];
        for _ in 0..4 { self.process(&silence, &silence, &mut out, 1.0, 0.0); }
    }

    /// `mic` and `reference` are in [-1, 1] at 48 kHz. Pass a zero-filled reference on loopback gaps.
    /// Returns the RNNoise VAD probability. `wet` and `dry` are linear gains for the attenuation floor.
    pub fn process(&mut self, mic: &[f32; BLOCK], reference: &[f32; BLOCK],
                   out: &mut [f32; BLOCK], wet: f32, dry: f32) -> f32 {
        // 1. Reference first, dithered at -180 dBFS so a silent reference can't go subnormal (sonora #34).
        for (d, s) in self.ref_buf.iter_mut().zip(reference) {
            *d = *s + 1e-9 * dither(&mut self.rng);
        }
        let r = self.apm.process_render_f32(&[&self.ref_buf[..]], &mut [&mut self.render_scratch[..]]);
        debug_assert!(r.is_ok());

        // 2. Mic. This drains the render queue and runs AEC3; the output lags by 430 samples (1.8).
        let r = self.apm.process_capture_f32(&[&mic[..]], &mut [&mut self.aec_out[..]]);
        debug_assert!(r.is_ok());

        // 3. RNNoise wants i16 scale. Replace non-finite samples with 0 (NaN reaches unwrap_unchecked).
        for (d, s) in self.rnn_in.iter_mut().zip(&self.aec_out) {
            *d = if s.is_finite() { *s * S16 } else { 0.0 };
        }
        let vad = self.rnn.process_frame(&mut self.rnn_out[..], &self.rnn_in[..]);

        // 4. Dry/wet: rnn_out is block k-1, so mix it with dry_prev (also block k-1). Back to [-1, 1].
        for i in 0..BLOCK {
            out[i] = ((wet * self.rnn_out[i] + dry * self.dry_prev[i]) * INV_S16).clamp(-1.0, 1.0);
        }
        self.dry_prev.copy_from_slice(&self.rnn_in);
        vad
    }

    /// Publish AEC stats (call after process). ERL/ERLE/delay_ms are the only populated fields.
    pub fn aec_stats(&self) -> (Option<f64>, Option<f64>, Option<i32>) {
        let s = self.apm.statistics();
        (s.echo_return_loss, s.echo_return_loss_enhancement, s.delay_ms)
    }
}
```

The stream offset of this chain is 430 samples (AEC3, 8.96 ms, measured, 1.8) plus 480 (RNNoise,
10 ms, 2.3): 910 samples (18.96 ms), on top of the 10 ms block and the device buffers. With AEC
bypassed, the 430 samples go away. `Chain::latency` reports the same sum
(`crates/nf-stages/tests/chain.rs`).

### 3.2 Hand-over (no allocation or drop on the audio thread)

Build `AudioProcessing` and `Box<DenoiseState>` on the control thread, then send them to the DSP thread
through an `rtrb` ring. There, swap them with `std::mem::replace` and push the old ones back on a return
ring so they're dropped off-thread. Call `warm_up()` again only when the DSP thread itself is new; the
easyfft setup is per thread, not per state.
