//! Cross-builds the userland demo binaries under `userland/` and the kernel modules under
//! `modules/` so `src/main.rs` can embed them via `include_bytes!(env!(...))`. This keeps
//! `cargo build`/`cargo run`/`cargo test` working with no manual pre-step.
//!
//! Builds into target-dirs of their own (`target/userland`, `target/modules`), not the shared
//! workspace `target/` directory: cargo takes a lock on the target directory for the whole outer
//! build, including while this build script runs, so a nested `cargo build` sharing that same
//! target directory can deadlock waiting for a lock the outer, still-running build already holds.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let ring3_smoke_elf_path = build_userland_crate("ring3-smoke", "RING3_SMOKE_ELF_PATH");
    build_userland_crate("stsh", "STSH_ELF_PATH");
    build_userland_crate("fork-exec-smoke", "FORK_EXEC_SMOKE_ELF_PATH");

    build_module_crate("hello", "HELLO", &[]);
    build_module_crate("native_abi", "NATIVE_ABI", &[]);

    // ring3-smoke is embedded into the FAT32 image below (as SMOKE.ELF) so stsh's fork+execve+wait
    // path has a real, already-working target it can run as an actual file, not just another
    // include_bytes!'d demo -- see CLAUDE.md's process/scheduler section.
    let ring3_smoke_elf = std::fs::read(&ring3_smoke_elf_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", ring3_smoke_elf_path.display()));

    // musl-smoke is a first real (patched) musl static binary -- see CLAUDE.md's musl section --
    // embedded into the FAT32 image below (as MUSL.ELF) the same way ring3-smoke is, so stsh's
    // existing fork+execve+wait path can run it as a real file with no separate boot-time wiring.
    let musl_sysroot = build_musl_sysroot();
    let musl_smoke_elf_path = build_musl_smoke(&musl_sysroot);
    let musl_smoke_elf = std::fs::read(&musl_smoke_elf_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", musl_smoke_elf_path.display()));

    let fat32_image_path = write_fat32_image(&ring3_smoke_elf, &musl_smoke_elf);
    build_module_crate(
        "fat32",
        "FAT32",
        &[("FAT32_IMAGE_PATH", fat32_image_path.to_str().unwrap())],
    );
}

/// Configures, builds, and installs the vendored, OxideBSD-patched musl (`third_party/musl` -- a
/// submodule pointing at a personal fork, patched on its own `oxidebsd` branch to speak this
/// kernel's native ABI directly -- see `CLAUDE.md`'s musl section) into `target/musl-sysroot`,
/// producing a `musl-gcc`-style wrapper this build script can shell out to for
/// `userland/musl-smoke/`. Uses musl's own build system directly (`configure`/`make`/
/// `make install`) -- there's no Cargo/Rust involved at all, it's a plain C library. Skips
/// `./configure` if a `config.mak` already exists (configure itself takes several seconds
/// re-probing the host compiler on every run; `make`/`make install` are already fast, idempotent
/// no-ops when nothing changed, so only configure needs this guard).
fn build_musl_sysroot() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let musl_dir = Path::new(manifest_dir).join("third_party/musl");
    let sysroot = Path::new(manifest_dir).join("target/musl-sysroot");

    println!(
        "cargo:rerun-if-changed={}",
        musl_dir.join("arch/x86_64").display()
    );
    println!("cargo:rerun-if-changed={}", musl_dir.join("src").display());

    if !musl_dir.join("config.mak").exists() {
        // Run via its own path (not `sh configure`, and not a bare relative "configure"): the
        // script derives its own source directory from `${0%/configure}` (build.rs:201 in
        // musl's own configure) -- given anything that doesn't literally end in "/configure",
        // that substitution is a no-op and it tries to `cd` into a nonexistent directory named
        // after whatever `$0` was. `./configure` (a real, executable path ending in "/configure")
        // is the one invocation shape that satisfies its own self-location logic.
        let status = Command::new("./configure")
            .current_dir(&musl_dir)
            .args([
                "--disable-shared",
                &format!("--prefix={}", sysroot.display()),
            ])
            .status()
            .unwrap_or_else(|e| panic!("failed to run musl's configure: {e}"));
        if !status.success() {
            panic!("musl configure failed: {status}");
        }
    }

    let jobs = std::thread::available_parallelism().map_or(1, |n| n.get());
    let status = Command::new("make")
        .current_dir(&musl_dir)
        .args(["-j", &jobs.to_string()])
        .status()
        .unwrap_or_else(|e| panic!("failed to run make for musl: {e}"));
    if !status.success() {
        panic!("musl build failed: {status}");
    }

    let status = Command::new("make")
        .current_dir(&musl_dir)
        .arg("install")
        .status()
        .unwrap_or_else(|e| panic!("failed to run make install for musl: {e}"));
    if !status.success() {
        panic!("musl install failed: {status}");
    }

    sysroot
}

/// Cross-builds `userland/musl-smoke/main.c` against `sysroot` (see `build_musl_sysroot` above),
/// at a load address (`0xa00000`) clear of both the bootloader's own ~6 MiB identity-mapped
/// low-memory region and every other userland crate's load base (`0x600000`-`0x900000`) --
/// confirmed empirically via `readelf -hl` before this was written, the same discipline
/// CLAUDE.md's own `ring3-smoke` load-address collision story already established. Unlike every
/// other `userland/*` crate this isn't a Rust crate at all -- musl-smoke exists specifically to
/// exercise a real musl static binary, so it's built with `musl-gcc` directly, no cargo involved.
fn build_musl_smoke(sysroot: &Path) -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let src = Path::new(manifest_dir).join("userland/musl-smoke/main.c");
    let target_dir = Path::new(manifest_dir).join("target/musl-smoke");
    std::fs::create_dir_all(&target_dir).expect("failed to create target/musl-smoke");
    let out = target_dir.join("musl-smoke");

    println!("cargo:rerun-if-changed={}", src.display());

    let musl_gcc = sysroot.join("bin/musl-gcc");
    let status = Command::new(&musl_gcc)
        .arg("-static")
        .arg("-no-pie")
        .arg("-Wl,-Ttext-segment=0xa00000")
        .arg("-O2")
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .status()
        .unwrap_or_else(|e| panic!("failed to run musl-gcc for musl-smoke: {e}"));
    if !status.success() {
        panic!("building musl-smoke failed: {status}");
    }
    out
}

/// Cross-builds the userland crate at `userland/<crate_name>/` and exposes its resulting ELF's
/// path via `cargo:rustc-env=<env_var>=<path>`, and returns that same path so callers that need
/// the raw bytes on the host side (`main`, for embedding `ring3-smoke` into the FAT32 image) don't
/// have to re-derive it.
fn build_userland_crate(crate_name: &str, env_var: &str) -> PathBuf {
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

    let cargo = cargo_bin();
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
    elf_path
}

/// Cross-builds the kernel module crate at `modules/<crate_name>/` into a single relocatable
/// (`ET_REL`) object file, ready for `src/module.rs` to load and relocate at boot, and exposes it
/// via `cargo:rustc-env=<name_var>_MOD_PATH=<path>` (`name_var` is `env_var` upper-cased). See
/// `CLAUDE.md`'s module-loading section for the full rationale; in short:
///
/// - Module crates are plain `#![no_std]` `lib` crates -- no `_start`, no linker script, no final
///   link. `cargo rustc -- --emit=obj -C codegen-units=1` produces exactly one `ET_REL` object,
///   skipping the link step entirely.
/// - `-C relocation-model=static` (scoped to this nested build only, via `RUSTFLAGS`) keeps every
///   relocation a simple absolute/PC-relative form -- no GOT -- in exchange for requiring the
///   module's eventual mapped address to stay within the low 2 GiB (see `src/module.rs`'s
///   `MODULE_VA_BASE`).
/// - The module's own object alone has an open-ended, code-content-dependent set of undefined
///   symbols (anything from `memcpy` to `core::fmt::write` to panic machinery, depending on what
///   the module's code happens to do) -- not something a hand-curated kernel API table can
///   practically enumerate in advance. A build-time partial relink (`rust-lld -r`, not a final
///   link) against the exact `core`/`alloc`/`compiler_builtins` rlibs this same build produced
///   closes over all of that, leaving only the module's genuine calls into the kernel API plus
///   one fixed, compiler-synthesized panic-entry symbol (discovered below, not hardcoded, since
///   its exact mangled name is toolchain-dependent) unresolved.
///
/// `extra_env` is passed straight through to the nested `cargo rustc` invocation -- used by the
/// `fat32` module to receive its generated disk image's path (`FAT32_IMAGE_PATH`) for its own
/// `include_bytes!(env!("FAT32_IMAGE_PATH"))`, since that module has no `build.rs` of its own
/// (modules never do -- there's no linker script to pass, they're never linked at all).
fn build_module_crate(crate_name: &str, env_var: &str, extra_env: &[(&str, &str)]) {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let module_dir = Path::new(manifest_dir).join("modules").join(crate_name);
    let target_dir = Path::new(manifest_dir).join("target/modules");

    println!(
        "cargo:rerun-if-changed={}",
        module_dir.join("src").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        module_dir.join("Cargo.toml").display()
    );

    let cargo = cargo_bin();
    let mut command = Command::new(&cargo);
    command
        .current_dir(manifest_dir)
        .args([
            "rustc",
            "--manifest-path",
            module_dir.join("Cargo.toml").to_str().unwrap(),
            "--release",
            "--lib",
            "--target-dir",
            target_dir.to_str().unwrap(),
            "--",
            "--emit=obj",
            "-C",
            "codegen-units=1",
        ])
        .env_remove("CARGO_MANIFEST_DIR")
        .env_remove("CARGO_PKG_NAME")
        // See the doc comment above: eliminates GOT-indirected relocations everywhere, including
        // inside the precompiled core/alloc this nested `-Z build-std` invocation produces (which
        // doesn't inherit the trailing `--emit=obj`-style flags, only RUSTFLAGS).
        .env("RUSTFLAGS", "-C relocation-model=static");
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let status = command
        .status()
        .unwrap_or_else(|e| panic!("failed to run cargo rustc for module {crate_name}: {e}"));

    if !status.success() {
        panic!("building the {crate_name} module's object file failed: {status}");
    }

    let deps_dir = target_dir.join("x86_64-oxidebsd/release/deps");
    let module_obj =
        newest_matching(&deps_dir, &format!("{crate_name}-"), ".o").unwrap_or_else(|| {
            panic!(
                "{crate_name}: no object file found in {}",
                deps_dir.display()
            )
        });

    let sysroot = rustc_output(manifest_dir, &["--print", "sysroot"]);
    let host = host_triple(manifest_dir);
    let llvm_bin = Path::new(&sysroot)
        .join("lib/rustlib")
        .join(&host)
        .join("bin");

    let merged_obj = target_dir.join(format!("{crate_name}-merged.o"));
    partial_link(crate_name, &llvm_bin, &deps_dir, &module_obj, &merged_obj);

    let panic_symbol = discover_panic_symbol(&llvm_bin, &merged_obj);

    println!(
        "cargo:rustc-env={env_var}_MOD_PATH={}",
        merged_obj.display()
    );
    println!(
        "cargo:rustc-env={env_var}_MOD_PANIC_SYMBOL={}",
        panic_symbol.as_deref().unwrap_or("")
    );
}

/// Merges `module_obj` with the exact `core`/`alloc`/`compiler_builtins` rlibs found in
/// `deps_dir` via a relocatable ("partial") link -- `-r`, not a final link -- so that any symbol
/// the module's code references from those crates resolves at build time. Archive members are
/// pulled in only if actually referenced (ordinary linker semantics), wrapped in
/// `--start-group`/`--end-group` since `core`/`alloc`/`compiler_builtins` reference each other
/// and a single pass wouldn't otherwise guarantee a resolving order.
///
/// `--gc-sections -u module_init`: archive-member selection is coarse (a whole `.o` file, which
/// for `-Z build-std`'s own precompiled `core`/`alloc` can bundle many unrelated functions
/// together), so referencing just one symbol from a bundled member can otherwise pull in
/// everything else defined alongside it. This was discovered as a real, non-optional requirement
/// (not the "nice to have, defer it" size optimization an earlier draft of this design assumed
/// it'd be) when `modules/fat32/`'s very first boot attempt exhausted the kernel's small heap:
/// referencing `core::panicking::panic_bounds_check` (reachable from any ordinary slice
/// indexing) alone pulled in most of `core::fmt`'s numeric/Unicode tables, ballooning that one
/// module to 3+ MB across ~2900 sections. `-u module_init` marks every module's sole real entry
/// point as a GC root (`-r` produces no executable with an implicit entry point of its own, so
/// nothing is reachable by default) -- `--gc-sections` then prunes every section not transitively
/// reachable from it, which brought that same object down to ~60 sections.
fn partial_link(
    crate_name: &str,
    llvm_bin: &Path,
    deps_dir: &Path,
    module_obj: &Path,
    merged_obj: &Path,
) {
    let lld = llvm_bin.join("rust-lld");
    assert!(
        lld.exists(),
        "rust-lld not found at {} -- is the llvm-tools-preview rustup component installed? \
         (see rust-toolchain.toml)",
        lld.display()
    );

    let find_rlib = |name: &str| {
        newest_matching(deps_dir, &format!("lib{name}-"), ".rlib").unwrap_or_else(|| {
            panic!(
                "{crate_name}: no {name} rlib found in {} -- is `-Z build-std` producing one?",
                deps_dir.display()
            )
        })
    };
    let core_rlib = find_rlib("core");
    let alloc_rlib = find_rlib("alloc");
    let compiler_builtins_rlib = find_rlib("compiler_builtins");

    let status = Command::new(&lld)
        .args([
            "-flavor",
            "gnu",
            "-r",
            "--gc-sections",
            "-u",
            "module_init",
            "-o",
            merged_obj.to_str().unwrap(),
            module_obj.to_str().unwrap(),
            "--start-group",
        ])
        .args([&core_rlib, &alloc_rlib, &compiler_builtins_rlib])
        .arg("--end-group")
        .status()
        .unwrap_or_else(|e| panic!("failed to run rust-lld for module {crate_name}: {e}"));

    if !status.success() {
        panic!("partial link for module {crate_name} failed: {status}");
    }
}

/// Scans `object`'s undefined symbols for the compiler-synthesized panic entry point
/// (`core::panicking`'s internal `rust_begin_unwind` declaration, called by every panicking-path
/// function `core`/`alloc` contain). Its exact mangled name embeds a crate-metadata hash that's
/// toolchain-dependent and not worth hardcoding -- `rust_begin_unwind` still appears as a literal
/// substring of the mangled name (Rust's v0 mangling spells out path components as length-prefixed
/// text), so a substring search is enough to find it reliably. Returns `None` if the module's code
/// never actually references it (e.g. no panicking-capable operations survived optimization) --
/// that's fine, `src/module.rs`'s resolver only needs entries for symbols a module actually uses.
fn discover_panic_symbol(llvm_bin: &Path, object: &Path) -> Option<String> {
    let nm = llvm_bin.join("llvm-nm");
    let output = Command::new(&nm)
        .args(["--undefined-only", "--format=just-symbols"])
        .arg(object)
        .output()
        .unwrap_or_else(|e| panic!("failed to run llvm-nm on {}: {e}", object.display()));
    assert!(
        output.status.success(),
        "llvm-nm failed for {}",
        object.display()
    );
    String::from_utf8(output.stdout)
        .expect("llvm-nm output wasn't valid UTF-8")
        .lines()
        .find(|line| line.contains("rust_begin_unwind"))
        .map(|s| s.trim().to_string())
}

/// Generates a small, deliberately non-spec-minimum-sized but structurally correct FAT32 disk
/// image (own code, not `mkfs.fat` -- see `CLAUDE.md`'s module-loading/FAT32 section for why:
/// hermeticity, and a real `mkfs.fat`-produced FAT32 volume needs to be tens of megabytes to meet
/// Microsoft's minimum-cluster-count heuristic, impractical to embed), writes it to
/// `target/modules/fat32.img`, and returns that path for `build_module_crate`'s `extra_env` to
/// pass through as `FAT32_IMAGE_PATH`. Real BPB/FSInfo, 2 FAT copies, 32-bit FAT entries, and the
/// root directory as a proper cluster chain (not FAT16's fixed region) -- only this kernel's own
/// hand-rolled parser (`modules/fat32/`) ever needs to read it, so the "real minimum size" rule is
/// safe to deliberately violate.
fn write_fat32_image(smoke_elf_bytes: &[u8], musl_elf_bytes: &[u8]) -> PathBuf {
    let image = generate_fat32_image(smoke_elf_bytes, musl_elf_bytes);
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let target_dir = Path::new(manifest_dir).join("target/modules");
    std::fs::create_dir_all(&target_dir).expect("failed to create target/modules");
    let path = target_dir.join("fat32.img");
    std::fs::write(&path, &image)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
    path
}

const FAT32_BYTES_PER_SECTOR: usize = 512;
const FAT32_SECTORS_PER_CLUSTER: u8 = 1;
const FAT32_RESERVED_SECTORS: u32 = 32;
const FAT32_NUM_FATS: u32 = 2;
/// 2 MiB total -- far below the ~65525-cluster count real FAT32 volumes are conventionally
/// expected to have, deliberately (see this function's caller's doc comment).
const FAT32_TOTAL_SECTORS: u32 = 4096;

const FAT32_ROOT_CLUSTER: u32 = 2;
const FAT32_HELLO_CLUSTER: u32 = 3;
const FAT32_BIG_FIRST_CLUSTER: u32 = 4;
const FAT32_BIG_CLUSTER_COUNT: u32 = 3;
/// SMOKE.ELF's cluster count isn't a fixed constant like BIG.TXT's -- it depends on the built
/// `ring3-smoke` ELF's actual size, computed at image-generation time from `smoke_elf_bytes.len()`.
const FAT32_SMOKE_FIRST_CLUSTER: u32 = FAT32_BIG_FIRST_CLUSTER + FAT32_BIG_CLUSTER_COUNT;
/// MUSL.ELF's own first cluster isn't a fixed constant either -- it starts right after however
/// many clusters SMOKE.ELF ends up needing, computed at image-generation time just like
/// `FAT32_SMOKE_FIRST_CLUSTER`'s own runtime-computed cluster count is chained onto BIG.TXT's.
const FAT32_EOC: u32 = 0x0FFF_FFFF;

const FAT32_HELLO_CONTENTS: &[u8] = b"Hello from FAT32!\n";
/// Deliberately a formula-derived pattern (`b'A' + index % 26`), not a literal, so
/// `modules/fat32`'s own self-check can independently recompute the expected bytes rather than
/// needing a second copy of a large literal kept in sync by hand.
const FAT32_BIG_FILE_LEN: usize = 1224;

fn fat32_big_file_byte(index: usize) -> u8 {
    b'A' + (index % 26) as u8
}

fn generate_fat32_image(smoke_elf_bytes: &[u8], musl_elf_bytes: &[u8]) -> Vec<u8> {
    let smoke_cluster_count =
        (smoke_elf_bytes.len().div_ceil(FAT32_BYTES_PER_SECTOR) as u32).max(1);
    let musl_first_cluster = FAT32_SMOKE_FIRST_CLUSTER + smoke_cluster_count;
    let musl_cluster_count = (musl_elf_bytes.len().div_ceil(FAT32_BYTES_PER_SECTOR) as u32).max(1);

    // Solve for the FAT size (in sectors) that exactly covers the clusters left over once that
    // same FAT size is reserved -- a small fixed-point iteration, since the FAT's own size is
    // tiny relative to the volume and converges in only a couple of passes.
    let mut fat_size_sectors: u32 = 1;
    for _ in 0..8 {
        let data_sectors =
            FAT32_TOTAL_SECTORS - FAT32_RESERVED_SECTORS - FAT32_NUM_FATS * fat_size_sectors;
        let total_clusters = data_sectors / FAT32_SECTORS_PER_CLUSTER as u32;
        let fat_bytes_needed = (total_clusters + 2) * 4;
        fat_size_sectors = fat_bytes_needed.div_ceil(FAT32_BYTES_PER_SECTOR as u32);
    }
    let data_start_sector = FAT32_RESERVED_SECTORS + FAT32_NUM_FATS * fat_size_sectors;

    let highest_cluster_used = musl_first_cluster + musl_cluster_count - 1;
    let data_clusters =
        (FAT32_TOTAL_SECTORS - data_start_sector) / FAT32_SECTORS_PER_CLUSTER as u32;
    assert!(
        highest_cluster_used < 2 + data_clusters,
        "ring3-smoke ({} bytes) + musl-smoke ({} bytes) no longer fit in the embedded FAT32 image \
         ({} total bytes) -- raise FAT32_TOTAL_SECTORS",
        smoke_elf_bytes.len(),
        musl_elf_bytes.len(),
        FAT32_TOTAL_SECTORS as usize * FAT32_BYTES_PER_SECTOR
    );

    let mut image = vec![0u8; FAT32_TOTAL_SECTORS as usize * FAT32_BYTES_PER_SECTOR];

    // --- Boot sector / BPB (sector 0) ---
    {
        let bs = &mut image[0..FAT32_BYTES_PER_SECTOR];
        bs[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]); // BS_jmpBoot
        bs[3..11].copy_from_slice(b"OXIDEBSD"); // BS_OEMName
        bs[11..13].copy_from_slice(&(FAT32_BYTES_PER_SECTOR as u16).to_le_bytes()); // BPB_BytsPerSec
        bs[13] = FAT32_SECTORS_PER_CLUSTER; // BPB_SecPerClus
        bs[14..16].copy_from_slice(&(FAT32_RESERVED_SECTORS as u16).to_le_bytes()); // BPB_RsvdSecCnt
        bs[16] = FAT32_NUM_FATS as u8; // BPB_NumFATs
        // BPB_RootEntCnt (17..19) and BPB_TotSec16 (19..21) are 0 for FAT32.
        bs[21] = 0xF8; // BPB_Media (fixed disk)
        // BPB_FATSz16 (22..24) is 0 for FAT32 -- BPB_FATSz32 below is authoritative.
        bs[24..26].copy_from_slice(&32u16.to_le_bytes()); // BPB_SecPerTrk (dummy geometry)
        bs[26..28].copy_from_slice(&64u16.to_le_bytes()); // BPB_NumHeads (dummy geometry)
        bs[32..36].copy_from_slice(&FAT32_TOTAL_SECTORS.to_le_bytes()); // BPB_TotSec32
        bs[36..40].copy_from_slice(&fat_size_sectors.to_le_bytes()); // BPB_FATSz32
        bs[44..48].copy_from_slice(&FAT32_ROOT_CLUSTER.to_le_bytes()); // BPB_RootClus
        bs[48..50].copy_from_slice(&1u16.to_le_bytes()); // BPB_FSInfo (sector 1)
        bs[50..52].copy_from_slice(&6u16.to_le_bytes()); // BPB_BkBootSec (sector 6)
        bs[64] = 0x80; // BS_DrvNum
        bs[66] = 0x29; // BS_BootSig (marks VolID/VolLab/FilSysType below as valid)
        bs[67..71].copy_from_slice(&0x0BAD_F32Fu32.to_le_bytes()); // BS_VolID
        bs[71..82].copy_from_slice(b"OXIDEBSD FS"); // BS_VolLab (11 bytes)
        bs[82..90].copy_from_slice(b"FAT32   "); // BS_FilSysType (informational only)
        bs[510] = 0x55;
        bs[511] = 0xAA;
    }

    // --- FSInfo sector (sector 1) --- structural authenticity only: modules/fat32's own parser
    // never reads this (real FAT32 drivers treat it as a non-authoritative performance hint), so
    // its free-cluster fields are left "unknown" rather than computed precisely.
    {
        let fs = &mut image[FAT32_BYTES_PER_SECTOR..2 * FAT32_BYTES_PER_SECTOR];
        fs[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes()); // LeadSig
        fs[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes()); // StrucSig
        fs[488..492].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // Free_Count (unknown)
        fs[492..496].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // Next_Free (unknown)
        fs[508..512].copy_from_slice(&0xAA55_0000u32.to_le_bytes()); // TrailSig
    }

    // --- Backup boot sector (sector 6, per BPB_BkBootSec) ---
    {
        let (before, after) = image.split_at_mut(6 * FAT32_BYTES_PER_SECTOR);
        after[0..FAT32_BYTES_PER_SECTOR].copy_from_slice(&before[0..FAT32_BYTES_PER_SECTOR]);
    }

    // --- FAT tables (both copies kept identical) ---
    for fat_index in 0..FAT32_NUM_FATS {
        write_fat_entry(&mut image, fat_index, fat_size_sectors, 0, 0x0FFF_FFF8);
        write_fat_entry(&mut image, fat_index, fat_size_sectors, 1, 0x0FFF_FFFF);
        write_fat_entry(
            &mut image,
            fat_index,
            fat_size_sectors,
            FAT32_ROOT_CLUSTER,
            FAT32_EOC,
        );
        write_fat_entry(
            &mut image,
            fat_index,
            fat_size_sectors,
            FAT32_HELLO_CLUSTER,
            FAT32_EOC,
        );
        for i in 0..FAT32_BIG_CLUSTER_COUNT {
            let cluster = FAT32_BIG_FIRST_CLUSTER + i;
            let value = if i + 1 == FAT32_BIG_CLUSTER_COUNT {
                FAT32_EOC
            } else {
                cluster + 1
            };
            write_fat_entry(&mut image, fat_index, fat_size_sectors, cluster, value);
        }
        for i in 0..smoke_cluster_count {
            let cluster = FAT32_SMOKE_FIRST_CLUSTER + i;
            let value = if i + 1 == smoke_cluster_count {
                FAT32_EOC
            } else {
                cluster + 1
            };
            write_fat_entry(&mut image, fat_index, fat_size_sectors, cluster, value);
        }
        for i in 0..musl_cluster_count {
            let cluster = musl_first_cluster + i;
            let value = if i + 1 == musl_cluster_count {
                FAT32_EOC
            } else {
                cluster + 1
            };
            write_fat_entry(&mut image, fat_index, fat_size_sectors, cluster, value);
        }
    }

    let cluster_offset = |cluster: u32| -> usize {
        (data_start_sector as usize + (cluster as usize - 2) * FAT32_SECTORS_PER_CLUSTER as usize)
            * FAT32_BYTES_PER_SECTOR
    };

    // --- Root directory (cluster 2): volume label + three file entries ---
    {
        let root_offset = cluster_offset(FAT32_ROOT_CLUSTER);
        let mut entry_offset = root_offset;
        write_dir_entry(&mut image, entry_offset, b"OXIDEBSD FS", 0x08, 0, 0);
        entry_offset += 32;
        write_dir_entry(
            &mut image,
            entry_offset,
            b"HELLO   TXT",
            0x20,
            FAT32_HELLO_CLUSTER,
            FAT32_HELLO_CONTENTS.len() as u32,
        );
        entry_offset += 32;
        write_dir_entry(
            &mut image,
            entry_offset,
            b"BIG     TXT",
            0x20,
            FAT32_BIG_FIRST_CLUSTER,
            FAT32_BIG_FILE_LEN as u32,
        );
        entry_offset += 32;
        write_dir_entry(
            &mut image,
            entry_offset,
            b"SMOKE   ELF",
            0x20,
            FAT32_SMOKE_FIRST_CLUSTER,
            smoke_elf_bytes.len() as u32,
        );
        entry_offset += 32;
        write_dir_entry(
            &mut image,
            entry_offset,
            b"MUSL    ELF",
            0x20,
            musl_first_cluster,
            musl_elf_bytes.len() as u32,
        );
        // No further entries -- the byte after this one is already 0 (image starts zeroed),
        // which is the FAT directory end-of-listing marker.
    }

    // --- HELLO.TXT contents ---
    {
        let offset = cluster_offset(FAT32_HELLO_CLUSTER);
        image[offset..offset + FAT32_HELLO_CONTENTS.len()].copy_from_slice(FAT32_HELLO_CONTENTS);
    }

    // --- BIG.TXT contents (spans multiple clusters, exercising chain-following) ---
    {
        let mut remaining = FAT32_BIG_FILE_LEN;
        let mut written = 0usize;
        for i in 0..FAT32_BIG_CLUSTER_COUNT {
            let cluster = FAT32_BIG_FIRST_CLUSTER + i;
            let offset = cluster_offset(cluster);
            let chunk_len = remaining.min(FAT32_BYTES_PER_SECTOR);
            for j in 0..chunk_len {
                image[offset + j] = fat32_big_file_byte(written + j);
            }
            written += chunk_len;
            remaining -= chunk_len;
        }
    }

    // --- SMOKE.ELF contents (the built ring3-smoke binary, chunked across smoke_cluster_count
    // clusters exactly like BIG.TXT's chain above, generalized for an arbitrary byte length) ---
    {
        for (i, chunk) in smoke_elf_bytes.chunks(FAT32_BYTES_PER_SECTOR).enumerate() {
            let cluster = FAT32_SMOKE_FIRST_CLUSTER + i as u32;
            let offset = cluster_offset(cluster);
            image[offset..offset + chunk.len()].copy_from_slice(chunk);
        }
    }

    // --- MUSL.ELF contents (the built musl-smoke binary -- see CLAUDE.md's musl section --
    // chunked the same way SMOKE.ELF's own bytes are above) ---
    {
        for (i, chunk) in musl_elf_bytes.chunks(FAT32_BYTES_PER_SECTOR).enumerate() {
            let cluster = musl_first_cluster + i as u32;
            let offset = cluster_offset(cluster);
            image[offset..offset + chunk.len()].copy_from_slice(chunk);
        }
    }

    image
}

fn write_fat_entry(
    image: &mut [u8],
    fat_index: u32,
    fat_size_sectors: u32,
    cluster: u32,
    value: u32,
) {
    let fat_start =
        (FAT32_RESERVED_SECTORS + fat_index * fat_size_sectors) as usize * FAT32_BYTES_PER_SECTOR;
    let offset = fat_start + cluster as usize * 4;
    image[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_dir_entry(
    image: &mut [u8],
    offset: usize,
    name_11: &[u8; 11],
    attr: u8,
    first_cluster: u32,
    size: u32,
) {
    let entry = &mut image[offset..offset + 32];
    entry[0..11].copy_from_slice(name_11);
    entry[11] = attr;
    entry[20..22].copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
    entry[26..28].copy_from_slice(&(first_cluster as u16).to_le_bytes());
    entry[28..32].copy_from_slice(&size.to_le_bytes());
}

/// Finds the file matching `<prefix>*<suffix>` most recently modified in `dir` -- filenames under
/// `deps/` carry a non-deterministic metadata hash, so an exact name can't be predicted, and
/// picking the newest (rather than asserting exactly one) tolerates stale artifacts left behind by
/// a prior build with different flags reusing the same target-dir.
fn newest_matching(dir: &Path, prefix: &str, suffix: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(prefix) && name.ends_with(suffix)
        })
        .max_by_key(|entry| entry.metadata().and_then(|m| m.modified()).ok())
        .map(|entry| entry.path())
}

fn cargo_bin() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

fn rustc_output(manifest_dir: &str, args: &[&str]) -> String {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let output = Command::new(&rustc)
        .current_dir(manifest_dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run rustc {args:?}: {e}"));
    assert!(output.status.success(), "rustc {args:?} failed");
    String::from_utf8(output.stdout)
        .expect("rustc output wasn't valid UTF-8")
        .trim()
        .to_string()
}

fn host_triple(manifest_dir: &str) -> String {
    let verbose = rustc_output(manifest_dir, &["-vV"]);
    verbose
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .expect("rustc -vV output missing a 'host:' line")
        .to_string()
}
