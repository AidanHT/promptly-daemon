//! Capture the target triple this binary is built for.
//!
//! Cargo exposes `TARGET` to build scripts but not to the crate itself, and
//! `promptly update` needs it to name the right release asset (the release
//! workflow tags each archive with the same triple). Re-export it as a compile
//! env var so `env!("PROMPTLY_BUILD_TARGET")` resolves it at compile time.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=PROMPTLY_BUILD_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
