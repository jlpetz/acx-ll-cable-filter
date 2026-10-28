"""Tests for tools/check_cpu_gate_asm.py: what it flags, how it walks from `main` (calls, data, the C runtime's
initialisers, funclets), where it stops, and which asm it reads by default. Run from any directory:
    python tools/test_check_cpu_gate_asm.py
The asm is synthetic, in the shape of the bins' post-LTO asm; the symbols are real v0 ones from noise_filter.s of
2026-10-28.
"""
import io
import os
import sys
import tempfile
import time
import unittest

sys.dont_write_bytecode = True  # before the import: no __pycache__ in tools/
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import check_cpu_gate_asm as c  # noqa: E402

OUR_MAIN = "_RNvCskE2adrfNma9_12noise_filter4main"
APP_MAIN = "_RNvCskE2adrfNma9_12noise_filter8app_main"
REFUSE = "_RNvNtCskE2adrfNma9_12noise_filter9cpu_check6refuse"
PANIC_FMT = "_RNvNtCsiuJ7SMTLTVY_4core9panicking9panic_fmt"
POOL_ALLOC = "_RNvMs2_NtCshvR3Jm0FhB9_6nf_mem6routerNtB5_6Router10pool_alloc"
SHOW = "_RNvCskE2adrfNma9_12noise_filter7cpu_box"


def func(name, *body):
    return [f"\t.def\t{name};", "\t.scl\t2;", "\t.type\t32;", "\t.endef",
            f'\t.section\t.text,"xr",one_only,{name}', f"{name}:", ".Lfunc_begin0:", f".seh_proc {name}",
            *("\t" + b for b in body), ".Lfunc_end0:", "\t.seh_endproc", ""]


def data(name, *body, section=".rdata"):
    return [f'\t.section\t{section},"dr",one_only,{name}', "\t.p2align\t3, 0x0", f"{name}:", *("\t" + b for b in body), ""]


def program(main=(), our_main=(), refuse=(), app_main=(), extra=()):
    """rustc's `main` calling ours, which runs the gate and tail-jumps to app_main, as in the real asm"""
    return [
        *func("main", "pushq\t%rbp", *main, f"leaq\t{OUR_MAIN}(%rip), %rcx", "callq\t*%rcx", "xorl\t%eax, %eax",
              "popq\t%rbp", "retq"),
        *func(OUR_MAIN, "cpuid", *our_main, "testl\t%eax, %eax", "je\t.LBB1_2", f"leaq\t{SHOW}(%rip), %rdx",
              f"callq\t{REFUSE}", "movb\t$5, %al", "retq", f"jmp\t{APP_MAIN}"),
        *func(REFUSE, "subq\t$1064, %rsp", *refuse, "callq\t*__imp_GetStdHandle(%rip)", "callq\t*__imp_WriteFile(%rip)",
              "callq\t*%rdx", "retq"),
        *func(SHOW, "callq\t*__imp_MessageBoxA(%rip)", "retq"),
        *func(APP_MAIN, "vmovaps\t%xmm6, 16(%rsp)", "tzcntl\t%eax, %ecx", *app_main, "retq"),
        *extra,
    ]


def run(lines):
    out = io.StringIO()
    failures = c.check_lines("test.s", lines, out)
    return failures, out.getvalue()


class Flags(unittest.TestCase):
    def test_what_needs_more_than_the_targets_baseline(self):
        for m, why in [("vmovaps", "VEX"), ("vpxor", "VEX"), ("vfmadd231ps", "VEX"), ("vcvtph2ps", "VEX"),
                       ("vzeroupper", "VEX"), ("shlxl", "BMI"), ("sarxq", "BMI"), ("mulxq", "BMI"), ("andnl", "BMI"),
                       ("tzcntl", "BMI"), ("blsrq", "BMI"), ("pextq", "BMI"), ("lzcntq", "LZCNT"), ("movbel", "MOVBE"),
                       ("xsaveopt64", "XSAVE"), ("kmovw", "AVX-512 mask"), ("popcntl", "x86-64-v2"),
                       ("crc32b", "x86-64-v2"), ("pshufb", "x86-64-v2"), ("pminud", "x86-64-v2"), ("pextrq", "x86-64-v2"),
                       ("pmovzxbw", "x86-64-v2"), ("roundss", "x86-64-v2"), ("ptest", "x86-64-v2")]:
            self.assertEqual(c.flagged(m), why, m)

    def test_the_targets_baseline_passes(self):
        # x86-64's SSE and SSE2, and the windows-msvc target's SSE3, CMPXCHG16B and LAHF/SAHF; BSF and BSR; XGETBV
        for m in ["movaps", "movups", "pxor", "pextrw", "pinsrw", "cvtsi2ssl", "addps", "movddup", "haddps", "lddqu",
                  "cmpxchg16b", "lahf", "sahf", "bsfl", "bsrq", "xgetbv", "cpuid", "verw", "movq", "callq", "jmp"]:
            self.assertIsNone(c.flagged(m), m)

    def test_prefixes_are_skipped(self):
        self.assertEqual(c.MNEMONIC.match("\tlock\t\tcmpxchgq\t%rdx, (%rcx)").group(1), "cmpxchgq")
        self.assertEqual(c.MNEMONIC.match("\trep\t\tbsfl\t%eax, %ecx").group(1), "bsfl")
        self.assertEqual(c.MNEMONIC.match("\trex64 jmpq\t*__imp_HeapFree(%rip)").group(1), "jmpq")


class Walk(unittest.TestCase):
    def test_a_clean_gate_passes(self):
        failures, out = run(program())
        self.assertEqual(failures, 0, out)
        self.assertIn("PASS", out)
        self.assertIn("Windows imports reached: GetStdHandle, MessageBoxA, WriteFile", out)
        self.assertIn("noise_filter::app_main", out)

    def test_a_vex_store_before_the_gate_fails(self):
        for where in ["main", "our_main", "refuse"]:
            failures, out = run(program(**{where: ["vmovaps\t%xmm6, 16(%rsp)"]}))
            self.assertEqual(failures, 1, (where, out))
            self.assertIn("vmovaps\t%xmm6, 16(%rsp)   [VEX]", out)

    def test_bmi_after_the_gate_or_in_a_panic_passes(self):
        panic = func(PANIC_FMT, "shlxl\t%eax, %ecx, %edx", "ud2")
        failures, out = run(program(refuse=[f"callq\t{PANIC_FMT}"], app_main=["shlxl\t%eax, %ecx, %edx"], extra=panic))
        self.assertEqual(failures, 0, out)

    def test_a_function_pointer_in_data_is_followed(self):
        hidden = "_RNvCskE2adrfNma9_12noise_filter6hidden"
        lines = program(main=["movq\tanon.abc.1(%rip), %r8", "callq\t*%r8"],
                        extra=[*data("anon.abc.1", f".quad\t{hidden}", section=".data"),
                               *func(hidden, "shlxl\t%eax, %ecx, %edx", "retq")])
        failures, out = run(lines)
        self.assertEqual(failures, 1, out)
        self.assertIn("noise_filter::hidden", out)

    def test_unwind_tables_lead_to_the_catch_funclets(self):
        funclet = '"?catch$10@?0?main@4HA"'
        lines = program(main=["nop", ".seh_handlerdata", ".long\t$cppxdata$main@IMGREL"],
                        extra=[*data("$cppxdata$main", ".long\t429065506", f".long\t{funclet}@IMGREL", section=".xdata"),
                               f"{funclet}:", "\tvmovups\t%xmm0, (%rsp)", "\tretq"])
        failures, out = run(lines)
        self.assertEqual(failures, 1, out)
        self.assertIn(funclet, out)

    def test_the_c_runtimes_initialisers_are_roots(self):
        init = "_RNvCskE2adrfNma9_12noise_filter4init"
        lines = program(extra=['\t.section\t.CRT$XCU,"dr"', f"\t.quad\t{init}", *func(init, "vpxor\t%xmm0, %xmm0, %xmm0")])
        failures, out = run(lines)
        self.assertEqual(failures, 1, out)
        self.assertIn("1 .CRT$X* entries", out)

    def test_the_pool_is_allowed(self):
        lines = program(refuse=[f"callq\t{POOL_ALLOC}"], extra=func(POOL_ALLOC, "lzcntq\t%r8, %rax", "retq"))
        failures, out = run(lines)
        self.assertEqual(failures, 0, out)
        self.assertIn("allowed: nf_mem::router::Router::pool_alloc", out)

    def test_drifted_names_fail(self):
        no_app_main = [line.replace(APP_MAIN, "_RNvCskE2adrfNma9_12noise_filter9real_main") for line in program()]
        failures, out = run(no_app_main)
        self.assertEqual(failures, 2, out)  # and the walk goes on into the renamed function's v3 code
        self.assertIn("never reached noise_filter::app_main", out)
        self.assertIn("FAIL: noise_filter::real_main", out)
        no_refuse = [line.replace(REFUSE, "_RNvNtCskE2adrfNma9_12noise_filter9cpu_check4tell") for line in program()]
        failures, out = run(no_refuse)
        self.assertEqual(failures, 1, out)
        self.assertIn("never reached cpu_check::refuse", out)
        failures, out = run([line for line in program() if not line.startswith("main:")])
        self.assertEqual(failures, 1, out)
        self.assertIn("has no `main`", out)


class DefaultAsm(unittest.TestCase):
    def test_the_newest_with_a_main(self):
        with tempfile.TemporaryDirectory() as root:
            def put(target, hash_, text, age):
                d = os.path.join(root, "target", target, "release", "build", "nf-app", hash_, "out")
                os.makedirs(d, exist_ok=True)
                path = os.path.join(d, "noise_filter.s")
                with open(path, "w") as f:
                    f.write(text)
                t = time.time() - age
                os.utime(path, (t, t))
                return path
            old = put("a", "h1", "\t.text\nmain:\n\tretq\n", 100)
            put("b", "h2", "\t.text\n", 10)  # newer, but cut short before `main`
            self.assertEqual(os.path.normpath(c.newest("noise_filter.s", root)), os.path.normpath(old))
            self.assertIsNone(c.newest("noise_filter_cli.s", root))


if __name__ == "__main__":
    unittest.main()
