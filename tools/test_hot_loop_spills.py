"""Tests for tools/hot_loop_spills.py: which functions it selects (only audio-thread code), and
which asm it reads by default. Run from any directory:
    python tools/test_hot_loop_spills.py
The symbols are real v0 ones, copied from the bin's asm of 2026-10-26 (the stage interface's of
2026-11-02), except the two generics instantiated with a closure, which are built by hand.
"""
import io
import os
import subprocess
import sys
import tempfile
import time
import unittest
from contextlib import redirect_stderr

sys.dont_write_bytecode = True  # before the import: no __pycache__ in tools/
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import hot_loop_spills as h  # noqa: E402

# On the audio thread
METER = "_RNvMs_NtNtCs8pgegnlmNbx_7nf_core3dsp5meterNtB4_5Meter7measure"
NOISE_STAGE_PROCESS = "_RNvXs0_NtCshJVJJNpKG7l_7nf_core5noiseNtB5_10NoiseStageNtNtB7_5stage5Stage7process"
RNNOISE_PROCESS_HOP = ("_RNvXs0_NtCskiPHVW4EiF1_9nf_stages7rnnoiseNtB5_12RnnoiseModel"
                       "NtNtCshJVJJNpKG7l_7nf_core5noise10NoiseModel11process_hop")
DITHER = "_RNvMNtCs8pgegnlmNbx_7nf_core2rtNtB2_6Dither5apply"
AUDIO_THREAD = ("_RINvNtNtCs4OTV8YV3LlN_3std3sys9backtrace28___rust_begin_short_backtrace"
                "NCNvNtCsnYpoMpMsaZ_9nf_engine5audio5spawn0uEB1d_")
RUN_SLOT_AUDIO = "_RINvNtCs8pgegnlmNbx_7nf_core5chain8run_slotNCNvNtCsnYpoMpMsaZ_9nf_engine5audio3run0E"
# Off it
SPEECH_LIKE = "_RNvNtCsajscNIkuRER_9nf_stages5synth11speech_like"
WITH_ECHO = "_RNvMNtCsajscNIkuRER_9nf_stages5benchNtB2_7Signals9with_echo"
START_LOCKED = "_RNvNtCsnYpoMpMsaZ_9nf_engine7control12start_locked"
LOWER_FIRST = "_RNvNtCsnYpoMpMsaZ_9nf_engine6errors11lower_first"
BUILD_REPORT = "_RNvNtCsnYpoMpMsaZ_9nf_engine12latency_test12build_report"
LATENCY_THREAD = ("_RINvNtNtCs4OTV8YV3LlN_3std3sys9backtrace28___rust_begin_short_backtrace"
                  "NCNvNtCsnYpoMpMsaZ_9nf_engine12latency_test5start0uEB1d_")
CONTROL_THREAD = ("_RINvNtNtCs4OTV8YV3LlN_3std3sys9backtrace28___rust_begin_short_backtrace"
                  "NCNvMNtCsnYpoMpMsaZ_9nf_engine7controlNtB1c_5Inner21ensure_control_thread0uEB1e_")
LISTENER_SHIM = ("_RNSNvYNCNvNtCsnYpoMpMsaZ_9nf_engine7control17register_listener0INtNtNtCsiuJ7SMTLTVY_4core3ops"
                 "8function6FnOnceTNtNtCs4otmnd0BFEd_5alloc6string6StringEE9call_once6vtableBa_")
GUI_SAVER_THREAD = ("_RINvNtNtCs4OTV8YV3LlN_3std3sys9backtrace28___rust_begin_short_backtrace"
                    "NCNvMs4_NtCs5cpTG4XF0eX_6nf_gui8settingsNtB1f_5Saver5spawn0uEB1h_")
RUN_SLOT_GUI = "_RINvNtCs8pgegnlmNbx_7nf_core5chain8run_slotNCNvNtCs5cpTG4XF0eX_6nf_gui4live4tick0E"
RNNOISE_PREPARE = ("_RNvXs0_NtCskiPHVW4EiF1_9nf_stages7rnnoiseNtB5_12RnnoiseModel"
                   "NtNtCshJVJJNpKG7l_7nf_core5noise10NoiseModel7prepare")
HARNESS_RUN_CHAIN = "_RNvNtCskiPHVW4EiF1_9nf_stages7harness9run_chain"
BUILD_CHAIN = "_RNvCskiPHVW4EiF1_9nf_stages11build_chain"
PROFILE_CHECK_CLOSURE = "_RNCNvNtCshJVJJNpKG7l_7nf_core5stage18check_host_profile0B5_"
PROFILE_CHECK_FIND = ("_RINvXs2J_NtNtCsc2SWBamaTzL_4core5slice4iterINtB7_4IterNtNtNtCshJVJJNpKG7l_7nf_core5stage5ports"
                      "13AudioPortInfoENtNtNtNtBb_4iter6traits8iterator8Iterator4findNCNvBU_18check_host_profiles0_0EBW_")


class Selection(unittest.TestCase):
    def test_audio_thread_code_is_selected(self):
        for sym in (METER, NOISE_STAGE_PROCESS, RNNOISE_PROCESS_HOP, DITHER, AUDIO_THREAD, RUN_SLOT_AUDIO):
            self.assertTrue(h.is_hot(sym, h.HOT), h.describe(sym).name)
        self.assertEqual(h.describe(NOISE_STAGE_PROCESS).name, "<nf_core::noise::NoiseStage as nf_core::stage::Stage>::process")

    def test_off_thread_modules_are_left_out(self):
        for sym in (SPEECH_LIKE, WITH_ECHO, START_LOCKED, LOWER_FIRST, BUILD_REPORT, LATENCY_THREAD,
                    CONTROL_THREAD, LISTENER_SHIM, GUI_SAVER_THREAD, RNNOISE_PREPARE, HARNESS_RUN_CHAIN, BUILD_CHAIN,
                    PROFILE_CHECK_CLOSURE, PROFILE_CHECK_FIND):
            self.assertFalse(h.is_hot(sym, h.HOT), h.describe(sym).name)

    def test_a_hot_generic_instantiated_with_a_cold_closure_is_cold(self):
        # Its own path is nf_core::chain, but the GUI's closure says which thread runs it
        self.assertEqual(h.describe(RUN_SLOT_GUI).spine, "nf_core::chain::run_slot")
        self.assertFalse(h.is_hot(RUN_SLOT_GUI, h.HOT))

    def test_paths_inside_a_cold_entry_lift_it(self):
        self.assertTrue(h.is_hot(SPEECH_LIKE, ["nf_stages::synth"]))
        self.assertFalse(h.is_hot(WITH_ECHO, ["nf_stages::synth"]))  # bench stays cold
        self.assertNotIn("nf_engine::control", h.cold_paths(["nf_engine::control::start_locked"]))
        self.assertEqual(h.cold_paths(h.HOT), h.COLD_PATHS)

    def test_a_closure_names_its_module_in_a_generic_argument(self):
        self.assertEqual(h.describe(AUDIO_THREAD).name,
                         "std::sys::backtrace::__rust_begin_short_backtrace::<audio::spawn::{closure#0}, ()>")
        self.assertEqual(h.describe(GUI_SAVER_THREAD).name,
                         "std::sys::backtrace::__rust_begin_short_backtrace::<Saver::spawn::{closure#0}, ()>")


def asm(path, main=True, body=""):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", newline="\n") as f:
        f.write("\t.text\n" + body + ("main:\n\tretq\n" if main else "") + "\t.globl\t_fltused\n")
    return path


def age(path, seconds_ago):
    t = time.time() - seconds_ago
    os.utime(path, (t, t))


class DefaultAsm(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = self.tmp.name

    def tearDown(self):
        self.tmp.cleanup()

    def at(self, rel):
        return os.path.normpath(os.path.join(self.root, rel))

    def pick(self, args=()):
        err = io.StringIO()
        with redirect_stderr(err):
            path = h.binary_asm(list(args), self.root)
        return path, err.getvalue()

    def test_newest_bin_asm_with_main_in_any_target_dir(self):
        old = asm(self.at("target/release/build/nf-app/aaa/out/noise_filter.s"))
        new = asm(self.at("target/fix-tools/release/build/nf-app/bbb/out/noise_filter.s"))
        age(old, 300)
        age(new, 200)
        self.assertEqual(self.pick(), (new, ""))

    def test_passes_over_newer_asm_without_main_and_libs(self):
        good = asm(self.at("target/review-perf/release/build/nf-app/aaa/out/noise_filter.s"))
        partial = asm(self.at("target/fix-app/release/build/nf-app/bbb/out/noise_filter.s"), main=False)
        lib = asm(self.at("target/agent-core/release/build/nf-core/ccc/out/nf_core-ccc.s"), main=False)
        old_lib = asm(self.at("target/release/build/nf-stages/ddd/out/nf_stages-ddd.s"), main=False)
        age(old_lib, 400)
        age(good, 300)
        age(lib, 200)
        age(partial, 100)
        path, notes = self.pick()
        self.assertEqual(path, good)
        self.assertIn(partial, notes)
        self.assertIn("1 newer lib asm", notes)
        self.assertIn(lib, notes)
        self.assertNotIn(old_lib, notes)

    def test_old_layout_counts(self):
        dep = asm(self.at("target/x/release/deps/noise_filter-0123.s"))
        self.assertEqual(self.pick()[0], dep)

    def test_failures(self):
        with self.assertRaises(SystemExit) as e:
            self.pick()
        self.assertIn("no asm under", str(e.exception.code))
        lib = asm(self.at("target/release/build/nf-core/ccc/out/nf_core-ccc.s"), main=False)
        with self.assertRaises(SystemExit) as e:
            self.pick()
        self.assertIn("only libs' asm", str(e.exception.code))
        asm(self.at("target/release/build/nf-app/aaa/out/noise_filter.s"), main=False)
        with self.assertRaises(SystemExit) as e:
            self.pick()
        self.assertIn("no bin asm under", str(e.exception.code))
        with self.assertRaises(SystemExit) as e:  # named on the command line: refused
            self.pick([lib])
        self.assertIn("isn't the binary's asm", str(e.exception.code))

    def test_main_found_across_chunk_boundaries(self):
        path = asm(self.at("a.s"), body="x" * 37 + "\n")
        for size in range(1, 12):
            self.assertTrue(h.has_main(path, size), size)
        self.assertFalse(h.has_main(asm(self.at("b.s"), main=False, body="domain:\n_main:\n"), 3))
        with open(self.at("c.s"), "w") as f:
            f.write("main:\n")
        self.assertTrue(h.has_main(self.at("c.s"), 2))


# A loop over memory through an index register: flagged `indexed`, so it shows in the table
LOOP = """{sym}:
.Lfunc_begin{n}:
.LBB{n}_1:
\tmovups\t(%rcx,%rax,4), %xmm0
\tmulps\t%xmm0, %xmm0
\taddps\t%xmm0, %xmm1
\taddq\t$4, %rax
\tcmpq\t$480, %rax
\tjne\t.LBB{n}_1
\tretq
.Lfunc_end{n}:
"""


class EndToEnd(unittest.TestCase):
    def test_only_audio_thread_functions_reach_the_table(self):
        with tempfile.TemporaryDirectory() as tmp:
            syms = [METER, SPEECH_LIKE, START_LOCKED, BUILD_REPORT, AUDIO_THREAD, LATENCY_THREAD, RUN_SLOT_GUI]
            path = asm(os.path.join(tmp, "noise_filter.s"),
                       body="".join(LOOP.format(sym=s, n=i) for i, s in enumerate(syms)))
            out = subprocess.run([sys.executable, "-I", os.path.join(HERE, "hot_loop_spills.py"), path],
                                 capture_output=True, text=True, check=True).stdout
        self.assertIn("nf_core::dsp::meter::Meter::measure", out)
        self.assertIn("__rust_begin_short_backtrace::<audio::spawn", out)  # the table cuts names at 64
        self.assertIn("total (2 functions)", out)
        for name in ("speech_like", "start_locked", "build_report", "latency_test", "start::{closure", "live::tick"):
            self.assertNotIn(name, out)


if __name__ == "__main__":
    unittest.main()
