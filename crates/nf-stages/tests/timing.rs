//! Per-block cost of the stages and the chain: p50, p99 and max in µs over 30 s of audio, each
//! block timed alone. Ignored: a measurement, not a check. Run with
//! `cargo test -p nf-stages --release --test timing -- --ignored --nocapture`.
//! The code is `nf_stages::bench`, which `noise-filter bench` runs too. The dev box is a VM over
//! Remote Desktop: expect noise, and compare runs on the same machine.
//!
//! Each test prints whether sonora's FTZ/DAZ guard is built in (nf-stages' `sonora-denormal-guard`, TODO 10). To compare the
//! two, build both test exes (`--no-run`, once with `--features sonora-denormal-guard`) and run them in turn with
//! `--ignored --nocapture --test-threads 1`, so the two tests don't run at once.

use nf_stages::bench::{self, Signals};

const SECONDS: f32 = 30.0;

/// Says what the build is.
fn preamble() {
    if cfg!(debug_assertions) {
        eprintln!("debug build: our crates run unoptimised, so these numbers mean little (use --release)");
    }
    eprintln!("sonora's FTZ/DAZ guard: {}", if cfg!(feature = "sonora-denormal-guard") { "on" } else { "off" });
}

#[test]
#[ignore = "timing measurement; run with --release"]
fn per_block_costs() {
    preamble();
    let sig = Signals::synthetic(SECONDS);
    let rows = [
        bench::rnnoise(&sig, None).expect("RNNoise"),
        bench::rnnoise_model(&sig, None).expect("RNNoise model"),
        bench::noise_post(&sig, None),
        bench::aec3(&sig, None).expect("AEC3"),
        bench::aec3_silent_reference(&sig, None).expect("AEC3"),
        bench::sonora_undithered(&sig, None),
        bench::chain(&sig, true, None).expect("chain"),
        bench::chain(&sig, false, None).expect("chain"),
    ];
    for row in &rows {
        eprintln!("{}", row.line());
    }
}

/// Sustained silence after 3 s of the scene (TODO 10: what sonora's FTZ/DAZ guard buys over our dither): AEC3 and the chain
/// with the engine's dither on the mic and the reference, the stage with only its own dither on the reference, and, as the
/// control, sonora with an undithered zero reference (`per_block_costs` has it too).
#[test]
#[ignore = "timing measurement; run with --release"]
fn silence_costs() {
    preamble();
    let sig = Signals::synthetic(SECONDS);
    let rows = [
        bench::aec3_silence(&sig, true, None).expect("AEC3"),
        bench::chain_silence(&sig, None).expect("chain"),
        bench::aec3_silence(&sig, false, None).expect("AEC3"),
        bench::sonora_undithered(&sig, None),
    ];
    for row in &rows {
        eprintln!("{}", row.line());
    }
}
