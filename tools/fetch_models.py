"""Fetch model packs into a local cache (TODO 3, 5). No weights go in the repo: packs/<id>/manifest.json is the record of
what a pack holds and where each file comes from, and this script is the recipe.

Run from any directory:
    python -I tools/fetch_models.py [--dir DIR] [--pin] [ID ...]
With no id it fetches every packs/*/manifest.json. Standard library only.

The cache (default %LOCALAPPDATA%\\noise-filter\\models, or NF_MODELS) holds one folder per pack, named by its id:
    <id>/manifest.json   the repo's manifest, byte for byte, copied last: a folder with a manifest is a complete pack
    <id>/<path>          each file the manifest lists (the ONNX model, its licence, conformance audio under conformance/)
A second run fetches only what's missing or doesn't match its size and SHA-256.

Downloads are untrusted data: nothing downloaded is executed, imported or parsed here. Every file is checked against its
pinned size (a cap on the download too), SHA-256 and git blob id; names are checked before they become paths, and sources
must be https. --pin records the size, SHA-256 and git blob id of files that have none yet, in the repo's manifest.

The host (nf-ort) checks the sizes and hashes again when it loads a pack, and hashes the very bytes it hands to ONNX Runtime.
"""
from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import re
import sys
import urllib.parse
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent
PACKS = REPO / "packs"
SPEC_PREFIX = "noise-filter.model-pack/"
ID_RULE = re.compile(r"[a-z0-9][a-z0-9-]{0,63}")
ROLES = ("model", "license", "conformance")
MAX_MANIFEST = 1 << 20
MAX_FILE = 256 << 20  # any one file: DeepFilterNet3_ll is about 39 MB


def _load_test_audio_tools():
    """fetch_test_audio.py, loaded by path: `python -I` leaves this folder out of sys.path. Its top-level imports are the
    standard library's only (it imports numpy and soundfile inside the functions that convert audio)."""
    sys.dont_write_bytecode = True  # no __pycache__ in tools/
    spec = importlib.util.spec_from_file_location("nf_fetch_test_audio", HERE / "fetch_test_audio.py")
    mod = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = mod  # dataclasses look their module up here
    spec.loader.exec_module(mod)
    return mod


fta = _load_test_audio_tools()
FetchError, Http, safe_parts, inside, sha256_file, write_atomic, dump_json = (
    fta.FetchError, fta.Http, fta.safe_parts, fta.inside, fta.sha256_file, fta.write_atomic, fta.dump_json)


def git_blob_id(data):
    """git's object id for a file's bytes: SHA-1 over "blob <size>\\0" and the bytes."""
    return hashlib.sha1(b"blob %d\0" % len(data) + data).hexdigest()


def _hex(value, n):
    return isinstance(value, str) and re.fullmatch(f"[0-9a-f]{{{n}}}", value) is not None


def validate(doc, folder_name=None, pin=False):
    """The manifest's checks before anything is fetched: spec major 0, the id rule (and the folder named by it), files with
    safe, unique relative paths and known roles, pins well formed where given, https sources, and exactly one model, the one
    graph.model names. Without `pin` (--pin) every file must be fully pinned (its size, SHA-256 and, with a source, git blob),
    so nothing unpinned is downloaded only to be refused. Raises FetchError."""
    if not isinstance(doc, dict):
        raise FetchError("the manifest isn't a JSON object")
    spec = doc.get("spec")
    if not isinstance(spec, str) or not spec.startswith(SPEC_PREFIX):
        raise FetchError(f"spec {spec!r} isn't {SPEC_PREFIX}<major>")
    if spec[len(SPEC_PREFIX):] != "0":
        raise FetchError(f"spec {spec!r}: only major version 0 is known")
    pid = doc.get("id")
    if not isinstance(pid, str) or not ID_RULE.fullmatch(pid):
        raise FetchError(f"id {pid!r} must match {ID_RULE.pattern}")
    if folder_name is not None and folder_name != pid:
        raise FetchError(f"the folder is {folder_name!r}, but the id is {pid!r}: they must be the same")
    files = doc.get("files")
    if not isinstance(files, list) or not files:
        raise FetchError("no files")
    seen = set()
    for f in files:
        if not isinstance(f, dict):
            raise FetchError("a files[] entry isn't an object")
        path = f.get("path")
        parts = safe_parts(path)
        key = "/".join(parts).lower()  # Windows folds case
        if key in seen or key == "manifest.json":
            raise FetchError(f"{path!r} is listed twice, or is the manifest itself")
        seen.add(key)
        if f.get("role") not in ROLES:
            raise FetchError(f"{path}: role {f.get('role')!r} isn't one of {', '.join(ROLES)}")
        size = f.get("size")
        if size is not None and (not isinstance(size, int) or isinstance(size, bool) or not 0 < size <= MAX_FILE):
            raise FetchError(f"{path}: size {size!r} must be a whole number of bytes from 1 to {MAX_FILE:,}")
        if f.get("sha256") is not None and not _hex(f["sha256"], 64):
            raise FetchError(f"{path}: sha256 {f['sha256']!r} isn't 64 lower-case hex digits")
        src = f.get("source")
        if src is not None:
            if not isinstance(src, dict) or urllib.parse.urlsplit(src.get("url") or "").scheme != "https":
                raise FetchError(f"{path}: the source must be an https URL")
            for k, n in (("git_blob", 40), ("git_commit", 40)):
                if src.get(k) is not None and not _hex(src[k], n):
                    raise FetchError(f"{path}: source.{k} {src[k]!r} isn't {n} lower-case hex digits")
        missing = [k for k, v in (("size", size), ("sha256", f.get("sha256"))) if v is None]
        if isinstance(src, dict) and src.get("git_blob") is None:
            missing.append("git_blob")
        if missing and not pin:
            raise FetchError(f"{path} isn't fully pinned (no {', '.join(missing)}): run with --pin to fetch it and record them")
    models = [f["path"] for f in files if f["role"] == "model"]
    graph = doc.get("graph")
    named = graph.get("model") if isinstance(graph, dict) else None
    if models != [named]:
        raise FetchError(f"v0 takes exactly one model file, the one graph.model names ({named!r}); files[] has {models}")


class Pack:
    """One pack's fetch."""

    def __init__(self, manifest_path, cache, http, pin=False, log=print):
        self.manifest_path, self.http, self.pin, self.log = Path(manifest_path), http, pin, log
        self.cache = Path(cache)
        self.pinned = []

    def load(self):
        raw = self.manifest_path.read_bytes()
        if len(raw) > MAX_MANIFEST:
            raise FetchError(f"{self.manifest_path} is over {MAX_MANIFEST:,} bytes")
        try:
            doc = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, ValueError) as e:
            raise FetchError(f"{self.manifest_path}: {e}") from None
        validate(doc, self.manifest_path.parent.name, self.pin)
        return doc

    def have(self, f, path):
        """The cached copy matches its pinned size and SHA-256."""
        return (bool(f.get("sha256")) and f.get("size") is not None and path.is_file() and path.stat().st_size == f["size"]
                and sha256_file(path) == f["sha256"])

    def check(self, f, n, sha, blob):
        """The download against its pins, or the pins recorded under --pin. Errors name the file."""
        path, src = f["path"], f.get("source") or {}
        missing = [k for k, v in (("size", f.get("size")), ("sha256", f.get("sha256")), ("git_blob", src.get("git_blob"))) if v is None]
        if f.get("size") is not None and n != f["size"]:
            raise FetchError(f"{path}: {n:,} B, pinned {f['size']:,}")
        if f.get("sha256") is not None and sha != f["sha256"]:
            raise FetchError(f"{path}: SHA-256 {sha}, pinned {f['sha256']}")
        if src.get("git_blob") is not None and blob != src["git_blob"]:
            raise FetchError(f"{path}: git blob {blob}, pinned {src['git_blob']}")
        if missing and not self.pin:
            raise FetchError(f"{path} isn't fully pinned (no {', '.join(missing)}; it is {n:,} B, SHA-256 {sha}, git blob {blob}):"
                             " run with --pin to record it")
        if missing:
            f["size"], f["sha256"] = n, sha
            if src:
                src["git_blob"] = blob
            self.pinned.append(f"{path}: {', '.join(missing)}")

    def fetch(self):
        """Fetches what's missing, then copies the manifest. Returns the pack's folder."""
        doc = self.load()
        dest = inside(self.cache, doc["id"])
        dest.mkdir(parents=True, exist_ok=True)
        todo = [(f, inside(dest, *safe_parts(f["path"]))) for f in doc["files"]]
        todo = [(f, p) for f, p in todo if not self.have(f, p)]
        cached = dest / "manifest.json"
        if not todo:
            self.log(f"{doc['id']}: all {len(doc['files'])} files already in {dest}")
        else:
            cached.unlink(missing_ok=True)  # incomplete from here until it is copied back
            for f, path in todo:
                src = f.get("source") or {}
                if not src.get("url"):
                    raise FetchError(f"{f['path']} isn't in the cache and has no source to fetch it from")
                part = path.with_name(path.name + ".part")
                try:
                    part, n, sha, _ = self.http.download(src["url"], path, f.get("size") or MAX_FILE)
                    blob = git_blob_id(part.read_bytes())  # within the size cap: the download refused more
                    self.check(f, n, sha, blob)
                    os.replace(part, path)
                finally:
                    part.unlink(missing_ok=True)  # a failed or refused download leaves nothing behind
                self.log(f"{doc['id']}: {f['path']}: {n:,} B (size, SHA-256 and git blob ok)")
        if self.pinned:
            self.manifest_path.write_text(dump_json(doc) + "\n", encoding="utf-8", newline="\n")
            self.log(f"{doc['id']}: pinned {', '.join(self.pinned)} in {self.manifest_path}")
        data = self.manifest_path.read_bytes()
        if not (cached.is_file() and cached.read_bytes() == data):
            write_atomic(cached, data)
        return dest


def default_cache():
    if os.environ.get("NF_MODELS"):
        return Path(os.environ["NF_MODELS"])
    base = os.environ.get("LOCALAPPDATA")
    if not base:
        sys.exit("LOCALAPPDATA isn't set: give the cache folder with --dir")
    return Path(base) / "noise-filter" / "models"


def manifests(ids, packs=PACKS):
    """packs/<id>/manifest.json for each id (each checked against the id rule first), or every pack's."""
    if not ids:
        found = sorted(packs.glob("*/manifest.json"))
        if not found:
            raise FetchError(f"no packs in {packs}")
        return found
    out = []
    for pid in ids:
        if not ID_RULE.fullmatch(pid):
            raise FetchError(f"{pid!r} isn't a pack id ({ID_RULE.pattern})")
        path = packs / pid / "manifest.json"
        if not path.is_file():
            raise FetchError(f"no pack {pid!r}: {path} doesn't exist")
        out.append(path)
    return out


def main(argv=None, http=None, packs=PACKS):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("ids", nargs="*", metavar="ID", help="the packs to fetch (default: every packs/*/manifest.json)")
    ap.add_argument("--dir", type=Path, help="the cache folder (default %%NF_MODELS%%, else %%LOCALAPPDATA%%\\noise-filter\\models)")
    ap.add_argument("--pin", action="store_true", help="record the size, SHA-256 and git blob id of files that have none, in the manifest")
    args = ap.parse_args(argv)
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(errors="replace")
    cache = args.dir or default_cache()
    http = http or Http()
    failed = 0
    try:
        paths = manifests(args.ids, packs)
    except FetchError as e:
        print(f"FAILED: {e}")
        return 1
    print(f"cache: {cache}", flush=True)
    for path in paths:
        try:
            dest = Pack(path, cache, http, pin=args.pin).fetch()
            print(f"{path.parent.name}: ready in {dest}")
        except (OSError, FetchError) as e:
            failed += 1
            print(f"FAILED: {path.parent.name}: {e}")
    print(f"{http.requests} HTTP requests, {http.received:,} B received")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
