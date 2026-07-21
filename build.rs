//! Cross-builds the userland demo binaries under `userland/` so `src/main.rs` can embed them via
//! `include_bytes!(env!(...))`. This keeps `cargo build`/`cargo run`/`cargo test` working with no
//! manual pre-step.
//!
//! Builds into a target-dir of its own (`target/userland`), not the shared workspace `target/`
//! directory: cargo takes a lock on the target directory for the whole outer build, including
//! while this build script runs, so a nested `cargo build` sharing that same target directory can
//! deadlock waiting for a lock the outer, still-running build already holds.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    build_userland_crate("ring3-smoke", "RING3_SMOKE_ELF_PATH");
    build_userland_crate("linux-syscall-smoke", "LINUX_SYSCALL_SMOKE_ELF_PATH");
}

/// Cross-builds the userland crate at `userland/<crate_name>/` and exposes its resulting ELF's
/// path via `cargo:rustc-env=<env_var>=<path>`.
fn build_userland_crate(crate_name: &str, env_var: &str) {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let userland_dir = Path::new(manifest_dir).join("userland").join(crate_name);
    let target_dir = Path::new(manifest_dir).join("target/userland");

    println!(
        "cargo:rerun-if-changed={}",
        userland_dir.join("src").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        userland_dir.join("Cargo.toml").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        userland_dir.join("linker.ld").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        userland_dir.join("build.rs").display()
    );

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let status = Command::new(&cargo)
        .current_dir(manifest_dir)
        .args([
            "build",
            "--manifest-path",
            userland_dir.join("Cargo.toml").to_str().unwrap(),
            "--release",
            "--target-dir",
            target_dir.to_str().unwrap(),
        ])
        // Building a userland crate is itself a `cargo build` invocation; without clearing these,
        // it inherits the outer build's CARGO_* env vars (wrong package name/manifest dir/etc.)
        // from when *this* build script was invoked.
        .env_remove("CARGO_MANIFEST_DIR")
        .env_remove("CARGO_PKG_NAME")
        .status()
        .unwrap_or_else(|e| panic!("failed to run cargo for {crate_name}: {e}"));

    if !status.success() {
        panic!("building the {crate_name} userland binary failed: {status}");
    }

    let elf_path: PathBuf = target_dir.join("x86_64-oxidebsd/release").join(crate_name);
    assert!(
        elf_path.exists(),
        "{crate_name} build reported success but {} doesn't exist",
        elf_path.display()
    );
    println!("cargo:rustc-env={env_var}={}", elf_path.display());
}
