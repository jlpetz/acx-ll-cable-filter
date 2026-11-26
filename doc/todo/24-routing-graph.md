# TODO 24: a routing graph (groundwork for a mixer)

## The owner's intent (2026-11-26)
"Our app is a low latency driver for an audio cable with pluggable filters that can be added into the input/output
pipeline of the cable. But if I want multiple pipelines off the same feed ..." and, on Voicemeeter (VB-Audio's mixer
for its cables): "You can split the cable at different points or merge and split different channels/cables in the app.
Having a low latency path for that in the driver or user land filters would be a feature I think we eventually want to
add ... I don't want to build a Voice Meter app just yet, but this driver and user component should have the ground work
for it."

So: no mixer app now; the per-cable driver (TODO 19's step 1) and the engine's fan-out (TODO 19's E1, Astra's) are built
so that one can be added without redoing either.

The owner chose mono and stereo for the first routing implementation. Surround layouts such as 5.1 and 7.1 can come later.
Today the app averages capture channels to mono and the direct writer repeats that sample on left and right. E1 should
keep both channels of a stereo source through the split. A dry recording gets the original left and right. A branch
using a mono voice effect downmixes at that effect's input. A mono signal stays mono until a stereo output needs the
same sample on both channels.

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

## Not now
The mixer window, per-channel routing in the GUI, multichannel cables, the render tap itself, mixing inputs on different
clocks beyond the adapters the WASAPI output already has.
