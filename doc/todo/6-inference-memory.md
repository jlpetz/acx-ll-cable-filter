# 6. Hand-written Rust ports and inference memory

Status: fact-checked 2026-10-26, against the onnxruntime v1.31.0 sources (the version ort rc.15
ships), the ort, rlsf and tract v0.23.9 sources, and Microsoft Learn. Numbers marked "estimate"
are ours.

## Decision (owner, 2026-10-26)
- **Large pages by default.** Requiring `SeLockMemoryPrivilege` is acceptable.

## Our design and answers (not owner decisions)
- **Setup:**
  - One elevated grant to the user account (the installer, or a `--setup-large-pages` step; TMR does it with secedit, `privileges.rs`).
  - It takes effect at the account's next sign-in (Microsoft: "the next time the owner of the account logs on"; no reboot). Relaunching the app isn't enough: TMR's in-app "Restart TMR" message is wrong, its README's "sign out and back in" is right.
  - Grant to the interactive user's SID, passed in by the non-elevated launcher, never to the elevated process's `USERNAME`: a standard user who elevates with an admin's password would otherwise grant the admin. TMR's `auto_grant_large_page_privilege` can't be reused as-is.
  - At run time the app enables the privilege in its own token (`AdjustTokenPrivileges`), which needs no admin.
- **Running without admin afterwards (unconfirmed):** a standard account should keep the right. An admin account in Admin Approval Mode may lose it in its UAC-filtered token (Windows Internals lists only five privileges kept; cited from memory, not fetched), and would then get the fallback. XMRig's docs say "On Windows 10 once you obtain the privilege, admin rights no longer required to use huge pages". That's third-party, so confirm it with a non-elevated launch from an admin account in Admin Approval Mode, and from a standard account. Microsoft only says the non-elevated token loses "administrative Windows privileges", without a list.
- **Correction to the premise:** locking 4 KB pages with `VirtualLock` does *not* need this privilege; it needs working-set quota (`SetProcessWorkingSetSizeEx`). The privilege is needed only for large pages (and AWE).
- **Allocation:**
  - One block at start-up: weights, state, scratch and audio buffers.
  - Microsoft says to allocate large pages once at start-up, because contiguous memory gets scarce and allocating it "can significantly affect system performance".
  - It's always read/write, non-pageable, and not part of the working set.
- **Fallback (our proposal; the owner said requiring the privilege is fine, so refusing to start is the alternative):** if the privilege is missing or the allocation fails, use 4 KB pages pinned with `VirtualLock` and show a warning ("large pages unavailable: run setup").
- **Our estimate of the gain:** small for 0.7 MB models (under 0.3%), a few percent for DFN3_ll. The guaranteed residency is the main value.

## The question (owner, 2026-10-26)
We said both ONNX runtimes allocate on each call. The owner asked why, and whether persistent,
non-pageable 2 MB pages from VirtualAlloc2, as TMR uses, could avoid that.

## Answered: no INT8, no GPU (owner's question, 2026-10-25)
- No INT8 or GPU version exists for these models.
- INT8 risks artifacts for no gain: per-call overhead, not MACs, sets the cost. (UL-UNAS is ~0.56
  MMAC per 16 ms frame, ~0.05% of a core's arithmetic peak.)
- A GPU adds a per-frame round trip, and frames queue behind the game.
- So: FP32 on the CPU, one thread.

## Why the runtimes allocate
"Allocates per call" means allocator calls into memory that is already committed, not fresh OS
pages. The cost is microseconds of jitter from bookkeeping and uncontended locks. The bigger risk is
that all of it is pageable.

- **ONNX Runtime, per `Run()`:**
  - It builds a new ExecutionFrame each time. A session supports concurrent runs, so intermediate
    memory can't be owned by the session (our reading of the code).
  - With the memory pattern on (the default) it allocates one peak-size block per run and frees it
    afterwards.
  - Outputs are new caller-owned values unless preallocated.
  - The BFC arena takes a mutex on every alloc and free, and never shrinks by default.
  - Bookkeeping objects (`feed_name_vec`, `fetch_vec`, FeedsFetchesManager, OrtValue/Tensor) go
    through C++ `operator new` on the UCRT heap. No `OrtAllocator` can intercept them.
  - `telemetry_mutex_` is taken on every successful run.
  - With `intra_op_num_threads = 1` there is no thread pool and no spinning.
- **ort rc.15 on top:**
  - Every output that isn't preallocated costs a `CreateAllocator` (C++ heap) plus Rust `Arc`s;
    `run_binding` adds an array and wrappers each call.
  - Only `OutputSelector::preallocate` is allocation-free for outputs.
  - Its name and pointer `SmallVec`s spill to the heap above 6 inputs, 4 outputs or 768 bytes of
    names.
  - **Traps:**
    - `ep::CPU::default()` *disables* the arena (`use_arena = false`).
    - `Allocator::default()` is ORT's non-arena heap allocator.
    - The `Reserve` field of `OrtAllocator` needs ORT API 18+, while ort-sys defaults to 17.
- **tract:**
  - It hands each op a fresh output tensor on every eval, with TValue wrappers and TVec spills.
  - Its CPU backend has no arena (only its CUDA and Metal backends do).
  - The matmul scratch is a reused thread-local.

## What protects the audio thread
- **Paging is the real risk.** Under memory pressure (a game using most of the RAM) Windows trims
  working sets.
  - Since Windows 10 a trimmed private page usually comes back from the in-RAM compression store,
    which costs CPU time rather than disk I/O.
  - Code pages (`onnxruntime.dll`, our exe) are re-read from disk.
  - Per-fault cost is unmeasured; measure with ETW hard-fault events under load.
- **Fix without admin:**
  - `SetProcessWorkingSetSizeEx(min, max, QUOTA_LIMITS_HARDWS_MIN_ENABLE)`; raising the limits
    needs a privilege "all users generally have". A hard minimum is a size floor, not a per-page
    lock.
  - So also `VirtualLock` the hot ranges (buffers, weights, and even the `.text` section). It's
    capped at the minimum working set minus a small overhead.
- **Large pages:**
  - Always resident and non-pageable, but they need `SeLockMemoryPrivilege` and must be
    allocated once at start-up (contiguous memory gets scarce after long uptime).
  - They're always read/write, so they can't be combined with read-only weights.
  - Benefit (our estimate): under 0.3% for a 0.7 MB model. For DFN3_ll's 39 MB, probably low
    single-digit percent of inference time. fp16/int8 weights would gain more there.
- **QoS:** a process whose window is minimised or hidden gets Low QoS unless it's audible or its
  threads are MMCSS-tagged. The engine's audio threads must use MMCSS "Pro Audio" (#8). That
  matters given the owner's minimise-for-now plan.

## TMR code to reuse
- `TMR-APP/src/memory/backend.rs:191-394`:
  - `VirtualAlloc2(MEM_RESERVE | MEM_COMMIT | MEM_LARGE_PAGES)` with
    `MEM_EXTENDED_PARAMETER_NONPAGED_LARGE` / `_HUGE`;
  - stepping down from 1 GB to 2 MB to normal pages.
- Caveat: its auto mode uses 2 MB pages only for requests of 4 MB or more, and `Prefer(Large)` falls back to 4 KB silently (no signal to the caller). So we write our own call: `MEM_LARGE_PAGES` with 2 MiB size and alignment (TMR's alignment rule), classify failures (1314 = no privilege, 1450 = no contiguous memory), then fall back to 4 KB + `VirtualLock` with a warning in our code. Never call TMR's `setup_large_pages_automatically()` from the allocation path (it runs secedit when elevated).
- **Don't** use `MEM_REPLACE_PLACEHOLDER`. A refused large-page replace followed by a free
  bugchecks build 26100 (0x139; see `TMR-APP/doc/todo/75-allocators.md`).
- Private VirtualAlloc2 memory can't be shared between processes. Cross-process audio (#17) needs
  a `CreateFileMapping2` section instead.

## Strategy
- **Both paths:**
  - large pages by default (Decision above), with the `VirtualLock` fallback;
  - the engine is its own small process;
  - a hard working-set minimum (engine size + 32-64 MB), plus `VirtualLock` on hot ranges;
  - pre-fault every buffer at init;
  - FFT and other thread-local state warmed up on the audio thread itself;
  - our own large-page call (above), sized at start-up for the largest model; a model switch that doesn't fit restarts the engine.
- **ORT path (interim):**
  - `with_intra_threads(1)`, `with_intra_op_spinning(false)`, `with_memory_pattern(true)`; keep the arena (don't add
    `ep::CPU::default()`). Not `with_flush_to_zero`: it sets FTZ/DAZ for good on the first session's thread ("Denormals and
    flush-to-zero" below).
  - Create the input values once over pinned buffers.
  - Preallocate every output with `OutputSelector::preallocate` (built: one `RunOptions`, and the three state outputs
    copied into the state inputs after each run, 27 KB, about 3 µs, simpler than alternating two option sets).
  - Optionally register a pinned TLSF `OrtAllocator` (rlsf, behind an uncontended lock) with
    `RegisterAllocator` + `with_env_allocators`. It catches the pattern block, outputs, temp space
    and prepacked weights. Raw weights need `with_external_initializer` over pinned memory.
  - **What remains:** measured 2026-11-02 at about 3,710 C runtime allocations and frees a run ("Measured" below), not a
    few. ORT is linked statically on Windows, so a C++ shim replacing `operator new/delete` in our exe could route them
    too; untested.
- **Hand-written ports (long term):**
  - `process()` allocates nothing;
  - weights, state and scratch in one 64-byte-aligned arena at load;
  - the arena lives in the start-up large-page block (large pages by default, Decision above), with the `VirtualLock` fallback;
  - enforce with a counting `#[global_allocator]` in tests (`assert_no_alloc` is hard-wired to
    System and can't be layered).
- **tract meanwhile:** a router `#[global_allocator]`:
  - a const thread-local flag picks the pool;
  - frees are routed by address range;
  - rlsf TLSF over the pinned region;
  - model loading runs under the flag;
  - it falls back to System and counts when the pool is full.
- **Existing ports show the approach:**
  - `nnnoiseless`: mature. Allocation-free per frame after a warm-up call, but reset builds a new
    Box.
  - `ul_unas_rs`: immature and unvalidated (0 stars, 3 commits). Its README's 16 ms counts only the
    stream offset; UL-UNAS's latency is 32 ms.

## Measured (2026-11-02): ONNX Runtime's heap per run, for UL-UNAS
The dev VM (Windows Server 2025 over Remote Desktop, 8 vCPUs; the figures say nothing about the owner's desktops), ONNX
Runtime 1.31.0 through ort 2.0.0-rc.15 (pyke's static build, `ORT Build Info: git-branch=rel-1.31.0, git-commit-id=402979a`),
a release build, FP32, CPU, one thread, `ulunas_stream_simple.onnx`, its inputs the native STFT frames of `noisy_0174`.
Uncommitted when measured (the working tree on top of 878b029). `cargo test -p nf-ort --release --test ort -- --ignored
todo6 --nocapture --test-threads 1`. Two runs: the first (before the review's fixes, with `with_flush_to_zero` on) and the
second (after them: no flush-to-zero, the catch-all counter in); the allocation counts are the same in both.

### Method: the import address table, patched (no admin rights, no external tools)
- `nf_ort::heap_probe` (tests and `bench` only, never the live path) walks the import tables of the executable,
  `msvcp140.dll` and `msvcp140_1.dll`, matches `malloc`, `free`, `calloc`, `realloc`, `_aligned_*`, `_recalloc` and the
  `_*_base` variants by name in any DLL (ONNX Runtime's `operator new` and CPU allocator reach them through the exe's
  imports from `api-ms-win-crt-heap-l1-1-0.dll`), and `HeapAlloc`, `HeapReAlloc` and `HeapFree` (direct callers: Rust's
  `System` allocator), and points each at a hook that counts and forwards. Counting is per thread (const thread-locals,
  only while that thread's flag is on), so one run's calls are counted on the thread that runs it. Byte counts saturate
  (a `calloc` whose product overflows can't panic inside a hook).
- **The catch-all (added after the review):** `ucrtbase.dll`'s own imports of `HeapAlloc`, `HeapReAlloc` and `HeapFree`,
  which every C runtime heap call passes through whoever made it. It patched 20 imports in all (9 in the exe, 4 in
  msvcp140, 4 in msvcp140_1, 3 in ucrtbase), none skipped.
- **Self-check, and the independent cross-check until WPR/ETW is run:**
  - building a session shows hundreds of thousands of C runtime allocations on the building thread, so the probe isn't
    blind (no static C runtime or mimalloc inside pyke's library);
  - on a run the per-caller hooks and the catch-all agree exactly: 3,710 and 3,710 allocations, 3,710 and 3,710 frees
    (asserted in `cargo test`, `memory.rs`); kernelbase's `Rtl*Heap`, the exe's `VirtualAlloc`, vcruntime and every other
    thread showed 0 a run in the verifier's check;
  - a process's first session creation is where they differ: the catch-all sees 976 allocations more (callers outside the
    patched tables), so the per-caller figure is a lower bound there and the catch-all's is the figure.
  WPR/ETW, on the owner's desktop with admin rights, stays the cross-check.
- **Rejected:** heap statistics (`HeapSummary`, `HeapWalk`: net figures, blind to pairs within a run; used only for the
  leak check below); ONNX Runtime's allocator hooks (an env `OrtAllocator`: they see arena growth and tensors, not
  `operator new`); `_CrtSetAllocHook` (debug C runtime only).

### Figures
- **Session creation** (the process's first): **about 898,000 C runtime allocations** (the catch-all 898,147; the
  per-caller hooks see 897,171) and 757,666 frees, 66.5 MB asked for in all, 113 direct heap calls, 117-119 ms. A later
  session in the same process: 666,796 (both counts the same), 80 ms.
- **The first run:** 4,131-4,147 allocations, 4,127-4,143 frees, 451 KB.
- **Steady state** (10,000 runs after 1,000 warm-up runs, defaults: arena on, memory pattern on, Level 3, outputs
  preallocated), per run:
  - **3,710 C runtime allocations and 3,710 frees** (min, median and max 3,710/3,710/3,711), 305 KB asked for; the
    catch-all the same;
  - direct heap calls 0, Rust allocations 0 (ort's Rust side, with the outputs preallocated, touches no heap);
  - soft page faults 10.2-10.3 per 1,000 runs; private bytes 25.1 → 25.5 MB; live heap blocks 142,842 → 142,847 (no leak);
  - **run time** (the worker's deadline reasoning quotes these, the 10,000-run figures, not the 30 s timing row's):

    | Run | p50 µs | p99 | p99.9 | max | cycles p50 |
    |---|---|---|---|---|---|
    | first (FTZ on) | 950 | 1,961 | 2,869 | 5,405 | 2.55 M |
    | second (FTZ off) | 969 | 1,053 | 1,106 | 1,449 | 2.60 M |

    The VM's scheduling moves the tail from run to run (other runs: p99 1.0-1.4 ms); all of it is far under the worker's
    deadline (a block, 10 ms, at the depth's spacing: below).
- **Variants** (3,000 runs after 500 each; the second run's, FTZ off):

| Variant | C allocs a run | KB a run | Rust a run | Faults / 1000 | p50 µs | p99 µs |
|---|---|---|---|---|---|---|
| defaults (again) | 3,710 | 305 | 0 | 8.0 | 983 | 1,076 |
| memory pattern off | 3,943 | 315 | 0 | 8.0 | 1,038 | 1,092 |
| arena off (`ep::CPU` without it) | 3,264 | 655 | 0 | 27.0 | 899 | 956 |
| optimisation level 1 | 3,683 | 302 | 0 | 6.7 | 976 | 1,068 |
| optimisation level 2 | 3,683 | 302 | 0 | 10.0 | 981 | 1,077 |
| outputs not preallocated | 3,734 (+16 heap) | 307 | 8 | 4.3 | 993 | 1,064 |
| exact digital silence | 3,710 | 305 | 0 | 10.0 | 947 | 1,013 |
| dithered silence (±1e-9) | 3,710 | 305 | 0 | 9.3 | 956 | 1,024 |

- **What the app's paths make** (`tests/ort/memory.rs`, `tests/ort/timing.rs`): the audio thread's side of the pipelined
  stage makes 0 Rust allocations and 0 C runtime or heap calls a block, the catch-all's included (asserted, 550 paced
  blocks with a reset; and 2,900 blocks in the timing row, p50 4.2 µs, p99 5.1). **The worker makes 0 Rust allocations a
  job** (asserted: `rt_no_alloc.rs`, and 2,999 jobs in the timing row) and 7,420 C runtime calls a run. The 2 the first
  report saw were the model info's clone (its owned id and name) sent with "ready" after the baseline was taken; the
  baseline is now taken after it. A plain `cargo test` pins the run at 5,000 C runtime allocations at most
  (`MAX_CRT_ALLOCS_PER_RUN`), to catch an ONNX Runtime update or an option that makes it heavier.

### Denormals and flush-to-zero (corrected after the review)
- The first report said "with one thread `with_flush_to_zero` reaches nothing". Wrong: ONNX Runtime applies
  `session.set_denormal_as_zero` once a process, to the thread that builds the first session, and leaves FTZ and DAZ set in
  its MXCSR for good (the verifier: 0x1f80 to 0x9fe0 on that thread; a later session's thread kept 0x1f80). So the first
  run's silence variants ran with FTZ/DAZ on, inline (`process`, `bench`, the tests) it was set on the thread that runs
  AEC3 and the floor too (against the project's no-FTZ/DAZ rule), and live only the first worker after a process start had
  it.
- It is off now (`session.rs`). With it off, exact silence costs p50 947 µs and dithered silence 956 against speech's 969:
  no denormal stall on UL-UNAS (the verifier's FTZ-on figures, 875-887 µs for silence after speech, against 888-891 with it
  off, say the same). `memory.rs` checks, in a process of its own (where its session is the first), that building and
  warming up a session leaves the thread's MXCSR control bits as they were (0x1f80 before and after; only the status flags
  move) and that a denormal survives on it.
- A model that needs FTZ would set and restore MXCSR on its worker alone, as part of the worker exception.

### What it means (ours)
- "A few UCRT-heap allocations per run" was wrong by three orders of magnitude: about 3,700 malloc/free pairs a run, 5 a node
  of the 697, are ONNX Runtime's own bookkeeping (shapes, kernel temporaries, intermediate tensors' objects), not the
  arena's. The arena and the memory pattern take only some hundreds of them; preallocating the outputs saves ort's 8 Rust
  and 16 heap calls.
- **The worker exception holds:** none of this reaches the audio thread. But each run takes the process heap's (LFH) paths
  about 7,400 times, so a stall in another thread's heap use can delay the worker; a miss costs one block of gain-matched dry
  signal, counted (`InferenceStats::misses`).
- Level 2 against 3: the same allocations, the same p50; neither is a reason to move from Level 3. The bench keeps 3.
- Next options, in order: (1) the hand port (zero allocations a run; it could run on the audio thread at 1440); (2) an
  `operator new` shim in our exe routing ONNX Runtime's C++ allocations to a dedicated, pinned heap (~3,700 calls a run;
  the probe shows the import path such a shim would take); (3) an env `OrtAllocator` over TLSF in the locked pool, which
  would catch only the arena's share.

## The pipeline's depth and the worker's deadline (after the review)
- **The deadline is the call `D` calls later, not a block's time.** The engine processes every complete block of a wake back
  to back (`audio.rs`, step 3), so a wake that brings `k` blocks gives the worker microseconds for the jobs pushed earlier
  in it. With the worker caught up before the wake, it costs `max(0, k - D)` misses (each a block of gain-matched dry
  signal). The first design had `D = 1`.
- **Measured on the dev VM** (`nf_ort::bench::bursts`, the real pack on an unwaited worker, 300 blocks at real time on
  average; the timing test and `bench --model` print it; run p50 about 1.07-1.09 ms throughout):

  | Wakes of | depth 1 | depth 2 |
  |---|---|---|
  | 1 block every 10 ms | 0 misses (0%) | 0 (0%) |
  | 2 blocks every 20 ms | 148 (49.3%) | 0 (0%) |
  | 3 blocks every 30 ms | 198 (66.0%) | 98 (32.7%) |

  `pipeline.rs`'s `a_burst_costs_the_blocks_past_the_depth_and_nothing_else` checks the rule exactly and deterministically
  (the worker held through each burst): depths 1-3, bursts of 1, 2, 3 and 6, the misses and late results counted to the
  block, no gap, and every other block the model's.
- **Expected per device mode at the default depth 2** (ours, from TODO 8's and 13's delivery figures; the soak's new
  "wakes" line measures it):
  - the exclusive mic (the default, 3.33 ms periods, no bursts on the VM): one block a wake, so about none, but for a late
    wake that brings three;
  - a shared 10 ms period delivered steadily (a sound card in step with the engine): none;
  - VB-CABLE shared on the VM (0, 480 or 960 frames a 10 ms event, 960 a fifth to a third of the time): none at depth 2,
    20-33% of blocks at depth 1;
  - a 20 ms device period (some USB and Bluetooth devices): none at depth 2, half the blocks at depth 1;
  - a stall's backlog of `k` blocks (up to the mic FIFO's 20): `k - 2` misses, and never a gap (the rings hold 32), so the
    model's state carries on.
- **Chosen (ours): depth 2 by default, 60 ms live for UL-UNAS** (stream offset 2400 = 1440 + 2 × 480, plus the 10 ms block),
  against 50 ms at depth 1. The data that exists says two-block wakes are common (the cables, 20 ms devices), and at depth 1
  they cost a fifth to a half of the blocks, which an A/B by ear would hear as unfiltered noise and blame on the model.
  `--model-depth 1-4` (the config's `model_depth`) sets it; the soak summary's "wakes" line counts the wakes of 1, 2, 3 and
  4+ blocks and says what depths 1, 2 and 3 would have missed, and the status line counts the bursty wakes with a model,
  so the owner can pick 1 where bursts prove rare. Since superseded (2026-11-02, ours): the depth is automatic by default,
  1 when the mic delivers a block a wake, from those same wake counts (TODO 5's detail, "The automatic depth").

## Open
- Whether the operator-new shim works with pyke's static ORT.
- The same figures on the owner's desktops (WPR/ETW with admin rights as the cross-check, and the probe), and a soak's
  "wakes" line there to confirm or lower the depth.
