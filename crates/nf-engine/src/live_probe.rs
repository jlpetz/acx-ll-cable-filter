//! A live trace of how the loopback delivers against the mic's wakes (TODO 10), for the reference's timeline
//! (reference.rs) to be replayed against. **It opens real audio devices**, so it's built only with the `live-probe`
//! feature, ignored, and does nothing unless `NF_LIVE_PROBE` is set: no plain `cargo test` can reach a device. The owner
//! runs it, with `NF_LIVE_PROBE=1 cargo test -p nf-engine --features live-probe --lib live_probe -- --ignored --nocapture`
//! (and lints it with `cargo clippy -p nf-engine --all-targets --features live-probe`).
//!
//! It opens what the engine would on the dev box's cables. The mic is `NF_PROBE_MIC` (default "CABLE-A Output"), with a
//! render stream feeding it a 300 Hz tone, `NF_PROBE_FEED` ("CABLE-A Input"). The reference endpoint is `NF_PROBE_REF`
//! ("CABLE Input"): a far-end stream playing 500 Hz, the keep-alive and the loopback. Names match by prefix. For
//! `NF_PROBE_S` seconds (20) it waits on the mic's event as the audio loop does, reads every mic packet, tops the renders
//! up and polls the loopback. Every packet goes to `%LOCALAPPDATA%\noise-filter\traces\loopback-<reference>-<n>.csv`, as
//! `now,kind,frames,pos,qpc`: the wake and the packet's QPC stamp in hns from the start (the stamp -1 when the driver
//! flagged it), kind 0 for the mic and 1 for the loopback, and the device position. The trace replays
//! (`traces_hold_the_echo_path` and `aec3_converges_on_the_traces`, pipeline/tests/timeline.rs) read every trace there.
//! It prints the delivery's statistics too: frames per wake, the data's age when read, and the swing of loopback frames
//! against mic frames.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use wasapi::{DeviceEnumerator, Direction};

use crate::api::Flow;
use crate::devices::{list_devices, resolve};
use crate::errors::StreamRole;
use crate::pipeline::{CaptureSource, RenderSink};
use crate::sched::{Com, Mmcss, MmcssPriority};
use crate::stream::{CaptureStream, KeepAlive, MicOptions, RenderStream};
use crate::timing::Qpc;

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

/// The id of the first `flow` endpoint whose name starts with `name`.
fn id_of(name: &str, flow: Flow) -> String {
    let all = list_devices().expect("list the devices");
    all.iter().find(|d| d.flow == flow && d.name.starts_with(name)).unwrap_or_else(|| panic!("no {flow:?} device {name}")).id.clone()
}

/// The `p` quantile of `v` (sorted in place); NaN when empty.
fn quantile(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    if v.is_empty() { f64::NAN } else { v[((v.len() - 1) as f64 * p).round() as usize] }
}

/// Tops `s` up to 100 ms of a `hz` tone.
fn top_up(s: &mut RenderStream, phase: &mut f64, hz: f64, buf: &mut [f32]) {
    let pad = s.padding().expect("padding");
    let n = 4800usize.saturating_sub(pad).min(s.buffer_frames() - pad).min(buf.len());
    for v in &mut buf[..n] {
        *v = (0.1 * phase.sin()) as f32;
        *phase += 2.0 * std::f64::consts::PI * hz / 48_000.0;
    }
    if n > 0 {
        s.write(&buf[..n]).expect("write");
    }
}

/// The next free `loopback-<reference>-<n>.csv` in the trace folder (created if need be).
fn trace_path(reference: &str) -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let dir = base.join("noise-filter").join("traces");
    std::fs::create_dir_all(&dir).expect("create the trace folder");
    let name: String = reference.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    (1..).map(|n| dir.join(format!("loopback-{name}-{n}.csv"))).find(|p| !p.exists()).expect("a free name")
}

/// One wake: when (ms from the start), and the frames each stream delivered.
struct Wake {
    t_ms: f64,
    mic: u64,
    lb: u64,
}

#[test]
#[ignore]
fn live_probe() {
    if std::env::var_os("NF_LIVE_PROBE").is_none() {
        return;
    }
    let (mic_name, feed_name, ref_name) =
        (env("NF_PROBE_MIC", "CABLE-A Output"), env("NF_PROBE_FEED", "CABLE-A Input"), env("NF_PROBE_REF", "CABLE Input"));
    let secs: f64 = env("NF_PROBE_S", "20").parse().expect("NF_PROBE_S in seconds");
    let (mic_id, feed_id, ref_id) = (id_of(&mic_name, Flow::Capture), id_of(&feed_name, Flow::Render), id_of(&ref_name, Flow::Render));

    let _com = Com::mta().expect("COM");
    let en = DeviceEnumerator::new().expect("enumerator");
    let device = |id: &str, dir, role| resolve(&en, Some(id), dir, role).expect("resolve");
    let mic_dev = device(&mic_id, Direction::Capture, StreamRole::Mic);
    let feed_dev = device(&feed_id, Direction::Render, StreamRole::Probe);
    let ref_dev = device(&ref_id, Direction::Render, StreamRole::Reference);
    let mut mic = CaptureStream::open_mic(&mic_dev, MicOptions { low_latency: true, raw: true }).expect("open the mic");
    let mut feed = RenderStream::open(&feed_dev, 2_000_000).expect("open the feed");
    let mut far = RenderStream::open(&ref_dev, 2_000_000).expect("open the far end");
    let mut ka = KeepAlive::open(&ref_dev).expect("open the keep-alive");
    let mut lb = CaptureStream::open_loopback(&ref_dev).expect("open the loopback");
    eprintln!("mic {:?}\nloopback {:?}", mic.info.stream_info(), lb.info.stream_info());

    let qpc = Qpc::new();
    let mut buf = vec![0.0f32; 48_000];
    let (mut feed_phase, mut far_phase) = (0.0, 0.0);
    top_up(&mut feed, &mut feed_phase, 300.0, &mut buf);
    top_up(&mut far, &mut far_phase, 500.0, &mut buf);
    ka.top_up().expect("keep-alive");
    let mmcss = Mmcss::enter(MmcssPriority::Critical);
    for start in [feed.start(), far.start(), ka.start(), lb.start(), mic.start()] {
        start.expect("start");
    }

    let t0 = qpc.now_hns() as f64;
    let mut pkt = vec![0.0f32; 96_000];
    let mut wakes: Vec<Wake> = Vec::with_capacity(10_000);
    let (mut lb_age, mut mic_age) = (Vec::new(), Vec::new());
    let mut log = String::from("now,kind,frames,pos,qpc\n");
    let stamp = |q: Option<u64>| q.map_or(-1.0, |q| q as f64 - t0);
    loop {
        mic.wait(100);
        let now = qpc.now_hns() as f64;
        if now - t0 > secs * 1e7 {
            break;
        }
        let mut w = Wake { t_ms: (now - t0) / 1e4, mic: 0, lb: 0 };
        while let Some(p) = mic.read_packet(&mut pkt).expect("read the mic") {
            if let Some(q) = p.qpc_hns {
                mic_age.push((now - q as f64) / 1e4);
            }
            let _ = writeln!(log, "{},0,{},{},{}", now - t0, p.frames, p.device_pos, stamp(p.qpc_hns));
            w.mic += p.frames as u64;
        }
        top_up(&mut feed, &mut feed_phase, 300.0, &mut buf);
        top_up(&mut far, &mut far_phase, 500.0, &mut buf);
        ka.top_up().expect("keep-alive");
        while let Some(p) = lb.read_packet(&mut pkt).expect("read the loopback") {
            if let Some(q) = p.qpc_hns {
                lb_age.push((now - q as f64) / 1e4);
            }
            let _ = writeln!(log, "{},1,{},{},{}", now - t0, p.frames, p.device_pos, stamp(p.qpc_hns));
            w.lb += p.frames as u64;
        }
        wakes.push(w);
    }
    drop(mmcss);
    for s in [&mic, &lb] {
        s.stop();
    }
    ka.stop();
    far.stop();
    feed.stop();
    let path = trace_path(&ref_name);
    std::fs::write(&path, log).expect("write the trace");

    // The first second is the streams' start-up.
    let w: Vec<&Wake> = wakes.iter().filter(|x| x.t_ms > 1000.0).collect();
    let mut dt: Vec<f64> = w.windows(2).map(|p| p[1].t_ms - p[0].t_ms).collect();
    let per_wake = |f: &dyn Fn(&Wake) -> u64| {
        let mut h = BTreeMap::new();
        for x in &w {
            *h.entry(f(x)).or_insert(0u32) += 1;
        }
        h
    };
    eprintln!("{} wakes over {secs:.0} s; trace: {}", w.len(), path.display());
    eprintln!("wake interval ms: p50 {:.2} p99 {:.2} max {:.2}", quantile(&mut dt, 0.5), quantile(&mut dt, 0.99), quantile(&mut dt, 1.0));
    eprintln!("mic frames per wake: {:?}", per_wake(&|x| x.mic));
    eprintln!("loopback frames per wake: {:?}", per_wake(&|x| x.lb));
    for (name, age) in [("mic", &mut mic_age), ("loopback", &mut lb_age)] {
        eprintln!(
            "{name} data age when read, ms: p50 {:.2} p99 {:.2} max {:.2}",
            quantile(age, 0.5),
            quantile(age, 0.99),
            quantile(age, 1.0)
        );
    }
    // The swing the reference has to absorb: loopback frames delivered less mic frames, detrended.
    let (mut cm, mut cl) = (0i64, 0i64);
    let mut pts: Vec<(f64, f64)> = Vec::new();
    for x in &wakes {
        (cm, cl) = (cm + x.mic as i64, cl + x.lb as i64);
        if x.t_ms > 1000.0 {
            pts.push((x.t_ms, (cl - cm) as f64));
        }
    }
    let n = pts.len() as f64;
    let (mx, my) = (pts.iter().map(|p| p.0).sum::<f64>() / n, pts.iter().map(|p| p.1).sum::<f64>() / n);
    let slope = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum::<f64>() / pts.iter().map(|p| (p.0 - mx).powi(2)).sum::<f64>();
    let mut resid: Vec<f64> = pts.iter().map(|p| p.1 - (my + slope * (p.0 - mx))).collect();
    let (lo, hi) = (quantile(&mut resid, 0.0), quantile(&mut resid, 1.0));
    eprintln!(
        "loopback - mic frames, detrended: swing {:.0} frames ({:.1} ms); drift by count {:.1} ppm",
        hi - lo,
        (hi - lo) / 48.0,
        slope / 48.0 * 1e6
    );
}
