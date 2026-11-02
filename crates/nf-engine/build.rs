//! With the `onnx` feature this crate's test binaries link ONNX Runtime (through nf-ort), whose prebuilt Windows library links
//! DirectML, D3D12 and DXGI: delay-loaded, as nf-ort's build.rs explains, so the tests start without `DirectML.dll` beside
//! them. Without the feature, nothing.

fn main() {
    if std::env::var_os("CARGO_FEATURE_ONNX").is_some() && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        for dll in ["DirectML.dll", "d3d12.dll", "dxgi.dll"] {
            println!("cargo::rustc-link-arg=/DELAYLOAD:{dll}");
        }
        println!("cargo::rustc-link-arg=delayimp.lib");
    }
    println!("cargo::rerun-if-changed=build.rs");
}
