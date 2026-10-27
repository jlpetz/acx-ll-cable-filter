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
  - `with_intra_threads(1)`, `with_intra_op_spinning(false)`, `with_flush_to_zero`,
    `with_memory_pattern(true)`; keep the arena (don't add `ep::CPU::default()`).
  - Create the input values once over pinned buffers.
  - Preallocate every output with `OutputSelector::preallocate`, using two `RunOptions` that
    alternate cache sets (one frame's outputs are the next frame's inputs).
  - Optionally register a pinned TLSF `OrtAllocator` (rlsf, behind an uncontended lock) with
    `RegisterAllocator` + `with_env_allocators`. It catches the pattern block, outputs, temp space
    and prepacked weights. Raw weights need `with_external_initializer` over pinned memory.
  - **What remains:** a few UCRT-heap allocations per run. ORT is linked statically on Windows, so
    a C++ shim replacing `operator new/delete` in our exe could route them too; untested.
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

## Open
- How many heap allocations and how long one ORT 1.31 `Run` takes for UL-UNAS. Measure with
  WPR/ETW.
- Whether the operator-new shim works with pyke's static ORT.
