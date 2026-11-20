# The dither and FTZ/DAZ matrix (TODO 10, 2026-11-21)

The question (doc/todo/10-aec.md, "Dither and FTZ/DAZ"): the engine dithers every mic block and every reference block at
about 1e-9 (-180 dBFS; `pipeline.rs`, `DITHER`) so that the filters don't stall on denormals during exact digital silence,
and sonora 0.3.0 also sets FTZ/DAZ inside each of its calls (the "guard", formally undefined behaviour in Rust). The owner
(2026-11-20): decide from tests that cover every filter the chain can run; exact digital silence matters for studio use.

**How it was measured.**
- **Harness:** an ignored test target, `crates/nf-engine/tests/dither_matrix`, with `tools/dither_matrix.sh` and
  `tools/dither_matrix_summary.py`. It lives on branch `worktree-wf_4989e0ed-c97-3` (d0f6095, 90bb776), on top of the
  vendored sonora with the guard as a cargo feature (7d85d1c, 6c838ef).
- **Chains and feed:** built and fed as the engine builds and feeds them.
- **Run:** run-20261121-005101, 10 ABBA rounds of both builds (guard off, guard on) on the dev VM (Xeon 6975P-C, 4 cores),
  each round after a discarded warm-up. 6 rounds were quiet by the CPU rule (1, 3, 4, 5, 7, 8).
- **Figures:** medians over those 6 rounds, in us per 10 ms block, with the gate off. The gate doesn't change the cost.
- **Checks:** two independent analysts, then a check that recomputed every number in this summary from the raw JSON.

## Results

**What stalls.** A stall needs a filter's state to decay from signal into exact-zero input. Silence from start-up never
stalls anything.
- **AEC3**, with the mic clean and the guard off: the tail after speech goes to 381.5 us p50 [380.5-382.1] (445 p99),
  against 46.4 with today's dither. It ramps over about 3 s, then holds for as long as the mic stays exactly zero. It ends
  on the first live block. The guard removes it (46.3).
- **RNNoise alone**, with the mic clean, in either guard build: its model goes to 215.5 us p50 (p99 253 in the tail, 480 in
  the far-only phase), against 18.6. It holds for the whole phase, also after its output is exact zero. The guard can't
  touch it: it acts only inside sonora.
- **RNNoise after AEC3** doesn't stall with a clean mic: AEC3's output carries its own reference dither through at about -181
  dBFS. With nothing dithered anywhere, it does stall, in both builds (chain p50 260).
- **UL-UNAS** doesn't stall under any policy. Its model costs about 0.9 ms (881-946 us), inline here; live it runs on the
  inference worker. After a stalled AEC3 it costs 19-24 us more, and the guard removes that too.
- **The gate, floor and sanitising:** p99 up to 4.5 us in RNNoise's clean-mic tail, against 0.6.

**Sonora's guard with today's dither.** Over 91 AEC3 cell phases the median paired off-minus-on difference is -0.05 us (single
cells -0.75 to +0.45), inside the run-to-run noise. 419 of 427 cell phases were bit-identical across the builds; the 8
others are all "bare" (nothing dithered), which is not an app setting.

**Exact zeros.**
- **Float32, with today's dither:** never, in any chain. Bypass settles at -180 dBFS. The filters settle at -174 to -180
  with the gate off and -214 to -220 with the static or auto gate.
- **24-bit** (the engine's exclusive-mode conversion; Windows' shared-mode conversion isn't modelled): exact zero in every
  chain under every policy, and the policies differ by 10 ms at most. After speech with today's dither: bypass at once,
  UL-UNAS from 70 ms, RNNoise from 130 ms, AEC3 chains from 330-370 ms.
- **One case never reaches zero:** AEC3 with the far end playing and the mic at zero. AEC3 itself outputs -75 to -89 dBFS
  there, under every policy.

**Speech.** Each policy is compared against today's dither with another seed, the control, which measures what any -180 dB
perturbation does.
- **Every chain but UL-UNAS:** within the control.
- **UL-UNAS alone, with a clean mic, after digital silence:** off by -35.9 dBFS RMS (-13 peak), against the control's -65.5.
  SI-SDR drops from 3.51 to -0.94 dB (the control's 3.42). A hypothesis, untested: its log-feature clamp at 1e-12.
- **UL-UNAS after AEC3:** 5.7 dB past the control.

## The options

| Option | Chain p99, worst phase, us (AEC3 / RNNoise / AEC3+RNNoise) | Float32 silence exact zero | 24-bit | Speech | Soundness |
|---|---|---|---|---|---|
| A. Today: mic and reference dithered, guard on (stock sonora) | 66 / 47 / 106 | never | yes, every chain | baseline | formally UB |
| B. A with the guard off | 64 / 48 / 107 | never | as A | bit-identical to A | sound; needs the vendored patch |
| C. Reference only, guard on | 66 / 480 / 105 | bypass at once, UL-UNAS 70 ms, RNNoise 1040 ms; AEC3 chains never after speech (-181 to -182 dBFS) | as A | UL-UNAS degraded (above) | UB |
| D. Reference only, guard off | 447 / 481 / 487 | as C | as A | as C | sound |
| E. B, with no mic dither in bypass | as B; bypass 0.6 us p50 | bypass at once; filter chains as A | as A | as A in every filter chain | sound |
| F. E plus an end stage writing exact zeros once the mic has been silent past the chain's tail (TODO 17) | not built | every chain, when chosen | as A or sooner | none expected while the mic is live | sound |

E is assembled from measured cells: bypass under "ref" or "none", plus the filter chains under today's dither. It hasn't
been tested as an implementation, including switching the mic dither on when a filter is switched on.

## Our recommendation (for the owner to decide)

**Option E now, and F later for exact float32 silence in the studio case.**
- **Turn the guard off.** With today's dither it saves nothing measurable and changes no output, so the formally-UB code
  buys nothing.
- **Keep the mic dither whenever a filter runs.** A clean mic stalls RNNoise 10x in either build and degrades UL-UNAS after
  silence.
- **Drop the mic dither in bypass**, the one place a clean mic is free.
- **Add a test** that every block AEC3 receives is dithered: with the guard off and nothing dithered, its tail reached
  1,209 us p99.
- **The engine's reference dither wasn't tested on its own.** The AEC3 stage dithers its reference itself, so "ref" and
  "none" behave the same. Keeping both is the conservative choice.

## What this can't settle
- **Other CPUs.** One Xeon VM. Denormal penalties depend on the CPU, so rerun this on the owner's AMD desktops.
- **The live engine.** Offline, the blocks ran back to back, the reference was block-aligned, and UL-UNAS ran inline, not
  on its worker. A live run with mute toggled is the check.
- **Real mutes.** Whether a mute delivers exact zeros: a live mic's noise floor is far above the dither, so only digital
  silence triggers any of this. That is an inference, not measured.
- **The material.** One speech pair, one synthetic room echo, one control seed. Nobody has listened to the UL-UNAS
  difference.
- **Silence length.** No silence longer than 15 s was tested.
- **The earlier figures.** tests/timing.rs measured 47.2 against 46.2 us for the guard earlier; this harness puts any saving
  under 0.2 us at the median.
