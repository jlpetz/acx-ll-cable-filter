"""A one-off cross-check of nf-ort's ported metric features (TODO 14) against numpy, run by hand:
    NF_METRIC_DUMP=DIR cargo test -p nf-ort --test ort dump_metric_features -- --ignored
    python -I tools/check_metric_features.py DIR [--table-0.4 kaiser_best.npz] [--table-0.2 kaiser_best.npz]
- SIGMOS: recomputes the model's input from signal.f32 the way ICASSP2024/sigmos/sigmos.py (SIG-Challenge bf45251) frames it,
  written out here with numpy alone (np.pad, strided frames, np.fft.rfft for scipy.fft.rfft: both pocketfft), and compares it
  with sigmos_features.f32 (nf-ort's `Sigmos::features`).
- DNSMOS's resampling: recomputes signal.f32 at 16 kHz as dnsmos_local.py (DNS-Challenge 591184a) gets it, through librosa
  0.8.1's `resample` (its requirements.txt's pin) and resampy's `resample` and `resample_f` (0.4.3's core.py and interpn.py,
  the loop written out over numpy arrays), with resampy 0.3-0.4.3's kaiser_best, and compares it with dnsmos_16k.f32 (nf-ort's
  `resample::decimate3`); then, for scale, how far resampy 0.2.2's kaiser_best would have put it.
- The filters' tables are rebuilt by resampy's `sinc_window` (np.sinc, and scipy's Kaiser window written out with np.i0), with
  the parameters nf-ort uses. Given resampy's own kaiser_best.npz files (`--table-0.4`, `--table-0.2`: downloads, so untrusted
  data, read with allow_pickle=False), it checks the rebuilt tables against them and resamples with theirs.
The upstream scripts themselves are never run (downloads are untrusted data). Needs numpy only.
"""
import argparse
import sys
from pathlib import Path

import numpy as np

# resampy's kaiser_best: (zeros, points a crossing, Kaiser beta, roll-off). 0.3.0 on: beta solved from its table (resampy
# keeps only the table and the roll-off); 0.2.2: its docstring's.
KAISER_BEST_0_4 = (50, 8192, 12.984585247040012, 0.9173473712608761)
KAISER_BEST_0_2 = (64, 512, 14.769656459379492, 0.9475937167399596)


def sigmos_features(signal):
    """sigmos.py's stft and compressed_mag_complex, transcribed."""
    dft_size, frame_size, window_length = 960, 480, 960
    window = np.sqrt(np.hanning(int(window_length) + 1)[:-1]).astype(np.float32)
    last_frame = len(signal) % frame_size
    if last_frame == 0:
        last_frame = frame_size
    padded = np.pad(signal, ((window_length - frame_size, window_length - last_frame),))
    n = 1 + (len(padded) - window_length) // frame_size
    frames = np.lib.stride_tricks.as_strided(padded, shape=(n, window_length), strides=(padded.strides[0] * frame_size, padded.strides[0]))
    spec = np.fft.rfft(frames * window, n=dft_size).astype(np.complex64)
    x = spec.view(np.float32).reshape(spec.shape + (2,)).swapaxes(-1, -2)
    x2 = np.maximum((x * x).sum(axis=-2, keepdims=True), 1e-12)
    x = np.power(x2, (0.3 - 1) / 2) * x
    mag = np.power(x2, 0.3 / 2)
    features = np.concatenate((mag, x), axis=-2)
    return np.expand_dims(np.transpose(features, (1, 0, 2)), 0)


def sinc_window(zeros, precision, beta, rolloff):
    """resampy's filters.sinc_window with window=scipy.signal.windows.kaiser(beta), the Kaiser window written out."""
    num_bits = precision
    n = num_bits * zeros
    sinc_win = rolloff * np.sinc(rolloff * np.linspace(0, zeros, num=n + 1, endpoint=True))
    m = np.arange(0, 2 * n + 1)
    alpha = (2 * n + 1 - 1) / 2.0
    taper = (np.i0(beta * np.sqrt(1 - ((m - alpha) / alpha) ** 2.0)) / np.i0(float(beta)))[n:]
    return taper * sinc_win, num_bits


def resampy_resample(x, sr_orig, sr_new, interp_win, precision):
    """resampy 0.4.3's resample for one channel (core.py), and its _resample_loop (interpn.py) over all the output samples at
    once: the same arithmetic per sample, in the same order (each output's left wing, i ascending, then its right wing)."""
    sample_ratio = float(sr_new) / sr_orig
    n_out = int(x.shape[-1] * float(sr_new) / float(sr_orig))
    if n_out < 1:
        raise ValueError("too short")
    time_increment = 1.0 / sample_ratio
    t_out = np.arange(n_out) * time_increment
    if sample_ratio < 1:
        interp_win = sample_ratio * interp_win
    interp_delta = np.diff(interp_win, append=interp_win[-1])
    scale = min(1.0, sample_ratio)
    num_table = precision
    index_step = int(scale * num_table)
    nwin = interp_win.shape[0]
    n_orig = x.shape[0]
    y = np.zeros(n_out, dtype=x.dtype)
    n = t_out.astype(np.int64)  # int(time_register): t_out >= 0
    frac = scale * (t_out - n)
    index_frac = frac * num_table
    offset = index_frac.astype(np.int64)
    eta = index_frac - offset
    i_max = np.minimum(n + 1, (nwin - offset) // index_step)
    for i in range(int(i_max.max())):
        k = i < i_max
        j = offset[k] + i * index_step
        y[k] += (interp_win[j] + eta[k] * interp_delta[j]) * x[n[k] - i]
    frac = scale - frac
    index_frac = frac * num_table
    offset = index_frac.astype(np.int64)
    eta = index_frac - offset
    k_max = np.minimum(n_orig - n - 1, (nwin - offset) // index_step)
    for kk in range(int(k_max.max())):
        k = kk < k_max
        j = offset[k] + kk * index_step
        y[k] += (interp_win[j] + eta[k] * interp_delta[j]) * x[n[k] + kk + 1]
    return y


def librosa_resample(y, orig_sr, target_sr, table):
    """librosa 0.8.1's resample(y, orig_sr, target_sr) with res_type="kaiser_best": resampy, then fix_length to the ceiling."""
    ratio = float(target_sr) / orig_sr
    n_samples = int(np.ceil(y.shape[-1] * ratio))
    y_hat = resampy_resample(y, orig_sr, target_sr, *table)
    if len(y_hat) < n_samples:  # util.fix_length, mode="constant"
        y_hat = np.pad(y_hat, (0, n_samples - len(y_hat)))
    return np.asfortranarray(y_hat[:n_samples], dtype=y.dtype)


def table(params, path, name):
    """A filter's table rebuilt; checked against resampy's file and that used instead, if given."""
    rebuilt, precision = sinc_window(*params)
    if path is None:
        return rebuilt, precision
    with np.load(path, allow_pickle=False) as d:
        theirs, their_precision, rolloff = d["half_window"], int(d["precision"]), float(d["rolloff"])
    rel = np.abs(rebuilt - theirs) / np.maximum(np.abs(theirs), 1e-300) if theirs.shape == rebuilt.shape else None
    if rel is None or their_precision != precision or rolloff != params[3]:
        print(f"  {name}: {path} isn't this filter ({theirs.shape}, precision {their_precision}, roll-off {rolloff!r})")
        sys.exit(1)
    big = np.abs(theirs) > 1e-12
    print(f"  {name}: the rebuilt table is resampy's to {rel[big].max():.1e} (relative), {theirs.size} entries")
    return theirs, their_precision


def band_db(a, b):
    """a's difference from b per band of 16 kHz audio, against b, in dB."""
    spec_a, spec_b = np.fft.rfft(a * np.hanning(len(a))), np.fft.rfft(b * np.hanning(len(b)))
    hz = np.fft.rfftfreq(len(a), 1 / 16000)
    out = []
    for lo, hi in ((0, 4000), (4000, 7000), (7000, 7500), (7500, 8000)):
        m = (hz >= lo) & (hz < hi)
        out.append(f"{lo}-{hi} Hz {10 * np.log10(np.sum(np.abs(spec_a[m] - spec_b[m]) ** 2) / np.sum(np.abs(spec_b[m]) ** 2)):.1f} dB")
    return ", ".join(out)


def main(argv):
    ap = argparse.ArgumentParser()
    ap.add_argument("dir")
    ap.add_argument("--table-0.4", dest="t04", help="resampy 0.3-0.4.3's data/kaiser_best.npz")
    ap.add_argument("--table-0.2", dest="t02", help="resampy 0.2.2's data/kaiser_best.npz")
    args = ap.parse_args(argv)
    d = Path(args.dir)
    signal = np.fromfile(d / "signal.f32", dtype="<f4").astype(np.float64)  # soundfile reads float64
    ours = np.fromfile(d / "sigmos_features.f32", dtype="<f4")
    ref = sigmos_features(signal)
    print(f"SIGMOS features: shape {ref.shape}; ours {ours.size} values")
    ref = ref.ravel()
    if ref.size != ours.size:
        print("FAILED: the sizes differ")
        return 1
    err = np.abs(ref - ours)
    rel = err / np.maximum(np.abs(ref), 1e-3)
    if err.max() == 0:
        print("  identical, value for value")
    else:
        rms_db = 20 * np.log10(np.sqrt((err ** 2).mean()) / np.sqrt((ref ** 2).mean()))
        print(f"  max |diff| {err.max():.3g}, max relative {rel.max():.3g}, RMS error {rms_db:.1f} dB")
    ok = rel.max() < 1e-4

    print("DNSMOS's 48 to 16 kHz resampling (librosa 0.8.1, resampy kaiser_best):")
    new, old = table(KAISER_BEST_0_4, args.t04, "resampy 0.3-0.4.3"), table(KAISER_BEST_0_2, args.t02, "resampy 0.2.2")
    ours16 = np.fromfile(d / "dnsmos_16k.f32", dtype="<f4")
    theirs = librosa_resample(signal, 48000, 16000, new)
    print(f"  {len(signal)} samples in, {len(theirs)} out (ceil(n / 3)); ours {len(ours16)}")
    if len(theirs) != len(ours16):
        print("FAILED: the lengths differ")
        return 1
    theirs32 = theirs.astype(np.float32)
    differ = np.flatnonzero(theirs32 != ours16)
    ulps = np.abs(theirs32.view(np.int32).astype(np.int64) - ours16.view(np.int32).astype(np.int64))
    same = "every one identical" if len(differ) == 0 else f"{len(theirs) - len(differ)} identical, the rest within {ulps.max()} ulp"
    print(f"  against resampy 0.3-0.4.3's in float32: {same} (float64's error in them at most "
          f"{np.abs(theirs - ours16.astype(np.float64)).max():.1e}); the last {len(theirs) - int(len(signal) * 16000.0 / 48000.0)} librosa's padding")
    ok = ok and ulps.max() <= 1
    older = librosa_resample(signal, 48000, 16000, old)
    a, b = older[2000:-2000], theirs[2000:-2000]
    print(f"  resampy 0.2.2's would differ from it by: {band_db(a, b)}")
    print("OK" if ok else "FAILED")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
