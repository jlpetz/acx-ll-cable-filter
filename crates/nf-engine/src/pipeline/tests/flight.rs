//! The flight recorder on the audio path (flight.rs): the pipeline's own facts, through its ring, make the dump. Scenes at a
//! known time, each through the audio loop's own mic step (`audio::mic_wake`: the stall watch and the output's wakes) and the
//! recorder's core, the thread's waits simulated as the loop waits:
//! - a mic that stops delivering for 150 ms (its packets lost, a wait timing out once) runs the output dry: one dump, its
//!   window the 10 s before and the 3 s after, read as the mic stopping, not the thread or the output;
//! - with an exclusive output (its events waking the thread every 2 ms), a thread held up 120 ms finds both events fired and
//!   the output's reported: no stall is counted, and it's read as the thread held up; a mic's driver that holds its packets
//!   back 60 ms is read as the mic;
//! - with a shared output (the thread woken by the mic alone), a thread held 60 ms and a driver that held its packets back
//!   60 ms give the same facts, and the reading says it can't tell which.

use rtrb::RingBuffer;

use super::events::VSink;
use super::*;
use crate::api::FlightConfig;
use crate::audio::{StallWatch, Wake, mic_wake};
use crate::flight::{Core, Limits, Trigger, Wall, Window, read, write_dump};
use crate::recorder::{ChainItem, ChainTx};

const T0: f64 = 1e10;
/// The scenes' events start 20 s in.
const AT: f64 = T0 + 20e7;
const MS: f64 = 1e4;

/// What wakes the thread besides its 100 ms timeout: the mic's events alone (a shared output), or an exclusive output's too.
#[derive(Clone, Copy, PartialEq)]
enum Woken {
    MicAlone,
    AndOutput,
}

#[derive(Clone, Copy, PartialEq)]
enum Event {
    /// The mic's packets due then are lost (its position skips them).
    MicLost,
    /// The mic's driver holds the packets due then back and delivers them together at the end, stamped on time.
    MicHeldBack,
    /// The thread doesn't run: a wake due then comes at the end, with whatever has fired by then.
    ThreadHeld,
}

struct Scene {
    windows: Vec<Window>,
    /// The stalls the loop's mic step counted (telemetry's, which a soak's exit code counts).
    stalls: u64,
}

/// 40 s of the loop: the mic's packet j is captured from T0 + 10j ms and ready (its event set) 0.5 ms after its end; with
/// `AndOutput` an exclusive output's event fires every 2 ms too. From each wake the thread waits for an event that's set (the
/// output's reported first when both are, the mic's left set for the next wait) or 100 ms; `event` happens from `AT` for
/// `len_ms`.
fn scene(woken: Woken, event: Event, len_ms: f64) -> Scene {
    let len = len_ms * MS;
    let due = |j: u64| T0 + (j + 1) as f64 * 1e5 + 0.5 * MS;
    let during = |t: f64| t > AT && t <= AT + len;
    // When packet j is ready; `None`, lost.
    let ready = |j: u64| match event {
        Event::MicLost if during(due(j)) => None,
        Event::MicHeldBack if during(due(j)) => Some(AT + len),
        _ => Some(due(j)),
    };
    let cfg = PipelineConfig { out_period: 480, out_margin: 96, ref_period: None, max_packet: 4800 };
    let mut p: Pipeline = Pipeline::new(cfg, DriftController::new(cfg.out_target() as f32), SlipResampler::new(), SlipResampler::new());
    let mut sink = VSink::new(480, 0.0, 0.3, T0);
    p.prefill(&mut sink).unwrap();
    let (tx, mut rx) = RingBuffer::<ChainItem>::new(64);
    p.flight = Some(ChainTx::new(tx));
    let mut core = Core::new(Limits::of(&FlightConfig::new("unused")), T0);
    let tel = Telemetry::new();
    let (mut mic, mut ticks) = (FakeCapture::default(), FakeTicks(0));
    let mut watch = StallWatch::new(T0 as i64);
    let exclusive = woken == Woken::AndOutput;
    let (mut j, mut now, mut mic_set, mut out_set, mut out_ticks) = (0u64, T0, false, false, 0u64);
    let (mut windows, mut stalls) = (Vec::new(), 0u64);
    while now < T0 + 40e7 {
        // The wait: none if an event is set; else to the next one, or the timeout.
        let mut t = if mic_set || (exclusive && out_set) {
            now
        } else {
            let next_mic = (j..).find_map(ready).unwrap();
            let next_out = if exclusive { T0 + (out_ticks + 1) as f64 * 2.0 * MS } else { f64::INFINITY };
            (now + 100.0 * MS).min(next_mic).min(next_out)
        };
        if event == Event::ThreadHeld && t >= AT && t < AT + len {
            t = AT + len;
        }
        // What has fired by then: the mic's packets (into the device's buffer), the output's event.
        loop {
            match ready(j) {
                Some(r) if r <= t => {
                    mic.push(j * 480, Some((T0 + j as f64 * 1e5) as u64), vec![0.1; 480]);
                    mic_set = true;
                    j += 1;
                }
                None if due(j) <= t => j += 1,
                _ => break,
            }
        }
        let k = ((t - T0) / (2.0 * MS)).floor() as u64;
        if exclusive && k > out_ticks {
            (out_set, out_ticks) = (true, k);
        }
        let wake = if exclusive && out_set {
            out_set = false;
            Wake { mic: false, out: true }
        } else if mic_set {
            mic_set = false;
            Wake { mic: true, out: false }
        } else {
            Wake::default()
        };
        now = t;
        sink.advance_to(now);
        let (_, stalled) = mic_wake(&mut p, &mut mic, &mut watch, wake, now as i64).unwrap();
        stalls += u64::from(stalled);
        while p.process_block(&mut CopyChain, &mut sink, &mut ticks, &tel, false).unwrap() {
            if p.second_due() {
                p.publish_second(&tel, &CopyChain, 0, 0, 0);
            }
        }
        // The recorder's thread, inline.
        while let Ok(item) = rx.pop() {
            core.feed(1, &item, now);
        }
        while let Some(w) = core.poll(now) {
            windows.push(w);
        }
    }
    assert!(core.flush().is_none(), "nothing pending at the end");
    Scene { windows, stalls }
}

/// The dump's trigger's block.
fn trigger_entry(w: &Window) -> (usize, &crate::flight::Entry) {
    let at = w.entries.iter().position(|e| e.seq == w.first.seq).unwrap();
    (at, &w.entries[at])
}

#[test]
fn a_mic_stall_at_a_known_time_makes_one_dump_around_it() {
    let s = scene(Woken::MicAlone, Event::MicLost, 150.0);
    assert_eq!((s.windows.len(), s.stalls), (1, 1), "one dump; one wait timed out");
    let w = &s.windows[0];
    let (at, t) = trigger_entry(w);
    // The trigger: the first block after the stall, which found the output dry and the mic's position 15 packets on.
    assert_eq!(w.first.trigger, Trigger::OutputUnderrun);
    let also: Vec<Trigger> = w.also.iter().filter(|h| h.seq == w.first.seq).map(|h| h.trigger).collect();
    assert_eq!(also, [Trigger::OutputDeficit, Trigger::MicStall, Trigger::MicSkip, Trigger::MicGlitch]);
    assert!((1_998..=2_001).contains(&t.block.index), "{}", t.block.index);
    assert!(t.block.output.underrun && t.block.output.zeros > 0);
    assert_eq!((t.block.thread.stalls, t.block.thread.wake_gap_ms.round(), t.block.mic_skipped), (1, 100.0, 15 * 480));
    let delivery_gap = (t.block.mic_wake_hns - w.entries[at - 1].block.mic_wake_hns) / 1e4;
    assert!((delivery_gap - 160.0).abs() < 0.01, "{delivery_gap}");
    // The window: the 10 s before it, it, and the 3 s after, in order.
    let idx: Vec<u64> = w.entries.iter().map(|e| e.block.index).collect();
    assert_eq!((idx[0], *idx.last().unwrap(), idx.len()), (t.block.index - 1_000, t.block.index + 300, 1_301));
    assert!(idx.windows(2).all(|p| p[1] == p[0] + 1) && w.post_short == 0);
    assert!(w.entries[..at].iter().all(|e| e.hits == 0), "nothing before it");
    assert!(w.entries.iter().all(|e| e.block.thread.out_wakes == 0), "a shared output wakes nothing");
    // Read as the mic stopping, the thread on time.
    let r = read(w);
    let expect = format!("The mic stopped delivering: no packet for 160 ms before block {} (1 wait timed out", t.block.index);
    assert!(r.verdict.starts_with(&expect), "{}", r.verdict);
    // And written out.
    let dir = std::env::temp_dir().join("nf-engine-tests").join(format!("flight-scene-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let wall = Wall { text: "2026-11-02 14:03:12.000".into(), stamp: "20261102-140312".into() };
    let d = write_dump(&dir, w, &wall, 20).unwrap();
    assert_eq!(d.also, ["output-deficit", "mic-stall", "mic-skip", "mic-glitch"]);
    let text = std::fs::read_to_string(d.path.join("summary.txt")).unwrap();
    assert!(text.contains(&r.verdict) && text.contains("10.00 s before the trigger and 3.00 s after"), "{text}");
    assert_eq!(std::fs::read_to_string(d.path.join("flight_blocks.csv")).unwrap().lines().count(), 1_302);
    let _ = std::fs::remove_dir_all(dir);
}

/// With an exclusive output, a thread held up 120 ms runs again with both events fired and the output's reported: the mic's
/// packets are waiting, so no stall is counted (in the facts or telemetry), and the reading says the thread was held up.
#[test]
fn a_thread_held_up_past_the_stall_time_with_an_exclusive_output_is_no_stall() {
    let s = scene(Woken::AndOutput, Event::ThreadHeld, 120.0);
    assert_eq!((s.windows.len(), s.stalls), (1, 0), "one dump, no stall");
    let w = &s.windows[0];
    let (_, t) = trigger_entry(w);
    let triggers: Vec<Trigger> = std::iter::once(w.first.trigger).chain(w.also.iter().map(|h| h.trigger)).collect();
    // The output ran dry while the thread was held, and the backlog it found overfilled it after (coarse drops).
    assert_eq!(&triggers[..2], [Trigger::OutputUnderrun, Trigger::LateWake], "{triggers:?}");
    assert!(triggers.contains(&Trigger::OutputDrop) && !triggers.contains(&Trigger::MicStall), "{triggers:?}");
    assert_eq!((t.block.thread.wake_gap_ms.round(), t.block.thread.stalls), (122.0, 0));
    assert!(t.block.mic_queued >= 480 * 10, "the mic's packets waited: {}", t.block.mic_queued);
    let r = read(w);
    let expect = format!("The audio thread was held up: no wake for 122 ms before block {}", t.block.index);
    assert!(r.verdict.starts_with(&expect) && r.verdict.contains("not the device"), "{}", r.verdict);
}

/// With an exclusive output, a mic's driver that holds its packets back 60 ms and then delivers them at once: the output's
/// events kept waking the thread on time, so it's the mic.
#[test]
fn a_mic_that_holds_its_packets_back_with_an_exclusive_output_is_the_mic() {
    let s = scene(Woken::AndOutput, Event::MicHeldBack, 60.0);
    assert_eq!((s.windows.len(), s.stalls), (1, 0));
    let w = &s.windows[0];
    let r = read(w);
    assert!(r.verdict.starts_with("The mic stopped delivering: no packet for 70 ms before block"), "{}", r.verdict);
    assert!(r.verdict.contains("while the audio thread woke on time"), "{}", r.verdict);
    assert!(trigger_entry(w).1.block.thread.wake_gap_ms <= 2.5, "the output's events woke it");
}

/// With a shared output the thread wakes on the mic's events alone: a thread held 60 ms and a driver that held its packets
/// back 60 ms give the same facts, and the reading says it can't tell which rather than blaming either.
#[test]
fn with_a_shared_output_a_held_thread_and_a_held_back_mic_read_the_same() {
    let (thread, driver) = (scene(Woken::MicAlone, Event::ThreadHeld, 60.0), scene(Woken::MicAlone, Event::MicHeldBack, 60.0));
    let read_one = |s: &Scene| {
        assert_eq!((s.windows.len(), s.stalls), (1, 0));
        let w = &s.windows[0];
        let t = trigger_entry(w).1.block;
        ((t.index, t.thread.wake_gap_ms, t.mic_queued, t.thread.out_wakes), read(w).verdict)
    };
    let (thread, driver) = (read_one(&thread), read_one(&driver));
    assert_eq!(thread, driver, "the same facts, the same reading");
    let either =
        format!("The audio thread was held up, or the mic's driver held its samples back: no wake for 70 ms before block {}", thread.0.0);
    assert!(thread.1.starts_with(&either) && thread.1.contains("can't tell"), "{}", thread.1);
}
