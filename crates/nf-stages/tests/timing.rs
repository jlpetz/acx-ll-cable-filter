//! Per-block cost of the stages and the chain: p50, p99 and max in µs over 30 s of audio, each
//! block timed alone. Ignored: a measurement, not a check. Run with
//! `cargo test -p nf-stages --release --test timing -- --ignored --nocapture`.
//! The code is `nf_stages::bench`, which `noise-filter bench` runs too. The dev box is a VM over
//! Remote Desktop: expect noise, and compare runs on the same machine.

use nf_stages::bench::{self, Signals};

const SECONDS: f32 = 30.0;

#[test]
#[ignore = "timing measurement; run with --release"]
fn per_block_costs() {
    if cfg!(debug_assertions) {
        eprintln!("debug build: our crates run unoptimised, so these numbers mean little (use --release)");
    }
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
