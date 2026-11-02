# noise-filter CLAP host profile v0 (draft, unpublished)

What noise-filter guarantees a stage, and what it expects of one (TODO 17). Every stage is CLAP-shaped: the echo
canceller, the noise filter and, later, effects. Our own stages implement it in-process through the Rust trait
`nf_core::stage::Stage`, with no FFI. A `.clap` export of them, and third-party CLAP plugins in an fx-host process, come
later; this profile is what both will follow.

Status: a draft. The namespace (`noise-filter`) and every id below are provisional. They become reverse-DNS, and freeze,
before anything is published.

## Format
- 48 kHz, f32, mono ports.
- `activate(48000, 480, 480)`: fixed 480-frame (10 ms) blocks. Our stages refuse any other format.
- Samples are finite (the engine zeros NaN and ±inf at the device edge) but may exceed ±1, and a garbage float capture
  buffer can hold any finite value. Our stages sanitise their inputs again, and clamp them to ±1e4 (+80 dBFS) before any
  model: RNNoise's ×32768 overflows a sample of about 1e17 to inf.
- No events, no MIDI, no transport.

## Ports per role (`clap.audio-ports`)
Port 0 of each direction is the main port (`CLAP_AUDIO_PORT_IS_MAIN`); there is exactly one output; all ports are mono
(`CLAP_PORT_MONO`, channel count 1); no in-place pairs.

| Role (feature tag) | Input 0 (main) | Input 1 (non-main) | Output 0 (main) |
|---|---|---|---|
| Echo canceller (`noise-filter:echo-cancel`) | Mic | Speaker reference (required) | Echo-cancelled mic |
| Noise filter (`noise-filter:denoise`) | Input | optional | Output |
| Effect (neither tag) | Input | optional | Output |

- The **speaker reference** is the render endpoint's loopback, already resampled onto the mic's clock and aligned by the
  host's reference timeline, with the engine's lead of 12-18 ms (TODO 10) over the mic. It is the v0 host's only
  sidechain source: any stage with a second input gets it.
- **That lead holds for the echo canceller only.** Every slot gets the same reference block, not delayed by the slots
  upstream. A later stage's main input lags the mic by the upstream latency (430 samples with AEC3 on, 0 with it off,
  changing while it drains), so its sidechain leads by that much more. No v0 stage takes a sidechain there; the first
  one that does decides whether the host delays it.
- A stage's role comes from its feature tags: one role tag gives that role, none an effect, both is refused.
- The host checks all of this at activate (`check_host_profile`) and refuses a stage in the wrong slot.

## Threads and real-time rules
| Call | Thread | Rule |
|---|---|---|
| descriptor, audio ports | any | constant |
| `activate` | **the thread that will process** (deviation: CLAP says main thread) | may allocate; may be called again, which implies a deactivate; must not change our stages' latency |
| `process` | audio | no allocation, locks, system calls or logging; O(1) per block. One non-blocking system call is allowed: a noise stage may wake its inference worker (`Thread::unpark`: an atomic swap, and `WakeByAddressSingle` only if the worker is parked) |
| `reset` | audio | no locks or system calls; a no-op when nothing was processed since the last one. No allocation, with one known exception: RNNoise's second real reset before the next `activate` rebuilds its state in place (ten allocations, from the engine's real-time pool) |
| latency | any | valid after `activate`, fixed until the next; the host caches it |
| params: list, value | whoever holds the stage | constant list; reading a value is real-time safe |
| params: set | whoever holds the stage: while active, the processing thread between blocks (where CLAP's in-events sit) | never during `process`; no allocation or locks; takes effect at the next block |
| state save, load | whoever holds the stage, never during `process` (CLAP: main thread) | may allocate, so not on the audio thread while it runs; nothing calls it live in v0 |
| extension values | audio, after `process` | no allocation or locks |
| a model's inference (`noise-filter.denoise` around a model pack) | the stage's own inference worker: MMCSS "Pro Audio" (normal priority, under the audio thread's critical; without MMCSS, HIGHEST, under the audio thread's TIME_CRITICAL fallback, and a start-up warning), building its model (loading and hashing the pack, the session, the warm-up) outside the real-time allocator scope and running it inside it | per-run allocations are allowed there, and measured (TODO 6: ONNX Runtime's are about 3,700 a run; the worker's own Rust side makes none); the audio thread exchanges whole blocks with it through two bounded lock-free rings (32 blocks each) and uses each result `depth` calls later (2 by default: +960 to the latency), playing the delayed input, gain-matched, when a result is late. **The deadline is that later call, not a block's time:** the host processes a wake's blocks back to back, so a wake of `k` blocks costs `max(0, k - depth)` substitute blocks. No FTZ/DAZ on it either (ONNX Runtime's flush-to-zero isn't set) |

Activating on the processing thread exists for easyfft's per-thread FFT state (RNNoise): built anywhere else, the first
block allocates. The `.clap` export has to warm that thread up itself.

No other thread calls a stage. The settings window's values reach our noise stage through the host: its params are
views of the chain's shared controls, which `Engine::apply` writes from another thread. A stage whose params are its own (an effect, a
model pack's `controls[]`) needs the host to queue values for it; that queue comes with the first such stage.

## Latency (`clap.latency`)
- The stream offset: `output[n]` corresponds to `input[n - latency]`. Not the algorithmic latency, which adds one hop of
  collecting a frame that only exists live.
- RNNoise 480, AEC3 430 (measured): 910 with both. A noise model with its own hop is re-blocked to 480 inside the noise
  stage, which adds `hop - gcd(hop, 480)`: see `model-pack-v0.md`. A model on an inference worker adds `depth` × 480 (2 by
  default): UL-UNAS 2400, 2830 with AEC3 (measured; 1920 and 2350 at depth 1).
- A stage whose latency is over a block starts up for `ceil(latency / 480)` blocks after a reset; the host keeps a stage
  switched on on its own delay line (the same latency) until that has passed, then crossfades into it (`chain.rs`), so a
  switch-on plays no start-up silence.
- The host's harness measures it to the sample for every stage (TODO 2's check 1).

## Bypass, reset and priming
The host owns bypass; stages declare no `IS_BYPASS` param.
- **Off:** a copy, with no stage CPU and no delay.
- **Off to on:** the host resets the stage, primes it with the previous block, and dip-splices from 0 latency to the
  stage's.
- **On to off:** the host crossfades into a delay line of the stage's latency, then drops the latency at a quiet block.
- **Measurement passthrough:** the stage is replaced by a delay line of its latency.

A stage whose latency exceeds one block starts with `latency - 480` samples from its reset state.

## Extensions
| Extension | Who | Notes |
|---|---|---|
| `clap.audio-ports` | all | as above |
| `clap.latency` | all | as above |
| `clap.params` | the noise filter | no automation or modulation flags; flags use CLAP's bits (`STEPPED` 1, `HIDDEN` 4, `READONLY` 8, `ENUM` 65536) |
| `clap.state` | all | the default blob holds the params (below) |
| `clap.gain-adjustment-metering/0` (CLAP draft) | the noise filter | deviations: the block's output level against its latency-aligned input's, in dB (<= 0), not the last sample's; NaN before the first block and after a reset, where the draft says zero |
| `noise-filter.speech-probability/0` (ours) | the noise filter, when its model has a VAD | 0..1, or NaN when unknown: the model's estimate for the window that produced the block just output |
| `noise-filter.aec-telemetry/0` (ours) | the echo canceller | echo-path delay (ms) and ERLE (dB), NaN when unknown |

gui, log and thread-check belong to the fx-host phase.

### The noise filter's params
Ids are stable. The ranges are the DSP's accepted domain; the settings window keeps narrower spans. A set rejects NaN,
clamps to the range and truncates a stepped value. The host checks every stage's params: unique ids, a finite
increasing range, the default in range, a stepped param integral, and an enum param stepped too (CLAP's rule).

| id | name | unit | range | default | flags |
|---|---|---|---|---|---|
| 1 | Floor | dB | -120..0 | -60 | |
| 2 | Gate mode (0 Off, 1 Static, 2 Auto) | | 0..2 | 0 | STEPPED, ENUM |
| 3 | Gate threshold | dBFS | -120..0 | -50 | |
| 4 | Gate auto margin | dB | 0..40 | 6 | |
| 5 | Gate attack | ms | 0..1000 | 2 | |
| 6 | Gate release | ms | 0..5000 | 150 | |
| 7 | Gate hold | ms | 0..5000 | 100 | |
| 8 | Gate range | dB | -120..0 | -40 | |
| 9 | Gate holds open on speech | | 0..1 | 0 | STEPPED |

### The default state blob
Little-endian: `b"NFST"`, u16 version (1), u16 reserved (0), u32 count, then count × (u32 id, f64 value). Loading
ignores unknown ids and bytes after the last entry, keeps the params the blob leaves out, clamps each value, and refuses
a bad magic, an unknown version, a truncated blob or a NaN value.

## Feature tags
- Standard (CLAP's `plugin-features.h`): `audio-effect`, `restoration`, `gate`, `mono`.
- Ours (CLAP's `$namespace:$feature` rule): `noise-filter:echo-cancel`, `noise-filter:denoise`.
- Our stages: the echo canceller `noise-filter.echo-cancel` (`audio-effect, restoration, mono, noise-filter:echo-cancel`);
  the noise filter `noise-filter.denoise` (`audio-effect, restoration, gate, mono, noise-filter:denoise`).

## Not modelled in v0
`start_processing`, `stop_processing`, `deactivate`, the process status, `steady_time` and events. The `.clap` export maps
each to a no-op or a constant; `destroy` is the stage's drop.

## Deviations the `.clap` export must adapt
- **The host chooses the format.** A CLAP host activates at its own rate, with any frame count in `[min, max]` (bounded by
  `[1, INT32_MAX]`); our stages accept only 48 kHz in 480-frame blocks. The wrapper buffers host blocks into 480 (the
  noise stage's re-blocker, `nf_core::dsp::reblock`, does this already), adds that delay to the latency it reports, and
  resamples or refuses any rate but 48 kHz.
- **`activate` on the processing thread** (above): the export warms easyfft up on the host's audio thread itself, or
  its first block allocates.
- **`reset` isn't a full reset for AEC3.** CLAP's clears all processing state. Ours flushes the audio AEC3 holds and
  keeps its converged echo path and delay estimate, so it needn't reconverge after a latency test; a full reset would
  mean building a new AEC3, which allocates. The export documents it, or rebuilds on the main thread.
- **Gain adjustment:** measured over the block, and NaN where the draft says zero (above). The export maps NaN to 0 dB.
  The draft id `clap.gain-adjustment-metering/0` may change before it is final.
