//! Quality metrics through ONNX Runtime (TODO 14, the Lab's phase 0): DNSMOS P.835 and SIGMOS, both no-reference MOS
//! predictors, run offline by `noise-filter lab` on whole clips. Not real-time code: everything here allocates freely, and a
//! run takes a whole clip.
//!
//! - **Packs:** each metric is a pack of role `metric` (`packs/dnsmos`, `packs/sigmos`; fetched by `tools/fetch_models.py`
//!   into the models cache, as the noise models are). They share the noise packs' file checks (sizes, SHA-256, safe paths,
//!   no external data) but not their framing: [`check_metric`] takes a `metric` graph with a named float32 input and output,
//!   and the manifest's `metric.kind` names the runner it is for. A metric pack is never a noise model (`Pack::load`
//!   refuses role `metric`), and a noise pack is never a metric.
//! - **[`Dnsmos`]** (DNSMOS P.835, `sig_bak_ovr.onnx`, as `DNSMOS/dnsmos_local.py` at DNS-Challenge 591184a runs it): the
//!   clip at 16 kHz ([`resample::decimate3`] from 48 kHz, as the script's librosa and resampy do it), repeated until it's at
//!   least 9.01 s, cut into 9.01 s windows a second apart; each window's raw SIG, BAK and OVRL through the script's fixed polynomials; the clip's score is their
//!   mean. Deaf above 8 kHz.
//! - **[`Sigmos`]** (SIGMOS v1, ITU-T P.804's seven dimensions, as `ICASSP2024/sigmos/sigmos.py` at SIG-Challenge bf45251
//!   runs it): the whole clip at 48 kHz through a 960-point STFT (hop 480, a square-root periodic Hann, 480 zeros in front and
//!   up to 960 behind), each bin's magnitude compressed to the power 0.3 with its phase kept, as [magnitude, real, imaginary]
//!   planes; one run gives COL, DISC, LOUD, NOISE, REVERB, SIG and OVRL. Upstream calls it an alpha, with no published
//!   accuracy.
//!
//! The reference scripts aren't run here or anywhere in the project (downloads are untrusted data): their feature code is
//! ported, and the tests pin what the ported runners give.

use ort::logging::LogLevel;
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::{Tensor, TensorElementType, ValueType};

use crate::pack::{Manifest, Pack, PackError, Verify, installed_dir};

/// A metric pack's checks, after every pack's (`pack::check_files`): role and graph `metric`, one channel at 16 or 48 kHz, a
/// float32 input and output with fixed or dynamic (-1) dimensions, and the `metric` section that names its runner.
pub fn check_metric(m: &Manifest) -> Result<(), String> {
    if m.role.kind != "metric" {
        return Err(format!("role {:?}: a quality metric's pack has role metric", m.role.kind));
    }
    if m.graph.kind != "metric" {
        return Err(format!("graph kind {:?}: a quality metric's pack has graph kind metric", m.graph.kind));
    }
    if ![16_000, 48_000].contains(&m.signal.sample_rate) || m.signal.channels != 1 {
        return Err(format!("{} Hz, {} channels: a metric runs on one channel at 16 or 48 kHz", m.signal.sample_rate, m.signal.channels));
    }
    for (what, t) in [("input", &m.io.input), ("output", &m.io.output)] {
        if t.dtype != "float32" || t.name.is_empty() || t.shape.is_empty() || t.shape.iter().any(|&d| d == 0 || d < -1) {
            return Err(format!(
                "io.{what}: {:?} {:?} {}: a named float32 tensor, each dimension positive or -1",
                t.name, t.shape, t.dtype
            ));
        }
    }
    match &m.metric {
        Some(s) if !s.kind.is_empty() => Ok(()),
        _ => Err("a metric pack names its metric (metric.kind)".into()),
    }
}

/// The installed metric pack `id`, checked, its model verified, and its `metric.kind` the runner's `kind`.
pub fn load(id: &str, kind: &str) -> Result<Pack, PackError> {
    let pack = Pack::load_kind(&installed_dir(id)?, Verify::Runtime, check_metric)?;
    let found = pack.manifest.metric.as_ref().map_or("", |s| s.kind.as_str());
    if found != kind {
        return Err(PackError(format!("pack {id} is the metric {found:?}, not {kind:?}")));
    }
    Ok(pack)
}

/// A metric pack's session: one input and the named output, shapes per run.
pub struct MetricSession {
    session: Session,
    /// The manifest's input shape (-1: any length).
    input_shape: Vec<i64>,
    output: usize,
    output_len: usize,
}

/// A graph dimension against the manifest's: equal, or both dynamic.
fn same_dims(graph: &[i64], manifest: &[i64]) -> bool {
    graph.len() == manifest.len() && graph.iter().zip(manifest).all(|(&g, &m)| g == m || (g < 0 && m < 0))
}

impl MetricSession {
    /// Builds the session from the pack's verified bytes (one thread: the Lab runs clips on several workers, each with its
    /// own sessions) and checks the graph's one input and the named output against the manifest.
    pub fn new(pack: &Pack) -> Result<Self, String> {
        let m = &pack.manifest;
        let id = &m.id;
        let e = |e: ort::Error| format!("{id}: ONNX Runtime: {e}");
        let env = crate::env::env()?;
        let mut builder = Session::builder(env)
            .map_err(e)?
            .with_intra_threads(1)
            .map_err(|x| e(x.into()))?
            .with_inter_threads(1)
            .map_err(|x| e(x.into()))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|x| e(x.into()))?
            .with_deterministic_compute(true)
            .map_err(|x| e(x.into()))?
            .with_no_environment_execution_providers()
            .map_err(|x| e(x.into()))?
            .with_log_level(LogLevel::Error)
            .map_err(|x| e(x.into()))?;
        crate::onnx_check::refuse_external_data(&pack.model_bytes).map_err(|why| format!("{id}: {why}"))?;
        let session = builder.commit_from_memory(&pack.model_bytes).map_err(e)?;
        let tensor = |what: &str, dtype: &ValueType, want: &[i64]| match dtype {
            ValueType::Tensor { ty: TensorElementType::Float32, shape, .. } if same_dims(shape, want) => Ok(()),
            other => Err(format!("{id}: the graph's {what} is {other}, the manifest says float32 {want:?}")),
        };
        let [input] = session.inputs() else {
            return Err(format!("{id}: the graph has {} inputs, the manifest one", session.inputs().len()));
        };
        if input.name() != m.io.input.name {
            return Err(format!("{id}: the graph's input is {:?}, the manifest says {:?}", input.name(), m.io.input.name));
        }
        tensor("input", input.dtype(), &m.io.input.shape)?;
        let output = session
            .outputs()
            .iter()
            .position(|o| o.name() == m.io.output.name)
            .ok_or_else(|| format!("{id}: the graph has no output named {:?}", m.io.output.name))?;
        tensor("output", session.outputs()[output].dtype(), &m.io.output.shape)?;
        let output_len = m.io.output.shape.iter().map(|&d| d.max(1) as usize).product();
        Ok(Self { session, input_shape: m.io.input.shape.clone(), output, output_len })
    }

    /// One run on `data` shaped `shape` (the manifest's shape with its -1 dimensions filled in): the named output, flat.
    pub fn run(&mut self, data: Vec<f32>, shape: &[usize]) -> Result<Vec<f32>, String> {
        let fits = shape.len() == self.input_shape.len() && shape.iter().zip(&self.input_shape).all(|(&s, &m)| m < 0 || s as i64 == m);
        if !fits || shape.iter().product::<usize>() != data.len() {
            return Err(format!("an input of {} values shaped {shape:?}, the graph takes {:?}", data.len(), self.input_shape));
        }
        let tensor = Tensor::from_array((shape.to_vec(), data.into_boxed_slice())).map_err(|e| e.to_string())?;
        let outputs = self.session.run(ort::inputs![&tensor]).map_err(|e| e.to_string())?;
        let (_, y) = outputs[self.output].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
        if y.len() != self.output_len {
            return Err(format!("the output has {} values, the manifest {}", y.len(), self.output_len));
        }
        Ok(y.to_vec())
    }
}

/// DNSMOS P.835's clip score: the mean over its windows of the calibrated SIG, BAK and OVRL (and of the raw ones).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DnsmosScore {
    pub sig: f64,
    pub bak: f64,
    pub ovrl: f64,
    pub sig_raw: f64,
    pub bak_raw: f64,
    pub ovrl_raw: f64,
    /// The 9.01 s windows scored.
    pub windows: usize,
}

/// DNSMOS P.835 (see the module notes).
pub struct Dnsmos {
    session: MetricSession,
}

impl Dnsmos {
    /// The pack's id, and the metric its manifest must name.
    pub const ID: &str = "dnsmos";
    pub const KIND: &str = "dnsmos-p835";
    /// dnsmos_local.py's rate and window: 9.01 s, a window every second.
    pub const RATE: usize = 16_000;
    pub const INPUT_LENGTH_S: f64 = 9.01;

    pub fn new() -> Result<Self, String> {
        let pack = load(Self::ID, Self::KIND).map_err(|e| e.0)?;
        Ok(Self { session: MetricSession::new(&pack)? })
    }

    /// The window's length in samples: `int(9.01 * 16000)`, as the script computes it (144160).
    pub fn window_len() -> usize {
        (Self::INPUT_LENGTH_S * Self::RATE as f64) as usize
    }

    /// Where the script's windows lie in a clip of `len` samples at 16 kHz (after it has been repeated to 9.01 s or more):
    /// `int(floor(len / fs) - 9.01) + 1` windows, window `k` from `int(k * fs)` to `int((k + 9.01) * fs)`, each the
    /// window's length (any shorter one is skipped), in f64 as Python computes them. So the script's own quirk is kept: for
    /// k from 7 to 23 (and some from 119 on), `(k + 9.01) * 16000` falls just under a whole number in floating point, the
    /// window comes out a sample short and is skipped (a 45 s clip scores 19 of its 36 windows); clips of 16 s or less lose
    /// none.
    pub fn windows(len: usize) -> Vec<std::ops::Range<usize>> {
        let fs = Self::RATE as f64;
        let hops = ((len as f64 / fs).floor() - Self::INPUT_LENGTH_S) as i64 + 1;
        (0..hops.max(0) as usize)
            .map(|k| (k as f64 * fs) as usize..((k as f64 + Self::INPUT_LENGTH_S) * fs) as usize)
            .filter(|r| r.end <= len && r.len() >= Self::window_len())
            .collect()
    }

    /// The script's polynomials from a window's raw SIG, BAK and OVRL to the calibrated ones (the non-personalised set).
    pub fn calibrate(sig: f64, bak: f64, ovrl: f64) -> (f64, f64, f64) {
        let p = |c: [f64; 3], x: f64| (c[0] * x + c[1]) * x + c[2];
        (
            p([-0.08397278, 1.22083953, 0.0052439], sig),
            p([-0.13166888, 1.60915514, -0.39604546], bak),
            p([-0.06766283, 1.11546468, 0.04602535], ovrl),
        )
    }

    /// Scores a clip at 48 kHz: resampled to 16 kHz first ([`resample::decimate3`]).
    pub fn score_48k(&mut self, x: &[f32]) -> Result<DnsmosScore, String> {
        self.score_16k(&resample::decimate3(x))
    }

    /// Scores a clip at 16 kHz.
    pub fn score_16k(&mut self, x: &[f32]) -> Result<DnsmosScore, String> {
        if x.is_empty() || x.iter().all(|&v| v == 0.0) {
            return Err("DNSMOS: the clip is empty or silent".into());
        }
        let mut audio = x.to_vec();
        while audio.len() < Self::window_len() {
            audio.extend_from_within(..);
        }
        let windows = Self::windows(audio.len());
        let mut sum = [0.0f64; 6];
        for w in &windows {
            let y = self.session.run(audio[w.clone()].to_vec(), &[1, w.len()])?;
            let (sig, bak, ovrl) = (f64::from(y[0]), f64::from(y[1]), f64::from(y[2]));
            let (s, b, o) = Self::calibrate(sig, bak, ovrl);
            for (acc, v) in sum.iter_mut().zip([s, b, o, sig, bak, ovrl]) {
                *acc += v;
            }
        }
        let n = windows.len().max(1) as f64;
        let [sig, bak, ovrl, sig_raw, bak_raw, ovrl_raw] = sum.map(|v| v / n);
        Ok(DnsmosScore { sig, bak, ovrl, sig_raw, bak_raw, ovrl_raw, windows: windows.len() })
    }
}

/// SIGMOS's seven dimensions for a clip, in the model's output order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SigmosScore {
    pub col: f64,
    pub disc: f64,
    pub loud: f64,
    pub noise: f64,
    pub reverb: f64,
    pub sig: f64,
    pub ovrl: f64,
}

impl SigmosScore {
    /// The dimensions' names, in order (sigmos.py's `MOS_*` keys without the prefix).
    pub const NAMES: [&str; 7] = ["COL", "DISC", "LOUD", "NOISE", "REVERB", "SIG", "OVRL"];

    pub fn values(&self) -> [f64; 7] {
        [self.col, self.disc, self.loud, self.noise, self.reverb, self.sig, self.ovrl]
    }
}

/// SIGMOS v1 (see the module notes).
pub struct Sigmos {
    session: MetricSession,
}

impl Sigmos {
    pub const ID: &str = "sigmos";
    pub const KIND: &str = "sigmos-v1";
    /// sigmos.py's STFT: 960 points, hop 480, at 48 kHz.
    pub const DFT: usize = 960;
    pub const HOP: usize = 480;
    pub const BINS: usize = Self::DFT / 2 + 1;
    /// The magnitude's compression exponent.
    pub const COMPRESS: f32 = 0.3;

    pub fn new() -> Result<Self, String> {
        let pack = load(Self::ID, Self::KIND).map_err(|e| e.0)?;
        Ok(Self { session: MetricSession::new(&pack)? })
    }

    /// The model's input for a 48 kHz clip: [1, 3, frames, 481], the planes magnitude, real and imaginary (sigmos.py's
    /// `stft` then `compressed_mag_complex`), and the frame count.
    pub fn features(x: &[f32]) -> (Vec<f32>, usize) {
        let (dft, hop) = (Self::DFT, Self::HOP);
        let last = match x.len() % hop {
            0 => hop,
            r => r,
        };
        // 480 zeros in front, 960 - last behind
        let mut padded = vec![0.0f64; dft - hop];
        padded.extend(x.iter().map(|&v| f64::from(v)));
        padded.resize(padded.len() + dft - last, 0.0);
        let frames = 1 + (padded.len() - dft) / hop;
        // np.sqrt(np.hanning(961)[:-1]) in float32: the periodic Hann's square root
        let window: Vec<f64> =
            (0..dft).map(|n| f64::from((0.5 - 0.5 * (std::f64::consts::TAU * n as f64 / dft as f64).cos()).sqrt() as f32)).collect();
        let mut planner = realfft::RealFftPlanner::<f64>::new();
        let fft = planner.plan_fft_forward(dft);
        let (mut buf, mut spec) = (fft.make_input_vec(), fft.make_output_vec());
        let plane = frames * Self::BINS;
        let mut out = vec![0.0f32; 3 * plane];
        for f in 0..frames {
            for (b, (&v, &w)) in buf.iter_mut().zip(padded[f * hop..f * hop + dft].iter().zip(&window)) {
                *b = v * w;
            }
            fft.process(&mut buf, &mut spec).expect("the FFT's sizes are its own");
            for (k, c) in spec.iter().enumerate() {
                // complex64, then float32 arithmetic, as numpy does it
                let (re, im) = (c.re as f32, c.im as f32);
                let x2 = (re * re + im * im).max(1e-12);
                let scale = x2.powf((Self::COMPRESS - 1.0) / 2.0);
                let i = f * Self::BINS + k;
                out[i] = x2.powf(Self::COMPRESS / 2.0);
                out[plane + i] = scale * re;
                out[2 * plane + i] = scale * im;
            }
        }
        (out, frames)
    }

    /// Scores a 48 kHz clip.
    pub fn score(&mut self, x: &[f32]) -> Result<SigmosScore, String> {
        if x.is_empty() {
            return Err("SIGMOS: the clip is empty".into());
        }
        let (features, frames) = Self::features(x);
        let y = self.session.run(features, &[1, 3, frames, Self::BINS])?;
        let v: Vec<f64> = y.iter().map(|&v| f64::from(v)).collect();
        Ok(SigmosScore { col: v[0], disc: v[1], loud: v[2], noise: v[3], reverb: v[4], sig: v[5], ovrl: v[6] })
    }
}

/// The offline resampler for DNSMOS: 48 kHz to 16 kHz as `dnsmos_local.py` resamples a 48 kHz file. It calls
/// `librosa.resample(aud, input_fs, fs)` with the rates positionally, which dates it before librosa 0.10, and DNS-Challenge
/// 591184a's `requirements.txt` pins librosa 0.8.1: its `resample` runs `resampy.resample(..., filter="kaiser_best")` on the
/// float64 samples, then `fix_length`s the result to `ceil(n / 3)` with zeros. librosa 0.8.1 asks for `resampy >= 0.2.2`, so
/// what it gets depends on when it was installed: resampy 0.3.0 (2022-06-29) regenerated `kaiser_best`, and every release since
/// (0.4.3 now; the arithmetic below unchanged from 0.2.2) has the new one. The pinned set (numpy 1.22.4, onnxruntime 1.13.1)
/// dates from late 2022, so [`decimate3`] is resampy 0.3 to 0.4.3's filter ([`KAISER_BEST`]) and arithmetic, reproduced
/// exactly:
///
/// - **The filter** is a table, its right wing: `precision` points a zero crossing over `zeros` crossings (409 601 entries),
///   `rolloff · sinc(rolloff · j / precision) · kaiser(j)` from resampy's `sinc_window` and scipy's Kaiser window of
///   `2 · zeros · precision + 1` points with its β; scaled by the ratio (1/3) for a decimation.
/// - **Its reads** (resampy's `resample_f`): the table's index step is `int(precision / 3)` = 2730, not 2730.67, so the left
///   wing (input samples `3m - i`) reads entry `2730 i` and the right wing (input samples `3m + 1 + k`) the table linearly
///   interpolated at `2730.67 + 2730 k`: a slightly asymmetric filter, 150 taps behind and 149 ahead of the output's input
///   sample (its gain 0.0015 dB over unity at DC, -0.26 dB at 7 kHz, -14.5 at 7.5 kHz).
/// - **The length:** resampy gives `floor(n / 3)` samples, librosa pads them to `ceil(n / 3)`.
///
/// resampy 0.2.2's `kaiser_best` ([`KAISER_BEST_0_2`]: 64 crossings, 512 points each, its index step 170 for 170.67) is
/// another filter: it differs from this by -52 dB below 4 kHz and -46 dB to 7 kHz, but in the transition band above that by as
/// much as the signal (`tools/check_metric_features.py` ports both), and on the DNSMOS tests' synthetic scenes it moves the
/// scores by up to 0.03 (BAK). `tools/check_metric_features.py` checks [`decimate3`] against numpy ports of librosa 0.8.1's
/// `resample` and resampy 0.4.3's loop: identical, every float32 sample.
pub mod resample {
    /// A resampy filter: its table's zero crossings and points a crossing, its Kaiser window's β and its cut-off as a fraction
    /// of the output's Nyquist.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Filter {
        pub zeros: usize,
        pub precision: usize,
        pub beta: f64,
        pub rolloff: f64,
    }

    /// resampy's `kaiser_best` from 0.3.0 on. resampy stores the table and its roll-off but not β (its `create_filter.py`
    /// printed it to 6 digits, 12.9846): this β, solved from the table's last entry, rebuilds all 409 601 entries to within
    /// 7e-16 of resampy 0.4.3's (relative).
    pub const KAISER_BEST: Filter = Filter { zeros: 50, precision: 8_192, beta: 12.984_585_247_040_012, rolloff: 0.917_347_371_260_876_1 };
    /// resampy 0.2.2's `kaiser_best` (2019 to mid-2022; its docstring's parameters, its table's 512 points a crossing): the
    /// formula rebuilds its 32 769 entries to within 6e-16.
    pub const KAISER_BEST_0_2: Filter =
        Filter { zeros: 64, precision: 512, beta: 14.769_656_459_379_492, rolloff: 0.947_593_716_739_959_6 };
    /// The rates' ratio as Python's `float(16000) / 48000`.
    const RATIO: f64 = 16_000.0 / 48_000.0;

    /// The modified Bessel function of the first kind, order 0 (its power series, to double precision).
    fn i0(x: f64) -> f64 {
        let (mut sum, mut term, q) = (1.0f64, 1.0f64, x * x / 4.0);
        for k in 1..500 {
            term *= q / (k * k) as f64;
            sum += term;
            if term < sum * 1e-17 {
                break;
            }
        }
        sum
    }

    impl Filter {
        /// The table's entries: `zeros · precision + 1`.
        pub fn entries(&self) -> usize {
            self.zeros * self.precision + 1
        }

        /// The table's entry `j`, scaled by the ratio as `resample` scales it (`norm` is `i0(beta)`), each part computed as
        /// numpy and scipy compute it.
        fn entry(&self, j: usize, norm: f64) -> f64 {
            // np.sinc(rolloff * linspace(0, zeros, n + 1)): sin(pi x) / (pi x), 1 at 0
            let y = std::f64::consts::PI * (self.rolloff * (j as f64 / self.precision as f64));
            let sinc = if j == 0 { 1.0 } else { y.sin() / y };
            // scipy.signal.windows.kaiser(2n + 1, beta)[n:]
            let r = j as f64 / (self.entries() - 1) as f64;
            let taper = i0(self.beta * (1.0 - r * r).sqrt()) / norm;
            RATIO * (taper * (self.rolloff * sinc))
        }

        /// resampy's whole table, scaled.
        pub fn table(&self) -> Vec<f64> {
            let norm = i0(self.beta);
            (0..self.entries()).map(|j| self.entry(j, norm)).collect()
        }

        /// The decimator's taps, as `resample_f` computes them for every output sample (each one's input position is a whole
        /// sample, `3m`): the left wing's for input samples `3m - i` and the right wing's for `3m + 1 + k`. Only the entries
        /// they read are computed.
        pub fn taps(&self) -> (Vec<f64>, Vec<f64>) {
            let (norm, nwin, num_table) = (i0(self.beta), self.entries(), self.precision as f64);
            let at = |j: usize| self.entry(j, norm);
            let delta = |j: usize| if j + 1 < nwin { at(j + 1) - at(j) } else { 0.0 };
            let index_step = (RATIO * num_table) as usize;
            // The left wing: frac 0, so offset 0 and eta 0
            let left = (0..nwin / index_step).map(|i| at(i * index_step)).collect();
            // The right wing: frac = scale - 0
            let index_frac = RATIO * num_table;
            let offset = index_frac as usize;
            let eta = index_frac - offset as f64;
            let right =
                (0..(nwin - offset) / index_step).map(|k| at(offset + k * index_step) + eta * delta(offset + k * index_step)).collect();
            (left, right)
        }
    }

    /// `x` (48 kHz) at 16 kHz, as librosa 0.8.1 and resampy 0.4 give it ([`KAISER_BEST`]; see [`decimate3_with`]).
    pub fn decimate3(x: &[f32]) -> Vec<f32> {
        decimate3_with(&KAISER_BEST, x)
    }

    /// `x` (48 kHz) at 16 kHz through `filter` as librosa 0.8.1 and resampy run it: output sample m is the left wing over
    /// `x[3m - i]` and then the right wing over `x[3m + 1 + k]`, summed in that order in float64 (taps past either end left
    /// out), for `floor(n / 3)` samples, then zeros to `ceil(n / 3)`; rounded to float32 at the end, as the script's windows
    /// are.
    pub fn decimate3_with(filter: &Filter, x: &[f32]) -> Vec<f32> {
        let (left, right) = filter.taps();
        let n = x.len();
        let mut y: Vec<f32> = (0..n / 3)
            .map(|m| {
                let c = 3 * m;
                let mut acc = 0.0f64;
                for (i, h) in left.iter().enumerate().take(c + 1) {
                    acc += h * f64::from(x[c - i]);
                }
                for (k, h) in right.iter().enumerate().take(n - c - 1) {
                    acc += h * f64::from(x[c + 1 + k]);
                }
                acc as f32
            })
            .collect();
        y.resize(n.div_ceil(3), 0.0);
        y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_windows_are_dnsmos_local_s() {
        assert_eq!(Dnsmos::window_len(), 144_160);
        // 15 s: int(15 - 9.01) + 1 = 6 windows, a second apart
        let w = Dnsmos::windows(15 * 16_000);
        assert_eq!(w.len(), 6);
        assert_eq!((w[0].clone(), w[5].clone()), (0..144_160, 80_000..224_160));
        // 9.5 s and 10 s: one (int(0.49) + 1, int(0.99) + 1)
        assert_eq!(Dnsmos::windows(152_000).len(), 1);
        assert_eq!(Dnsmos::windows(160_000).len(), 1);
        // 20.3 s: int(10.99) + 1 = 11 windows, but from the eighth on (16.01 s) the end rounds down a sample in floating
        // point (256159.999...), so each is a sample short and the script skips it: 7, as Python gives. A 45 s clip keeps 19
        // of 36. The Lab's clips (15 s at most) lose none.
        let w = Dnsmos::windows(324_800);
        assert_eq!(w.len(), 7);
        assert!(w.iter().all(|r| r.len() == 144_160));
        assert_eq!(Dnsmos::windows(45 * 16_000).len(), 19);
        assert!(Dnsmos::windows(100_000).is_empty());
        // Every clip under 17 s keeps all its windows (`lab`'s cap of 16 s rests on it); at 17 s the eighth goes
        let all = |len: usize| ((len as f64 / 16_000.0).floor() - Dnsmos::INPUT_LENGTH_S) as usize + 1;
        assert!((Dnsmos::window_len()..17 * 16_000).all(|len| Dnsmos::windows(len).len() == all(len)));
        assert_eq!((Dnsmos::windows(17 * 16_000).len(), all(17 * 16_000)), (7, 8));
    }

    #[test]
    fn the_calibration_is_the_scripts_polynomials() {
        let (s, b, o) = Dnsmos::calibrate(3.0, 3.0, 3.0);
        assert!((s - (-0.08397278 * 9.0 + 1.22083953 * 3.0 + 0.0052439)).abs() < 1e-12);
        assert!((b - (-0.13166888 * 9.0 + 1.60915514 * 3.0 - 0.39604546)).abs() < 1e-12);
        assert!((o - (-0.06766283 * 9.0 + 1.11546468 * 3.0 + 0.04602535)).abs() < 1e-12);
    }

    /// resampy's `resample_f` (0.4.3, the general loop over any output times) written out as it reads, run with `resample`'s
    /// set-up for 48 to 16 kHz, then librosa 0.8.1's `fix_length`: [`resample::decimate3`] is the same, value for value.
    fn resampy_literal(filter: &resample::Filter, x: &[f64]) -> Vec<f64> {
        let sample_ratio = 16_000.0f64 / 48_000.0;
        let n_out = (x.len() as f64 * 16_000.0 / 48_000.0) as usize;
        let time_increment = 1.0 / sample_ratio;
        let t_out: Vec<f64> = (0..n_out).map(|t| t as f64 * time_increment).collect();
        let interp_win = filter.table();
        let interp_delta: Vec<f64> = (0..interp_win.len()).map(|j| interp_win.get(j + 1).map_or(0.0, |v| v - interp_win[j])).collect();
        let num_table = filter.precision;
        let scale = sample_ratio.min(1.0);
        let index_step = (scale * num_table as f64) as usize;
        let (nwin, n_orig) = (interp_win.len(), x.len());
        let mut y = vec![0.0f64; n_out];
        for t in 0..n_out {
            let time_register = t_out[t];
            let n = time_register as usize;
            let mut frac = scale * (time_register - n as f64);
            let mut index_frac = frac * num_table as f64;
            let mut offset = index_frac as usize;
            let mut eta = index_frac - offset as f64;
            let i_max = (n + 1).min((nwin - offset) / index_step);
            for i in 0..i_max {
                let weight = interp_win[offset + i * index_step] + eta * interp_delta[offset + i * index_step];
                y[t] += weight * x[n - i];
            }
            frac = scale - frac;
            index_frac = frac * num_table as f64;
            offset = index_frac as usize;
            eta = index_frac - offset as f64;
            let k_max = (n_orig - n - 1).min((nwin - offset) / index_step);
            for k in 0..k_max {
                let weight = interp_win[offset + k * index_step] + eta * interp_delta[offset + k * index_step];
                y[t] += weight * x[n + k + 1];
            }
        }
        // librosa: n_samples = int(np.ceil(len * ratio)), fix_length pads with zeros
        y.resize((x.len() as f64 * sample_ratio).ceil() as usize, 0.0);
        y
    }

    /// The resampler is resampy's loop, value for value, at every length's ends (and so librosa's padding), for both filters;
    /// its taps are resampy's (150 behind, 149 ahead); a tone in the band comes through at its level with no delay to speak
    /// of, one above 8 kHz is gone.
    #[test]
    fn the_resampler_is_resampy_s_and_keeps_the_band() {
        let (left, right) = resample::KAISER_BEST.taps();
        assert_eq!((left.len(), right.len()), (150, 149));
        let full = resample::KAISER_BEST.table();
        assert_eq!(
            (full.len(), left[7], right[0]),
            (409_601, full[7 * 2730], full[2730] + (2730.0 + 2.0 / 3.0 - 2730.0) * (full[2731] - full[2730]))
        );
        let old = resample::KAISER_BEST_0_2.taps();
        assert_eq!((old.0.len(), old.1.len()), (192, 191), "0.2.2's: 32 769 entries in steps of 170");
        // resampy's gain at DC: 0.0015 dB over unity
        let dc = left.iter().chain(&right).sum::<f64>();
        assert!((dc - 1.000_171_446).abs() < 1e-8, "DC gain {dc:.9}");
        // A deterministic signal: a chirp and a little noise
        let x: Vec<f32> = (0..4_003)
            .map(|i| {
                let t = i as f64 / 48_000.0;
                (0.3 * (std::f64::consts::TAU * (200.0 + 40_000.0 * t) * t).sin() + 0.01 * ((i * 7_919 % 1_000) as f64 / 500.0 - 1.0))
                    as f32
            })
            .collect();
        for filter in [resample::KAISER_BEST, resample::KAISER_BEST_0_2] {
            for len in [4_003, 4_002, 4_001, 600, 301, 2] {
                let ours = resample::decimate3_with(&filter, &x[..len]);
                let theirs = resampy_literal(&filter, &x[..len].iter().map(|&v| f64::from(v)).collect::<Vec<_>>());
                assert_eq!(ours.len(), len.div_ceil(3), "{len}");
                assert_eq!(ours, theirs.iter().map(|&v| v as f32).collect::<Vec<_>>(), "{filter:?} {len}");
            }
        }
        assert_eq!(resample::decimate3(&x), resample::decimate3_with(&resample::KAISER_BEST, &x));
        let tone =
            |hz: f64, n: usize| -> Vec<f32> { (0..n).map(|i| (std::f64::consts::TAU * hz * i as f64 / 48_000.0).sin() as f32).collect() };
        let rms = |v: &[f32]| (v.iter().map(|&s| f64::from(s) * f64::from(s)).sum::<f64>() / v.len() as f64).sqrt();
        let x = tone(1000.0, 48_000);
        let y = resample::decimate3(&x);
        assert_eq!(y.len(), 16_000);
        let worst = (1000..15_000).map(|m| (y[m] - x[3 * m]).abs()).fold(0.0f32, f32::max);
        let gain = |hz: f64| rms(&resample::decimate3(&tone(hz, 48_000))[1000..15_000]) / rms(&tone(hz, 48_000)[3000..45_000]);
        let db = |g: f64| 20.0 * g.log10();
        // Sample m is the tone at input sample 3m to 2e-4 (the gain and the wings' asymmetry); the band flat to 6 kHz, the
        // roll-off (0.917 of 8 kHz) from 7 kHz, and 9 kHz (which would alias to 7) 90 dB down
        assert!(worst < 2e-4, "{worst}");
        for (hz, want, within) in [(1000.0, 0.0015, 0.001), (6000.0, 0.0021, 0.001), (7000.0, -0.26, 0.01), (7500.0, -14.5, 0.1)] {
            assert!((db(gain(hz)) - want).abs() < within, "{hz} Hz: {:+.4} dB", db(gain(hz)));
        }
        assert!(db(gain(9000.0)) < -85.0, "9 kHz: {:+.1} dB", db(gain(9000.0)));
    }

    #[test]
    fn sigmos_features_are_framed_as_the_script_frames_them() {
        // 1000 samples: last frame 40, padded to 480 + 1000 + 920 = 2400, so 4 frames
        let x: Vec<f32> = (0..1000).map(|i| ((i as f32) * 0.01).sin()).collect();
        let (f, frames) = Sigmos::features(&x);
        assert_eq!((frames, f.len()), (4, 3 * 4 * 481));
        // A whole number of hops: 960 samples, padded 480 + 960 + 480 = 1920, so 3 frames
        assert_eq!(Sigmos::features(&vec![0.1; 960]).1, 3);
        // Silence: the magnitude plane is 1e-12^0.15, the phase planes 0
        let (s, _) = Sigmos::features(&[0.0; 480]);
        let plane = s.len() / 3;
        assert!(s[..plane].iter().all(|&v| (v - 1e-12f32.powf(0.15)).abs() < 1e-9) && s[plane..].iter().all(|&v| v == 0.0));
        // Each bin: the magnitude plane is |X|^0.3 and the phase planes' own magnitude is the same
        let p = f.len() / 3;
        for i in [5, 481 + 17, 3 * 481 + 200] {
            let m = (f[p + i] * f[p + i] + f[2 * p + i] * f[2 * p + i]).sqrt();
            assert!((m - f[i]).abs() < 1e-5 * f[i].max(1.0), "bin {i}: {m} vs {}", f[i]);
        }
    }

    #[test]
    fn a_metric_manifest_is_checked_as_one() {
        let m: Manifest = serde_json::from_str(include_str!("../../../packs/dnsmos/manifest.json")).unwrap();
        assert_eq!(check_metric(&m), Ok(()));
        assert!(crate::pack::check(&m, "dnsmos").unwrap_err().contains("role"), "never a noise model");
        let s: Manifest = serde_json::from_str(include_str!("../../../packs/sigmos/manifest.json")).unwrap();
        assert_eq!(crate::pack::check_files(&s, "sigmos").and_then(|()| check_metric(&s)), Ok(()));
        let u: Manifest = serde_json::from_str(include_str!("../../../packs/ulunas/manifest.json")).unwrap();
        assert!(check_metric(&u).unwrap_err().contains("role"), "a noise model is never a metric");
        let mut bad = m.clone();
        bad.signal.sample_rate = 44_100;
        assert!(check_metric(&bad).is_err());
        let mut bad = m.clone();
        bad.metric = None;
        assert!(check_metric(&bad).unwrap_err().contains("metric.kind"));
        let mut bad = m;
        bad.io.input.shape = vec![1, 0];
        assert!(check_metric(&bad).unwrap_err().contains("io.input"));
        assert!(same_dims(&[1, -1, 481], &[1, -1, 481]) && !same_dims(&[1, 3, 481], &[1, -1, 481]) && !same_dims(&[1], &[1, 1]));
    }
}
