//! Per-block cost of the audio-thread kernels, timed in batches (doc/dev/api/hot-loop-tools.md §4:
//! one 480-sample loop is below a QPC tick). Ignored: a measurement, not a check. Run with
//! `cargo test -p nf-core --release --test timing -- --ignored --nocapture`.

use std::hint::black_box;
use std::time::Instant;

use nf_core::config::{GateConfig, GateMode, ProcessingConfig};
use nf_core::controls::ChainControls;
use nf_core::dsp::delay::DelayLine;
use nf_core::dsp::drift::DriftController;
use nf_core::dsp::fade::crossfade;
use nf_core::dsp::gate::Gate;
use nf_core::dsp::meter::Meter;
use nf_core::dsp::mix::FloorMix;
use nf_core::dsp::noise_post::NoisePost;
use nf_core::dsp::slip::SlipResampler;
use nf_core::rt::Dither;
use nf_core::{BLOCK, Block};

const BATCH: usize = 2000;
const BATCHES: usize = 15;

/// Median and range of the per-block time over the batches, in ns.
fn time(name: &str, mut f: impl FnMut(usize)) {
    for i in 0..BATCH {
        f(i); // warm-up
    }
    let mut per_block: Vec<f64> = (0..BATCHES)
        .map(|_| {
            let t = Instant::now();
            for i in 0..BATCH {
                f(i);
            }
            t.elapsed().as_nanos() as f64 / BATCH as f64
        })
        .collect();
    per_block.sort_by(f64::total_cmp);
    eprintln!("{name:<28} {:>8.0} ns/block (min {:.0}, max {:.0})", per_block[BATCHES / 2], per_block[0], per_block[BATCHES - 1]);
}

fn tone(k: usize) -> Block {
    std::array::from_fn(|i| 0.2 * ((k * 31 + i) as f32 * 0.03).sin())
}

#[test]
#[ignore = "timing measurement"]
fn per_block_costs() {
    let x = tone(0);
    let y = tone(1);

    let mut meter = Meter::new();
    time("Meter::measure", |_| {
        black_box(meter.measure(black_box(&x)));
    });

    let mut line = DelayLine::new(4096, 480);
    let mut out = [0.0f32; BLOCK];
    time("DelayLine::process", |_| {
        line.process(black_box(&x), &mut out);
        black_box(&out);
    });

    time("crossfade", |_| {
        crossfade(black_box(&x), black_box(&y), &mut out);
        black_box(&out);
    });

    let mut mix = FloorMix::new();
    time("FloorMix (steady)", |_| {
        let mut w = y;
        mix.process(black_box(&x), &mut w, -20.0);
        black_box(&w);
    });
    time("FloorMix (ramping)", |i| {
        let mut w = y;
        mix.process(black_box(&x), &mut w, if i % 2 == 0 { -20.0 } else { -30.0 });
        black_box(&w);
    });

    let controls = ChainControls::new(&ProcessingConfig {
        gate: GateConfig { mode: GateMode::Static, threshold_db: -30.0, ..GateConfig::default() },
        ..ProcessingConfig::default()
    });
    let mut gate = Gate::new();
    time("Gate (alternating)", |i| {
        let mut b = if (i / 7) % 2 == 0 { x } else { x.map(|v| v * 0.001) };
        black_box(gate.process(&mut b, &controls.noise.gate.load(), f32::NAN));
        black_box(&b);
    });

    let mut post = NoisePost::new(480);
    time("NoisePost::process", |_| {
        let mut w = y;
        black_box(post.process(black_box(&x), &mut w, &controls.noise, 0.7));
        black_box(&w);
    });

    let mut slip = SlipResampler::new();
    let long = [0.1f32; BLOCK + 1];
    time("SlipResampler 481 -> 480", |_| {
        slip.resample(black_box(&long), &mut out);
        black_box(&out);
    });
    let mut out481 = [0.0f32; BLOCK + 1];
    time("SlipResampler 480 -> 481", |_| {
        slip.resample(black_box(&x), &mut out481);
        black_box(&out481);
    });

    // The engine dithers the mic and the reference, and AEC3 its reference: three a block.
    let mut dither = Dither::new(1);
    time("Dither::apply", |_| {
        let mut b = x;
        dither.apply(&mut b, 1e-9);
        black_box(&b);
    });

    let mut drift = DriftController::new(960.0);
    time("DriftController::update", |i| {
        black_box(drift.update(black_box(960.0 + (i % 64) as f32)));
    });
}
