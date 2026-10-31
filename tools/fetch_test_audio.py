"""Fetch the test-audio starter set into a local cache (TODO 14; the catalogue is
doc/todo/14-test-audio-catalogue-2026-11-01.md). No audio goes in the repo: this script and
tools/test_audio_sources.json are the record of what we use.

Run from any directory:
    python -I tools/fetch_test_audio.py [--dir DIR] [--freesound-dir DIR] [--pin]

It needs numpy, soundfile and soxr. Install them into the interpreter itself, not with --user,
because python -I leaves out the user site-packages:
    python -m pip install numpy soundfile soxr

The cache (default %LOCALAPPDATA%\\noise-filter\\test-audio) holds:
    raw/<source>/   the downloads as published: whole files, and zip members fetched by HTTP Range
    wav48/          48 kHz mono 16-bit PCM WAV, built from raw/ by each output's recipe
    manifest.json   per output: source URLs, licence, attribution, changes, SHA-256s, rate, duration
    ATTRIBUTION.md  generated from the manifest
A second run fetches and rebuilds only what's missing or doesn't match its SHA-256.

Freesound needs a login, so its five CC0 clips aren't downloaded. Save each original (Download on its page,
https://freesound.org/s/<id>/, not the preview) into your Downloads folder (or --freesound-dir), keeping the name
Freesound gives it, <id>__<user>__<title>.<ext>. A missing clip is reported with its link and skipped.

Downloads are untrusted data: nothing downloaded is executed, every file is checked against its
pinned size and SHA-256 (zip members against their CRC-32 too), sizes are capped, and names are
checked before they become paths. --pin records the size and SHA-256 of entries that have none yet.
"""
from __future__ import annotations

import argparse
import hashlib
import http.client
import json
import os
import re
import ssl
import struct
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import zlib
from dataclasses import dataclass
from pathlib import Path

HERE = Path(__file__).resolve().parent
SOURCES = HERE / "test_audio_sources.json"
RATE = 48_000
# Part of every output's fingerprint: change it when the conversion changes, and every output is rebuilt
CONVERTER = "1: soundfile float64, mean of the channels, soxr VHQ to 48 kHz, raised-cosine fades, joins, trims, rounded to 16-bit"
USER_AGENT = "noise-filter-test-audio/1 (tools/fetch_test_audio.py)"
INSTALL = "python -m pip install numpy soundfile soxr"
MAX_FILE = 512 << 20  # any one download or zip member
MAX_CENTRAL_DIR = 64 << 20  # VCTK 0.92's is 12.9 MB (133,135 members)
KINDS = ("zip", "file", "freesound")


class FetchError(Exception):
    pass


# --- Names and paths -------------------------------------------------------------------------------

_RESERVED = {"CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", *(f"COM{i}" for i in range(10)), *(f"LPT{i}" for i in range(10))}
_BAD = set('<>:"|?*\\') | {chr(c) for c in range(32)} | {"\x7f"}


def safe_parts(name):
    """The '/'-separated parts of a zip member or file name. Refuses anything that could land outside the
    folder it's written into, or that Windows reads differently: absolute paths, drives, '.', '..', empty
    parts, backslashes, colons (alternate streams), control characters, device names, trailing dots and spaces."""
    if not isinstance(name, str) or not name or len(name) > 400:
        raise FetchError(f"unsafe name {name!r}")
    parts = name.split("/")
    for p in parts:
        if (p in ("", ".", "..") or any(c in _BAD for c in p) or p != p.rstrip(" .")
                or p.split(".")[0].rstrip(" ").upper() in _RESERVED):
            raise FetchError(f"unsafe name {name!r}")
    return parts


def inside(root, *parts):
    """root/parts, once it's checked to resolve inside root (a junction or symlink could point elsewhere)."""
    base = Path(root).resolve()
    path = base.joinpath(*parts).resolve()
    if base not in path.parents:
        raise FetchError(f"{path} is outside {base}")
    return path


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(1 << 20):
            h.update(chunk)
    return h.hexdigest()


def write_atomic(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    part = path.with_name(path.name + ".part")
    part.write_bytes(data)
    os.replace(part, path)


# --- HTTP ------------------------------------------------------------------------------------------

class HttpsOnlyRedirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        if urllib.parse.urlsplit(newurl).scheme != "https":
            raise FetchError(f"{req.full_url} redirects to {newurl}, which isn't https")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def tls_context():
    """Certificate and host-name checks as usual, without the strict RFC 5280 checks Python 3.13 turned on: the
    CA chain of archive.org's file servers fails them ("Basic Constraints of CA cert not marked critical")."""
    ctx = ssl.create_default_context()
    ctx.verify_flags &= ~ssl.VERIFY_X509_STRICT
    return ctx


def _retry_after(headers, default):
    try:
        return min(max(float(headers.get("Retry-After")), 1.0), 300.0)
    except (AttributeError, TypeError, ValueError):
        return default


class Http:
    """GETs, whole or by Range, retried on dropped connections, short bodies and HTTP 429 and 5xx (honouring Retry-After).
    Six tries wait 5, 10, 20, 40 and 80 s between them: archive.org has answered 500 three times running."""

    def __init__(self, opener=None, sleep=time.sleep, tries=6, timeout=60):
        self.opener = opener or urllib.request.build_opener(HttpsOnlyRedirects, urllib.request.HTTPSHandler(context=tls_context()))
        self.sleep, self.tries, self.timeout = sleep, tries, timeout
        self.requests = 0
        self.received = 0

    def _get(self, url, headers, body):
        """body(response) for a GET of url, under retries."""
        headers = {"User-Agent": USER_AGENT, "Accept-Encoding": "identity", **headers}
        for attempt in range(self.tries):
            last = attempt + 1 == self.tries
            try:
                self.requests += 1
                with self.opener.open(urllib.request.Request(url, headers=headers), timeout=self.timeout) as r:
                    return body(r)
            except urllib.error.HTTPError as e:
                if last or e.code not in (429, 500, 502, 503, 504):
                    raise FetchError(f"{url}: HTTP {e.code} {e.reason}") from None
                wait, why = _retry_after(e.headers, 5.0 * 2**attempt), f"HTTP {e.code}"
            except (OSError, http.client.HTTPException) as e:  # URLError, timeouts, resets, short reads
                if last or isinstance(getattr(e, "reason", e), ssl.SSLCertVerificationError):
                    raise FetchError(f"{url}: {e}") from None
                wait, why = 5.0 * 2**attempt, str(e)
            print(f"  {why}; retrying in {wait:.0f} s: {url}", flush=True)
            self.sleep(wait)
        raise AssertionError("unreachable")

    def read_range(self, url, start, length):
        """Bytes [start, start + length) of url, and the file's total size from Content-Range."""
        if length <= 0 or length > MAX_FILE:
            raise FetchError(f"{url}: refusing a {length}-byte read")
        end = start + length - 1

        def body(r):
            if r.status != 206:
                raise FetchError(f"{url} ignored the Range request (HTTP {r.status})")
            got = r.headers.get("Content-Range") or ""
            m = re.fullmatch(r"bytes (\d+)-(\d+)/(\d+)", got.strip())
            if not m or (int(m[1]), int(m[2])) != (start, end):
                raise FetchError(f"{url}: asked for bytes {start}-{end}, got Content-Range {got!r}")
            data = r.read(length + 1)
            if len(data) < length:  # the connection dropped: http.client returns what it got without complaint
                raise http.client.HTTPException(f"the connection dropped after {len(data):,} of {length:,} bytes")
            if len(data) > length:
                raise FetchError(f"{url}: more than {length} bytes in a {length}-byte range")
            self.received += length
            return data, int(m[3])

        return self._get(url, {"Range": f"bytes={start}-{end}"}, body)

    def download(self, url, dest, size=None):
        """Streams url into dest.part, refusing more than `size` bytes (MAX_FILE if None).
        Returns (the .part path, its size, SHA-256, SHA-1); the caller checks them and renames it."""
        cap = MAX_FILE if size is None else size
        part = dest.with_name(dest.name + ".part")
        dest.parent.mkdir(parents=True, exist_ok=True)

        def body(r):
            if r.status != 200:
                raise FetchError(f"{url}: HTTP {r.status}")
            declared = r.headers.get("Content-Length")
            if declared is not None and int(declared) > cap:
                raise FetchError(f"{url} is {int(declared):,} bytes, more than the {cap:,} expected")
            h256, h1, n = hashlib.sha256(), hashlib.sha1(), 0
            with open(part, "wb") as f:
                while chunk := r.read(1 << 20):
                    n += len(chunk)
                    if n > cap:
                        raise FetchError(f"{url} sent more than the {cap:,} bytes expected")
                    h256.update(chunk)
                    h1.update(chunk)
                    f.write(chunk)
            if declared is not None and n < int(declared):  # the same: retried, not passed on as a pin mismatch
                raise http.client.HTTPException(f"the connection dropped after {n:,} of {int(declared):,} bytes")
            self.received += n
            return n, h256.hexdigest(), h1.hexdigest()

        try:
            n, s256, s1 = self._get(url, {}, body)
        except BaseException:
            part.unlink(missing_ok=True)
            raise
        return part, n, s256, s1


class RemoteFile:
    """A file read by HTTP Range requests. Every response must report the pinned total size, so a file
    that changed upstream is caught at the first read."""

    def __init__(self, http, url, size=None):
        self.http, self.url = http, url
        self.size = size if size is not None else http.read_range(url, 0, 1)[1]

    def read(self, start, length):
        if start < 0 or start + length > self.size:
            raise FetchError(f"{self.url}: bytes {start}-{start + length - 1} are past its end ({self.size:,} B)")
        data, total = self.http.read_range(self.url, start, length)
        if total != self.size:
            raise FetchError(f"{self.url} is {total:,} bytes now, not the {self.size:,} pinned: it changed upstream")
        return data


# --- Zip members by Range --------------------------------------------------------------------------

EOCD, ZIP64_LOCATOR, ZIP64_EOCD, CENTRAL, LOCAL = b"PK\x05\x06", b"PK\x06\x07", b"PK\x06\x06", b"PK\x01\x02", b"PK\x03\x04"
CENTRAL_FMT = "<4s6H3I5H2I"  # 46 bytes: ... flags, method, time, date, crc, sizes, name/extra/comment lengths, ..., offset


@dataclass
class Member:
    name: str
    raw_name: bytes
    flags: int
    method: int
    crc: int
    csize: int
    usize: int
    offset: int


def zip64_extra(extra, usize, csize, offset):
    """Sizes and offset with any 0xFFFFFFFF replaced from the ZIP64 extra field (tag 1), in its fixed order."""
    p = 0
    while p + 4 <= len(extra):
        tag, n = struct.unpack_from("<HH", extra, p)
        if tag == 1:
            values = iter(struct.unpack_from(f"<{n // 8}Q", extra, p + 4))
            try:
                usize = next(values) if usize == 0xFFFFFFFF else usize
                csize = next(values) if csize == 0xFFFFFFFF else csize
                offset = next(values) if offset == 0xFFFFFFFF else offset
            except StopIteration:
                raise FetchError("a ZIP64 extra field is too short") from None
            break
        p += 4 + n
    return usize, csize, offset


def central_directory(f):
    """The members of the zip that f (.size, .read(start, length)) holds, by name. Reads only the
    end-of-central-directory record (and its ZIP64 form) and the central directory."""
    try:
        tail_at = max(0, f.size - (22 + 0xFFFF + 20))
        tail = f.read(tail_at, f.size - tail_at)
        i = len(tail)
        while True:
            i = tail.rfind(EOCD, 0, i)
            if i < 0:
                raise FetchError("not a zip: no end-of-central-directory record")
            if len(tail) - i >= 22 and i + 22 + struct.unpack_from("<H", tail, i + 20)[0] == len(tail):
                break
        count, cd_size, cd_at = struct.unpack_from("<HII", tail, i + 10)
        if 0xFFFF == count or 0xFFFFFFFF in (cd_size, cd_at):
            if i < 20 or tail[i - 20:i - 16] != ZIP64_LOCATOR:
                raise FetchError("ZIP64 locator missing")
            record = f.read(struct.unpack_from("<Q", tail, i - 12)[0], 56)
            if record[:4] != ZIP64_EOCD:
                raise FetchError("ZIP64 end-of-central-directory record missing")
            count, cd_size, cd_at = struct.unpack_from("<3Q", record, 32)
        if cd_size > MAX_CENTRAL_DIR or cd_at + cd_size > f.size:
            raise FetchError(f"implausible central directory: {cd_size:,} B at {cd_at:,}")
        cd = f.read(cd_at, cd_size) if cd_size else b""
        members, p = {}, 0
        while p < len(cd):
            fields = struct.unpack_from(CENTRAL_FMT, cd, p)
            if fields[0] != CENTRAL:
                raise FetchError(f"central directory entry {len(members)} is corrupt")
            flags, method, crc, csize, usize, n, e, c, offset = *fields[3:5], *fields[7:13], fields[16]
            if p + 46 + n + e + c > len(cd):
                raise FetchError(f"central directory entry {len(members)} is truncated")
            raw = cd[p + 46:p + 46 + n]
            usize, csize, offset = zip64_extra(cd[p + 46 + n:p + 46 + n + e], usize, csize, offset)
            name = raw.decode("utf-8" if flags & 0x800 else "cp437", errors="replace")
            if name in members:
                raise FetchError(f"the zip lists {name!r} twice")
            members[name] = Member(name, raw, flags, method, crc, csize, usize, offset)
            p += 46 + n + e + c
    except struct.error:
        raise FetchError("the zip's directory is truncated") from None
    if len(members) != count:
        raise FetchError(f"the zip says it has {count} members but lists {len(members)}")
    return members


def read_member(f, m, cap=MAX_FILE):
    """Member m's bytes: usually one Range read for its local header and data, then inflated and CRC-checked."""
    if m.flags & 1:
        raise FetchError(f"{m.name} is encrypted")
    if m.method not in (0, 8):
        raise FetchError(f"{m.name}: compression method {m.method} isn't supported")
    if max(m.usize, m.csize) > cap:
        raise FetchError(f"{m.name} is {m.usize:,} bytes, over the {cap:,}-byte cap")
    if m.offset + 30 + len(m.raw_name) + m.csize > f.size:
        raise FetchError(f"{m.name} runs past the end of the zip")
    end = min(f.size, m.offset + 30 + len(m.raw_name) + 256 + m.csize)  # 256: room for the local extra field
    buf = f.read(m.offset, end - m.offset)
    if buf[:4] != LOCAL:
        raise FetchError(f"{m.name}: no local header at {m.offset:,}")
    n, e = struct.unpack_from("<HH", buf, 26)
    if buf[30:30 + n] != m.raw_name:
        raise FetchError(f"{m.name}: its local header names {buf[30:30 + n]!r}")
    start = 30 + n + e
    if len(buf) < start + m.csize:
        if m.offset + start + m.csize > f.size:
            raise FetchError(f"{m.name} runs past the end of the zip")
        buf += f.read(m.offset + len(buf), start + m.csize - len(buf))
    packed = buf[start:start + m.csize]
    if m.method == 0:
        data = packed
    else:
        z = zlib.decompressobj(-15)
        try:
            data = z.decompress(packed, m.usize + 1)  # never more than one byte past the stated size
        except zlib.error as e:
            raise FetchError(f"{m.name} doesn't inflate: {e}") from None
    if len(data) != m.usize:
        raise FetchError(f"{m.name} is {len(data):,} bytes, not the {m.usize:,} its directory entry says")
    if zlib.crc32(data) != m.crc:
        raise FetchError(f"{m.name}: CRC-32 mismatch")
    return data


# --- Sources ---------------------------------------------------------------------------------------

def load_sources(path):
    """The sources file, checked: kinds, https URLs, safe and unique names, and outputs that name real inputs."""
    doc = json.loads(Path(path).read_text(encoding="utf-8"))
    if doc.get("schema") != 1:
        raise FetchError(f"{path}: schema {doc.get('schema')!r}, expected 1")
    by_id = {}
    for src in doc["sources"]:
        sid = src["id"]
        if len(safe_parts(sid)) != 1 or sid in by_id or src["kind"] not in KINDS:
            raise FetchError(f"{path}: bad source {sid!r}")
        by_id[sid] = src
        urls = [src["page"], src["licence_url"], *([src["url"]] if src["kind"] == "zip" else [])]
        urls += [f.get("url") or f["page"] for f in src["files"] if src["kind"] != "zip"]
        for url in urls:
            if urllib.parse.urlsplit(url).scheme != "https":
                raise FetchError(f"{path}: {sid}: {url} isn't https")
        in_raw = [raw_name(f) for f in src["files"] if src["kind"] != "freesound"]
        if len(set(in_raw)) != len(in_raw) or len({f["name"] for f in src["files"]}) != len(src["files"]):
            raise FetchError(f"{path}: {sid}: two files would share a name in raw/")
        if src["kind"] == "freesound" and not all(re.fullmatch(r"\d{1,10}", f["name"]) for f in src["files"]):
            raise FetchError(f"{path}: {sid}: Freesound files are named by their numeric id")
    names = set()
    for out in doc["outputs"]:
        src = by_id.get(out["source"])
        known = {f["name"] for f in src["files"]} if src else set()
        if (len(safe_parts(out["name"])) != 1 or out["name"] in names or not src or not out["inputs"]
                or not set(out["inputs"]) <= known or min(out.get(k, 0) for k in ("gap_s", "fade_ms", "start_s")) < 0):
            raise FetchError(f"{path}: bad output {out.get('name')!r}")
        names.add(out["name"])
    return doc


def raw_name(spec):
    """The file name in raw/<source>/ for a zip member or file (a member keeps its last path part)."""
    return safe_parts(spec["name"])[-1]


@dataclass
class Raw:
    """An input file in raw/, checked against its pin."""
    path: Path
    sha256: str
    url: str  # the file's URL, the zip's URL, or the Freesound page
    member: str | None = None


def _matches(folder, fid):
    """The files in folder named <fid>__*, as Freesound names its downloads."""
    if not folder.is_dir():
        return []
    return sorted(p for p in folder.glob(f"{fid}__*") if p.is_file() and not p.name.endswith(".part"))


class Run:
    """One fetch: what came in, what was pinned, what failed, and what's missing."""

    def __init__(self, doc, cache, http, freesound_dir, pin=False, log=print):
        self.doc, self.http, self.freesound_dir, self.pin, self.log = doc, http, Path(freesound_dir), pin, log
        self.cache = Path(cache).resolve()
        self.raw_dir, self.wav_dir = self.cache / "raw", self.cache / "wav48"
        self.raws = {}  # (source id, name) -> Raw
        self.pinned, self.problems, self.missing = [], [], []

    def check(self, sid, spec, size, sha, name):
        """Compare with the pin, or record a pin under --pin. Errors name the file; the caller adds the source id."""
        if spec.get("sha256"):
            if sha != spec["sha256"] or spec.get("size", size) != size:
                raise FetchError(f"{name}: {size:,} B, SHA-256 {sha}; pinned {spec.get('size')} B, {spec['sha256']}")
        elif self.pin:
            spec["size"], spec["sha256"] = size, sha
            self.pinned.append(f"{sid}: {name}")
        else:
            raise FetchError(f"{name} isn't pinned (it is {size:,} B, SHA-256 {sha}); run with --pin to record it")

    def have(self, spec, path):
        """True if path is already in raw/ and matches its pin."""
        return bool(spec.get("sha256")) and path.is_file() and sha256_file(path) == spec["sha256"]

    def fetch_zip(self, src):
        sid, todo = src["id"], []
        for spec in src["files"]:
            path = inside(self.raw_dir, sid, raw_name(spec))
            if self.have(spec, path):
                self.raws[sid, spec["name"]] = Raw(path, spec["sha256"], src["url"], spec["name"])
            else:
                todo.append((spec, path))
        if not todo:
            n = len(src["files"])
            self.log(f"{sid}: {'all ' * (n > 1)}{n} member{'s' * (n != 1)} already in raw/{sid}/")
            return
        before = self.http.requests, self.http.received
        f = RemoteFile(self.http, src["url"], src.get("size"))
        if src.get("size") is None and self.pin:
            src["size"] = f.size
            self.pinned.append(f"{sid}: zip size")
        members = central_directory(f)
        self.log(f"{sid}: {len(todo)} of {len(src['files'])} members to fetch; the zip is {f.size:,} B with {len(members):,} members")
        for spec, path in todo:
            m = members.get(spec["name"])
            if m is None:
                raise FetchError(f"{spec['name']} isn't in the zip")
            if spec.get("size") is not None and m.usize != spec["size"]:
                raise FetchError(f"{spec['name']} is {m.usize:,} B in the zip, pinned {spec['size']:,}")
            data = read_member(f, m)
            sha = hashlib.sha256(data).hexdigest()
            self.check(sid, spec, len(data), sha, spec["name"])
            write_atomic(path, data)
            self.raws[sid, spec["name"]] = Raw(path, sha, src["url"], spec["name"])
            self.log(f"  {spec['name']}: {len(data):,} B (CRC-32 and SHA-256 ok)")
        n, got = self.http.requests - before[0], self.http.received - before[1]
        self.log(f"{sid}: fetched {len(todo)} member{'s' * (len(todo) != 1)} in {n} Range requests, {got:,} B received")

    def fetch_files(self, src):
        sid = src["id"]
        for spec in src["files"]:
            path = inside(self.raw_dir, sid, raw_name(spec))
            if self.have(spec, path):
                self.log(f"{sid}: already in raw/{sid}/: {path.name}")
            else:
                part, n, sha, sha1 = self.http.download(spec["url"], path, spec.get("size"))
                try:
                    if spec.get("sha1") and sha1 != spec["sha1"]:
                        raise FetchError(f"{spec['url']}: SHA-1 {sha1}, the publisher's is {spec['sha1']}")
                    self.check(sid, spec, n, sha, spec["name"])
                    os.replace(part, path)
                finally:
                    part.unlink(missing_ok=True)
                self.log(f"{sid}: downloaded {path.name}: {n:,} B (SHA-256{' and the publisher SHA-1' if spec.get('sha1') else ''} ok)")
            self.raws[sid, spec["name"]] = Raw(path, spec["sha256"], spec["url"])

    def fetch_freesound(self, src):
        """Copies the owner's downloads of the clips (<id>__*) into raw/. A missing clip is reported, not fatal, and a
        clip that fails its pin is a problem for that clip only."""
        sid = src["id"]
        cached = inside(self.raw_dir, sid)
        for spec in src["files"]:
            fid, page, pin = spec["name"], spec["page"], spec.get("sha256")
            try:
                mine = [p for p in _matches(cached, fid) if not pin or sha256_file(p) == pin]
                if mine:
                    path = mine[0]
                    self.log(f"{sid}: already in raw/{sid}/: {path.name}")
                else:
                    found = _matches(self.freesound_dir, fid)
                    if not found:
                        self.missing.append(f"{fid} \"{spec['title']}\" by {spec['author']}: {page}")
                        self.log(f"{sid}: MISSING {fid} \"{spec['title']}\" by {spec['author']}: download the original from {page}"
                                 f" into {self.freesound_dir}")
                        continue
                    if len(found) > 1:
                        found.sort(key=lambda p: sha256_file(p) != pin)  # the pinned one first
                        self.log(f"{sid}: {len(found)} files match {fid}__*; using {found[0].name}")
                    if found[0].stat().st_size > MAX_FILE:
                        raise FetchError(f"{found[0]} is over the {MAX_FILE:,}-byte cap")
                    data = found[0].read_bytes()
                    if pin:
                        try:
                            self.check(sid, spec, len(data), hashlib.sha256(data).hexdigest(), found[0].name)
                        except FetchError as e:
                            raise FetchError(f"{e}. Is it the preview? Download the original again from {page}") from None
                    path = inside(cached, *safe_parts(found[0].name))
                    write_atomic(path, data)
                    self.log(f"{sid}: copied {found[0]} ({len(data):,} B)")
                sha = pin or sha256_file(path)
                if not pin and self.pin:
                    self.check(sid, spec, path.stat().st_size, sha, path.name)
                elif not pin:
                    self.log(f"{sid}: {path.name} isn't pinned; its SHA-256 goes in the manifest only")
                self.raws[sid, fid] = Raw(path, sha, page)
            except FetchError as e:
                self.problems.append(f"{sid}: {e}")
                self.log(f"{sid}: FAILED: {e}")

    def fetch_all(self):
        fetchers = {"zip": self.fetch_zip, "file": self.fetch_files, "freesound": self.fetch_freesound}
        for src in self.doc["sources"]:
            try:
                fetchers[src["kind"]](src)
            except FetchError as e:
                self.problems.append(f"{src['id']}: {e}")
                self.log(f"{src['id']}: FAILED: {e}")


# --- Conversion ------------------------------------------------------------------------------------

def audio_libs():
    try:
        import numpy
        import soundfile
        import soxr
    except ImportError as e:
        sys.exit(f"{e}. Install the dependencies with:\n    {INSTALL}\n(not --user: python -I leaves out the user site-packages)")
    return numpy, soundfile, soxr


def downmix(x):
    """frames x channels -> mono, the mean of the channels."""
    return x.mean(axis=1) if x.shape[1] > 1 else x[:, 0].copy()


def fade(x, n):
    """x with a raised-cosine fade over its first and last n samples (so a join into silence doesn't click)."""
    np = audio_libs()[0]
    n = min(n, len(x) // 2)
    if n:
        ramp = 0.5 - 0.5 * np.cos(np.pi * (np.arange(n) + 0.5) / n)
        x = x.copy()
        x[:n] *= ramp
        x[-n:] *= ramp[::-1]
    return x


def join(pieces, gap_frames):
    """The pieces end to end, with gap_frames of silence between neighbours."""
    np = audio_libs()[0]
    parts = []
    for i, p in enumerate(pieces):
        if i and gap_frames:
            parts.append(np.zeros(gap_frames))
        parts.append(p)
    return np.concatenate(parts) if parts else np.zeros(0)


def trim(x, start_s=0.0, length_s=None):
    a = round(start_s * RATE)
    b = len(x) if length_s is None else a + round(length_s * RATE)
    if not 0 <= a < b <= len(x):
        raise FetchError(f"the trim {start_s} s + {length_s} s is outside the {len(x) / RATE:.2f} s take")
    return x[a:b]


def to_pcm16(x):
    """Rounded to 16-bit (no dither), and how many samples were past full scale (|x| > 1) and had to be clipped.
    16-bit input comes back unchanged; a 24-bit or float full-scale peak rounding to 32768 is held at 32767 and not counted."""
    np = audio_libs()[0]
    clipped = int(np.count_nonzero(np.abs(x) > 1.0))
    return np.clip(np.round(x * 32768.0), -32768, 32767).astype("<i2"), clipped


def wav_pcm16(samples, rate=RATE):
    """A canonical 44-byte-header mono 16-bit PCM WAV, the plainest form hound (nf-app's reader) takes."""
    data = samples.astype("<i2").tobytes()
    fmt = struct.pack("<HHIIHH", 1, 1, rate, rate * 2, 2, 16)
    return b"RIFF" + struct.pack("<I", 36 + len(data)) + b"WAVE" + b"fmt " + struct.pack("<I", 16) + fmt + b"data" + struct.pack("<I", len(data)) + data


_BITS = {"PCM_U8": "8-bit", "PCM_S8": "8-bit", "PCM_24": "24-bit", "PCM_32": "32-bit integer", "FLOAT": "32-bit float", "DOUBLE": "64-bit float"}


def describe_changes(out, infos, clipped):
    """What the recipe did to the sources, for the manifest and ATTRIBUTION.md."""
    changes = []
    if len(infos) > 1:
        gap = out.get("gap_s", 0)
        changes.append(f"{len(infos)} recordings joined end to end" + (f", with {gap:g} s of silence between them" if gap else ""))
    containers = sorted({i["format"] for i in infos} - {"WAV", "WAVEX"})
    if containers:
        changes.append(f"decoded from {'/'.join(containers)}")
    channels = sorted({i["channels"] for i in infos if i["channels"] > 1})
    if channels:
        changes.append(f"downmixed from {'/'.join(map(str, channels))} channels to mono by averaging them")
    rates = sorted({i["rate"] for i in infos if i["rate"] != RATE})
    if rates:
        changes.append(f"resampled from {'/'.join(f'{r:,}' for r in rates)} Hz to 48,000 Hz (soxr, very high quality)")
    if out.get("fade_ms"):
        changes.append(f"a {out['fade_ms']:g} ms fade in and out on each recording")
    depths = sorted({_BITS.get(i["subtype"], i["subtype"]) for i in infos if i["subtype"] != "PCM_16"})
    if depths or channels or rates or out.get("fade_ms"):
        changes.append(("requantised from " + "/".join(depths) + " to" if depths else "rounded back to") + " 16-bit (no dither)")
    if "start_s" in out or "length_s" in out:
        a = out.get("start_s", 0)
        changes.append(f"cut to {a:g}-{a + out['length_s']:g} s" if "length_s" in out else f"the first {a:g} s cut")
    if clipped:
        changes.append(f"{clipped:,} samples clipped to full scale")
    return changes or ["none: the samples are copied unchanged into a new WAV file"]


def convert(out, raws):
    """The output's WAV bytes and its audio facts for the manifest."""
    np, sf, soxr = audio_libs()
    pieces, infos = [], []
    for raw in raws:
        info = sf.info(str(raw.path))
        x, rate = sf.read(str(raw.path), dtype="float64", always_2d=True)
        mono = downmix(x)
        if rate != RATE:
            mono = soxr.resample(mono, rate, RATE, quality="VHQ")
        pieces.append(fade(mono, round(out.get("fade_ms", 0) * RATE / 1000)))
        infos.append({"rate": rate, "channels": x.shape[1], "format": info.format, "subtype": info.subtype, "frames": len(x)})
    y = join(pieces, round(out.get("gap_s", 0) * RATE))
    if "start_s" in out or "length_s" in out:
        y = trim(y, out.get("start_s", 0), out.get("length_s"))
    pcm, clipped = to_pcm16(y)
    return wav_pcm16(pcm), infos, describe_changes(out, infos, clipped)


def fingerprint(out, raws):
    key = {"converter": CONVERTER, "recipe": out, "inputs": [r.sha256 for r in raws]}
    return hashlib.sha256(json.dumps(key, sort_keys=True).encode()).hexdigest()


def build_outputs(run, previous):
    """Builds wav48/ from raw/ (skipping outputs that are already up to date) and returns the manifest entries."""
    sources = {s["id"]: s for s in run.doc["sources"]}
    entries = []
    for out in run.doc["outputs"]:
        src = sources[out["source"]]
        raws = [run.raws.get((src["id"], name)) for name in out["inputs"]]
        rel = f"wav48/{out['name']}.wav"
        path = inside(run.wav_dir, out["name"] + ".wav")
        if None in raws:
            gone = path.is_file()
            path.unlink(missing_ok=True)  # wav48/ holds only what the manifest lists
            run.log(f"skipped {rel}: its input isn't in raw/{' (removed its old copy)' if gone else ''}")
            continue
        fp = fingerprint(out, raws)
        prev, kept = previous.get(rel) or {}, ("sha256", "bytes", "frames", "duration_s", "changes", "inputs")
        if prev.get("fingerprint") == fp and all(k in prev for k in kept) and path.is_file() and sha256_file(path) == prev["sha256"]:
            audio = {k: prev[k] for k in kept}
            inputs = prev["inputs"]
            run.log(f"up to date: {rel}")
        else:
            data, infos, changes = convert(out, raws)
            write_atomic(path, data)
            frames = (len(data) - 44) // 2
            audio = {"sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data), "frames": frames,
                     "duration_s": round(frames / RATE, 3), "changes": changes}
            inputs = [{"url": r.url, **({"member": r.member} if r.member else {}), "raw": r.path.relative_to(run.cache).as_posix(),
                       "sha256": r.sha256, "rate": i["rate"], "channels": i["channels"], "format": f"{i['format']} {i['subtype']}",
                       "duration_s": round(i["frames"] / i["rate"], 3)} for r, i in zip(raws, infos)]
            run.log(f"built {rel}: {audio['duration_s']:.2f} s, {len(data):,} B; changes: {'; '.join(changes)}")
        entries.append({
            "file": rel, "sha256": audio["sha256"], "bytes": audio["bytes"], "rate": RATE, "channels": 1, "bits": 16,
            "frames": audio["frames"], "duration_s": audio["duration_s"], "source": src["id"],
            "title": out.get("title", src["title"]), "page": out.get("page", src["page"]),
            "licence": src["licence"], "licence_url": src["licence_url"], "tier": src["tier"],
            "attribution": out.get("attribution", src["attribution"]), "changes": audio["changes"],
            "inputs": inputs, "fingerprint": fp})
    return entries


# --- Manifest and attribution ----------------------------------------------------------------------

def dump_json(v, indent=0, used=0, width=130):
    """JSON with values kept on one line when they fit in `width`, and objects of plain values always on one line
    (one zip member a line)."""
    one = json.dumps(v, ensure_ascii=False)
    plain = isinstance(v, dict) and not any(isinstance(x, (dict, list)) for x in v.values())
    if not isinstance(v, (dict, list)) or not v or plain or indent + used + len(one) <= width:
        return one
    pad = " " * (indent + 2)
    if isinstance(v, list):
        body = [pad + dump_json(x, indent + 2, 0, width) for x in v]
    else:
        body = [f"{pad}{json.dumps(k, ensure_ascii=False)}: " + dump_json(x, indent + 2, len(json.dumps(k, ensure_ascii=False)) + 2, width)
                for k, x in v.items()]
    return ("[" if isinstance(v, list) else "{") + "\n" + ",\n".join(body) + "\n" + " " * indent + ("]" if isinstance(v, list) else "}")


MANIFEST_ABOUT = "Generated by tools/fetch_test_audio.py. A local test cache: never commit, re-host or share these files."


def attribution_md(entries, doc):
    lines = ["# Test-audio attribution", "",
             "Generated by `tools/fetch_test_audio.py` from `manifest.json`. These files are a local test cache for noise-filter:",
             "they are not committed, re-hosted or shared. Tier `unlicensed` means no licence grants rights to the recordings.", ""]
    for e in entries:
        zips = [i for i in e["inputs"] if "member" in i]
        if zips:
            frm = f"{zips[0]['url']}, member{'s' if len(zips) > 1 else ''} " + ", ".join(f"`{i['member']}`" for i in zips)
        else:
            frm = ", ".join(i["url"] for i in e["inputs"])
        lines += [f"## {e['file']}", "",
                  f"- **Source:** [{e['title']}]({e['page']}); from {frm}",
                  f"- **Licence:** [{e['licence']}]({e['licence_url']}); tier `{e['tier']}`",
                  f"- **Attribution:** {e['attribution']}",
                  f"- **Changes:** {'; '.join(e['changes'])}.",
                  f"- {e['duration_s']:.2f} s at 48 kHz, mono, 16-bit; SHA-256 `{e['sha256']}`", ""]
    used = {e["source"] for e in entries}
    for src in doc["sources"]:
        if src.get("notice") and src["id"] in used:
            lines += [f"## Notice: {src['title']}", "", "```", *src["notice"], "```", ""]
    return "\n".join(lines)


def write_if_changed(path, text):
    if path.is_file() and path.read_text(encoding="utf-8") == text:
        return False
    write_atomic(path, text.encode("utf-8"))
    return True


def run_all(run):
    """Fetches, builds and writes the manifest and ATTRIBUTION.md. Returns the manifest entries."""
    run.cache.mkdir(parents=True, exist_ok=True)
    run.fetch_all()
    manifest_path = run.cache / "manifest.json"
    previous = {}
    if manifest_path.is_file():
        try:
            previous = {e["file"]: e for e in json.loads(manifest_path.read_text(encoding="utf-8"))["outputs"]}
        except (ValueError, KeyError, TypeError):
            run.log("manifest.json was unreadable; rebuilding everything")
    entries = build_outputs(run, previous)
    manifest = dump_json({"about": MANIFEST_ABOUT, "outputs": entries}) + "\n"
    wrote = [n for n, t in (("manifest.json", manifest), ("ATTRIBUTION.md", attribution_md(entries, run.doc)))
             if write_if_changed(run.cache / n, t)]
    run.log(f"{'wrote ' + ' and '.join(wrote) if wrote else 'manifest.json and ATTRIBUTION.md unchanged'}: {len(entries)} outputs")
    return entries


def default_cache():
    base = os.environ.get("LOCALAPPDATA")
    if not base:
        sys.exit("LOCALAPPDATA isn't set: give the cache folder with --dir")
    return Path(base) / "noise-filter" / "test-audio"


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--dir", type=Path, help="the cache folder (default %%LOCALAPPDATA%%\\noise-filter\\test-audio)")
    ap.add_argument("--freesound-dir", type=Path, default=Path.home() / "Downloads",
                    help="where your Freesound downloads are (default %(default)s)")
    ap.add_argument("--sources", type=Path, default=SOURCES, help="the sources file (default %(default)s)")
    ap.add_argument("--pin", action="store_true", help="record the size and SHA-256 of entries that have none in the sources file")
    args = ap.parse_args(argv)
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(errors="replace")
    audio_libs()  # fail early, with the install line
    try:
        doc = load_sources(args.sources)
    except (OSError, ValueError, KeyError, TypeError, FetchError) as e:
        sys.exit(f"{args.sources}: {e}")
    run = Run(doc, args.dir or default_cache(), Http(), args.freesound_dir, pin=args.pin)
    print(f"cache: {run.cache}", flush=True)
    t = time.monotonic()
    entries = run_all(run)
    if run.pinned:
        args.sources.write_text(dump_json(doc) + "\n", encoding="utf-8")
        print(f"pinned {len(run.pinned)} entries in {args.sources}")
    total = sum(e["duration_s"] for e in entries)
    print(f"done in {time.monotonic() - t:.0f} s ({run.http.requests} HTTP requests, {run.http.received:,} B received): "
          f"{len(entries)} files, {total:.0f} s of audio, in {run.wav_dir}")
    if run.missing:
        print(f"missing {len(run.missing)} Freesound clip(s). Log in to freesound.org and, on each page below, use Download"
              f" (the original, not the preview). Keep Freesound's file name (<id>__<user>__<title>) and save it into"
              f" {run.freesound_dir}, or pass --freesound-dir; then run again:")
        for m in run.missing:
            print(f"  {m}")
    for p in run.problems:
        print(f"FAILED: {p}")
    return 1 if run.problems else 0


if __name__ == "__main__":
    sys.exit(main())
