"""Stack spills and other review flags in the hot loops of noise-filter's real-time code, read from
the binary's post-LTO asm.

Adapted on 2026-10-26 from TMR's `tools/hot_loop_spills.py` (TMR-APP, TODO 99 there; the rules it
checks are Rule 6 of TMR-APP/doc/simd_codegen_rules.md). How to use it, and the codegen rules for
our DSP loops: doc/dev/api/hot-loop-tools.md.

Which functions. Only audio-thread code. A function is ours when its own path starts with one of
HOT's prefixes and isn't under COLD_PATHS (the modules and crates that run on other threads), and
its name isn't in COLD (a closure is never cold by name). The own path comes from demangling the v0
symbol: the module an inherent or trait impl is written in, a closure's parent function, a generic
function itself, not the types it's instantiated with. So
`<nf_core::noise::NoiseStage as nf_core::stage::Stage>::process` is nf_core::noise's, and std's
`drop_glue::<nf_core::dsp::delay::DelayLine>` isn't ours. One exception: a std generic instantiated
with one of our closures is selected, as it can hold the closure's body (a thread's entry,
`<{closure} as FnOnce>::call_once::{shim:vtable#0}`, or an iterator adapter that didn't inline).
A function that names a closure written under COLD_PATHS (a thread's entry, a callback, one of our
generics instantiated with it) runs on that code's thread, so it is left out whatever its path.
`--paths` inside a COLD_PATHS entry (`--paths nf_stages::synth`) lifts that entry.

Which loops. A hot loop is an innermost loop (a backward branch's range with no other loop inside
it) that walks memory, makes no call except to a panic, and has at most 600 instructions. Walking
memory means an access off the stack, or to an array on it through an index register (a scratch
block on the stack, or accumulators that should have been registers; both show in the stack
columns). A "loop" whose range holds a `ret` is an epilogue laid out inside an outer loop's blocks,
and is skipped.

The table, per function, worst first:
  round trip  a stack slot stored and reloaded each iteration: a value carried through memory, so a
              store-to-load latency on the loop's dependency chain (an RMW like `addl $1, 8(%rsp)`
              counts as both)
  reload      a stack slot the loop only reads (a value it never changes, reloaded each iteration)
              or only writes (a value kept for after the loop, an array on the stack filled)
Each is split by the register spilled: gpr (general purpose) or simd (xmm/ymm/zmm, or a k mask).
A stack access through an index register (`64(%rsp,%rax,4)`, an array kept on the stack) counts.
In a function that sets a frame pointer (`.seh_setframe %rbp`), `(%rbp)` operands are stack too;
TMR's tool counted them as memory. Review columns, counted but not failed:
  panic    a branch from the loop to a panic (a bounds check, an unwrap, an overflow or division
           check left in the loop), or a call to one in it. In a per-sample loop it usually blocks
           vectorisation as well.
  scalar   float maths only in scalar form (`mulss`, `addss`, `maxss`...), none packed: the loop
           didn't vectorise. Right for a recurrence that can't be split (an IIR); otherwise read it.
           A recurrence often can be: a one-pole can be a table of powers (the gate's ramps are
           now index-derived instead), and `Dither` runs eight xorshift lanes. A vectorised loop over a slice of unknown length leaves a scalar
           remainder loop, which shows here too. Loops over a whole `Block` have none.
  indexed  memory addressed through an index register (`(%rax,%rcx,4)`) rather than a walked
           pointer. It cost TMR 6-16% in memory-bound kernels. Our blocks sit in L1, so treat it
           as information unless a measurement says otherwise.
  const    a constant read from the binary's data (`(%rip)`) every iteration: a reload that the
           stack columns can't see (an abs mask folded into an `andps`, a broadcast left in the loop)
  long     over 150 instructions: read it rather than assume it is cold

A hot-shaped loop that makes any other call isn't in the table. Below it, those loops are counted
by why: timing (QPC, `Instant`), device (WASAPI, event waits, MMCSS), dispatch (the callee is one of
ours with loops of its own, or a few calls below), or a struct move (a memcpy of a constant size).
The rest are listed by function and callee, to read:
  rt     an allocation, a lock, logging, formatting or a sleep: a real-time rule breach if the loop
         runs on the audio thread (CLAUDE.md), fine in set-up code
  libm   a libm call (`powf`, `expf`, `log10f`, `fmaf`...): per-sample maths that belongs per block,
         or `mul_add` without FMA (our x86-64-v3 baseline has it; at v2 `mul_add` was a call to `fmaf`)
  read   anything else: an indirect call (`dyn Stage`, a COM method), a memcpy or memset of a
         computed size (a copy or fill loop LLVM collapsed), a helper that didn't inline

Build the asm, then run this from any directory (its paths are the repo's):
    cargo rustc --release -p nf-app --bin noise-filter -- --emit asm
    python tools/hot_loop_spills.py [file.s] [--detail SUBSTRING [--all]] [--max N]
                                    [--paths PREFIX,PREFIX...] [--functions]
With no file it reads the newest bin asm that has a `main`, in the default target dir or any other
(`CARGO_TARGET_DIR=target/NAME`): `target/[NAME/]release/build/nf-app/<hash>/out/noise_filter.s`
(Cargo's build-dir layout; `deps/noise_filter-<hash>.s` in the old one). One with no `main` there
(a lib target's, or one cut short before it) is passed over with a note. A lib's asm,
`build/nf-<crate>/<hash>/out/nf_<crate>-<hash>.s` (from `cargo rustc -p nf-core ...`), is pre-LTO
and makes a believable table, so it is never read: a note names it when it is newer than the asm
read, and named on the command line it is refused. An asm with no `main` exits 1. So does one with
no v0 (`_R`) symbol, and one in which no hot loop is found (HOT no longer names the crates' modules).
  --detail  prints each flagged loop (stack or any review flag) of the functions whose name contains
            SUBSTRING, with the loops that call something listed; --all adds their clean loops
  --max     exits 1 when the loops touching the stack exceed N (a ratchet for a pre-release check)
  --paths   replaces HOT, e.g. `--paths nnnoiseless` to read a dependency's loops (inlined code is
            counted in the function it was inlined into), or `--paths sonora` for sonora's loops
  --functions  lists the functions selected and their hot loops, to check HOT and COLD
Its tests (selection and the default asm): python tools/test_hot_loop_spills.py
"""
import glob
import os
import re
import sys
from collections import defaultdict

sys.dont_write_bytecode = True  # no __pycache__ in tools/ if another tool imports this one

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# Ours, by own-path prefix: what runs per block or per packet on the audio thread (the chain, the
# DSP, the stage interface and the noise stage, the stages and models, the engine's capture,
# conversion and render loops, and the latency test's tap, which `audio.rs` feeds). nf_core::latency's
# analysis and sweep run off the audio thread and may allocate, so they aren't here; RollingStats
# (latency::stats) runs per block.
HOT = ["nf_core::dsp", "nf_core::chain", "nf_core::rt", "nf_core::latency::stats", "nf_core::telemetry",
       "nf_core::noise", "nf_core::stage", "nf_stages", "nf_engine"]
# Code that never runs on the audio thread: parts of HOT, and the crates that call into HOT from
# their own threads. A function under one is left out, and so is one that names a closure written
# under one (`is_hot`).
COLD_PATHS = [
    "nf_core::stage::check_host_profile",  # the host-profile check, at activate (and its closures)
    "nf_core::stage::state",  # state save and load: never on the audio thread while it runs
    "nf_stages::synth", "nf_stages::bench",  # test signals, the bench command (the main thread)
    "nf_stages::harness",  # the offline harness: the `process` and `quality` commands, the tests
    "nf_engine::api", "nf_engine::control", "nf_engine::errors",  # the caller's and the control thread
    "nf_engine::devices",  # enumeration (its own worker), and the lookup a start makes
    "nf_engine::latency_test",  # the latency test's thread: the probe, its capture and the analysis
    "nf_gui", "nf_app",  # the window, the CLI, the offline `process` command
]
# The standard library's crates: their generics are selected when instantiated with one of our closures
STD_CRATES = {"core", "alloc", "std"}
# Functions, by their last name, that run off the hot path: construction, activation (a model's
# `prepare`), reset (a fill LLVM turns into a memset), a stage's state, formatting, clones,
# comparisons. Add a cold helper here when it shows.
COLD = {"new", "default", "activate", "prepare", "build_chain", "reset", "save_state", "load_state", "fmt", "clone",
        "clone_from", "eq", "ne", "drop", "info", "stats"}

# A call that ends in a panic: such a call is on a cold branch, so the loop stays in the table
PANIC = re.compile(r"^<?(core|std)::panicking::|(^|::)(unwrap_failed|expect_failed|assert_failed\w*|handle_error"
                   r"|handle_alloc_error|panic_\w+|\w+_fail)(::<.*)?$|^(abort|__fastfail)$|^std::process::abort")
# Calls a hot-shaped loop may make off the per-sample path (once a block or a packet), by callee
COLD_CALLS = {
    "timing": [r"^QueryPerformance(Counter|Frequency)$", r"^<?std::time::", r"::perf_counter::", r"^GetTickCount",
               r"^GetSystemTimePreciseAsFileTime$"],
    "device": [r"^<?wasapi::", r"^<?windows::Win32::Media::", r"^WaitFor(Single|Multiple)Objects?(Ex)?$",
               r"^(Set|Reset)Event$", r"^AvSetMmThreadCharacteristics[AW]?$", r"^AvRevertMmThreadCharacteristics$"],
}
# Calls listed below the table, by kind (see the module docs)
LISTED = {
    "rt": [r"^__rust_", r"^__rdl_", r"^__rg_", r"^<?alloc::(raw_vec|alloc|fmt|string|vec|collections|boxed|sync|rc)::",
           r"^Heap(Alloc|Free|ReAlloc|Size)$", r"^GetProcessHeap$", r"^(Virtual|Local|Global)(Alloc|Free|Lock)$",
           r"^<?(nf_mem|rlsf|log|parking_lot)::", r"^<?std::(sync|io|fs|thread|sys::sync)::", r"^<?core::fmt::",
           r"SRWLock|CriticalSection|^WaitOnAddress$|^WakeByAddress|^Sleep(Ex)?$|^SwitchToThread$",
           r"^OutputDebugString|^WriteFile$|^WriteConsole"],
    "libm": [r"^(fma|pow|exp|exp2|expm1|log|log10|log2|log1p|sqrt|sin|cos|tan|asin|acos|atan|atan2|sinh|cosh|tanh"
             r"|cbrt|hypot|fmod|ldexp|frexp|round|floor|ceil|trunc|nearbyint|rint)f?$", r"^<?(std|core)::f(32|64)::"],
}
COLD_CALLS = {k: [re.compile(p) for p in v] for k, v in COLD_CALLS.items()}
LISTED = {k: [re.compile(p) for p in v] for k, v in LISTED.items()}
# Listed kinds, in the order a loop with several is filed under
LIST_ORDER = ["rt", "libm", "read"]
# memcpy and its kind: with a constant size it is a struct (or a whole Block) moved, not a copy loop
MEM_CALLS = ("memcpy", "memmove", "memset")
CONST_SIZE = re.compile(r"^mov[lq] \$(\d+), %r8d?$")
MAX_INSTRUCTIONS = 600
LONG = 150

BUILD = "cargo rustc --release -p nf-app --bin noise-filter -- --emit asm"
# Where the build leaves the bin's asm, relative to the repo: Cargo's build-dir layout, then the
# old one, each in the default target dir or another (CARGO_TARGET_DIR=target/NAME)
BIN_ASM = ["target/release/build/nf-app/**/*.s", "target/*/release/build/nf-app/**/*.s",
           "target/release/deps/noise_filter-*.s", "target/*/release/deps/noise_filter-*.s"]
# And the libs' (pre-LTO), never read: named when newer than the asm read, so that it isn't mistaken
# for the one read
LIB_ASM = ["target/release/build/nf-*/**/*.s", "target/*/release/build/nf-*/**/*.s",
           "target/release/deps/nf_*-*.s", "target/*/release/deps/nf_*-*.s"]
# A `main` label at a line's start: only the bin's asm has one
MAIN = b"\nmain:"

FUNC = re.compile(r"^(_R[\w.$]+):")
LABEL = re.compile(r"^(\.L\w+):")
BRANCH = re.compile(r"^\s+(j[a-z]+)\s+(\.L\w+)")
# A memory operand: (base, index); the displacement before it may be a symbol (`sym+8(%rip)`)
MEM = re.compile(r"\((%\w+)?(?:,(%\w+))?(?:,\d)?\)")
SIMD = re.compile(r"%[xyz]mm\d+|%k[0-7]\b")
# Float arithmetic, scalar (ss, sd) or packed (ps, pd)
FLOAT_OP = re.compile(r"^v?(?:add|sub|mul|div|max|min|sqrt|rcp|rsqrt|round|hadd|hsub|dp)(ss|sd|ps|pd)\b"
                      r"|^vf(?:n?m(?:add|sub)|maddsub|msubadd)(?:132|213|231)(ss|sd|ps|pd)\b")
# Stores that don't read their destination; any other op with a memory destination reads it too
PURE_STORES = ("mov", "vmov", "set", "pextr", "vpextr", "extractps", "vextractps", "vextract", "stos", "vpmov", "kmov")
COMPARES = ("cmp", "test", "ucomis", "comis", "vucomis", "vcomis", "bt")
KINDS = ["round trip gpr", "round trip simd", "reload gpr", "reload simd"]
REVIEW = ["panic", "scalar", "indexed", "const", "long"]


# --- v0 demangling: the own path, for selection, and a readable name ----------------------------

B62 = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"
BASIC = dict(a="i8", b="bool", c="char", d="f64", e="str", f="f32", h="u8", i="isize", j="usize", l="i32", m="u32",
             n="i128", o="u128", s="i16", t="u16", u="()", v="...", x="i64", y="u64", z="!", p="_")
INTEGER = set("htmyojaslxni")


class Path:
    """A demangled path: its display segments, its crate, its spine (crate, modules, then the impl's
    type or `<T as Trait>`, the function, closures: what HOT matches) and whether it is a closure."""
    __slots__ = ("segs", "crate", "spine", "closure")

    def __init__(self, segs, crate, spine, closure=False):
        self.segs, self.crate, self.spine, self.closure = segs, crate, spine, closure

    def text(self, short=False):
        if not short:
            return "::".join(self.segs)
        if not self.closure:
            return self.segs[-1]  # in a generic argument: the last name
        # A closure: its function and the module or type that is in, so that the audio thread's
        # `audio::spawn::{closure#0}` reads apart from the GUI's `Saver::spawn::{closure#0}`
        k = len(self.segs) - 1
        while k > 0 and self.segs[k].startswith("{"):
            k -= 1
        return "::".join(self.segs[max(0, k - 1):])


class Demangler:
    """A v0 symbol's path (https://doc.rust-lang.org/rustc/symbol-mangling/v0.html). Generic
    arguments are printed short (`Box<dyn Stage>`), lifetimes dropped, punycode left encoded."""

    def __init__(self, s):
        self.s, self.i, self.short, self.depth = s, 0, 0, 0
        self.closures = []  # every closure's spine met, generic arguments' too

    def peek(self):
        return self.s[self.i:self.i + 1]

    def eat(self, c):
        if self.peek() == c:
            self.i += 1
            return True
        return False

    def b62(self):
        if self.eat("_"):
            return 0
        n = 0
        while True:
            c = self.peek()
            self.i += 1
            if c == "_":
                return n + 1
            if not c or c not in B62:
                raise ValueError("base-62 number")
            n = n * 62 + B62.index(c)

    def opt(self, tag):
        return self.b62() + 1 if self.eat(tag) else 0

    def ident(self):
        dis = self.opt("s")
        self.eat("u")
        j = self.i
        if self.peek() == "0":
            self.i += 1
        else:
            while self.peek().isdigit():
                self.i += 1
        if j == self.i:
            raise ValueError("identifier")
        n = int(self.s[j:self.i])
        self.eat("_")
        name = self.s[self.i:self.i + n]
        if len(name) != n:
            raise ValueError("identifier length")
        self.i += n
        return name, dis

    def backref(self, parse):
        at = self.i - 1
        pos = self.b62()
        if pos >= at:
            raise ValueError("forward backref")
        saved, self.i = self.i, pos
        try:
            return parse()
        finally:
            self.i = saved

    def path(self, in_type=False):
        self.depth += 1
        if self.depth > 200:
            raise ValueError("too deep")
        try:
            return self._path(in_type)
        finally:
            self.depth -= 1

    def _path(self, in_type):
        c = self.peek()
        self.i += 1
        if c == "C":
            name, _ = self.ident()
            return Path([name], name, [name])
        if c in "MX":
            self.opt("s")
            impl = self.path()
            text, tpath = self.type_()
            if c == "X":
                trait = self.path(True).text(self.short > 0)
                seg = f"<{text} as {trait}>"
                return Path([seg], impl.crate, impl.spine + [seg])
            if tpath is not None:
                return Path(list(tpath.segs) if not self.short else [text], impl.crate, impl.spine + [tpath.segs[-1].split("<")[0]])
            return Path([f"<{text}>"], impl.crate, impl.spine + [f"<{text}>"])
        if c == "Y":
            text, _ = self.type_()
            trait = self.path(True)
            seg = f"<{text} as {trait.text(self.short > 0)}>"
            return Path([seg], trait.crate, trait.spine[:-1] + [seg])
        if c == "N":
            ns = self.peek()
            self.i += 1
            p = self.path(in_type)
            name, dis = self.ident()
            if ns == "C":
                seg = f"{{closure#{dis}}}"
                self.closures.append("::".join(p.spine + [seg]))
            elif ns.isupper():
                seg = f"{{{'shim' if ns == 'S' else ns}:{name}#{dis}}}"
            else:
                seg = name
            return Path(p.segs + [seg], p.crate, p.spine + [seg], ns == "C")
        if c == "I":
            p = self.path(in_type)
            self.short += 1
            args = []
            while not self.eat("E"):
                if not self.peek():
                    raise ValueError("generic arguments")
                a = self.generic_arg()
                if a is not None:
                    args.append(a)
            self.short -= 1
            segs = p.segs
            if args:
                segs = p.segs[:-1] + [p.segs[-1] + ("<" if in_type else "::<") + ", ".join(args) + ">"]
            return Path(segs, p.crate, p.spine, p.closure)
        if c == "B":
            return self.backref(lambda: self.path(in_type))
        raise ValueError(f"path tag {c!r}")

    def generic_arg(self):
        if self.eat("L"):
            self.b62()
            return None
        if self.eat("K"):
            return self.const()
        return self.type_()[0]

    def type_(self):
        """(text, Path or None)"""
        c = self.peek()
        if not c:
            raise ValueError("type")
        if c in BASIC:
            self.i += 1
            return BASIC[c], None
        if c == "B":
            self.i += 1
            return self.backref(self.type_)
        if c not in "ASTRQPOFD":
            p = self.path(True)
            return p.text(self.short > 0), p
        self.i += 1
        if c == "A":
            t = self.type_()[0]
            return f"[{t}; {self.const()}]", None
        if c == "S":
            return f"[{self.type_()[0]}]", None
        if c == "T":
            ts = []
            while not self.eat("E"):
                ts.append(self.type_()[0])
            return "(" + ", ".join(ts) + ("," if len(ts) == 1 else "") + ")", None
        if c in "RQ":
            if self.eat("L"):
                self.b62()
            return ("&" if c == "R" else "&mut ") + self.type_()[0], None
        if c in "PO":
            return ("*const " if c == "P" else "*mut ") + self.type_()[0], None
        if c == "F":
            self.opt("G")
            unsafe = self.eat("U")
            if self.eat("K") and not self.eat("C"):
                self.ident()
            args = []
            while not self.eat("E"):
                args.append(self.type_()[0])
            ret = self.type_()[0]
            return ("unsafe " if unsafe else "") + "fn(" + ", ".join(args) + ")" + ("" if ret == "()" else " -> " + ret), None
        # D: dyn bounds, then a lifetime
        self.opt("G")
        traits = []
        while not self.eat("E"):
            text = self.path(True).text(self.short > 0)
            binds = []
            while self.eat("p"):
                name, _ = self.ident()
                binds.append(f"{name} = {self.type_()[0]}")
            traits.append(text + ("<" + ", ".join(binds) + ">" if binds else ""))
        if self.eat("L"):
            self.b62()
        return "dyn " + " + ".join(traits), None

    def hex_data(self):
        neg = self.eat("n")
        j = self.i
        while self.peek() and self.peek() != "_":
            self.i += 1
        digits = self.s[j:self.i]
        if not self.eat("_") or any(d not in "0123456789abcdef" for d in digits):
            raise ValueError("const data")
        v = int(digits, 16) if digits else 0
        return -v if neg else v

    def const(self):
        c = self.peek()
        self.i += 1
        if c == "B":
            return self.backref(self.const)
        if c == "p":
            return "_"
        if c in INTEGER:
            return str(self.hex_data())
        if c == "b":
            return "true" if self.hex_data() else "false"
        if c == "c":
            return repr(chr(self.hex_data()))
        if c == "e":
            j = self.i
            while self.peek() and self.peek() != "_":
                self.i += 1
            raw = self.s[j:self.i]
            self.eat("_")
            return repr(bytes.fromhex(raw).decode("utf-8", "replace"))
        if c in "RQ":
            return "&" + self.const()
        if c in "AT":
            items = []
            while not self.eat("E"):
                items.append(self.const())
            return ("[" + ", ".join(items) + "]") if c == "A" else ("(" + ", ".join(items) + ")")
        if c == "V":
            name = self.path(True).text(True)
            if self.eat("U"):
                return name
            if self.eat("T"):
                items = []
                while not self.eat("E"):
                    items.append(self.const())
                return f"{name}(" + ", ".join(items) + ")"
            if self.eat("S"):
                fields = []
                while not self.eat("E"):
                    field, _ = self.ident()
                    fields.append(f"{field}: {self.const()}")
                return f"{name} {{ " + ", ".join(fields) + " }"
        raise ValueError(f"const tag {c!r}")


class Desc:
    """What the tool needs of a symbol: its display name, crate, spine (joined), last name, and the
    closures it names (its own, and those in its generic arguments and self type)."""
    __slots__ = ("name", "crate", "spine", "last", "closure", "closures")

    def __init__(self, path, closures):
        self.name, self.crate, self.closure = path.text(), path.crate, path.closure
        self.spine = "::".join(path.spine)
        self.last = path.spine[-1]
        self.closures = closures


_described = {}


def describe(sym):
    """A v0 symbol (`_R...`, an LLVM suffix such as `.llvm.123` or a static's `.0` ignored) as a
    `Desc`, or None when it isn't one or doesn't parse."""
    if sym in _described:
        return _described[sym]
    out = None
    base = sym.split(".", 1)[0]
    if base.startswith("_R"):
        d = Demangler(base[2:])
        while d.peek().isdigit():  # an encoding version; v0's own is omitted
            d.i += 1
        try:
            out = Desc(d.path(), d.closures)
        except (ValueError, IndexError, RecursionError):
            out = None
    _described[sym] = out
    return out


def under(spine, prefixes):
    return any(spine == p or spine.startswith(p + "::") for p in prefixes)


def cold_paths(hot):
    """COLD_PATHS less any entry that one of `hot`'s prefixes lies within: `--paths
    nf_stages::synth` asks for exactly that module."""
    return [c for c in COLD_PATHS if not any(under(h, [c]) for h in hot)]


def is_hot(sym, hot, cold=None):
    d = describe(sym)
    if d is None:
        return False
    cold = cold_paths(hot) if cold is None else cold
    # Code that names a closure written off the audio thread runs there: a thread's entry, a
    # callback, one of our generics instantiated with the closure
    if any(under(c, cold) for c in d.closures):
        return False
    if under(d.spine, hot):
        return not under(d.spine, cold) and (d.closure or d.last not in COLD)
    # std's code around one of our closures, which can hold its body: a thread's entry
    # (`<{closure} as FnOnce>::call_once::{shim:vtable#0}`, `Builder::spawn_unchecked_`), an
    # iterator adapter that didn't inline
    return d.crate in STD_CRATES and any(under(c, hot) for c in d.closures)


# --- reading the asm --------------------------------------------------------------------------

def functions(path, hot=HOT):
    """The hot functions' bodies, by symbol, and the count of v0 symbols seen. Read line by line:
    the binary's asm runs to hundreds of MB."""
    cold = cold_paths(hot)
    out, name, body, seen = {}, None, [], 0
    with open(path, encoding="utf-8", errors="replace") as f:
        for line in f:
            line = line.rstrip("\n")
            if line.startswith("_R") and (m := FUNC.match(line)):
                if name:
                    out[name] = body
                seen += 1
                sym = m.group(1)
                name, body = (sym if is_hot(sym, hot, cold) else None), []
                continue
            if name:
                # A function ends at its end label or its unwind info's; a static, at the next symbol's preamble
                if line.startswith((".Lfunc_end", "\t.def\t")) or "seh_endproc" in line:
                    out[name] = body
                    name, body = None, []
                else:
                    body.append(line)
    if name:
        out[name] = body
    return out, seen


def loop_ranges(body):
    """The labels of a function's body, by line, and its innermost loops, each (start, end): a
    backward branch's target and the branch, with no other such range inside it."""
    labels = {}
    for i, line in enumerate(body):
        m = LABEL.match(line)
        if m:
            labels[m.group(1)] = i
    loops = [(labels[m.group(2)], i) for i, line in enumerate(body)
             if (m := BRANCH.match(line)) and m.group(2) in labels and labels[m.group(2)] < i]
    inner = [(s, e) for s, e in loops if not any(s <= s2 and e2 <= e and (s2, e2) != (s, e) for s2, e2 in loops)]
    return labels, inner


def instructions(lines):
    """The instructions among asm lines, whitespace normalised (`mulss %xmm1, %xmm0`)."""
    return [re.sub(r"\s+", " ", x.strip()) for x in lines
            if x.startswith("\t") and x.strip() and not x.strip().startswith((".", "#"))]


def accesses(x, fp):
    """An instruction's memory operands, each (kind, indexed): kind "stack", "const" (`%rip`) or
    "mem". None for an `lea` (arithmetic) or a `nop`."""
    if x.startswith(("lea", "nop")):
        return []
    out = []
    for m in MEM.finditer(x):
        base, index = m.group(1), m.group(2)
        if base == "%rip":
            out.append(("const", False))
        elif base == "%rsp" or (fp and base == "%rbp"):
            out.append(("stack", bool(index)))
        else:
            out.append(("mem", bool(index)))
    return out


def hot_shaped(loop, fp):
    """Whether a loop is shaped like a hot one: not too long, no `ret`, and walking memory (off the
    stack, or an array on it through an index register: a scratch block, or accumulators that
    should have been registers; both show in the stack columns)."""
    return (len(loop) <= MAX_INSTRUCTIONS and not any(x.startswith("ret") for x in loop)
            and any(k == "mem" or (k == "stack" and ix) for x in loop for k, ix in accesses(x, fp)))


def callee_name(target):
    """A call's target as the tables name it: a symbol demangled, an import by its name, an
    indirect call as written."""
    if target.startswith("_R"):
        d = describe(target)
        return d.name if d else target[:60]
    m = re.match(r"\*__imp_(\w+)\(%rip\)$", target)
    if m:
        return m.group(1)
    return "indirect " + target[1:] if target.startswith("*") else target


def call_kind(name):
    if PANIC.search(name.split("::<")[0]) or PANIC.search(name):
        return "panic"
    for kinds in (COLD_CALLS, LISTED):
        for kind, patterns in kinds.items():
            if any(p.search(name) for p in patterns):
                return kind
    return None


def loops_below(sym, kernels, depth=3, seen=None):
    """Whether our function `sym`, or one it calls a few levels down, has a loop: a callee that hands
    its work on to the closures holding its loops is a kernel all the same."""
    seen = set() if seen is None else seen
    if sym in seen or depth < 0:
        return False
    seen.add(sym)
    body = kernels[sym]
    if loop_ranges(body)[1]:
        return True
    callees = {x.split(None, 1)[-1] for x in instructions(body) if x.startswith("call")}
    return any(c in kernels and loops_below(c, kernels, depth - 1, seen) for c in callees)


def calls_in(loop, kernels):
    """A hot-shaped loop's calls, each (why, callee name): "panic", a COLD_CALLS or LISTED kind,
    "dispatch" (one of ours with loops below it), "struct move N B" (a memcpy of a constant size),
    or "read"."""
    found = []
    for i, x in enumerate(loop):
        if not x.startswith("call"):
            continue
        target = x.split(None, 1)[-1]
        name = callee_name(target)
        why = call_kind(name)
        if why is None and name in MEM_CALLS:
            # The size is the third argument, %r8, set a few instructions before
            size = next((m.group(1) for y in loop[max(0, i - 6):i][::-1] if (m := CONST_SIZE.match(y))), None)
            why = f"struct move {size} B" if size else "read"
        elif why is None and target in kernels:
            why = "dispatch" if loops_below(target, kernels) else "read"
        found.append((why or "read", name))
    # A loop that hands one of ours a block makes its other unknown calls once a block too
    if any(why == "dispatch" for why, _ in found):
        found = [("dispatch" if why == "read" else why, name) for why, name in found]
    return found


def leads_to_panic(body, labels, start, hops=3):
    """Whether the code from the label at line `start` reaches a panic call within a few
    instructions, falling through labels and following up to `hops` jumps."""
    i, n = start + 1, 0
    while i < len(body) and n < 16:
        line = body[i]
        i += 1
        x = instructions([line])
        if not x:
            continue
        x, n = x[0], n + 1
        op = x.split(None, 1)[0]
        if op.startswith("call"):
            return call_kind(callee_name(x.split(None, 1)[-1])) == "panic"
        if op == "jmp":
            target = labels.get(x.split(None, 1)[-1])
            if target is None or hops == 0:
                return False
            i, n, hops = target + 1, 0, hops - 1
        elif op.startswith(("ret", "ud2", "int3")):
            return False
    return False


def panic_exit(body, labels, s, e):
    """Whether the loop at lines s..e can branch out to a panic: a bounds or other check survived."""
    for line in body[s:e + 1]:
        m = BRANCH.match(line)
        if m and (t := labels.get(m.group(2))) is not None and not s <= t <= e and leads_to_panic(body, labels, t):
            return True
    return False


def scalar_only(loop):
    kinds = {m.group(1) or m.group(2) for x in loop if (m := FLOAT_OP.match(x))}
    return bool(kinds) and not kinds & {"ps", "pd"}


def classify(loop, fp):
    """(kind, stack lines, review flags) of a hot-shaped loop: kind "clean" when it keeps everything
    in registers, else its worst spill (`KINDS`); the flags are `REVIEW` names but "panic"."""
    acc = [(x, accesses(x, fp)) for x in loop]
    flags = []
    if scalar_only(loop):
        flags.append("scalar")
    if any(k == "mem" and ix for x, a in acc if not x.startswith("prefetch") for k, ix in a):
        flags.append("indexed")
    if any(k == "const" for _, a in acc for k, _ in a):
        flags.append("const")
    if len(loop) > LONG:
        flags.append("long")
    stack = [x for x, a in acc if any(k == "stack" for k, _ in a)]
    if not stack:
        return "clean", [], flags
    slot = re.compile(r"-?\d*\(%rsp(?:,%\w+(?:,\d)?)?\)" + (r"|-?\d*\(%rbp(?:,%\w+(?:,\d)?)?\)" if fp else ""))
    loads, stores, simd = set(), set(), set()
    for x in stack:
        op, _, operands = x.partition(" ")
        # The last operand is the destination: split at the commas outside an address's parentheses
        dest = re.split(r",(?![^(]*\))", operands)[-1].strip()
        for m in slot.finditer(x):
            key = m.group(0)
            if key in dest and slot.search(dest) and not op.startswith(COMPARES):
                stores.add(key)
                if not op.startswith(PURE_STORES):
                    loads.add(key)
            else:
                loads.add(key)
            if SIMD.search(x):
                simd.add(key)
    trips = loads & stores
    # The loop's worst spill decides its kind; SIMD when that spill is a SIMD register's
    if trips:
        return ("round trip simd" if trips & simd else "round trip gpr"), stack, flags
    # Read only (a value the loop never changes), or written only (a value kept for after the loop,
    # an array on the stack filled)
    return ("reload simd" if (loads or stores) & simd else "reload gpr"), stack, flags


def has_main(path, chunk_size=1 << 22):
    """Whether an asm defines `main`. Read in binary chunks: `main` comes early in the bin's asm,
    and a lib's (hundreds of MB, no `main`) is read once to its end."""
    tail = b"\n"  # a `main:` on the first line
    with open(path, "rb") as f:
        while chunk := f.read(chunk_size):
            buf = tail + chunk
            if MAIN in buf:
                return True
            tail = buf[-(len(MAIN) - 1):]  # a `main:` split across chunks
    return False


def found(patterns, root):
    return {os.path.normpath(f) for p in patterns for f in glob.glob(os.path.join(root, p), recursive=True)}


def binary_asm(args, root=ROOT):
    """The asm to read: the file named in `args`, else the newest bin asm the build left that has a
    `main`, from any target dir. Exits unless it is the binary's, the one with a `main`: a lib's has
    the same functions before LTO. Notes, on stderr, the newer asm it passed over."""
    files = [a for a in args if a.endswith(".s")]
    if files:
        if not has_main(files[0]):
            sys.exit(f"FAIL: {files[0]} isn't the binary's asm (no `main`), so not post-LTO: build it with `{BUILD}`")
        return files[0]
    bins = found(BIN_ASM, root)
    libs = found(LIB_ASM, root) - bins
    path, passed = None, []
    for f in sorted(bins, key=os.path.getmtime, reverse=True):
        if has_main(f):
            path = f
            break
        passed.append(f)
    if path is None:
        where = os.path.join(root, "target")
        if passed:
            sys.exit(f"FAIL: no bin asm under {where} has a `main` (newest: {passed[0]}): build it with `{BUILD}`")
        if libs:
            sys.exit(f"FAIL: only libs' asm under {where}, which is pre-LTO: build the bin's with `{BUILD}`")
        sys.exit(f"no asm under {where}: build it with `{BUILD}`")
    built = os.path.getmtime(path)
    for f in passed:
        print(f"note: passed over {f}, newer but with no `main` (a lib target's, or still being written)",
              file=sys.stderr)
    newer = sorted((f for f in libs if os.path.getmtime(f) > built), key=os.path.getmtime, reverse=True)
    if newer:
        print(f"note: passed over {len(newer)} newer lib asm (pre-LTO, not what the exe runs), newest {newer[0]}",
              file=sys.stderr)
    return path


def warn_if_stale(path):
    built = os.path.getmtime(path)
    newer = [f for f in glob.glob(os.path.join(ROOT, "crates", "*", "src", "**", "*.rs"), recursive=True)
             if os.path.getmtime(f) > built]
    if newer:
        newest = max(newer, key=os.path.getmtime)
        print(f"warning: {len(newer)} source file(s) changed after this asm was built (newest "
              f"{os.path.relpath(newest, ROOT)}); rebuild it with `{BUILD}`", file=sys.stderr)


def main():
    args = sys.argv[1:]
    detail = args[args.index("--detail") + 1] if "--detail" in args else None
    show_clean = "--all" in args
    limit = int(args[args.index("--max") + 1]) if "--max" in args else None
    hot_paths = args[args.index("--paths") + 1].split(",") if "--paths" in args else HOT
    path = binary_asm(args)
    warn_if_stale(path)
    print(f"asm: {path}")
    kernels, seen = functions(path, hot_paths)
    if not seen:
        sys.exit(f"FAIL: no v0 (`_R`) symbol in {path}: the selection needs v0 mangling (the pinned nightly's "
                 "default; else add `-C symbol-mangling-version=v0` to the rustflags)")
    columns = KINDS + REVIEW
    counts = defaultdict(lambda: dict.fromkeys(columns, 0))
    hot = set()
    calling = defaultdict(int)  # loops that call, by why
    to_read = []
    per_function = defaultdict(int)
    for sym, body in kernels.items():
        name = describe(sym).name
        fp = any("seh_setframe" in line and "%rbp" in line for line in body)
        labels, ranges = loop_ranges(body)
        for s, e in ranges:
            loop = instructions(body[s:e + 1])
            if not hot_shaped(loop, fp):
                continue
            called = calls_in(loop, kernels)
            blocking = [(why, callee) for why, callee in called if why != "panic"]
            if blocking:
                whys = {why for why, _ in blocking}
                listed = [k for k in LIST_ORDER if k in whys]
                calling[listed[0] if listed else sorted(whys)[0]] += 1
                if listed:
                    to_read.append((name, len(loop), sorted({f"{callee} ({why})" for why, callee in blocking if why in LIST_ORDER})))
                if detail and detail in name and (listed or show_clean):
                    print(f"--- {name}: calls {', '.join(sorted({f'{c} ({w})' for w, c in blocking}))}, {len(loop)} instructions")
                    print("    " + "\n    ".join(loop))
                continue
            kind, stack, flags = classify(loop, fp)
            if called or panic_exit(body, labels, s, e):
                flags = ["panic"] + flags
            hot.add(name)
            per_function[name] += 1
            if kind != "clean":
                counts[name][kind] += 1
            for flag in flags:
                counts[name][flag] += 1
            if kind == "clean" and not flags and not show_clean:
                continue
            if detail and detail in name:
                print(f"--- {name}: {', '.join([kind] + flags)}, {len(loop)} instructions, {len(stack)} on the stack")
                print("    " + "\n    ".join(loop))
    if "--functions" in args:
        for sym in sorted(kernels, key=lambda k: describe(k).name):
            name = describe(sym).name
            print(f"  {name[:100]:100s} {per_function.get(name, 0):3d} hot loops")
    # Else an empty table reads as a clean one
    if not hot:
        sys.exit(f"FAIL: no hot loop in {path} under {', '.join(hot_paths)}: the crates' or modules' names have "
                 "drifted from HOT, or COLD leaves them out")
    widths = (7, 8, 7, 6, 6, 7, 8, 6, 5)
    total = {k: sum(c[k] for c in counts.values()) for k in columns}
    print(f"{'':64s} {'round trip':>16s} {'reload':>14s}")
    print(f"{'function':64s} {'gpr':>7s} {'simd':>8s} {'gpr':>7s} {'simd':>6s} "
          + " ".join(f"{k:>{w}s}" for k, w in zip(REVIEW, widths[4:])))
    for name in sorted(counts, key=lambda n: tuple(-counts[n][k] for k in columns) + (n,)):
        c = counts[name]
        print(f"{name[:64]:64s} " + " ".join(f"{c[k]:{w}d}" for k, w in zip(columns, widths)))
    print(f"{'total (' + str(len(counts)) + ' functions)':64s} " + " ".join(f"{total[k]:{w}d}" for k, w in zip(columns, widths)))
    print(f"functions whose hot loops are all clean and unflagged: {len(hot - set(counts))}")
    known = ", ".join(f"{n} {why}" for why, n in sorted(calling.items()) if why not in LIST_ORDER)
    listed = ", ".join(f"{calling[k]} {k}" for k in LIST_ORDER)
    print(f"hot-shaped loops that make a call, so not in the table: {sum(calling.values())} ({known or 'none known'}; "
          f"listed: {listed})")
    for name, length, callees in sorted(to_read):
        print(f"  {name[:64]:64s} {length:4d} instructions, calls {', '.join(callees)}")
    spilling = sum(total[k] for k in KINDS)
    if limit is not None and spilling > limit:
        print(f"FAIL: {spilling} hot loops touch the stack, over {limit}")
        sys.exit(1)


if __name__ == "__main__":
    main()
