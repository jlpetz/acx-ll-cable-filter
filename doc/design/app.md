# App spec (nf-app → `noise-filter.exe`, v0)

## Start-up (main.rs)
- **The CPU gate, first:** the build is x86-64-v3 (`.cargo/config.toml`). `main` calls
  `cpu_check::gate` before anything else (the panic hook, the log, the console, the single-instance
  check, the memory pool), and the rest runs in `app_main`, which is never inlined into it. A CPU
  that lacks a v3 feature (CPUID and XCR0, read directly) gets an ASCII `MessageBoxA` naming what's
  missing, the same text on standard error if there is one, and exit code 5. The launcher does the
  same on standard error only, and in a console of its own waits for Enter before it exits.
  `cpu_check.rs` says why the way there avoids AVX and BMI instructions;
  `tools/check_cpu_gate_asm.py` checks that in the release asm (`doc/dev/build-report.md` §11).
- **Allocator:** `#[global_allocator] static GLOBAL: nf_mem::RtAllocator = nf_mem::RtAllocator::new();`
- **Memory, in this order, before any engine thread exists:**
  1. `nf_mem::privilege::enable_lock_memory_privilege()`; a missing privilege isn't an error;
  2. `nf_mem::install_pool(PagePolicy::Large, 32 MiB)`;
  3. `nf_mem::set_working_set_minimum(64 MiB)`.
  Log the `PoolStatus`. The engine reads `nf_mem::pool_status()` for its status line.
- **Logging:** a small `log::Log` that appends to `%LOCALAPPDATA%\noise-filter\noise-filter.log`
  (with a 1 MB rotation to `.old`). Only non-real-time threads log. Add a panic hook that logs the
  panic and its location. Release is `panic = "abort"`.
- **Window or console:** a double-click opens only the window, never a console. CLI subcommands
  from a terminal print to it. Choose the mechanism from `doc/dev/api/windows-reactor.md`:
  - TMR's console subsystem plus the `consoleAllocationPolicy` manifest, which works on 26100+;
  - or the windows subsystem plus `AttachConsole(ATTACH_PARENT_PROCESS)` for subcommands.

  Windows 11 22000-26099 must not flash a console on a double-click. Document the choice in
  `main.rs`. The choice made: the windows subsystem plus `AttachConsole(ATTACH_PARENT_PROCESS)`.
- **The console launcher:** cmd.exe and PowerShell don't wait for a windows-subsystem exe, so
  `noise-filter-cli.exe` (console subsystem, about 140 KB) sits next to it. It ignores Ctrl+C
  itself, runs `noise-filter.exe` with the same arguments and inheritable copies of its standard
  handles, waits, and exits with the child's code; the child attaches to the console and handles
  Ctrl+C. Copied to `noise-filter.com`, a plain `noise-filter ...` in a terminal runs it (PATHEXT
  puts .COM first). Nothing creates the `.com` yet, and Win+R or Start search would then pick it
  too and flash a console: the packaging decides (TODO #18). Without the launcher, `run` warns
  once that Ctrl+C may not reach it.
- **One window:** a second window launch finds the first (a named mutex, then its visible window)
  and brings it forward instead of opening another. The headless `run` isn't guarded: both can
  feed the cable.

## Commands
| Command | What |
|---|---|
| *(none)*, `gui` | Opens the window: `nf_gui::run(Arc::new(Engine::new()), settings_path)`. |
| `devices` | Lists capture and render endpoints: name, id, default marks, mix format. |
| `run [--config FILE] [--mic ID] [--output ID] [--reference ID] [--aec] [--no-ns] [--floor DB] [--gate off\|static\|auto] [--threshold DB]` | A headless engine. Prints a status line once a second (state, input/output dBFS, clip, latency median/p95 and its budget's means `[mic delivery+wait+output queue+render side]`, CPU p99, drift, dropouts, RT allocations). Ctrl+C stops it cleanly (`SetConsoleCtrlHandler`). Device IDs accept a name substring too (first match, case-insensitive). |
| `latency --probe DEV --capture DEV [--mode virtual\|acoustic] [--reps N] [run's options]` | Starts the engine with those options, runs the latency test, prints the report (with the readout's budget over the sweeps: engine.md, "The latency budget"), and exits with 0 if `valid > 0`. |
| `latency --cable-only --probe DEV --capture DEV [--reps N]` | No engine: the same sweeps from the probe straight to the capture (`nf_engine::cable_latency_test`, blocking this thread), timing one cable alone (probe play-out by its AudioClock to the arrival by the capture's QPC stamps). Prints a report labelled "cable only"; exit 0 if `valid > 0`. `run`'s options, `--mode` and `--through-model` are refused rather than ignored, so nobody reads the result as the filter's. |
| `quality --probe DEV --capture DEV [--test noise\|echo] [--far DEV] [--echo-delay MS] [--seconds N] [--snr DB] [--speech FILE] [--noise FILE] [--far-speech FILE] [--save DIR] [run's options]` | Plays a test scene through the filter in-process and measures what came back (TODO 14, Lab phase 0's live half). The scene is synthetic by default (speech in pink noise at `--snr`, 10 dB; or, for `--test echo`, at least 12 s of 3 s turns of far only, double talk and near only, with the far voice's room echo 6 dB under the near speech and noise 60 dB under it, and the far voice played on `--far`, which is also the echo reference, with AEC on unless `--no-aec`); 48 kHz WAVs replace its parts. The echo test places the far voice so that its echo reaches the mic `--echo-delay` after the far end's loopback taps it, by both streams' stamps (0-300 ms, 25 by default: a speaker's output latency and the air; `quality::echo_delay`): the take starts with a 0.2 s sweep on both devices, a thread beside `play_record` finds it in the chain recording as it grows (every 20 ms, `ChainRecorder::copy_from`; only from half a second before the lead ends, where it can be, so each look correlates about a second), adds the reference's lead to its lag in the chain for the devices' own offset, and sets the far voice's start (an `nf_engine::LatePart`; the far voice plays the delay ahead of its echo, less that offset). Without it in time (or with AEC off, so no reference) the far voice plays at its default, as for devices that add nothing. The devices' offset must be measured in the take itself: on the dev VM's cables it moved by about 10 ms from one take to the next (-86 to -37 ms over six takes) and held within a take. It refuses an echo reference that is the probe, starts the engine with `run`'s options (the chain recorder armed before the start in every echo take, and with `--save`), waits 1.5 s, plays and records with `nf_engine::play_record`, and stops the engine. It aligns the recording with the clean speech window by window (2 s windows near an envelope-correlation delay, so a drifting or stepping path stays aligned; a median coefficient under 0.3 is no result) and prints the alignment (windows, delay range, drift), the levels per region (noise in pauses, also 200 ms clear of speech, and the speech's own share in its cores; or, each inside its own turns, the echo's drop where only the far end talks, pooled and per far-only turn after the first, the first as converging, against the ceiling the scene's noise sets, and the near speech's share alone, in double talk and with both talkers mid-syllable), the engine's drift and AEC3 estimates, for the echo test the echo delay (asked, and measured in each far-only turn of the chain recording: the echo against its reference by GCC-PHAT within 40 ms of where the calibration put it, which is the lag AEC3 sees, plus the reference's lead, which is the delay after the tap; the median turn and the range, with a warning when AEC3 got the echo less than 2 ms after its reference, or before it, when the delay landed more than 2 ms off the asked, or moved) and the calibration (the devices' offset and how far ahead of its echo the far voice played), the latency readout's budget over the take, the dropouts (the engine's counters over the take, and the recording's gaps) and the streams. `--save DIR` writes input, clean, far (as played against input), recording and aligned WAVs, and what the chain got and gave live: the chain recorder holds the run from its first block, stops before the engine, and writes `chain_mic.wav`, `chain_ref.wav` and `chain_out.wav` (the mic and reference blocks as the chain got them, dithered, and its output; 48 kHz mono float), `chain_blocks.csv` (a row a block: index, capture QPC, the mic's timing and moves (samples its FIFO discarded, device position skipped unfilled), the reference's take with the echo path in samples, its lead, slips, owed zeros and jumps, AEC3's estimates, a flag where the chain's settings changed, and the block's latency budget) and `chain_config.json` (the engine config). One line says how many blocks were saved, from where in the run, any lost to a full ring, any settings change, and the `process --warm-up` command that reruns them into `chain_out.wav` exactly. Exit 0 when aligned, 3 when not. |
| `process IN.wav OUT.wav [--ref REF.wav] [--align] [--warm-up] [--config FILE] [--aec] [--no-ns] [--floor DB] [--gate ...]` | Offline through the same `Chain` and stages. Input must be 48 kHz (mono, or downmixed); the reference too. Writes 48 kHz mono f32. Prints the chain latency. The chain starts fresh, where the engine first runs three dithered silent blocks through it; `--warm-up` does the same (`nf_engine::warm_up_chain`), so `quality --save`'s `chain_mic.wav` and `chain_ref.wav`, with `--config chain_config.json`, rerun into exactly `chain_out.wav` (the input isn't dithered: those blocks already are). |
| `bench [IN.wav] [--seconds N] [--aec]` | Offline per-block CPU: p50/p99/max µs for RNNoise, for AEC3 (with a synthetic reference), and for the whole chain, plus `nf_mem::thread_scope_allocs()` growth after warm-up (inside an `RtScope`, after the FFT warm-up). Uses a synthetic speech-like signal when no file is given. `--seconds` is capped at 300 (the synthetic echo is one FFT convolution: about 1.2 GB at 300 s). |
| `setup-large-pages [--sid SID]` | Not elevated: `run_elevated(nf_mem::privilege::helper_args(<current SID>))`, that is `setup-large-pages --sid <SID> --elevated-helper`, then report its exit code. The hidden `--elevated-helper` marks the relaunch: a helper that finds itself not elevated (UAC off) exits with 4 instead of relaunching itself again. Elevated: `grant_lock_memory_privilege(sid)`, where the SID is `--sid`'s, else the desktop session's user (Remote Desktop Services' record, then Explorer's token), so an over-the-shoulder admin's prompt still grants the account that runs noise-filter; each branch names the account. Afterwards print "Sign out of Windows and back in, then restart noise-filter". The window's "Enable large pages" button runs the same helper. |
| `--help`, `--version` | |

Argument parsing is hand-rolled (no clap) to keep the binary small.

## Exit codes
0 is OK; 1 is a usage error; 2 is an engine, device or file error; 3 is a test with no valid result
(`latency`: no valid repetition; `quality`: the recording didn't line up with the scene; either one
cancelled); 4 is the large-page helper started without elevation. `nf_mem::privilege::HELPER_NOT_ELEVATED`
holds the 4 the window reads, and a compile-time check ties it to `Exit::NotElevated`. 5 is a CPU
without x86-64-v3's features (`Exit::UnsupportedCpu`, tied by a compile-time check to
`cpu_check::EXIT_CODE`, which the CPU gate returns in both exes).
