"""Generate and check a 48 kHz stereo PCM16 playback-start marker. Requires NumPy. See doc/dev/playback-start.md."""
import argparse
from pathlib import Path
import sys
import wave

import numpy as np


RATE = 48000
FRAMES = 2 * RATE + 137
FADE = 480


def marker():
    rng = np.random.Generator(np.random.PCG64(3837590))
    magnitude = rng.integers(2400, 4800, size=(FRAMES, 2), dtype=np.int16)
    magnitude *= 2 * rng.integers(0, 2, size=magnitude.shape, dtype=np.int16) - 1
    return magnitude


def check(recording, source):
    # Find the source timeline using the interior, independently of how much of its opening survived.
    anchor = 2 * FADE
    length = 192
    if len(recording) < len(source):
        return 'INCONCLUSIVE: capture the whole clip with silence on both sides', 2
    signal = recording[:, 0].astype(np.float64)
    reference = source[anchor:anchor + length, 0].astype(np.float64)
    scores = np.correlate(signal, reference, mode='valid')
    offset = int(np.argmax(scores)) - anchor
    body = signal[offset + anchor:offset + anchor + length] if offset >= 0 else np.empty(0)
    if body.size != length or np.linalg.norm(body) == 0 or np.dot(body, reference) / (np.linalg.norm(body) * np.linalg.norm(reference)) < .99:
        return 'INCONCLUSIVE: no matching interior marker in the recording', 2
    if offset < FADE or offset + len(source) + FADE > len(recording):
        return 'INCONCLUSIVE: missing the silence guard around this playback', 2
    captured = recording[offset:offset + len(source)].astype(np.float64)
    expected = source.astype(np.float64)
    gain = (captured[FADE:] * expected[FADE:]).sum() / np.square(expected[FADE:]).sum()
    # Permit a few PCM16 LSBs of rounding/dither in the guards and steady signal.
    rounding_tolerance = 4.0
    if not .25 <= gain <= 1.1:
        return 'INCONCLUSIVE: use unity gain and record the specified cable without processing', 2
    guards = np.concatenate((recording[offset - FADE:offset], recording[offset + len(source):offset + len(source) + FADE]))
    if np.abs(guards.astype(np.int32)).max() > rounding_tolerance:
        return 'INCONCLUSIVE: unrelated sound, duplicated tail, or wrong alignment in the silence guards', 2
    # A real WMP fade starts at one PCM16 LSB. Follow both channels' seeded signs, preserving these quiet
    # samples while rejecting isolated dither hits. Use the complete matching suffix so an interior hole in
    # the fade cannot be hidden by a coincidental match near frame zero.
    opening = captured[:FADE]
    matches = np.all(np.sign(opening) == np.sign(expected[:FADE]), axis=1)
    mismatches = np.flatnonzero(~matches)
    first = int(mismatches[-1] + 1) if mismatches.size else 0
    loud = np.max(np.abs(opening), axis=1) > rounding_tolerance
    wrong_sign = (~matches) & loud
    errors = np.any(np.abs(captured[FADE:] - expected[FADE:] * gain) > rounding_tolerance, axis=1)
    valid = first <= 2 and loud.any() and not errors.any() and not wrong_sign.any()
    info = (f'continuous stereo match from source frame {first} of the {FADE}-frame opening, '
            f'wrong frames after fade {int(errors.sum())}, opening sign errors {int(wrong_sign.sum())}, gain {gain:.4f}')
    return ('PASS: ' if valid else 'FAIL: ') + info, int(not valid)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('operation', choices=('make', 'check'))
    parser.add_argument('wav', type=Path)
    args = parser.parse_args()
    source = marker()
    if args.operation == 'make':
        with wave.open(str(args.wav), 'wb') as f:
            f.setparams((2, 2, RATE, 0, 'NONE', 'not compressed'))
            f.writeframes(source.astype('<i2').tobytes())
        return 0
    with wave.open(str(args.wav), 'rb') as f:
        if (f.getnchannels(), f.getsampwidth(), f.getframerate(), f.getcomptype()) != (2, 2, RATE, 'NONE'):
            parser.error('export an uncompressed 48 kHz stereo PCM16 WAV')
        recording = np.frombuffer(f.readframes(f.getnframes()), dtype='<i2').reshape(-1, 2)
    message, result = check(recording, source)
    print(message)
    return result


if __name__ == '__main__':
    sys.exit(main())
