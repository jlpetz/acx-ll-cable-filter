//! pyke's prebuilt ONNX Runtime for Windows always contains the DirectML execution provider, so ort-sys links `DirectML`,
//! `D3D12`, `DXGI` and `DXCORE` (ort-sys `build/static_link/mod.rs`). We never register that provider: delay-loading them
//! keeps the DLLs out of the import table's load-time list, so a test binary or the app starts without `DirectML.dll` next
//! to it (we don't copy it: no `copy-dylibs`), and only a provider we never add would load them.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        // dxcore.lib is linked too, but nothing in ONNX Runtime 1.31 imports from it (LNK4199 if delay-loaded).
        for dll in ["DirectML.dll", "d3d12.dll", "dxgi.dll"] {
            println!("cargo::rustc-link-arg=/DELAYLOAD:{dll}");
        }
        // A link argument rather than a link-lib: it reaches every binary this package links (its tests), and the helper
        // the delay-load stubs call (`__delayLoadHelper2`) is in delayimp.lib.
        println!("cargo::rustc-link-arg=delayimp.lib");
    }
    println!("cargo::rerun-if-changed=build.rs");
}
