//! The target features the build enables (`NF_TARGET_FEATURES`), for `cpu_check`'s test that it checks each. As TMR's build.rs.
//!
//! With the `onnx` feature (TODO 5) the exes link ONNX Runtime, whose prebuilt Windows library links DirectML, D3D12 and
//! DXGI: delay-loaded (nf-ort's build.rs says why), so the exe starts without `DirectML.dll` beside it and never loads them
//! (no DirectML provider is registered).

fn main() {
    println!("cargo::rustc-env=NF_TARGET_FEATURES={}", std::env::var("CARGO_CFG_TARGET_FEATURE").unwrap_or_default());
    if std::env::var_os("CARGO_FEATURE_ONNX").is_some() && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        // The app only: the launcher (noise-filter-cli) links no ONNX Runtime, and the flags would warn there (LNK4199).
        for arg in ["/DELAYLOAD:DirectML.dll", "/DELAYLOAD:d3d12.dll", "/DELAYLOAD:dxgi.dll", "delayimp.lib"] {
            println!("cargo::rustc-link-arg-bin=noise-filter={arg}");
        }
    }
    // The features follow the rustflags, and a change there rebuilds and reruns this anyway
    println!("cargo::rerun-if-changed=build.rs");
}
