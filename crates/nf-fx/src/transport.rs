//! The mapped section contains only bounded atomic integers, including the wire representation of floats and UTF-8.
//! Never expose Rust pointers, references, enum discriminants, or unvalidated lengths in the parent from a worker process.
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::*;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::native::{Mapping, Process, Wake};
use crate::plugin::Plugin;
use crate::{EffectInfo, MAX_DEPTH};
pub(crate) const BLOCK: usize = 480;
pub(crate) const PARAMS: usize = 128;
pub(crate) const SLOTS: usize = MAX_DEPTH + 1;
const META: usize = 65536;
const WRITING: u32 = 1;
const INPUT: u32 = 2;
const WORKING: u32 = 3;
const READY: u32 = 4;
const HELPER_READY: u32 = 1;
const HELPER_FAILED: u32 = 2;

#[repr(C, align(64))]
pub(crate) struct Slot {
    state: AtomicU32,
    sequence: AtomicU64,
    epoch: AtomicU64,
    notify: AtomicU32,
    audio: [AtomicU32; BLOCK],
}
impl Slot {
    pub(crate) fn submit(&self, seq: u64, epoch: u64, notify: bool, data: &[f32; BLOCK]) -> bool {
        let state = self.state.load(Acquire);
        if state == WORKING || self.state.compare_exchange(state, WRITING, AcqRel, Relaxed).is_err() {
            return false;
        }
        for (dst, src) in self.audio.iter().zip(data) {
            dst.store(src.to_bits(), Relaxed)
        }
        self.sequence.store(seq, Relaxed);
        self.epoch.store(epoch, Relaxed);
        self.notify.store(u32::from(notify), Relaxed);
        self.state.store(INPUT, Release);
        true
    }
    pub(crate) fn available(&self, seq: u64) -> bool {
        self.state.load(Acquire) == READY && self.sequence.load(Relaxed) == seq
    }
    pub(crate) fn read_output(&self, seq: u64, out: &mut [f32; BLOCK]) -> bool {
        if !self.available(seq) {
            return false;
        }
        for (dst, src) in out.iter_mut().zip(&self.audio) {
            *dst = f32::from_bits(src.load(Relaxed));
            if !dst.is_finite() {
                return false;
            }
        }
        true
    }
    fn take(&self, seq: u64, out: &mut [f32; BLOCK]) -> bool {
        if self.state.compare_exchange(INPUT, WORKING, AcqRel, Relaxed).is_err() {
            return false;
        }
        // A new request may have replaced this slot before the worker acquired it. Leave that request runnable.
        if self.sequence.load(Relaxed) != seq {
            self.state.store(INPUT, Release);
            return false;
        }
        for (dst, src) in out.iter_mut().zip(&self.audio) {
            *dst = f32::from_bits(src.load(Relaxed));
        }
        true
    }
    fn finish(&self, data: &[f32; BLOCK]) {
        for (dst, src) in self.audio.iter().zip(data) {
            dst.store(src.to_bits(), Relaxed);
        }
        self.state.store(READY, Release);
    }
}

#[repr(C)]
pub(crate) struct Shared {
    status: AtomicU32,
    pub stop: AtomicU32,
    pub latest: AtomicU64,
    pub depth: AtomicU32,
    pub slots: [Slot; SLOTS],
    pub values: [AtomicU64; PARAMS],
    meta_len: AtomicU32,
    meta: [AtomicU8; META],
}
impl Shared {
    fn write(&self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() > META {
            return Err("Effect metadata exceeds 64 KiB".into());
        }
        for (dst, b) in self.meta.iter().zip(bytes) {
            dst.store(*b, Relaxed);
        }
        self.meta_len.store(bytes.len() as u32, Release);
        Ok(())
    }
    fn read(&self) -> Result<Vec<u8>, String> {
        let count = self.meta_len.load(Acquire) as usize;
        let data = self.meta.get(..count).ok_or("Invalid CLAP helper response length")?;
        Ok(data.iter().map(|byte| byte.load(Relaxed)).collect())
    }
    pub fn failed(&self) -> bool {
        self.status.load(Acquire) == HELPER_FAILED
    }
    fn fail(&self, error: &str) {
        let bytes = error.as_bytes();
        let _ = self.write(&bytes[..bytes.len().min(META)]);
        self.status.store(HELPER_FAILED, Release);
    }
}

#[derive(Serialize, Deserialize)]
struct Request {
    path: PathBuf,
    plugin_id: Option<String>,
}

pub(crate) struct Session {
    map: Mapping,
    wake: Wake,
    done: Wake,
    pub(super) process: Process,
}
impl Session {
    pub fn shared(&self) -> &Shared {
        self.map.get()
    }
    pub fn signal(&self) {
        self.wake.signal();
    }
    pub fn exited(&self) -> bool {
        self.process.exited()
    }
    pub fn stopping(&self) {
        self.shared().stop.store(1, Release);
        self.signal();
        self.process.stopping();
    }
    pub fn submit(&self, seq: u64, epoch: u64, notify: bool, input: &[f32; BLOCK]) -> bool {
        let shared = self.shared();
        if shared.failed() || !shared.slots[seq as usize % SLOTS].submit(seq, epoch, notify, input) {
            return false;
        }
        shared.latest.store(seq, Release);
        // SetEvent signals the worker without waiting for it. All sample buffers and handles exist before processing starts.
        self.signal();
        true
    }
    /// Rapidly priming an offline stream after reset can wrap a slot still owned by the preceding epoch's processor.
    pub fn submit_offline(&self, seq: u64, epoch: u64, input: &[f32; BLOCK], bypass: bool) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !self.submit(seq, epoch, true, input) {
            if self.shared().failed() || self.exited() {
                return if bypass { Ok(()) } else { Err("CLAP effect exited before accepting the offline block".into()) };
            }
            if Instant::now() >= deadline {
                return if bypass { Ok(()) } else { Err("CLAP effect timed out accepting the offline block".into()) };
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }
    /// The WAV caller may wait. Always check the sequence after waking; an auto-reset event can remain signaled
    /// by a prior block, and a completed response can be available before entering this function.
    pub fn wait_offline(&self, seq: u64) -> Result<(), String> {
        let shared = self.shared();
        let slot = &shared.slots[seq as usize % SLOTS];
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if slot.available(seq) {
                break Ok(());
            }
            if shared.failed() {
                break Err("CLAP effect failed during offline processing".into());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break Err("CLAP effect timed out during offline processing".into());
            }
            if slot.notify.load(Relaxed) == 0 {
                // Only a caller changing a single client from live to offline can encounter a submitted live block.
                // Live blocks don't signal completion; poll once per millisecond for this handover.
                if self.exited() {
                    break Err("CLAP effect exited during offline processing".into());
                }
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            if let Err(e) = self.process.wait_for_response(&self.done, remaining.as_millis().max(1) as u32) {
                break Err(e);
            }
        }
    }
    pub fn launch(path: &Path, plugin_id: Option<&str>) -> Result<(Self, EffectInfo), String> {
        let mut exe = std::env::current_exe().map_err(|e| e.to_string())?;
        if !exe.file_stem().is_some_and(|n| n.eq_ignore_ascii_case("noise-filter")) {
            exe = exe.parent().ok_or("No executable directory")?.join("nf-fx-host.exe");
        }
        Self::launch_with(&exe, path, plugin_id)
    }
    pub(crate) fn launch_with(exe: &Path, path: &Path, plugin_id: Option<&str>) -> Result<(Self, EffectInfo), String> {
        let path = std::fs::canonicalize(path).map_err(|e| format!("Open effect {}: {e}", path.display()))?;
        let request = Request { path, plugin_id: plugin_id.map(str::to_owned) };
        let map = Mapping::new()?;
        map.get().depth.store(1, Relaxed);
        let wake = Wake::new()?;
        let done = Wake::new()?;
        map.get().write(&serde_json::to_vec(&request).map_err(|e| e.to_string())?)?;
        let process = Process::start(exe, &map, &wake, &done)?;
        let session = Self { map, wake, done, process };
        let shared = session.shared();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match shared.status.load(Acquire) {
                HELPER_READY => {
                    let info = serde_json::from_slice(&shared.read()?).map_err(|e| format!("Invalid CLAP metadata: {e}"))?;
                    return Ok((session, info));
                }
                HELPER_FAILED => return Err(String::from_utf8_lossy(&shared.read()?).into_owned()),
                _ => {}
            }
            if session.exited() {
                return Err("The isolated CLAP helper exited during initialization".into());
            }
            if Instant::now() >= deadline {
                return Err("The isolated CLAP helper timed out during initialization".into());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.stopping();
    }
}

/// Auto-reset events coalesce when one microphone wake submits several blocks. Take the oldest request still due for playback.
fn next_input<'a>(slots: &'a [Slot; SLOTS], last: u64, latest: u64, depth: usize, input: &mut [f32; BLOCK]) -> Option<(u64, &'a Slot)> {
    if latest <= last {
        return None;
    }
    let first = (last + 1).max(latest.saturating_sub(depth.clamp(1, MAX_DEPTH) as u64 - 1));
    (first..=latest).find_map(|seq| {
        let slot = &slots[seq as usize % SLOTS];
        slot.take(seq, input).then_some((seq, slot))
    })
}

pub(crate) fn worker_entry() -> Option<u8> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--nf-fx-worker")) {
        return None;
    }
    let handles: Vec<usize> = args.map(|s| s.to_str().and_then(|s| s.parse().ok()).unwrap_or(0)).collect();
    if handles.len() != 3 || handles.contains(&0) || handles[0] == handles[1] || handles[0] == handles[2] || handles[1] == handles[2] {
        return Some(2);
    }
    let Ok(map) = Mapping::map(unsafe { OwnedHandle::from_raw_handle(handles[0] as _) }) else { return Some(2) };
    let wake = Wake(unsafe { OwnedHandle::from_raw_handle(handles[1] as _) });
    let done = Wake(unsafe { OwnedHandle::from_raw_handle(handles[2] as _) });
    match work(map.get(), wake, done) {
        Ok(()) => Some(0),
        Err(e) => {
            map.get().fail(&e);
            Some(1)
        }
    }
}
fn work(shared: &Shared, wake: Wake, done: Wake) -> Result<(), String> {
    crate::native::verify_quiet_mode()?;
    crate::native::verify_worker_token()?;
    let request: Request = serde_json::from_slice(&shared.read()?).map_err(|e| e.to_string())?;
    let plugin = Plugin::open(&request.path, request.plugin_id.as_deref())?;
    let info = plugin.info();
    info.validate()?;
    for (cell, p) in shared.values.iter().zip(&info.params) {
        cell.store(p.default.to_bits(), Relaxed);
    }
    shared.write(&serde_json::to_vec(info).map_err(|e| e.to_string())?)?;
    std::thread::scope(|scope| {
        let (main, audio) = plugin.split();
        let audio_thread = scope.spawn(move || -> Result<(), String> {
            let _mmcss = crate::native::ProAudio::enter()?;
            let mut processor = audio.start()?;
            shared.status.store(HELPER_READY, Release);
            let mut last = 0;
            let mut epoch = 0;
            let mut input = [0.; BLOCK];
            let mut output = [0.; BLOCK];
            while shared.stop.load(Acquire) == 0 && !main.restart_requested() {
                let latest = shared.latest.load(Acquire);
                let Some((seq, slot)) = next_input(&shared.slots, last, latest, shared.depth.load(Relaxed) as usize, &mut input) else {
                    wake.wait(25)?;
                    continue;
                };
                // The epoch belongs to this specific block; a pending pre-reset request cannot consume the reset.
                let current_epoch = slot.epoch.load(Relaxed);
                if seq != last + 1 || epoch != current_epoch {
                    processor.reset();
                }
                epoch = current_epoch;
                let notify = slot.notify.load(Relaxed) != 0;
                processor.process(&input, &mut output, &shared.values)?;
                slot.finish(&output);
                // Live processing polls the slot at its scheduled deadline and never waits on `done`.
                if notify {
                    done.signal();
                }
                last = seq;
            }
            if main.restart_requested() {
                Err("The CLAP effect requested a restart. Reload it to resume processing.".into())
            } else {
                Ok(())
            }
        });
        while !audio_thread.is_finished() {
            main.callbacks();
            std::thread::sleep(Duration::from_millis(5));
        }
        audio_thread.join().map_err(|_| "Effect audio worker panicked".to_string())?
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot() -> Slot {
        Slot {
            state: AtomicU32::new(0),
            sequence: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            notify: AtomicU32::new(0),
            audio: [const { AtomicU32::new(0) }; BLOCK],
        }
    }

    #[test]
    fn grouped_capture_delivers_every_effect_block_in_order() {
        use crate::{BlockDelay, BlockStatus};
        for depth in 1..=MAX_DEPTH {
            let mut delay = BlockDelay::new();
            delay.depth = depth;
            let slots = std::array::from_fn(|_| slot());
            let mut latest = 0;
            let mut last_processed = 0;
            let mut audio = [0.; BLOCK];
            for group in 0..6 {
                // Only the engine runs during the burst, just as when it drains multiple complete blocks on one microphone wake.
                for _ in 0..depth {
                    let input = [(latest + 1) as f32; BLOCK];
                    let result =
                        delay.advance(latest, &input, &mut audio, false, |seq, out| slots[seq as usize % SLOTS].read_output(seq, out));
                    if group == 0 {
                        assert_eq!(result, BlockStatus::Priming);
                        assert_eq!(audio, [0.; BLOCK]);
                    } else {
                        assert_eq!(result, BlockStatus::Processed);
                        assert_eq!(audio, [((latest + 1) as usize - depth) as f32 * 0.5; BLOCK]);
                    }
                    latest += 1;
                    assert!(slots[latest as usize % SLOTS].submit(latest, 0, false, &input));
                }
                // One worker wake must drain the whole burst even if every SetEvent coalesced.
                while let Some((seq, slot)) = next_input(&slots, last_processed, latest, depth, &mut audio) {
                    assert_eq!(seq, last_processed + 1);
                    assert_eq!(audio, [seq as f32; BLOCK]);
                    slot.finish(&[seq as f32 * 0.5; BLOCK]);
                    last_processed = seq;
                }
                assert_eq!(last_processed, latest);
            }
        }
    }

    #[test]
    fn recovering_worker_skips_only_requests_past_their_deadline() {
        let slots = std::array::from_fn(|_| slot());
        for seq in 1..=4 {
            assert!(slots[seq as usize % SLOTS].submit(seq, 0, false, &[seq as f32; BLOCK]));
        }
        let mut input = [0.; BLOCK];
        let (seq, slot) = next_input(&slots, 0, 4, 2, &mut input).unwrap();
        assert_eq!(seq, 3);
        assert_eq!(input, [3.; BLOCK]);
        slot.finish(&input);
        let (seq, slot) = next_input(&slots, seq, 4, 2, &mut input).unwrap();
        assert_eq!(seq, 4);
        slot.finish(&input);
        assert!(next_input(&slots, seq, 4, 2, &mut input).is_none());
    }

    #[test]
    fn late_worker_can_never_deliver_a_stale_block() {
        let slot = slot();
        let mut audio = [0.0; BLOCK];
        assert!(slot.submit(1, 0, false, &[1.0; BLOCK]));
        assert!(slot.take(1, &mut audio));
        assert_eq!(audio, [1.0; BLOCK]);
        assert!(!slot.submit(4, 1, false, &[4.0; BLOCK]));
        assert_eq!(slot.epoch.load(Relaxed), 0);
        assert!(!slot.read_output(1, &mut audio));
        slot.finish(&[2.0; BLOCK]);
        assert!(!slot.read_output(4, &mut audio));
        assert!(slot.submit(7, 1, true, &[7.0; BLOCK]));
        assert!(!slot.take(4, &mut audio));
        assert!(slot.take(7, &mut audio));
        assert_eq!(slot.epoch.load(Relaxed), 1);
        slot.finish(&[14.0; BLOCK]);
        assert!(slot.read_output(7, &mut audio));
        assert_eq!(audio, [14.0; BLOCK]);
    }

    #[test]
    fn parent_rejects_nonfinite_samples_in_a_completed_response() {
        let slot = slot();
        let mut audio = [0.0; BLOCK];
        assert!(slot.submit(1, 0, false, &audio));
        assert!(slot.take(1, &mut audio));
        audio[BLOCK - 1] = f32::NAN;
        slot.finish(&audio);
        assert!(!slot.read_output(1, &mut audio));
    }
}
