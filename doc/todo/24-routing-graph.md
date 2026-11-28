# TODO 24: a routing graph (groundwork for a mixer)

## Working implementation on the development box, 2026-11-27

`llcable-routing` processes the frames delivered in each capture packet. `llcable-audio` captures each selected endpoint once and feeds independent direct rings. A separately clocked playback destination has a bounded queue and its own worker. The legacy voice application also uses the shared Windows conversion and scheduling code. The [build and quick start](../../README.md) covers both windows and the independent SDK client.

With one virtual stereo source, two recording clients verified 236,703 consecutive frames at unity and 236,575 at quarter gain after joining, with every checked sample exact. A separate two-input test played distinct tones into each input and checked all four tones in each output recording. With direct and external playback enabled together, all 283,775 direct frames matched. The Windows shared playback branch through VB-CABLE matched 275,918 of 275,950 frames at quarter gain. These checks exercise identity, channel preservation and gain. They do not measure acoustic or end-to-end latency.

The driver's first pass after a fresh reader joins discards pre-existing ring contents and can deliver a packet of silence. The live runtime reports startup silence and subsequent underruns separately. The passive panel shows the raw cumulative counters.

The release mixer passed a separate check through its WinUI controls. All three cable-only sweeps matched on A.
A player then sent 600 Hz on the left and 1 kHz on the right into A. With a −12 dB destination stage on B, an
independent WASAPI recorder measured 0.08038 left and 0.04019 right, the expected amplitudes for inputs of 0.32
and 0.16. The mixer's stage diagnostics read −9.9 dBFS in and −21.9 dBFS out. Both feeds were idle after Stop.

On the recorded speech-and-fan fixture, RNNoise cut stationary fan level by 47.9 dB and Sonora Moderate by 14.8 dB. The aligned speech projection changed by −0.5 and −3.7 dB respectively. Wall-clock p99 on the dev VM was 39.8 and 9.3 µs per 128-frame callback. The [comparison example](../../crates/llcable-effects/examples/compare_noise.rs) reproduces the procedure. Real speech, double-talk, other noise types and physical-device timing remain important release checks.

### Dry-branch timing on the dev box, 2026-11-29

The [branch probe](../../crates/llcable-audio/examples/branch_probe.rs) wrote a six-second clip to VB-A and recorded the
two native low-latency outputs in separate WASAPI clients. Each of the 288,000 stereo frames encoded its source position.
A separate monitoring bus stamped the router's input before either destination ran. Every 1,009th frame was a timing
marker (286 per clip). The receiving client stamped the whole packet after its read, on the same monotonic host clock.
These are application receipt times. A 128-frame recording packet at 48 kHz spans 2.667 ms.

All runs used 48 kHz, a 128-frame source playback period and 256-frame target fill, a 144-frame source capture period,
128-frame destination readers, and 8,192-frame direct rings with 144-frame starting fill. Arrival times below are in milliseconds.

| Pair, order | B chain | Router input to A median | p99 | Source submission to A median |
| --- | --- | ---: | ---: | ---: |
| 1, first | Empty | 4.524 | 7.053 | 60.779 |
| 1, second | RNNoise | 5.085 | 7.714 | 65.363 |
| 2, first | RNNoise | 6.079 | 8.734 | 66.180 |
| 2, second | Empty | 4.082 | 7.064 | 63.500 |
| 3, first | Empty | 6.099 | 8.698 | 68.254 |
| 3, second | RNNoise | 5.607 | 8.675 | 72.318 |

A delivered all 1,728,000 stereo frames bit-exact and in order. Dry B matched every frame in each of its three runs.
Wet B delivered nonzero finite dual-mono output. Neither destination reported steady-state underruns or ring drops.
The paired router-input-to-A median changes with B wet were +0.561, +1.997 and −0.492 ms. The largest individual A
receipt was 9.215 ms. The measurements show no additional 10 ms hop on A. Each session started its readers independently,
so these paired differences also include packet phase. The source-submission column includes the VB-A playback and capture path.

In a second test, one running session kept both readers, the source and the direct rings open. It alternated B between an
empty chain and RNNoise five times. Each two-second window began 300 ms after the change and contained 95 timing markers.

| Pair | Router input to A median, dry / wet (ms) | p99, dry / wet (ms) | Source submission to A median, dry / wet (ms) |
| --- | ---: | ---: | ---: |
| 1 | 5.068 / 5.588 | 8.105 / 8.587 | 63.975 / 64.004 |
| 2 | 6.092 / 5.592 | 7.646 / 8.147 | 63.974 / 63.934 |
| 3 | 5.069 / 5.091 | 7.577 / 7.684 | 64.035 / 63.897 |

With the same readers, the paired wet-minus-dry median changes from router input to A were +0.520, −0.500 and +0.022 ms.
A reproduced all 687,744 source frames in sequence, including all five transitions. Its 682 marker arrivals peaked at
8.587 ms from router input. B reproduced the exact stereo code in the three dry windows and delivered active, finite dual
mono in each wet window. These packet-level figures are specific to the virtual endpoints and scheduling on the dev box.

### Continuity through a destination edit and device cycle, 2026-11-29

The same probe kept the source, dry A route and A recorder running while releasing B, changing its send gain from 0.25 to
0.5, and resuming it. It then disabled B's Windows devnode, observed B go offline, re-enabled it and waited for automatic
reattachment. A received all 219,136 source frames bit-exact and consecutive, including the device cycle. Its driver attach
count stayed at 12. B's recording contained 38,256 exact frames at gain 0.25 before the edit, 38,528 silent frames while
released, and 38,256 exact frames at gain 0.5 in each of the post-resume and post-reattachment samples.

### Running the probes

To reproduce, take the virtual source's recording and matching playback IDs from `noise-filter-cli devices`.
With `$capture` and `$playback` holding those IDs, run the following commands on idle virtual endpoints.
The last command temporarily disables and re-enables the virtual Windows device labelled B. Its duration argument
and the `switch` duration argument limit the source run time. Both commands stop the source when their checks finish.

```powershell
cargo build --release -p llcable-audio --example branch_probe
foreach ($mode in 'dry','rnnoise','rnnoise','dry','dry','rnnoise') {
    & .\target\release\examples\branch_probe.exe $mode $capture $playback A B 6
    if ($LASTEXITCODE) { throw "Probe failed in $mode" }
}
& .\target\release\examples\branch_probe.exe switch $capture $playback A B 25
if ($LASTEXITCODE) { throw "Live switching probe failed" }
cargo build --release -p nf-vmic --bin acx-ll-cable
& .\target\release\examples\branch_probe.exe continuity $capture $playback A B 60 .\target\release\acx-ll-cable.exe
```

## The owner's intent (2026-11-26)
"Our app is a low latency driver for an audio cable with pluggable filters that can be added into the input/output
pipeline of the cable. But if I want multiple pipelines off the same feed ..." and, on Voicemeeter (VB-Audio's mixer
for its cables): "You can split the cable at different points or merge and split different channels/cables in the app.
Having a low latency path for that in the driver or user land filters would be a feature I think we eventually want to
add ... I don't want to build a Voice Meter app just yet, but this driver and user component should have the ground work
for it."

The initial scope was driver and engine groundwork. The owner subsequently asked for the destination-focused v2 mixer.
The owner chose mono and stereo for its first routing implementation. The mixer preserves both channels on dry routes.
A mono voice stage downmixes at its input, and its output can feed either channel or both. The legacy single-chain app
still averages the capture channels for its voice processing.

## Why in the app (the owner, 2026-11-26)
One capture feeding every destination lets the physical microphone be opened in exclusive mode, which admits a single
client: several apps opening the mic themselves would force it to shared mode. With VB-CABLE A as the mic on the dev VM (TODO 13), mic delivery was about
0.7 ms exclusive against 17-19 ms shared, app + output cable 38.5-41.0 ms with the defaults against 46.7-49.4 with the mic
shared. Exclusive is the default since 2026-11-02, with the shared fallback for a device that refuses it (some drivers,
Bluetooth hands-free, or Windows' "Allow applications to take exclusive control" turned off). Two costs to show the user:
other apps record from the cables while the app holds the mic, and exclusive mode bypasses the shared Windows audio engine.
Processing in the microphone's own hardware can still apply.

## The model (ours, for the owner and Astra)
A routing graph evaluated block by block on the engine's real-time thread, in topological order:
- **Sources:** a physical input (WASAPI capture), and later a cable's playback side (below: the render tap).
- **Stages:** the filters we have (AEC, the noise models, the gate, a CLAP effect) and any later ones.
- A split gives consumers a read-only view of the same samples in one pass. A branch that changes samples writes into
  preallocated scratch. The split needs no extra audio period. Measure processing time with all intended branches active.
- **Mix (merge):** the sum of several inputs with a gain each. Inputs on one clock mix in the same cycle; each input's
  path latency is known (every node reports its own), and a mix can delay the earlier inputs to align them (optional per
  mix: off for a mic and a separate music feed, on for a dry and a processed copy of one voice).
- **Channel map:** split stereo into two monos, merge monos, route channels across feeds.
- **Sinks:** a cable's app ring (the direct route, lowest latency) or a WASAPI output.

### Routing with no effects

The owner wants routing to work with no effects and to pay only for the processing enabled on each path.
A standalone cable already connects its Windows playback and recording endpoints with the app closed. An app-managed
route should also start with an empty chain. Forward each available packet on a dry branch, preserving its channels.
Schedule effect calls and their buffering only on branches that pass through those effects. A user can explicitly enable
time alignment where branches meet. Device periods, ring headroom and any required clock adaptation still apply.

The mixer's dry-branch acceptance check is recorded above. Empty chains forward each input packet and create no plugin
worker. RNNoise assembles 480-frame hops on its branch. The single-chain app's separate [short-packet path](../dev/short-packet-path.md)
has its own eligibility rules. A loaded CLAP effect's live bypass keeps its worker and delay.

## Clocks (the part that decides latency)
- Our cables use the same QPC time base at the same nominal rate. Each reader starts independently and has its own
  packet size, phase, ring headroom and restart handling. A split needs no extra queue between the app's branches.
  For a mix, identify the source frame positions and account for the processing delay of each input.
- **A device on its own clock** (a USB or Bluetooth mic, real speakers, VB-CABLE) joins through an adapter on its own
  thread: a lock-free queue and drift control (slips or resampling, as the WASAPI output already does, TODO 9). That
  costs it one queue (a period and a margin); only it pays.
- **The graph's clock:** the physical mic's when one is the main input (today), or one of our cables' when the graph
  feeds only our cables; to choose per graph later.

## Heavy nodes
A node too slow for the shared thread (a large model used by one branch) runs on its own worker with a deadline and a
fallback, as the model pipeline and the CLAP host already do (TODO 6, TODO 17), so one slow branch can't hold up the
others.

## Groundwork in the driver (TODO 19)
- **One app ring per cable, a low-latency sink:** built in step 1 (per-cable state; one writer per cable).
- **A render tap per cable (to design, not build yet):** what apps play into a cable's Speakers, offered to the engine
  as a low-latency source through a read-only lock-free ring, as the app ring is in the other direction. Today the only
  ways in are the driver's own loopback onto the cable's microphone and WASAPI loopback capture, which is slower. The
  per-cable state leaves room for a second ring.
- Each direct ring currently carries 48 kHz stereo float32 (`crates/nf-vmic/src/protocol.rs`). It can carry distinct
  left and right samples or the same mono sample on both channels. Surround would require a new ring layout.
- **Mixing stays in user mode:** the driver only moves samples, which keeps it small and the mixer free to change.

## Groundwork in the engine (TODO 19's E1, Astra's)
- E1's fan-out is built as a small graph core (nodes, edges, topological order, latency per node) even though the
  window shows only simple routes (a destination dry, from a shared chain, or with its own).
- Our cables' branches run inline on the capture thread and write their rings directly; only a foreign-clock destination
  (WASAPI) or a heavy branch gets its own thread and queue (the amendment to TODO 19's design, E1).
- A ring's attachment stays on the thread that made it (Windows cancels a thread's pending I/O when it exits,
  `device.rs`), but writing samples into the mapped ring is plain memory any thread can do: the capture thread writes
  every cable's ring while the attachments live on a thread that outlives them.
- Settings describe the graph by stable ids (cables by CableId), versioned; presets for the simple routes.

## Remaining work

Surround cables, arbitrary per-channel mapping in the GUI, the direct render tap and optional delay compensation for
merging paths with different processing delays remain open. Separately clocked Windows sources can already feed one mix.
Long-duration clock-drift and acoustic tests on physical devices remain release checks.
