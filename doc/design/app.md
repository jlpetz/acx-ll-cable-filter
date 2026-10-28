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
| `run [--config FILE] [--mic ID] [--output ID] [--reference ID] [--aec] [--no-ns] [--floor DB] [--gate off\|static\|auto] [--threshold DB]` | A headless engine. Prints a status line once a second (state, input/output dBFS, clip, latency median/p95, CPU p99, drift, dropouts, RT allocations). Ctrl+C stops it cleanly (`SetConsoleCtrlHandler`). Device IDs accept a name substring too (first match, case-insensitive). |
| `latency --probe DEV --capture DEV [--mode virtual\|acoustic] [--reps N] [run's options]` | Starts the engine with those options, runs the latency test, prints the report, and exits with 0 if `valid > 0`. |
| `process IN.wav OUT.wav [--ref REF.wav] [--aec] [--no-ns] [--floor DB] [--gate ...]` | Offline through the same `Chain` and stages. Input must be 48 kHz (mono, or downmixed); the reference too. Writes 48 kHz mono f32. Prints the chain latency. |
| `bench [IN.wav] [--seconds N] [--aec]` | Offline per-block CPU: p50/p99/max µs for RNNoise, for AEC3 (with a synthetic reference), and for the whole chain, plus `nf_mem::thread_scope_allocs()` growth after warm-up (inside an `RtScope`, after the FFT warm-up). Uses a synthetic speech-like signal when no file is given. `--seconds` is capped at 300 (the synthetic echo is one FFT convolution: about 1.2 GB at 300 s). |
| `setup-large-pages [--sid SID]` | Not elevated: `run_elevated(nf_mem::privilege::helper_args(<current SID>))`, that is `setup-large-pages --sid <SID> --elevated-helper`, then report its exit code. The hidden `--elevated-helper` marks the relaunch: a helper that finds itself not elevated (UAC off) exits with 4 instead of relaunching itself again. Elevated: `grant_lock_memory_privilege(sid)`, where the SID is `--sid`'s, else the desktop session's user (Remote Desktop Services' record, then Explorer's token), so an over-the-shoulder admin's prompt still grants the account that runs noise-filter; each branch names the account. Afterwards print "Sign out of Windows and back in, then restart noise-filter". The window's "Enable large pages" button runs the same helper. |
| `--help`, `--version` | |

Argument parsing is hand-rolled (no clap) to keep the binary small.

## Exit codes
0 is OK; 1 is a usage error; 2 is an engine, device or file error; 3 is a test with no valid result;
4 is the large-page helper started without elevation. `nf_mem::privilege::HELPER_NOT_ELEVATED`
holds the 4 the window reads, and a compile-time check ties it to `Exit::NotElevated`. 5 is a CPU
without x86-64-v3's features (`Exit::UnsupportedCpu`, tied by a compile-time check to
`cpu_check::EXIT_CODE`, which the CPU gate returns in both exes).
