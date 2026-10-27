# vendor/

Patched copies of two crates.io crates, wired in by `[patch.crates-io]` in the workspace
`Cargo.toml`. The patch is temporary. Delete this directory and those two entries once upstream
fixes the issue (sonora PR #37) in a release we can pin.

| Directory | Upstream | Licence |
|---|---|---|
| `sonora-0.2.0/` | crates.io `sonora` 0.2.0, git `a024d6ef` (`crates/sonora`) of github.com/dignifiedquire/sonora | BSD-3-Clause, `LICENSE` |
| `sonora-common-audio-0.2.0/` | crates.io `sonora-common-audio` 0.2.0, same commit (`crates/sonora-common-audio`) | BSD-3-Clause, `LICENSE` |

Both are the published packages as cargo unpacked them (the `.cargo-ok` marker removed). The
manifests are unchanged. The other sonora crates (`sonora-aec3`, `-agc2`, `-fft`, `-ns`, `-simd`)
still come from crates.io.

## What changed

There are two changed files. Every change is marked "noise-filter vendor patch".

- `sonora-0.2.0/src/three_band_filter_bank.rs`: `filter_core`, parts 1 and 3 (6 `mul_add`).
- `sonora-common-audio-0.2.0/src/cascaded_biquad_filter.rs`: `process`, `process_in_place` and
  `apply_biquad` (12 `mul_add`). One comment is reworded.

Each `x.mul_add(a, b)` becomes `madd(x, a, b)`, using a private helper in each file:

```rust
#[inline(always)]
fn madd(a: f32, b: f32, c: f32) -> f32 {
    #[cfg(target_feature = "fma")]
    { a.mul_add(b, c) }
    #[cfg(not(target_feature = "fma"))]
    { a * b + c }
}
```

The operation order is unchanged. FMA builds (x86-64-v3, or anything with `+fma`) still fuse and
give exactly what upstream gives. Builds without FMA compute an unfused `a * b + c`.

## Why

Our baseline is `target-cpu=x86-64-v2` (`.cargo/config.toml`), which has no FMA. There,
`f32::mul_add` lowers to a call to the C runtime's `fmaf` (`api-ms-win-crt-math-l1-1-0.dll`) for
every sample. The call blocks inlining and vectorisation of the 3-band split and merge, and of
every biquad (the high-pass and AEC3's decimator). The 2026-10-26 review (performance F1) measured
this at about 36 µs of AEC3's 90 µs per 10 ms block.

Measured on the dev VM with `crates/nf-stages/tests/timing.rs`
(`cargo test -p nf-stages --release --test timing -- --ignored --nocapture`). Each value is the
median of 3 runs, in µs per block. Other builds were running on the box, so the p99 figures are
noisy.

| Row | p50 before | p50 after | p99 before | p99 after |
|---|---|---|---|---|
| AEC3 (echo + near end + noise) | 90.3 | 54.5 | 116.1 | 82.4 |
| AEC3, silent reference (dithered) | 85.5 | 50.4 | 95.3 | 59.7 |
| chain: meters + AEC3 + RNNoise | 130.5 | 95.7 | 160.4 | 124.2 |
| RNNoise (unaffected) | 39.8 | 39.9 | 54.4 | 56.7 |

The timing test exe had 162 `fmaf` call sites before the patch and has 60 after. All 60 remaining
sites are in `sonora-fft`'s `fft4g`, which only sonora's noise suppressor (`sonora-ns`) calls. We
build AEC3 with `noise_suppression: None` (`crates/nf-stages/src/aec3.rs`), so that code never
runs. If the noise suppressor is ever switched on, vendor `sonora-fft` as well and patch
`fft4g.rs`, which has 36 `mul_add`.

To count the sites: list the exe's imports with `llvm-readobj --coff-imports` to find `fmaf`'s
IAT slot, find its thunk in the output of `llvm-objdump -d`, and count the calls to that thunk.
Both tools ship with the nightly toolchain under `lib/rustlib/x86_64-pc-windows-msvc/bin`.

## Effect on results

Without FMA, the results now differ from upstream's in the last bits, because a fused and an
unfused multiply-add round differently. AEC3's adaptation amplifies those differences. We ran
32 synthetic rooms, each with the mic signal scaled by 1 and by 1 ± 1e-7, and measured ERLE
from 3 s onwards. Both versions average 44.24 dB over the 96 runs. Individual rooms move by up
to 4.4 dB either way, but the 1e-7 scaling alone moves them by up to 5.7 dB within one version,
so the change is rounding noise rather than a bias. sonora's own 198 unit tests and sonora-common-audio's 51 pass with the patch, and so does every
nf-stages test, including AEC3's.

## Tests

`crates/nf-stages/tests/sonora_patch.rs` runs both filters against ports with the same operation
order and compares the output bit for bit. It expects unfused output without FMA and fused output
with it. If the patch is dropped while the baseline stays at v2, the test fails. That failure was
checked against the unpatched crates.

## Dropping the patch

When a sonora release includes the fix (sonora PR #37):

1. Bump the `sonora` pin in the workspace `Cargo.toml`, and the `sonora-common-audio`
   dev-dependency pin in `crates/nf-stages/Cargo.toml`.
2. Remove the two `[patch.crates-io]` entries and this directory.
3. Run `tests/sonora_patch.rs`. If upstream fixed it the same way, the test still passes. If it
   now fails, upstream chose differently (for example, never fusing), so update or delete the
   test.
4. Re-run the timing test.
