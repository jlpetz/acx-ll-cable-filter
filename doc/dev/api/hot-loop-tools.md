# Hot-loop tools and codegen rules for noise-filter's DSP loops

This note covers how to check what the compiler made of our per-sample loops, and how to write those
loops so they vectorise, stay in registers and make no calls. Everything here was read from source:
- **TMR's tools:** `TMR/…` is `C:\Users\Administrator\Downloads\TMR\TMR-APP\…`, read-only.
- **The pinned nightly's std:** `STD/…` is
  `%USERPROFILE%\.rustup\toolchains\nightly-x86_64-pc-windows-msvc\lib\rustlib\src\rust\library\…`,
  rustc 1.101.0-nightly (1a2c4797e 2026-10-21).
- **Our files:** `tools/hot_loop_spills.py` (adapted 2026-10-26, its tests in
  `tools/test_hot_loop_spills.py`) and `crates/…`.
- **Refreshed 2026-10-26** after the first builds: the bin's asm now exists, and §0 and §3 say
  what the code does now (DelayLine, `Dither`, the gate's one-pole, index conversion). Line
  numbers below are from that day's tree.

Two kinds of claim aren't backed by a source read here, and are marked:
- **(LLVM)** is compiler behaviour: what to expect. The tool is how to check it.
- **(MS)** comes from Microsoft's documentation.

---

## 0. Pitfalls first

1. **Read the bin's post-LTO asm, never a lib's.** A lib's asm, e.g. from
   `cargo rustc --release -p nf-core -- --emit asm`, is pre-LTO. It has the same functions and makes a
   believable table. By default the tool reads the newest asm with a `main` under any target dir's
   `release/build/nf-app/` (`binary_asm`, `tools/hot_loop_spills.py:745-777`). A newer lib asm is
   passed over with a note on stderr, and an asm with no `main` named on the command line is refused
   (exit 1). `nf-app`'s `main` runs the whole app now (`crates/nf-app/src/main.rs`), so the bin's asm
   holds the audio path: 202 MB and 9.0 M lines, with `main` at line 85 148.
2. **Rust has no fast-math, so a float reduction stays serial.**
   - Rust never reassociates float maths: `iter().sum::<f32>()` is `fold(-0.0, |a, b| a + b)`
     (`STD/core/src/iter/traits/accum.rs:161-168`), one `addss` chain (LLVM).
   - To vectorise a sum, peak or RMS, write independent accumulator lanes (§3 R4). The other route
     is `algebraic_add` (stable since 1.98.0, `STD/core/src/num/f32.rs:1835`), but our declared
     `rust-version` is 1.91 (`Cargo.toml:21`).
3. **`f32::mul_add` is an FMA instruction only with FMA.** The baseline is `x86-64-v3`
   (`.cargo/config.toml:12`, since 2026-10-28), which has FMA, so `mul_add` is one `vfmadd…`.
   `mul_add` is `intrinsics::fmaf32` (`STD/core/src/num/f32.rs:2181-2183`), and without the `fma`
   feature that becomes a call to `fmaf` (LLVM): a libm call per sample. Rust never fuses `a * b + c`
   by itself (no FP contraction), so that stays a `vmulps` and a `vaddps`.
   - The v2-era lesson: at our old `x86-64-v2` baseline sonora 0.2.0 had 162 `fmaf` call sites,
     about 36 µs of AEC3's 90 µs a block (the 2026-10-26 review's performance F1). We vendored it
     with `mul_add` written as `a * b + c` until the move to v3 made that moot (AEC3 now about 50 µs,
     `doc/dev/build-report.md` §11). A build without FMA brings the calls back, so `tests/aec3.rs`
     in nf-stages checks the build has it.
4. **A per-sample recurrence doesn't vectorise.** A value carried from sample to sample (`g += step`,
   `g = g * a + b`, an RNG's state) is a serial chain, and LLVM won't vectorise a float induction
   without reassociation (LLVM).
   - Derive linear ramps from the index, as `crossfade` does (`crates/nf-core/src/dsp/fade.rs:7-15`),
     and convert the index as `i32` (pitfall 9).
   - Turn one-pole smoothing into a table of powers (§3 R3), or make the ramp linear in a domain
     where it can come from the index. The gate now does the latter: its ramps are linear in dB,
     with `powf` at the block's ends only and linear amplitude interpolated from the index (R2).
   - Split a generator into independent lanes when only its statistics matter. `Dither` was a single
     xorshift32 stream: 793 ns a block, inlined at seven sites, with 6 round-trip and 10 scalar loops
     in the table. It now runs eight lanes over `as_chunks_mut::<8>`, which compile to packed
     `pslld`/`psrld`/`pxor`/`cvtdq2ps`/`mulps`/`addps` and take 150 ns
     (`crates/nf-core/src/rt.rs:41-101`, `doc/dev/build-report.md` §6).
5. **Bounds checks stay in loops that the compiler can't prove are in range.** A ring indexed per
   sample with `w & mask` keeps its check unless LLVM can prove that `mask < buf.len()` (LLVM).
   `DelayLine::process` doesn't index per sample. It writes the whole block, then reads the delayed
   block, each as at most two `copy_from_slice`s split at the wrap (`write_ring`/`read_ring`,
   `crates/nf-core/src/dsp/delay.rs:38-81`). The `min`s on the split points never bind, but they make
   every bound provable, so no panic path is compiled in. It takes 29 ns a block (build report §5).
   Slices of runtime length (device buffers, the slip resampler's 479-481) need re-slicing, `zip`,
   `chunks_exact` or `as_chunks` (§3 R1).
6. **`f32::max` isn't a bare `maxps`.** It ignores NaN (`maximum_number_nsz_f32`,
   `STD/core/src/num/f32.rs:1058-1060`). x86 `maxps` returns its second operand when either operand
   is NaN, so `f32::max` needs a NaN fix-up (compare and blend) unless LLVM can prove there's no NaN
   (LLVM). `if a > m { a } else { m }` is exactly `maxps`. `clamp` asserts `min <= max` on every call
   (`f32.rs:1664-1671`).
7. **Inlining causes spills.** Our DSP kernels are `#[inline]` and get inlined into
   `Chain::process_timed` (`crates/nf-core/src/chain.rs:120`), and from there into the engine's audio
   loop. Two consequences:
   - The tool shows each kernel's loops under the function it was inlined into, not under its own
     name.
   - The loop's constants can spill there. Fix that with `#[inline(never)]` (§3 R7).
8. **QPC can't time a single glue loop.** Our clock is QPC: about 100 ns a tick at the usual 10 MHz
   (MS; read `QueryPerformanceFrequency`). One 480-sample vector loop takes about 0.1 µs (estimate). Time
   batches of blocks, and compare builds alternately (§4).
9. **Convert an index to float as `i32`, not `usize`.** `(i + 1) as f32` with `i: usize` is a 64-bit
   integer conversion, and neither SSE4.2 nor AVX2 has a packed one (AVX-512DQ's `vcvtqq2ps` is the
   first). LLVM keeps the index as `u64` lanes and converts each on its own (`pextrq`/`movq`,
   `cvtsi2ss`, `insertps`). `(i as i32 + 1) as f32` is one packed `cvtdq2ps` per 4 samples at v2,
   per 8 at v3.
   - Checked 2026-10-26 by compiling a probe with this rustc at `-C target-cpu=x86-64-v2`: a gain
     ramp over a 480-sample block. Per 8 samples, the `usize` loop took 53 instructions with 8
     `cvtsi2ss`, the `i32` loop 20 with 2 `cvtdq2ps`. Rechecked 2026-10-28 at `x86-64-v3`: the
     `usize` loop still has one scalar `vcvtsi2ss` a sample, the `i32` loop one `vcvtdq2ps` per 8.
   - `u32` has no packed conversion at this baseline either. LLVM used `cvtdq2ps` for a `u32` index
     only because it could prove the value stays below 2^31. Random bits need a signed reinterpret:
     `Dither::apply` converts `x as i32` (`crates/nf-core/src/rt.rs:76-92`).
   - The tool can't flag it: the rest of the loop is packed, so `scalar` stays clear. Look for
     `cvtsi2ss` in the loop with `--detail`.
   - Our code converts as `i32` in `crossfade` (`crates/nf-core/src/dsp/fade.rs:11-12`),
     `FloorMix::process`'s ramp (`mix.rs:67-71`), the gate's ramps (`gate.rs`, `ramp_to_unity` and
     after it) and `SlipResampler::resample` (`slip.rs:161-162`).

---

## 1. Files

| File | What it is | Where |
|---|---|---|
| `tools/hot_loop_spills.py` | Ours, adapted from TMR's. The spill and review table for our crates' hot loops | this repo |
| `tools/test_hot_loop_spills.py` | Its tests: what it selects (real v0 symbols from the bin's asm), which asm it reads by default, and a synthetic asm end to end | this repo |
| `tools/check_cpu_gate_asm.py` | Not a hot-loop tool, but it reads the same post-LTO asm, of both exes, and imports this tool's demangler and `PANIC`: it fails if the code a CPU runs before the CPU gate has a VEX, BMI, LZCNT or MOVBE instruction (`crates/nf-app/src/cpu_check.rs`, build report §11) | this repo, with `tools/test_check_cpu_gate_asm.py` |
| `TMR/tools/hot_loop_spills.py` | The original. Selects symbols containing `3tmr<module>` | `MODULES` `:59`, `COLD` `:62`, `BUILD` `:78`, `ASM` glob `:81`, `main` check `:293-304`, no-hot-loop fail `:347-348` |
| `TMR/tools/bench_ab.py` | Runs several exes alternately and compares per-test speeds | §4 |
| `TMR/tools/asm_fingerprint.py` | A normalised hash of each function, to prove a refactor left the instructions alone | Not ported. It imports `hot_loop_spills` for its selection (`:64`) and has its own `WORKERS` list (`:89`). Porting it means changing that import to our `functions(path, hot)` |
| `TMR/tools/README.md` | How TMR uses them; the tools' limits | `:10-67`, `:69-84`, `:86-183` |
| `TMR/doc/simd_codegen_rules.md` | TMR's codegen rules. Rule 2: accumulators. Rule 5: benchmarking. Rule 6: hot loops | `:112`, `:232`, `:254-363`, checklist `:395` |

TMR's Rules 1, 3 and 4 (memset idiom, bandwidth-bound width, per-width macros) are about memory
tests. They barely apply to 1920-byte blocks that sit in L1.

---

## 2. Running `tools/hot_loop_spills.py`

```bash
cargo rustc --release -p nf-app --bin noise-filter -- --emit asm   # the bin's asm, post-LTO
python tools/hot_loop_spills.py                       # the newest bin asm under target/: the table, worst first
python tools/hot_loop_spills.py --detail process_timed --all   # one function's loops, clean ones too
python tools/hot_loop_spills.py --functions           # what HOT / COLD select, and each one's hot-loop count
python tools/hot_loop_spills.py --paths nnnoiseless   # a dependency's own functions instead of ours
python tools/hot_loop_spills.py --paths sonora        # sonora's loops, with any libm calls listed
python tools/hot_loop_spills.py --max 0               # exit 1 if any hot loop touches the stack (ratchet)
python tools/hot_loop_spills.py path/to/file.s        # a saved asm
python tools/test_hot_loop_spills.py                  # the tool's own tests (no build needed)
```

Run it from any directory: paths are resolved from the script's folder (`:92`). The tool itself
writes nothing. The project rules allow `cargo build`, and so this `cargo rustc`. Only the owner runs
the exe.

### Where the asm lands
- Cargo on this nightly uses its new build-dir layout, `target/release/build/<package>/<hash>/out/`.
  We saw TMR's bin asm at `TMR/target/release/build/test-memory-r/<hash>/out/tmr.s`, next to `tmr.exe`.
  So `--emit asm` adds to Cargo's own emits, and the exe is still linked.
- Ours is `target/release/build/nf-app/<hash>/out/noise_filter.s`: the crate name is the bin name
  with `-` turned into `_`. With `CARGO_TARGET_DIR=target/NAME` it is
  `target/NAME/release/build/nf-app/<hash>/out/noise_filter.s` (seen under `target/review-perf/` and
  `target/agent-app/`).
- **The default asm** is the newest one that has a `main`, in any of those dirs, or in the old
  layout's `deps/noise_filter-*.s` (`BIN_ASM`, `:155-156`). The name under `build/nf-app/` doesn't
  matter. An asm there with no `main`, such as a lib target's, is passed over with a note. The libs'
  `build/nf-*/…` asm (`LIB_ASM`, `:159-160`) is never read: it is only named, in a note, when it is
  newer than the asm read.
- `codegen-units = 1` (`Cargo.toml:47`) gives one `.s` file. TMR's was 225 MB and 10.2 M lines, and
  ours is 202 MB and 9.0 M lines. The tool streams the file and reads ours in about 2 s.
- Save the old build's asm, or its `--detail` output, before rebuilding: the next build overwrites it
  (`TMR/tools/README.md:25-26`).
- Release builds have line tables (`Cargo.toml:49`). The tool skips their `.cv_loc` and `.Ltmp` lines.

### What it selects (`:94-120`, `is_hot` `:498-512`)
Only code that runs on the audio thread.
- **Own path.** A function is ours when its demangled own path is under a `HOT` prefix. The own path
  is the module an impl is written in, a closure's parent function, or a generic function itself,
  never the types it is instantiated with. The prefixes are `nf_core::{dsp, chain, rt, latency::stats,
  telemetry, noise, stage}`, `nf_stages` and `nf_engine`. The noise stage's per-block loops are
  `<nf_core::noise::NoiseStage as nf_core::stage::Stage>::process`'s, and a model's are its `process_hop`.
- **Exclusions.** A function under `COLD_PATHS` is left out:
  - `nf_core::stage::check_host_profile` (at activate, with its closures) and `nf_core::stage::state` (state
    save and load, never on the audio thread while it runs);
  - `nf_stages::{synth, bench, harness}`: test signals, the `bench` command, and the offline harness (the
    `process` and `quality` commands, the tests);
  - `nf_engine::{api, control, errors, devices}`: the caller's thread, the control thread and
    enumeration;
  - `nf_engine::latency_test`: the latency test's own thread. The tap it reads is fed by
    `audio.rs`, which stays selected;
  - `nf_gui` and `nf_app`, which aren't in `HOT` anyway: listing them keeps one of our generics out
    when it is instantiated with their closure (next bullet).

  So is a function whose last name is in `COLD`: `new`, `default`, `activate`, a model's `prepare`,
  `build_chain`, `reset`, a stage's `save_state` and `load_state`, `fmt`, `clone`, and so on.
- **Closures decide the thread.** A function that names a closure written under `COLD_PATHS` (in its
  generic arguments or self type) runs on that code's thread, so it is left out whatever its own path:
  the control thread's `__rust_begin_short_backtrace::<Inner::ensure_control_thread::{closure#0}, ()>`,
  or `nf_core::chain::run_slot::<live::tick::{closure#0}>` if the GUI ever instantiated one.
- **`--paths` inside a `COLD_PATHS` entry lifts it.** `--paths nf_stages::synth` reads the synth
  module; the other entries still apply (`cold_paths`, `:492-495`).
- **Std wrappers.** A std generic instantiated with one of our closures is selected too, because it
  can hold that closure's body. The audio thread's entry is one:
  `std::sys::backtrace::__rust_begin_short_backtrace::<audio::spawn::{closure#0}, ()>`, from
  `crates/nf-engine/src/audio.rs:96-97`.
- **Names are full demangled paths**, e.g. `nf_core::dsp::meter::Meter::measure` and
  `<nf_core::noise::NoiseStage as nf_core::stage::Stage>::process`. Generic arguments are shortened
  (`Box<dyn Stage>`). A closure in a generic argument keeps its function and the module or type
  before it (`audio::spawn::{closure#0}`, the GUI's `Saver::spawn::{closure#0}`). `--detail` matches
  a substring of this name; the table cuts names at 64 characters.

### The columns
- **round trip / reload, gpr / simd:** stack slots stored and reloaded each iteration, or only read
  (or only written). These are TMR's columns. Here, a read-modify-write such as `addq $1, 16(%rsp)`
  counts as a round trip.
- **panic:** the loop branches to a panic, or calls one: a bounds check, `unwrap` or division check
  left in the loop. Panic calls don't remove a loop from the table.
- **scalar:** float maths only in scalar form (`mulss`, `addss`, …), with no packed op: the loop
  didn't vectorise. That is right for a recurrence that can't be split; many can (pitfall 4). A
  vectorised slice loop's remainder loop shows here too. A packed loop with per-lane conversions
  (pitfall 9) doesn't.
- **indexed:** addressing like `(%rax,%rcx,4)`. TMR measured 6-16% for it on memory-bound kernels;
  in our L1-resident blocks, treat it as information only.
- **const:** a `(%rip)` load every iteration.
- **long:** more than 150 instructions.
- **Below the table:** loops that call something.
  - Not listed, only counted: `timing` (QPC, `Instant`), `device` (wasapi, waits, MMCSS), `dispatch`,
    and `struct move N B` (a `memcpy` of constant size, such as a whole `Block`).
  - Listed by function and callee: `rt` (allocation, lock, log, format or sleep, which breaks the
    real-time rules if the loop runs on the audio thread), `libm` (`powf`, `expf`, `fmaf`, …) and
    `read` (indirect calls such as `dyn Stage` or COM, a `memcpy` of computed size, a helper that
    didn't inline).
- **Notes (stderr, no failure):** a newer asm passed over for having no `main`, and newer libs'
  asm, by default (`:770-776`); source files changed after the asm was built (`:780-787`).
- **Failures (exit 1):** an asm named on the command line with no `main`; by default, no bin asm with
  a `main` (`:751-768`); no v0 (`_R`) symbols (`:801-802`); no hot loop found (`:849-850`); `--max N`
  exceeded. Rust 1.101 nightly mangles with v0 by default: TMR's asm and our debug rlibs both have
  `_R` symbols.

### How it differs from TMR's tool, and how it was checked
- **Selection.** TMR's tool matches raw symbol substrings (`TMR/tools/hot_loop_spills.py:167-188`),
  which also picks up std generics instantiated over its types. Ours demangles v0 (`:182-485`). On
  TMR's asm, the demangler parsed all 7020 function and static symbols with no failure, and all 269 of
  nf-core's debug rlib.
- **Frame pointers.** In a function that sets one (`.seh_setframe %rbp`), `(%rbp)` operands count as
  stack (`:811`). TMR's tool counts them as memory: its `OFF_STACK` matches any `%r…` base but `%rsp`
  (`TMR/…:86`), so `%rbp` and `%rip` operands counted too.
- **Memory accesses.** `lea` and `nop` don't count. A `%rip` operand doesn't make a loop hot-shaped.
  An array on the stack walked through an index register does (a scratch block on the stack).
- **Columns.** TMR's `width` column is gone: its kernels' names carry a width and ours don't. The
  `panic` and `scalar` columns are new.
- **Cross-check.** Pointed at TMR's asm with `--paths tmr::tests,…` (TMR's 13 modules), ours finds
  2198 hot loops where TMR's finds 2126. The difference comes from the selection and the fixes above.
  A synthetic asm exercised every column, every call kind and every failure.
- **On our bin's asm** (2026-10-26, `target/review-perf/…`), the table has 13 functions, all on the
  audio thread. Before the off-thread modules joined `COLD_PATHS` it had 24: the other 11 were
  `synth`, `bench`, `control`, `errors` and `latency_test` code, which held every gpr round trip and
  most of the `rt` and libm listings.

### Limits
TMR's own limits (`TMR/tools/README.md:37-48`) still hold:
- It goes by shape, so cold set-up inlined into a hot function can show up. Read a loop before acting
  on it.
- A value reloaded through a pointer register (closure captures pinned in memory, Rule 6.10) looks
  clean.
- If a loop's skip path forms a smaller backward branch, only that path is classified.

Ours adds:
- `scalar` can't tell a recurrence from a missed vectorisation.
- A packed loop that converts lane by lane (`usize` to `f32`, pitfall 9) isn't flagged.
- `COLD` goes by name, so a cold helper with another name shows up until you add it.
- Set-up that runs on the audio thread (opening streams, the start-up warnings in `audio::run`) is
  selected: it is audio-thread code, just not per block.
- It can't see denormal stalls. The engine doesn't set FTZ/DAZ (a persistent MXCSR change is UB in
  Rust; `enable_flush_denormals`, `crates/nf-core/src/rt.rs:3-28`, is kept for experiments only). It
  dithers the mic and AEC3's reference instead (`Dither`, about -180 dBFS).

### Keeping it in step with the code
- An audio-thread module in a new crate or path: add its prefix to `HOT` (`:99-100`).
- An off-thread module inside `HOT`, or a new crate that calls into it from its own thread: add it to
  `COLD_PATHS` (`:104-113`), then check `--functions`.
- A cold helper that has a loop: add it to `COLD` (`:119-120`).
- New device or timing calls: add them to `COLD_CALLS` (`:126-131`).
- If `nf-app` gains a lib target, its asm lands under `build/nf-app/` too. Having no `main`, it is
  passed over with a note.
- After changing the selection, run `python tools/test_hot_loop_spills.py`.

---

## 3. Codegen rules for small DSP loops

**Budget.** Measured on the dev VM at x86-64-v3 (`doc/dev/build-report.md` §11, p50 per block):
RNNoise 36.1 µs, AEC3 50.4 µs, the whole chain 84.6 µs (at x86-64-v2: 39.6, 54.4 and 97.0, and
AEC3 90.8 before sonora's `fmaf` calls were patched out, pitfall 3). The glue kernels take 3-226 ns
each (4-306 at v2). At AVX2 width, a glue loop over 480 samples is about 60 vector iterations, well
under 1 µs. So the goal for the base isn't hand-tuning. It is:
- every per-sample loop vectorised, or knowingly scalar;
- no stack traffic;
- no calls;
- no bounds checks.

Beyond that, measure before changing anything. TMR Rule 6.12 (`TMR/doc/simd_codegen_rules.md:316-321`):
clean asm is necessary, not sufficient.

**What the build gives us:**
- **Baseline `x86-64-v3`** (`.cargo/config.toml:1-12`, since 2026-10-28; x86-64-v2 before): AVX2,
  FMA, BMI1/2, F16C, LZCNT and MOVBE, and 16 `ymm` registers of 8 × f32. The exes check the CPU at
  start-up (`crates/nf-app/src/cpu_check.rs`).
  - Expect the VEX forms, mostly on `ymm`: `vmovups`/`vmovaps`, `vmulps`, `vaddps`, `vmaxps`,
    `vandps` (abs, with a mask constant), `vcvtdq2ps` (an `i32` index to float, pitfall 9),
    `vshufps`/`vunpcklps`/`vperm…` (interleave), `vsqrtps`, and `vfmadd…` only where the source says
    `mul_add` (pitfall 3). Scalar forms end in `ss`. Every SSE instruction is VEX-encoded too.
  - `is_x86_feature_detected!` is a constant `true` for v3's features; for one above them (AVX-512,
    say) it is a real run-time check (cached, but a load and a branch). Keep it out of loops, and
    dispatch once, in `activate`.
  - A `#[target_feature]` function's features don't reach a helper it calls that isn't inlined (TMR
    Rule 4, `:218-230`; Rule 6.2, `:265-269`). Our glue loops don't need more than v3.
- **Release profile** (`Cargo.toml:45-49`): `opt-level = 3`, fat LTO, `codegen-units = 1`,
  `panic = "abort"`. Under abort a panic still costs a compare and branch, and still blocks
  vectorisation.
- **Dev profile:** dependencies are built at `opt-level = 3` (`Cargo.toml:52-53`), but our own crates
  aren't. Never judge codegen from a debug build.
- **Win64 ABI (MS):** `xmm6`-`xmm15` are callee-saved, so a call inside a loop forces saves and
  restores around it. A struct over 8 bytes is passed as a pointer to a copy (TMR Rule 6.16, `:340-342`).

### R1. Bounds: give the compiler lengths it can see
- **Fixed-size blocks.** `&Block` and `&mut Block` are `[f32; 480]` (`crates/nf-core/src/lib.rs:25`).
  With `for i in 0..BLOCK` or `zip` there's no bounds check (LLVM).
- **Slices.** Re-slice to one length before the loop, then `zip`:
  ```rust
  fn add_into(dst: &mut [f32], src: &[f32]) {
      let n = dst.len().min(src.len());
      let (dst, src) = (&mut dst[..n], &src[..n]); // one check, before the loop
      for (d, s) in dst.iter_mut().zip(src) {
          *d += *s;
      }
  }
  ```
- **Fixed-size groups.**
  - `chunks_exact(N)` (stable 1.31, `STD/core/src/slice/mod.rs:1239-1243`) gives chunks of known
    length plus `.remainder()`.
  - `as_chunks::<N>()` (stable 1.88.0, `:1394-1399`) gives `(&[[T; N]], &[T])`: arrays, so the type
    carries the length.
  - `chunks(N)` gives chunks of unknown length: don't use it in hot code.
- **Ring buffers.** Indexing with `w & mask` is checked unless the slice's length is known to be
  `mask + 1`. Two fixes:
  - Before the loop, take `let buf = &mut self.buf[..mask + 1];`. LLVM should then prove the index is
    in range (LLVM; check the `panic` column).
  - Or do one `copy_from_slice` per contiguous segment: write the block (at most two segments), then
    read the delayed block (at most two). `DelayLine` does this (pitfall 5). Its ring holds at least
    `capacity + BLOCK + 1` samples (`delay.rs:8`, `:20`), so writing the whole block before reading
    gives the same output as interleaving the two per sample; a test checks it against the old
    per-sample ring.

### R2. Ramps come from the index, not from accumulation
This applies to per-sample gain ramps, the floor mix's `lim` ramp (`crates/nf-core/src/dsp/mix.rs:7-8`)
and crossfades. `t = (i as i32 + 1) as f32 * step` vectorises (`cvtdq2ps` on an index vector). Leave
out the `as i32` and the index converts lane by lane (pitfall 9). `t += step` is a loop-carried float
add: serial, and it drifts.
```rust
/// Gain ramping linearly from `g0` (the last block's end) to `g1` across the block.
#[inline]
pub fn gain_ramp(buf: &mut Block, g0: f32, g1: f32) {
    if g0 == g1 {
        if g0 != 1.0 {
            buf.iter_mut().for_each(|s| *s *= g0);
        }
        return;
    }
    let step = (g1 - g0) * (1.0 / BLOCK as f32);
    for (i, s) in buf.iter_mut().enumerate() {
        *s *= g0 + (i as i32 + 1) as f32 * step; // the last sample is g1, to rounding
    }
}

/// Floor mix: wet = lim*dry + (1-lim)*wet, with lim ramping from lim0 to lim1.
#[inline]
pub fn floor_mix(dry: &Block, wet: &mut Block, lim0: f32, lim1: f32) {
    let step = (lim1 - lim0) * (1.0 / BLOCK as f32);
    for (i, (w, &d)) in wet.iter_mut().zip(dry).enumerate() {
        let lim = lim0 + (i as i32 + 1) as f32 * step;
        *w = lim * d + (1.0 - lim) * *w; // the contract's form (mix.rs:4)
    }
}
```
`*w += lim * (d - *w)` is the same mix in fewer operations, but its rounding differs. Without
reassociation the two forms aren't bit-identical, so a test that checks "the exact mix" (`mix.rs:9-10`)
must use the form the code uses.

### R3. One-pole smoothing: make the recurrence a table
(The gate used this until 2026-10-26; its ramps are now linear in dB and use R2. The pattern stands
for any one-pole that has to stay one.) A one-pole ramp has `a = exp(-1 / ms_to_samples(t))`. Per sample, `g = target + (g - target) * a` is a serial
mul-add chain: about 480 × (mul + add latency) ≈ 4k cycles ≈ 1 µs (estimate), and flagged `scalar`.
The gate decides open or closed once a block, so `target` is constant within the block, and the
closed form is the same thing up to rounding: `g[i] = target + (g0 - target) * a^(i+1)`. Compute the
powers when attack or release changes, not per block, and the loop vectorises:
```rust
/// a^(i+1) for i in 0..BLOCK. Rebuild it when the time constant changes. It can live inline in
/// the gate (1920 B), so it needs no allocation.
pub fn pow_table(a: f32, t: &mut Block) {
    let mut p = 1.0f64; // f64 so the 480th power doesn't drift
    for v in t.iter_mut() {
        p *= a as f64;
        *v = p as f32;
    }
}

/// Applies the one-pole gain from `g0` towards `target` and returns the gain at the block's end.
#[inline]
pub fn one_pole_gain(buf: &mut Block, g0: f32, target: f32, pow: &Block) -> f32 {
    let d = g0 - target;
    for (s, p) in buf.iter_mut().zip(pow) {
        *s *= target + d * p;
    }
    target + d * pow[BLOCK - 1]
}
```
The options were the table above, a linear ramp to the one-pole's end value computed once a block
(R2), or the scalar loop accepted as expected. The gate took the table: `OnePole` in
`crates/nf-core/src/dsp/gate.rs` holds the powers per time constant and rebuilds them only when
`attack_ms` or `release_ms` changes (the module docs, "How it does it"). The gate takes 81 ns a
block (build report §5).

### R4. Reductions (peak, RMS, sums) need independent lanes
- **The meter is the right shape.** `Meter::measure` (`crates/nf-core/src/dsp/meter.rs`) and
  `mean_square` beside it keep 8 independent lanes for the peak and for the sum of squares, over
  `as_chunks::<8>()`. Each lane is its own chain, so LLVM can pack them into vectors (one `ymm` at
  v3, two `xmm` at v2) without reassociating anything (LLVM). The final horizontal fold runs once a
  block. The tool shows `Meter::measure`'s loops packed, with no stack traffic.
- **Why it works:** TMR Rule 2 (`TMR/doc/simd_codegen_rules.md:112-201`) says to use N independent
  accumulators and merge them once. N is capped by the register budget: 16 vector registers, with
  about 13 usable (`:166-180`). Eight f32 lanes cost one `ymm` per reduction at v3. With 480 samples
  there are 60 iterations, so more chains wouldn't pay.
- **Peak with a compare instead of `f32::max`:** `f32::max` adds NaN fix-ups (pitfall 6). A NaN
  sample then never wins (the compare is false), which suits a meter.
  ```rust
  pub fn peak_and_sum_sq(x: &Block) -> (f32, f32) {
      let mut peak = [0.0f32; 8];
      let mut sum = [0.0f32; 8];
      let (chunks, _) = x.as_chunks::<8>(); // 480 = 60 x 8: no remainder
      for c in chunks {
          for l in 0..8 {
              let a = c[l].abs(); // andps with a sign mask
              peak[l] = if a > peak[l] { a } else { peak[l] }; // one maxps (LLVM)
              sum[l] += c[l] * c[l];
          }
      }
      (peak.iter().fold(0.0f32, |m, &v| if v > m { v } else { m }), sum.iter().sum())
  }
  ```
- **The other route:** `algebraic_add` and `algebraic_mul` (`STD/core/src/num/f32.rs:1835`, `:1857`) let
  LLVM reassociate a plain `fold` and vectorise it. Two costs:
  - their results are non-deterministic (`STD/core/src/primitive_docs.rs:1293-1312`), so no
    bit-exact tests on them;
  - they need `rust-version >= 1.98`, a bump that is the owner's call.

  Written-out lanes work on any version.

### R5. No libm, division or `mul_add` per sample
- **libm calls.** `db_to_lin` is `powf` and `lin_to_db` is `log10` (`crates/nf-core/src/lib.rs:28-38`);
  coefficients use `exp`. All are calls. Compute them once a block or on a parameter change. `sqrt`
  is an instruction. The tool lists any libm call in a hot-shaped loop.
- **Division.** `x / c` with a loop-invariant `c` stays a `divps`: strict FP may not swap it for
  `x * (1/c)` unless the result is exact (LLVM). Hoist `let inv = 1.0 / c;` out of the loop.
- **`mul_add`** is one `vfmadd…` at our v3 baseline, and a call to `fmaf` in a build without FMA
  (pitfall 3). `mul_add_relaxed` would allow an unfused result (`STD/core/src/num/f32.rs:1920-1922`),
  but it is unstable (`float_mul_add_relaxed`).
- **`clamp`** with run-time bounds puts an assert in the loop unless it is hoisted
  (`f32.rs:1664-1671`). Use compares, or `x.max(lo).min(hi)` where NaN handling doesn't matter.

### R6. No calls in a hot loop
Calls to look for:
- libm, and `fmaf` in a build without FMA;
- allocation and logging, which the real-time rules ban on the audio thread anyway (CLAUDE.md);
- `dyn` calls (once a block is fine; once a sample isn't);
- `is_x86_feature_detected!`;
- a closure passed as `&dyn Fn`, which also pins its captures in memory where the tool can't see
  them (TMR Rule 6.10, `:307-310`).

`Chain::process_timed` takes its clock as a generic `F: FnMut() -> u64` (`chain.rs:120`), so the
clock is monomorphised and inlined; that is the right shape. Calls once a block or once a packet
(QPC, WASAPI, `memcpy` of a `Block`) sit outside the per-sample loops, and the tool counts them under
the table without listing them.

### R7. Spills: give a loop its own function when inlining crowds it
- LLVM allocates registers per function (TMR Rule 6.1, `:260-264`). Inlined into
  `process_timed`, with the chain's state live, a kernel's constants can spill.
- If the tool shows stack traffic in a DSP loop:
  - move the kernel into an `#[inline(never)]` fn taking `&Block`s and a few scalars by value (not a
    struct over 8 bytes: Rule 6.16);
  - inside it, destructure `self`'s fields into locals before the loop;
  - re-read the asm.
- The call this adds costs nothing at once a block.
- Rewording the source rarely changes LLVM's choices: change the structure, then re-read
  (Rule 6.8, `:300-302`).

### R8. Interleave and deinterleave at the device edge
- **Sizes.** WASAPI buffers are bytes (`read_from_device(&self, data: &mut [u8])`;
  `doc/dev/api/wasapi.md:456`). The shared-mode format is usually f32.
- **Fixed strides.** Make the channel count a const generic and dispatch once a packet (TMR
  Rule 6.4, `:286-287`). A run-time stride gives a gather-shaped loop.
  ```rust
  /// Mono from C interleaved little-endian f32 channels. `out.len()` frames are read.
  fn downmix<const C: usize>(bytes: &[u8], out: &mut [f32]) {
      let (samples, _) = bytes.as_chunks::<4>();
      let (frames, _) = samples.as_chunks::<C>();
      let scale = 1.0 / C as f32;
      for (o, f) in out.iter_mut().zip(frames) {
          let mut acc = 0.0f32;
          for s in f {
              acc += f32::from_le_bytes(*s); // a plain 4-byte load on x86
          }
          *o = acc * scale;
      }
  }

  /// Mono to stereo, as little-endian f32 bytes.
  fn upmix2(src: &[f32], bytes: &mut [u8]) {
      let (samples, _) = bytes.as_chunks_mut::<4>();
      let (frames, _) = samples.as_chunks_mut::<2>();
      for (f, &x) in frames.iter_mut().zip(src) {
          let b = x.to_le_bytes();
          *f = [b, b];
      }
  }

  fn downmix_any(channels: usize, bytes: &[u8], out: &mut [f32]) {
      match channels {
          1 => downmix::<1>(bytes, out),
          2 => downmix::<2>(bytes, out),
          4 => downmix::<4>(bytes, out),
          6 => downmix::<6>(bytes, out),
          8 => downmix::<8>(bytes, out),
          _ => { /* a slow generic path, chunks_exact(channels * 4) */ }
      }
  }
  ```
- **Alignment.** The byte forms above need none. `align_to::<f32>()` is faster only when the buffer
  happens to be aligned, and the type doesn't promise that.
- **Float to int.** `as` casts saturate and send NaN to 0, which adds instructions. If a device ever
  needs an integer format, clamp first and then convert.
- **Where we are.** `CaptureStream::read_packet` shows 12 `scalar` loops, one per format and
  channel-count shape, each adding a frame's channels in order: a missed vectorisation of
  interleaved loads, under 0.5 µs a packet. It is left as a follow-up until a measurement says it
  matters (build report §6).

### R9. Crossfades and short tails
- `crossfade` (`fade.rs:7-15`) is already in the right form: index-derived `t` converted as `i32`,
  fixed-length arrays. It runs only in the block where a toggle switches (`run_slot`, `chain.rs`).
- The slip resampler's slices are 479-481 samples long (`crates/nf-core/src/dsp/slip.rs:5`), and
  `resample` walks each in runs that share an input offset. At v3 a run gets an 8-wide `ymm` body
  plus a scalar remainder of up to 7 samples, with no 4-wide step between them (checked 2026-10-28:
  `--detail SlipResampler::resample --all`). The `scalar` column flags the remainder. That's fine.
  TMR Rule 6.9 (`:303-306`) warns that short tails get masked. AVX2 has masked moves
  (`vmaskmovps`), but LLVM doesn't fold this tail into them, so expect a plain scalar tail (LLVM).

---

## 4. Measuring: `bench_ab.py`'s alternating A/B, applied to offline per-block timing

**What TMR's tool does** (`TMR/tools/bench_ab.py`):
- It runs two or more exes in turn, A B A B, for `--runs` rounds (`:108-112`).
- From each run's results JSON it takes each test's speed, averaged over cycles (`speeds_of`, `:40-49`).
- It prints the mean and min-max, the raw ratio, and the ratio relative to a reference test measured
  in the same run (`:126-140`), so the box's state cancels out.
- One exe under two labels (an A/A run) gives the noise floor: 1.0-1.6% on TMR's dev box
  (`TMR/tools/README.md:83-84`).
- Copy each exe out of `target/` first (`:80`), and build nothing while it runs.
- `--reparse` rebuilds the table from saved outputs.

**How it maps to us.** The per-block timing exists; the A/B script is still a proposal.
- **What is timed:** `noise-filter bench [IN.wav] [--seconds N] [--aec]` and
  `crates/nf-stages/tests/timing.rs` time RNNoise, AEC3 and the chain per block (p50/p99/max/mean,
  through `nf_stages::bench`). `crates/nf-core/tests/timing.rs` times the glue kernels in ns per block.
  Both tests are `#[ignore]`d; run them with `--release -- --ignored --nocapture` (build report §5).
  TODO #2's harness adds CPU per frame on WAV recordings.
- **Unit:** ns per block, per stage. Lower is better, so report B/A time. For the reference, use a
  stage the change doesn't touch, measured in the same run: RNNoise's time when tuning the meter, or
  a fixed synthetic kernel kept in the bench for that purpose.
- **Clock resolution:** batch the blocks. A single glue loop is below one QPC tick (pitfall 8), and
  `ChainReport`'s per-stage ticks (`chain.rs:30-32`) are only good for whole stages. Time, say, 1000
  blocks per sample. Report the median and min-max of the batch means, never best-of (TMR Rule 5.2,
  `:238`).
- **Dead-code elimination:** `std::hint::black_box` the input block and the output each iteration,
  or LLVM can delete the work (Rule 5.4, `:242-245`). Suspiciously round numbers mean DCE.
- **Across processes** (`bench_ab.py`'s way): two exes, each running all blocks, alternated. This
  catches layout and inlining effects from the real build.
- **Within one process:** run batches of A and B alternately in one run. That is cheaper for a
  micro-kernel, but both variants then sit in one binary with their own code layout, which can swing
  tight loops by 10-25% (Rule 5.5, `:246-248`). Confirm a win across processes.
- **Adapting the script:** replace `speeds_of` with a reader of the bench's JSON, for example
  `{"stages": {"<id>": {"p50_ns": …, "p99_ns": …}}}`. Invert the ratio, and run
  `exe bench <wav> --json <file>`. `bench` prints a table today and has no `--json`; the format is
  ours to define in TODO #2.
- **Who runs it:**
  - The owner runs the exe.
  - `cargo bench` needs asking first (CLAUDE.md, "Who runs what"). The alternative is timing frames
    inside an `#[ignore]`d test (`doc/todo/2-offline-harness.md:9-10`).
  - The dev box is a Windows Server 2025 VM over Remote Desktop: expect noise, and run an A/A first.
