"""Whether what a CPU runs before the CPU gate refuses it keeps to the target's baseline, read from each gated exe's post-LTO
asm. crates/nf-app/src/cpu_check.rs's module doc asks for this check after a change to it, to a gated `main` or to the
toolchain, and doc/todo/18-release.md makes it a release step (doc/dev/build-report.md section 11).

Why. The build is x86-64-v3, so a CPU without AVX faults on the first VEX instruction, which at v3 includes every SSE
instruction our code has, and one without BMI1/2, LZCNT or MOVBE on theirs (TZCNT and LZCNT don't fault there: they run
as BSF and BSR and give another answer). The gate is there to refuse such a CPU with a message, so nothing on the way to
it may use them. Which instructions that path has is LLVM's choice, not the source's: a toolchain update, or a small
change such as a zeroed array, can put a VEX store in `main`, and the only sign would be a silent crash on exactly the
CPUs the gate is for.

What it walks. From `main` (rustc's C entry point, with std's runtime start-up inlined into it) and anything in a
`.CRT$X*` section (the C runtime's initialisers and TLS callbacks, which run before `main`), every symbol referenced:
calls, tail jumps, addresses taken (`leaq f(%rip)`: a function pointer, such as the gate's `show`), and what a data
object holds (a vtable, a static function pointer such as std's `SetThreadDescription::PTR`, the unwind tables that name
catch funclets). It stops at
- the code after the gate (`STOP`): `noise_filter::app_main` and `noise_filter_cli::launch_and_exit` run only once the
  gate has let the CPU through;
- panics (`hot_loop_spills.PANIC`): nothing before the gate panics, and a CPU on such a path would fault either way.
The walk follows every reference, not only the branches a refused CPU takes, so it reaches code that can't run before
the gate. `ALLOWED` names the functions whose flagged instructions are only on such branches, each with why.

What it flags, in each function reached:
- any VEX or EVEX instruction (a `v...` mnemonic: AVX, AVX2, FMA, F16C, and at v3 every SSE instruction), and AVX-512's
  mask instructions;
- BMI1 and BMI2 (ANDN, BEXTR, BLSI, BLSMSK, BLSR, BZHI, MULX, PDEP, PEXT, RORX, SARX, SHLX, SHRX, TZCNT), LZCNT, MOVBE,
  and XSAVE's instructions;
- the rest of x86-64-v2, since the gate refuses a CPU without it too: POPCNT, SSE4.2's CRC32, and the legacy (non-VEX)
  SSSE3 and SSE4 instructions.
The target's own baseline passes: `x86_64-pc-windows-msvc` is x86-64 (SSE, SSE2) plus CMPXCHG16B, LAHF/SAHF and SSE3
(`rustc -Z unstable-options --print target-spec-json`). std's own functions are built for it and keep it under our LTO
(`movaps %xmm6, ...` there is legacy SSE), and every CPU that 64-bit Windows 8.1 or later runs on has it: Windows needs
CMPXCHG16B and LAHF/SAHF, and every CPU with both has SSE3. XGETBV passes too: the gate runs it only once CPUID's OSXSAVE
says it may. Code outside the asm (the C runtime's `memcpy`, Windows' DLLs) is listed, not checked.

Build the asm (the exes are linked as usual, next to it), then run this from any directory:
    cargo rustc --release -p nf-app --bin noise-filter -- --emit asm
    cargo rustc --release -p nf-app --bin noise-filter-cli -- --emit asm
    cargo rustc --release -p llcable-mixer --bin llcable-mixer -- --emit asm
    python tools/check_cpu_gate_asm.py [noise_filter.s noise_filter_cli.s llcable_mixer.s] [--list]
With no files it reads the newest asm of each executable that has a `main`, from any target dir
(`target/[NAME/]release/build/<package>/<hash>/out/`). It exits 1 on a flagged instruction that `ALLOWED` doesn't cover, and
when the walk no longer finds the gate's refusal or the code after the gate (the names have drifted). `--list` prints every
function reached, with its instruction count. Its tests: python tools/test_check_cpu_gate_asm.py
"""
import glob
import os
import re
import sys

sys.dont_write_bytecode = True  # before the import: no __pycache__ in tools/
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from hot_loop_spills import PANIC, describe, has_main  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

EXES = {
    "noise_filter.s": "cargo rustc --release -p nf-app --bin noise-filter -- --emit asm",
    "noise_filter_cli.s": "cargo rustc --release -p nf-app --bin noise-filter-cli -- --emit asm",
    "llcable_mixer.s": "cargo rustc --release -p llcable-mixer --bin llcable-mixer -- --emit asm",
}
# Where the build leaves each bin's asm, relative to the repo: Cargo's build-dir layout, then the old one, each in the
# default target dir or another (CARGO_TARGET_DIR=target/NAME)
ASM_DIRS = [f"{target}/release/build/{package}/*/out"
            for target in ("target", "target/*") for package in ("nf-app", "llcable-mixer")]
OLD_ASM = ["target/release/deps", "target/*/release/deps"]

# The code after the gate, where the walk stops, by demangled name
STOP = {"noise_filter::app_main", "noise_filter_cli::launch_and_exit", "llcable_mixer::app_main"}
# The gate's refusal: the walk must reach it, or it isn't walking the gate's path
REFUSE = "cpu_check::refuse"

# Functions the walk reaches whose flagged instructions can't run before the gate, by demangled name: (pattern, why).
# nf-mem's router is noise-filter.exe's global allocator. Its pool paths (rlsf's bitmaps: TZCNT, LZCNT, SHLX, SHRX) run only
# on a thread inside an RtScope (`in_scope()`) or for memory in the pool (`owns(p)`), and neither exists until app_main's
# setup_memory installs the pool; its system-heap path is clean. The walk gets there through std's clean-up, which frees
# stdout's buffer if stdout was used, and std's error formatting. Nothing before the gate uses either, or allocates at all.
# The allowance is per function, so it can't tell a pool branch from the heap path: keep it to these.
ALLOWED = [(re.compile(p), why) for p, why in [
    (r"^__rustc::__rust_(alloc|alloc_zeroed|realloc|dealloc)$",
     "the global allocator's entry points, into which LLVM may inline the router's pool paths"),
    (r"^nf_mem::router::Router::(pool_alloc|pool_free|pool_realloc)$", "the router's pool paths"),
    (r"^<nf_mem::router::Router as core::alloc::global::GlobalAlloc>::(alloc|alloc_zeroed|realloc|dealloc)$",
     "the router's pool branches, behind in_scope() or owns(p)"),
    (r"^<?rlsf::", "rlsf, the pool's allocator"),
]]

# Instruction prefixes the asm writes before a mnemonic
MNEMONIC = re.compile(r"\t(?:(?:lock|rep|repe|repz|repne|repnz|rex64|notrack|data16|addr32)\s+)*([a-z][a-z0-9]*)")
# BMI1 and BMI2, LZCNT, MOVBE, with AT&T's operand-size suffix
BMI = re.compile(r"(andn|bextr|blsi|blsmsk|blsr|bzhi|mulx|pdep|pext|rorx|sarx|shlx|shrx|tzcnt)[wlq]?")
LZCNT_MOVBE = re.compile(r"(lzcnt|movbe)[wlq]?")
XSAVE = re.compile(r"(xsave|xsaveopt|xsavec|xsaves|xrstor|xrstors)(64)?")
KMASK = re.compile(r"k(mov|and|andn|or|xor|xnor|not|test|ortest|shiftl|shiftr|add|unpck)[bwdq]+")
# x86-64-v2 above the target's baseline (which has SSE3, CMPXCHG16B and LAHF/SAHF): POPCNT, SSE4.2's CRC32, then the legacy
# (non-VEX) SSSE3 and SSE4.1/4.2 instructions
V2_OTHER = re.compile(r"popcnt[wlq]?|crc32[bwlq]")
V2_SSE = set("""pabsb pabsw pabsd palignr phaddw phaddd phaddsw phsubw phsubd phsubsw pmaddubsw pmulhrsw pshufb psignb psignw psignd
    blendpd blendps blendvpd blendvps dppd dpps extractps insertps movntdqa mpsadbw packusdw pblendvb pblendw pcmpeqq
    pextrb pextrd pextrq phminposuw pinsrb pinsrd pinsrq pmaxsb pmaxsd pmaxud pmaxuw pminsb pminsd pminud pminuw
    pmuldq pmulld ptest roundpd roundps roundsd roundss pcmpestri pcmpestrm pcmpistri pcmpistrm pcmpgtq""".split())
PMOVX = re.compile(r"pmov[sz]x(bw|bd|bq|wd|wq|dq)")
# Mnemonics that start with `v` but aren't VEX (and won't be in user code)
NOT_VEX = {"verr", "verw"}

# Directives whose operands can name a symbol
DATA = ("\t.quad", "\t.long", "\t.rva", "\t.secrel32")
RELOC = re.compile(r"@(?:IMGREL|SECREL32|SECTION|PLT|GOTPCREL)\b")
TOKEN = re.compile(r'"[^"]+"|(?<![%\w$?@.])(?:[A-Za-z_?]|\$[A-Za-z_])[\w$?@.]*')
LABEL = re.compile(r'("[^"]+"|[^\s"#][^\s:]*):')
IMMEDIATE = re.compile(r"\$[A-Za-z_]")


def flagged(mnemonic):
    """Why an instruction needs more than the target's baseline, or None."""
    m = mnemonic
    if m.startswith("v") and m not in NOT_VEX:
        return "VEX"
    if BMI.fullmatch(m):
        return "BMI"
    if LZCNT_MOVBE.fullmatch(m):
        return m[:5].upper()
    if XSAVE.fullmatch(m):
        return "XSAVE"
    if KMASK.fullmatch(m):
        return "AVX-512 mask"
    if m in V2_SSE or PMOVX.fullmatch(m) or V2_OTHER.fullmatch(m):
        return "x86-64-v2"
    return None


def tokens(text):
    """The names in an operand list that can be symbols (the caller keeps those the asm defines)."""
    return set(TOKEN.findall(RELOC.sub("", text)))


class Node:
    """A symbol of the asm: a function or a data object."""
    __slots__ = ("line", "instructions", "flags", "refs")

    def __init__(self, line):
        self.line, self.instructions, self.flags, self.refs = line, 0, [], set()


def parse(lines):
    """The asm's symbols, by name, and the names the `.CRT$X*` sections hold. A symbol runs from its label to the next
    non-local one; `.L` labels are inside it."""
    nodes, crt = {}, set()
    cur, section = None, ""
    for n, line in enumerate(lines, 1):
        if line.startswith("\t"):
            if line.startswith("\t."):
                if line.startswith("\t.section"):
                    parts = line.split(None, 1)
                    section = parts[1].split(",", 1)[0] if len(parts) > 1 else ""
                elif line.startswith(DATA):
                    parts = line.split(None, 1)
                    refs = tokens(parts[1]) if len(parts) > 1 else set()
                    if section.startswith(".CRT$X"):
                        crt |= refs
                    if cur is not None:
                        cur.refs |= refs
                continue
            if cur is None:
                continue
            m = MNEMONIC.match(line)
            if not m:
                continue
            cur.instructions += 1
            why = flagged(m.group(1))
            if why:
                cur.flags.append((n, line.strip(), why))
            rest = line[m.end():]
            # A symbol in an instruction: RIP-relative, a relocation (`@SECREL32`), a branch target, or an immediate
            if "(%rip)" in rest or "@" in rest or m.group(1).startswith(("call", "j")) or IMMEDIATE.search(rest):
                cur.refs |= tokens(rest)
        elif line.startswith(".L") or not line.strip() or line.startswith("#"):
            continue
        elif m := LABEL.match(line):
            name = m.group(1)
            cur = nodes.setdefault(name, Node(n))
    return nodes, crt


def display(sym):
    d = describe(sym.strip('"'))
    return d.name if d else sym


def walk(nodes, roots):
    """What the CPU can run from `roots` until the gate has let it through: the symbols reached, in the order found, the
    STOP and panic symbols the walk stopped at, and the names reached that the asm doesn't define."""
    seen, order, stopped, outside = set(), [], set(), set()
    todo = list(roots)
    while todo:
        sym = todo.pop()
        if sym in seen:
            continue
        seen.add(sym)
        if sym not in nodes:
            outside.add(sym)
            continue
        name = display(sym)
        if name in STOP or PANIC.search(name):
            stopped.add(name)
            continue
        order.append(sym)
        todo.extend(sorted(nodes[sym].refs, reverse=True))
    return order, stopped, outside


def allowed(name):
    return next((why for pattern, why in ALLOWED if pattern.search(name)), None)


def check(path, out=sys.stdout, list_all=False):
    """Checks one exe's asm file; returns the number of failures."""
    with open(path, encoding="utf-8", errors="replace") as f:
        return check_lines(path, (line.rstrip("\n") for line in f), out, list_all)


def check_lines(path, lines, out=sys.stdout, list_all=False):
    """Checks one exe's asm, as lines; returns the number of failures."""
    nodes, crt = parse(lines)
    if "main" not in nodes:
        print(f"FAIL: {path} has no `main`: not a bin's post-LTO asm", file=out)
        return 1
    order, stopped, outside = walk(nodes, ["main"] + sorted(crt))
    code = [s for s in order if nodes[s].instructions]
    names = {s: display(s) for s in order}
    failures = 0
    print(f"asm: {path}", file=out)
    print(f"  reached {len(code)} functions ({sum(nodes[s].instructions for s in code)} instructions) and "
          f"{len(order) - len(code)} data objects from main" + (f" and {len(crt)} .CRT$X* entries" if crt else "")
          + f"; stopped at {len(stopped)} (" + ", ".join(sorted(n for n in stopped if n in STOP)) + " and panics)", file=out)
    if not stopped & STOP:
        print(f"  FAIL: the walk never reached {' or '.join(sorted(STOP))}: has the code after the gate been renamed?",
              file=out)
        failures += 1
    if not any(names[s].endswith(REFUSE) for s in code):
        print(f"  FAIL: the walk never reached {REFUSE}: has the gate moved, or been renamed?", file=out)
        failures += 1
    if list_all:
        for s in sorted(code, key=lambda s: names[s]):
            print(f"    {nodes[s].instructions:6d}  {len(nodes[s].flags):3d} flagged  {names[s][:150]}", file=out)
    for s in sorted(code, key=lambda s: names[s]):
        node = nodes[s]
        if not node.flags:
            continue
        why = allowed(names[s])
        kinds = sorted({k for _, _, k in node.flags})
        if why:
            print(f"  allowed: {names[s][:110]}: {len(node.flags)} ({', '.join(kinds)}): {why}", file=out)
            continue
        failures += 1
        print(f"  FAIL: {names[s]} ({s}, line {node.line}): {len(node.flags)} flagged ({', '.join(kinds)})", file=out)
        for n, text, kind in node.flags[:8]:
            print(f"    line {n}: {text}   [{kind}]", file=out)
    imports = sorted(o[len("__imp_"):] for o in outside if o.startswith("__imp_"))
    other = sorted(o for o in outside if not o.startswith("__imp_"))
    print(f"  Windows imports reached: {', '.join(imports) or 'none'}", file=out)
    print(f"  outside the asm, not checked: {', '.join(other) or 'none'}", file=out)
    print("  " + ("PASS" if not failures else f"FAIL: {failures}"), file=out)
    return failures


def newest(name, root=ROOT):
    """The newest asm of this name that has a `main`, from any target dir, or None."""
    stem = name[:-2]
    found = [f for d in ASM_DIRS for f in glob.glob(os.path.join(root, d, name))]
    found += [f for d in OLD_ASM for f in glob.glob(os.path.join(root, d, f"{stem}-*.s"))]
    for f in sorted(found, key=os.path.getmtime, reverse=True):
        if has_main(f):
            return f
    return None


def warn_if_stale(path, root=ROOT, out=sys.stderr):
    built = os.path.getmtime(path)
    newer = [f for f in glob.glob(os.path.join(root, "crates", "*", "src", "**", "*.rs"), recursive=True)
             if os.path.getmtime(f) > built]
    if newer:
        newest_src = max(newer, key=os.path.getmtime)
        print(f"warning: {len(newer)} source file(s) changed after {path} was built (newest "
              f"{os.path.relpath(newest_src, root)}): rebuild it", file=out)


def main():
    args = sys.argv[1:]
    files = [a for a in args if a.endswith(".s")]
    if not files:
        for name, build in EXES.items():
            path = newest(name)
            if path is None:
                sys.exit(f"FAIL: no {name} with a `main` under {os.path.join(ROOT, 'target')}: build it with `{build}`")
            files.append(path)
    failures = 0
    for path in files:
        warn_if_stale(path)
        failures += check(path, list_all="--list" in args)
    if failures:
        sys.exit(1)


if __name__ == "__main__":
    main()
