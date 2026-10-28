//! The target features the build enables (`NF_TARGET_FEATURES`), for `cpu_check`'s test that it checks each. As TMR's build.rs.

fn main() {
    println!("cargo::rustc-env=NF_TARGET_FEATURES={}", std::env::var("CARGO_CFG_TARGET_FEATURE").unwrap_or_default());
    // The features follow the rustflags, and a change there rebuilds and reruns this anyway
    println!("cargo::rerun-if-changed=build.rs");
}
