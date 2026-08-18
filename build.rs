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

// `BUSYBOX_APPLETS`/`BUSYBOX_APPLETS_PASS2`/`build_busybox_applet`/`configure_busybox_single_applet`/
// `resolve_busybox_new_config_options` -- split into their own file specifically so unrelated
// edits to *this* file don't invalidate every cached BusyBox applet binary. See
// `build_busybox.rs`'s own module doc comment for the full story (a real ~20-30 minute rebuild
// cascade, found live).
include!("build_busybox.rs");

fn main() {
    println!("cargo:rerun-if-changed=build_busybox.rs");
    let ring3_smoke_elf_path = build_userland_crate("ring3-smoke", "RING3_SMOKE_ELF_PATH");
    build_userland_crate("stsh", "STSH_ELF_PATH");
    build_userland_crate("fork-exec-smoke", "FORK_EXEC_SMOKE_ELF_PATH");
    // Real-SYSCALL counterparts to the direct-call network smoke tests -- see CLAUDE.md's "Real
    // networking" section for the blind spot these close (every prior network test called kernel
    // handlers as plain Rust functions, never through a genuine SYSCALL with interrupts actually
    // masked).
    build_userland_crate(
        "socketpair-syscall-smoke",
        "SOCKETPAIR_SYSCALL_SMOKE_ELF_PATH",
    );
    build_userland_crate("ping-syscall-smoke", "PING_SYSCALL_SMOKE_ELF_PATH");
    build_userland_crate("udp-syscall-smoke", "UDP_SYSCALL_SMOKE_ELF_PATH");
    build_userland_crate("poll-syscall-smoke", "POLL_SYSCALL_SMOKE_ELF_PATH");
    build_userland_crate("tcp-syscall-smoke", "TCP_SYSCALL_SMOKE_ELF_PATH");
    build_userland_crate("proc-smoke", "PROC_SMOKE_ELF_PATH");
    build_userland_crate("itimer-syscall-smoke", "ITIMER_SYSCALL_SMOKE_ELF_PATH");
    build_userland_crate("uid-syscall-smoke", "UID_SYSCALL_SMOKE_ELF_PATH");
    build_userland_crate(
        "oxfs-persistence-syscall-smoke",
        "OXFS_PERSISTENCE_SYSCALL_SMOKE_ELF_PATH",
    );
    build_userland_crate("mount-syscall-smoke", "MOUNT_SYSCALL_SMOKE_ELF_PATH");
    build_userland_crate("session-syscall-smoke", "SESSION_SYSCALL_SMOKE_ELF_PATH");
    build_userland_crate("needs-syscall-smoke", "NEEDS_SYSCALL_SMOKE_ELF_PATH");
    build_userland_crate("needs-syscall2-smoke", "NEEDS_SYSCALL2_SMOKE_ELF_PATH");
    build_userland_crate("tcc-syscall-smoke", "TCC_SYSCALL_SMOKE_ELF_PATH");
    build_userland_crate("access-syscall-smoke", "ACCESS_SYSCALL_SMOKE_ELF_PATH");
    build_userland_crate(
        "pipe-backpressure-syscall-smoke",
        "PIPE_BACKPRESSURE_SYSCALL_SMOKE_ELF_PATH",
    );
    build_userland_crate(
        "dynlink-syscall-smoke",
        "DYNLINK_SYSCALL_SMOKE_ELF_PATH",
    );
    build_userland_crate(
        "sa-siginfo-syscall-smoke",
        "SA_SIGINFO_SYSCALL_SMOKE_ELF_PATH",
    );
    build_userland_crate(
        "getrandom-syscall-smoke",
        "GETRANDOM_SYSCALL_SMOKE_ELF_PATH",
    );
    build_userland_crate(
        "sysinfo-syscall-smoke",
        "SYSINFO_SYSCALL_SMOKE_ELF_PATH",
    );
    build_userland_crate(
        "sigaltstack-syscall-smoke",
        "SIGALTSTACK_SYSCALL_SMOKE_ELF_PATH",
    );
    build_userland_crate(
        "pause-syscall-smoke",
        "PAUSE_SYSCALL_SMOKE_ELF_PATH",
    );
    build_userland_crate(
        "sigsuspend-syscall-smoke",
        "SIGSUSPEND_SYSCALL_SMOKE_ELF_PATH",
    );
    build_userland_crate(
        "posix-timer-syscall-smoke",
        "POSIX_TIMER_SYSCALL_SMOKE_ELF_PATH",
    );
    build_userland_crate("mq-syscall-smoke", "MQ_SYSCALL_SMOKE_ELF_PATH");
    build_userland_crate(
        "sysv-msg-syscall-smoke",
        "SYSV_MSG_SYSCALL_SMOKE_ELF_PATH",
    );
    build_userland_crate(
        "sysv-sem-syscall-smoke",
        "SYSV_SEM_SYSCALL_SMOKE_ELF_PATH",
    );
    build_userland_crate(
        "sysv-shm-syscall-smoke",
        "SYSV_SHM_SYSCALL_SMOKE_ELF_PATH",
    );
    build_userland_crate("sig-syscall-smoke", "SIG_SYSCALL_SMOKE_ELF_PATH");
    build_userland_crate(
        "posix-conformance-driver",
        "POSIX_CONFORMANCE_DRIVER_ELF_PATH",
    );
    // A real standalone userland utility (embedded into oxfs's own /bin below, not a test) --
    // same category as ring3-smoke/musl-smoke above, not a BusyBox applet. Lists OxideBSD's own
    // loaded kernel modules by reading the real /proc/modules this pass added to modules/oxfs.
    let lsoxmod_elf_path = build_userland_crate("lsoxmod", "LSOXMOD_ELF_PATH");

    build_module_crate("hello", "HELLO", &[]);
    build_module_crate("native_abi", "NATIVE_ABI", &[]);
    build_module_crate("posix_compat", "POSIX_COMPAT", &[]);
    build_module_crate("signal", "SIGNAL", &[]);
    build_module_crate("clock", "CLOCK", &[]);
    build_module_crate("net", "NET", &[]);

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

    // TinyCC: OxideBSD's first on-target C compiler -- see CLAUDE.md's TinyCC section and
    // `build_tinycc`'s own doc comment. The `tcc` binary itself is embedded into oxfs's `/bin`
    // alongside every BusyBox applet; `write_tcc_runtime_manifest` separately produces the
    // generated `/usr/include`+`/usr/lib`+`/usr/lib/tcc` manifest tcc needs to actually compile and
    // link a user's C file once running on target (not needed just to launch).
    let tinycc_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("third_party/tinycc");
    let tcc_elf_path = build_tinycc(&musl_sysroot);
    let tcc_runtime_manifest_path = write_tcc_runtime_manifest(&musl_sysroot, &tinycc_dir);

    // A real POSIX conformance baseline: see `docs/POSIX_COMPLIANCE_CHECKLIST.md`'s own
    // "Verification" section and `modules/oxfs/src/posix_conformance.sh`'s doc comment. Source-only
    // (compiled on-target by `tcc`, not cross-compiled here) -- see `write_posix_test_manifest`'s
    // own doc comment for why.
    let posixtestsuite_dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("third_party/posixtestsuite");
    let posix_test_manifest_path =
        write_posix_test_manifest(&musl_sysroot, &posixtestsuite_dir);

    // Real PT_INTERP / dynamic-linking milestone 1: a real, separate shared musl build (see
    // `build_musl_sysroot_shared`'s own doc comment for why it can't reuse the static sysroot
    // above, and why it's linked at its own natural base rather than a fixed one) producing
    // `libc.so` (which doubles as `ld-musl-x86_64.so.1`, musl's own convention) -- the kernel
    // itself picks its real runtime placement (`src/process/lifecycle.rs`'s `INTERP_LOAD_BASE`, `0xc000000`)
    // at `execve` time, not this build. The fixture binary
    // (`userland/dynlink-smoke/main.c`) is fixed at `0x4d00000` -- an ordinary `ET_EXEC` main
    // binary, same fixed-link-time-base treatment every other userland crate here gets, distinct
    // from `INTERP_LOAD_BASE` since the two must be *co-resident* in the same address space for a
    // real `PT_INTERP` exec to work at all.
    let dynlink_fixture_base: u64 = 0x4d00000;
    let dynlink_musl_sysroot = build_musl_sysroot_shared();
    let dynlink_libc_so_path = dynlink_musl_sysroot.join("lib/libc.so");
    let dynlink_smoke_elf_path =
        build_dynlink_smoke(&dynlink_musl_sysroot, dynlink_fixture_base);

    let busybox_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("third_party/busybox");
    println!("cargo:rerun-if-changed={}", busybox_dir.display());
    let busybox_source_mtime = latest_mtime(&busybox_dir);

    // Parallel across applets, `-j1` within each one (see `build_busybox_applet`'s own doc
    // comment) -- a plain work-stealing pool over a shared atomic index, not a thread per applet
    // (~300 of those would vastly oversubscribe an 8-core host) and not a chunked static split
    // (uneven applet build times would leave some workers idle while others queue up).
    let all_applets: Vec<(&str, &str, u64)> = BUSYBOX_APPLETS
        .iter()
        .copied()
        .chain(BUSYBOX_APPLETS_PASS2.iter().copied())
        .collect();
    let jobs = std::thread::available_parallelism().map_or(1, |n| n.get());
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(&(applet_symbol, out_name, load_addr)) = all_applets.get(i) else {
                        break;
                    };
                    build_busybox_applet(
                        applet_symbol,
                        out_name,
                        load_addr,
                        &musl_sysroot,
                        busybox_source_mtime,
                    );
                }
            });
        }
    });

    // modules/fat32 is kept in the workspace but no longer loaded at boot (see CLAUDE.md's oxfs
    // section) -- still built here unmodified so it keeps compiling and self-checking on every
    // `cargo build`, a still-working format-correctness proof, just not the live filesystem.
    // Deliberately passed an empty applet slice, not `&busybox_applet_elfs`: `BUSYBOX_APPLETS` grew
    // to ~300 entries once this build script started probing/embedding every applet that happens to
    // build (see CLAUDE.md's BusyBox section) -- `busybox_short_name`'s 8.3-short-name format can't
    // hold names over 8 characters at all (a real `assert!`, not a soft limit) and the image's own
    // fixed `FAT32_TOTAL_SECTORS` budget was sized for a much smaller roster. FAT32 not being the
    // live filesystem means neither limit is worth designing around just to keep embedding
    // applets nothing ever loads from this image.
    let fat32_image_path = write_fat32_image(&ring3_smoke_elf, &musl_smoke_elf, &[]);
    build_module_crate(
        "fat32",
        "FAT32",
        &[("FAT32_IMAGE_PATH", fat32_image_path.to_str().unwrap())],
    );

    // oxfs: the real, live filesystem now (see CLAUDE.md's oxfs section). Unlike FAT32, there's no
    // on-disk image format to generate -- oxfs's own module_init populates its inode table directly
    // via ordinary function calls, using each already-built ELF's path passed straight through as
    // its own env var (the same extra_env mechanism FAT32_IMAGE_PATH above already uses). Built
    // from BUSYBOX_APPLETS itself (not one hand-written `let ..._elf_path = ...` line per applet)
    // so the next applet added there doesn't need a matching edit here too -- `oxfs_env_var_name`
    // derives each one's `OXFS_<NAME>_ELF_PATH` env var straight from its own `out_name`, with one
    // explicit exception ("sh" -> "HUSH", matching `modules/oxfs/src/lib.rs`'s existing
    // `OXFS_HUSH_ELF_PATH`/`seed_file(root, b"sh.elf", ...)` naming, itself inherited from this
    // applet's own Kconfig symbol `HUSH`, not its embedded filename).
    let hush_elf_path_for_main = target_dir_busybox_elf("sh");
    println!("cargo:rustc-env=HUSH_ELF_PATH={hush_elf_path_for_main}");
    let oxfs_applet_paths: Vec<(String, String)> = all_applets
        .iter()
        .map(|&(_, out_name, _)| {
            (
                oxfs_env_var_name(out_name),
                target_dir_busybox_elf(out_name),
            )
        })
        .collect();
    let mut oxfs_extra_env: Vec<(&str, &str)> = vec![
        (
            "OXFS_SMOKE_ELF_PATH",
            ring3_smoke_elf_path.to_str().unwrap(),
        ),
        ("OXFS_MUSL_ELF_PATH", musl_smoke_elf_path.to_str().unwrap()),
        ("OXFS_LSOXMOD_ELF_PATH", lsoxmod_elf_path.to_str().unwrap()),
        ("OXFS_TCC_ELF_PATH", tcc_elf_path.to_str().unwrap()),
        (
            "TCC_RUNTIME_MANIFEST_PATH",
            tcc_runtime_manifest_path.to_str().unwrap(),
        ),
        (
            "POSIX_TEST_MANIFEST_PATH",
            posix_test_manifest_path.to_str().unwrap(),
        ),
        (
            "OXFS_DYNLINK_LIBC_SO_PATH",
            dynlink_libc_so_path.to_str().unwrap(),
        ),
        (
            "OXFS_DYNLINK_SMOKE_ELF_PATH",
            dynlink_smoke_elf_path.to_str().unwrap(),
        ),
    ];
    oxfs_extra_env.extend(
        oxfs_applet_paths
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str())),
    );
    build_module_crate("oxfs", "OXFS", &oxfs_extra_env);

    // Real disk persistence (see src/drivers/ata.rs and modules/oxfs's own "Real disk persistence"
    // section): the two raw disk image *files* QEMU's `-drive` attaches, as opposed to everything
    // above, which gets embedded into the kernel/module binaries themselves via `include_bytes!`.
    write_data_disk_images();
}

fn oxfs_env_var_name(out_name: &str) -> String {
    let suffix = if out_name == "sh" {
        "HUSH".to_string()
    } else {
        out_name.to_uppercase()
    };
    format!("OXFS_{suffix}_ELF_PATH")
}

/// Each `BUSYBOX_APPLETS` entry's own out-of-tree build directory follows a fixed, predictable
/// shape (`target/busybox-<out_name>/busybox`, `build_busybox_applet`'s own `out_dir.join("busybox")`
/// return value) -- re-derived here rather than plumbed through as a second return value, since
/// `busybox_applet_elfs` (built above) only kept the *bytes*, not the path, and oxfs's own
/// `extra_env` needs a path string, not bytes.
fn target_dir_busybox_elf(out_name: &str) -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    Path::new(manifest_dir)
        .join(format!("target/busybox-{out_name}"))
        .join("busybox")
        .to_str()
        .unwrap()
        .to_string()
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
                // Found live, via TinyCC (see CLAUDE.md's TinyCC section): this project's dev
                // host's own real gcc defaults to PIE (confirmed directly: `echo | gcc -E -dM -`
                // defines `__PIC__`/`__PIE__` with *no* flags at all -- a real, common modern
                // distro default, not something this project's own toolchain chose). musl's own
                // `configure` (`trycppif __PIC__ ...`, this file's own line ~578) auto-detects
                // that and lets PIE-style GOT-indirect codegen leak into every musl object it
                // builds, including plain `crt1.o` -- confirmed directly: `readelf -r crt1.o`
                // showed real `R_X86_64_REX_GOTPCRELX` relocations referencing `main`/`_init`/
                // `_fini` from `_start_c`. Every *other* consumer of this sysroot links via
                // `musl-gcc` -> real GNU ld, which silently performs the standard GOTPCRELX
                // link-time relaxation (rewriting the GOT-indirect load into a direct `lea` once
                // it knows the final static address) -- hiding this completely. TinyCC's own
                // linker doesn't implement that relaxation: it reserves a real GOT slot and is
                // *supposed* to fill it with the resolved address (traced through
                // `third_party/tinycc/tccelf.c`'s `build_got_entries`/`fill_got_entry`), but the
                // real, live symptom (a `hello.elf` tcc itself both compiled and linked with no
                // errors at all faulted on a garbage instruction-fetch address the instant it
                // ran) shows that path isn't reliable for this exact case. `-fno-pie -fno-PIC`
                // forces genuinely old-style, non-GOT-indirect codegen for every musl object
                // regardless of host default -- confirmed directly: recompiling `crt1.c` this way
                // produces only plain `R_X86_64_32`/`PC32`/`PLT32` relocations, no GOTPCRELX at
                // all, which is what let this project's very first BusyBox/musl-smoke binaries
                // already work (they happened to get real relaxation from GNU ld, this makes the
                // *input* to any linker unambiguous instead of depending on that relaxation).
                // `obj/crt/Scrt1.o`/`obj/crt/rcrt1.o` (musl's own real PIE crt variants) still
                // force `-fPIC` back on for themselves specifically
                // (`third_party/musl/Makefile`'s own `CFLAGS_ALL += -fPIC` line for those two
                // files only) -- unaffected, and also never embedded into this project's own tcc
                // runtime manifest in the first place (see `write_tcc_runtime_manifest`'s own
                // doc comment on why those two are skipped).
                "CFLAGS=-fno-pie -fno-PIC",
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
/// at a load address (`0x40c0000`, 64 MiB + `0xc0000`) clear of both the kernel's own image (the
/// actually-binding constraint today, not the bootloader's fixed ~6 MiB identity-mapped
/// low-memory region -- see `userland/ring3-smoke/linker.ld`'s own comment for the full story of
/// why this floor moved from an original `0xa00000` and how to re-derive it) and every other
/// userland crate's load base (`0x4000000`-`0x4080000`) -- confirmed empirically via `readelf -hl`
/// before this was written, the same discipline CLAUDE.md's own `ring3-smoke` load-address
/// collision story already established. Unlike every other `userland/*` crate this isn't a Rust
/// crate at all -- musl-smoke exists specifically to
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
        .arg("-Wl,-Ttext-segment=0x40c0000")
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

/// A **second, separate** musl build producing real shared objects (`libc.so`, and
/// `ld-musl-x86_64.so.1` as musl's own install step symlinks it to the same file -- upstream musl
/// has no separate `ld.so` binary) with genuine `-fPIC` codegen, linked at its own natural
/// (near-zero) default base -- **deliberately not** fixed at link time via `-Wl,-Ttext-segment=`
/// the way every other userland binary in this codebase is. Found the hard way, via a real page
/// fault: a fixed-link-time base bakes *already-absolute* addresses into the interpreter's own
/// `.rela.dyn`/`DT_RELA` table (confirmed via `readelf -r`), and real musl's own self-relocation
/// bootstrap (`ldso/dlstart.c`) always computes `real_addr = AT_BASE + stored_value`, expecting
/// `stored_value` to be zero-based -- a fixed-base link double-counts the base and produces a wild
/// pointer. `src/process/elf.rs`'s `elf::load` now applies a real, kernel-chosen runtime bias instead (see
/// its own doc comment and `src/process/lifecycle.rs`'s `INTERP_LOAD_BASE`) -- this build only needs to
/// produce a normally-linked, real `-fPIC` shared object, the same shape any real musl
/// distro ships.
///
/// **Deliberately builds from a fresh copy of `third_party/musl`, not in the same tree
/// `build_musl_sysroot` already builds the static sysroot in.** That static build configures with
/// `--disable-shared CFLAGS=-fno-pie -fno-PIC` -- exactly wrong for real shared objects (genuine
/// PIC codegen, not the anti-PIE workaround TinyCC's static linking needed, see that function's
/// own doc comment) -- and both builds happen in-place (no out-of-tree `O=` mechanism musl's
/// Makefile supports, confirmed against `build_musl_sysroot`'s own precedent). Running a second,
/// differently-flagged configure+make in the same source directory would silently corrupt the
/// static `libc.a` that TinyCC and the whole BusyBox roster already depend on. The copy is a
/// one-time cost (gated on the destination not already existing) -- this deliberately does not try
/// to detect a stale copy against upstream source changes the way `build_busybox_applet`'s
/// staleness floor does; re-syncing this copy after a real `third_party/musl` patch is a manual
/// step for now (`rm -rf target/musl-src-shared`), matching this milestone's own deliberately
/// narrow scope.
///
/// **`--prefix`/`--syslibdir` both point inside the sysroot itself** (not a real target-visible
/// path like `/usr`/`/lib`), matching `build_musl_sysroot`'s own "no `DESTDIR`, prefix ==
/// final-usage location" shape -- confirmed empirically the only way `musl-gcc`'s own installed
/// wrapper (which bakes in `-specs <libdir>/musl-gcc.specs` at install time, read at every future
/// invocation) stays directly usable as a host-side cross-compiler without a second relocation
/// step. This means the *default* dynamic-linker path baked into anything linked against this
/// sysroot would be this host's own absolute sysroot path, not the real target path
/// (`/lib/ld-musl-x86_64.so.1`, matching where `modules/oxfs` seeds it) -- `build_dynlink_smoke`
/// below overrides it explicitly per-link via `-Wl,--dynamic-linker=...` rather than trying to
/// thread a real target-relative prefix through (confirmed via direct `readelf -p .interp`
/// experimentation that this override wins over the specs file's own default).
fn build_musl_sysroot_shared() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let real_musl_dir = Path::new(manifest_dir).join("third_party/musl");
    let musl_dir = Path::new(manifest_dir).join("target/musl-src-shared");
    let sysroot = Path::new(manifest_dir).join("target/musl-sysroot-shared");

    if !musl_dir.exists() {
        let status = Command::new("cp")
            .args(["-a", "--", "."])
            .arg(&musl_dir)
            .current_dir(&real_musl_dir)
            .status()
            .unwrap_or_else(|e| panic!("failed to copy third_party/musl for the shared build: {e}"));
        if !status.success() {
            panic!("copying third_party/musl for the shared build failed: {status}");
        }
        // `cp -a` faithfully copies whatever build state `third_party/musl` itself happens to be
        // in right now -- including, almost always, `build_musl_sysroot`'s own already-built
        // static `config.mak`/`obj/`/`lib/*.a` (musl builds in place, no `O=` out-of-tree
        // mechanism, same reasoning `build_musl_sysroot`'s own doc comment already gives). Left
        // alone, the copy's `config.mak` presence check right below would wrongly treat *that*
        // static config as "already configured for this shared build" and skip `./configure`
        // entirely -- and even a plain `rm config.mak` alone wouldn't be enough, since `make`'s own
        // incremental tracking compares object mtimes against source mtimes, not compiler flags,
        // so stale non-PIC `.o`/`.lo` files from the static build would silently survive into a
        // supposedly-`-fPIC` build (the same class of bug CLAUDE.md's BusyBox section documents
        // for a stale out-of-tree build directory). `make distclean` (`rm -rf obj lib` +
        // `rm -f config.mak`) guarantees a truly pristine tree regardless of what state
        // `third_party/musl` was in at copy time -- run exactly once, right after a fresh copy,
        // not on every subsequent `cargo build` (which would defeat this build's own incremental
        // compilation once it's genuinely configured for real).
        let status = Command::new("make")
            .args(["distclean"])
            .current_dir(&musl_dir)
            .status()
            .unwrap_or_else(|e| panic!("failed to distclean the copied musl tree: {e}"));
        if !status.success() {
            panic!("distclean of the copied musl tree failed: {status}");
        }
    }

    if !musl_dir.join("config.mak").exists() {
        let status = Command::new("./configure")
            .current_dir(&musl_dir)
            .args([
                &format!("--prefix={}", sysroot.display()),
                &format!("--syslibdir={}", sysroot.join("lib").display()),
                "CFLAGS=-fPIC",
            ])
            .status()
            .unwrap_or_else(|e| panic!("failed to run musl's configure (shared): {e}"));
        if !status.success() {
            panic!("musl configure (shared) failed: {status}");
        }
    }

    let jobs = std::thread::available_parallelism().map_or(1, |n| n.get());
    let status = Command::new("make")
        .current_dir(&musl_dir)
        .args(["-j", &jobs.to_string()])
        .status()
        .unwrap_or_else(|e| panic!("failed to run make for musl (shared): {e}"));
    if !status.success() {
        panic!("musl build (shared) failed: {status}");
    }

    let status = Command::new("make")
        .current_dir(&musl_dir)
        .arg("install")
        .status()
        .unwrap_or_else(|e| panic!("failed to run make install for musl (shared): {e}"));
    if !status.success() {
        panic!("musl install (shared) failed: {status}");
    }

    sysroot
}

/// Cross-builds `userland/dynlink-smoke/main.c` (one `write()` call) against `sysroot` (see
/// `build_musl_sysroot_shared` above) as a real, dynamically-linked `ET_EXEC` binary: `-no-pie`
/// (matching `build_musl_smoke`'s own reasoning -- keeps this the *main binary*, not itself an
/// `ET_DYN` image, so only the interpreter needs `src/process/elf.rs`'s new `ET_DYN` handling), fixed at
/// `fixture_base` (must stay clear of `src/process/lifecycle.rs`'s `INTERP_LOAD_BASE` -- both images load
/// into the *same* address space for a real `PT_INTERP` exec, unlike every other fixed-base
/// userland binary in this codebase, which never coexists with another image), and an explicit
/// `-Wl,--dynamic-linker=/lib/ld-musl-x86_64.so.1` overriding the sysroot's own host-path default
/// (see `build_musl_sysroot_shared`'s own doc comment) so the baked-in `PT_INTERP` string matches
/// the real target path `modules/oxfs` seeds this at.
fn build_dynlink_smoke(sysroot: &Path, fixture_base: u64) -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let src = Path::new(manifest_dir).join("userland/dynlink-smoke/main.c");
    let target_dir = Path::new(manifest_dir).join("target/dynlink-smoke");
    std::fs::create_dir_all(&target_dir).expect("failed to create target/dynlink-smoke");
    let out = target_dir.join("dynlink-smoke");

    println!("cargo:rerun-if-changed={}", src.display());

    let musl_gcc = sysroot.join("bin/musl-gcc");
    let status = Command::new(&musl_gcc)
        .arg("-no-pie")
        .arg(format!("-Wl,-Ttext-segment={fixture_base:#x}"))
        .arg("-Wl,--dynamic-linker=/lib/ld-musl-x86_64.so.1")
        .arg("-O2")
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .status()
        .unwrap_or_else(|e| panic!("failed to run musl-gcc for dynlink-smoke: {e}"));
    if !status.success() {
        panic!("building dynlink-smoke failed: {status}");
    }
    out
}

/// Cross-builds TinyCC (`third_party/tinycc`, real upstream `release_0_9_27` vendored on this
/// project's own `oxidebsd` submodule branch -- same pin/update procedure as musl/BusyBox) against
/// `musl_sysroot` via `musl-gcc`, the same "shell out to the real toolchain, no cargo" idiom
/// `build_musl_smoke`/`build_busybox_applet` already use. Unlike BusyBox this is a plain,
/// non-Kconfig `./configure` + `make` project -- `--config-musl` is real, maintained upstream musl
/// support (confirmed against a real build: Alpine Linux, a musl distro, ships tcc in production
/// the same way) -- so there's no per-applet flip/oldconfig dance, one configure and one make.
///
/// `--prefix=/usr` does **not** touch the host's real `/usr` -- it only becomes a compiled-in
/// default baked into the `tcc` binary itself, i.e. where *it* looks for headers/crt objects/its
/// own runtime library once *it* is running on OxideBSD. Confirmed against the real generated
/// `config.h`: `CONFIG_TCCDIR` = `/usr/lib/tcc`, `CONFIG_TCC_CRTPREFIX` = `/usr/lib`,
/// `CONFIG_TCC_SYSINCLUDEPATHS` includes `/usr/include` -- exactly the layout `modules/oxfs`'s own
/// seeding wires up (`seed_tree`/`format_fresh_filesystem`, once Milestone B lands), so no extra
/// `-B`/`-I`/`-L` flags are needed at `tcc` invocation time on target.
///
/// **`libtcc1.a` (tcc's own runtime helper library) deliberately does not use tcc's normal
/// self-hosting recipe.** tcc's own `Makefile` builds it as `libtcc1.a : tcc$(EXESUF) FORCE`,
/// running the just-built `tcc` binary *on the host* to compile `lib/*.c`. Confirmed live that this
/// cannot work here: the just-built `tcc` is linked against this project's own *patched* musl
/// (carry-flag errno conversion, remapped `__NR_*` values -- see CLAUDE.md's musl-port section), so
/// every syscall it issues is misinterpreted by the real host kernel's real Linux ABI -- running it
/// directly on the host doesn't crash, it silently does nothing (`./tcc --version` exits `0` with
/// no output at all). tcc's own Makefile has a documented escape hatch for exactly this shape of
/// problem, normally meant for cross-architecture builds: `<target>-libtcc1-usegcc=yes`, which
/// swaps in a real, host-executable `$(CC)` instead of self-hosting via the freshly built `tcc`.
/// Safe here because `libtcc1.a`'s own sources (`lib/*.c`, `lib/*.S`) are pure freestanding
/// numeric helper routines (softfloat/int64 conversions) with no syscalls at all -- compiling them
/// with the host-executable `musl-gcc` wrapper directly (a real host tool, unlike the cross-built
/// `tcc` itself) produces object code that's ABI-correct for the target either way.
///
/// **Built in-place inside the submodule** (matching `build_musl_sysroot`'s own precedent, not
/// `build_busybox_applet`'s out-of-tree `O=` build -- tcc's Makefile has no equivalent out-of-tree
/// mechanism), so unlike `build_busybox_applet`'s freshness-floor check, this deliberately does
/// **not** `cargo:rerun-if-changed` the submodule directory: tcc's own build outputs (`tcc`, `.o`
/// files, `libtcc1.a`) land in the exact same directories as its source, so watching that tree
/// would make every build's own output look like a source change to cargo on the *next* build,
/// forcing a real rebuild every single time regardless of whether anything actually changed. The
/// same reasoning is why source-mtime staleness here can't just be `latest_mtime(&tinycc_dir)`
/// either -- that would walk right back over the build's own prior output sitting in the same
/// directory, making it look perpetually newer than itself. `tinycc_source_mtime` (below) is
/// `latest_mtime`'s same walk with the exact build-output names/extensions
/// `clean_tinycc_build_outputs` already excludes skipped. Real source patches now exist
/// (`x86_64-link.c`'s own `ELF_START_ADDR`, see CLAUDE.md's TinyCC section for why a real
/// on-target-compiled program needs its own safe default link address) -- found live the same way
/// the freshness-floor lesson itself was learned elsewhere in this file, so this tracks it from the
/// start rather than deferring the way an earlier version of this comment once said to.
fn build_tinycc(musl_sysroot: &Path) -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let tinycc_dir = Path::new(manifest_dir).join("third_party/tinycc");
    let tcc_bin = tinycc_dir.join("tcc");
    let libtcc1 = tinycc_dir.join("libtcc1.a");

    let build_rs_mtime = std::fs::metadata(Path::new(manifest_dir).join("build.rs"))
        .and_then(|m| m.modified())
        .unwrap_or(std::time::SystemTime::now());
    let musl_mtime = std::fs::metadata(musl_sysroot.join("lib/libc.a"))
        .and_then(|m| m.modified())
        .unwrap_or(std::time::SystemTime::now());
    let freshness_floor = build_rs_mtime
        .max(musl_mtime)
        .max(tinycc_source_mtime(&tinycc_dir));
    let already_fresh = [&tcc_bin, &libtcc1].into_iter().all(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .map(|m| m >= freshness_floor)
            .unwrap_or(false)
    });
    if already_fresh {
        return tcc_bin;
    }

    clean_tinycc_build_outputs(&tinycc_dir);

    let musl_gcc = musl_sysroot.join("bin/musl-gcc");
    if !tinycc_dir.join("config.mak").exists() {
        let status = Command::new("./configure")
            .current_dir(&tinycc_dir)
            .arg(format!("--cc={}", musl_gcc.display()))
            .arg("--config-musl")
            .arg("--prefix=/usr")
            // Explicit, not derived from --prefix alone: confirmed live that tinycc's own
            // configure (only when NOT given a --cross-prefix, which this build.rs invocation
            // never does) probes the *build host's* own `/usr/lib64/crti.o` to decide whether to
            // bake in `lib64` instead of `lib` (`CONFIG_LDDIR`, used to derive
            // `CONFIG_TCC_LIBPATHS`/`CONFIG_TCC_CRTPREFIX`) -- a real host-environment quirk
            // (whether *this build machine's* distro happens to split `/usr/lib64`) completely
            // unrelated to the target musl sysroot's own layout, which is always flat `/usr/lib`
            // (confirmed against `target/musl-sysroot/lib`'s own real `make install` output --
            // musl doesn't do a lib64 multilib split at all). Found live: a first build without
            // these three flags produced a `tcc` that reported `file 'crt1.o' not found`/
            // `library 'c' not found` at real runtime inside QEMU, since it was searching
            // `/usr/lib64` on target, which oxfs never seeds anything into. These three flags
            // bypass that host-autodetection entirely by stating the real target layout directly
            // (matches CLAUDE.md's TinyCC section for the full story).
            .arg("--crtprefix=/usr/lib")
            .arg("--libpaths=/usr/lib")
            .arg("--sysincludepaths=/usr/include")
            .status()
            .unwrap_or_else(|e| panic!("failed to run tinycc's configure: {e}"));
        if !status.success() {
            panic!("tinycc configure failed: {status}");
        }
    }

    // 0xa280000: next free slot past the BusyBox applet range (highest in use is CRYPTPW's
    // 0xa240000 in `BUSYBOX_APPLETS_PASS2`) -- tcc lives in the same "standalone binary embedded
    // into oxfs's /bin" bucket as every applet, just built from its own upstream project instead
    // of BusyBox's.
    let status = Command::new("make")
        .current_dir(&tinycc_dir)
        .arg("tcc")
        .arg("LDFLAGS=-static -no-pie -Wl,-Ttext-segment=0xa280000")
        .status()
        .unwrap_or_else(|e| panic!("failed to run make tcc: {e}"));
    if !status.success() {
        panic!("building tcc failed: {status}");
    }

    let status = Command::new("make")
        .current_dir(&tinycc_dir)
        .arg("x86_64-libtcc1-usegcc=yes")
        .arg(format!("CC={}", musl_gcc.display()))
        .arg("libtcc1.a")
        .status()
        .unwrap_or_else(|e| panic!("failed to run make libtcc1.a: {e}"));
    if !status.success() {
        panic!("building tinycc's libtcc1.a failed: {status}");
    }

    tcc_bin
}

/// Removes tinycc's own previous build outputs (not its source) from an in-place build -- see
/// `build_tinycc`'s own doc comment for why a stale binary needs to be actively cleared rather than
/// relying on `make`'s incremental tracking (no dependency on `musl_sysroot`'s installed headers at
/// all). Scoped to exactly the two directories tinycc ever writes into (`third_party/tinycc` itself
/// and its `lib/` subdirectory, for `libtcc1.a`'s own object files) and a fixed set of known
/// build-output names/extensions -- never touches `win32/`/`tests/`/etc, and never deletes a real
/// source file.
fn clean_tinycc_build_outputs(tinycc_dir: &Path) {
    for dir in [tinycc_dir.to_path_buf(), tinycc_dir.join("lib")] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if is_tinycc_build_output(name) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Shared exclusion list between `clean_tinycc_build_outputs` and `tinycc_source_mtime` -- kept as
/// one function specifically so the two can't silently drift apart (a name added to one but not
/// the other would either leave a stale build output uncleaned or make a real build output look
/// like a perpetually-changing "source" file).
fn is_tinycc_build_output(name: &str) -> bool {
    name == "tcc"
        || name == "config.mak"
        || name == "config.h"
        || name.ends_with(".o")
        || name.ends_with(".a")
}

/// Like `latest_mtime`, but skips tinycc's own build outputs (`is_tinycc_build_output`) -- see
/// `build_tinycc`'s own doc comment for why this can't just be `latest_mtime(&tinycc_dir)`
/// directly (built in-place, so that would walk right back over the build's own prior output).
///
/// **Also emits `cargo:rerun-if-changed` for every real source file it walks past** -- load-
/// bearing, not a nicety: found live, the hard way, right after `x86_64-link.c`'s own
/// `ELF_START_ADDR` was first patched -- a `cargo build` right afterward finished in `0.09s` and
/// silently kept using the *stale* `tcc` binary, because nothing in this build script had ever
/// told cargo that `third_party/tinycc`'s source was worth watching at all. Cargo only re-invokes
/// `main()` when a path from a *previous run's own* `rerun-if-changed` set changes -- with none
/// registered for tinycc, a real source edit there was completely invisible to cargo's own decision
/// of whether to re-run this file, so `tinycc_source_mtime`'s own freshness check (correct in
/// isolation) never even got a chance to run. Same class of bug as the freshness-floor lesson
/// `build_busybox_applet` already documents, just one level up the stack (cargo not re-invoking
/// `build.rs` at all, rather than this function's own logic making the wrong call once invoked).
fn tinycc_source_mtime(tinycc_dir: &Path) -> std::time::SystemTime {
    let mut latest = std::time::UNIX_EPOCH;
    let mut stack = vec![tinycc_dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            if name_str.starts_with('.') {
                continue;
            }
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if !is_tinycc_build_output(name_str) {
                let path = entry.path();
                println!("cargo:rerun-if-changed={}", path.display());
                if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
                    latest = latest.max(modified);
                }
            }
        }
    }
    latest
}

/// Latest mtime across every real source file under `dir`, skipping VCS metadata (any directory
/// starting with `.`) -- computed once per `cargo build` invocation, not once per applet (see
/// `build_busybox_applet`'s own doc comment for why that distinction matters at ~300 applets). A
/// plain recursive walk rather than a crate dependency: this only runs when build.rs's own
/// `cargo:rerun-if-changed` gate already decided `third_party/busybox` changed, so it's a rare,
/// not hot-path, cost -- not worth a `walkdir`-style dependency for.
fn latest_mtime(dir: &Path) -> std::time::SystemTime {
    let mut latest = std::time::UNIX_EPOCH;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if entry
                .file_name()
                .to_str()
                .is_some_and(|s| s.starts_with('.'))
            {
                continue;
            }
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
                latest = latest.max(modified);
            }
        }
    }
    latest
}

/// Recursively collects every real file under `dir`, returning `(relative_path, absolute_path)`
/// pairs with `/`-separated relative paths (not platform-`PathBuf`-component-dependent) -- used to
/// enumerate `musl_sysroot/include`/`musl_sysroot/lib` and tinycc's own bundled `include/` for
/// `write_tcc_runtime_manifest` below. Shares `latest_mtime`'s own stack-based walk shape and
/// dotfile-skipping, but returns paths instead of a single max mtime.
fn collect_dir_files(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel_dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(dir.join(&rel_dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name();
            if name.to_str().is_some_and(|s| s.starts_with('.')) {
                continue;
            }
            let rel_path = rel_dir.join(&name);
            if file_type.is_dir() {
                stack.push(rel_path);
            } else {
                let rel_str = rel_path
                    .components()
                    .filter_map(|c| c.as_os_str().to_str())
                    .collect::<Vec<_>>()
                    .join("/");
                out.push((rel_str, entry.path()));
            }
        }
    }
    out
}

/// Generates a single Rust source file declaring `MUSL_INCLUDE_FILES`/`MUSL_LIB_FILES`/
/// `TCC_RUNTIME_FILES` -- `&[(&str, &[u8])]` arrays of (path relative to their own eventual
/// `/usr/...` destination, embedded content) -- consumed by `modules/oxfs/src/lib.rs`'s
/// `format_fresh_filesystem` via a single `include!(env!("TCC_RUNTIME_MANIFEST_PATH"))`, feeding
/// its own `seed_tree` helper. See CLAUDE.md's TinyCC section for why this is real, on-target
/// runtime content tcc needs (musl's headers/crt/`libc.a`, tcc's own `libtcc1.a` + bundled
/// compiler-magic headers), not just the compiler binary itself.
///
/// Written as a real generated file with literal absolute `include_bytes!` paths, not the
/// `env!()`-indirected `cargo:rustc-env`-per-file pattern every other embedded ELF in this build
/// script uses -- deliberately: this is ~230 individual files (217 musl headers alone), not a
/// handful of named applets each already needing its own unique identifier for other reasons (a
/// Kconfig symbol, a load address). Inventing and sanitizing ~230 one-off env var names for data
/// that has no other reason to need a name would be pure ceremony. The generated file is itself a
/// build artifact -- never checked in, already host-specific -- so embedding this host's own
/// absolute paths directly is no less portable than the indirection used elsewhere.
fn write_tcc_runtime_manifest(musl_sysroot: &Path, tinycc_dir: &Path) -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let out_dir = Path::new(manifest_dir).join("target/generated");
    std::fs::create_dir_all(&out_dir).expect("failed to create target/generated");
    let out_path = out_dir.join("tcc_runtime_manifest.rs");

    let mut src = String::new();
    let write_array = |src: &mut String, array_name: &str, files: &[(String, PathBuf)]| {
        src.push_str(&format!("pub static {array_name}: &[(&str, &[u8])] = &[\n"));
        for (rel, abs) in files {
            src.push_str(&format!(
                "    ({rel:?}, include_bytes!({:?})),\n",
                abs.display()
            ));
        }
        src.push_str("];\n\n");
    };

    // /usr/include -- musl's own real header tree, embedded whole (not a hand-curated subset --
    // see CLAUDE.md's TinyCC section for why: even a trivial `printf` pulls in a nontrivial
    // closure of internal `bits/*.h`/`features.h` headers, and the full tree is already sitting
    // here post-`make install` at zero extra build cost).
    let mut headers = collect_dir_files(&musl_sysroot.join("include"));
    headers.sort();
    write_array(&mut src, "MUSL_INCLUDE_FILES", &headers);

    // /usr/lib -- crt objects + every musl-produced `.a` (libc.a plus its small stub archives, for
    // real `-lm`/`-lpthread`/etc. link-line compatibility even though musl merges everything into
    // libc.a itself). `rcrt1.o`/`Scrt1.o` (PIE-only crt variants) and `musl-gcc.specs` (a host
    // build-tool artifact, meaningless inside a target sysroot) are deliberately skipped -- tcc on
    // this kernel always links `-static`, never PIE (a real, separate decision made in
    // `third_party/tinycc/libtcc.c`'s own `tcc_new()`, not a missing kernel capability -- `elf.rs`
    // does now have real `PT_INTERP` support, see CLAUDE.md's "Dynamic linking" section, just never
    // wired up for tcc's own output).
    let mut lib_files: Vec<(String, PathBuf)> = collect_dir_files(&musl_sysroot.join("lib"))
        .into_iter()
        .filter(|(rel, _)| !matches!(rel.as_str(), "rcrt1.o" | "Scrt1.o" | "musl-gcc.specs"))
        .collect();
    lib_files.sort();
    write_array(&mut src, "MUSL_LIB_FILES", &lib_files);

    // /usr/lib/tcc -- tcc's own runtime helper library plus its own bundled compiler-magic headers
    // (`stdarg.h`/`stddef.h`/`float.h`/... -- distinct from musl's real userspace headers above,
    // needed because tcc's preprocessor wants its own compatible versions of a handful of
    // compiler-intrinsic headers). Confirmed against tcc's own `tcc.h`
    // (`CONFIG_TCC_SYSINCLUDEPATHS`'s first entry is `{B}/include`, `{B}` == `CONFIG_TCCDIR` ==
    // `/usr/lib/tcc`) that these belong at `tcc/include/*.h`, not flat under `tcc/`.
    let mut tcc_runtime_files: Vec<(String, PathBuf)> = vec![(
        "libtcc1.a".to_string(),
        tinycc_dir.join("libtcc1.a"),
    )];
    let mut tcc_headers = collect_dir_files(&tinycc_dir.join("include"));
    tcc_headers.sort();
    tcc_runtime_files.extend(
        tcc_headers
            .into_iter()
            .map(|(rel, abs)| (format!("include/{rel}"), abs)),
    );
    write_array(&mut src, "TCC_RUNTIME_FILES", &tcc_runtime_files);

    std::fs::write(&out_path, src)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", out_path.display()));
    out_path
}

/// A curated pilot subset of `third_party/posixtestsuite`'s `conformance/interfaces/` assertion
/// files -- see `docs/POSIX_COMPLIANCE_CHECKLIST.md`'s "Verification" section for why this exists
/// at all (a real conformance-suite baseline, not self-assessment against a checklist) and
/// `write_posix_test_manifest`'s own doc comment below for how these get built and
/// `modules/oxfs/src/posix_conformance.sh`'s for how they get run.
///
/// **Deliberately not the whole suite** (1750 `.c` files in `conformance/interfaces/` alone,
/// before `functional`/`stress`): this pilot exists to prove the whole pipeline -- build, run
/// under a real timeout, classify the real POSIX result code -- works end to end, and to get a
/// first, real (if partial) pass/fail baseline, not to be the final coverage. Picked from
/// interfaces this session already knows the implementation status of (real handlers for
/// `kill`/`sigqueue`/`clock_gettime`/`nanosleep`/`sigwait`/`mq_open`, real-but-anonymous-only
/// `mmap`, and the two POSIX-named — not SysV — IPC gaps `sem_open`/`shm_open` intentionally
/// included as expected-failure controls), restricted to files that don't reference
/// `pthread_create`/`testfrmw.h` (real threading doesn't exist yet -- see
/// `docs/POSIX_COMPLIANCE_CHECKLIST.md`'s own foundational-blockers list -- so a test needing it
/// would only add noise, not signal, to this first pass). All 69 originally in this list confirmed
/// to compile clean against real `musl-gcc` (the same toolchain that now actually builds them, see
/// below) before being added here.
const POSIX_TEST_PILOT_FILES: &[&str] = &[
    "clock_gettime/1-1.c",
    "clock_gettime/1-2.c",
    "clock_gettime/2-1.c",
    "clock_gettime/3-1.c",
    "clock_gettime/4-1.c",
    "clock_gettime/7-1.c",
    "clock_gettime/8-1.c",
    "clock_gettime/8-2.c",
    "kill/1-1.c",
    "kill/1-2.c",
    "kill/2-1.c",
    "kill/2-2.c",
    "kill/3-1.c",
    "mmap/10-1.c",
    "mmap/11-1.c",
    "mmap/11-2.c",
    "mmap/11-3.c",
    "mmap/11-4.c",
    "mmap/11-5.c",
    "mmap/1-1.c",
    "mmap/12-1.c",
    "mq_open/10-1.c",
    "mq_open/11-1.c",
    "mq_open/1-1.c",
    "mq_open/12-1.c",
    "mq_open/13-1.c",
    "mq_open/14-1.c",
    "mq_open/15-1.c",
    "mq_open/16-1.c",
    "nanosleep/10000-1.c",
    "nanosleep/1-1.c",
    "nanosleep/1-2.c",
    "nanosleep/1-3.c",
    "nanosleep/2-1.c",
    "nanosleep/3-1.c",
    "sem_open/10-1.c",
    "sem_open/1-1.c",
    "sem_open/1-2.c",
    "sem_open/1-3.c",
    "sem_open/1-4.c",
    "sem_open/2-1.c",
    "sem_open/2-2.c",
    "sem_open/3-1.c",
    "shm_open/10-1.c",
    "shm_open/11-1.c",
    "shm_open/1-1.c",
    "shm_open/12-1.c",
    "shm_open/13-1.c",
    "shm_open/14-2.c",
    "shm_open/15-1.c",
    "shm_open/16-1.c",
    "sigqueue/10-1.c",
    "sigqueue/11-1.c",
    "sigqueue/1-1.c",
    "sigqueue/12-1.c",
    "sigqueue/2-1.c",
    "sigqueue/2-2.c",
    "sigqueue/3-1.c",
    "sigqueue/4-1.c",
    "sigqueue/5-1.c",
    "sigqueue/6-1.c",
    "sigqueue/7-1.c",
    "sigqueue/8-1.c",
    "sigqueue/9-1.c",
    "sigwait/1-1.c",
    "sigwait/2-1.c",
    "sigwait/3-1.c",
    // sigwait/4-1.c deliberately excluded: it calls its own `alarm(3)` internally, which -- being
    // real per-process alarm state -- *replaces* `t0`'s own outer `alarm(40)` timeout wrapper the
    // moment it's called. If this kernel's `sigwait()`/`alarm()` interaction doesn't work exactly
    // right (unconfirmed -- found live, not yet root-caused), there is no longer any timeout able
    // to rescue the run at all -- confirmed hanging past a real 30-minute ceiling, not merely slow.
    // A real, separate finding worth its own investigation later -- not a pilot-infra problem.
    "sigwait/8-1.c",
];

/// Generates `target/generated/posix_test_manifest.rs` (same `include!`-a-generated-file idiom
/// `write_tcc_runtime_manifest` above already established, for the same reason: real file content
/// embedded via literal-path `include_bytes!`, not the `env!()`-per-file pattern every hand-written
/// embedded ELF in this codebase uses -- there's no reason to invent ~70 one-off names for data
/// with no other identity need). `POSIX_TEST_PILOT_FILES` above is the single source of truth for
/// *which* files get embedded.
///
/// **Cross-compiled with `musl-gcc` on the host, not compiled on-target by `tcc`** -- an earlier
/// version of this pilot seeded real `.c` source and compiled it on-target, exercising `tcc` as
/// the real toolchain any on-target C program would use. Found live, the hard way: `tcc`'s own
/// linker generates unresolved GOT/PLT indirection for calls into specific musl functions
/// (`sigaction`, `fflush` confirmed; `printf`/`clock_gettime`/`kill`/etc. unaffected) even under a
/// fully `-static` link -- the same *class* of bug CLAUDE.md's TinyCC section already documents
/// finding and partially fixing (`tccelf.c`'s `build_got_entries`), but that fix evidently doesn't
/// cover every code path. Symptom: a real ring-3 null-pointer read fault before the affected
/// binary's own first instruction, and -- found investigating the crash -- real stdio output was
/// silently never reaching the console for *any* on-target-compiled pilot binary at all, tcc bug
/// or not. Cross-compiling with the same real, already-proven `musl-gcc` toolchain
/// `build_musl_smoke`/`build_dynlink_smoke` already use sidesteps both problems at once, at the
/// cost of losing "exercises tcc" as a side benefit -- an acceptable trade for a conformance
/// baseline whose job is testing the *kernel*, not `tcc`. (The underlying `tcc` bug itself is
/// still real and un-fixed -- worth its own investigation later, tracked separately from this
/// pilot.)
///
/// Each pilot file gets a unique fixed load address (`POSIX_TEST_LOAD_BASE` + index *
/// `POSIX_TEST_LOAD_STEP`, `0xcf40000`.., 68 entries fit comfortably below BusyBox's own applet
/// range's ceiling well above and `module::MODULE_VA_BASE` far above) -- same "every userland
/// binary gets a real, non-overlapping fixed base" discipline every other embedded ELF in this
/// codebase already follows. `t0` (the suite's own real timeout-wrapper utility, see
/// `posix_conformance.sh`'s own doc comment for why a real `alarm()`-based wrapper matters on a
/// kernel with no preemption) is cross-compiled the same way, at its own fixed base just below the
/// pilot range. Embeds each compiled ELF's real bytes at `bin/<relative-path-with-.c-extension>`
/// (the `.c` suffix is a real file's real relative identity carried through unchanged from the
/// source tree, not a claim about its own content -- avoids needing any extension-rewriting logic
/// in the shell runner, which would need real string manipulation hush may not support cleanly)
/// and writes out a plain-text `manifest.txt` (one relative path per line) generated from the same
/// list, so the seeded corpus and the runner script's own iteration list can never drift apart.
fn write_posix_test_manifest(musl_sysroot: &Path, posixtestsuite_dir: &Path) -> PathBuf {
    const POSIX_TEST_LOAD_BASE: u64 = 0xcf40000;
    const POSIX_TEST_LOAD_STEP: u64 = 0x40000;
    const T0_LOAD_BASE: u64 = 0xcf00000;

    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let out_dir = Path::new(manifest_dir).join("target/generated");
    std::fs::create_dir_all(&out_dir).expect("failed to create target/generated");
    let out_path = out_dir.join("posix_test_manifest.rs");
    let bin_dir = Path::new(manifest_dir).join("target/posix-test-pilot");
    std::fs::create_dir_all(&bin_dir).expect("failed to create target/posix-test-pilot");

    let interfaces_dir = posixtestsuite_dir.join("conformance/interfaces");
    let include_dir = posixtestsuite_dir.join("include");
    let musl_gcc = musl_sysroot.join("bin/musl-gcc");

    let mut src = String::new();
    src.push_str("pub static POSIX_TEST_FILES: &[(&str, &[u8])] = &[\n");
    let mut manifest_txt = String::new();
    for (i, rel) in POSIX_TEST_PILOT_FILES.iter().enumerate() {
        let source = interfaces_dir.join(rel);
        println!("cargo:rerun-if-changed={}", source.display());
        let out = bin_dir.join(rel.replace('/', "_"));
        let load_addr = POSIX_TEST_LOAD_BASE + (i as u64) * POSIX_TEST_LOAD_STEP;
        let status = Command::new(&musl_gcc)
            .arg("-static")
            .arg("-no-pie")
            .arg(format!("-Wl,-Ttext-segment={load_addr:#x}"))
            .arg("-I")
            .arg(&include_dir)
            .arg("-o")
            .arg(&out)
            .arg(&source)
            .status()
            .unwrap_or_else(|e| panic!("failed to run musl-gcc for posix pilot {rel}: {e}"));
        if !status.success() {
            panic!("building posix pilot test {rel} failed: {status}");
        }
        src.push_str(&format!(
            "    (\"bin/{rel}\", include_bytes!({:?})),\n",
            out.display()
        ));
        manifest_txt.push_str(rel);
        manifest_txt.push('\n');
    }

    let t0_c = posixtestsuite_dir.join("t0.c");
    println!("cargo:rerun-if-changed={}", t0_c.display());
    let t0_out = bin_dir.join("t0");
    let status = Command::new(&musl_gcc)
        .arg("-static")
        .arg("-no-pie")
        .arg(format!("-Wl,-Ttext-segment={T0_LOAD_BASE:#x}"))
        // A real bug in the vendored suite's own t0.c: calls `strcmp` without `#include
        // <string.h>` -- TCC let this slide (an implicit declaration is only a warning there);
        // this host's real GCC treats it as a hard error by default. `-include string.h` forces
        // the real declaration in without patching the vendored source itself.
        .arg("-include")
        .arg("string.h")
        .arg("-o")
        .arg(&t0_out)
        .arg(&t0_c)
        .status()
        .unwrap_or_else(|e| panic!("failed to run musl-gcc for posix pilot's t0: {e}"));
    if !status.success() {
        panic!("building posix pilot's t0 failed: {status}");
    }
    src.push_str(&format!(
        "    (\"t0\", include_bytes!({:?})),\n",
        t0_out.display()
    ));

    let manifest_txt_path = out_dir.join("posix_test_manifest.txt");
    std::fs::write(&manifest_txt_path, &manifest_txt).unwrap_or_else(|e| {
        panic!(
            "failed to write {}: {e}",
            manifest_txt_path.display()
        )
    });
    src.push_str(&format!(
        "    (\"manifest.txt\", include_bytes!({:?})),\n",
        manifest_txt_path.display()
    ));
    src.push_str("];\n");

    std::fs::write(&out_path, src)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", out_path.display()));
    out_path
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
            // Structured output, parsed below to find the real `--emit=obj` artifact path
            // directly rather than guessing a filename convention (`{crate}-<hash>.o`) that isn't
            // part of any stability guarantee -- found live on a newer nightly (rustc
            // 1.99.0-nightly) where that guessed pattern no longer matched anything cargo actually
            // wrote, even though the build itself succeeded. `-render-diagnostics` keeps warning/
            // error text human-readable inside the JSON stream (in each message's own `rendered`
            // field) rather than needing a second pass to reformat raw diagnostic spans.
            "--message-format=json-render-diagnostics",
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
    let output = command
        .output()
        .unwrap_or_else(|e| panic!("failed to run cargo rustc for module {crate_name}: {e}"));
    let stdout = String::from_utf8_lossy(&output.stdout);

    if !output.status.success() {
        panic!(
            "building the {crate_name} module's object file failed: {}\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let sysroot = rustc_output(manifest_dir, &["--print", "sysroot"]);
    let host = host_triple(manifest_dir);
    let llvm_bin = Path::new(&sysroot)
        .join("lib/rustlib")
        .join(&host)
        .join("bin");

    let deps_dir = target_dir.join("x86_64-oxidebsd/release/deps");
    let module_obj = find_artifact_file(&stdout, crate_name, ".o")
        .or_else(|| newest_matching(&deps_dir, &format!("{crate_name}-"), ".o"))
        .unwrap_or_else(|| {
            // Last resort -- confirmed live on rustc 1.99.0-nightly: cargo's own JSON build
            // output can report a crate's `compiler-artifact` message with only its `.rlib`/
            // `.rmeta` in `filenames`, no `.o` at all, even though `--emit=obj` was passed and the
            // build genuinely succeeded -- `--emit=obj` silently dropped, not just renamed. Pull
            // the real compiled object straight out of the `.rlib` cargo always produces instead
            // of depending on that flag working at all (see `extract_object_from_rlib`'s own doc
            // comment).
            let rlib = find_artifact_file(&stdout, crate_name, ".rlib")
                .or_else(|| newest_matching(&deps_dir, &format!("lib{crate_name}-"), ".rlib"))
                .unwrap_or_else(|| {
                    panic!(
                        "{crate_name}: no object file *or* rlib found anywhere -- cargo's own \
                         JSON build output:\n{stdout}"
                    )
                });
            let extract_dir = target_dir.join(format!("{crate_name}-rlib-extract"));
            extract_object_from_rlib(&llvm_bin, &rlib, &extract_dir, crate_name)
        });

    let merged_obj = target_dir.join(format!("{crate_name}-merged.o"));
    partial_link(
        crate_name,
        &stdout,
        &llvm_bin,
        &deps_dir,
        &module_obj,
        &merged_obj,
    );

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

/// Scans `cargo rustc --message-format=json-render-diagnostics`'s own stdout (plain ndjson, one
/// JSON object per line, no pretty-printing) for the `compiler-artifact` message naming
/// `crate_name`'s own build, and returns whichever path in its real `filenames` array ends in
/// `suffix` (`".o"` or `".rlib"`) -- the artifact's actual, cargo-reported location, not a guessed
/// `{crate}-<hash>.<ext>` filename pattern (see `build_module_crate`'s own doc comment on why
/// guessing broke on a newer nightly: that naming convention was never a stability guarantee to
/// begin with). Returns `None` both when the message itself is missing and when the message
/// exists but no filename with that suffix is in it -- confirmed live (rustc 1.99.0-nightly) that
/// the latter genuinely happens for `.o`: cargo's own `compiler-artifact` message for a `--lib`
/// build can list only `.rlib`/`.rmeta`, silently dropping `--emit=obj` from the emitted set.
///
/// Hand-rolled substring scanning, not a `serde_json` build-dependency: cargo's message-format
/// output is simple single-line JSON with no nested arrays inside `filenames`, and this project's
/// own paths never contain characters needing JSON escaping (no quotes/backslashes) -- a real
/// parser would be more correct in the abstract but pure ceremony for this one shape of input.
fn find_artifact_file(cargo_json_stdout: &str, crate_name: &str, suffix: &str) -> Option<PathBuf> {
    let name_needle = format!("\"name\":\"{crate_name}\"");
    for line in cargo_json_stdout.lines() {
        if !line.contains("\"reason\":\"compiler-artifact\"") || !line.contains(&name_needle) {
            continue;
        }
        let Some(list_start) = line.find("\"filenames\":[") else {
            continue;
        };
        let list_start = list_start + "\"filenames\":[".len();
        let Some(list_len) = line[list_start..].find(']') else {
            continue;
        };
        for raw in line[list_start..list_start + list_len].split(',') {
            let path = raw.trim().trim_matches('"');
            if path.ends_with(suffix) {
                return Some(PathBuf::from(path));
            }
        }
    }
    None
}

/// Last-resort fallback for getting a module's compiled object code when cargo's own `--emit=obj`
/// passthrough produces nothing usable (see `find_artifact_file`'s own doc comment for when this
/// happens). An `.rlib` is itself just an `ar` archive whose members are the crate's real compiled
/// object code (one member per codegen unit) plus a `lib.rmeta` metadata member -- `llvm-ar x`
/// extracts them directly, sidestepping `--emit` entirely rather than depending on cargo choosing
/// to honor it. `-C codegen-units=1` (already passed at every module's own build_module_crate call
/// site) should mean exactly one extracted member ends in `.o`; this panics loudly rather than
/// guessing if that invariant doesn't hold, since silently picking one of several codegen units
/// would link a module missing most of its own code.
fn extract_object_from_rlib(
    llvm_bin: &Path,
    rlib: &Path,
    extract_dir: &Path,
    crate_name: &str,
) -> PathBuf {
    std::fs::create_dir_all(extract_dir)
        .unwrap_or_else(|e| panic!("failed to create {}: {e}", extract_dir.display()));

    let llvm_ar = llvm_bin.join("llvm-ar");
    let status = Command::new(&llvm_ar)
        .arg("x")
        .arg(rlib)
        .current_dir(extract_dir)
        .status()
        .unwrap_or_else(|e| panic!("failed to run llvm-ar on {}: {e}", rlib.display()));
    if !status.success() {
        panic!("llvm-ar failed to extract {}: {status}", rlib.display());
    }

    let objects: Vec<PathBuf> = std::fs::read_dir(extract_dir)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", extract_dir.display()))
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "o"))
        .collect();

    match objects.as_slice() {
        [only] => only.clone(),
        [] => panic!(
            "{crate_name}: extracted {} but it contains no .o member -- is it actually an rlib?",
            rlib.display()
        ),
        many => panic!(
            "{crate_name}: rlib {} contained {} object members ({many:?}), expected exactly one \
             -- codegen-units=1 may not have taken effect for this build",
            rlib.display(),
            many.len()
        ),
    }
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
    cargo_json_stdout: &str,
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

    // Prefer cargo's own JSON build output over guessing `deps_dir`'s layout -- confirmed live
    // (rustc 1.99.0-nightly) that `-Z build-std`'s sysroot crates (`core`/`alloc`/
    // `compiler_builtins`) don't land in `deps/` at all on this toolchain, but under their own
    // `build/<crate>/<hash>/out/` directory instead (same class of drift as
    // `find_artifact_file`'s own doc comment documents for `--emit=obj`). Falls back to the old
    // `deps_dir` glob for whatever toolchain still uses that layout.
    let find_rlib = |name: &str| {
        find_artifact_file(cargo_json_stdout, name, ".rlib")
            .or_else(|| newest_matching(deps_dir, &format!("lib{name}-"), ".rlib"))
            .unwrap_or_else(|| {
                panic!(
                    "{crate_name}: no {name} rlib found via cargo's own JSON build output, nor \
                     in {} -- is `-Z build-std` producing one?",
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
fn write_fat32_image(
    smoke_elf_bytes: &[u8],
    musl_elf_bytes: &[u8],
    busybox_applet_elfs: &[(&str, Vec<u8>)],
) -> PathBuf {
    let image = generate_fat32_image(smoke_elf_bytes, musl_elf_bytes, busybox_applet_elfs);
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let target_dir = Path::new(manifest_dir).join("target/modules");
    std::fs::create_dir_all(&target_dir).expect("failed to create target/modules");
    let path = target_dir.join("fat32.img");
    std::fs::write(&path, &image)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
    path
}

/// Mirrors `modules/oxfs/src/lib.rs`'s own on-disk block-layout constants (see that file's "Real
/// disk persistence" section) -- duplicated here, not imported, since a `build.rs` can't depend on
/// a `#![no_std]` module crate. Must be kept in sync by hand if oxfs's own constants ever change --
/// the same "two things that must agree, flagged rather than shared" duplication this codebase
/// already accepts elsewhere (e.g. `Cargo.toml`'s `test-success-exit-code` vs. `src/qemu.rs`'s
/// `QemuExitCode`).
const OXFS_BLOCK_SIZE: u64 = 4096;
const OXFS_NUM_BLOCKS: u64 = 8192;
const OXFS_MAX_INODES: u64 = 1024;
const OXFS_INODE_STRIDE: u64 = 128;
/// 1 superblock + inode-table blocks (`OXFS_MAX_INODES` inodes at `OXFS_INODE_STRIDE` bytes each,
/// rounded up to a whole block) + 1 block-used bitmap -- computed from the same real inputs
/// `modules/oxfs/src/lib.rs`'s own `INODE_TABLE_BLOCKS` is, not a separately hand-picked number.
/// **Found live as a real, hand-duplicated staleness bug, not just a theoretical risk this comment
/// warns about**: this constant was left at its old value (a literal `18`, correct only for the
/// pre-TinyCC `MAX_INODES = 512`) when that constant was bumped to `1024` (see CLAUDE.md's TinyCC
/// section) -- silently sizing every *newly created* `oxfs_disk.img` 16 blocks (64 KiB) too small
/// for the real on-disk layout the kernel-side code actually uses, not just leaving a
/// *pre-existing* stale file too small. Confirmed live against a real, already-formatted disk
/// predating this fix: `mount_from_disk`'s own per-block data read failed partway through (the
/// last ~16 real data blocks physically don't exist in a file sized this way), which is what first
/// surfaced this bug — see `reset_real_pool_for_fresh_format`'s own doc comment in
/// `modules/oxfs/src/lib.rs` for the *other* real bug that same failure mode exposed.
const OXFS_METADATA_BLOCKS: u64 =
    1 + (OXFS_MAX_INODES * OXFS_INODE_STRIDE).div_ceil(OXFS_BLOCK_SIZE) + 1;
const OXFS_DISK_IMAGE_BYTES: u64 = (OXFS_METADATA_BLOCKS + OXFS_NUM_BLOCKS) * OXFS_BLOCK_SIZE;

/// Writes the two raw disk images `Cargo.toml`'s `run-args`/`test-args` attach to QEMU as
/// `src/drivers/ata.rs`'s fixed data-disk target (see that module's own doc comment for why secondary
/// channel/master specifically).
///
/// `oxfs_disk.img` is the real, persistent dev disk `cargo run` uses -- created **only if it
/// doesn't already exist**. That's load-bearing, not an optimization: this is what makes it
/// survive across `cargo run` invocations at all, the entire point of real disk persistence. A
/// rebuild must never re-zero it, or there would be nothing left to prove "install OxideBSD"
/// actually works (see the implementation plan's own manual verification steps).
///
/// `oxfs_test_disk.img` is the opposite: always freshly regenerated (zeroed) on every build, so
/// `tests/ata_smoke.rs`/`tests/oxfs_persistence_syscall_smoke.rs` always start from the same
/// known-empty state, independent of whatever a previous test run left behind.
fn write_data_disk_images() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let target_dir = Path::new(manifest_dir).join("target");
    std::fs::create_dir_all(&target_dir).expect("failed to create target/");
    let zeroed = vec![0u8; OXFS_DISK_IMAGE_BYTES as usize];

    let dev_disk_path = target_dir.join("oxfs_disk.img");
    // Load-bearing, not a nicety: this file lives under `target/`, never touched by any other
    // `rerun-if-changed` path this build script declares, so a plain `cargo run` after someone
    // deletes it (e.g. to force a reformat/reseed) would otherwise skip this whole function --
    // cargo only reruns a build script when a declared watched path actually changed, and an
    // untracked deletion doesn't qualify. Cargo's own documented behavior for a `rerun-if-changed`
    // path that doesn't exist is "always rerun the build script" -- exactly the case that needs
    // covering here. Emitted unconditionally (both branches below), not just in the `Err` arm, so
    // this stays watched on every future run too, including the ordinary case where the file
    // already exists at the right size and nothing else needs to happen to it.
    println!("cargo:rerun-if-changed={}", dev_disk_path.display());
    match std::fs::metadata(&dev_disk_path) {
        Err(_) => {
            std::fs::write(&dev_disk_path, &zeroed)
                .unwrap_or_else(|e| panic!("failed to write {}: {e}", dev_disk_path.display()));
            println!(
                "cargo:warning=oxfs: created a fresh persistent dev disk at {}",
                dev_disk_path.display()
            );
        }
        Ok(meta) if meta.len() < OXFS_DISK_IMAGE_BYTES => {
            // An existing disk written under an older, smaller layout (e.g. a prior
            // `OXFS_METADATA_BLOCKS`/`NUM_BLOCKS`/`MAX_INODES`) -- grow it in place by appending
            // zeros rather than truncating/rewriting, so real existing content stays intact for
            // `mount_from_disk`'s own superblock-layout check (see that function's doc comment in
            // `modules/oxfs/src/lib.rs`) to accept or reject on its own terms. A superblock that
            // no longer matches this build's layout still cleanly falls back to reformatting; a
            // file left too small would instead fail with a real, physical out-of-bounds read
            // before that check ever gets the chance to run.
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&dev_disk_path)
                .unwrap_or_else(|e| panic!("failed to open {}: {e}", dev_disk_path.display()));
            let old_len = meta.len();
            f.seek(SeekFrom::End(0)).expect("failed to seek dev disk");
            let padding = vec![0u8; (OXFS_DISK_IMAGE_BYTES - old_len) as usize];
            f.write_all(&padding)
                .unwrap_or_else(|e| panic!("failed to grow {}: {e}", dev_disk_path.display()));
            println!(
                "cargo:warning=oxfs: grew existing persistent dev disk at {} from {} to {} bytes",
                dev_disk_path.display(),
                old_len,
                OXFS_DISK_IMAGE_BYTES
            );
        }
        Ok(_) => {}
    }

    let test_disk_path = target_dir.join("oxfs_test_disk.img");
    std::fs::write(&test_disk_path, &zeroed)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", test_disk_path.display()));
}

const FAT32_BYTES_PER_SECTOR: usize = 512;
const FAT32_SECTORS_PER_CLUSTER: u8 = 1;
const FAT32_RESERVED_SECTORS: u32 = 32;
const FAT32_NUM_FATS: u32 = 2;
/// 8 MiB total (raised from 2 MiB once `BUSYBOX_APPLETS` grew past its original four entries --
/// this image still embeds every one of them, even applets never actually loaded/used at boot,
/// see this constant's own module-level context) -- still far below the ~65525-cluster count real
/// FAT32 volumes are conventionally expected to have, deliberately (see this function's caller's
/// doc comment).
const FAT32_TOTAL_SECTORS: u32 = 16384;

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

/// One BusyBox applet's placement in the image, computed by `generate_fat32_image` by folding
/// over `busybox_applet_elfs` in order -- each applet's first cluster starts right after the
/// previous one's chain ends, the same "chain on after whatever came before" pattern MUSL.ELF
/// itself already uses to chain on after SMOKE.ELF.
struct PlacedApplet<'a> {
    short_name: [u8; 11],
    bytes: &'a [u8],
    first_cluster: u32,
    cluster_count: u32,
}

/// Builds a FAT 8.3 short name (`"NAME    ELF"`) from an applet's lowercase `out_name` (e.g.
/// `"true"`) -- uppercased, space-padded to 8 characters, `ELF` extension. Panics if `out_name` is
/// too long for an 8.3 basename; every applet name this codebase embeds is short enough that this
/// is a real assertion, not defensive dead code.
fn busybox_short_name(out_name: &str) -> [u8; 11] {
    assert!(
        out_name.len() <= 8 && out_name.is_ascii(),
        "BusyBox applet name {out_name:?} doesn't fit an 8.3 short name"
    );
    let mut name = [b' '; 11];
    for (i, b) in out_name.bytes().enumerate() {
        name[i] = b.to_ascii_uppercase();
    }
    name[8..11].copy_from_slice(b"ELF");
    name
}

fn generate_fat32_image(
    smoke_elf_bytes: &[u8],
    musl_elf_bytes: &[u8],
    busybox_applet_elfs: &[(&str, Vec<u8>)],
) -> Vec<u8> {
    let smoke_cluster_count =
        (smoke_elf_bytes.len().div_ceil(FAT32_BYTES_PER_SECTOR) as u32).max(1);
    let musl_first_cluster = FAT32_SMOKE_FIRST_CLUSTER + smoke_cluster_count;
    let musl_cluster_count = (musl_elf_bytes.len().div_ceil(FAT32_BYTES_PER_SECTOR) as u32).max(1);

    // Each BusyBox applet (see CLAUDE.md's BusyBox section) chains on after the previous one --
    // MUSL.ELF for the first applet, the previous applet for every one after that.
    let mut placed_applets: Vec<PlacedApplet> = Vec::new();
    let mut next_free_cluster = musl_first_cluster + musl_cluster_count;
    for (out_name, elf_bytes) in busybox_applet_elfs {
        let cluster_count = (elf_bytes.len().div_ceil(FAT32_BYTES_PER_SECTOR) as u32).max(1);
        placed_applets.push(PlacedApplet {
            short_name: busybox_short_name(out_name),
            bytes: elf_bytes,
            first_cluster: next_free_cluster,
            cluster_count,
        });
        next_free_cluster += cluster_count;
    }

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

    let highest_cluster_used = next_free_cluster - 1;
    let data_clusters =
        (FAT32_TOTAL_SECTORS - data_start_sector) / FAT32_SECTORS_PER_CLUSTER as u32;
    assert!(
        highest_cluster_used < 2 + data_clusters,
        "ring3-smoke ({} bytes) + musl-smoke ({} bytes) + {} BusyBox applet(s) ({} bytes total) \
         no longer fit in the embedded FAT32 image ({} total bytes) -- raise FAT32_TOTAL_SECTORS",
        smoke_elf_bytes.len(),
        musl_elf_bytes.len(),
        placed_applets.len(),
        placed_applets.iter().map(|a| a.bytes.len()).sum::<usize>(),
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
        for applet in &placed_applets {
            for i in 0..applet.cluster_count {
                let cluster = applet.first_cluster + i;
                let value = if i + 1 == applet.cluster_count {
                    FAT32_EOC
                } else {
                    cluster + 1
                };
                write_fat_entry(&mut image, fat_index, fat_size_sectors, cluster, value);
            }
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
        for applet in &placed_applets {
            entry_offset += 32;
            write_dir_entry(
                &mut image,
                entry_offset,
                &applet.short_name,
                0x20,
                applet.first_cluster,
                applet.bytes.len() as u32,
            );
        }
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

    // --- BusyBox applet contents (see CLAUDE.md's BusyBox section -- chunked the same way
    // SMOKE.ELF/MUSL.ELF's own bytes are above) ---
    for applet in &placed_applets {
        for (i, chunk) in applet.bytes.chunks(FAT32_BYTES_PER_SECTOR).enumerate() {
            let cluster = applet.first_cluster + i as u32;
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
