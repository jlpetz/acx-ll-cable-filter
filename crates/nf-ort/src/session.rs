//! A pack's ONNX Runtime session with its I/O preallocated (TODO 6): what one `run` costs is ONNX Runtime's own work, and
//! the Rust side of a run touches no heap.
//!
//! - **Options** ([`SessionOptions::default`]): one intra-op and one inter-op thread, sequential, no spinning; the memory
//!   pattern on; Level 3 graph optimisation (the bench compares 1, 2 and 3); deterministic compute; no environment
//!   execution providers; session log level Error. No execution provider is added: `ep::CPU::default()` turns the CPU arena
//!   *off*, and leaving it out keeps it on. The variants (arena off, pattern off, outputs not preallocated) exist for TODO
//!   6's measurements.
//! - **No flush-to-zero.** `with_flush_to_zero` (`session.set_denormal_as_zero`) doesn't only reach pool threads: ONNX
//!   Runtime applies it once per process, to the thread that builds the first session, and leaves FTZ and DAZ set in its
//!   MXCSR for good (the verifier's check, 2026-11-02: 0x1f80 to 0x9fe0 on the first session's thread, a later session's
//!   untouched). Inline (`process`, `bench`, tests) that is the thread that runs AEC3 and the floor too, against the
//!   project's rule (no FTZ/DAZ: a persistent MXCSR change is UB in Rust; audio.rs), and live only the first worker after
//!   a process start would have it. It bought nothing measurable for UL-UNAS (TODO 6's detail), so it's off; a test checks
//!   that building and warming up a session leaves the calling thread's MXCSR as it was. A model that needs it would set
//!   and restore MXCSR on its worker alone, as part of the worker exception.
//! - **Inputs:** owned tensors made once, written in place before each run, passed by position in the session's input order
//!   (`ort::inputs!`; a `ValueMap` would allocate). UL-UNAS's 4 inputs, 4 outputs and 89 bytes of names fit ort's stack
//!   buffers (6 inputs, 4 outputs, 768 bytes), so `run_with_options` doesn't allocate on the Rust side.
//! - **Outputs:** one `RunOptions` with every output preallocated (`OutputSelector::preallocate`), read by index after the
//!   run. The state outputs are then copied into the state inputs (UL-UNAS: 6816 floats, about 27 KB). `IoBinding` isn't
//!   used: `run_binding` allocates an array and wrappers every call.
//! - **Load-time checks:** the manifest's names, dtypes and shapes against the graph's declared inputs and outputs, and
//!   every state pair present; nothing else in or out.
//! - The session is built from the pack's verified bytes, and only after [`crate::onnx_check::refuse_external_data`] has
//!   found no tensor whose data lives in another file (ONNX Runtime would resolve one against the current folder). No op
//!   library is registered, so a custom-op domain fails here.

use ort::ep;
use ort::logging::LogLevel;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::{HasSelectedOutputs, NoSelectedOutputs, OutputSelector, RunOptions, Session};
use ort::value::{Tensor, TensorElementType, ValueType};

use crate::pack::Pack;

/// How the session is built (see the module notes; the defaults are the app's).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionOptions {
    /// Graph optimisation: 0 (none) to 3.
    pub opt_level: u8,
    pub memory_pattern: bool,
    /// The CPU arena (on unless asked: off needs `ep::CPU` with the arena off).
    pub arena: bool,
    /// Every output preallocated, so ort makes no output values per run.
    pub preallocate_outputs: bool,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self { opt_level: 3, memory_pattern: true, arena: true, preallocate_outputs: true }
    }
}

enum Runs {
    Preallocated(RunOptions<HasSelectedOutputs>),
    Plain(RunOptions<NoSelectedOutputs>),
}

/// A session over a pack's model, its input tensors made and its outputs preallocated.
pub struct ModelSession {
    session: Session,
    /// In the session's input order.
    inputs: Vec<Tensor<f32>>,
    runs: Runs,
    mix_in: usize,
    enh_out: usize,
    /// (session input, session output) per state pair.
    state: Vec<(usize, usize)>,
    /// The last run's model output (`enh`).
    enh: Box<[f32]>,
}

fn dims(shape: &[i64]) -> Result<Vec<usize>, String> {
    shape.iter().map(|&d| usize::try_from(d).map_err(|_| format!("a negative dimension in {shape:?}"))).collect()
}

fn zeros(shape: &[i64]) -> Result<Tensor<f32>, String> {
    let d = dims(shape)?;
    let n = d.iter().product::<usize>();
    Tensor::from_array((d, vec![0.0f32; n].into_boxed_slice())).map_err(|e| e.to_string())
}

/// `outlet`'s dtype and shape against the manifest's.
fn matches(name: &str, dtype: &ValueType, shape: &[i64]) -> Result<(), String> {
    match dtype {
        ValueType::Tensor { ty: TensorElementType::Float32, shape: s, .. } if s[..] == *shape => Ok(()),
        other => Err(format!("the graph's {name} is {other}, the manifest says float32 {shape:?}")),
    }
}

impl ModelSession {
    /// Builds the session on the calling thread (the worker: ONNX Runtime keeps per-thread buffers) and checks its signature.
    pub fn new(pack: &Pack, opts: SessionOptions) -> Result<Self, String> {
        let env = crate::env::env()?;
        let level = match opts.opt_level {
            0 => GraphOptimizationLevel::Disable,
            1 => GraphOptimizationLevel::Level1,
            2 => GraphOptimizationLevel::Level2,
            _ => GraphOptimizationLevel::Level3,
        };
        let e = |e: ort::Error| format!("{}: ONNX Runtime: {e}", pack.manifest.id);
        let mut b = Session::builder(env)
            .map_err(e)?
            .with_intra_threads(1)
            .map_err(|x| e(x.into()))?
            .with_inter_threads(1)
            .map_err(|x| e(x.into()))?
            .with_parallel_execution(false)
            .map_err(|x| e(x.into()))?
            .with_intra_op_spinning(false)
            .map_err(|x| e(x.into()))?
            .with_inter_op_spinning(false)
            .map_err(|x| e(x.into()))?
            .with_memory_pattern(opts.memory_pattern)
            .map_err(|x| e(x.into()))?
            .with_optimization_level(level)
            .map_err(|x| e(x.into()))?
            .with_deterministic_compute(true)
            .map_err(|x| e(x.into()))?
            .with_no_environment_execution_providers()
            .map_err(|x| e(x.into()))?
            .with_log_level(LogLevel::Error)
            .map_err(|x| e(x.into()))?;
        if !opts.arena {
            b = b.with_execution_providers([ep::CPU::default().with_arena_allocator(false).build()]).map_err(|x| e(x.into()))?;
        }
        crate::onnx_check::refuse_external_data(&pack.model_bytes).map_err(|why| format!("{}: {why}", pack.manifest.id))?;
        let session = b.commit_from_memory(&pack.model_bytes).map_err(e)?;
        Self::with_session(pack, session, opts)
    }

    fn with_session(pack: &Pack, session: Session, opts: SessionOptions) -> Result<Self, String> {
        let m = &pack.manifest;
        let id = &m.id;
        let input_index = |name: &str| session.inputs().iter().position(|o| o.name() == name);
        let output_index = |name: &str| session.outputs().iter().position(|o| o.name() == name);
        let missing = |what: &str, name: &str| format!("{id}: the graph has no {what} named {name:?}");
        // Every graph input and output must be the manifest's.
        let mut shapes: Vec<Option<Vec<i64>>> = vec![None; session.inputs().len()];
        let mix_in = input_index(&m.io.input.name).ok_or_else(|| missing("input", &m.io.input.name))?;
        shapes[mix_in] = Some(m.io.input.shape.clone());
        let enh_out = output_index(&m.io.output.name).ok_or_else(|| missing("output", &m.io.output.name))?;
        let mut out_shapes: Vec<Option<Vec<i64>>> = vec![None; session.outputs().len()];
        out_shapes[enh_out] = Some(m.io.output.shape.clone());
        let mut state = Vec::with_capacity(m.state.len());
        for p in &m.state {
            let i = input_index(&p.input).ok_or_else(|| missing("state input", &p.input))?;
            let o = output_index(&p.output).ok_or_else(|| missing("state output", &p.output))?;
            if shapes[i].is_some() || out_shapes[o].is_some() {
                return Err(format!("{id}: {} or {} is listed twice", p.input, p.output));
            }
            shapes[i] = Some(p.shape.clone());
            out_shapes[o] = Some(p.shape.clone());
            state.push((i, o));
        }
        for (outlets, list, what) in [(session.inputs(), &shapes, "input"), (session.outputs(), &out_shapes, "output")] {
            for (outlet, shape) in outlets.iter().zip(list.iter()) {
                let shape = shape.as_ref().ok_or_else(|| format!("{id}: the graph's {what} {:?} isn't in the manifest", outlet.name()))?;
                matches(outlet.name(), outlet.dtype(), shape).map_err(|e| format!("{id}: {e}"))?;
            }
        }
        let inputs = shapes.iter().map(|s| zeros(s.as_deref().unwrap_or(&[]))).collect::<Result<Vec<_>, _>>()?;
        let ort_err = |e: ort::Error| format!("{id}: {e}");
        let runs = if opts.preallocate_outputs {
            let mut sel = OutputSelector::default();
            for (outlet, shape) in session.outputs().iter().zip(&out_shapes) {
                sel = sel.preallocate(outlet.name(), zeros(shape.as_deref().unwrap_or(&[]))?);
            }
            Runs::Preallocated(RunOptions::new().map_err(ort_err)?.with_outputs(sel))
        } else {
            Runs::Plain(RunOptions::new().map_err(ort_err)?)
        };
        let enh_len = dims(&m.io.output.shape)?.iter().product::<usize>();
        Ok(Self { session, inputs, runs, mix_in, enh_out, state, enh: vec![0.0; enh_len].into_boxed_slice() })
    }

    /// The model's input frame, to write before [`ModelSession::run`].
    pub fn mix_mut(&mut self) -> &mut [f32] {
        self.inputs[self.mix_in].extract_tensor_mut().1
    }

    /// The model's input frame as last written.
    pub fn mix(&self) -> &[f32] {
        self.inputs[self.mix_in].extract_tensor().1
    }

    /// The last run's output frame.
    pub fn enh(&self) -> &[f32] {
        &self.enh
    }

    /// The session (its inputs and outputs, for a test's plain run).
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Runs one frame: the model's output into [`ModelSession::enh`], the state outputs into the state inputs.
    #[inline]
    pub fn run(&mut self) -> Result<(), String> {
        let inputs = &self.inputs;
        let session = &mut self.session;
        // UL-UNAS has four inputs; the macro takes a fixed count, so the general case goes through a slice view of the
        // same values (SessionInputs::ValueSlice: positional, no allocation).
        let values: [&Tensor<f32>; 8] = std::array::from_fn(|i| &inputs[i.min(inputs.len() - 1)]);
        let n = inputs.len();
        macro_rules! run {
            ($opts:expr) => {
                match n {
                    1 => session.run_with_options(ort::inputs![values[0]], $opts),
                    2 => session.run_with_options(ort::inputs![values[0], values[1]], $opts),
                    3 => session.run_with_options(ort::inputs![values[0], values[1], values[2]], $opts),
                    4 => session.run_with_options(ort::inputs![values[0], values[1], values[2], values[3]], $opts),
                    5 => session.run_with_options(ort::inputs![values[0], values[1], values[2], values[3], values[4]], $opts),
                    6 => session.run_with_options(ort::inputs![values[0], values[1], values[2], values[3], values[4], values[5]], $opts),
                    _ => return Err(format!("{n} inputs: the host takes up to 6 (ort's stack buffers)")),
                }
            };
        }
        let outputs = match &self.runs {
            Runs::Preallocated(o) => run!(o),
            Runs::Plain(o) => run!(o),
        }
        .map_err(|e| e.to_string())?;
        let (_, enh) = outputs[self.enh_out].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
        let k = enh.len().min(self.enh.len());
        self.enh[..k].copy_from_slice(&enh[..k]);
        for &(i, o) in &self.state {
            let (_, src) = outputs[o].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
            let dst = self.inputs[i].extract_tensor_mut().1;
            let k = src.len().min(dst.len());
            dst[..k].copy_from_slice(&src[..k]);
        }
        Ok(())
    }

    /// Zeros the state inputs (each state pair's `init`), the input frame and the last output.
    pub fn reset(&mut self) {
        for t in &mut self.inputs {
            t.extract_tensor_mut().1.fill(0.0);
        }
        self.enh.fill(0.0);
    }

    /// The state inputs' total length, in floats.
    pub fn state_len(&self) -> usize {
        self.state.iter().map(|&(i, _)| self.inputs[i].extract_tensor().1.len()).sum()
    }
}
