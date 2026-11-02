"""Offline tests for tools/fetch_models.py, through a fake opener serving bytes by URL: the manifest's checks (spec major, id
rule, folder name, paths, roles, https sources, one model), hash, size and git-blob mismatches, path traversal, idempotent
reruns, --pin, and the repo's own manifests.
Run from any directory:
    python -I tools/test_fetch_models.py
"""
import contextlib
import copy
import hashlib
import importlib.util
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.dont_write_bytecode = True  # before the import: no __pycache__ in tools/
HERE = Path(__file__).resolve().parent


def _load(name, file):
    spec = importlib.util.spec_from_file_location(name, HERE / file)
    mod = importlib.util.module_from_spec(spec)
    sys.modules[name] = mod
    spec.loader.exec_module(mod)
    return mod


fm = _load("nf_fetch_models", "fetch_models.py")

BASE = "https://example.test/pack/"
MODEL = b"\x08\x06not really onnx" * 50
LICENSE = b"MIT License\n"
WAV = b"RIFF....WAVEfmt " + bytes(200)


class FakeResponse(io.BytesIO):
    def __init__(self, status, headers, body):
        super().__init__(body)
        self.status, self.headers = status, headers


class FakeOpener:
    """Serves bytes by URL and logs each request; an unknown URL is a 404."""

    def __init__(self, files):
        self.files, self.requests = files, []

    def open(self, req, timeout=None):
        self.requests.append(req.full_url)
        if req.full_url not in self.files:
            import urllib.error
            raise urllib.error.HTTPError(req.full_url, 404, "Not Found", {}, io.BytesIO())
        data = self.files[req.full_url]
        return FakeResponse(200, {"Content-Length": str(len(data))}, data)


def entry(path, role, data, pinned=True):
    e = {"path": path, "role": role, "source": {"url": BASE + path.replace("/", "_")}}
    if pinned:
        e["size"], e["sha256"] = len(data), hashlib.sha256(data).hexdigest()
        e["source"]["git_blob"] = fm.git_blob_id(data)
    return e


def manifest(pinned=True):
    return {
        "spec": "noise-filter.model-pack/0",
        "id": "demo",
        "version": "0.1.0",
        "files": [entry("model.onnx", "model", MODEL, pinned), entry("LICENSE", "license", LICENSE, pinned),
                  entry("conformance/noisy.wav", "conformance", WAV, pinned)],
        "graph": {"kind": "spectral", "model": "model.onnx"},
    }


def served(doc):
    by = {"model.onnx": MODEL, "LICENSE": LICENSE, "conformance/noisy.wav": WAV}
    return {f["source"]["url"]: by[f["path"]] for f in doc["files"]}


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.packs, self.cache = root / "packs", root / "cache"
        self.out = self.enterContext(contextlib.redirect_stdout(io.StringIO()))
        self.log = []

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, doc, folder="demo"):
        path = self.packs / folder / "manifest.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(fm.dump_json(doc) + "\n", encoding="utf-8", newline="\n")
        return path

    def fetch(self, path, files, pin=False):
        opener = FakeOpener(files)
        dest = fm.Pack(path, self.cache, fm.Http(opener, sleep=lambda s: None), pin=pin, log=self.log.append).fetch()
        return dest, opener


class Manifest(Base):
    def refused(self, change, why, folder="demo"):
        doc = manifest()
        change(doc)
        with self.assertRaisesRegex(fm.FetchError, why):
            fm.Pack(self.write(doc, folder), self.cache, None).load()

    def test_a_good_manifest_passes(self):
        doc = fm.Pack(self.write(manifest()), self.cache, None).load()
        self.assertEqual(doc["id"], "demo")

    def test_the_spec_major_must_be_0(self):
        self.refused(lambda d: d.update(spec="noise-filter.model-pack/1"), "only major version 0")
        self.refused(lambda d: d.update(spec="something-else/0"), "isn't noise-filter.model-pack")

    def test_the_id_rule_and_the_folder_name(self):
        for bad in ("Demo", "-demo", "de mo", "de/mo", "a" * 65, "", "demo.x"):
            self.refused(lambda d, b=bad: d.update(id=b), "must match")
        self.refused(lambda d: None, "the folder is 'other'", folder="other")

    def test_paths_must_stay_inside_the_pack_and_be_unique(self):
        for bad in ("../x", "/abs", "C:/x", "a\\b", "a/../b", "con.wav", "x.", "a//b", "manifest.json", "Manifest.JSON"):
            self.refused(lambda d, b=bad: d["files"].append(entry(b, "conformance", WAV)), "unsafe name|listed twice")
        self.refused(lambda d: d["files"].append(entry("LICENSE".lower(), "license", LICENSE)), "listed twice")

    def test_sources_must_be_https_and_pins_well_formed(self):
        self.refused(lambda d: d["files"][1]["source"].update(url="http://example.test/LICENSE"), "https")
        self.refused(lambda d: d["files"][1]["source"].update(url="file:///C:/Windows/win.ini"), "https")
        self.refused(lambda d: d["files"][1].update(sha256="ABC"), "64 lower-case hex")
        self.refused(lambda d: d["files"][1]["source"].update(git_blob="xyz"), "40 lower-case hex")
        self.refused(lambda d: d["files"][1].update(size=0), "whole number of bytes")
        self.refused(lambda d: d["files"][1].update(role="code"), "role")

    def test_exactly_one_model_the_graph_names(self):
        self.refused(lambda d: d["graph"].update(model="other.onnx"), "exactly one model")
        self.refused(lambda d: d["files"].append(entry("second.onnx", "model", MODEL)), "exactly one model")


class Fetch(Base):
    def test_a_pack_is_fetched_checked_and_its_manifest_copied_last(self):
        doc = manifest()
        path = self.write(doc)
        dest, opener = self.fetch(path, served(doc))
        self.assertEqual(dest, (self.cache / "demo").resolve())
        self.assertEqual((dest / "model.onnx").read_bytes(), MODEL)
        self.assertEqual((dest / "conformance" / "noisy.wav").read_bytes(), WAV)
        self.assertEqual((dest / "manifest.json").read_bytes(), path.read_bytes(), "byte for byte")
        self.assertEqual(len(opener.requests), 3)
        self.assertEqual(sorted(p.name for p in dest.rglob("*.part")), [])

    def test_a_rerun_fetches_nothing_and_a_damaged_file_comes_back(self):
        doc = manifest()
        path = self.write(doc)
        dest, _ = self.fetch(path, served(doc))
        _, opener = self.fetch(path, served(doc))
        self.assertEqual(opener.requests, [], "idempotent")
        (dest / "LICENSE").write_bytes(b"tampered")
        _, opener = self.fetch(path, served(doc))
        self.assertEqual(opener.requests, [doc["files"][1]["source"]["url"]])
        self.assertEqual((dest / "LICENSE").read_bytes(), LICENSE)

    def test_a_hash_mismatch_is_refused_and_leaves_no_manifest(self):
        doc = manifest()
        path = self.write(doc)
        files = served(doc)
        files[doc["files"][0]["source"]["url"]] = MODEL[:-1] + b"!"  # same size, other bytes
        with self.assertRaisesRegex(fm.FetchError, "model.onnx: SHA-256"):
            self.fetch(path, files)
        dest = self.cache / "demo"
        self.assertFalse((dest / "manifest.json").exists(), "an incomplete pack has no manifest")
        self.assertFalse((dest / "model.onnx").exists())
        self.assertEqual(list(dest.rglob("*.part")), [])

    def test_a_git_blob_mismatch_is_refused(self):
        doc = manifest()
        doc["files"][1]["source"]["git_blob"] = "0" * 40
        with self.assertRaisesRegex(fm.FetchError, "LICENSE: git blob"):
            self.fetch(self.write(doc), served(doc))

    def test_the_size_caps_the_download(self):
        doc = manifest()
        path = self.write(doc)
        files = served(doc)
        files[doc["files"][2]["source"]["url"]] = WAV + b"more"
        with self.assertRaisesRegex(fm.FetchError, "more than the"):
            self.fetch(path, files)
        self.assertEqual(list((self.cache / "demo").rglob("*.part")), [])

    def test_an_unpinned_file_needs_pin_and_is_refused_before_any_download(self):
        for drop in ["size", "sha256", "git_blob"]:
            doc = manifest()
            if drop == "git_blob":
                del doc["files"][1]["source"]["git_blob"]
            else:
                del doc["files"][1][drop]
            path = self.write(doc)
            opener = FakeOpener(served(doc))
            with self.assertRaisesRegex(fm.FetchError, f"LICENSE isn't fully pinned \\(no {drop}\\).*--pin"):
                fm.Pack(path, self.cache, fm.Http(opener, sleep=lambda s: None), log=self.log.append).fetch()
            self.assertEqual(opener.requests, [], f"no {drop}: nothing downloaded")

    def test_a_download_that_fails_leaves_no_part_file(self):
        doc = manifest()
        path = self.write(doc)

        class Failing:
            """Leaves its .part behind and raises, as a careless HTTP client might."""

            def download(self, url, dest, size=None):
                dest.parent.mkdir(parents=True, exist_ok=True)
                dest.with_name(dest.name + ".part").write_bytes(b"half")
                raise fm.FetchError(f"{url}: retries exhausted")

        with self.assertRaisesRegex(fm.FetchError, "retries exhausted"):
            fm.Pack(path, self.cache, Failing(), log=self.log.append).fetch()
        self.assertEqual(list((self.cache / "demo").rglob("*.part")), [])

    def test_pin_records_size_sha256_and_git_blob_in_the_repo_manifest(self):
        doc = manifest(pinned=False)
        path = self.write(doc)
        dest, _ = self.fetch(path, served(doc), pin=True)
        pinned = json.loads(path.read_text(encoding="utf-8"))
        self.assertEqual(pinned, manifest(pinned=True))
        self.assertEqual((dest / "manifest.json").read_bytes(), path.read_bytes(), "the pinned manifest is the one copied")
        _, opener = self.fetch(path, served(doc))
        self.assertEqual(opener.requests, [], "pinned and cached: nothing to fetch")

    def test_pin_never_overrides_a_pin_that_disagrees(self):
        doc = manifest()
        doc["files"][0]["sha256"] = "1" * 64
        with self.assertRaisesRegex(fm.FetchError, "model.onnx: SHA-256"):
            self.fetch(self.write(doc), served(doc), pin=True)

    def test_an_lfs_file_is_checked_against_its_pointer_s_blob(self):
        """A Git LFS file's blob in the tree is its pointer's: the pin is the pointer's blob, made from the download's
        SHA-256 and size; the content's own blob id fails, and --pin records the pointer's."""
        sha = hashlib.sha256(MODEL).hexdigest()
        pointer = fm.lfs_pointer(sha, len(MODEL))
        self.assertEqual(pointer, b"version https://git-lfs.github.com/spec/v1\noid sha256:" + sha.encode() + b"\nsize %d\n" % len(MODEL))
        doc = manifest()
        doc["files"][0]["source"].update(lfs=True, git_blob=fm.git_blob_id(pointer))
        dest, _ = self.fetch(self.write(doc), served(doc))
        self.assertEqual((dest / "model.onnx").read_bytes(), MODEL)
        doc["files"][0]["source"]["git_blob"] = fm.git_blob_id(MODEL)
        (dest / "model.onnx").unlink()
        with self.assertRaisesRegex(fm.FetchError, "model.onnx: git blob"):
            self.fetch(self.write(doc, "demo"), served(doc))
        unpinned = manifest(pinned=False)
        unpinned["files"][0]["source"]["lfs"] = True
        path = self.write(unpinned)
        (dest / "model.onnx").unlink(missing_ok=True)
        self.fetch(path, served(unpinned), pin=True)
        pinned = json.loads(path.read_text(encoding="utf-8"))
        self.assertEqual(pinned["files"][0]["source"]["git_blob"], fm.git_blob_id(pointer))
        # SIGMOS's pointer at SIG-Challenge bf45251 (26,264,942 B): its blob is the tree's, 508d54d...
        sigmos = fm.lfs_pointer("f939dcc1945055a435565b4369e27dafd0f87df3cea4e2ff6eb81225e52cc53b", 26_264_942)
        self.assertEqual((len(sigmos), fm.git_blob_id(sigmos)), (133, "508d54d74690b3dcb2ae58797a9111ae412297bf"))

    def test_the_lfs_flag_is_a_bool(self):
        doc = manifest()
        doc["files"][0]["source"]["lfs"] = "yes"
        with self.assertRaisesRegex(fm.FetchError, "source.lfs"):
            fm.Pack(self.write(doc), self.cache, None).load()

    def test_main_picks_packs_by_id_and_refuses_bad_ids(self):
        doc = manifest()
        self.write(doc)
        opener = FakeOpener(served(doc))
        http = fm.Http(opener, sleep=lambda s: None)
        self.assertEqual(fm.main(["--dir", str(self.cache), "demo"], http=http, packs=self.packs), 0)
        self.assertEqual(fm.main(["--dir", str(self.cache), "../demo"], http=http, packs=self.packs), 1)
        self.assertEqual(fm.main(["--dir", str(self.cache), "missing"], http=http, packs=self.packs), 1)
        self.assertIn("isn't a pack id", self.out.getvalue())


class RepoPacks(unittest.TestCase):
    def test_every_manifest_in_the_repo_validates_and_is_fully_pinned(self):
        paths = fm.manifests([], fm.PACKS)
        self.assertTrue(paths)
        for path in paths:
            doc = fm.Pack(path, Path(tempfile.gettempdir()), None).load()
            for f in doc["files"]:
                self.assertTrue(f.get("size") and f.get("sha256") and f["source"].get("git_blob"), f"{path}: {f['path']}")
            self.assertEqual(path.read_text(encoding="utf-8"), fm.dump_json(copy.deepcopy(doc)) + "\n", f"{path}: dump_json's layout")


if __name__ == "__main__":
    unittest.main()
