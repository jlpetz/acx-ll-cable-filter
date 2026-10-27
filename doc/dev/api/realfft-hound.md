# API reference: realfft 3.5.0 (+ rustfft 6.4.1, num-complex 0.4.6) and hound 3.5.1

Written 2026-10-26 from the registry sources (versions from `Cargo.lock`):

| crate | path under `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/` | used by |
|---|---|---|
| realfft 3.5.0 | `realfft-3.5.0/src/lib.rs` (one file) | `nf-core` |
| rustfft 6.4.1 | `rustfft-6.4.1/src/{lib,plan,common,twiddles}.rs` | via realfft |
| num-complex 0.4.6 | `num-complex-0.4.6/src/lib.rs` | via `realfft::num_complex` |
| hound 3.5.1 | `hound-3.5.1/src/{lib,read,write}.rs` | `nf-stages`, `nf-app` |

`file:line` below is relative to each crate's folder. Recipe code in section 3 is ours: it was
written against these sources but never compiled (this task forbade running cargo). Compile it and
unit-test it before relying on it.

---

## 1. realfft 3.5.0

### 1.1 Re-exports and the error type
```rust
pub use rustfft::num_complex;   // src/lib.rs:3  -> realfft::num_complex::Complex
pub use rustfft::num_traits;    // src/lib.rs:4
pub use rustfft::FftNum;        // src/lib.rs:5  (f32, f64)
```
realfft doesn't re-export `rustfft` or `FftPlanner`. To call `RealToComplexEven::new(len, &mut FftPlanner)`
(lib.rs:94-118, 374) directly you need your own `rustfft = "6.4"` dependency. The planner below is enough for us.

```rust
pub enum FftError {                       // src/lib.rs:18-41
    InputBuffer(usize, usize),            // (expected, got): transform NOT performed
    OutputBuffer(usize, usize),           // (expected, got): NOT performed
    ScratchBuffer(usize, usize),          // (minimum, got): NOT performed
    InputValues(bool, bool),              // c2r only: (first, last) bin had im != 0.
}                                         //   The transform WAS performed (see 1.5)
impl fmt::Debug / fmt::Display / std::error::Error for FftError   // :71-83
type Res<T> = Result<T, FftError>;        // :15 (private alias)
```
`Debug` prints the same text as `Display`, not the variant name (:71-75). Use `matches!` in tests.
`FftError` holds only `usize`s and `bool`s, so it's `Send + Sync + 'static`.

### 1.2 Planner
```rust
pub struct RealFftPlanner<T: FftNum> { planner: FftPlanner<T>,                          // :232-236
    r2c_cache: HashMap<usize, Arc<dyn RealToComplex<T>>>, c2r_cache: HashMap<usize, Arc<dyn ComplexToReal<T>>> }
impl<T: FftNum> RealFftPlanner<T> {
    pub fn new() -> Self;                                                          // :240
    pub fn plan_fft_forward(&mut self, len: usize) -> Arc<dyn RealToComplex<T>>;  // :252
    pub fn plan_fft_inverse(&mut self, len: usize) -> Arc<dyn ComplexToReal<T>>;  // :270
}
impl<T: FftNum> Default for RealFftPlanner<T>                                      // :286
```
- **Caching:** the same `len` returns `Arc::clone` of the cached plan (:253-254, :271-272), so asking
  again is cheap but still needs `&mut self`.
- **Odd and even lengths:** odd `len` uses a complex FFT of `len` (:256-257, :296-307). Even `len`
  packs the input into `len/2` complex values and post-processes with twiddles (:374-394), which
  is about 2x faster for long FFTs (README:11-25). **Use even lengths.**
- **SIMD:** `FftPlanner::new()` chooses AVX+FMA, then SSE4.1, then scalar, at runtime
  (rustfft `src/plan.rs:72-94`, `src/avx/avx_planner.rs:121-125`). realfft's default features
  enable rustfft's `avx`, `sse` and `neon` (realfft Cargo.toml `[features]`).

### 1.3 Traits (all methods take `&self`; the plan objects are immutable)
```rust
pub trait RealToComplex<T>: Sync + Send {                                         // :123
    fn process(&self, input: &mut [T], output: &mut [Complex<T>]) -> Res<()>;     // :131
    fn process_with_scratch(&self, input: &mut [T], output: &mut [Complex<T>],
                            scratch: &mut [Complex<T>]) -> Res<()>;               // :138-143
    fn get_scratch_len(&self) -> usize;                                           // :146
    fn len(&self) -> usize;                                                       // :150 (real length N)
    fn complex_len(&self) -> usize { self.len() / 2 + 1 }                         // :153-155
    fn make_input_vec(&self) -> Vec<T>;                 // len N            :158
    fn make_output_vec(&self) -> Vec<Complex<T>>;       // len N/2+1        :161
    fn make_scratch_vec(&self) -> Vec<Complex<T>>;      // len get_scratch_len() :164
}
pub trait ComplexToReal<T>: Sync + Send {                                         // :170
    fn process(&self, input: &mut [Complex<T>], output: &mut [T]) -> Res<()>;     // :182
    fn process_with_scratch(&self, input: &mut [Complex<T>], output: &mut [T],
                            scratch: &mut [Complex<T>]) -> Res<()>;               // :189-194
    fn get_scratch_len(&self) -> usize;                                           // :197
    fn len(&self) -> usize;                                                       // :201 (real output N)
    fn complex_len(&self) -> usize { self.len() / 2 + 1 }                         // :204-206
    fn make_input_vec(&self) -> Vec<Complex<T>>;        // len N/2+1        :209
    fn make_output_vec(&self) -> Vec<T>;                // len N            :212
    fn make_scratch_vec(&self) -> Vec<Complex<T>>;      //                  :215
}
```
- **Lengths must match exactly:** input `== len()` and output `== complex_len()` (or the reverse
  for c2r). Anything else returns `InputBuffer`/`OutputBuffer` (:322-331, :409-418, :553-562,
  :655-664). Unlike rustfft, there's no batching of several chunks in one call.
- **Scratch may be longer than needed:** the check is `scratch.len() < scratch_len` (:332, :419,
  :563, :665; README:160-161 "3.2.0 allow scratch larger"). So one buffer sized
  `max(r2c.get_scratch_len(), c2r.get_scratch_len())` serves both directions.
- **The scratch length can be 0:** for even N it is the inner FFT's
  `get_outofplace_scratch_len()` (:387, :634), which rustfft says "for most FFT sizes … returns 0"
  (rustfft `src/lib.rs:266`). Always size it from `get_scratch_len()`. rustfft says the value
  "may change from one version … to the next" (rustfft lib.rs:261, 268).

### 1.4 Layout, sign and scaling
- **Forward:** `X[k] = Σ_n x[n]·e^{-j2πkn/N}`. **Inverse:** `x[n] = Σ_k X[k]·e^{+j2πkn/N}`, with
  **no 1/N on either** (rustfft `src/twiddles.rs:11-21`; inverse = conj twiddle; rustfft
  lib.rs:81-85; realfft README:83-88). So `c2r(r2c(x)) = N·x`. Scale once by `1/N`.
- **Spectrum:** `[X0, X1, …, X_{N/2}]` (`N/2+1` bins, ascending frequency, rustfft lib.rs:87-89).
  Bin k is at `k·fs/N` Hz.
  - `X0.im` is forced to exactly 0 (:342-345 odd, :440-443 even).
  - For even N, `X_{N/2}.im` is forced to exactly 0 too (:444-447).
- **The c2r input must have `im == 0` at bin 0, and at bin N/2 when N is even.** Otherwise the code
  zeroes those imaginary parts in place, runs the transform anyway, and returns
  `Err(InputValues(first, last))` (:567-572/589-591 odd; :671-682/752-754 even). `?` then
  **throws away a valid result**. After multiplying spectra, set `spec[0].im = 0.0` and
  `spec[last].im = 0.0` yourself. A NaN imaginary part trips the check too (`NaN != 0`).

### 1.5 Buffers are destroyed
- **r2c, even N:** `input` is reinterpreted as `&mut [Complex<T>]` (:424-428) and passed as the
  *input* of rustfft's out-of-place FFT, which "uses both the input buffer and scratch buffer as
  scratch space" (rustfft lib.rs:215-216). **The input is garbage afterwards.**
- **r2c, odd N:** the code copies the input into scratch (:337-339), so the input survives. The
  doc (:127-128) still says garbage, so don't rely on it.
- **c2r:** the input spectrum is preprocessed in place (:684-742) and then used as FFT input
  (:750-751). **The spectrum is garbage afterwards.** Copy it first if you need it again.

### 1.6 What allocates

| call | allocates? | source |
|---|---|---|
| `RealFftPlanner::new()` | yes: `FftPlanner::new` boxes the AVX planner | rustfft avx_planner.rs:113-114 |
| `plan_fft_forward/inverse` (first time per len) | yes: twiddle `Vec`s, `Arc`s, `HashMap` inserts | :257-262, :383-385, :630-632 |
| `plan_*` (cached len) | no heap allocation (a HashMap lookup and `Arc::clone`) | :253-254 |
| `make_input_vec/make_output_vec/make_scratch_vec` | yes (`vec![…]`) | :357-367, :509-519, :603-613, :766-776 |
| `process()` (both traits) | **yes, on every call**: `make_scratch_vec()` (no heap when the length is 0) | :311-314, :398-401, :542-545, :644-647 |
| `process_with_scratch()` | **no**, in realfft (:316-347, :403-500, :547-593, :649-756) and rustfft (see below) | |
| drop of the last `Arc<dyn …>` | frees | |

- **rustfft:** I grepped every `perform_fft_*` in `src/algorithm`, `src/avx` and `src/sse` for
  `vec!`, `Vec::`, `to_vec`, `collect` and `Box::new`, and found hits only in constructors and doc
  comments. The scratch-taking paths go through `fft_helper_*` (src/fft_helper.rs), which doesn't
  allocate. rustfft's own `Fft::process` allocates (rustfft lib.rs:195-198).
- **No locks or thread-locals:** a grep for `Rc<`, `RefCell`, `Cell<`, `thread_local`,
  `static mut`, `Mutex` and `OnceLock` in rustfft `src` returns nothing.
- **For our audio threads:** plan and `make_*` on the control thread, outside an `RtScope` (the
  pool is for the audio path). Move the finished struct in; it's `Send`. Drop it outside the audio
  thread.

### 1.7 Send/Sync and threading
- **Plans:** `RealToComplex<T>` and `ComplexToReal<T>` have `Sync + Send` supertraits (:123,
  :170), so `Arc<dyn RealToComplex<f32>>` can be shared by any number of threads at once, each with
  its own buffers. realfft's `examples/concurrency.rs` does exactly that.
- **Planners:** `RealFftPlanner<T>` is auto `Send + Sync`. Its fields are HashMaps of those Arcs
  plus `FftPlanner<T>`, whose internals are `HashMap<usize, Arc<dyn Fft<T>>>` (with
  `Fft: Sync + Send`, rustfft lib.rs:184) and `Box<dyn AvxPlannerInternalAPI<T>>` (with
  `: Send + Sync`, avx_planner.rs:194). Planning takes `&mut self`, so a shared planner needs a
  `Mutex`. Better: plan once at setup.
- **No COM or OS calls.** CPU feature detection is `is_x86_feature_detected!` at plan time.

### 1.8 Sizes
- **Fast lengths:** RustFFT (AVX) is fastest for `2^a·3^b`, very fast when every prime factor is
  ≤ 11, and markedly slower otherwise; a prime length can be about 5x slower
  (rustfft lib.rs:93-114).
- **Our usual lengths:**
  - 9600 (2^7·3·5^2) and 96000 (2^8·3·5^3) are fine.
  - Use `next_fast_len` (3.1) when you have a free choice.
  - Keep N even for realfft's fast path.

### 1.9 num-complex 0.4.6 bits we use
- **The type:** `#[repr(C)] pub struct Complex<T> { pub re: T, pub im: T }` (src/lib.rs:80-93),
  which has the same alignment as `T`. That's why realfft's even-length reinterpretation of `&mut [f32]` is
  sound.
- **Methods and operators:**
  - `pub const fn new(re, im)` (:132);
  - `pub fn norm_sqr(&self) -> T` (:149);
  - `pub fn conj(&self) -> Self` (:175);
  - `pub fn scale(&self, t: T)` (:155);
  - `pub fn norm(self) -> T` calls `hypot` (:217) and is slower than `norm_sqr().sqrt()`; prefer
    the latter in loops;
  - `Complex * Complex` (:779) and `Complex * T` (:1152) are implemented;
  - `Complex::ZERO` exists (:1184, needs `T: ConstZero`). `Complex::new(0.0, 0.0)` is the
    no-surprise choice.

---

## 2. hound 3.5.1

- **Edition and MSRV:** hound is an edition-2015 crate (no `edition` key; it uses `try!` and bare
  trait objects). Its `rust-toolchain.toml` (1.40.0) has no effect on our builds.
- **Not real-time:** every read and write goes through `std::io`. **Never call hound on an audio
  thread.** Use an `rtrb` ring to a writer thread, or the offline harness only.

### 2.1 Spec, sample format, errors (`src/lib.rs`)
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat { Float, Int }                                   // :320-329
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WavSpec { pub channels: u16, pub sample_rate: u32,
                     pub bits_per_sample: u16, pub sample_format: SampleFormat }   // :332-349
#[derive(Clone, Copy)]
pub struct WavSpecEx { pub spec: WavSpec, pub bytes_per_sample: u16 }  // :352-362
#[derive(Debug)]
pub enum Error { IoError(io::Error), FormatError(&'static str), TooWide,
                 UnfinishedSample, Unsupported, InvalidSampleFormat }  // :365-392
impl fmt::Display, error::Error, From<io::Error> for Error             // :394-447
pub type Result<T> = result::Result<T, Error>;                         // :450
pub trait Sample: Sized { … fn as_i16(self) -> i16; }                  // :79-96, impl for i8 i16 i32 f32
```
`hound::Error` is auto `Send + Sync`, so `?` into `Box<dyn Error + Send + Sync>` or `anyhow` works.

**hound never converts between int and float.** The Rust sample type must match the file:

| file | read as | source |
|---|---|---|
| Float, 32-bit | `f32` only. Int types give `InvalidSampleFormat`. | lib.rs:307-316 |
| Int, 16-bit | `i16` or `i32`. `f32` gives `InvalidSampleFormat`. | lib.rs:237-248, :274-287 |
| Int, 24-bit (3- or 4-byte container), 32-bit | `i32` only. `i16` gives `TooWide`. | lib.rs:244, :281-284 |

Every sample of a mismatched iterator yields `Err`.

### 2.2 Reader (`src/read.rs`; re-exported at lib.rs:68)
```rust
pub struct WavReader<R> { … }                                                   // :217
impl<R: io::Read> WavReader<R> {
    pub fn new(reader: R) -> Result<WavReader<R>>;           // :596 reads the header immediately
    pub fn spec(&self) -> WavSpec;                           // :631
    pub fn samples<'wr, S: Sample>(&'wr mut self) -> WavSamples<'wr, R, S>;    // :650
    pub fn into_samples<S: Sample>(self) -> WavIntoSamples<R, S>;              // :660
    pub fn duration(&self) -> u32;   // frames (samples per channel)            :673
    pub fn len(&self) -> u32;        // total samples, all channels             :683
    pub fn into_inner(self) -> R;                                               // :688
    pub fn seek(&mut self, time: u32) -> io::Result<()> where R: io::Seek;      // :702 (frames)
}
impl WavReader<io::BufReader<fs::File>> {
    pub fn open<P: AsRef<path::Path>>(filename: P) -> Result<WavReader<io::BufReader<fs::File>>>; // :720
}
// Both iterators: Item = hound::Result<S>, and both implement ExactSizeIterator (:748-788)
pub fn read_wave_header<R: io::Read>(reader: &mut R) -> Result<u64>;           // :266
```
- **Iterators:** `samples()` is streaming and interleaved. A second call continues where the first
  stopped (:635-641).
- **What it reads:**
  - `WAVE_FORMAT_PCM` (fmt chunk of 16, 18 or 40 bytes, :465-503);
  - `IEEE_FLOAT` (fmt chunk of 16, or 18 with `cbSize == 0`, else `FormatError`, :505-535);
  - `EXTENSIBLE` (`cbSize` must be 22; PCM or IEEE_FLOAT subformat, :537-590);
  - ADPCM and other tags give `Unsupported` (:451-457).
- **Chunks:**
  - unknown chunks are skipped by exactly `len` bytes, with no RIFF pad byte (:325-328);
  - `fact` reads only 4 bytes (:303-315);
  - `fmt` must come before `data`, and chunks after `data` are ignored (:316-323).
- **`seek` bug:** it uses `bits_per_sample / 8` rather than the container size (:705), so it's
  wrong for 24-in-4-byte files. It's fine for 16-bit int and 32-bit float.

### 2.3 Writer (`src/write.rs`)
```rust
pub struct WavWriter<W> where W: io::Write + io::Seek { … }                     // :160
impl<W: io::Write + io::Seek> WavWriter<W> {
    pub fn new(writer: W, spec: WavSpec) -> Result<WavWriter<W>>;               // :209 (writes header now)
    pub fn new_with_spec_ex(writer: W, spec_ex: WavSpecEx) -> Result<WavWriter<W>>; // :227
    pub fn write_sample<S: Sample>(&mut self, sample: S) -> Result<()>;         // :429
    pub fn get_i16_writer<'s>(&'s mut self, num_samples: u32) -> SampleWriter16<'s, W>; // :454
    pub fn flush(&mut self) -> Result<()>;       // rewrite header + flush, keep writing  :527
    pub fn finalize(mut self) -> Result<()>;     // rewrite header + flush               :540
    pub fn spec(&self) -> WavSpec;               // :557
    pub fn duration(&self) -> u32;               // frames written                       :566
    pub fn len(&self) -> u32;                    // samples written                      :574
}
impl<W: io::Write + io::Seek> Drop for WavWriter<W>   // finalizes if you didn't; errors ignored :579-590
impl WavWriter<io::BufWriter<fs::File>> {
    pub fn create<P: AsRef<path::Path>>(filename: P, spec: WavSpec)
        -> Result<WavWriter<io::BufWriter<fs::File>>>;   // truncates               :645
    pub fn append<P: AsRef<path::Path>>(filename: P) -> Result<WavWriter<io::BufWriter<fs::File>>>; // :660
}
impl<W: io::Read + io::Write + io::Seek> WavWriter<W> { pub fn new_append(writer: W) -> Result<WavWriter<W>>; } // :702
pub struct SampleWriter16<'parent, W> where W: io::Write + io::Seek + 'parent { … }  // :736
impl<'parent, W: io::Write + io::Seek> SampleWriter16<'parent, W> {
    pub fn write_sample<S: Sample>(&mut self, sample: S);               // :765 panics past num_samples
    pub unsafe fn write_sample_unchecked<S: Sample>(&mut self, sample: S); // :788
    pub fn flush(self) -> Result<()>;                                   // :799 panics if under-filled
}
```
- **Header choice:** the writer uses PCMWAVEFORMAT (16-byte fmt, 44-byte header) when
  `channels <= 2 && bits <= 16`, and WAVE_FORMAT_EXTENSIBLE (40-byte fmt, 68-byte header)
  otherwise (:234-250). **Mono f32 is therefore written as EXTENSIBLE with the IEEE_FLOAT
  subformat** (:404-411).
- **Bit depths:** 8, 16, 24 and 32 are allowed; anything else returns `Err(Unsupported)` before
  the header is written (:256-266).
- **`Float` with bits ≠ 32 panics** in the header writer instead of returning `Err` (:353-360,
  :408-415).
- **No format check on write.**
  - `write_sample` dispatches only on `(bits, byte_width)`. An `f32` written to a 32-bit **Int**
    spec writes raw float bits (lib.rs:296-300), and an `i16` or `i32` written to a Float spec
    writes integers (lib.rs:227, :264).
  - Both are silent corruption. Assert `spec.sample_format` matches before writing.
- **`SampleWriter16`:**
  - `get_i16_writer` panics unless the spec is Int/16 (:457-462);
  - `write_sample(f32)` panics (`f32::as_i16`, lib.rs:303-305);
  - it does no range check; it keeps the low 16 bits (:756-758).
- **Size limit:** the length is a `u32` byte count (:175, :435), so 4 GiB at most. That's about
  6.2 h of mono f32 at 48 kHz, or 12.4 h of i16. Past that, the `+=` panics in debug builds (dev
  overflow checks) and wraps in release, corrupting the header.
- **Finalize:** call `finalize()` and handle its error. Drop finalizes too, but swallows errors
  (:582-588).
- **Send:** `WavWriter<W>` is `Send` iff `W: Send`, and `WavReader<R>` iff `R: Send`. A
  `BufWriter<File>` and a `BufReader<File>` are both Send.

### 2.4 Snippets (mono, 48 kHz)
```rust
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
type AnyErr = Box<dyn std::error::Error + Send + Sync>;

pub const SPEC_F32: WavSpec = WavSpec { channels: 1, sample_rate: 48_000, bits_per_sample: 32, sample_format: SampleFormat::Float };
pub const SPEC_I16: WavSpec = WavSpec { channels: 1, sample_rate: 48_000, bits_per_sample: 16, sample_format: SampleFormat::Int };

pub fn write_f32(path: &std::path::Path, x: &[f32]) -> Result<(), AnyErr> {
    let mut w = WavWriter::create(path, SPEC_F32)?;
    for &s in x { w.write_sample(s)?; }
    w.finalize()?;                                   // don't rely on Drop
    Ok(())
}

pub fn write_i16(path: &std::path::Path, x: &[f32]) -> Result<(), AnyErr> {
    let mut w = WavWriter::create(path, SPEC_I16)?;
    {   // fast path: one buffered write, no per-sample Result
        let mut sw = w.get_i16_writer(x.len() as u32);
        for &s in x { sw.write_sample((s.clamp(-1.0, 1.0) * 32767.0).round() as i16); }
        sw.flush()?;
    }
    w.finalize()?;
    Ok(())
}

/// Any mono 16/24/32-bit int or 32-bit float file -> f32 in [-1, 1). Returns (samples, sample_rate).
pub fn read_mono_f32(path: &std::path::Path) -> Result<(Vec<f32>, u32), AnyErr> {
    let mut r = WavReader::open(path)?;
    let spec = r.spec();
    if spec.channels != 1 { return Err(format!("expected mono, got {} channels", spec.channels).into()); }
    let x: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (SampleFormat::Float, 32) => r.samples::<f32>().collect::<hound::Result<_>>()?,
        (SampleFormat::Int, 16) => r.samples::<i16>().map(|s| s.map(|v| v as f32 / 32_768.0)).collect::<hound::Result<_>>()?,
        (SampleFormat::Int, 24) => r.samples::<i32>().map(|s| s.map(|v| v as f32 / 8_388_608.0)).collect::<hound::Result<_>>()?,
        (SampleFormat::Int, 32) => r.samples::<i32>().map(|s| s.map(|v| v as f32 / 2_147_483_648.0)).collect::<hound::Result<_>>()?,
        (f, b) => return Err(format!("unsupported WAV: {f:?} {b}-bit").into()),
    };
    Ok((x, spec.sample_rate))   // caller checks 48 000; hound doesn't resample
}
```
The scaling conventions (÷32768 in, ×32767 with a clamp out) are our choice. hound has none.

---

## 3. Recipes: delay estimation (allocation-free after `new`)

These are for the latency test (TODO #13, `doc/todo/13-latency-measurement.md`) and the offline
harness (#2).
- **Where it runs:** the analysis runs after capture, but we keep it allocation-free so it can
  share a thread with I/O.
- **Allocation:** every `new` allocates. Every `estimate`, `deconvolve` and `arrival` call doesn't.

### 3.1 Shared helpers
```rust
use std::sync::Arc;
use realfft::num_complex::Complex;
use realfft::{ComplexToReal, FftError, RealFftPlanner, RealToComplex};

/// Smallest 2^a·3^b >= n with a >= 1 (even: realfft's fast path; 2^a·3^b: RustFFT's fastest, rustfft lib.rs:100).
pub fn next_fast_len(n: usize) -> usize {
    let n = n.max(2);
    let (mut best, mut p3) = (usize::MAX, 1usize);
    while p3 <= n {
        let mut v = 2 * p3;
        while v < n { v *= 2; }
        best = best.min(v);
        p3 *= 3;
    }
    best
}

/// Vertex of the parabola through (-1,a), (0,b), (1,c): p = ½(a−c)/(a−2b+c).
/// |p| <= ½ when b is a local maximum. (J. O. Smith III, *Spectral Audio Signal Processing*,
/// "Quadratic Interpolation of Spectral Peaks", CCRMA.) Returns 0 if b isn't a strict maximum.
#[inline]
pub fn parabolic_offset(a: f32, b: f32, c: f32) -> f32 {
    let d = a - 2.0 * b + c;
    if d < 0.0 { (0.5 * (a - c) / d).clamp(-0.5, 0.5) } else { 0.0 }
}
```

### 3.2 GCC-PHAT (Knapp and Carter 1976), with PHAT-β (Donohue et al. 2007)
**Maths** (rustfft's DFT sign, 1.4; M = FFT length):
1. **Correlation:**
   - Let `c[τ] = Σ_n x[n]·y[n+τ]`. If `y[n] = x[n−D]`, then `c[τ] = Σ_n x[n]·x[n+τ−D]` peaks at
     **τ = D (y lags x by D)**.
   - With `X = DFT_M(x)` and `Y = DFT_M(y)`, the unnormalised inverse of `G[k] = conj(X[k])·Y[k]`
     is `M·Σ_n x[n]·y[(n+τ) mod M]`. Substitute and use `Σ_k e^{j2πk(n−m+τ)/M} = M·δ`.
   - Zero-padding both signals of length N to **M = 2N (≥ 2N−1)** makes the circular result equal
     the linear one for |τ| ≤ N−1.
   - Lag τ ≥ 0 sits at index τ; lag τ < 0 sits at index M+τ.
2. **Weighting (GCC):**
   - The weighted correlation is `IDFT(Ψ·G)`. PHAT uses `Ψ = 1/|G|`, which keeps only phase, so a
     pure delay gives a near-impulse.
   - PHAT-β uses `Ψ = 1/|G|^β`: β = 1 is PHAT, β = 0 is plain cross-correlation, and #13 picks
     β ≈ 0.7-0.8 for speech.
   - We add a relative floor, `|G|^β + ε·max|G|^β`, so empty bins don't amplify noise.
3. **Confidence:**
   - With `H = Ψ·G`, the triangle inequality gives |IDFT(H)[τ]| ≤ Σ over all M bins of |H[k]|.
     That sum is `|H₀| + |H_{M/2}| + 2·Σ_{k=1}^{M/2−1}|H_k|` (Hermitian).
   - So `confidence = |peak| / Σ|H|` lies in [0, 1]. It is 1 when every in-band bin agrees on one
     delay.
4. **Sub-sample:** parabolic interpolation on the peak and its two neighbours.
   - It is biased toward the integer for sinc-like (PHAT) peaks; see Céspedes et al., Ultrasonic
     Imaging 17 (1995) 142-171.
   - If you need better than about a tenth of a sample, zero-pad H to L·M before the inverse
     (upsampling), at an L-times cost.

```rust
#[derive(Clone, Copy, Debug)]
pub struct DelayEstimate {
    pub delay: f64,            // sub-sample; > 0: y lags x
    pub lag: isize,            // integer peak lag
    pub confidence: f32,       // |peak| / Σ|H|, 0..=1
    pub peak_to_sidelobe: f32, // |peak| / max |c| outside ±guard lags (search window only)
    pub inverted: bool,        // negative peak (polarity flip)
}

pub struct GccPhat {
    n: usize, m: usize,
    r2c: Arc<dyn RealToComplex<f32>>, c2r: Arc<dyn ComplexToReal<f32>>,
    xt: Vec<f32>, yt: Vec<f32>,                 // m, zero-padded time signals (r2c trashes them)
    xs: Vec<Complex<f32>>, ys: Vec<Complex<f32>>, // m/2+1
    corr: Vec<f32>,                             // m
    scratch: Vec<Complex<f32>>,
    k_lo: usize, k_hi: usize, beta: f32, rel_floor: f32,
}

impl GccPhat {
    /// Setup (allocates). n = max signal length, m = 2n. band_hz e.g. (100.0, 8000.0) for speech.
    pub fn new(planner: &mut RealFftPlanner<f32>, n: usize, fs: f32, band_hz: (f32, f32), beta: f32) -> Self {
        assert!(n >= 2 && (0.0..=1.0).contains(&beta));
        let m = 2 * n;
        let half = m / 2;
        let r2c = planner.plan_fft_forward(m);
        let c2r = planner.plan_fft_inverse(m);
        let scratch_len = r2c.get_scratch_len().max(c2r.get_scratch_len());
        let bin_hz = fs / m as f32;
        Self {
            n, m,
            xt: r2c.make_input_vec(), yt: r2c.make_input_vec(),
            xs: r2c.make_output_vec(), ys: r2c.make_output_vec(),
            corr: c2r.make_output_vec(),
            scratch: vec![Complex::new(0.0, 0.0); scratch_len],
            k_lo: ((band_hz.0 / bin_hz).ceil() as usize).min(half),
            k_hi: ((band_hz.1 / bin_hz).floor() as usize).min(half),
            r2c, c2r, beta, rel_floor: 1e-6,
        }
    }

    /// Allocation-free. x, y: <= n samples each. Lags searched: min_lag..=max_lag, |lag| <= n-1.
    /// Ok(None) if the band holds no energy.
    pub fn estimate(&mut self, x: &[f32], y: &[f32], min_lag: isize, max_lag: isize, guard: usize)
        -> Result<Option<DelayEstimate>, FftError>
    {
        let (n, m) = (self.n, self.m);
        let half = m / 2;
        assert!(x.len() <= n && y.len() <= n);
        assert!(-(n as isize - 1) <= min_lag && min_lag <= max_lag && max_lag <= n as isize - 1);

        self.xt[..x.len()].copy_from_slice(x); self.xt[x.len()..].fill(0.0);
        self.yt[..y.len()].copy_from_slice(y); self.yt[y.len()..].fill(0.0);
        self.r2c.process_with_scratch(&mut self.xt, &mut self.xs, &mut self.scratch)?;
        self.r2c.process_with_scratch(&mut self.yt, &mut self.ys, &mut self.scratch)?;

        // G = conj(X)·Y in band, 0 outside (stored in ys).
        let mut gmax2 = 0.0f32;
        for k in 0..=half {
            let g = if (self.k_lo..=self.k_hi).contains(&k) { self.xs[k].conj() * self.ys[k] }
                    else { Complex::new(0.0, 0.0) };
            gmax2 = gmax2.max(g.norm_sqr());
            self.ys[k] = g;
        }
        if gmax2.is_nan() || gmax2 <= 0.0 { return Ok(None); }

        // H = G / (|G|^β + floor); accumulate Σ|H| over the full Hermitian spectrum.
        let hb = 0.5 * self.beta;
        let floor = self.rel_floor * gmax2.powf(hb);
        let mut sum_abs = 0.0f64;
        for k in 0..=half {
            let g = self.ys[k];
            let mag2 = g.norm_sqr();
            let mag_b = if self.beta == 1.0 { mag2.sqrt() } else { mag2.powf(hb) };
            let h = g * (1.0 / (mag_b + floor));
            let mult = if k == 0 || k == half { 1.0 } else { 2.0 };
            sum_abs += mult * f64::from(h.norm_sqr().sqrt());
            self.ys[k] = h;
        }
        self.ys[0].im = 0.0;          // c2r demands real DC and Nyquist bins (1.4)
        self.ys[half].im = 0.0;
        self.c2r.process_with_scratch(&mut self.ys, &mut self.corr, &mut self.scratch)?;

        let idx = move |lag: isize| if lag >= 0 { lag as usize } else { (m as isize + lag) as usize };
        let (mut best_lag, mut best) = (min_lag, -1.0f32);
        for lag in min_lag..=max_lag {
            let v = self.corr[idx(lag)].abs();
            if v > best { best = v; best_lag = lag; }
        }
        let mut side = 0.0f32;
        for lag in min_lag..=max_lag {
            if lag.abs_diff(best_lag) > guard { side = side.max(self.corr[idx(lag)].abs()); }
        }
        let i = idx(best_lag);
        let s = if self.corr[i] < 0.0 { -1.0 } else { 1.0 };
        let d = parabolic_offset(s * self.corr[(i + m - 1) % m], s * self.corr[i], s * self.corr[(i + 1) % m]);
        Ok(Some(DelayEstimate {
            delay: best_lag as f64 + f64::from(d),
            lag: best_lag,
            confidence: (f64::from(best) / sum_abs) as f32,
            peak_to_sidelobe: if side > 0.0 { best / side } else { f32::INFINITY },
            inverted: s < 0.0,
        }))
    }
}
```
- **Cost per call:** two r2c and one c2r of length M.
- **A fixed `x` (a known probe)** can have its spectrum cached to save one r2c. That's a variant;
  it isn't written here.
- **Memory:** about 20·M bytes of buffers plus the plans. With n = 48 000 (1 s), M = 96 000
  (2^8·3·5^3).

### 3.3 Exponential sine sweep and Farina inverse filter
Farina, "Simultaneous measurement of impulse response and distortion with a swept-sine technique",
AES 108th Convention, Paris 2000, preprint 5093. Fades follow Müller and Massarani, "Transfer-Function
Measurement with Sweeps", JAES 49(6) 2001.
- **Sweep:** `x(t) = sin(ω₁·L·(e^{t/L} − 1))` for 0 ≤ t ≤ T, with **L = T / ln(ω₂/ω₁)**.
  - The instantaneous frequency is `ω₁·e^{t/L}`, which reaches ω₂ at t = T.
  - The time spent per Hz is ∝ 1/f, so the energy spectrum is pink (−3 dB/octave).
- **Inverse filter:** `f(t) = x(T − t)·e^{−t/L}`.
  - At reversed time t, the instantaneous frequency is `ω(T−t) = ω₁e^{(T−t)/L}`, and the gain is
    `e^{−t/L} = ω(T−t)/ω₂`. That is +6 dB/octave in amplitude, from 0 dB at f₂ down to f₁/f₂.
  - Combined with the time reversal (phase), `x ⊛ f` is a band-limited impulse at t ≈ T, index
    N−1. We calibrate the exact position numerically.
- **Harmonics:** the k-th harmonic's IR comes **L·ln k earlier** than the linear IR (Farina 2000),
  so it separates out in time.
- **Precision:** the phase reaches `ω₂·L` (≈ 9.1·10⁴ rad for 20 Hz-20 kHz over 5 s), so compute it
  in **f64**. An f32 ULP there is about 0.008 rad.
- **Option:** for phase-synchronised harmonics, use Novak et al., "Synchronized Swept-Sine", JAES
  63(10) 2015, with L rounded so that f₁·L is an integer. It isn't needed for delay.
- **Deconvolution:** `h = y ⊛ f` as a **linear** convolution, with FFT length
  M ≥ len(y) + len(f) − 1. A path with delay D peaks at `ref_pos + D`, where `ref_pos` is the
  peak of `sweep ⊛ f` itself. That self-calibration also normalises the gain to 1.

```rust
pub struct Ess { pub sweep: Vec<f32>, pub inverse: Vec<f32>, pub l_s: f64 }

/// f2 < fs/2. Raised-cosine fades (fade_*_s may be 0). amp e.g. 0.5 (−6 dBFS).
pub fn exp_sine_sweep(fs: f64, f1: f64, f2: f64, t_s: f64, fade_in_s: f64, fade_out_s: f64, amp: f32) -> Ess {
    use std::f64::consts::PI;
    assert!(f1 > 0.0 && f1 < f2 && f2 < 0.5 * fs && t_s > 0.0);
    let n = (t_s * fs).round() as usize;
    let l = t_s / (f2 / f1).ln();                       // L = T / ln(f2/f1), seconds
    let k = 2.0 * PI * f1 * l;                          // ω1·L
    let (n_in, n_out) = ((fade_in_s * fs).round() as usize, (fade_out_s * fs).round() as usize);
    let hann = |i: usize, len: usize| 0.5 - 0.5 * (PI * i as f64 / len as f64).cos();
    let sweep: Vec<f32> = (0..n).map(|i| {
        let t = i as f64 / fs;
        let mut v = (k * ((t / l).exp() - 1.0)).sin();  // f64 phase
        if i < n_in { v *= hann(i, n_in); }
        if n - 1 - i < n_out { v *= hann(n - 1 - i, n_out); }
        amp * v as f32
    }).collect();
    // f[i] = x[N-1-i] · e^{-i/(fs·L)}   (Farina inverse; 0 dB at f2 down to f1/f2 at f1)
    let inverse = (0..n).map(|i| sweep[n - 1 - i] * (-(i as f64) / (fs * l)).exp() as f32).collect();
    Ess { sweep, inverse, l_s: l }
}

#[derive(Clone, Copy, Debug)]
pub struct Arrival { pub delay: f64, pub gain: f32, pub peak_to_sidelobe: f32 }

pub struct EssDeconvolver {
    r2c: Arc<dyn RealToComplex<f32>>, c2r: Arc<dyn ComplexToReal<f32>>,
    inv_spec: Vec<Complex<f32>>,   // FFT(inverse) / M / calibration peak
    time: Vec<f32>, spec: Vec<Complex<f32>>, scratch: Vec<Complex<f32>>,
    inv_len: usize, max_capture: usize, valid_len: usize,
    ref_pos: f64,                  // fractional peak index of sweep ⊛ inverse = "delay 0"
}

impl EssDeconvolver {
    /// Setup (allocates). max_capture = longest capture passed to deconvolve (sweep + max delay + tail).
    pub fn new(planner: &mut RealFftPlanner<f32>, ess: &Ess, max_capture: usize) -> Result<Self, FftError> {
        let inv_len = ess.inverse.len();
        assert!(inv_len > 0 && max_capture >= ess.sweep.len());
        let m = next_fast_len(max_capture + inv_len - 1);    // linear, not circular
        let r2c = planner.plan_fft_forward(m);
        let c2r = planner.plan_fft_inverse(m);
        let mut scratch = vec![Complex::new(0.0, 0.0); r2c.get_scratch_len().max(c2r.get_scratch_len())];
        let mut time = r2c.make_input_vec();
        let mut inv_spec = r2c.make_output_vec();
        time[..inv_len].copy_from_slice(&ess.inverse);
        r2c.process_with_scratch(&mut time, &mut inv_spec, &mut scratch)?;
        let inv_m = 1.0 / m as f32;                          // c2r(r2c(x)) = M·x
        for v in &mut inv_spec { *v = *v * inv_m; }
        let mut d = Self { spec: r2c.make_output_vec(), r2c, c2r, inv_spec, time, scratch,
                           inv_len, max_capture, valid_len: 0, ref_pos: 0.0 };
        d.deconvolve(&ess.sweep)?;                           // calibrate on the probe itself
        let (pos, val) = d.peak_in(0, d.valid_len).expect("silent sweep");
        let g = 1.0 / val;
        for v in &mut d.inv_spec { *v = *v * g; }            // identity path -> gain 1.0
        d.ref_pos = pos;
        Ok(d)
    }

    /// Allocation-free. Returns h = capture ⊛ inverse (len capture.len() + inv_len − 1).
    pub fn deconvolve(&mut self, capture: &[f32]) -> Result<&[f32], FftError> {
        let n = capture.len();
        assert!(n > 0 && n <= self.max_capture);
        self.time[..n].copy_from_slice(capture);
        self.time[n..].fill(0.0);
        self.r2c.process_with_scratch(&mut self.time, &mut self.spec, &mut self.scratch)?;
        for (s, f) in self.spec.iter_mut().zip(&self.inv_spec) { *s = *s * *f; }
        let last = self.spec.len() - 1;
        self.spec[0].im = 0.0;
        self.spec[last].im = 0.0;
        self.c2r.process_with_scratch(&mut self.spec, &mut self.time, &mut self.scratch)?;
        self.valid_len = n + self.inv_len - 1;
        Ok(&self.time[..self.valid_len])
    }

    /// Allocation-free. Arrival in the last deconvolve, delays min_delay..=max_delay samples.
    /// first_within_db = Some(6.0): first local max within 6 dB of the strongest (the direct path
    /// in a room, #13); None: the strongest.
    pub fn arrival(&self, min_delay: usize, max_delay: usize, guard: usize, first_within_db: Option<f32>) -> Option<Arrival> {
        let r0 = self.ref_pos.round() as usize;
        let (lo, hi) = (r0 + min_delay, (r0 + max_delay + 1).min(self.valid_len));
        if lo >= hi { return None; }
        let h = &self.time[..self.valid_len];
        let mut i = (lo..hi).max_by(|&a, &b| h[a].abs().total_cmp(&h[b].abs()))?;
        let best = h[i].abs();
        if best == 0.0 { return None; }
        if let Some(db) = first_within_db {
            let thr = best * 10f32.powf(-db / 20.0);
            i = (lo..hi).find(|&j| h[j].abs() >= thr).unwrap_or(i);
            while i + 1 < hi && h[i + 1].abs() >= h[i].abs() { i += 1; }
        }
        let side = (lo..hi).filter(|&j| j.abs_diff(i) > guard).map(|j| h[j].abs()).fold(0.0f32, f32::max);
        let (pos, gain) = self.refine(i);
        Some(Arrival { delay: pos - self.ref_pos, gain,
                       peak_to_sidelobe: if side > 0.0 { h[i].abs() / side } else { f32::INFINITY } })
    }

    fn peak_in(&self, lo: usize, hi: usize) -> Option<(f64, f32)> {
        let h = &self.time[..self.valid_len];
        let i = (lo..hi.min(h.len())).max_by(|&a, &b| h[a].abs().total_cmp(&h[b].abs()))?;
        if h[i] == 0.0 { None } else { Some(self.refine(i)) }
    }

    fn refine(&self, i: usize) -> (f64, f32) {
        let h = &self.time[..self.valid_len];
        let s = if h[i] < 0.0 { -1.0 } else { 1.0 };
        let d = if i > 0 && i + 1 < h.len() { parabolic_offset(s * h[i - 1], s * h[i], s * h[i + 1]) } else { 0.0 };
        (i as f64 + f64::from(d), h[i])
    }
}
```
**The virtual loop (#13, layer 2).** Capture cable A (the app's mic) as `a` and cable B (the app's
output) as `b`. Each capture's first frame has a QPC stamp, `qa0` and `qb0`, converted to seconds
with one unit (see the wasapi reference for the units).
```rust
fn app_plus_cable_b_s(d: &mut EssDeconvolver, a: &[f32], b: &[f32], qa0_s: f64, qb0_s: f64, fs: f64) -> Option<f64> {
    let max = a.len().max(b.len());
    d.deconvolve(a).ok()?; let ta = d.arrival(0, max, 32, None)?;
    d.deconvolve(b).ok()?; let tb = d.arrival(0, max, 32, None)?;
    Some((tb.delay - ta.delay) / fs + (qb0_s - qa0_s))   // ref_pos cancels
}
```
- **Gating:** gate on `gain` (about 1.0 for an untouched digital loop) and `peak_to_sidelobe`. The
  thresholds aren't decided yet.
- **Through the model:** the sweep gets removed, so use speech and `GccPhat`
  (β ≈ 0.7-0.8, 1-2 s windows, the median of several), as in #13.

---

## 4. Pitfalls (the short list)
1. **Use only `process_with_scratch` on any hot path.**
   - `process()` allocates scratch on every call, in both realfft and rustfft.
   - Planning and `make_*_vec` allocate. Do them at setup, outside the audio thread's `RtScope`.
   - Free the plans off the audio thread.
2. **Inputs are destroyed.**
   - r2c (even N) trashes the time buffer, and c2r trashes the spectrum.
   - Refill the time buffer before every forward transform. Copy a spectrum you still need.
3. **c2r needs `im == 0` at bin 0 and bin N/2.** Otherwise it returns
   `Err(InputValues)` *after* computing a correct result, so `?` discards good data. Zero
   those two imaginary parts after any spectral multiply.
4. **No normalisation anywhere:** `c2r(r2c(x)) = N·x`, forward is e^{−j}, inverse e^{+j}. Fold
   `1/N` into a filter spectrum once. Zero-pad to ≥ 2N−1 (we use 2N) or the correlation wraps.
5. **hound converts nothing and checks little on write.**
   - Reading an i16 file as `f32` gives `InvalidSampleFormat` on every sample.
   - Writing `f32` to an Int/32 spec, or `i16` to a Float spec, silently writes garbage.
   - `Float` with bits ≠ 32 panics.
   - Always call `finalize()`; Drop ignores errors.
   - The file is limited to 4 GiB (u32).
   - hound does blocking I/O and must never run on an audio thread.

Also:
- realfft's `FftError` `Debug` shows the message, not the variant.
- realfft docs call the odd-length r2c input "garbage", but the code keeps it.
- hound's `seek` is wrong for 24-in-4-byte files.
- Unknown odd-length RIFF chunks before `data` misparse in hound (no pad byte).

## 5. References
These are cited from memory, not from the crate sources. Check them before quoting them publicly.
- **Knapp and Carter**, "The generalized correlation method for estimation of time delay", IEEE
  Trans. ASSP 24(4):320-327, 1976 (GCC and PHAT).
- **Donohue, Hannemann and Dietz**, "Performance of phase transform for detecting sound sources
  with microphone arrays in reverberant and noisy environments", Signal Processing 87(7), 2007
  (PHAT-β).
- **Farina**, AES 108th Convention, 2000, preprint 5093 (ESS, the inverse filter, and harmonic
  separation at L·ln k).
- **Müller and Massarani**, JAES 49(6):443-471, 2001 (sweep fades and practice).
- **Novak et al.**, JAES 63(10), 2015 (synchronised swept-sine).
- **J. O. Smith III**, *Spectral Audio Signal Processing*, CCRMA, "Quadratic Interpolation of
  Spectral Peaks" (the parabolic peak).
- **Céspedes, Huang, Ophir and Spratt**, Ultrasonic Imaging 17:142-171, 1995 (sub-sample delay
  estimators and their bias).
