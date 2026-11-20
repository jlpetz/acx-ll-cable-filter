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

## 11. The move to x86-64-v3 (2026-10-28)

The owner moved the CPU baseline from x86-64-v2 to x86-64-v3, as in TMR. Built and tested on the dev box (rustc
1.101.0-nightly 1a2c4797e, `CARGO_TARGET_DIR=target/v3`; its Xeon 6975P-C has every v3 feature). **Nobody has run
either exe**, on this CPU or on one without v3: the refusal has only been checked through tests and the disassembly.

**What changed**
- **`.cargo/config.toml`:** `target-cpu=x86-64-v3`, with why, which CPUs it leaves out and the sources. Checked
  against primary sources before writing it:
  - Intel ARK: the N100 (Alder Lake-N) and i7-4770 (Haswell) list AVX2; the J4005 (Gemini Lake) and N5105 (Jasper
    Lake) only SSE4.2; the Pentium Gold G6400 and Celeron G5900 (Comet Lake, Q2'20) only SSE4.1/4.2; the Pentium
    Gold 7505 (Tiger Lake, Q4'20) and G7400 (Alder Lake, Q1'22) list AVX2.
  - Intel's optimisation manual (248966-051, table 1-1) puts the Intel N-series on Gracemont.
  - AMD's BKDGs: family 15h models 30h-3Fh (49125, Steamroller) read AVX2 and BMI2 as 0; models 60h-6Fh (50742,
    Excavator) read AVX2, BMI1, BMI2, FMA, F16C, MOVBE and ABM (LZCNT) as 1.
  - Two corrections to the brief: "pre-2020 Pentium Gold or Celeron" became "up to Comet Lake (2020)", since the
    G6400 and G5900 launched in 2020 without AVX; and not all of AMD before Zen is left out, since Excavator has v3.
- **The CPU gate**, `crates/nf-app/src/cpu_check.rs`, ported from TMR's without CLFLUSHOPT: CPUID and XCR0 read
  directly, 16 features, the refusal built in a stack buffer and written with `WriteFile` (and, from
  noise-filter.exe, `MessageBoxA`). Both `main`s call `gate` first, and the rest is `#[inline(never)]` (`app_main`,
  `launch_and_exit`). `crates/nf-app/build.rs` (new) exports `NF_TARGET_FEATURES`. Exit code 5 is
  `Exit::UnsupportedCpu`, with a compile-time check against `cpu_check::EXIT_CODE`; `--help` and app.md list it.
- **The vendoring is gone:** `vendor/`, `[patch.crates-io]`, `exclude = ["vendor"]`, nf-stages' `sonora-common-audio`
  dev-dependency and `tests/sonora_patch.rs`. `Cargo.lock` was regenerated offline (`cargo metadata --offline`):
  sonora and sonora-common-audio 0.2.0 come from crates.io again, and their checksums match the registry's
  `.crate` files. In the test's place, nf-stages' `tests/aec3.rs` has `the_build_has_fma_for_sonoras_mul_add`, a
  `const` assert, so a build without FMA doesn't compile it (checked at `-C target-cpu=x86-64-v2`). Neither exe
  imports `fmaf` now; the v2 timing exe did.
- **Docs:** `vendor/README.md` is deleted. Updated: `doc/dev/api/sonora-nnnoiseless.md` (1.9, and nnnoiseless'
  SIMD), `doc/dev/api/hot-loop-tools.md` (pitfalls 3 and 9, §3's budget and baseline, R4, R5, R6),
  `doc/design/architecture.md`, `doc/design/app.md`, TODO #10 and `doc/todo/10-aec.md`, `tools/hot_loop_spills.py`'s
  help, and four nf-core comments on index conversion (no packed `usize` to `f32` below AVX-512; at v3 a probe still
  gives one scalar `vcvtsi2ss` a sample for `usize`, and one `vcvtdq2ps` per 8 for `i32`).

**The way to the refusal, in the release exes.** At v3 every SSE instruction is VEX-encoded, so a CPU without AVX
faults on any XMM use, not only on AVX2's. Each function the CPU runs before the gate has refused was disassembled
whole, over its range in the PDB: `cdb -z <exe> -y .` for the symbols and sizes (`x /v`), then `llvm-objdump -d`
from `--start-address` to `--stop-address`, scanned for any `v…` mnemonic and for ANDN, BEXTR, BLS*, BZHI, MULX,
PDEP, PEXT, RORX, SARX, SHLX, SHRX, TZCNT, LZCNT and MOVBE:

| Exe | Function | Instructions | Forbidden | Vector registers |
|---|---|---|---|---|
| noise-filter.exe | `main` (rustc's, with std's runtime init inlined) | 61 | 0 | 0 |
| | `__rust_begin_short_backtrace<fn() -> ExitCode>` | 5 | 0 | 0 |
| | `noise_filter::main` (the gate inlined; tail-jumps to `app_main`) | 198 | 0 | 0 |
| | `cpu_check::refuse` (`cpu_box` inlined) | 171 | 0 | 0 |
| noise-filter-cli.exe | `main` | 57 | 0 | 0 |
| | `__rust_begin_short_backtrace<fn()>` | 5 | 0 | 0 |
| | `noise_filter_cli::main` | 188 | 0 | 0 |
| | `cpu_check::refuse` | 164 | 0 | 0 |

- What they call is std's own code, which keeps std's baseline (`SetThreadDescription::load`, `Once::call` for the
  runtime's clean-up, `process::exit`, `rt::cleanup`, `sys::exit::exit`: also scanned, also clean), or Windows.
- Before `main` the C runtime runs only its own initialisers: no Rust function sits in `.CRT$XC*` or `.CRT$XI*`,
  and the TLS callback list is empty. std's runtime init doesn't allocate.
- nf-mem's allocator: the system-heap paths of `__rust_alloc`, `__rust_realloc` and `__rust_dealloc` are clean, but
  the pool paths of alloc, realloc and dealloc (`Router::pool_alloc`, `Router::realloc`, rlsf's `allocate` and
  `deallocate`) have TZCNT, LZCNT, SHLX and SHRX. They are reached only inside an RtScope or for pool memory, so never
  before the gate, which allocates nothing anyway (corrected in §11.1; this said dealloc only).
- The launcher imports no user32.
- This holds for this source and this compiler. `tools/check_cpu_gate_asm.py` (§11.1) now checks it in the asm: run
  it after changing `cpu_check.rs`, either `main` or the toolchain, and before a release. A debug build isn't covered.

**Timing (release, this VM).** `cargo test --release -p nf-stages --test timing -- --ignored --nocapture`, run as
the test exe 3 times at each baseline, alternating v2 and v3. The v2 build is the last commit (0a777d6, with the
vendored patch), built from a clean export into `target/v2-baseline`, so both columns come from the same session.
Medians of the 3 runs, µs per block:

| Row | p50 v2 | p50 v3 | change | p99 v2 | p99 v3 | mean v2 | mean v3 |
|---|---|---|---|---|---|---|---|
| RNNoise (noisy speech) | 39.6 | **36.1** | -9% | 55.2 | 50.7 | 40.5 | 36.6 |
| AEC3 (echo + near end + noise) | 54.4 | **50.4** | -7% | 76.7 | 72.5 | 55.7 | 50.5 |
| AEC3, silent reference (dithered) | 50.8 | **45.7** | -10% | 71.9 | 60.6 | 47.3 | 42.0 |
| sonora, zero reference, no dither | 655.9 | 669.5 | +2% | 759.8 | 729.1 | 576.3 | 575.5 |
| chain: meters + AEC3 + RNNoise | 97.0 | **84.6** | -13% | 126.5 | 105.8 | 97.9 | 85.2 |
| chain: meters + RNNoise | 40.2 | **34.8** | -13% | 59.7 | 45.4 | 41.2 | 35.3 |

- The v2 column matches §9.5 (39.8, 55.0 and 96.5 at p50), so the session was quiet.
- AEC3 at v3 with crates.io's sonora is about 4 µs a block faster than at v2 with the patch: with FMA, `mul_add` is
  one `vfmadd`. So there was no slowdown to investigate.
- RNNoise gains too, although rustfft picks its kernels at run time: nnnoiseless' own loops are now built for AVX2.
- The undithered zero-reference row, sonora's denormal stall, which the engine's dither avoids, is 2% slower.
- The chain now takes 0.85% of a 10 ms block at p50.

Glue kernels, `cargo test --release -p nf-core --test timing -- --ignored --nocapture`, alternated the same way,
median ns per block, v2 → v3: Meter 60 → 47, DelayLine 26 → 29, crossfade 80 → 46, FloorMix steady 56 → 45,
FloorMix ramping 135 → 79, Gate 82 → 67, NoisePost 231 → 226, Slip 481→480 291 → 191, Slip 480→481 292 → 181,
Dither 151 → 107, DriftController 4 → 3.

**Release exes** (`cargo build --release -p nf-app`): `noise-filter.exe` is 5,330,432 bytes (5,250,560 in §9.4) and
`noise-filter-cli.exe` 145,920 (143,872). Subsystems 2 and 3, as before.

**Hot loops** (`tools/hot_loop_spills.py` on each build's post-LTO asm, the same source at v2 and v3). The totals
move from 1/1/7/0/2/15/67/4/2 to 1/1/10/1/2/14/73/6/2 (round trip gpr and simd, reload gpr and simd, panic,
scalar, indexed, const, long). The new gpr reloads are block-level code: `audio_loop`'s packet and ring
bookkeeping (3, from 1) and `RollingStats::percentile`, once a second. The one new simd reload is per sample: the
4-channel f32 downmix in `CaptureStream::read_packet`, now 8 frames a pass with `vpermps`, takes one mask operand
from the stack each pass (`vpminud 176(%rsp)`). That is one L1 load per 8 frames, on 4-channel mics only, so it was
left alone. `fmaf` no longer shows.

**Tests:** `cargo test --workspace --no-fail-fast`, debug: **449 passed, 0 failed, 10 ignored** (§10: 436 and 10).

| Crate | Pass | Ignored | Was (§10) |
|---|---|---|---|
| nf-app | 69 (`noise-filter`) + 13 (`noise-filter-cli`) | 0 | 62 + 6 |
| nf-core | 95 unit + 3 `rt_no_alloc` | 2 unit + 1 `timing` | same |
| nf-engine | 110 | 5 | same |
| nf-gui | 67 | 0 | same |
| nf-mem | 42 unit + 5 `global_alloc` | 1 | same |
| nf-stages | 19 unit + 9 aec3 + 4 chain + 5 rnnoise + 5 `rt_no_alloc` + 3 `rt_pool` | 1 (`timing`) | 8 aec3, 2 `sonora_patch` |

`cpu_check`'s 7 tests run in both nf-app binaries, since each includes the module: that is the +14. The new aec3
test is +1, and `sonora_patch`'s 2 went. `python tools/test_hot_loop_spills.py`: 11 pass. `python todo_check.py`
is clean. Also checked: with `RUSTFLAGS="-C target-cpu=x86-64-v4"`, build.rs reruns and `NF_TARGET_FEATURES` lists
the AVX-512 features, which `every_feature_the_build_enables_is_checked` would then reject.

**Clippy:** `cargo clippy --workspace --all-targets -- -D warnings` is clean. It first flagged the FMA test's
`assert!(cfg!(..))` as constant (`assertions_on_constants`); that is now the `const` assert above.

**Left open**
- Nobody has seen the refusal: no CPU here lacks a feature, and the tests fake CPUs through their CPUID words only.
  An old Pentium or Celeron, or a VM whose hypervisor masks AVX, would show it.
- On such a CPU noise-filter.exe shows the box at every launch, a CLI command's too: the gate runs before the
  arguments are parsed, since parsing them takes v3 code.
- The v2 comparison builds (`target/v2-baseline`, from `git archive 0a777d6`) and `target/v3-asm` were deleted
  afterwards; the commands above rebuild them. (`target/v3-asm` was rebuilt for §11.1 and is kept.)

### 11.1 Verification follow-ups (2026-10-28)

Verifiers found ten problems in §11's changes, one medium and nine low. All ten were real, and all are fixed. None
changes what the gate does on a CPU that has every feature.

**Fixed**
- **Nothing checked the way to the refusal again (findings 1 and 3, the medium one).** §11's check was done by hand,
  and the toolchain floats on `nightly` with no `rust-toolchain` file. A `rustup update`, or a small source change
  (a zeroed array, a `format!`, `missing_in` lowered with SHLX or ANDN), could put a VEX or BMI instruction before
  the gate. The only sign would be a silent crash on exactly the CPUs the gate is for.
  - New: `tools/check_cpu_gate_asm.py`, with 12 tests on synthetic asm in `tools/test_check_cpu_gate_asm.py`. It
    reads both exes' post-LTO asm (`--emit asm`, as for `hot_loop_spills.py`) and walks from `main`, and from anything
    in a `.CRT$X*` section, to every symbol referenced: calls, tail jumps, addresses taken, and what data objects
    hold (vtables, std's `SetThreadDescription::PTR`, the unwind tables' catch funclets). It stops at `app_main` /
    `launch_and_exit` and at panics.
  - It fails on any VEX or EVEX instruction, on BMI1/2, LZCNT, MOVBE, XSAVE's instructions and AVX-512's masks, and
    on the rest of x86-64-v2 above the target's own baseline (POPCNT, CRC32, the legacy SSSE3 and SSE4
    instructions). It also fails when the walk no longer reaches `cpu_check::refuse`, or the code after the gate.
    Both exes take about 7 s.
  - The target's own baseline passes: `x86_64-pc-windows-msvc` is x86-64 plus CMPXCHG16B, LAHF/SAHF and SSE3
    (`rustc -Z unstable-options --print target-spec-json`). std is built for it, and std's own functions are where
    the legacy SSE on the path is.
  - `ALLOWED` lets the allocator's v3 code through, with why for each: the `__rust_*` entry points, the router's
    `pool_*` functions, `Router`'s `GlobalAlloc` methods and rlsf. Their flagged instructions are on pool branches
    only (finding 2). The walk reaches them through std's clean-up, which frees stdout's buffer if stdout was used,
    and through std's error formatting. The allowance is per function, so it can't tell those branches apart.
  - It catches a real regression: with the stop at `launch_and_exit` taken out, the walk goes into the launcher's
    code after the gate and reports 23 failures (VEX in `launch_and_exit`'s funclets, among others).
  - TODO #18 and `doc/todo/18-release.md` now make it a release step, and a step after every toolchain update.
  - Not done, left to the owner: pinning the nightly in `rust-toolchain.toml`, proposed (ours) in 18-release.md.
    Other sessions build in this tree, and a dated toolchain would make rustup fetch it. The verifier's structural
    option, running the gate from a `.CRT$XCU` initialiser before rustc's `main`, wasn't taken either: the gate and
    its message would still be v3 code, so the check is needed anyway.
- **The allocator's v3 code was under-described (finding 2).** It isn't only `__rust_dealloc`: the pool paths of
  alloc, realloc and dealloc all have TZCNT, LZCNT, SHLX or SHRX. The walk finds `Router::realloc` (13 flagged),
  `Router::pool_alloc` (10), `__rust_dealloc` (5), and rlsf's `allocate` (10), `deallocate` (5) and
  `unlink_free_block` (1). All of them sit behind `in_scope()` (false until an RtScope) or `owns(p)` (false until
  `install_pool`, in `app_main`'s setup_memory). The cpu_check.rs module doc and the bullet in §11 now say so.
- **The launcher's refusal vanished in a console of its own (finding 4).** A double-click on the `.com`, or Win+R,
  printed the refusal into a new console that closed at once. The launcher's `show` is now `refusal::pause`
  (launcher.rs). It makes the same tests as `launch`'s pause (one process on the console, a console window, a
  console as standard input), then writes "Press Enter to close this window." to standard error and waits in
  `ReadFile`. Its kernel32 calls are declared raw and its buffers are `MaybeUninit`, as in the gate.
  `cpu_check::write_stderr` is `pub(crate)` for it. `console_queries_run_anywhere` calls it, and under cargo test it
  returns at once.
- **Lakefield (finding 5).** Intel ARK lists only SSE4.1 and SSE4.2 for the Core i5-L16G7 and i3-L13G4 (Lakefield,
  Q2'20). The refusal now reads "Intel Core and Xeon from Haswell (2013) on, but for the Core i3/i5 L-series
  (Lakefield), and AMD from Zen (2017) on, with its Excavator APUs (2015-2016)". config.toml lists Lakefield among
  what is left out. The longest refusal is now 483 bytes, of `ROOM`'s 1024.
- **Stoney Ridge and the Atom list (finding 6).**
  - AMD's BKDG 55072 (family 15h models 70h-7Fh, Stoney Ridge, Rev 3.09) reads AVX, AVX2, BMI1, BMI2, FMA, F16C,
    MOVBE, ABM (LZCNT), XSAVE, POPCNT, SSE4.1/4.2, SSSE3, SSE3, CMPXCHG16B and LAHF/SAHF as 1. config.toml's
    Excavator exception is now models 60h-7Fh, 2015-2016.
  - The Atom list is now examples: chips on Atom cores before Alder Lake-N, branded Atom, Pentium or Celeron
    (Braswell, Apollo Lake, Gemini Lake, Jasper Lake, Elkhart Lake, Cherry Trail's Atom x5-Z8350).
  - ARK has no instruction-set row for the N4200 (Apollo Lake) or the Atom x6425E (Elkhart Lake). That those cores
    lack AVX comes from LLVM's CPU tables instead: `rustc --print cfg -C target-cpu=...` gives no AVX for
    silvermont, goldmont, goldmont-plus or tremont. The same tables back two facts §11 couldn't take from AMD:
    znver1 has every v3 feature, and btver2 (Jaguar) has AVX but no AVX2, BMI2 or FMA. They are a secondary source.
- **Stale SSE-era statements (finding 7).**
  - meter.rs: `LANES` is one `ymm` per reduction at v3.
  - hot-loop-tools.md: pitfall 8's "480-sample SSE loop" is a vector loop now. In R9, at v3 the slip resampler's
    runs get an 8-wide `ymm` body plus a scalar remainder of up to 7 samples, with no 4-wide step between them
    (`--detail SlipResampler::resample --all`). AVX2 has masked moves, but LLVM doesn't use them for this tail.
  - One more of the same: rt.rs's `DITHER_LANES` doc said "two SSE registers ... 150 ns"; it is one `ymm` and
    107 ns now.
  - hot-loop-tools.md's line references into `.cargo/config.toml` follow its new length (12 lines).
- **The release notes (finding 8).** `doc/todo/18-release.md` has a CPU line under Packaging (the owner's decision,
  what runs and what is refused, exit code 5), a step 0 for requirements in the user guide, and the asm check as a
  release step. Proposed (ours): the installer checks the CPU too. TODO #18's first line names the CPU and the
  check; the entry is 1.2 KB.
- **Bare sonora PR numbers (finding 9).** TODO #10 and 10-aec.md write `sonora #37`, `sonora #38` and so on, here
  and in the rest of 10-aec.md's sonora sections; a bare `#N` is a TODO item.
- **crt-static (finding 10).** `every_feature_the_build_enables_is_checked` leaves out features that aren't
  instructions (`NOT_ISA`: `crt-static`). Checked with `RUSTFLAGS="-C target-cpu=x86-64-v3 -C
  target-feature=+crt-static"` in a scratch target dir, deleted afterwards: `NF_TARGET_FEATURES` then holds `crt-static`, and the test passes
  (it failed on that before). 18-release.md notes
  that both exes import VCRUNTIME140.dll today (`llvm-readobj --coff-imports`). TMR's copy has the same gap; TMR
  isn't changed from here.

**The way to the refusal, rechecked** with the new script, on `target/v3-asm` rebuilt after the last source edit
(`CARGO_TARGET_DIR=target/v3-asm cargo rustc --release -p nf-app --bin noise-filter -- --emit asm`, the same for
`noise-filter-cli`, then `python tools/check_cpu_gate_asm.py --list`): both PASS.

| Exe | Reached | Instructions | Allowed (pool code) | Key functions, instructions |
|---|---|---|---|---|
| noise-filter.exe | 77 functions, 138 data objects | 5,261 | 6 functions | rustc's `main` 60, `noise_filter::main` 195, `refuse` 159, `__rust_begin_short_backtrace` 5 |
| noise-filter-cli.exe | 69 functions, 127 data objects | 3,837 | none | `main` 54, `noise_filter_cli::main` 185, `refuse` 196 (`refusal::pause` inlined), `__rust_begin_short_backtrace` 5 |

- The asm counts are a few lower than §11's disassembly (61, 198, 171...): the asm has no alignment padding.
- What the walk reaches outside the asm: Windows imports (the launcher's now include `GetConsoleProcessList`,
  `GetConsoleWindow`, `GetConsoleMode` and `ReadFile`) and the C runtime's `memcpy`, `memmove`, `memset` and
  `__chkstk`, which aren't checked.
- Release exes: `noise-filter.exe` 5,330,432 bytes as before, `noise-filter-cli.exe` 146,944 (145,920 in §11).

**Tests:** `CARGO_TARGET_DIR=target/v3 cargo test --workspace --no-fail-fast`: **449 passed, 0 failed, 10
ignored**, as in §11. No test was added on the Rust side: the launcher's `console_queries_run_anywhere` now also
calls the refusal's pause. `python tools/test_check_cpu_gate_asm.py`: 12 pass. `python tools/test_hot_loop_spills.py`:
11 pass. `python todo_check.py` is clean.

**Clippy:** `CARGO_TARGET_DIR=target/v3 cargo clippy --workspace --all-targets -- -D warnings` is clean.

**Left open**
- Nobody has seen the refusal or the launcher's wait for Enter on screen; both run only on a CPU without v3.
- The script follows every reference, so it over-approximates; its allowance is per function. A new allowance
  needs a reason that the code can't run before the gate.
- Pinning the toolchain is the owner's call (above).

## 12. sonora 0.3.0 (2026-11-20)

The pin moved from `sonora = "=0.2.0"` to `"=0.3.0"` (released 2026-11-07). `cargo update -p sonora` moved all seven
`sonora-*` crates to 0.3.0 and added no other package; our code compiled unchanged. Built and measured in a worktree from
main at b41f0c3, its own `target/`, rustc 1.102.0-nightly 5f4effd57, on the dev box: everything below ran once on 0.2.0,
then again on 0.3.0. Another session ran tests on the box meanwhile, so the timing alternates the two builds' exes.

**Allocations** (`cargo test -p nf-stages --test rt_no_alloc -- --nocapture`, dev and release alike):

| | 0.2.0 | 0.3.0 |
|---|---|---|
| AEC3 chain, blocks 50-450: mean a block | 43.5 | **0.0** |
| the most in one block | 49 | 0 |
| AEC3 chain and AEC3 alone, blocks 0-1200 (the new test) | | **0** |

**Latency conformance** (`cargo test -p nf-stages --release --test aec3 --test conformance -- --nocapture`), the same on
both: AEC3's impulse offset 430 at block phases 17, 301, 0 and 479, GCC-PHAT 429.972 samples at 40.0 dB; the chain's
measured offset equals `chain.latency()` in all eight combinations (0 and 480 without AEC3, 430 and 910 with it).

**Output:** bit-identical on our config. `tests/refactor_parity.rs` recorded with 0.2.0 (`NF_GOLDEN_WRITE=1`, dev) and
compared on 0.3.0: both streams (`chain-rnnoise`, `chain-aec3-rnnoise`), 2,000 blocks each, bit for bit. `tests/aec3.rs`
prints the same: ERLE after 3 s 45.3 and 47.7 dB (RT60 0.15 and 0.3), AEC3's own 18.1 and 13.6 dB, delay 36 ms; the near
end kept -3.6 and -4.1 dB in double talk, -0.5 and -0.9 alone. Of 0.3.0's behaviour changes (its release notes), none
reaches our config, by our reading: the input volume controller and AGC2 are off, our v3 build keeps the `mul_add` path,
the dither keeps the signals out of the subnormal range where FTZ/DAZ would change a result, and one render call per
capture call never underruns the render buffer.

**The echo-path guard** (`cargo test -p nf-engine --release --lib the_echo_path_guard_holds_the_mic_for_an_early_echo --
--ignored --nocapture`): passes on both, all 16 lines identical:

| Scene | Echo | Near voice | Guard | Hold | AEC3's ERLE, last 20 s | Its delay | Far only |
|---|---|---|---|---|---|---|---|
| steady (target 12.0 ms) | 5 ms | no | off / on | 0 / 10 ms | 6.9 / 19.3 dB | 0 ms | 55.6 / 61.7 dB |
| | | yes | off / on | 0 / 10 ms | 6.8 / 15.9 dB | 0 ms | 55.0 / 60.0 dB |
| | 40 ms | no | off / on | 0 / 0 ms | 19.4 / 19.4 dB | 24 ms | 62.6 / 62.6 dB |
| | | yes | off / on | 0 / 0 ms | 15.5 / 15.5 dB | 24 ms | 61.3 / 61.3 dB |
| later (target 20.9 ms; 65 gaps, 1 drop) | 15 ms | no | off / on | 0 / 10 ms | 6.9 / 14.3 dB | 0 ms | 59.8 / 61.9 dB |
| | | yes | off / on | 0 / 10 ms | 7.0 / 12.2 dB | 0 ms | 58.2 / 60.4 dB |
| | 5 ms | no | off / on | 0 / 20 ms | 5.5 / 13.5 dB | 0 ms | 49.5 / 61.2 dB |
| | | yes | off / on | 0 / 20 ms | 5.5 / 12.4 dB | 0 ms | 45.7 / 59.2 dB |

**The offline AEC3 check** (`cargo test -p nf-engine --release --lib aec3_converges_on_the_traces -- --ignored
--nocapture`, with the ten traces in `%LOCALAPPDATA%\noise-filter\traces`): all 120 lines (ten traces and five synthetic
scenes, each silent and talking at 25/40/60/100 ms) identical. Near voice silent, live ERLE 6.6-16.1 dB (mean 13.2),
steady 11.9-16.6 (14.4). It **fails on both, the same way:** `loopback-CABLE-B_Input-8` (2026-11-02, newer than TODO 10's
analysis) at 40, 60 and 100 ms, live ERLE 10.3, 10.3 and 9.4 dB against steady 14.3, 14.5 and 14.7 (the check allows 3).
Not from the bump; left for TODO 10.

**Timing** (`tests/timing.rs`, the two builds' test exes run alternately, five times each; medians of the runs, µs a
block; the third run of each, and the last of 0.2.0's chain row, were slowed by the other session's tests):

| Row | p50 0.2.0 | p50 0.3.0 | change | p99 0.2.0 | p99 0.3.0 | mean 0.2.0 | mean 0.3.0 |
|---|---|---|---|---|---|---|---|
| RNNoise (noisy speech) | 35.1 | 34.9 | -1% | 45.9 | 45.2 | 35.7 | 35.5 |
| RNNoise model alone | 34.6 | 34.0 | -2% | 45.6 | 47.6 | 35.2 | 34.6 |
| floor + gate (NoisePost, auto gate) | 0.5 | 0.5 | 0% | 0.8 | 0.9 | 0.5 | 0.5 |
| AEC3 (echo + near end + noise) | 49.5 | **47.0** | -5% | 75.9 | 63.8 | 49.9 | 47.3 |
| AEC3, silent reference (dithered) | 45.3 | **43.2** | -5% | 63.2 | 51.7 | 41.6 | 39.0 |
| sonora, zero reference, no dither | 674.3 | **45.9** | -93% | 816.9 | 55.2 | 585.3 | 41.5 |
| chain: meters + AEC3 + RNNoise | 87.4 | **84.8** | -3% | 122.9 | 108.4 | 88.0 | 85.4 |
| chain: meters + RNNoise | 35.8 | 36.9 | +3% | 51.0 | 52.8 | 37.4 | 37.8 |

- AEC3's p50 in the four quiet runs: 49.0-50.0 µs on 0.2.0, 46.8-47.2 on 0.3.0. The RNNoise-only rows don't run sonora;
  their spread is the box's.
- The undithered zero reference no longer stalls: 0.3.0 sets MXCSR's FTZ and DAZ for each `process_*` call
  (`DenormalDisabler`) and restores the thread's word on return. The engine still dithers, and sets no FTZ/DAZ itself.

**Test changes**
- nf-app `bench`'s test failed on 0.3.0 as it was: its `rows[3].allocations > 0` ("sonora 0.2.0 allocates per block; the
  counter sees it") met `allocs 0 (0.0/block)`. Renamed `bench_times_each_row_and_no_row_allocates_once_warm`, it now
  asserts 0 for every row, AEC3 and the chain with AEC3 included, and checks the counter itself on one `Vec` in the scope.
  Stricter than before, and the counter is still proven live.
- nf-stages `rt_no_alloc`: `an_aec3_chain_allocates_only_inside_sonora` (the chain's counts equal AEC3's alone, at most 100
  in a block, blocks 50-450) passed on 0.3.0, but at 0 it guarded nothing. It became
  `the_aec3_chain_does_not_allocate_after_build`: 0 in the chain and in AEC3 alone from the first block to 1,200, past
  AEC3's initial state, through the gate modes and with the meters toggled.
- nf-stages `aec3`: new `aec3_leaves_the_thread_s_mxcsr_as_it_found_it` (MXCSR's control bits the same after building the
  stage and after 200 blocks on a speaking and then a silent reference, and a denormal product still nonzero). The FMA
  test's comment says what 0.3.0 does without FMA.
- No figure changed, so no other expectation moved.

**Checks:** `cargo fmt --all --check` clean. `cargo clippy --workspace --all-targets -- -D warnings` and the same with
`--features nf-app/onnx` clean. `cargo test --workspace --no-fail-fast`: **815 passed, 0 failed, 30 ignored**, and the
same with `--features nf-app/onnx` (on 0.2.0 before the change: 814, 0 and 30 in both; the +1 is the MXCSR test).
`python todo_check.py` clean.

**Docs:** TODO 10's entry and detail, TODO 1's pin line, `doc/dev/api/sonora-nnnoiseless.md` (new 1.11, and notes in 1.7
and 1.10), `doc/design/engine.md` (the pool allocation rate), `doc/research-2026-10.md` (the latency holds), and the
comments in `aec3.rs`, `bench.rs`, `lib.rs`, `telemetry.rs`, `rt_pool.rs`. CLAUDE.md's "Future exception" line is about
ONNX Runtime's worker and stays true.

**Left open**
- The owner decides whether sonora's per-call FTZ/DAZ is acceptable (formally undefined behaviour in Rust, as its docs
  say; scoped to its own calls, and tested not to persist). Our proposal: accept it, and keep the dither.
- The render-buffer headroom fix (for underruns) can't show offline; live loopback gaps are where it would.
- Nothing was run on audio devices.

## 13. sonora's FTZ/DAZ guard as a feature (2026-11-20)

sonora 0.3.0 sets MXCSR's FTZ and DAZ for each `process_*` call (§12). That guard is now a cargo feature, off by default.
`vendor/sonora-0.3.0` is the released crate (its `.crate` checked against `Cargo.lock`'s SHA-256) plus a three-file patch,
`vendor/sonora-0.3.0.patch`, wired in by `[patch.crates-io]` as the vendored 0.2.0 was (2026-10-26 to 10-28;
`vendor/README.md`). Only `sonora` holds the guard (`src/denormal_disabler.rs`); the other six sonora crates still come from
crates.io, and `Cargo.lock` changed only in `sonora`'s source and checksum lines. Without the feature every target builds the
guard's no-op variant, as a target without hardware flush-to-zero does upstream; with it, the code is 0.3.0's. nf-stages'
`sonora-denormal-guard` turns it on and nf-app forwards it, so `cargo build --release --features sonora-denormal-guard` builds
the app with the guard. Built and measured in a worktree from main at 446bb4a, its own `target/`, rustc 1.102.0-nightly
5f4effd57, on the dev box.

**Machine code:** the release timing test exe built without the feature has no `stmxcsr` or `ldmxcsr` (llvm-objdump); with it,
2 and 6 (the two f32 entry points the bench calls: the set, the restore, and the restore on unwind).

**MXCSR inside and after a call** (`tests/mxcsr.rs`, new; the stage-level test moved there from `tests/aec3.rs`): a recording
global allocator reads MXCSR while sonora re-initialises for a new stream format inside each of its four entry points (865
allocator calls in the first). Without the feature every reading is 0x1f80, the caller's word; with it, 0x9fc0 (FTZ and DAZ
added). After each call the thread's word is the caller's, both ways. With its expectation inverted the test fails in both
builds.

**Output:** bit-identical both ways. `tests/refactor_parity.rs`' golden was recorded on crates.io's 0.3.0 (the patch and the
two feature lines commented out, `Cargo.lock` back on the registry source, then restored) and matched the patched crate bit for
bit without the feature and with it: both streams, 2,000 blocks each. The workspace suites pass in both builds, `rt_no_alloc`
included (`the_aec3_chain_does_not_allocate_after_build`, 0 allocations).

**Timing** (`tests/timing.rs`: new rows in `nf_stages::bench` and a new ignored test, `silence_costs`). Sustained silence is 3 s
of the timing scene (far-end speech, its echo and the near end), then 30 s of an exact-zero mic and reference, timed after the
usual 1 s warm-up (2,900 blocks). "Engine's dither" adds the engine's ±1e-9 to the mic and the reference (one generator, its
seed, the mic's block first, as `pipeline.rs` does) and then the stage adds its own to the reference: the app's signal.
"Stage's dither only" leaves the mic exact zero, as a host other than our engine might send it. The two release test exes ran
alternately in ABBA order with `--test-threads 1`, after one discarded run each, 26 rounds in all. Medians of 8 runs per build
from the quiet rounds (the mean of the middle two, rounded half up), µs a block:

| Row | p50 off | p50 on | p99 off | p99 on | max off | max on |
|---|---|---|---|---|---|---|
| AEC3, silence, engine's dither (the app) | 47.2 | **46.2** | 56.8 | 55.2 | 112.9 | 101.2 |
| chain + AEC3, silence, engine's dither (the app) | 66.4 | **65.5** | 76.8 | 76.5 | 115.9 | 181.6 |
| AEC3, silence, stage's dither only (mic undithered) | 388.0 | **46.0** | 465.1 | 55.8 | 692.8 | 116.9 |
| control: sonora, zero reference, no dither (16 runs) | 667.8 | **45.7** | 707.2 | 54.5 | 877.7 | 88.5 |
| AEC3 (echo + near end + noise) | 47.0 | 47.1 | 63.3 | 64.2 | 105.3 | 94.7 |
| AEC3, silent reference (dithered) | 43.1 | 43.1 | 52.2 | 52.1 | 124.5 | 135.2 |
| chain: meters + AEC3 + RNNoise | 83.7 | 83.7 | 105.6 | 105.2 | 199.4 | 207.4 |
| chain: meters + RNNoise (no sonora) | 35.6 | 35.5 | 46.2 | 46.5 | 82.4 | 78.7 |
| RNNoise (noisy speech; no sonora) | 34.9 | 35.2 | 47.2 | 45.4 | 98.9 | 87.5 |

- The box: a Windows Server 2025 VM (Xeon 6975P-C, 4 cores, 8 threads) over RDP. Another session ran `cargo` and `rustc` on
  and off throughout, and Defender scanned their output. Total CPU was logged each second by `typeperf`: one logger from round
  1, a second from round 7 and a third from round 15, all three left running, each sampling at its own phase within the second.
  A round counts if both its runs stayed at or under 18% mean and 25% peak in every logger then running (our one timing thread
  is 12.5%): rounds 15, 16, 20 and 22-26. Rounds 1-6 ran at 16-100% mean and 10-12 beside `rustc`; 7-9, 13, 14 and 17 peaked
  at 27-46%; 18, 19 and 21 caught `pwsh`, `codex`, TiWorker and an SSM worker. Round 17 was counted before review: its off
  run peaked at 27.4% in the first logger (24.3% in the third), with TiWorker busy for 1.3 s, and its AEC3 row with signal ran
  at 50.2 µs p50 and 1.25 ms max, against 46.8-48.5 µs and at most 174 µs in the 8 counted off runs. Dropping it left the
  app's two rows as they were; the other p50s moved by 0.2 µs at most, and the control's from 668.5 to 667.8. In the 8 quiet
  rounds RNNoise's p50, which the guard can't touch, stayed within 34.2-36.5 µs. The max column is the median of the runs'
  maxima; a dithered AEC3 row's max in one run is anywhere from 57 µs to 0.5 ms in either build (the VM), so it doesn't
  separate them.
- With our dither the guard is worth about 1 µs a block in sustained silence (2%, p50) and nothing measurable while there is
  signal. The 1 µs is consistent: the runs' p50 ranges don't overlap (AEC3 46.8-47.4 off and 45.9-46.5 on; the chain 66.1-67.1
  and 65.2-66.0). p99 differs by 1.6 µs at most in the rows with sonora, within the runs' spread: RNNoise's p99, which the
  guard can't touch, differs by 1.8, and the app's AEC3 row's p99 ranges overlap (55.5-64.5 off, 52.1-81.7 on).
- No subnormal is left in sonora's state in that silence. A scan of its `Debug` dump (as sonora's own
  `silent_render_leaves_no_subnormal_state` does; a throwaway test, not kept) after 3 s of the scene and 10 s of the engine's
  dithered silence found none; with the mic left at exact zero it found 777 values in 13 fields (the capture band-split filter,
  the high-pass, AEC3's capture decimator, comfort noise, the output framer). So the 1 µs is transient arithmetic or code
  layout, not a stall. Not traced further.
- Where the dither doesn't reach, the guard matters: a zero mic without the engine's dither costs 388 µs a block without it and
  46 with it, and the undithered zero reference 667.8 and 45.7 (§12: 674 on 0.2.0, 46 on 0.3.0). In the app the engine dithers
  both. The offline `process` command doesn't dither (a chain recording's blocks already are), so a WAV with digital silence
  runs at the slow figure there without the guard: still about 25 times real time, offline.

**Checks:** `cargo fmt --all --check` clean. `cargo clippy --workspace --all-targets -- -D warnings` clean with no features,
with `--features nf-stages/sonora-denormal-guard`, with `--features nf-app/onnx`, and with both. `cargo test --workspace
--no-fail-fast`: **855 passed, 0 failed, 31 ignored** without the guard and the same with it; with `--features nf-app/onnx`,
the same, guard off and on. `tests/refactor_parity.rs` (ignored by design) as above. `python todo_check.py` clean. The vendored crate builds
without a warning both ways, and sonora's own tests pass in it both ways (211 unit tests, 4 and 14 in its test files).

**Docs:** TODO 10's entry and `doc/todo/10-aec.md` (the measurements, our proposal, and a draft upstream request for an opt-out,
not posted), TODO 1's pin line, `doc/dev/api/sonora-nnnoiseless.md` (1.11, and a new 1.12), `doc/design/architecture.md`,
`doc/design/engine.md`, `vendor/README.md`, and the comments in `aec3.rs`, `bench.rs` and `tests/timing.rs`.

**Left open**
- The owner's call on the default. Our proposal: off, as built, with the dither (1 µs a block is all the guard buys the app).
- If the stage is offered to other hosts (TODO 17), it should dither its mic input as it does its reference (or that build turns
  the guard on): with an undithered zero mic it stalls at 388 µs a block.
- Nothing was run on audio devices.
