fn main() {
    println!("cargo::rustc-env=NF_TARGET_FEATURES={}", std::env::var("CARGO_CFG_TARGET_FEATURE").unwrap_or_default());
    println!("cargo::rerun-if-changed=build.rs");
}
