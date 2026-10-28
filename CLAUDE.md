# CLAUDE.md — noise-filter

A free, low-latency, system-wide mic noise filter and echo canceller for Windows 11, in Rust. It is
a standalone app: mic → echo cancel (WebRTC AEC3 via `sonora`) → noise stage (RNNoise first, then
ONNX model packs) → a virtual cable that Discord, Teams or OBS use as their mic. The settings window
is windows-reactor (WinUI 3). Public release is the intent.

## What's here
- `TODO.md` and `doc/todo/NN-slug.md` are the task list and its reasoning (see "TODO files").
- `doc/research-2026-10.md` is the fact-checked research record (models, AEC, WASAPI, GUI,
  landscape). Read it before re-researching anything; recheck version-sensitive items before
  coding against them.
- Code is reused from TMR (`C:\Users\Administrator\Downloads\TMR\TMR-APP`, a separate repo; read it,
  don't change it from here):
  - `src/memory/backend.rs`: large pages (the direct `VirtualAlloc2` path);
  - `src/memory/privileges.rs`: granting `SeLockMemoryPrivilege`;
  - `crates/tmr-gui`: Reactor 0.100.0 patterns.

  Paths written as `TMR-APP/...` in the docs mean that folder.

## Who runs what
- **The owner runs the app** (`cargo run`, the exe), including every live test with audio devices.
  Don't run it unless asked for a one-off.
- `cargo check`, `cargo clippy`, `cargo build` and `cargo test` are fine to run. Tests must not touch
  audio devices: they run the offline WAV harness (TODO #2).
- Ask before `cargo bench`, and before any test that allocates large pages or exhausts memory. Tests use 4 KB pages (the arena's test mode); the large-page path runs only in the app, or in an `#[ignore]`d test the owner runs.
- After a crash, read the logs and give the owner WinDbg commands; the owner analyses the dump.
- **Never use `MEM_REPLACE_PLACEHOLDER` for large pages.** On build 26100 a refused large-page
  replace followed by a free bugchecks the machine (0x139). Use TMR's direct path only.
- **The dev box** is Windows Server 2025 over Remote Desktop, with no real mic or speakers. Results
  from it say nothing about acoustics or device latency.

## Real-time rules
- Audio threads (capture, DSP, render, inference workers) register with MMCSS "Pro Audio".
- Nothing on an audio thread allocates, locks, logs or makes a blocking call. Enforce it in tests. (Future exception: an ORT inference worker, whose remaining per-run allocations are measured and kept off the capture and render threads, TODO #6.)
- Pin exact versions of preview crates (`wasapi`, `windows-reactor`, `ort`); expect migrations.

## Checks after writing code (TMR's routine; the owner, 2026-10-28)
- **`cargo fmt --all` first** (`rustfmt.toml`: TMR's layout, dense, 140 wide). A layout worth keeping
  (a table, one row a line) takes `#[rustfmt::skip]`. No comments inside a `use` list: they hold it
  one name a line. A reformat-only commit goes in `.git-blame-ignore-revs`. Comments wrap at 140 too,
  by hand (rustfmt leaves them alone); older comments still wrap nearer 100-130.
- **Then clippy at zero warnings:** `cargo clippy --workspace --all-targets -- -D warnings`, after
  non-trivial changes and before every commit. Fix rather than `#[allow]`.
- **Then the tests:** `cargo test --workspace`.
- **After touching the start-up path** (either exe's `main`, `cpu_check.rs`) or updating the
  toolchain: `python tools/check_cpu_gate_asm.py` on a fresh `--emit asm` build (TODO #18).

## Commits
- No `Co-Authored-By` line. Write "TODO NN", never `#NNN` (GitHub links it to an issue).

## TODO files
- **An entry holds status, decisions and the next step, nothing more.** Aim for under 2 KB;
  `python todo_check.py` fails anything over 3 KB. Options, reasoning, measurements and history go in
  `doc/todo/NN-slug.md`, linked from the entry's `**Detail**:` line. When the design moves on, update
  that file rather than growing the entry.
- **Format:** `### NN. [area] Title`, then `**Priority**: … | **Status**: …`, the state, `**Next**:`,
  `**Detail**:`.
- **Closing:** replace the entry with one or two sentences in `TODO_ARCHIVE.md` (what was done, the
  date, the commit) plus the Detail link. The detail file stays where it is.
- **Reading:** `grep -n "^### " TODO.md` lists the items. Read only the entry you need, then its
  detail file. Never read either file whole. When listing outstanding items, enumerate from the file.
- **Decisions:** record a decision as the owner's only when the owner made it. Proposals are marked
  as ours until the owner agrees.
- Other sessions and workflow agents edit these files. Re-read an entry right before editing it, and take a new item's number from `python todo_check.py`.
- Qualify external issue numbers with their repo (`gtcrn #76`, `tract #2629`); a bare `#N` means this TODO list.
- Run `python todo_check.py` after editing either file.
