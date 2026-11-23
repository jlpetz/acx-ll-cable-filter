//! A single external CLAP post-effect operating on the engine's 480-frame mono blocks.
//! The child processes blocks in order. The engine plays them one to four blocks later, with a delayed dry copy for any miss.
mod native;
mod plugin;
mod transport;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use transport::{BLOCK, SLOTS, Session, Shared};

/// Maximum number of 10 ms blocks the microphone can deliver in a single wake while maintaining the effect's delay.
pub const MAX_DEPTH: usize = 4;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Parameter {
    pub id: u32,
    pub name: String,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    pub stepped: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EffectInfo {
    pub id: String,
    pub name: String,
    pub params: Vec<Parameter>,
}

impl EffectInfo {
    fn validate(&self) -> Result<(), String> {
        if self.id.is_empty() || self.name.is_empty() || self.params.len() > transport::PARAMS {
            return Err("Invalid CLAP effect metadata".into());
        }
        for (i, p) in self.params.iter().enumerate() {
            if p.id == u32::MAX
                || !p.min.is_finite()
                || !p.max.is_finite()
                || !p.default.is_finite()
                || !(p.min..=p.max).contains(&p.default)
                || self.params[..i].iter().any(|other| other.id == p.id)
            {
                return Err("Invalid or duplicate CLAP parameter".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum BlockStatus {
    Priming,
    Bypassed,
    Processed,
    Missed,
}

struct Instance {
    session: Session,
    info: EffectInfo,
    processed: AtomicU64,
    missed: AtomicU64,
}

#[derive(Clone)]
pub struct FxControl(Arc<Instance>);
impl FxControl {
    pub fn set_parameter(&self, id: u32, value: f64) -> bool {
        let Some((index, p)) = self.0.info.params.iter().enumerate().find(|(_, p)| p.id == id) else { return false };
        if !value.is_finite() {
            return false;
        }
        let value = if p.stepped { value.trunc() } else { value }.clamp(p.min, p.max);
        self.shared().values[index].store(value.to_bits(), Ordering::Release);
        true
    }
    pub fn processed_blocks(&self) -> u64 {
        self.0.processed.load(Ordering::Relaxed)
    }
    pub fn missed_blocks(&self) -> u64 {
        self.0.missed.load(Ordering::Relaxed)
    }
    /// Control-thread query. Observes a fatal plugin exit even between audio blocks.
    pub fn failed(&self) -> bool {
        self.shared().failed() || self.0.session.exited()
    }
    fn shared(&self) -> &Shared {
        self.0.session.shared()
    }
}

pub struct FxClient {
    control: FxControl,
    sequence: u64,
    epoch: u64,
    delay: BlockDelay,
}

/// `cursor` addresses the block due at the next callback and the slot its replacement will occupy.
struct BlockDelay {
    blocks: Box<[[f32; BLOCK]; MAX_DEPTH]>,
    depth: usize,
    cursor: usize,
    filled: usize,
}
impl BlockDelay {
    fn new() -> Self {
        Self { blocks: Box::new([[0.; BLOCK]; MAX_DEPTH]), depth: 1, cursor: 0, filled: 0 }
    }
    fn reset(&mut self) {
        self.cursor = 0;
        self.filled = 0;
    }
    fn due(&self, last_submitted: u64) -> Option<u64> {
        (self.filled == self.depth).then(|| last_submitted - (self.depth - 1) as u64)
    }
    fn advance(
        &mut self, last_submitted: u64, input: &[f32; BLOCK], out: &mut [f32; BLOCK], bypass: bool,
        read_wet: impl FnOnce(u64, &mut [f32; BLOCK]) -> bool,
    ) -> BlockStatus {
        let status = match self.due(last_submitted) {
            None => {
                self.filled += 1;
                out.fill(0.);
                BlockStatus::Priming
            }
            Some(seq) if !bypass && read_wet(seq, out) => BlockStatus::Processed,
            Some(_) => {
                *out = self.blocks[self.cursor];
                if bypass { BlockStatus::Bypassed } else { BlockStatus::Missed }
            }
        };
        self.blocks[self.cursor] = *input;
        self.cursor = (self.cursor + 1) % self.depth;
        status
    }
}

impl FxClient {
    pub fn launch(path: &Path, plugin_id: Option<&str>) -> Result<Self, String> {
        let (session, info) = Session::launch(path, plugin_id)?;
        Self::from_session(session, info)
    }
    fn from_session(session: Session, info: EffectInfo) -> Result<Self, String> {
        info.validate()?;
        let instance = Instance { session, info, processed: AtomicU64::new(0), missed: AtomicU64::new(0) };
        Ok(Self { control: FxControl(Arc::new(instance)), sequence: 0, epoch: 0, delay: BlockDelay::new() })
    }
    /// Choose the fixed delay before the first audio block, once the microphone's delivery size is known.
    pub fn set_depth(&mut self, blocks: usize) -> Result<(), String> {
        if !(1..=MAX_DEPTH).contains(&blocks) || self.sequence != 0 {
            return Err(format!("Choose an effect depth of 1 to {MAX_DEPTH} blocks before processing"));
        }
        self.delay.depth = blocks;
        self.control.shared().depth.store(blocks as u32, Ordering::Relaxed);
        Ok(())
    }
    pub fn depth(&self) -> usize {
        self.delay.depth
    }
    pub fn info(&self) -> &EffectInfo {
        &self.control.0.info
    }
    pub fn control(&self) -> FxControl {
        self.control.clone()
    }
    pub fn set_parameter(&self, id: u32, value: f64) -> bool {
        self.control.set_parameter(id, value)
    }
    pub fn reset(&mut self) {
        self.delay.reset();
        self.epoch = self.epoch.wrapping_add(1);
    }
    /// WAV processing runs faster than real time. Wait for the due block while keeping exactly the live sample delay.
    pub fn process_offline(&mut self, input: &[f32; BLOCK], output: &mut [f32; BLOCK], bypass: bool) -> Result<(), String> {
        if !bypass && let Some(due) = self.delay.due(self.sequence) {
            self.control.0.session.wait_offline(due)?;
        }
        if self.output_block(input, output, bypass) == BlockStatus::Missed {
            return Err("CLAP effect returned an invalid audio block during offline processing".into());
        }
        self.control.0.session.submit_offline(self.sequence, self.epoch, input, bypass)?;
        Ok(())
    }
    pub fn process(&mut self, input: &[f32; BLOCK], output: &mut [f32; BLOCK], bypass: bool) -> BlockStatus {
        let status = self.output_block(input, output, bypass);
        self.control.0.session.submit(self.sequence, self.epoch, false, input);
        status
    }
    fn output_block(&mut self, input: &[f32; BLOCK], output: &mut [f32; BLOCK], bypass: bool) -> BlockStatus {
        let shared = self.control.shared();
        let status =
            self.delay.advance(self.sequence, input, output, bypass, |seq, out| shared.slots[seq as usize % SLOTS].read_output(seq, out));
        match status {
            BlockStatus::Processed => {
                self.control.0.processed.fetch_add(1, Ordering::Relaxed);
            }
            BlockStatus::Missed => {
                self.control.0.missed.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        self.sequence += 1;
        status
    }
}

impl Drop for FxClient {
    fn drop(&mut self) {
        self.control.0.session.stopping();
    }
}

/// Inspecting a DLL runs its initialization in the same restricted worker used for processing.
pub fn inspect(path: &Path, plugin_id: Option<&str>) -> Result<EffectInfo, String> {
    Ok(FxClient::launch(path, plugin_id)?.info().clone())
}

/// Dispatch the internal worker mode before normal GUI/CLI startup.
pub fn worker_entry() -> Option<u8> {
    transport::worker_entry()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn dry_history_matches_the_wet_deadline_even_after_reset_or_miss() {
        for depth in 1..=MAX_DEPTH {
            let mut delay = BlockDelay::new();
            delay.depth = depth;
            let mut out = [0.; BLOCK];
            let mut last = 0;
            for _ in 0..2 {
                delay.reset();
                for i in 0..depth + 6 {
                    let bypass = i % 2 == 0;
                    let status = delay.advance(last, &[(last + 1) as f32; BLOCK], &mut out, bypass, |seq, out| {
                        assert!(!bypass);
                        assert_eq!(seq, last + 1 - depth as u64);
                        out.fill(f32::NAN); // An invalid partial response must also fall back to the correct block.
                        false
                    });
                    if i < depth {
                        assert_eq!(status, BlockStatus::Priming);
                        assert_eq!(out, [0.; BLOCK]);
                    } else {
                        assert_eq!(status, if bypass { BlockStatus::Bypassed } else { BlockStatus::Missed });
                        assert_eq!(out, [(last + 1 - depth as u64) as f32; BLOCK]);
                    }
                    last += 1;
                }
            }
        }
    }

    /// The separate DLL and worker are explicitly built by the caller. No audio device is opened.
    #[test]
    #[ignore = "build the gain .clap and nf-fx-host then set NF_FX_TEST_PLUGIN and NF_FX_TEST_HOST"]
    fn isolated_effect_lifecycle_and_crash_bypass() {
        let path = PathBuf::from(std::env::var_os("NF_FX_TEST_PLUGIN").expect("NF_FX_TEST_PLUGIN"));
        let exe = PathBuf::from(std::env::var_os("NF_FX_TEST_HOST").expect("NF_FX_TEST_HOST"));
        assert_eq!(path.extension(), Some(OsStr::new("clap")));
        let (session, info) = Session::launch_with(&exe, &path, None).unwrap();
        let mut fx = FxClient::from_session(session, info).unwrap();
        assert_eq!(fx.depth(), 1);
        assert!(fx.set_depth(0).is_err());
        assert!(fx.set_depth(MAX_DEPTH + 1).is_err());
        fx.set_depth(MAX_DEPTH).unwrap();
        assert_eq!(fx.info().name, "Gain");
        let control = fx.control();
        assert_eq!(fx.info().params[0].id, 0);
        assert!(control.set_parameter(0, 0.5));
        assert!(!control.set_parameter(42, 0.1));
        assert!(!control.set_parameter(0, f64::NAN));
        let mut y = [0.; 480];
        for i in 0..50 {
            fx.process_offline(&[i as f32; 480], &mut y, false).unwrap();
            let expected = (i as f32 - MAX_DEPTH as f32).max(0.) * 0.5;
            assert_eq!(y, [expected; 480]);
        }
        assert!(fx.set_depth(1).is_err());
        assert!(control.set_parameter(0, 0.25));
        fx.process_offline(&[20.; 480], &mut y, true).unwrap();
        assert_eq!(y, [(50 - MAX_DEPTH) as f32; 480]);
        for _ in 1..MAX_DEPTH {
            fx.process_offline(&[16.; 480], &mut y, false).unwrap();
        }
        fx.process_offline(&[16.; 480], &mut y, false).unwrap();
        assert_eq!(y, [5.; 480]);
        fx.reset();
        for _ in 0..MAX_DEPTH {
            fx.process_offline(&[12.; 480], &mut y, false).unwrap();
            assert_eq!(y, [0.; 480]);
        }
        fx.process_offline(&[8.; 480], &mut y, false).unwrap();
        assert_eq!(y, [3.; 480]);
        fx.control.0.session.process.terminate_test_worker();
        assert!(control.failed());
        // Drain the in-flight responses that may already have arrived when the worker was terminated.
        for _ in 0..MAX_DEPTH {
            fx.process(&[9.; 480], &mut y, false);
        }
        assert_eq!(fx.process(&[10.; 480], &mut y, false), BlockStatus::Missed);
        assert_eq!(y, [9.; 480]);
        assert_eq!(fx.process(&[11.; 480], &mut y, false), BlockStatus::Missed);
        assert_eq!(y, [9.; 480]);
        assert!(fx.process_offline(&[12.; 480], &mut y, false).is_err());
        fx.process_offline(&[12.; 480], &mut y, true).unwrap();
        assert_eq!(y, [9.; 480]);
        println!(
            "Restricted worker passed 50 offline blocks, parameter and bypass changes, reset, process termination and delayed dry output."
        );
    }
}
