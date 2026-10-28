//! Whether this CPU can run noise-filter: the build is x86-64-v3 (`.cargo/config.toml`: AVX2, BMI1 and 2, FMA, F16C, LZCNT, MOVBE and
//! the levels below), and a CPU without one of them would fault on an illegal instruction somewhere. So `gate` is the first thing both
//! exes' `main` does, before the panic hook, the log, the console, the single-instance check and the memory pool. A CPU that lacks a
//! feature is told which, on standard error and, from noise-filter.exe, in a message box, and the exe exits with code 5 (`EXIT_CODE`).
//! Ported from TMR (`TMR-APP/src/cpu_check.rs`), without its CLFLUSHOPT, which we don't use.
//!
//! **The check reads CPUID itself**, and XCR0 (XGETBV) for whether the OS saves the AVX registers, which AVX needs as much as the
//! CPU's bit. `is_x86_feature_detected!` won't do: it expands to `cfg!(target_feature = ..) || <the run-time check>`, so for a feature
//! the build enables it is a constant `true`.
//!
//! **Which CPUs reach it.** Only code compiled in this workspace and its dependencies is x86-64-v3's: the standard library's own
//! functions keep the baseline they were built with (fat LTO keeps each function's target CPU), but what is inlined into our code, or
//! a generic instantiated in it, is v3. At v3 every SSE instruction is VEX-encoded too, so on a CPU without AVX, which is most of what
//! the baseline leaves out (Pentium, Celeron and Atom chips), any use of an XMM register faults, not only AVX2's. So the way to the
//! refusal keeps to general-purpose registers:
//! - `main` only calls `gate` and then the real main, which is `#[inline(never)]`: a large function's prologue saves XMM registers with
//!   VEX moves, as TMR's `main` does before its check.
//! - The refusal is built byte by byte in a buffer on the stack (`Text`), with volatile stores, so LLVM can't make the copies vector
//!   moves; and with no heap, which in noise-filter.exe is nf-mem's router (v3 code: the pool paths of its alloc, realloc and dealloc
//!   have TZCNT, LZCNT, SHLX and SHRX, reached only inside an RtScope or for pool memory, so never before the gate), and no `format!`.
//! - It goes out through `GetStdHandle` and `WriteFile`, declared here rather than taken from the `windows` crate, whose `GetStdHandle`
//!   builds an `Error` when there is no handle; and in noise-filter.exe through `MessageBoxA` (main.rs's `cpu_box`). ANSI, for ASCII
//!   text: TMR found that converting the text to UTF-16 is a generic whose size hint's divide by 3 is BMI2's MULX.
//!
//! Checked in both release exes (doc/dev/build-report.md §11): from the C runtime's call to `main` to the box, not one instruction is
//! VEX-encoded or needs BMI, LZCNT or MOVBE (std's own functions on the way use legacy SSE2, which every x86-64 CPU runs). Nothing in
//! the source can promise that, and LLVM decides it, so `tools/check_cpu_gate_asm.py` checks it in the post-LTO asm: run it after
//! changing this file, either `main`, the launcher's `refusal` or the toolchain, and before a release (doc/todo/18-release.md). A
//! debug build isn't covered.

use std::arch::x86_64::{__cpuid_count, _xgetbv};
use std::ffi::{CStr, c_void};
use std::mem::MaybeUninit;

/// The exit code for a CPU that can't run noise-filter (`Exit::UnsupportedCpu`; the launcher's too).
pub const EXIT_CODE: u8 = 5;

/// The CPUID words and XCR0 the check reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Cpuid {
    /// Leaf 1's ECX
    leaf1_ecx: u32,
    /// Leaf 7 (subleaf 0)'s EBX; 0 when the CPU has no leaf 7
    leaf7_ebx: u32,
    /// Leaf 0x8000_0001's ECX; 0 when the CPU has no such leaf
    ext1_ecx: u32,
    /// XCR0, the state components the OS saves; 0 when the OS hasn't turned XSAVE on (leaf 1 ECX bit 27, OSXSAVE), when XGETBV faults
    xcr0: u64,
}

/// Where CPUID gives a feature
#[derive(Debug, Clone, Copy)]
enum At {
    Leaf1Ecx(u32),
    Leaf7Ebx(u32),
    Ext1Ecx(u32),
    /// The OS saves the SSE and AVX registers (XCR0 bits 1 and 2), having turned XSAVE on
    OsAvxState,
}

/// A feature noise-filter is built for: its name in the refusal, where CPUID gives it (the Intel SDM's and AMD APM's CPUID chapters),
/// and in a test build its target feature's name (`rustc --print cfg -C target-cpu=x86-64-v3`), for the test that every one the build
/// enables is here.
struct Feature {
    name: &'static str,
    at: At,
    #[cfg(test)]
    rustc: &'static str,
}

/// A row of `FEATURES`: the refusal's name, the target feature's, where CPUID gives it
macro_rules! feature {
    ($name:literal, $rustc:literal, $at:expr) => {
        Feature {
            name: $name,
            at: $at,
            #[cfg(test)]
            rustc: $rustc,
        }
    };
}

/// x86-64-v3's features above x86-64's own (which every x86-64 CPU has: x87, FXSR, SSE, SSE2), in the levels' order. At most 32: a
/// missing set is a `u32`, bit `i` for row `i`.
#[rustfmt::skip]
const FEATURES: &[Feature] = &[
    // x86-64-v2
    feature!("CMPXCHG16B", "cmpxchg16b", At::Leaf1Ecx(13)),
    feature!("LAHF/SAHF", "lahfsahf", At::Ext1Ecx(0)),
    feature!("POPCNT", "popcnt", At::Leaf1Ecx(23)),
    feature!("SSE3", "sse3", At::Leaf1Ecx(0)),
    feature!("SSSE3", "ssse3", At::Leaf1Ecx(9)),
    feature!("SSE4.1", "sse4.1", At::Leaf1Ecx(19)),
    feature!("SSE4.2", "sse4.2", At::Leaf1Ecx(20)),
    // x86-64-v3
    feature!("AVX", "avx", At::Leaf1Ecx(28)),
    feature!("the OS's AVX support (XSAVE saving the AVX registers)", "", At::OsAvxState),
    feature!("AVX2", "avx2", At::Leaf7Ebx(5)),
    feature!("BMI1", "bmi1", At::Leaf7Ebx(3)),
    feature!("BMI2", "bmi2", At::Leaf7Ebx(8)),
    feature!("F16C", "f16c", At::Leaf1Ecx(29)),
    feature!("FMA", "fma", At::Leaf1Ecx(12)),
    feature!("LZCNT", "lzcnt", At::Ext1Ecx(5)),
    feature!("MOVBE", "movbe", At::Leaf1Ecx(22)),
    feature!("XSAVE", "xsave", At::Leaf1Ecx(26)),
];

const _: () = assert!(FEATURES.len() <= 32, "a missing set is a u32");

/// Leaf 1 ECX's OSXSAVE: the OS has turned XSAVE on, so XGETBV runs
const OSXSAVE: u32 = 1 << 27;
/// XCR0's SSE and AVX state components
const XCR0_SSE_AVX: u64 = 0b110;

/// This CPU's words.
fn read() -> Cpuid {
    // A leaf above the highest the CPU has is asked for only after leaf 0 or 0x8000_0000 says it is there
    let max = __cpuid_count(0, 0).eax;
    let leaf1 = __cpuid_count(1, 0);
    let leaf7_ebx = if max >= 7 { __cpuid_count(7, 0).ebx } else { 0 };
    let max_ext = __cpuid_count(0x8000_0000, 0).eax;
    let ext1_ecx = if max_ext >= 0x8000_0001 { __cpuid_count(0x8000_0001, 0).ecx } else { 0 };
    // SAFETY: XGETBV faults unless the OS has turned XSAVE on, which OSXSAVE says it has
    let xcr0 = if leaf1.ecx & OSXSAVE != 0 { unsafe { _xgetbv(0) } } else { 0 };
    Cpuid { leaf1_ecx: leaf1.ecx, leaf7_ebx, ext1_ecx, xcr0 }
}

/// Whether `cpu` has `at`
fn has(cpu: Cpuid, at: At) -> bool {
    match at {
        At::Leaf1Ecx(bit) => cpu.leaf1_ecx & (1 << bit) != 0,
        At::Leaf7Ebx(bit) => cpu.leaf7_ebx & (1 << bit) != 0,
        At::Ext1Ecx(bit) => cpu.ext1_ecx & (1 << bit) != 0,
        At::OsAvxState => cpu.leaf1_ecx & OSXSAVE != 0 && cpu.xcr0 & XCR0_SSE_AVX == XCR0_SSE_AVX,
    }
}

/// The features `cpu` lacks, of those noise-filter needs: bit `i` for `FEATURES[i]`, 0 when it can run noise-filter
fn missing_in(cpu: Cpuid) -> u32 {
    let mut missing = 0;
    for (i, f) in FEATURES.iter().enumerate() {
        if !has(cpu, f.at) {
            missing |= 1 << i;
        }
    }
    missing
}

/// The CPU gate, first thing in both exes' `main`: `None` when this CPU can run noise-filter. Otherwise the refusal has gone to
/// standard error, when there is one, and to `show` (noise-filter.exe's message box; the launcher's wait for Enter in a console of
/// its own), and this is the exit code to return.
pub fn gate(show: Option<fn(&CStr)>) -> Option<u8> {
    let missing = missing_in(read());
    if missing == 0 {
        return None;
    }
    refuse(missing, show);
    Some(EXIT_CODE)
}

/// Tells the user which of the features this CPU lacks (the module doc says why it is written this way)
#[cold]
#[inline(never)]
fn refuse(missing: u32, show: Option<fn(&CStr)>) {
    let mut text = Text::new();
    write_refusal(missing, &mut text);
    write_stderr(b"error: ");
    write_stderr(text.as_bytes());
    write_stderr(b"\n");
    if let Some(show) = show {
        show(text.as_c_str());
    }
}

/// The refusal for the features `missing`: ASCII, for noise-filter.exe's ANSI box
fn write_refusal(missing: u32, text: &mut Text) {
    text.push("noise-filter can't run on this CPU: it lacks ");
    let mut first = true;
    for (i, f) in FEATURES.iter().enumerate() {
        if missing & (1 << i) != 0 {
            if !first {
                text.push(", ");
            }
            text.push(f.name);
            first = false;
        }
    }
    text.push(
        ".\n\nnoise-filter is built for x86-64-v3 CPUs (AVX2, BMI1/2, FMA): Intel Core and Xeon from Haswell (2013) on, but for the \
         Core i3/i5 L-series (Lakefield), and AMD from Zen (2017) on, with its Excavator APUs (2015-2016). Many Pentium, Celeron and \
         Atom chips don't have it.",
    );
}

/// The room in `Text`, the NUL included: the longest refusal, with every feature missing, takes about 480 bytes
/// (`tests::the_refusal_is_ascii_and_fits`).
const ROOM: usize = 1024;

/// ASCII text, built without the heap or vector instructions (the module doc), and NUL-terminated for `MessageBoxA`.
struct Text {
    bytes: [MaybeUninit<u8>; ROOM],
    len: usize,
}

impl Text {
    fn new() -> Self {
        Self { bytes: [MaybeUninit::uninit(); ROOM], len: 0 }
    }

    /// Appends `s`, as much of it as fits with the NUL. A byte at a time with volatile stores, so that LLVM keeps the copy in
    /// general-purpose registers.
    fn push(&mut self, s: &str) {
        for &b in s.as_bytes() {
            if self.len + 1 >= ROOM {
                return;
            }
            // SAFETY: `len < ROOM - 1`, so the store is inside `bytes`
            unsafe { self.bytes.as_mut_ptr().add(self.len).cast::<u8>().write_volatile(b) };
            self.len += 1;
        }
    }

    fn as_bytes(&self) -> &[u8] {
        // SAFETY: `push` wrote the first `len` bytes
        unsafe { std::slice::from_raw_parts(self.bytes.as_ptr().cast(), self.len) }
    }

    /// The text with a NUL after it. `push` writes no NUL, so the text has none inside.
    fn as_c_str(&mut self) -> &CStr {
        // SAFETY: `len < ROOM` (`push` keeps a byte for the NUL), so the store is inside `bytes`, and the first `len + 1` bytes are then
        // written: `len` without a NUL, then the NUL
        unsafe {
            self.bytes.as_mut_ptr().add(self.len).cast::<u8>().write_volatile(0);
            CStr::from_bytes_with_nul_unchecked(std::slice::from_raw_parts(self.bytes.as_ptr().cast(), self.len + 1))
        }
    }
}

#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    fn GetStdHandle(which: u32) -> *mut c_void;
    fn WriteFile(file: *mut c_void, buffer: *const u8, len: u32, written: *mut u32, overlapped: *mut c_void) -> i32;
}

/// `STD_ERROR_HANDLE`, (DWORD)-12
const STD_ERROR_HANDLE: u32 = -12i32 as u32;
const INVALID_HANDLE_VALUE: *mut c_void = -1isize as *mut c_void;

/// `bytes` on standard error, if there is one: a console, a file or a pipe. A windowed noise-filter.exe has none unless its standard
/// handles were redirected; it hasn't attached to a console yet.
pub(crate) fn write_stderr(bytes: &[u8]) {
    // SAFETY: GetStdHandle takes no pointers. WriteFile gets a handle GetStdHandle returned (not null or invalid), `bytes` and its
    // length, and a live out-pointer, and no OVERLAPPED: standard handles aren't opened for overlapped I/O.
    unsafe {
        let handle = GetStdHandle(STD_ERROR_HANDLE);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return;
        }
        let mut written = 0u32;
        WriteFile(handle, bytes.as_ptr(), bytes.len() as u32, &mut written, std::ptr::null_mut());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A CPU with every feature and the OS's AVX support, as `FEATURES` places them
    fn full() -> Cpuid {
        let mut cpu = Cpuid { xcr0: 0b111, ..Default::default() };
        cpu.leaf1_ecx |= OSXSAVE;
        for f in FEATURES {
            match f.at {
                At::Leaf1Ecx(bit) => cpu.leaf1_ecx |= 1 << bit,
                At::Leaf7Ebx(bit) => cpu.leaf7_ebx |= 1 << bit,
                At::Ext1Ecx(bit) => cpu.ext1_ecx |= 1 << bit,
                At::OsAvxState => {}
            }
        }
        cpu
    }

    /// `cpu` without the features named
    fn without(mut cpu: Cpuid, names: &[&str]) -> Cpuid {
        for f in FEATURES.iter().filter(|f| names.contains(&f.name)) {
            match f.at {
                At::Leaf1Ecx(bit) => cpu.leaf1_ecx &= !(1 << bit),
                At::Leaf7Ebx(bit) => cpu.leaf7_ebx &= !(1 << bit),
                At::Ext1Ecx(bit) => cpu.ext1_ecx &= !(1 << bit),
                At::OsAvxState => cpu.xcr0 = 0b011,
            }
        }
        cpu
    }

    /// The names of the features `cpu` lacks, in `FEATURES`' order
    fn missing_names(cpu: Cpuid) -> Vec<&'static str> {
        let missing = missing_in(cpu);
        FEATURES.iter().enumerate().filter(|(i, _)| missing & (1 << i) != 0).map(|(_, f)| f.name).collect()
    }

    /// The refusal for `missing`, as a string
    fn refusal(missing: u32) -> String {
        let mut text = Text::new();
        write_refusal(missing, &mut text);
        let c = text.as_c_str().to_bytes().to_vec();
        assert_eq!(c, text.as_bytes(), "the NUL ends the text");
        String::from_utf8(c).expect("ASCII")
    }

    /// The machine the tests run on runs x86-64-v3 code, so it has every feature, and the gate lets it through
    #[test]
    fn this_machine_has_every_feature() {
        assert_eq!(missing_names(read()), Vec::<&str>::new(), "{:x?}", read());
        assert_eq!(gate(None), None);
    }

    /// The real words, not a constant: each of the three matches what std's run-time detection says for features the build doesn't
    /// enable, which std reads from CPUID alone (no OS state), as this does
    #[test]
    fn the_words_are_this_cpus() {
        let cpu = read();
        assert_ne!(cpu, Cpuid::default());
        assert_eq!(has(cpu, At::Leaf1Ecx(1)), is_x86_feature_detected!("pclmulqdq"), "leaf 1 ECX");
        assert_eq!(has(cpu, At::Leaf1Ecx(25)), is_x86_feature_detected!("aes"), "leaf 1 ECX");
        assert_eq!(has(cpu, At::Leaf7Ebx(18)), is_x86_feature_detected!("rdseed"), "leaf 7 EBX");
        assert_eq!(has(cpu, At::Leaf7Ebx(19)), is_x86_feature_detected!("adx"), "leaf 7 EBX");
        assert_eq!(has(cpu, At::Ext1Ecx(6)), is_x86_feature_detected!("sse4a"), "leaf 0x8000_0001 ECX");
    }

    /// The CPUID bits as the SDM gives them, independently of the table
    #[test]
    fn the_bits_are_the_sdms() {
        let bits: Vec<(&str, At)> = FEATURES.iter().map(|f| (f.name, f.at)).collect();
        let at = |name: &str| bits.iter().find(|(n, _)| *n == name).map(|&(_, a)| format!("{a:?}")).expect(name);
        for (name, want) in [
            ("CMPXCHG16B", "Leaf1Ecx(13)"),
            ("SSE3", "Leaf1Ecx(0)"),
            ("SSSE3", "Leaf1Ecx(9)"),
            ("SSE4.1", "Leaf1Ecx(19)"),
            ("SSE4.2", "Leaf1Ecx(20)"),
            ("AVX2", "Leaf7Ebx(5)"),
            ("BMI1", "Leaf7Ebx(3)"),
            ("BMI2", "Leaf7Ebx(8)"),
            ("AVX", "Leaf1Ecx(28)"),
            ("FMA", "Leaf1Ecx(12)"),
            ("F16C", "Leaf1Ecx(29)"),
            ("MOVBE", "Leaf1Ecx(22)"),
            ("POPCNT", "Leaf1Ecx(23)"),
            ("XSAVE", "Leaf1Ecx(26)"),
            ("LZCNT", "Ext1Ecx(5)"),
            ("LAHF/SAHF", "Ext1Ecx(0)"),
        ] {
            assert_eq!(at(name), want, "{name}");
        }
        assert_eq!(bits.iter().filter(|(_, a)| !matches!(a, At::OsAvxState)).count(), 16, "a row this test doesn't restate");
    }

    /// The refusal is ASCII whatever is missing, since noise-filter.exe shows it in an ANSI box, and the longest fits in `Text`
    #[test]
    fn the_refusal_is_ascii_and_fits() {
        let all = (1u32 << FEATURES.len()) - 1;
        let text = refusal(all);
        assert!(text.is_ascii());
        assert!(text.len() < ROOM - 1, "{} bytes", text.len());
        for f in FEATURES {
            assert!(text.contains(f.name), "{}", f.name);
        }
        let two = refusal(missing_in(without(full(), &["AVX2", "FMA"])));
        assert!(two.starts_with("noise-filter can't run on this CPU: it lacks AVX2, FMA.\n\n"), "{two}");
    }

    /// A text longer than the room is cut short, with the NUL still inside it
    #[test]
    fn a_long_text_is_cut_short() {
        let mut text = Text::new();
        for _ in 0..ROOM {
            text.push("ab");
        }
        assert_eq!(text.as_bytes().len(), ROOM - 1);
        assert_eq!(text.as_c_str().to_bytes().len(), ROOM - 1);
    }

    /// CPUs noise-filter refuses, by what they lack, and those it runs on
    #[test]
    fn a_cpu_is_refused_for_what_it_lacks() {
        assert_eq!(missing_names(full()), Vec::<&str>::new());
        // A Pentium or Celeron without AVX: none of AVX, AVX2, BMI, FMA or F16C. Real ones differ (a Skylake Pentium sets BMI's bits by
        // an erratum, a Goldmont Atom lacks LZCNT too)
        let pentium = without(full(), &["AVX", "AVX2", "BMI1", "BMI2", "FMA", "F16C"]);
        assert_eq!(missing_names(pentium), ["AVX", "AVX2", "BMI1", "BMI2", "F16C", "FMA"]);
        // Ivy Bridge: AVX and F16C, none of the rest of v3
        let ivy = without(full(), &["AVX2", "BMI1", "BMI2", "FMA", "LZCNT", "MOVBE"]);
        assert_eq!(missing_names(ivy), ["AVX2", "BMI1", "BMI2", "FMA", "LZCNT", "MOVBE"]);
        // Every feature the CPU has, but an OS that doesn't save the AVX registers (XCR0 without bit 2), or hasn't turned XSAVE on
        let os_off = "the OS's AVX support (XSAVE saving the AVX registers)";
        assert_eq!(missing_names(Cpuid { xcr0: 0b011, ..full() }), [os_off]);
        assert_eq!(missing_names(Cpuid { leaf1_ecx: full().leaf1_ecx & !OSXSAVE, ..full() }), [os_off]);
        // A CPU with no leaf 7 or no extended leaf reads them as 0
        let old = Cpuid { leaf7_ebx: 0, ext1_ecx: 0, ..full() };
        assert_eq!(missing_names(old), ["LAHF/SAHF", "AVX2", "BMI1", "BMI2", "LZCNT"]);
    }

    /// Every target feature this build enables, above x86-64's own, is checked (`NF_TARGET_FEATURES`, from build.rs): a baseline moved
    /// up (to x86-64-v4, say) without its features here fails this. The list also holds features that aren't instructions:
    /// `crt-static`, with `-C target-feature=+crt-static` (linking the C runtime statically, doc/todo/18-release.md).
    #[test]
    fn every_feature_the_build_enables_is_checked() {
        const X86_64: [&str; 4] = ["x87", "fxsr", "sse", "sse2"];
        const NOT_ISA: [&str; 1] = ["crt-static"];
        let checked: Vec<&str> = FEATURES.iter().map(|f| f.rustc).filter(|r| !r.is_empty()).collect();
        let enabled: Vec<&str> =
            env!("NF_TARGET_FEATURES").split(',').filter(|f| !f.is_empty() && !X86_64.contains(f) && !NOT_ISA.contains(f)).collect();
        assert!(!enabled.is_empty(), "build.rs gave no target features");
        for f in &enabled {
            assert!(checked.contains(f), "the build enables {f}, which cpu_check doesn't check");
        }
    }
}
