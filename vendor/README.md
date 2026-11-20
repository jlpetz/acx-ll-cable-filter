# vendor/

A patched copy of one crates.io crate, wired in by `[patch.crates-io]` in the workspace `Cargo.toml` (TODO 10). It makes
sonora 0.3.0's FTZ/DAZ guard a cargo feature, off by default, so that either build is one flag.

| Directory | Upstream | Licence |
|---|---|---|
| `sonora-0.3.0/` | crates.io `sonora` 0.3.0 (SHA-256 `d461e6c9ad112f8728919387a4bc5639fd28631f2070171efad38676e907edd8`, the checksum `Cargo.lock` had), git `1632d47c` (`crates/sonora`) of github.com/dignifiedquire/sonora | BSD-3-Clause, `LICENSE` |

It is the published package as cargo unpacked it (the `.cargo-ok` marker removed), plus the patch below. `sonora-0.3.0.patch`
is the whole difference: applied with `patch -p1` to the unpacked `.crate`, it gives this directory byte for byte (checked
2026-11-20) as git stores it, with LF line endings; a checkout with `core.autocrlf` shows CRLF. The other six sonora crates
(`sonora-aec3`, `-agc2`, `-common-audio`, `-fft`, `-ns`, `-simd`) still come from crates.io: none of them reads or writes MXCSR
or FPCR. The guard is `sonora`'s `src/denormal_disabler.rs`, which `audio_processing_impl.rs` holds in each of its four entry
points (`process_stream`, `process_reverse_stream` and their `_i16` forms).

## What changed

Three files. Every change is marked "noise-filter vendor patch".

- `Cargo.toml`: a `denormal-guard` feature, off by default. `Cargo.toml.orig` is left as published.
- `src/denormal_disabler.rs`: the x86 and aarch64 `imp` modules require `feature = "denormal-guard"` in their `cfg`, and the
  no-op module takes the negation. Without the feature every target builds the no-op module, as a target without hardware
  flush-to-zero does upstream: `DenormalDisabler::new` reads and writes nothing, and `is_supported()` is false. sonora's own
  tests check against `is_supported()`, so they hold either way. The module doc says so.
- `src/audio_processing.rs`: a paragraph in `AudioProcessing`'s "Floating-point environment" doc.

With the feature on, the code is 0.3.0's as released.

Without the feature, building an `AudioProcessing` emits `tracing::info!("Denormal disabler unsupported")`, as on an
unsupported target. We install no `tracing` subscriber and don't enable `tracing`'s `log` feature, so it goes nowhere; and
it happens when AEC3 is built, off the audio thread.

## Turning the guard on

nf-stages' `sonora-denormal-guard` feature turns on `sonora/denormal-guard`; nf-app forwards it.

    cargo build --release --features sonora-denormal-guard            # the app (nf-app is the default member)
    cargo test --workspace --features nf-stages/sonora-denormal-guard

## Why

sonora 0.3.0 sets MXCSR's FTZ and DAZ for the length of each `process_*` call and restores the thread's word on return (its
`DenormalDisabler`, sonora #34). Rust documents a changed MXCSR around ordinary Rust code as undefined behaviour, even when
it is restored. Our engine dithers the mic and the reference at about -180 dBFS instead, which keeps AEC3's filters out of
the subnormal range by defined means, and sets no FTZ/DAZ of its own. The owner (2026-11-20) is agnostic: "if there is a
benefit in their implementation maybe we make it switchable". `doc/todo/10-aec.md` has the decision and
`doc/dev/build-report.md` §13 the measurements.

## Tests

- `crates/nf-stages/tests/mxcsr.rs` reads MXCSR inside each of sonora's four entry points (a recording global allocator
  reads it while a call re-initialises sonora for a new stream format) and after them. It expects FTZ and DAZ inside exactly
  when the feature is on, and the thread's word restored after either way. A copy with its expectation inverted fails in
  both builds (0x1f80 inside without the guard, 0x9fc0 with it).
- The workspace suite runs in both builds: `refactor_parity` (bit-identical to the golden) and `rt_no_alloc` (no allocation)
  pass in each.
- sonora's own tests pass both ways (2026-11-20: 211 unit tests, and 4 and 14 in its two test files, with and without
  `--features denormal-guard`, `--locked`, `-C target-cpu=x86-64-v3`). Cargo won't run them in place in a worktree nested
  inside the main checkout (it walks past this workspace's `exclude` to the outer one), so copy the directory out first.

## Dropping the patch

If sonora ships an opt-out (the draft request is in `doc/todo/10-aec.md`), or the owner wants the guard always on:

1. Remove `[patch.crates-io]` and `exclude = ["vendor"]` from the workspace `Cargo.toml`, and this directory.
2. Always on: remove the `sonora-denormal-guard` features (nf-stages, nf-app) and every `cfg!` of it, which rustc's
   `unexpected_cfgs` lint would otherwise flag (an error under clippy's `-D warnings`): `GUARD` in `tests/mxcsr.rs` becomes
   `true`, the guard line in `tests/timing.rs`' `preamble()` goes (or prints "on"), and nf-stages'
   `SONORA_DENORMAL_GUARD` (`lib.rs`) becomes `true` or goes with its reader, nf-engine's `tests/dither_matrix` (its JSON
   lines' `guard`; `tools/dither_matrix.sh` builds and checks both). Then the feature's mentions: the comments in the
   workspace `Cargo.toml`, `aec3.rs`, `bench.rs`, `tests/mxcsr.rs` and `tests/timing.rs`, `doc/design/architecture.md`,
   `doc/design/engine.md`, `doc/dev/api/sonora-nnnoiseless.md` 1.12 and TODO 10. An upstream opt-out: point nf-stages'
   feature at it, minding which way round upstream's flag is.
3. `cargo update -p sonora` puts the registry source and checksum back in `Cargo.lock`.
4. Run `tests/mxcsr.rs`, `tests/refactor_parity.rs`, `tests/rt_no_alloc.rs` and the timing test (`silence_costs`).
