//! Refactor guard (ignored; kept for future refactors of the chain and its stages). It plays 20 s of scenes through
//! `build_chain`, with and without AEC3, while toggling the noise filter and the AEC, passthrough, the gate's modes, the floor,
//! the meters, NaN blocks and the repeated resets those toggles make, and records every output sample's bits, every block's
//! report and the chain's latency after each block.
//!
//! With `NF_GOLDEN_WRITE=1` it writes them to `$CARGO_TARGET_TMPDIR/golden`; otherwise it compares against what was written
//! there, bit for bit. Record on the tree before a refactor, compare after:
//! `NF_GOLDEN_WRITE=1 cargo test -p nf-stages --test refactor_parity -- --ignored`, then the same without the variable (with
//! the same `CARGO_TARGET_DIR`: the golden lives under it).
//!
//! It uses only APIs a stage refactor leaves alone: `build_chain`, `Chain::process` and its `ChainReport`, `Chain::latency`
//! and `ChainControls`. Nothing here may use the stage interface itself.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

use nf_core::chain::ChainReport;
use nf_core::config::{GateConfig, GateMode, ProcessingConfig};
use nf_core::controls::ChainControls;
use nf_core::dsp::meter::MeterReading;
use nf_core::rt::Dither;
use nf_core::{BLOCK, Block, db_to_lin};
use nf_stages::build_chain;
use nf_stages::synth::{Voice, convolve, delayed, pink_noise, room_impulse_response, scaled, speech_like};

const SR: usize = 48_000;
const LEN: usize = 20 * SR;
const BLOCKS: usize = LEN / BLOCK;
/// Words a block takes in the golden stream: the output, the noise report (flag + 2), two meters (flag + 4 each), the latency.
const STRIDE: usize = BLOCK + 3 + 5 + 5 + 1;

/// The mic and the reference: noisy near-end speech (0-5 s), echo only (5-10 s), double talk (10-15 s), impulses at block
/// phases 0, 17, 301 and 479 with a silent reference (15-17 s), then dithered silence; every block dithered as the engine
/// does, and NaN and infinite samples dotted through both.
fn scene() -> (Vec<f32>, Vec<f32>) {
    let s = |t: usize| t * SR;
    let mut near = speech_like(LEN, Voice::HIGH.at(-26.0), 41).samples;
    near[s(5)..s(10)].fill(0.0);
    near[s(15)..].fill(0.0);
    let mut far = speech_like(LEN, Voice::LOW, 42).samples;
    far[..s(5)].fill(0.0);
    far[s(15)..].fill(0.0);
    let mut noise = pink_noise(LEN, db_to_lin(-45.0), 43);
    noise[s(15)..].fill(0.0);
    let rir = room_impulse_response(9600, 0.15, 44);
    let echo = scaled(&delayed(&convolve(&far, &rir)[..LEN], 1920), 0.5);
    let mut mic: Vec<f32> = (0..LEN).map(|i| near[i] + echo[i] + noise[i]).collect();
    for (k, phase) in [0usize, 17, 301, 479].iter().cycle().take(40).enumerate() {
        mic[s(15) + k * 5 * BLOCK + phase] = 0.5;
    }
    let mut d = Dither::new(0xD17E);
    d.apply(&mut mic, 1e-9);
    d.apply(&mut far, 1e-9);
    for (b, (m, r)) in mic.as_chunks_mut::<BLOCK>().0.iter_mut().zip(far.as_chunks_mut::<BLOCK>().0).enumerate() {
        if b % 211 == 5 {
            m[3] = f32::NAN;
            m[100] = f32::NAN;
            m[200] = f32::INFINITY;
        }
        if b % 307 == 9 {
            r[7] = f32::NAN;
            r[50] = f32::NEG_INFINITY;
        }
    }
    (mic, far)
}

/// The settings at block `b`: on/off pulses (each switch-on and each end of passthrough resets the stage), the gate's three
/// modes with VAD on and off, floors from none to -200 dB, and the meters off now and then.
fn settings(b: usize) -> (ProcessingConfig, bool) {
    let r = |lo: usize, hi: usize| (lo..hi).contains(&b);
    let noise_enabled = !(r(300, 420) || r(900, 905) || r(1500, 1501) || r(1800, 1900));
    let aec_enabled = !(r(600, 700) || r(1250, 1400) || r(1600, 1602));
    let passthrough = r(400, 450) || r(1000, 1040) || r(1100, 1101) || r(1700, 1720) || r(1850, 1860);
    let gate = GateConfig {
        mode: [GateMode::Off, GateMode::Static, GateMode::Auto][b / 250 % 3],
        threshold_db: -45.0 - (b / 500) as f32 * 3.0,
        use_vad: b / 333 % 2 == 1,
        attack_ms: 2.0 + (b / 700) as f32,
        ..GateConfig::default()
    };
    let floor_db = [-60.0, -20.0, -6.0, -100.0, -200.0][b / 170 % 5];
    let cfg = ProcessingConfig { aec_enabled, noise_enabled, floor_db, gate, meters_enabled: b % 17 != 0, ..ProcessingConfig::default() };
    (cfg, passthrough)
}

fn meter_words(m: Option<MeterReading>, out: &mut Vec<u32>) {
    match m {
        Some(m) => out.extend([1, m.peak.to_bits(), m.mean_square.to_bits(), u32::from(m.clipped), u32::from(m.near_clip)]),
        None => out.extend([0; 5]),
    }
}

fn report_words(r: &ChainReport, latency: u32, out: &mut Vec<u32>) {
    match r.noise {
        Some(n) => out.extend([1, n.speech_probability.to_bits(), n.gain_reduction_db.to_bits()]),
        None => out.extend([0; 3]),
    }
    meter_words(r.input, out);
    meter_words(r.output, out);
    out.push(latency);
}

/// The golden stream of one chain over the scene.
fn run(with_aec: bool, mic: &[f32], far: &[f32]) -> Vec<u32> {
    let (cfg, _) = settings(0);
    let controls = Arc::new(ChainControls::new(&cfg));
    let mut chain = build_chain(controls.clone(), with_aec).expect("chain");
    let mut words = Vec::with_capacity(BLOCKS * STRIDE);
    let mut y: Block = [0.0; BLOCK];
    for (b, (m, r)) in mic.as_chunks::<BLOCK>().0.iter().zip(far.as_chunks::<BLOCK>().0).enumerate() {
        let (cfg, passthrough) = settings(b);
        controls.apply(&cfg);
        controls.passthrough.store(passthrough, Relaxed);
        let report = chain.process(m, r, &mut y);
        words.extend(y.iter().map(|v| v.to_bits()));
        report_words(&report, chain.latency(), &mut words);
    }
    words
}

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("golden").join(format!("{name}.bin"))
}

fn to_bytes(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

fn from_bytes(bytes: &[u8]) -> Vec<u32> {
    bytes.as_chunks::<4>().0.iter().map(|b| u32::from_le_bytes(*b)).collect()
}

/// What word `i` of the stream is, for the failure message.
fn describe(i: usize) -> String {
    let (b, k) = (i / STRIDE, i % STRIDE);
    let field = match k {
        k if k < BLOCK => format!("output sample {k}"),
        k => ["noise report flag", "speech probability", "gain reduction"]
            .into_iter()
            .chain(["input meter flag", "input peak", "input mean square", "input clipped", "input near clip"])
            .chain(["output meter flag", "output peak", "output mean square", "output clipped", "output near clip", "latency"])
            .nth(k - BLOCK)
            .unwrap_or("?")
            .to_owned(),
    };
    format!("block {b}, {field}")
}

#[test]
#[ignore = "refactor guard: record with NF_GOLDEN_WRITE=1 before a refactor, compare after"]
fn the_chain_is_bit_identical_to_the_recorded_golden() {
    let (mic, far) = scene();
    let write = std::env::var_os("NF_GOLDEN_WRITE").is_some_and(|v| v == "1");
    for with_aec in [false, true] {
        let name = if with_aec { "chain-aec3-rnnoise" } else { "chain-rnnoise" };
        let words = run(with_aec, &mic, &far);
        assert_eq!(words.len(), BLOCKS * STRIDE);
        let path = golden_path(name);
        if write {
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("golden dir");
            std::fs::write(&path, to_bytes(&words)).expect("write the golden");
            eprintln!("{name}: wrote {} ({} blocks)", path.display(), BLOCKS);
            continue;
        }
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("{}: {e}; record it first with NF_GOLDEN_WRITE=1 on the tree before the change", path.display()));
        let golden = from_bytes(&bytes);
        assert_eq!(golden.len(), words.len(), "{name}: the golden has another length");
        if let Some(i) = golden.iter().zip(&words).position(|(a, b)| a != b) {
            panic!("{name}: first difference at {} (golden {:#010x}, now {:#010x})", describe(i), golden[i], words[i]);
        }
        eprintln!("{name}: {} blocks bit-identical to {}", BLOCKS, path.display());
    }
}
