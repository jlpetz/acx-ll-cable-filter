"""Offline tests for tools/fetch_test_audio.py: retries, size caps and redirects, the Range zip reader (through a fake
opener serving a local zip), member-name safety, the recipe (fades, joins, downmix, resampling, 16-bit output), and the
manifest and ATTRIBUTION.md.
Run from any directory:
    python -I tools/test_fetch_test_audio.py
The audio tests need numpy, soundfile and soxr (python -m pip install numpy soundfile soxr) and are skipped without them.
"""
import contextlib
import dataclasses
import email.message
import hashlib
import io
import json
import os
import re
import struct
import sys
import tempfile
import unittest
import urllib.error
import urllib.request
import wave
import zipfile
from pathlib import Path

sys.dont_write_bytecode = True  # before the import: no __pycache__ in tools/
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import fetch_test_audio as fta  # noqa: E402

try:
    import numpy as np
    import soundfile as sf
    import soxr  # noqa: F401
    HAVE_AUDIO = True
except ImportError:
    HAVE_AUDIO = False

URL = "https://example.test/set.zip"


class FakeResponse(io.BytesIO):
    def __init__(self, status, headers, body):
        super().__init__(body)
        self.status, self.headers = status, headers


class FakeOpener:
    """Serves bytes by URL, honouring Range as a server would (or ignoring it, with ranges=False), and logs each request."""

    def __init__(self, files, ranges=True):
        self.files, self.ranges, self.requests = files, ranges, []

    def open(self, req, timeout=None):
        data = self.files[req.full_url]
        rng = req.get_header("Range")
        self.requests.append((req.full_url, rng))
        m = re.fullmatch(r"bytes=(\d+)-(\d+)", rng or "")
        if m and self.ranges:
            a, b = int(m[1]), min(int(m[2]), len(data) - 1)
            return FakeResponse(206, {"Content-Range": f"bytes {a}-{b}/{len(data)}"}, data[a:b + 1])
        return FakeResponse(200, {"Content-Length": str(len(data))}, data)

    def bytes_requested(self):
        return sum(int(b) - int(a) + 1 for _, r in self.requests for a, b in [r[6:].split("-")])


def http(opener):
    return fta.Http(opener, sleep=lambda s: None)


class ScriptedOpener:
    """Answers each request with the next item of its script: an exception to raise, or (status, headers, body)."""

    def __init__(self, *script):
        self.script, self.requests = list(script), []

    def open(self, req, timeout=None):
        self.requests.append((req.full_url, req.get_header("Range")))
        item = self.script.pop(0)
        if isinstance(item, BaseException):
            raise item
        return FakeResponse(*item)


def http_error(code, retry_after=None):
    headers = email.message.Message()
    if retry_after is not None:
        headers["Retry-After"] = str(retry_after)
    return urllib.error.HTTPError(URL, code, "msg", headers, io.BytesIO())


class Retries(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dest, self.sleeps = Path(self.tmp.name) / "f.bin", []
        self.out = self.enterContext(contextlib.redirect_stdout(io.StringIO()))  # the "retrying in" lines

    def tearDown(self):
        self.tmp.cleanup()

    def http(self, *script):
        return fta.Http(ScriptedOpener(*script), sleep=self.sleeps.append)

    def test_429_and_5xx_are_retried_honouring_retry_after(self):
        h = self.http(http_error(429, 7), http_error(503), (200, {"Content-Length": "3"}, b"abc"))
        part, n, sha, _ = h.download(URL, self.dest)
        self.assertEqual((part.read_bytes(), n, sha), (b"abc", 3, hashlib.sha256(b"abc").hexdigest()))
        self.assertEqual((h.requests, self.sleeps), (3, [7.0, 10.0]))

    def test_404_isnt_retried_and_the_last_try_gives_up(self):
        h = self.http(http_error(404))
        with self.assertRaisesRegex(fta.FetchError, "HTTP 404"):
            h.download(URL, self.dest)
        self.assertEqual((h.requests, self.sleeps), (1, []))
        h = self.http(*[http_error(429, 99999)] * 6)
        with self.assertRaisesRegex(fta.FetchError, "HTTP 429"):
            h.read_range(URL, 0, 10)
        self.assertEqual((h.requests, self.sleeps), (6, [300.0] * 5))  # Retry-After is capped at 300 s

    def test_a_short_body_is_retried_not_passed_on(self):
        h = self.http((200, {"Content-Length": "100"}, b"x" * 60), (200, {"Content-Length": "100"}, b"y" * 100))
        part, n, _, _ = h.download(URL, self.dest)
        self.assertEqual((part.read_bytes(), h.requests, self.sleeps), (b"y" * 100, 2, [5.0]))
        self.assertIn("the connection dropped after 60 of 100 bytes; retrying in 5 s", self.out.getvalue())
        rng = {"Content-Range": "bytes 0-99/1000"}
        h = self.http((206, rng, b"x" * 60), (206, rng, b"z" * 100))
        self.assertEqual((h.read_range(URL, 0, 100), h.requests), ((b"z" * 100, 1000), 2))

    def test_downloads_are_capped_and_leave_no_part_file(self):
        with self.assertRaisesRegex(fta.FetchError, "100 bytes, more than the 10 expected"):
            self.http((200, {"Content-Length": "100"}, b"x" * 100)).download(URL, self.dest, size=10)
        with self.assertRaisesRegex(fta.FetchError, "sent more than the 10 bytes"):
            self.http((200, {}, b"x" * (3 << 20))).download(URL, self.dest, size=10)  # no Content-Length: caught streaming
        self.assertEqual(list(Path(self.tmp.name).iterdir()), [])

    def test_redirects_must_stay_on_https(self):
        req = urllib.request.Request(URL, headers={"Range": "bytes=0-9"})
        redirect = lambda to: fta.HttpsOnlyRedirects().redirect_request(req, None, 302, "Found", email.message.Message(), to)  # noqa: E731
        for bad in ("http://example.test/set.zip", "file:///C:/Windows/win.ini"):
            with self.assertRaisesRegex(fta.FetchError, "isn't https", msg=bad):
                redirect(bad)
        new = redirect("https://mirror.example.test/set.zip")
        self.assertEqual((new.full_url, new.get_header("Range")), ("https://mirror.example.test/set.zip", "bytes=0-9"))


def make_zip(members, comment=b""):
    """A zip of (name, bytes, method) members, written by the stdlib."""
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        for name, data, method in members:
            z.writestr(zipfile.ZipInfo(name, (2026, 11, 1, 0, 0, 0)), data, compress_type=method)
        z.comment = comment
    return buf.getvalue()


def to_zip64(data):
    """The same zip with its end records in ZIP64 form (the 32-bit fields set to their 'see ZIP64' values), like VCTK's."""
    i = data.rindex(b"PK\x05\x06")
    count, cd_size, cd_at = struct.unpack_from("<HII", data, i + 10)
    record = struct.pack("<4sQHHIIQQQQ", b"PK\x06\x06", 44, 45, 45, 0, 0, count, count, cd_size, cd_at)
    locator = struct.pack("<4sIQI", b"PK\x06\x07", 0, i, 1)
    eocd = struct.pack("<4sHHHHIIH", b"PK\x05\x06", 0, 0, 0xFFFF, 0xFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0)
    return data[:i] + record + locator + eocd


FILLER = bytes(range(256)) * 8192  # 2 MiB that we never want to download
WAV_BYTES = b"RIFF....WAVEfmt " + bytes(range(200)) * 50
TEXT = b"Please call Stella. " * 200


class ZipByRange(unittest.TestCase):
    def setUp(self):
        self.zip = make_zip([("big/filler.bin", FILLER, zipfile.ZIP_STORED), ("48k/FA/FA01_01.wav", WAV_BYTES, zipfile.ZIP_STORED),
                             ("txt/p225_001.txt", TEXT, zipfile.ZIP_DEFLATED)], comment=b"a trailing archive comment")

    def test_members_come_back_intact_from_a_few_small_reads(self):
        opener = FakeOpener({URL: self.zip})
        f = fta.RemoteFile(http(opener), URL, len(self.zip))
        members = fta.central_directory(f)
        self.assertEqual(list(members), ["big/filler.bin", "48k/FA/FA01_01.wav", "txt/p225_001.txt"])
        self.assertEqual(fta.read_member(f, members["48k/FA/FA01_01.wav"]), WAV_BYTES)
        self.assertEqual(fta.read_member(f, members["txt/p225_001.txt"]), TEXT)
        self.assertTrue(all(r for _, r in opener.requests), "every request is a Range request")
        self.assertLessEqual(len(opener.requests), 4)  # the tail, the directory, one per member
        self.assertLess(opener.bytes_requested(), 100_000)  # the 2 MiB filler is never read

    def test_an_unknown_size_is_learnt_from_a_one_byte_read(self):
        opener = FakeOpener({URL: self.zip})
        self.assertEqual(fta.RemoteFile(http(opener), URL).size, len(self.zip))
        self.assertEqual(opener.requests, [(URL, "bytes=0-0")])

    def test_zip64_end_records_are_followed(self):
        z64 = to_zip64(self.zip)
        f = fta.RemoteFile(http(FakeOpener({URL: z64})), URL, len(z64))
        members = fta.central_directory(f)
        self.assertEqual(fta.read_member(f, members["txt/p225_001.txt"]), TEXT)

    def test_zip64_extra_fields_replace_only_the_placeholders(self):
        timestamp = struct.pack("<HHB4s", 0x5455, 5, 1, b"\0\0\0\0")  # an unrelated field first
        both = timestamp + struct.pack("<HHQQ", 1, 16, 5_000_000_000, 4_000_000_000)
        self.assertEqual(fta.zip64_extra(both, 0xFFFFFFFF, 0xFFFFFFFF, 7), (5_000_000_000, 4_000_000_000, 7))
        self.assertEqual(fta.zip64_extra(struct.pack("<HHQ", 1, 8, 6_000_000_000), 10, 10, 0xFFFFFFFF), (10, 10, 6_000_000_000))
        with self.assertRaisesRegex(fta.FetchError, "too short"):
            fta.zip64_extra(struct.pack("<HHQ", 1, 8, 1), 0xFFFFFFFF, 0xFFFFFFFF, 0)

    def test_a_corrupt_member_fails_its_crc(self):
        i = self.zip.index(WAV_BYTES) + 100
        bad = self.zip[:i] + bytes([self.zip[i] ^ 1]) + self.zip[i + 1:]
        f = fta.RemoteFile(http(FakeOpener({URL: bad})), URL, len(bad))
        with self.assertRaisesRegex(fta.FetchError, "CRC-32"):
            fta.read_member(f, fta.central_directory(f)["48k/FA/FA01_01.wav"])

    def test_inflating_stops_at_the_stated_size_and_the_cap(self):
        f = fta.RemoteFile(http(FakeOpener({URL: self.zip})), URL, len(self.zip))
        m = fta.central_directory(f)["txt/p225_001.txt"]
        with self.assertRaisesRegex(fta.FetchError, "is 1,001 bytes, not the 1,000"):
            fta.read_member(f, dataclasses.replace(m, usize=1000))  # a directory entry that understates a bomb
        with self.assertRaisesRegex(fta.FetchError, "over the 100-byte cap"):
            fta.read_member(f, m, cap=100)

    def test_a_server_that_ignores_range_is_refused(self):
        with self.assertRaisesRegex(fta.FetchError, "ignored the Range request"):
            fta.central_directory(fta.RemoteFile(http(FakeOpener({URL: self.zip}, ranges=False)), URL, len(self.zip)))

    def test_a_file_that_changed_upstream_is_caught(self):
        opener = FakeOpener({URL: self.zip})
        f = fta.RemoteFile(http(opener), URL, len(self.zip))
        opener.files[URL] = self.zip + b"appended"
        with self.assertRaisesRegex(fta.FetchError, "changed upstream"):
            fta.central_directory(f)

    def test_something_that_isnt_a_zip_is_refused(self):
        junk = b"<html>not found</html>" * 10
        with self.assertRaisesRegex(fta.FetchError, "not a zip"):
            fta.central_directory(fta.RemoteFile(http(FakeOpener({URL: junk})), URL, len(junk)))


class Names(unittest.TestCase):
    def test_ordinary_names_pass(self):
        for name in ("48k/FA/FA01_01.wav", "wav48_silence_trimmed/p225/p225_003_mic1.flac", "05 Variatio 4 a 1 Clav.flac",
                     "448209__florianreichelt__keyboard-typing.wav", "PCAFETER/ch01.wav", "console.wav"):
            self.assertEqual(fta.safe_parts(name), name.split("/"))

    def test_traversal_and_windows_tricks_are_refused(self):
        for name in ("", "../x.wav", "a/../../x.wav", "/etc/passwd", "C:/x.wav", "C:x.wav", "a\\..\\x.wav", "a//b", "./a",
                     "a/.", "x.wav:stream", "nul", "CON.wav", "com1.txt", "lpt9", "a/aux/b.wav", "conin$", "x.wav.", "x.wav ",
                     "a\x00b", "a\nb", 'a"b', "a?b", "a*b", "a|b", "a<b", None, 123, "x" * 401):
            with self.assertRaises(fta.FetchError, msg=repr(name)):
                fta.safe_parts(name)

    def test_inside_refuses_paths_that_resolve_outside_the_root(self):
        with tempfile.TemporaryDirectory() as d:
            self.assertEqual(fta.inside(d, "raw", "x.wav"), Path(d).resolve() / "raw" / "x.wav")
            for parts in ((), ("..", "x.wav"), ("raw", "..", "..", "x.wav"), (os.path.abspath(os.sep),)):
                with self.assertRaises(fta.FetchError, msg=repr(parts)):
                    fta.inside(d, *parts)

    def test_the_sources_file_is_checked(self):
        good = sources_doc()
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "s.json"
            for spoil in (lambda doc: doc["outputs"][0]["inputs"].append("no-such-member"),
                          lambda doc: doc["sources"][0].update(url="http://example.test/set.zip"),
                          lambda doc: doc["sources"][0]["files"].append({"name": "other/a.wav"}),  # same name in raw/
                          lambda doc: doc["sources"][0]["files"][0].update(name="../a.wav"),
                          lambda doc: doc["outputs"][0].update(name="..\\evil"),
                          lambda doc: doc["sources"][2]["files"][0].update(name="111*"),
                          lambda doc: doc["outputs"][0].update(gap_s=-1)):
                doc = json.loads(json.dumps(good))
                spoil(doc)
                path.write_text(json.dumps(doc), encoding="utf-8")
                with self.assertRaises(fta.FetchError):
                    fta.load_sources(path)
            path.write_text(json.dumps(good), encoding="utf-8")
            self.assertEqual(fta.load_sources(path), good)

    def test_the_json_writer_round_trips_and_keeps_short_values_on_one_line(self):
        doc = sources_doc()
        text = fta.dump_json(doc)
        self.assertEqual(json.loads(text), doc)
        lines = [line.strip() for line in text.splitlines()]
        self.assertIn('"files": [{"name": "sub/a.wav"}, {"name": "sub/b.wav"}]', lines)  # fits, so stays whole
        self.assertIn('{"name": "111", "title": "Fan", "author": "someone", "page": "https://freesound.org/s/111/"},', lines)
        self.assertLessEqual(max(map(len, text.splitlines())), 130)


def tone(n, rate=fta.RATE, amp=0.5, hz=1000.0):
    return amp * np.sin(2 * np.pi * hz * np.arange(n) / rate)


@unittest.skipUnless(HAVE_AUDIO, "needs numpy, soundfile and soxr")
class Recipe(unittest.TestCase):
    def test_downmix_averages_the_channels(self):
        self.assertEqual(fta.downmix(np.array([[1.0, 3.0], [5.0, 7.0]])).tolist(), [2.0, 6.0])
        self.assertEqual(fta.downmix(np.array([[1.0], [2.0]])).tolist(), [1.0, 2.0])

    def test_join_puts_the_gap_between_pieces_only(self):
        self.assertEqual(fta.join([np.ones(3), 2 * np.ones(2)], 2).tolist(), [1, 1, 1, 0, 0, 2, 2])
        self.assertEqual(fta.join([np.ones(3)], 2).tolist(), [1, 1, 1])
        self.assertEqual(len(fta.join([], 2)), 0)

    def test_fades_ramp_only_the_edges(self):
        y = fta.fade(np.ones(100), 10)
        self.assertLess(y[0], 0.03)
        self.assertTrue(np.all(np.diff(y[:10]) > 0))
        self.assertTrue(np.all(y[10:90] == 1.0))
        self.assertTrue(np.allclose(y, y[::-1]))
        self.assertEqual(len(fta.fade(np.ones(5), 10)), 5)  # a fade longer than half the piece is shortened

    def test_trim(self):
        x = np.arange(2 * fta.RATE, dtype=float)
        y = fta.trim(x, 0.5, 1.0)
        self.assertEqual((len(y), y[0]), (fta.RATE, fta.RATE // 2))
        with self.assertRaises(fta.FetchError):
            fta.trim(x, 1.5, 1.0)

    def test_16_bit_values_come_back_unchanged_and_only_overs_count_as_clipped(self):
        ints = np.array([-32768, -1, 0, 1, 32767])
        pcm, clipped = fta.to_pcm16(ints / 32768.0)
        self.assertEqual((pcm.tolist(), clipped), (ints.tolist(), 0))
        pcm, clipped = fta.to_pcm16(np.array([1.5, -1.5, 8388607 / 8388608]))  # the last is 24-bit full scale
        self.assertEqual((pcm.tolist(), clipped), ([32767, -32768, 32767], 2))

    def test_the_wav_is_plain_16_bit_pcm(self):
        data = fta.wav_pcm16(np.array([0, 1, -1, 32767, -32768], dtype="<i2"))
        with wave.open(io.BytesIO(data)) as w:
            self.assertEqual((w.getnchannels(), w.getsampwidth(), w.getframerate(), w.getcomptype(), w.getnframes()),
                             (1, 2, 48_000, "NONE", 5))
            self.assertEqual(np.frombuffer(w.readframes(5), "<i2").tolist(), [0, 1, -1, 32767, -32768])
        self.assertEqual(len(data), 44 + 10)

    def test_convert_downmixes_resamples_fades_and_joins(self):
        with tempfile.TemporaryDirectory() as d:
            a, b = Path(d) / "a.wav", Path(d) / "b.flac"
            sf.write(a, np.column_stack([np.full(4800, 0.5), np.full(4800, -0.25)]), 48_000, subtype="PCM_24")
            sf.write(b, tone(9600, rate=96_000), 96_000, subtype="PCM_16")
            out = {"name": "t", "source": "s", "inputs": ["a", "b"], "gap_s": 0.01, "fade_ms": 1}
            data, infos, changes = fta.convert(out, [fta.Raw(a, "", ""), fta.Raw(b, "", "")])
        y = np.frombuffer(data[44:], "<i2")
        self.assertEqual(len(y), 4800 + 480 + 4800)
        self.assertEqual(y[48:4800 - 48].tolist(), [4096] * (4800 - 96))  # (0.5 - 0.25) / 2, past the 1 ms (48-sample) fades
        self.assertEqual((y[0], y[4800:5280].tolist()), (round(4096 * fta.fade(np.ones(4800), 48)[0]), [0] * 480))
        self.assertGreater(np.abs(y[5280 + 100:5280 + 4700]).max(), 15_000)  # the tone survived resampling at 0.5
        self.assertEqual([i["rate"] for i in infos], [48_000, 96_000])
        self.assertEqual(changes, ["2 recordings joined end to end, with 0.01 s of silence between them", "decoded from FLAC",
                                   "downmixed from 2 channels to mono by averaging them",
                                   "resampled from 96,000 Hz to 48,000 Hz (soxr, very high quality)",
                                   "a 1 ms fade in and out on each recording", "requantised from 24-bit to 16-bit (no dither)"])

    def test_a_plain_source_is_copied_unchanged(self):
        info = {"rate": 48_000, "channels": 1, "format": "WAV", "subtype": "PCM_16", "frames": 10}
        self.assertEqual(fta.describe_changes({"inputs": ["x"]}, [info], 0), ["none: the samples are copied unchanged into a new WAV file"])


def wav16(x):
    return fta.wav_pcm16(np.round(np.asarray(x) * 32768).astype("<i2"))


def sources_doc(pins=None):
    """A small sources file: a zip of two speech members, a file, and two Freesound clips (one present)."""
    pins = pins or {}
    return {
        "schema": 1, "about": ["test"],
        "sources": [
            {"id": "z", "title": "Zip speech", "page": "https://example.test/", "kind": "zip", "url": URL, "licence": "BSD",
             "licence_url": "https://example.test/licence", "tier": "permissive", "attribution": "Speech (c) Someone.",
             "notice": ["Copyright (c) Someone", "All rights reserved."], **pins.get("zip", {}),
             "files": [{"name": "sub/a.wav", **pins.get("sub/a.wav", {})}, {"name": "sub/b.wav", **pins.get("sub/b.wav", {})}]},
            {"id": "f", "title": "A file", "page": "https://example.test/f", "kind": "file", "licence": "CC0 1.0",
             "licence_url": "https://example.test/cc0", "tier": "unlicensed", "attribution": "From the file set.",
             "files": [{"name": "f.wav", "url": "https://example.test/f.wav", **pins.get("f.wav", {})}]},
            {"id": "fs", "title": "Freesound", "page": "https://freesound.org/", "kind": "freesound", "licence": "CC0 1.0",
             "licence_url": "https://example.test/cc0", "tier": "permissive", "attribution": "CC0.",
             "files": [{"name": "111", "title": "Fan", "author": "someone", "page": "https://freesound.org/s/111/"},
                       {"name": "222", "title": "Dog", "author": "else", "page": "https://freesound.org/s/222/"}]},
        ],
        "outputs": [
            {"name": "speech_z", "source": "z", "inputs": ["sub/a.wav", "sub/b.wav"], "gap_s": 0.5},
            {"name": "noise_f", "source": "f", "inputs": ["f.wav"]},
            {"name": "noise_111", "source": "fs", "inputs": ["111"], "attribution": "\"Fan\" by someone, CC0."},
            {"name": "noise_222", "source": "fs", "inputs": ["222"]},
        ],
    }


@unittest.skipUnless(HAVE_AUDIO, "needs numpy, soundfile and soxr")
class ManifestAndAttribution(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.cache, self.downloads = root / "cache", root / "Downloads"
        self.downloads.mkdir()
        self.a, self.b, self.f = wav16(tone(4800)), wav16(tone(2400, amp=0.25)), wav16(np.full(960, 0.125))
        self.zip = make_zip([("sub/a.wav", self.a, zipfile.ZIP_DEFLATED), ("sub/b.wav", self.b, zipfile.ZIP_STORED)])
        self.opener = FakeOpener({URL: self.zip, "https://example.test/f.wav": self.f})
        sha = lambda b: {"size": len(b), "sha256": hashlib.sha256(b).hexdigest()}  # noqa: E731
        self.doc = sources_doc({"zip": {"size": len(self.zip)}, "sub/a.wav": sha(self.a), "sub/b.wav": sha(self.b), "f.wav": sha(self.f)})
        sf.write(self.downloads / "111__someone__fan.wav", np.column_stack([np.full(480, 0.5), np.zeros(480)]), 48_000, subtype="PCM_24")

    def tearDown(self):
        self.tmp.cleanup()

    def run_once(self, pin=False):
        logs = []
        run = fta.Run(self.doc, self.cache, http(self.opener), self.downloads, pin=pin, log=logs.append)
        return run, fta.run_all(run), logs

    def test_manifest_and_attribution_describe_every_output(self):
        run, entries, logs = self.run_once()
        self.assertEqual(run.problems, [])
        self.assertEqual(run.missing, ["222 \"Dog\" by else: https://freesound.org/s/222/"])
        manifest = json.loads((self.cache / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["outputs"], entries)
        self.assertEqual([e["file"] for e in entries], ["wav48/speech_z.wav", "wav48/noise_f.wav", "wav48/noise_111.wav"])
        for e in entries:
            self.assertEqual(e["sha256"], fta.sha256_file(self.cache / e["file"]))
            self.assertEqual((e["rate"], e["channels"], e["bits"]), (48_000, 1, 16))
            self.assertAlmostEqual(e["duration_s"], e["frames"] / 48_000, places=3)
            for i in e["inputs"]:
                self.assertEqual(i["sha256"], fta.sha256_file(self.cache / i["raw"]))
        speech, noise_f, noise_111 = entries
        self.assertEqual(speech["duration_s"], 0.65)  # 4800 + 24000 + 2400 frames
        self.assertEqual([i["member"] for i in speech["inputs"]], ["sub/a.wav", "sub/b.wav"])
        self.assertEqual({i["url"] for i in speech["inputs"]}, {URL})
        self.assertEqual((speech["licence"], speech["licence_url"], speech["tier"]), ("BSD", "https://example.test/licence", "permissive"))
        self.assertEqual(speech["changes"], ["2 recordings joined end to end, with 0.5 s of silence between them"])
        self.assertEqual(noise_f["changes"], ["none: the samples are copied unchanged into a new WAV file"])
        self.assertEqual((self.cache / noise_f["file"]).read_bytes(), self.f)
        self.assertEqual(noise_111["attribution"], "\"Fan\" by someone, CC0.")
        self.assertEqual(noise_111["inputs"][0]["url"], "https://freesound.org/s/111/")
        self.assertIn("downmixed from 2 channels to mono by averaging them", noise_111["changes"])
        md = (self.cache / "ATTRIBUTION.md").read_text(encoding="utf-8")
        for text in ("## wav48/speech_z.wav", "Speech (c) Someone.", "members `sub/a.wav`, `sub/b.wav`", "[BSD](https://example.test/licence)",
                     "tier `unlicensed`", "\"Fan\" by someone, CC0.", "## Notice: Zip speech", "All rights reserved.", speech["sha256"]):
            self.assertIn(text, md)
        self.assertNotIn("noise_222", md)
        self.assertNotIn("Dog", md)
        self.assertTrue(any("MISSING 222" in m and "https://freesound.org/s/222/" in m for m in logs), logs)

    def test_a_second_run_downloads_and_rebuilds_nothing(self):
        self.run_once()
        requests, manifest = len(self.opener.requests), (self.cache / "manifest.json").read_bytes()
        run, entries, logs = self.run_once()
        self.assertEqual(len(self.opener.requests), requests)
        self.assertEqual((self.cache / "manifest.json").read_bytes(), manifest)
        self.assertEqual(sum("up to date" in m for m in logs), 3)
        self.assertIn("manifest.json and ATTRIBUTION.md unchanged: 3 outputs", logs)
        (self.cache / "wav48" / "noise_f.wav").write_bytes(b"spoilt")  # a damaged output is rebuilt
        run, entries, logs = self.run_once()
        self.assertTrue(any(m.startswith("built wav48/noise_f.wav") for m in logs), logs)
        self.assertEqual(len(self.opener.requests), requests)

    def test_a_download_that_doesnt_match_its_pin_is_refused(self):
        self.opener.files["https://example.test/f.wav"] = wav16(np.full(960, 0.25))
        run, entries, logs = self.run_once()
        self.assertEqual(len(run.problems), 1)
        self.assertIn("f: f.wav", run.problems[0])
        self.assertNotIn("wav48/noise_f.wav", [e["file"] for e in entries])
        self.assertFalse((self.cache / "raw" / "f" / "f.wav").exists())
        self.assertEqual(list((self.cache / "raw" / "f").iterdir()), [])  # no .part left behind

    def test_a_wrong_freesound_file_is_explained_and_a_take_that_cant_be_built_is_removed(self):
        self.doc["sources"][2]["files"][0].update(size=1, sha256="0" * 64)  # the owner's download isn't the pinned original
        run, entries, logs = self.run_once()
        self.assertEqual(len(run.problems), 1)
        self.assertRegex(run.problems[0], r"^fs: 111__someone__fan\.wav: .*Download the original again from https://freesound\.org/s/111/$")
        self.assertNotIn("wav48/noise_111.wav", [e["file"] for e in entries])
        stale = self.cache / "wav48" / "noise_f.wav"
        self.assertTrue(stale.is_file())
        for p in (self.cache / "raw" / "f").iterdir():
            p.unlink()
        self.opener.files["https://example.test/f.wav"] = b"changed upstream"
        run, entries, logs = self.run_once()
        self.assertNotIn("wav48/noise_f.wav", [e["file"] for e in entries])
        self.assertFalse(stale.exists())
        self.assertIn("skipped wav48/noise_f.wav: its input isn't in raw/ (removed its old copy)", logs)

    def test_pin_records_what_has_no_pin_and_refuses_without_it(self):
        del self.doc["sources"][1]["files"][0]["sha256"]
        run, entries, logs = self.run_once()
        self.assertRegex(run.problems[0], "isn't pinned .* run with --pin")
        run, entries, logs = self.run_once(pin=True)
        self.assertEqual(run.problems, [])
        self.assertEqual(self.doc["sources"][1]["files"][0]["sha256"], hashlib.sha256(self.f).hexdigest())
        self.assertIn("f: f.wav", run.pinned)
        self.assertIn("fs: 111__someone__fan.wav", run.pinned)


if __name__ == "__main__":
    unittest.main()
