//! Cross-builds the `ring3-smoke` userland demo (see `userland/ring3-smoke/`) so `src/main.rs`
//! can embed it via `include_bytes!(env!("RING3_SMOKE_ELF_PATH"))`. This keeps `cargo build`/
//! `cargo run`/`cargo test` working with no manual pre-step.
//!
//! Builds into a target-dir of its own (`target/userland`), not the shared workspace `target/`
//! directory: cargo takes a lock on the target directory for the whole outer build, including
//! while this build script runs, so a nested `cargo build` sharing that same target directory can
//! deadlock waiting for a lock the outer, still-running build already holds.

use std::path::Path;
use std::process::Command;

fn main() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let userland_dir = Path::new(manifest_dir).join("userland/ring3-smoke");
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
        // Building ring3-smoke is itself a `cargo build` invocation; without clearing these, it
        // inherits the outer build's CARGO_* env vars (wrong package name/manifest dir/etc.) from
        // when *this* build script was invoked.
        .env_remove("CARGO_MANIFEST_DIR")
        .env_remove("CARGO_PKG_NAME")
        .status()
        .expect("failed to run cargo for the ring3-smoke userland binary");

    if !status.success() {
        panic!("building the ring3-smoke userland binary failed: {status}");
    }

    let elf_path = target_dir.join("x86_64-oxidebsd/release/ring3-smoke");
    assert!(
        elf_path.exists(),
        "ring3-smoke build reported success but {} doesn't exist",
        elf_path.display()
    );
    println!(
        "cargo:rustc-env=RING3_SMOKE_ELF_PATH={}",
        elf_path.display()
    );
}
