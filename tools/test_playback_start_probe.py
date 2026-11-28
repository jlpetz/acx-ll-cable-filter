"""Offline controls for the finite Media Player marker, including losses hidden by later alignment."""
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
try:
    import numpy as np
except ModuleNotFoundError as exc:
    if exc.name != 'numpy':
        raise
    np = None

if np is not None:
    from playback_start_probe import FADE, check, marker


@unittest.skipIf(np is None, 'needs NumPy for the playback marker')
class PlaybackStartProbeTests(unittest.TestCase):
    def setUp(self):
        self.source = marker()
        self.take = np.pad(self.source, ((2 * FADE, 3 * FADE), (0, 0)))
        self.audio = self.take[2 * FADE:2 * FADE + len(self.source)]
        ramp = np.arange(FADE)[:, None] / FADE
        self.audio[:FADE] = np.rint(self.audio[:FADE] * ramp)

    def test_shared_fade_and_asymmetric_silence_guards(self):
        message, result = check(self.take, self.source)
        self.assertEqual(result, 0, message)
        self.assertRegex(message, r'continuous stereo match from source frame 1\b')

    def test_missing_onset_is_measured_on_the_source_clock(self):
        self.audio[:416] = 0
        message, result = check(self.take, self.source)
        self.assertEqual(result, 1, message)
        self.assertRegex(message, r'continuous stereo match from source frame 416\b')
        self.assertIn('wrong frames after fade 0', message)

    def test_quantized_fade_starts_with_several_quiet_stereo_samples(self):
        self.audio[:6] = np.sign(self.source[:6]) * np.array([[0, 0], [1, 1], [2, 2], [3, 2], [3, 4], [5, 5]])
        message, result = check(self.take, self.source)
        self.assertEqual(result, 0, message)
        self.assertRegex(message, r'continuous stereo match from source frame 1\b')

    def test_sparse_dither_cannot_hide_missing_fade(self):
        self.audio[:415] = 0
        self.audio[1:415:7] = np.sign(self.source[1:415:7])
        message, result = check(self.take, self.source)
        self.assertEqual(result, 1, message)
        # Frame 414 is a chance two-channel sign match immediately adjacent to the real onset.
        self.assertRegex(message, r'continuous stereo match from source frame 414\b')

    def test_a_quiet_hole_inside_the_fade_fails(self):
        self.audio[300:327] = 0
        message, result = check(self.take, self.source)
        self.assertEqual(result, 1, message)
        self.assertRegex(message, r'continuous stereo match from source frame 327\b')

    def test_right_channel_loss_at_the_final_frame_fails(self):
        self.audio[-1, 1] = 0
        message, result = check(self.take, self.source)
        self.assertEqual(result, 1, message)
        self.assertIn('wrong frames after fade 1', message)

    def test_an_absent_marker_is_inconclusive(self):
        self.assertEqual(check(np.zeros_like(self.take), self.source)[1], 2)


if __name__ == '__main__':
    unittest.main()
