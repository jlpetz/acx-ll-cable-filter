# Build report: v0 integration (2026-10-26)

nf-app was written and the six crates were integrated. Everything here was built and tested on the dev box (Windows
Server 2025 VM over RDP, no audio devices, rustc 1.101.0-nightly 1a2c4797e, `CARGO_TARGET_DIR=target/agent-app`).
**Nobody has run the exe.** CLAUDE.md leaves that to the owner, so the window, every device path and the console
behaviour below are untested. The offline paths are covered by tests.

## 1. What was built: nf-app (`noise-filter.exe`)

| File | What it does |
|---|---|
| `main.rs` | The `RtAllocator` global allocator. Start-up order: log, panic hook, console, then memory (lock-pages privilege, `install_pool(Large, 32 MiB)`, `set_working_set_minimum(64 MiB)`) for the commands that run the engine or real-time code (`gui`, `run`, `latency`, `bench`). Then dispatch, the exit code, and the message box for a window that fails to open (none for a missing Windows App Runtime, where Reactor shows its own prompt). Documents the window-or-console choice. |
| `cli.rs` | Hand-rolled parser and help text. Takes `--opt value` and `--opt=value`, accepts values that start with `-`, and range-checks everything. |
| `console.rs` | `AttachConsole(ATTACH_PARENT_PROCESS)` for CLI commands. Fixes the standard handles (redirected files and pipes are kept). Allocates a console, and pauses for Enter, when there is no parent console. The Ctrl+C / close handler (`SetConsoleCtrlHandler`). |
| `logger.rs` | File logger at `%LOCALAPPDATA%\noise-filter\noise-filter.log`, rotating to `.log.old` at 1 MiB, with `log::set_max_level(Info)`. Each line carries the time, pid and thread. The panic hook logs the message, location and backtrace; in the window, a release (abort) build also shows a message box. |
| `config.rs` | `--config` reads either the window's `settings.json` (`{engine, ui}`) or a bare `EngineConfig`, with or without a BOM. Command-line overrides are applied on top (`--threshold` implies the static gate). Device names resolve to ids in this order: exact id, then exact name (any case), then the first name that contains the text. |
| `wav.rs` | Reads 48 kHz WAV (8/16/24/32-bit int or 32-bit float, any number of channels, averaged to mono). Writes 48 kHz mono f32 and checks `finalize`. |
| `commands/` | `devices`, `run` (prints a status line each second; Ctrl+C stops cleanly), `latency` (2 s settle, a progress line every 10%, the report, a stuck-test deadline), `process`, `bench`, `setup-large-pages`. |

**Window or console.** The exe uses the windows subsystem with `AttachConsole`. TMR's console subsystem plus manifest
was rejected because builds 22000-26099 ignore the manifest and flash a console, which app.md rules out. The
PE header was checked: subsystem 2 (GUI). What each launch does:
- A double-click, or `gui`, opens no console on any build.
- `cargo run -- …` and scripts wait for the program and see its output and exit code.
- Interactive cmd.exe and PowerShell don't wait for a windowed program. Use `start /wait` in cmd, or pipe the output in
  PowerShell. The help text says so. Since the review fixes, the console launcher `noise-filter-cli.exe` (or a copy
  named `noise-filter.com`) waits in both shells and passes Ctrl+C and the exit code through (§9).

**Exit codes** (app.md): 0 OK; 1 usage; 2 engine, device or file error; 3 no valid latency result, or the latency
test was cancelled. The elevated `setup-large-pages --sid` helper exits 0, 1 or 2 and opens no console. Its
non-elevated caller explains the code. Since the review fixes it is marked by the hidden `--elevated-helper` flag and
exits 4 when it starts without elevation, instead of relaunching itself (§9).

**Additions beyond app.md:**
- `process --align` drops the chain latency and flushes the tail, so `out[n]` lines up with `in[n]`.
- `process --config`.
- `--no-aec` and `--ns`, so a setting from `--config` can be overridden.
- `latency --through-model`.
- `help` / `-h` / `/?`.

**`process` output.** By default it is the live stream: as long as the input and lagging it by the chain latency,
which is printed.

**`bench`** runs `nf_stages::bench`, the same code as `tests/timing.rs` (§5). It times RNNoise, AEC3 (with a synthetic
far end as its reference) and the whole chain, which includes AEC3 with `--aec`. The stages are built and timed
inside an `RtScope`, and each row reports `thread_scope_allocs()` growth after the warm-up.

## 2. Tests: 323 pass, 7 ignored

| Crate | Pass | Ignored | Notes |
|---|---|---|---|
| nf-app | 44 | 0 | See below. |
| nf-core | 72 unit + 3 `rt_no_alloc` | 1 + 1 (`tuning_sweep`, `timing`) | 2 new `Dither` tests. |
| nf-engine | 70 | 3 | `real_drift_loop_and_resampler` and `the_audio_path_never_allocates_with_the_real_drift_loop` were also run with `--ignored` and pass. `lists_devices` was not run (it enumerates devices). |
| nf-gui | 48 | 0 | |
| nf-mem | 40 unit + 5 `global_alloc` | 1 (`large_pages_region`, owner only) | |
| nf-stages | 19 unit + 8 aec3 + 4 chain + 5 rnnoise + 5 `rt_no_alloc` | 1 (`timing`) | 3 new `bench` tests. Also passes in `--release`. |

nf-app's tests:
- **Parser:** every command's grammar, value checks, errors and help.
- **Config:** settings-file and bare formats; overrides; device resolution order and direction; the missing
  `--output`.
- **Logger:** line format, append, rotation and reopen; level gating; panic text.
- **Console:** allocation decisions and the redirection rules.
- **WAV I/O:** f32 round trip; int widths 8/16/24/32 with a stereo downmix; wrong rate; missing and junk files.
- **Status, report and device-list formatting.**
- **`process` on synthetic WAVs in a temp folder:**
  - stages off is bit-exact;
  - RNNoise lags by 480 samples (GCC-PHAT, within 1 sample), and `--align` brings that to 0 and equals the raw stream
    shifted;
  - stereo i16 input with a config file;
  - `--ref` cancels an echo by more than 15 dB against a silent reference;
  - the warnings for a short reference and a reference without `--aec`;
  - error cases and empty input.
- **`bench`:** 2 s runs, synthetic and from a file. With the real `RtAllocator` as global allocator, RNNoise and the
  RNNoise-only chain make 0 allocations after the warm-up, and AEC3's are counted.

## 3. Clippy

`cargo clippy --workspace --all-targets -- -D warnings` is clean, with `--no-deps` not needed.

## 4. Release exe

`target/agent-app/release/noise-filter.exe` is **5,193,728 bytes (4.95 MiB)**, built with fat LTO, `panic = "abort"`,
line tables and x86-64-v2. The PDB beside it is 21.8 MB.

## 5. Timing (release, this VM)

`cargo test --release -p nf-stages -- --ignored --nocapture`: per block over 30 s, after a 1 s warm-up, in µs.

| Row | p50 | p99 | max | mean |
|---|---|---|---|---|
| RNNoise (noisy speech) | 39.7 | 54.0 | 65.4 | 40.4 |
| AEC3 (echo + near end + noise) | 90.8 | 114.5 | 170.3 | 91.1 |
| AEC3, silent reference (dithered) | 86.0 | 97.3 | 106.0 | 80.1 |
| sonora, zero reference, no dither | 686.7 | 740.8 | 884.0 | 600.8 |
| chain: meters + AEC3 + RNNoise | 130.3 | 155.3 | 178.1 | 130.7 |
| chain: meters + RNNoise | 39.9 | 54.6 | 62.5 | 40.7 |

The chain uses 1.3% of a 10 ms block at p50 and 1.6% at p99. The dithered silent-reference row still shows that the
dither keeps sonora out of its denormal stall, also after the `Dither` change below.

Glue kernels, from `cargo test -p nf-core --release --test timing -- --ignored --nocapture`, in ns per block:

| Kernel | ns |
|---|---|
| Meter | 60 |
| DelayLine | 29 |
| crossfade | 83 |
| FloorMix, steady | 54 |
| FloorMix, ramping | 135 |
| Gate | 81 |
| NoisePost | 227 |
| Slip 481→480 | 306 |
| Slip 480→481 | 292 |
| **Dither** | **793 → 150** |
| DriftController | 4 |

## 6. Hot-loop analysis

**How it was run:**
```
cargo rustc --release -p nf-app --bin noise-filter -- --emit asm
python tools/hot_loop_spills.py target/agent-app/asm-{before,after}/noise_filter.s
```
The asm is the bin's, post-LTO (202 MB). Copies are kept under `target/agent-app/asm-before/` and `asm-after/`.

**Before the fix**, the audio thread's only per-sample problem was `Dither::apply`. It is a serial xorshift32
recurrence, inlined at six sites in `nf_engine::audio::run` plus one in `Aec3Stage::process`, and runs three times a
block: the engine's mic, the engine's reference, and AEC3's reference. It cost 793 ns a block. The tool counted it as
6 round-trip loops (the in-place block on the stack) and 10 scalar loops.

**The fix** (nf-core `rt.rs`): eight independent xorshift32 lanes over `as_chunks_mut::<8>`. The i32-to-f32 conversion
is signed, so it compiles to `cvtdq2ps`. The loop is now packed `pslld/psrld/pxor/cvtdq2ps/mulps/addps` with no stack
traffic. It takes 150 ns, or 0.45 µs a block for all three calls, against 2.4 µs before.

| function (selected rows) | round trip simd | reload simd | scalar | indexed |
|---|---|---|---|---|
| `nf_engine::audio::run`, before | 6 | 2 | 10 | 3 |
| `nf_engine::audio::run`, after | 0 | 2 | 1 | 0 |
| `Aec3Stage::process`, before | 0 | 0 | 1 | 3 |
| `Aec3Stage::process`, after | 0 | 0 | 0 | 2 |
| `nf_core::rt::Dither::apply`, after (no longer inlined) | 0 | 0 | 0 | 1 |

| totals (round trip gpr/simd, reload gpr/simd, panic, scalar, indexed, const, long) | functions |
|---|---|
| before: 1 / 6 / 10 / 5 / 8 / 45 / 114 / 6 / 3 | 23 |
| after: 1 / 0 / 10 / 5 / 8 / 35 / 111 / 6 / 3 | 24 |

**The flags left are not per-sample audio-thread problems:**
- **Off the audio thread** (selected only because their crate prefix is in `HOT`):
  - `nf_stages::synth::*` and `bench::Signals::with_echo`: bench signal synthesis;
  - `nf_engine::control::*`, `errors::lower_first` and `ensure_control_thread`: the control thread;
  - `latency_test::{send_tap, build_report, measure, conditions}` and `start::{closure}`: the latency-test thread.

  Every gpr reload, every panic flag and all the libm and `rt` call listings come from these, apart from the
  once-a-second statistics below.
- **Once a second, on the audio thread:** `RollingStats<100>::{percentile, max}`. Selecting over 100 values costs about
  1 µs a second. Its panic flags are bounds checks inside std's `select_nth_unstable`, and the remaining
  `audio::run` reload/scalar loops are `max`'s fold.
- **Only on a control change:** `NoisePost`'s `powf`/`exp` loops are behind bit-compare caches (the floor, the range,
  the gate's time constants).
- **Data, not spills:** the `run_slot` reload is the crossfade reading a block copy on the stack.
- **Known and small:** `SlipResampler`'s scalar remainder, on slipped blocks only.
- **Capture downmix:** `CaptureStream::read_packet` has 12 scalar loops, one per format and channel-count shape, each
  adding a frame's channels in order. That is a missed vectorisation of interleaved loads, at under 0.5 µs a packet.
  It is left as a follow-up, because the fix isn't clear-cut without a measurement.

There are no bounds checks, calls or register spills left in any per-sample loop on the audio thread.

## 7. Integration changes outside nf-app

1. **nf-stages, new `pub mod bench`** (`src/bench.rs`, an addition): `Signals::{synthetic, with_near}`,
   `BlockTimes` (`p50`, `p99`, `max`, `mean`, `line`), `time_blocks`, `rnnoise`, `aec3`, `aec3_silent_reference`,
   `sonora_undithered`, `chain`, `WARM_UP` and `AllocCounter`. `tests/timing.rs` now calls it, with the same six rows and
   signals, and `noise-filter bench` calls it too.
   - The raw-sonora row now uses the stage's own config. `aec3::build_apm` became `pub(crate)` for this.
   - The old test's hand-built config lacked `enforce_high_pass_filtering`.
   - `lib.rs` gained the module and a doc line.
2. **nf-core, `rt::Dither`** is now eight xorshift32 lanes (§6). The public API is unchanged: `const fn new(seed)` and
   `apply(&mut self, &mut [f32], amplitude)`.
   - The noise sequence differs, with the same distribution (uniform within ±amplitude, now checked for mean, variance
     and whiteness).
   - Lanes continue across calls that end on whole chunks of 8. All engine and stage tests pass unchanged.
   - `tests/timing.rs` gained a `Dither::apply` row.
3. **Cargo.lock**: nf-app's entry gained `serde_json` and `windows`, both already in the workspace. This was
   automatic.

No public signature changed anywhere. No other file outside `crates/nf-app` changed, apart from this report.

## 8. Known gaps

- **Nothing has run live:**
  - the window, the streams, MMCSS, `run`, `latency`, `devices` and `setup-large-pages`;
  - the console behaviour: attach, allocate, Ctrl+C, the close event, and redirection under cmd, PowerShell and
    Windows Terminal;
  - the panic message box.
- **Interactive shells don't wait** for the windowed exe, and `%ERRORLEVEL%` is lost there. If that matters more than
  the 22000-26099 flash, the alternatives are:
  - TMR's console subsystem plus manifest, which works only from 26100;
  - a small console-subsystem `noise-filter.com` shim beside the exe.
- **The large-page path is untested here.** `install_pool(Large)` runs only in the exe. Tests use no pool or 4 KB
  pages.
- **Log location of an over-the-shoulder elevation:** the elevated helper logs to the elevating admin's
  `%LOCALAPPDATA%`, which the non-elevated caller's message doesn't point to.
- **Stale docs, not edited (outside scope):**
  - `doc/dev/api/hot-loop-tools.md` pitfall 1 says nf-app's `main` is empty;
  - its §3 and the stages report call `Dither` a scalar recurrence;
  - pitfall 5 still describes DelayLine's old indexing.
- **Tool selection:** `tools/hot_loop_spills.py` selects off-thread code under `nf_stages::{synth, bench}` and
  `nf_engine::{control, errors, latency_test}`. Adding those to `COLD_PATHS` would leave only audio-thread functions in
  the table. `latency_test` holds no audio-thread code: the tap push is in `audio.rs`.
- **`bench` runs on the main thread at normal priority**, not under MMCSS.
- **Capture downmix vectorisation:** see §6.

## 9. Review fixes (2026-10-26)

The independent review of 2026-10-26 confirmed 36 of its 39 findings. It rejected clocks-latency F3 and F4 and
performance F2. Six fix agents (core, engine, gui, app, stages, tools) fixed them, each in its own crate. They also did
three known items: the gate redesign (the minimum tracker), the output fill interpolation and the hot-loop tool's
selection. An integration pass followed (§9.2). Everything was built and tested on the dev VM with
`CARGO_TARGET_DIR=target/fix-integrate`. **Nobody has run the exe**: the window, the devices, the consoles, the
launcher and the elevation paths are as untested as in §8.

The review file's fix texts are cut off mid-sentence for most findings. Where they were, the agents worked
from the visible intent; the choices that matter are noted below.

### 9.1 What each finding got

| Finding | Crate | Fix | Regression tests |
|---|---|---|---|
| **wasapi/A** (HIGH): a fault with no endpoint event left the filter off | nf-engine | A timed retry, 0.5 s doubling to 10 s (`control::Schedule`). It is armed by a device-related fault (DEVICE_INVALIDATED, RESOURCES_INVALIDATED, DEVICE_IN_USE, SERVICE_NOT_RUNNING, E_NOTFOUND), a mic stall, a dead thread, or a transient start failure (`StartFailure::transient()`). A successful start, `stop()`, or a cause a retry can't fix disarms it. | `the_timed_retry_backs_off`, `a_fault_arms_the_retry`, `transient_failures_are_retried` |
| **DSP-1**: switching a stage on faded to silence, then clicked | nf-core | A stage that didn't run last block is reset, then primed with the previous block's input (and reference) before its output is used. | The switch block equals the input delayed by the latency, at 480 and 430 |
| **DSP-3**, **gui/F4**: meters showed 1 block in 5 (GUI) or 100 (CLI), no peak hold | nf-core, nf-engine | `nf_core::dsp::meter::MeterBallistics`: peak held 1 s, then falling 20 dB/s; RMS averaged over 300 ms in the power domain. It runs on the audio thread in `Pipeline::publish_block` and resets when the meters are off. Found on the way: an Inf peak was held for good (now skipped), and both values now flush to 0 below -200 dB. | nf-core: hold, fall, restart, averaging, NaN/Inf, priming, reset. nf-engine: `published_meters_hold_peaks_and_average_the_rms` |
| **gui/F2**: each slider step queued a stream restart | nf-engine | `apply()` compares against the previous wanted config. A restart is judged when it comes due, against the run as it is then, so a processing-only change, or AEC turned off, never restarts. Restarts are gathered into one: 200 ms after the latest change, at most 1 s after the first. | `what_an_apply_asks_for`, `settings_changes_are_gathered_into_one_restart` |
| **gui/F3**, **wasapi/D**: changes and device events during a start were dropped | nf-engine | Device events that arrive during a start are held, then re-sent. Every start ends in `settle_start`, which puts the latest controls live. If devices, I/O or AEC changed meanwhile, it asks for another restart (a retry after a failed start). `apply()` writes the live controls under the `desired` lock. Lock order: desired, status, current. | `changes_during_a_start_are_judged_when_it_finishes` |
| **app-privilege/F1**: in PowerShell, Ctrl+C never reached `run` | nf-app | A new console-subsystem bin, `noise-filter-cli` (`launcher.rs`). It runs `noise-filter.exe` from its own folder with the same command line and inheritable copies of its std handles, lets Ctrl+C through to the child, waits, and exits with the child's code. No arguments or `gui` starts the window and returns at once. Copied or renamed to `noise-filter.com`, it becomes the default in both shells (PATHEXT). Stopgap: `run` and `latency` warn once when PSReadLine has cleared ENABLE_PROCESSED_INPUT. `default-run = "noise-filter"` keeps `cargo run` working. | 6 launcher tests (cmd.exe stands in for the app: exit codes, quoting, with and without std handles); `CtrlCWatch` warns once |
| **wasapi/B**: losing the reference or keep-alive killed the mic path | nf-engine | The fault is no longer fatal. The audio thread stops both streams once, calls `Pipeline::drop_reference()` (the chain then gets dithered zeros) and records it in `AudioShared::reference_fault`. The control side shows a warning once. Beyond the fix text: losing only the reference device (Removed, or a non-active state) is now a warning rather than a restart, and a reference that fails to start no longer fails the start. | `a_dropped_reference_serves_dithered_silence`, `reference_faults_name_their_stream`, `a_lost_reference_is_a_warning` |
| **wasapi/C**: a reference that wasn't there at start was never attached | nf-engine | When an explicitly picked reference that isn't attached appears (Added or Active) and AEC3 is built, the engine restarts to attach it. Following the default is unchanged. | `an_explicit_reference_is_attached_when_it_appears` |
| **clocks-latency/F1**: false underruns, with zeros padded on top of the mic backlog | nf-engine | `push_output` counts the whole mic blocks still queued. Zeros are written only when the padding is under a period and padding + pending + backlog fall short of the target, and only what the backlog won't cover. The drift loop still reads the output fill alone: feeding it the backlog as well gave 12 false underruns at two mic blocks per wake with 3 ms of jitter. | `a_late_wake_with_a_backlog_is_no_underrun`, `a_real_deficit_pads_only_what_the_backlog_wont_cover`, `two_blocks_per_wake_with_jitter_are_no_underruns` |
| **clocks-latency/F2**: a dead band of 2-3 blocks took 15-25 s to remove | nf-engine | Over target + 2 blocks (backlog included), the output drops whole blocks (pending samples, this block, then later blocks of the burst) until it is within half a block of the target, counted once per burst. The reference decides on its interpolated fill with the same threshold, and its first block aligns the FIFO without counting a drop. | `overfill_past_two_blocks_drops_back_to_within_half_a_block`, `a_burst_after_a_mic_stall_drops_its_excess`, `reference_coarse_corrections_have_no_dead_band` |
| **performance/F1**: sonora's `mul_add` called UCRT `fmaf` at x86-64-v2 | nf-stages, root `Cargo.toml` | sonora 0.2.0 and sonora-common-audio 0.2.0 are vendored under `vendor/` (licences kept) and patched in with `[patch.crates-io]`. The 18 `mul_add` calls on the AEC path (three-band filter bank, cascaded biquad) go through `madd()`, which fuses only under `target_feature = "fma"`. AEC3 p50 went from 90.8 to 55.0 µs and the chain from 130.3 to 96.5 µs (§9.5). The bin's `fmaf` call sites went from 162 to 60, all in sonora-fft's `fft4g`, which only sonora's noise suppressor reaches, and we leave that off. `vendor/README.md` says how to drop the patch once sonora PR #37 ships. | `tests/sonora_patch.rs`: bit-exact against same-order ports, unfused at v2, fused with FMA; it fails against unpatched upstream at v2 |
| **DSP-2**: reported gain reduction ignored the floor mix | nf-core, nf-engine, nf-gui | `NoisePost` measures what leaves the stage (floor mix and gate included) against the aligned input, clamped to [-120, 0]. Telemetry `gate_gain_db` is renamed `noise_reduction_db`, and the GUI reads "Reduction: x dB". | The -6 dB floor case now expects -5.18 dB; a silent model under a -10 dB floor reports -10 (was -120); GUI `reduction_and_speech` |
| **DSP-4**: speech and reduction readouts froze while the stage was off | nf-engine | Both are stored as NaN when the noise stage doesn't report, and at reset. The GUI shows dashes. | `noise_readouts_clear_when_the_stage_stops_reporting`, `telemetry_reset_clears_the_new_fields` |
| **DSP-5**: the Auto gate took 24-54 s to engage | nf-core | Part of the gate redesign (known items, below): blocks below -140 dBFS and non-finite blocks don't move the floor, and the floor acquires at 25 dB/s after `new()`, `reset()` or 0.5 s of silence. It now closes within 2 s on steady -60 dBFS noise: from a fresh gate, after the engine's 3-block warm-up, and after 2 s of silence. | `auto_closes_within_two_seconds_on_steady_noise_even_after_silence` |
| **DSP-6**: switch-off overlapped two copies offset by the latency (a comb) | nf-core | Transitions between paths at the same latency (a stage and its delay line, now fed in every mode) crossfade linearly. Switch-on from off is a dip splice: 2.5 ms out, then 2.5 ms in, never overlapping. Switch-off crossfades into the delay path and stops running the stage at once. The slot then drains, and drops the latency with a dip splice at the first block where both paths are below -50 dBFS, or after 1.5 s. On a 173 Hz sine the comb was -7.8 dB. The worst 10 ms window is now 0.00 dB for crossfades and -1.0 to -2.05 dB for splices. | Transitions on a sine and on white noise, at latencies 480 and 430, through on, off, the drain timeout, on again mid-drain, and into and out of passthrough |
| **DSP-7**: NaN or Inf from the mic reached the cable with both stages off | nf-engine | `convert.rs` maps non-finite values to 0 on the final value of `downmix_f32`, `downmix_generic` and the f32 upmix paths. That also catches a sum that overflows. `read_packet` still vectorises. | `non_finite_samples_become_zero_at_both_edges` |
| **gui/F1**, **wasapi/E**: the mic could be the output cable's own recording side | nf-engine | `open_session` resolves the devices before building the stages and compares the mic's and the output's interface friendly names. When following the default mic and they match, the start fails with `StartFailure::DefaultMicIsOutput`, which waits for another default, as a missing mic does. A mic picked by hand gets `Warning::MicSharesOutputDevice`. | `a_mic_on_the_output_device_waits_or_warns` |
| **gui/F5**: no single-instance guard | nf-app | `instance.rs`: before `setup_memory`, the window takes the `Local\noise-filter.gui` mutex until it exits (an abandoned mutex passes to the next launch). A second launch finds the first window (EnumWindows: title, then the same exe path), restores it if minimised, brings it forward and exits 0. If there's no window after 5 s, it shows a message box and exits 2. | `instance` and `mutex` tests (exe matching, abandoned mutex, a waiting claim) |
| **gui/F6**: a maximised size was saved; the default didn't fit a 1080p laptop | nf-gui | `screen.rs`: at start-up the saved size is clamped to the primary monitor's work area in DIPs, less the frame, never below the minimum. A size that fills the work area (maximised, snapped, or the clamped start-up size) isn't saved. The 520x760 default becomes 520x640 on 1080p at 150%. | 6 `screen` tests |
| **gui/F7**: an unreadable settings.json was overwritten with defaults | nf-gui | The file is renamed to `.bad` when possible. When it can't be, saving is off for the session (`Saver::disabled`) and the notice says so. Beyond the fix text: only file-not-found and path-not-found count as a first run. std reports an unreachable network path as NotFound too, which was the review's own scenario. | `a_locked_file_is_left_alone_and_nothing_is_saved`, `an_unreadable_file_is_kept_as_bad`, `only_a_missing_file_or_folder_is_a_first_run` |
| **gui/F8**: after a fault the window offered Start while the engine was armed | nf-gui | The button reads Stop while the engine is running, starting or `EngineStatus.retrying`, and the status line reads "Waiting to retry · …". | `a_retrying_engine_is_waiting_to_retry`, `a_retrying_engine_offers_stop` |
| **gui/F9**: settings were written without a flush before the rename | nf-gui | `write_synced` (create, `write_all`, `sync_all`), then the rename. The temp file is removed if either fails. | `writes_are_flushed_before_the_rename`, `a_failed_write_keeps_the_old_file` |
| **gui/F10**: no pool allocation rate; runtime faults never reached the InfoBar | nf-gui | The readout gains the RT heap and pool allocation line, the fill targets, mic stalls, coarse drops and tap overflows. A fault that stops a run raises "The filter stopped" once, cleared when the engine runs again. Warnings get their own InfoBar. | `the_readout_lines`, `the_fault_text`, `a_fault_reaches_the_infobar_once_and_clears_on_recovery`, `the_warnings_bar_follows_the_warnings` |
| **RT-1**: each audio-thread start leaked ~22 KB of the locked pool | nf-stages, nf-engine | `nf_stages::warm_up_fft_thread()` builds easyfft's per-thread state before `RtScope::enter()` on the audio thread. The integration pass added the same call to `bench` (§9.2). | nf-engine `a_warmed_thread_builds_the_noise_stage_without_leaking` (fails without the warm-up: 80 allocations, 34 frees). nf-stages `tests/rt_pool.rs`: 8 fresh threads leave the pool at its baseline, and a control thread without the warm-up keeps 22,040 bytes |
| **RT-2**: the latency test's analysis ran under MMCSS | nf-engine, nf-core | The test drops its MMCSS guard after recording, before the analysis. `Mmcss`'s fallback restores the thread's previous priority. The analysis is cheaper: a chunked strongest-first search, and a local sinc refinement instead of the whole spectrum per Newton step. 100 repetitions (9.6 M samples) now analyse in 0.58 s; one spectrum refinement alone took 76 ms, times 100. | `the_fallback_priority_is_restored_on_drop`, `entering_and_leaving_mmcss_leaves_the_priority_as_it_was`; analysis: chunked against naive, local against spectrum (within 6.8e-8 samples), the `#[ignore]` release timing test |
| **app-privilege/F2**: elevated setup granted to the elevating admin | nf-app | Without `--sid`, the elevated run finds the desktop user from the shell window's token (`account.rs`). If that user differs, it grants to them and names both accounts. If the lookup fails it grants to its own account, with a warning. | `choose_grantee` cases, read-only account lookups |
| **app-privilege/F3**: a non-elevated helper relaunched itself without limit | nf-app, nf-gui | A hidden `--elevated-helper` flag marks the helper. A helper that isn't elevated exits 4 (`Exit::NotElevated`) and doesn't relaunch. The window's button now passes the flag and explains code 4 (§9.2). | `next_step`, helper argument round trip, `NotElevated`; nf-gui `the_large_page_helper_is_marked_and_its_refusal_explained` |
| **app-privilege/F4**: one process's log rotation deleted another's history | nf-app | Rotation runs under a per-path named mutex and re-reads the size on disk. Before each line the logger takes other processes' writes into its size, and reopens the file when another process rotated it. | `a_second_process_doesnt_rotate_away_the_firsts_rotation`, `a_rotation_rechecks_the_size_on_disk`, `a_deleted_log_is_recreated` |
| **app-privilege/F5**: a panic in a CLI command with its own console vanished | nf-app | The panic hook prints the log path to stderr and shows the message box when the window or an allocated console would otherwise close unread. | `panics_get_a_box_only_when_nothing_else_would_show_them` |
| **app-privilege/F6**: elevation failed from a mapped drive | nf-mem | `run_elevated` starts the helper in the exe's folder, adds SEE_MASK_FLAG_NO_UI, and explains errors 267, 2 and 3 (mapped and subst drives aren't visible elevated). | `helper_dir` is the exe's folder; `launch_error` wording |
| **app-privilege/F7**: `bench --seconds 3600` needed 8-10 GB | nf-app | `--seconds` is capped at 300 (about 1.2 GB peak). The preferred overlap-add echo in `Signals::synthetic` isn't done. | 300 accepted, 301 and 3600 rejected |
| **wasapi/F**: most starts counted an underrun | nf-engine | The first block tops the output up to the target and isn't counted. | `the_first_block_sets_the_starting_fill_without_an_underrun` |
| **performance/F3**: every mic wake polled the loopback and keep-alive | nf-engine | `service_reference()` tops up the keep-alive and drains the loopback only on wakes where a block is ready (`Pipeline::block_ready`). The command, stall and tap checks still run on every wake. | `the_reference_is_serviced_only_on_block_wakes` (1 ms mic period: 10 reads per 100 wakes) |

**Known items done alongside:**
- **Gate redesign** (nf-core `dsp/gate.rs`, same API). It opens on block RMS at or above the threshold (Static) or the
  floor + margin (Auto), or on VAD >= 0.5 with `use_vad`. It closes only after `hold_ms` with the condition false, with
  3 dB of hysteresis. Ramps are linear in dB, so `attack_ms` and `release_ms` are the literal times between 0 dB and
  the range. `powf` runs at block ends only, and per sample the gain is interpolated from the index, vectorised. The
  Auto floor follows a power-smoothed level: it falls with tau 200 ms, rises at 3 dB/s closed or 1 dB/s open (3 dB/s
  while the model says "not speech"), and acquires at 25 dB/s. Measured on RNNoise output (speech-like voice in pink
  noise at 10 dB SNR, 120 s per case, 4 cases): pauses got -38.6, -39.4, -34.7 and -15.1 dB of extra attenuation, and
  speech changed by 0.00 dB. In Static mode the gate reaches -40 dB at hold + release (250 ms). nf-stages' gap tests
  now run at every default: Auto reaches -40 dB in all 8 gaps within 0.26-0.97 s and never reopens, and Static reaches
  the range 259-274 ms after each gap starts.
- **Output fill interpolation** (nf-engine). `ClockReading` carries a QPC stamp. The drift loop reads frames written
  plus pending, minus the device position carried forward to now (capped at 2 periods), calibrated to the padding by
  `FillOffset`. In the stepped-clock simulation the latency wander is 0.02 ms, against 8.07 ms at ±300 ppm on the
  padding alone. Underruns are still decided on the real padding.
- **`tools/hot_loop_spills.py`**. `COLD_PATHS` now leaves out the off-thread modules (`nf_stages::{synth, bench}`,
  `nf_engine::{control, errors, latency_test}`, `nf_gui`, `nf_app`), and closures written there. By default it reads the
  newest bin asm that has a `main`, in any target dir. A closure in a generic argument now shows its module. There are
  11 tests in `tools/test_hot_loop_spills.py`. `hot-loop-tools.md` was refreshed, AEC3's latency was corrected to 430
  samples (8.96 ms) wherever it was stated, and `rlsf-rtrb.md` maps its skeleton names to nf-mem's API.

**Not fixed, or only in part** (details are in the agents' reports):
- **Gate at the default 6 dB margin**: RNNoise residual bursts more than 6 dB over the floor can open it. One case in
  four did this once in 120 s, which capped its pause attenuation at -15 dB; margins of 9-12 dB gave about -39 dB in
  all four. On other speech seeds and noise levels some gaps take 1-2 s to close. The default margin is the owner's
  call.
- **The VAD at 0 dB SNR**: RNNoise reports speech in 246 of 248 pause blocks, so `use_vad` (on by default) holds the
  gate open. A VAD threshold, or the `use_vad` default, is the owner's decision.
- **wasapi/B**: a reference lost to a format change, or to an exclusive-mode app on the speakers, isn't re-attached
  automatically, because no Added or Active event follows. Re-attaching means a full restart, which interrupts the mic
  path.
- **gui/F6**: where the window opens is still Windows' choice. Reactor 0.100.0 doesn't expose the HWND, so the clamp
  uses the primary monitor.
- **gui/F10**: a fault is still invisible while the window is minimised. That would need a tray icon, a toast or
  FlashWindowEx.
- **gui/F7, F9 extras**: saving isn't turned back on mid-session, and there's no backup copy or write-through rename.
- **gui/F5**: started from a terminal, Windows may refuse SetForegroundWindow and only flash the taskbar button. The
  headless `run` isn't guarded against a running window.
- **app-privilege/F7**: only the 300 s cap; the overlap-add echo isn't done.
- **performance/F1**: sonora-fft's `fft4g` keeps 60 `fmaf` sites, reached only through sonora's noise suppressor,
  which is off. If NS is ever enabled, sonora-fft needs the same patch.
- **RT-2**: `find_sweep_arrivals` still does one FFT over the whole recording, about 200 MB at peak for 100
  repetitions. It runs in 0.58 s, so overlap-save wasn't worth it yet.
- **Pre-existing, unchanged**: once its pre-built spare is used, `RnnoiseStage::reset` builds a fresh `DenoiseState` on
  the audio thread (10 allocations, from the pool). That happens on a second reset before the next `activate()`, for
  example when the latency test leaves passthrough. `tests/rt_no_alloc.rs` pins this.

### 9.2 Integration changes

The fix agents' work built together as it was. The workspace had no build warnings and no failing test, and clippy
was clean. These are the integration pass's own changes:
1. **nf-gui `window.rs`** (app-privilege F3). The "Enable large pages" button now runs
   `setup-large-pages --sid SID --elevated-helper`, and the notice explains exit code 4 (started without elevation;
   is UAC off?). Before this, with UAC off, the button's helper could still relaunch itself, because only nf-app's
   own relaunch passed the flag. New test: `the_large_page_helper_is_marked_and_its_refusal_explained`.
2. **nf-engine `control.rs`, `api.rs`**: `EngineStatus.message` is now only the error that stopped the engine, or the
   last start failure, and `None` while running. Standing warnings are listed once, in `EngineStatus.warnings`. Before
   this the engine also joined them into `message`, so the window showed them twice (the status line and the new
   warnings InfoBar), and so did `run` (`note:` and `warning:` lines). The doc comment changed; the signature didn't.
   The tests were updated: `status_lists_the_warnings_once` (renamed from `status_joins_the_warnings`) and
   `a_lost_reference_is_a_warning`.
3. **nf-app `commands/bench.rs`** (RT-1): `bench` calls `nf_stages::warm_up_fft_thread()` before its `RtScope`, as the
   audio thread does, so a bench run no longer keeps ~22 KB of the pool. That was harmless, since the process exits
   right after, but it now matches the audio thread.

No public signature changed in the integration pass. The fix agents' cross-crate changes are in the table above. The
largest are the root `Cargo.toml` `[patch.crates-io]` and `vendor/`, nf-gui's new `windows` dependency, nf-app's
second `[[bin]]`, and nf-stages' test-only dev-dependencies (`nf-mem`, `sonora-common-audio`).

### 9.3 Tests: 420 pass, 9 ignored

`cargo test --workspace --no-fail-fast`, debug:

| Crate | Pass | Ignored | Was (§2) |
|---|---|---|---|
| nf-app | 62 (`noise-filter`) + 6 (`noise-filter-cli`) | 0 | 44 |
| nf-core | 93 unit + 3 `rt_no_alloc` | 2 unit (`tuning_sweep`, the release analysis timing) + 1 `timing` | 72 + 3 |
| nf-engine | 96 | 4 | 70 |
| nf-gui | 67 | 0 | 48 |
| nf-mem | 42 unit + 5 `global_alloc` | 1 (`large_pages_region`, owner only) | 40 + 5 |
| nf-stages | 19 unit + 8 aec3 + 4 chain + 5 rnnoise + 5 `rt_no_alloc` + 3 `rt_pool` + 2 `sonora_patch` | 1 (`timing`) | 41 |

These were also run with the ignored tests:
- nf-engine `-- --ignored --skip lists_devices`: `real_drift_loop_and_resampler`,
  `the_audio_path_never_allocates_with_the_real_drift_loop` and
  `the_interpolated_fill_holds_the_latency_still_at_slow_drift` pass. `lists_devices` was not run, because it
  enumerates devices.
- nf-core release, `a_hundred_repetitions_of_the_real_probe_finish_quickly`: 100 repetitions, 9,601,234 samples, in
  585 ms. Local and spectrum refinement agree to 1.9e-9 samples.
- `python tools/test_hot_loop_spills.py`: 11 pass. `python todo_check.py` is clean.

No test opens an audio device or a stream, or allocates large pages.

### 9.4 Clippy and the release exes

`cargo clippy --workspace --all-targets -- -D warnings` is clean, without `--no-deps`. Some agents saw nf-core gate
lints and nf-engine dead-code warnings while they worked; both were in-progress states, and neither is in the final
tree.

`cargo build --release -p nf-app` (fat LTO, `panic = "abort"`, line tables, x86-64-v2), PE headers checked:

| File | Bytes | Subsystem |
|---|---|---|
| `noise-filter.exe` | 5,250,560 (5.01 MiB; was 5,193,728) | 2 (GUI) |
| `noise-filter-cli.exe` | 143,872 (141 KiB) | 3 (console) |
| `noise_filter.pdb` / `noise_filter_cli.pdb` | 22,056,960 / 1,077,248 | |

### 9.5 Timing (release, this VM)

`cargo test --release -p nf-stages --test timing -- --ignored --nocapture`: per block over 30 s, after a 1 s warm-up,
in µs. The median of 3 runs is shown for each column, against §5's single run. The p99 and max columns are noisy on
this VM.

| Row | p50 | p99 | max | mean | p50 / p99 in §5 |
|---|---|---|---|---|---|
| RNNoise (noisy speech) | 39.8 | 56.9 | 103.2 | 40.9 | 39.7 / 54.0 |
| AEC3 (echo + near end + noise) | **55.0** | 92.4 | 141.8 | 56.4 | 90.8 / 114.5 |
| AEC3, silent reference (dithered) | **50.5** | 75.2 | 136.6 | 46.1 | 86.0 / 97.3 |
| sonora, zero reference, no dither | 656.1 | 797.0 | 1010.4 | 576.3 | 686.7 / 740.8 |
| chain: meters + AEC3 + RNNoise | **96.5** | 153.3 | 352.6 | 98.5 | 130.3 / 155.3 |
| chain: meters + RNNoise | 40.1 | 60.7 | 98.0 | 41.0 | 39.9 / 54.6 |

The vendored `madd` patch (performance/F1) takes 36 µs off AEC3 and 34 µs off the full chain at p50. The chain now
uses 1.0% of a 10 ms block at p50. The dithered silent-reference row still shows no denormal stall.

Glue kernels, `cargo test --release -p nf-core --test timing -- --ignored --nocapture`, ns per block (§5 in brackets):
Meter 60 (60), DelayLine 29 (29), crossfade 81 (83), FloorMix steady 57 (54), FloorMix ramping 133 (135), Gate 83
(81, now the redesigned gate), NoisePost 233 (227), Slip 481→480 290 (306), Slip 480→481 292 (292), Dither 151 (150),
DriftController 3 (4).

### 9.6 Hot loops

```
cargo rustc --release -p nf-app --bin noise-filter -- --emit asm
python -I tools/hot_loop_spills.py target/fix-integrate/release/build/nf-app/eb7eb956b7a6fa52/out/noise_filter.s
```
This is the bin's post-LTO asm (204.5 MB). It is the first table taken with the new `COLD_PATHS` on a build with all
the fixes, so every row is audio-thread code.

| function | round trip gpr | round trip simd | reload gpr | reload simd | panic | scalar | indexed | const | long |
|---|---|---|---|---|---|---|---|---|---|
| `nf_engine::audio::run` | 1 | 0 | 0 | 2 | 0 | 1 | 0 | 0 | 0 |
| `median_of_medians::<f32, …>` (RollingStats) | 0 | 0 | 2 | 0 | 2 | 0 | 5 | 0 | 2 |
| `RollingStats<100>::percentile` | 0 | 0 | 2 | 0 | 0 | 0 | 3 | 0 | 0 |
| `CaptureStream::read_packet` | 0 | 0 | 1 | 0 | 0 | 12 | 16 | 1 | 0 |
| `NoisePost::process` | 0 | 0 | 1 | 0 | 0 | 1 | 9 | 2 | 0 |
| `SlipResampler::resample` | 0 | 0 | 0 | 0 | 0 | 1 | 2 | 1 | 0 |
| `RenderStream::write` | 0 | 0 | 0 | 0 | 0 | 0 | 13 | 0 | 0 |
| `chain::run_slot` | 0 | 0 | 0 | 0 | 0 | 0 | 4 | 0 | 0 |
| `Aec3Stage::process` | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 | 0 |
| `RnnoiseStage::process` | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 | 0 |
| `quicksort::partition::<f32, …>` | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 | 0 |
| `Meter::measure` | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 | 0 |
| `meter::mean_square` | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 |
| `Dither::apply` | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 |
| **total (14 functions)** | 1 | 0 | 6 | 2 | 2 | 15 | 62 | 4 | 2 |

Two more functions have only clean loops. 27 hot-shaped loops make a call: 8 dispatch, 3 rt, 9 libm and 7 read.

**What the flags are.** None is a per-sample problem, so nothing was changed:
- **`audio::run`'s gpr round trip** is `service_tap`: rtrb's cached head and tail, kept on the stack across the wake
  loop. It runs once per wake, not per sample.
- **The `read_packet`, `NoisePost::process` and `audio::run` reloads and `const` flags** are block-level control code,
  where LLVM's block layout makes a back-edge. In `read_packet` it is the packet's exit path next to the 8-channel
  downmix's preheader (the 1/8 scale). In `NoisePost::process` it is the gate's and floor mix's per-block decisions.
  The per-sample gate, floor-mix, crossfade and dip-splice loops are vectorised with nothing on the stack.
- **`read_packet`'s 12 scalar loops** are the known missed vectorisation of the interleaved downmix (§6). DSP-7's
  non-finite mask didn't add any: the count is the same as the review's pre-fix build.
- **`RollingStats` and the sort** run once a second.
- **libm, all per block or set-up:**
  - `NoisePost::process`: `powf` and `log10f`, 4 loops. This is the gate's block-end `powf`, DSP-2's output
    measurement and FloorMix's cached `powf`.
  - `audio::run`: `log10f`, 4 loops. This is `publish_block` turning the meter ballistics into dB, 4 calls a block.
  - The audio thread's entry: one `sinf` loop, which is `warm_up_fft_thread`'s test block, before the RT scope.
- **rt:** `read_packet`'s `log_impl`, which runs only above the Info level, which is capped. The other two are
  `RawVec<Warning>::grow_one` in `open_session`, at start-up.
- **`fmaf`:** 60 call sites, down from 162, all in sonora-fft's `fft4g`, which nothing on our path runs
  (performance/F1).

Against the review's pre-fix asm (13 functions; totals 0/0/4/3/2/16/54/2/2 under the new `COLD_PATHS`), the changes
are:
- `meter::mean_square` is now out of line (DSP-2 calls it a second time).
- `RenderStream::write` has more indexed loops (DSP-7's f32 upmix paths).
- `run_slot` has the crossfade and dip-splice loops (DSP-6).
- The one gpr round trip is in `service_tap`.

There are still no bounds checks, calls or register spills in any per-sample loop on the audio thread.

### 9.7 Docs left stale

These fall outside the integration scope, and each needs a short edit:
- `doc/design/engine.md`:
  - Line 63 says the output drift loop reads `render_padding + pending`; it now reads the interpolated fill.
  - Lines 66-68 say "pad with zeros up to target" and "Above target + 3 blocks, drop whole blocks". The code now counts
    the backlog, uses 2 blocks with rounding down to within half a block, and lets the first block set the starting
    fill.
  - Line 75 says "gate gain"; it is now the stage's total reduction, and the meters have ballistics.
- `doc/design/gui.md`:
  - Line 33 says "Gate: -x dB"; it is now "Reduction: x dB", the stage's total.
  - Line 37's readout list lacks the fill targets, stalls, coarse drops, tap overflows, the allocations line, the
    warnings InfoBar and the fault notice.
- `doc/design/architecture.md`:
  - Lines 34-36, "Toggles", should describe the priming, the dip splice and the drain (DSP-1, DSP-6).
  - Line 32 says RNNoise is about 36 µs and AEC3 about 45 µs; this VM measures 39.8 and 55.0.
- `doc/design/app.md` needs:
  - the `noise-filter-cli` / `noise-filter.com` launcher;
  - helper exit code 4 and the hidden `--elevated-helper` flag;
  - the single-window guard;
  - the 300 s `bench` cap.
- `doc/dev/api/hot-loop-tools.md`:
  - Pitfall 3 still gives 162 `fmaf` sites, all sonora's; it is now 60, all in `fft4g`, NS only.
  - Its budget line gives AEC3 as 90.8 µs.
  - Line 48 and §3 R3 say the gate uses a table of powers; it now uses an index-derived linear ramp (R2). The tool's
    docstring says the same about the gate.
- `doc/dev/api/sonora-nnnoiseless.md` could mention the vendored patch.

**§8's known gaps, now closed:**
- The interactive-shell wait: the `noise-filter-cli` launcher.
- The tool's selection: `COLD_PATHS`.
- The stale hot-loop-tools pitfalls 1 and 5.

## 10. Verification follow-ups (2026-10-26)

Seven verifier lenses checked §9's fixes: 52 of 63 items fixed, 11 partly, and about 30 new problems,
mostly small. All of them were then worked through. The §9.7 docs are updated.

**Fixed**
- **Output timing (clocks-latency F1, the fill interpolation):**
  - after the start, a deficit is judged on the calibrated fill with this block and the backlog
    counted, and zeros top up only to the target;
  - the fill calibration (`FillOffset`) is never seeded afresh: low and stale readings are left out,
    the clock is carried forward at most a period, and a dry device or a clock jump re-anchors by
    the shift in the median of 32 readings;
  - the reference's coarse correction leaves out the mic backlog (F2).
- **A stepped-device event simulation** in the tests (`crates/nf-engine/src/pipeline/tests/events.rs`):
  late wakes, cable stalls, lost mic packets, wake jitter, and 441- and 1024-frame periods. The
  ignored `event_sweep` covers 528 cases: no fault before any event, zeros only where the device
  really ran dry.
- **The engine's control side:**
  - one long-lived audio thread (`audio::Worker`), so a retry no longer leaks easyfft's 22 KB per
    thread;
  - a run stuck in a driver keeps its thread, the next start waits for it (`DriverBusy`), and a
    zombie run checks the stop flag before it starts streams;
  - the backoff starts over only after a run has stayed up 30 s;
  - the log gets a failure streak at attempts 1, 2, 4, 8...;
  - the wait is recorded before Error is published;
  - `AUDCLNT_E_ENDPOINT_CREATE_FAILED` is transient.
- **Devices and warnings:**
  - a mic sharing the output's device is refused or warned about only for known virtual cables;
  - new warnings when the output is the default playback device (speakers, or a cable that every
    app's sound would go into);
  - a lost echo reference is re-attached on a property change, or by a loopback check on a timer
    (15 s doubling to 5 min).
- **Real-time:**
  - `prepare_rt_thread()` fixes the warm-up-then-scope order in one place;
  - `consume_pending` and `RollingStats` are bounded, and the post-LTO asm shows no panic calls left
    in the audio loop's `push_output`;
  - comments fixed in `stream.rs` and `convert.rs`.
- **DSP:**
  - the Auto gate's acquisition lasts at least 100 ms whatever the hold;
  - "hold open on speech (VAD)" defaults to off (RNNoise's VAD fires on 87-100% of noise blocks);
  - a drain ends when passthrough comes on, so a latency test can't change delay mid-run;
  - the chain doc states AEC3's low-tone crossfade dip;
  - the meters-off test no longer resets by hand.
- **GUI:**
  - each side of the window is remembered unless that side fills the screen;
  - a missing settings folder is a first run only if AppData exists (an unmapped drive is
    unreadable, so the session doesn't save).
- **App:**
  - the helper flag, its exit code and its arguments live in `nf_mem::privilege`, with a
    compile-time check against `Exit::NotElevated`;
  - `--help` lists exit code 4;
  - log rotation is detected by comparing the open file's size with the path's;
  - the desktop user for the large-page grant comes from the session (`WTSQuerySessionInformationW`),
    with Explorer's token as the fallback.
- **Workspace:**
  - `exclude = ["vendor"]`, so the vendored crates' own tests run in place (sonora-common-audio:
    51 + 2 pass);
  - TODO #8-#11, #13 and #18 are brought up to date.

**Tests:** `cargo test --workspace`: 436 passed, 0 failed, 10 ignored. `cargo clippy --workspace
--all-targets -- -D warnings` is clean.

**Left open, on purpose:**
- the Auto gate's 6 dB margin and its tails after talk spurts (TODO #11: tune on real recordings);
- a second RNNoise reset before `activate` takes 10 allocations from the pool (pinned by
  `rt_no_alloc.rs`);
- the window's opening position and a fault while minimised (no HWND in Reactor 0.100.0; the tray,
  #16);
- whether to ship `noise-filter.com` (TODO #18);
- `find_sweep_arrivals`' one whole-recording FFT (about 200 MB at 100 repetitions).

**Smoke test on the dev VM (session disconnected, so no audio endpoints at all):**
- `devices` lists none;
- `run --output "CABLE Input"` exits 2 with a clear message through the launcher;
- the window opens, idles at 0.3% of a core with an 81 MB working set (32 MiB large-page pool), and
  closes cleanly.
