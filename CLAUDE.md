# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

OxideBSD is a 100% Rust-based BSD-like operating system. Phase 1 of `ROADMAP.md` (a running,
interactive kernel) is done: GDT/TSS/IDT with a dedicated double-fault stack, fatal exceptions
reboot the machine, PIC-driven hardware interrupts with a timer tick and a PS/2 keyboard IRQ
handler (decodes scancodes, echoes typed characters, and feeds a small kernel-side stdin buffer —
see "Interactive shell" below), a VGA text-mode console mirroring serial output, and a heap
allocator backed by bootloader-provided paging. Phase 2 is well underway: the kernel can build a
*separate* address space (`src/address_space.rs`), load a static ELF64 binary into it
(`src/elf.rs`), jump into ring 3 (`src/usermode.rs`), and user-mode code can call back into the
kernel via a single native, BSD-style syscall ABI (`src/syscall.rs`: `SYSCALL`/`SYSRETQ`,
carry-flag error signaling, the traditional BSD/x86-Unix convention). A first genuinely interactive
userland program, `userland/stsh/` ("stupidshell"), now runs by default — see "Interactive shell"
below.

**This ABI used to be two independent, deliberately different mechanisms**: this native one over
`int 0x80`, and a separate Linux-compatible `SYSCALL`/`SYSRET` path (`src/linux_syscall.rs`,
negative-`RAX` error signaling) aimed at eventually running unmodified musl/BusyBox binaries. That
plan changed: rather than making the kernel Linux-syscall-compatible, musl is instead being ported
to speak *this* native ABI directly (patched on a fork, see "musl port" below) — so the native ABI
now owns the `SYSCALL`/`SYSRETQ` mechanism outright, and `src/linux_syscall.rs`/
`userland/linux-syscall-smoke/` are gone, having already served their purpose of proving the
mechanism works at all.

The kernel also has a real **dynamic module loader** (`src/module.rs` — see "Dynamic kernel
modules" below): it relocates independently-compiled `#![no_std]` code into the running kernel at
boot, resolves the handful of symbols that code references against a small hand-curated kernel API,
and calls the module's `module_init`. The native ABI's syscall-number → handler dispatch table
(`src/syscall.rs`) is no longer a hardcoded `match` — it's populated by one such module,
`modules/native_abi/`. A second module, `modules/fat32/` (see "FAT32 filesystem module" below),
implements a basic FAT32 filesystem — parsed from a build-time-generated, embedded in-memory disk
image, since no real block device driver exists yet — with read and write support (including
subdirectories), registering `SYS_OPEN`/`SYS_CLOSE`/`SYS_CHDIR`/`SYS_MKDIR` and, via a small
kernel-owned fd registry (`src/fd.rs`), extending `SYS_READ`/`SYS_WRITE` to real files. `stsh`'s
`cat`/`write`/`cd`/`ls`/`mkdir` commands exercise this end to end.

There is also a real **process abstraction and cooperative scheduler** (`src/process.rs`,
`src/scheduler.rs`, `src/context_switch.rs` — see "Process abstraction, scheduler, and
fork/exec/wait" below): a dynamically-allocated process table, kernel-thread-style context
switching between per-process kernel stacks, and `fork`/`execve`/`wait4`/`getpid` over the native
ABI. `stsh` genuinely runs other programs — any command that isn't a recognized built-in is
`fork`+`execve`+`wait`ed as a real child process, resolved through the same FAT32 filesystem
`cat`/`write` already use.

**A real, unmodified-above-the-syscall-layer musl static binary now runs** (see "musl port"
below): `userland/musl-smoke/`, built against a vendored, patched fork of musl
(`third_party/musl`), `execve`'d by `stsh` (`MUSL.ELF` in the embedded FAT32 image) exactly like
any other program. Getting there needed three real, previously-latent kernel bugs found and fixed
by actually running it (not caught by any test or lint): the initial process stack never carried a
real `argv`/`envp`/auxiliary vector (`src/user_stack.rs`, new); this kernel never enabled SSE at
the hardware level at all (`src/fpu.rs`, new); and a new `SYS_WRITEV` syscall was needed because
musl's entire stdio write path goes through `writev`, not plain `write` — see "musl port" for the
full story of each.

See "User-mode execution", "Syscall ABI", "musl port", "Interactive shell", "Dynamic kernel
modules", "FAT32 filesystem module", and "Process abstraction, scheduler, and fork/exec/wait"
below for the current, deliberate limits (`sys_write`/`sys_read` don't validate their pointers, no
line editing beyond backspace/Ctrl+C/Ctrl+D in the shell, no module unload/reload, FAT32 writes
don't persist across reboot, no preemptive scheduling, no copy-on-write fork, no frame
deallocation anywhere, and — for the musl port — no `envp` passthrough across `execve`, `open`/
`execve`'s argument-convention mismatches with real musl calls of the same name, and several
syscalls musl's fuller startup would need still unimplemented). A *real* filesystem (backed by an
actual block device, not an embedded image) doesn't exist yet. Architecture decisions for
remaining subsystems have not been made and should be discussed with the user before large
structural commitments are made.

Target architecture is x86_64 only for now.

## Toolchain

- Requires the `nightly` Rust channel — pinned via `rust-toolchain.toml` (rustup will fetch it
  automatically). Several unstable features are load-bearing: `-Z build-std` (the kernel builds
  `core`/`alloc`/`compiler_builtins` from source for the custom target — there is no prebuilt std
  for `x86_64-oxidebsd.json`), `-Z json-target-spec`, and `-Z panic-abort-tests`.
- Requires the `bootimage` cargo subcommand (`cargo install bootimage`) and `qemu-system-x86_64`
  on `PATH`. `bootimage` combines the compiled kernel ELF with the `bootloader` crate (v0.9, the
  older BIOS-image-generation API — not the newer v0.11+ artifact-dependency API) into a bootable
  disk image.
- `.cargo/config.toml` sets the default build target to `x86_64-oxidebsd.json` (a custom target
  spec in the repo root) and sets `runner = "bootimage runner"`, so `cargo run`/`cargo test`
  transparently build a bootimage and launch it in QEMU.

## Commands

- Build kernel ELF only: `cargo build`
- Build bootable disk image: `cargo bootimage`
- Boot the kernel in QEMU (serial output goes to stdio): `cargo run`
- Run all tests (lib unit tests + `tests/basic_boot.rs`, each booted individually in QEMU): `cargo test`
- Run one integration test target: `cargo test --test basic_boot`
- Lint: `cargo clippy`
- Format: `cargo fmt`

`cargo build`/`cargo run`/`cargo test`/`cargo clippy`/`cargo fmt` at the repo root only ever
target the `oxidebsd` package, even though it's now a workspace — the `userland/*` crates and the
`modules/*` crates (`modules/hello`, `modules/native_abi`, `modules/fat32` — see "Dynamic kernel
modules" below) are all separate members cargo doesn't build by default in a "root package"
workspace like this one; the root package's own `build.rs` cross-builds all of them as a side
effect of building `oxidebsd` (see "User-mode execution" and "Dynamic kernel modules" below). To
build/lint/format one of them directly: append `--manifest-path <userland-or-modules>/<name>/
Cargo.toml` (and, to avoid the target-directory-locking gotcha described below, `--target-dir
target/userland` or `--target-dir target/modules` as appropriate). `modules/fat32/` additionally
needs `FAT32_IMAGE_PATH` set in the environment when built this way (its own `include_bytes!`
depends on it, normally supplied by the root `build.rs`'s generated image — see "Dynamic kernel
modules" below) — e.g. `FAT32_IMAGE_PATH=$(pwd)/target/modules/fat32.img cargo clippy
--manifest-path modules/fat32/Cargo.toml --target-dir target/modules` (after at least one full
`cargo build` has generated that file).

There is no `cargo check`/`cargo test` fast path that skips QEMU — every test target is its own
bootable kernel image that QEMU actually boots, so `cargo test` is slow (each target rebuilds a
bootimage and launches an emulator instance).

## Test architecture

There is no libtest/`#[test]` — the kernel is `no_std` and has no OS to run a normal test binary
under, so tests instead boot in QEMU and self-report:

- `src/qemu.rs`: writes to the `isa-debug-exit` QEMU device (port `0xf4`) to make QEMU exit with a
  code that encodes pass/fail. `test-success-exit-code` in `Cargo.toml` (`33`) must stay in sync
  with `QemuExitCode::Success` — QEMU maps a written value `v` to process exit code `(v << 1) | 1`.
- `src/serial.rs`: a hand-rolled 16550 UART (COM1) driver, write-only, used for all kernel/test
  output — read via `-serial stdio` (set in `Cargo.toml`'s `[package.metadata.bootimage]`).
- `src/lib.rs`: defines the shared `no_std` test scaffolding (`test_runner`, `test_panic_handler`,
  `hlt_loop`) built on the nightly `custom_test_frameworks` feature (`#[test_case]`,
  `#![test_runner(...)]`, `#![reexport_test_harness_main = "test_main"]`). It also boots itself
  under `#[cfg(test)]` via its own `entry_point!`, so `cargo test --lib` runs any `#[test_case]`s
  declared in `src/lib.rs`.
- `tests/basic_boot.rs`: an integration test, declared with `harness = false` in `Cargo.toml`.
  **Important:** `harness = false` means Cargo does not pass `--test` to rustc, which means
  `custom_test_frameworks`/`#[test_case]` machinery never activates for that file — this is a real
  gotcha that cost real debugging time. Files under `tests/` must instead define their own
  `fn main()` (via `entry_point!`) that runs assertions directly and calls `exit_qemu(...)` itself,
  rather than trying to reuse the `#[test_case]`/`test_runner` pattern from `lib.rs`.
- `tests/fork_wait.rs`: a second `harness = false` integration test (see "Process abstraction,
  scheduler, and fork/exec/wait" below for the full design) that boots the kernel, spawns a
  dedicated `userland/fork-exec-smoke/` binary as pid 1, and verifies a real `fork`/`wait4`/`exit`
  round trip. Since `scheduler::start`/`process::do_exit` never return control to a test's own
  `main` the way `basic_boot.rs`'s straight-line assertions do, it registers a test-only syscall
  number directly against `oxidebsd::syscall::oxidebsd_register_syscall` (`pub`, not `pub(crate)`,
  specifically so an external test crate can reach it) whose handler calls `exit_qemu` from that
  syscall-handling context instead.

## Custom target spec (`x86_64-oxidebsd.json`)

Notes on nonobvious fields, since these have shifted across nightly rustc versions and getting
them wrong fails obscurely:
- `target-pointer-width` / `target-c-int-width` must be **numbers**, not strings (older
  target-spec examples found online use strings and will fail to parse on current nightly).
- Floating point returns require an explicit ABI: `"features": "...,+soft-float"` alone is not
  enough — `"rustc-abi": "softfloat"` must also be set, or `core`/`compiler_builtins` fail to build
  with an LLVM SSE-register error (this target disables SSE/MMX, `disable-redzone: true`, since
  interrupt handlers can't safely use SSE state or the red zone).
- `panic-strategy: "abort"` is the only strategy this target supports, which is *why*
  `-Z panic-abort-tests` is required in `.cargo/config.toml`: without it, Cargo builds test
  binaries without `-C panic=abort` (assuming an unwind-capable libtest harness), producing a
  second, ABI-incompatible build of `core` and failing with "duplicate lang item" errors when
  linked against the `panic=abort` build used everywhere else.

## Memory management (`src/memory.rs`, `src/allocator.rs`)

The kernel doesn't build its own page tables from scratch — it reuses the ones the bootloader
already set up, which the `map_physical_memory` bootloader feature exposes as a full mapping of
physical memory at `BootInfo::physical_memory_offset`:
- `memory::init` walks `CR3` to find the level 4 table's physical address, then adds
  `physical_memory_offset` to get a virtual pointer to it — this is *not* a general-purpose
  physical-to-virtual scheme, it only works because the bootloader identity-mapped-with-offset all
  of physical RAM. `memory::init` must be called at most once (it hands out a `&'static mut`
  reference to that table); calling it twice would alias mutable references to the same memory.
- `memory::BootInfoFrameAllocator` allocates physical frames by walking
  `BootInfo::memory_map`'s `Usable` regions in order and never reusing one — there's no
  deallocation path yet, so freed frames currently just leak. Fine for a single heap-mapping call
  at boot; will need revisiting once anything frees frames at runtime.
- The heap lives at a fixed virtual address (`allocator::HEAP_START`, `0x_4444_4444_0000`) chosen
  to be far from anything else mapped, but its *size* is no longer a fixed constant:
  `allocator::compute_heap_size` scales it to whatever RAM this particular boot actually reports
  (`memory::usable_ram_bytes()`, itself populated by `memory::BootInfoFrameAllocator::init` from
  `BootInfo::memory_map` — see "Scaling to detected RAM" below), clamped between a proven-sufficient
  floor (4 MiB, the old fixed value) and a ceiling (128 MiB) that just bounds one-time boot mapping
  cost on a RAM-rich host. `allocator::init_heap` takes the computed size as a parameter and maps
  that range page-by-page before handing it to the global allocator.
- **Sizing that scales with detected RAM, and sizing that deliberately doesn't.** Besides the heap
  above, `process::kernel_stack_size()`/`process::user_stack_pages()` (see "Process abstraction,
  scheduler, and fork/exec/wait" below) scale the same way — each a `spin::Lazy` reading
  `memory::usable_ram_bytes()` once, clamped between the old fixed value (kept as a floor, since
  it's already proven sufficient) and a ceiling that just bounds how generous a RAM-rich boot gets
  for free. This exists because the only thing that used to actually read real hardware's RAM size
  was `BootInfoFrameAllocator` itself — everything downstream (heap, per-process stacks) was a
  constant tuned against whatever RAM happened to be in the one test VM used during development,
  which wouldn't necessarily suit a much smaller or much larger target machine. Deliberately *not*
  scaled: `modules/fat32`'s embedded disk image size (fixed at build time, before any target
  machine's actual RAM is known — nothing at runtime could inform it) and `module::MODULE_VA_BASE`/
  `MODULE_REGION_CEILING` (a virtual-address-range limit forced by the module loader's
  relocation-model choice — see "Dynamic kernel modules" below — not a physical-RAM quantity, so
  more RAM wouldn't let it grow even in principle).
- The `#[global_allocator]` is `linked_list_allocator`'s `Heap` wrapped in a project-local
  `Locked<T>` (a thin `spin::Mutex` newtype), not the crate's own `LockedHeap` — see Dependency
  notes below for why.
- `oxidebsd::init` now takes `&'static BootInfo` (previously took nothing) because heap setup
  needs `physical_memory_offset` and `memory_map` from it. All three entry points
  (`src/main.rs`, `src/lib.rs`'s `#[cfg(test)]` entry, `tests/basic_boot.rs`) pass their
  `BootInfo` straight through.

## User-mode execution (`src/address_space.rs`, `src/elf.rs`, `src/usermode.rs`)

Proves paging beyond the kernel's own address space, address-space separation, and ELF loading all
work, by loading an embedded userland binary and jumping into ring 3. `src/process.rs`'s `spawn`
builds the first process (`stsh`, pid 1 — see "Interactive shell" below) this way at boot, and
`src/process.rs`'s `do_execve` builds every subsequent one the same way, just later and mid-syscall
— see "Process abstraction, scheduler, and fork/exec/wait" below for how control actually reaches
ring 3 now (through the scheduler's own trampolines, not a direct call). `userland/ring3-smoke/`
(OxideBSD's own native ABI — see "Syscall ABI" below) isn't spawned directly at boot; it's instead
embedded into the FAT32 image (see "FAT32 filesystem module" below) as `SMOKE.ELF` so `stsh` can
genuinely `execve` it as a real file. `userland/musl-smoke/` (see "musl port" below) is embedded
and `execve`'d the exact same way, as `MUSL.ELF`. Whichever binary runs, the pattern is the same:
it does something observable (prints a message and exits with a distinctive code, or — for `stsh`
— runs an interactive read/dispatch loop), and that output over serial/VGA is the actual
end-to-end proof paging/ELF-loading/ring-3/syscalls all work together.

- **The userland demo build.** `userland/ring3-smoke/`, `userland/stsh/`, and
  `userland/fork-exec-smoke/` (a minimal fork/wait-only smoke test purpose-built for
  `tests/fork_wait.rs` — see "Process abstraction, scheduler, and fork/exec/wait" below) are
  separate workspace members (root `Cargo.toml` gained a `[workspace]` table; the root package is
  still its own workspace member, same as before). The root package's `build.rs` cross-builds all
  of them (via a shared `build_userland_crate` helper) into `target/userland/` — a **separate**
  target directory from the outer build's own `target/`, not the shared one: invoking a nested
  `cargo build` against the *same* target directory a still-running outer cargo invocation already
  holds a lock on (build scripts run as part of that outer build) can deadlock. Each resulting
  ELF's path is exposed via `cargo:rustc-env=<NAME>_ELF_PATH=...`, so `src/main.rs`/`tests/*.rs` can
  `include_bytes!(env!("..._ELF_PATH"))` — this keeps `cargo build`/`cargo run`/`cargo test` working
  with no manual pre-step. Each userland crate's `linker.ld` forces its own distinct load base
  (`0x800000` for `ring3-smoke`, `0x600000` for `stsh`, `0x700000` for `fork-exec-smoke`) clear of
  the kernel's own image, heap, and phys-memory-offset window (see below) — genuinely required
  now, not just future-proofing, since `execve` can load a binary into a running system rather than
  only at a one-binary-per-boot demo; each crate's own tiny `build.rs` passes its script as a link
  arg to just its own binary, not via `RUSTFLAGS`, which would apply to (and force a rebuild of)
  the `-Z build-std`-compiled core/alloc/compiler_builtins shared with the outer build.
  `userland/musl-smoke/` is built completely differently — see "musl port" below — since it isn't
  a Rust crate at all, just one `.c` file compiled with `musl-gcc`; its load base (`0xa00000`) was
  picked the same way, just via an explicit `-Wl,-Ttext-segment=` linker flag instead of a
  `linker.ld`.
  - **These load bases must also stay clear of the `bootloader` crate's own identity-mapped
    low-memory region** — a real bug, found and fixed: `bootloader` (v0.9) identity-maps roughly
    the first 6 MiB of physical memory (kernel-only, not `USER_ACCESSIBLE`) as part of its own
    bootstrap, independent of and larger than the kernel's own image, confirmed empirically (PD
    entries 0–2 present at boot, entry 3+ not). Since every fresh address space
    shares/aliases whatever's kernel-only in the currently active table (see `AddressSpace::fork`/
    `new_excluding_user` below), a user ELF loaded inside that identity-mapped range collides with
    it (`MapToError::PageAlreadyMapped`) — `ring3-smoke` originally sat at `0x400000`, squarely
    inside it, and this only surfaced once `execve` started actually exercising the path (a
    one-demo-per-boot `spawn` at boot never collided by coincidence). `0x600000` and up are
    confirmed clear.
- **Address spaces are a shallow copy — except when they can't be.** `AddressSpace::new` allocates
  a fresh frame for a new level 4 table and copies all 512 entries from the *currently active* one
  into it — not a "higher half only" split (this kernel has no such split at all: kernel code, the
  heap, the phys-mem-offset window, and every user ELF's load address all coexist in the low
  canonical range at different indices, not in any clean high/low half), and not a deep clone: the
  copy is just raw entries (pointers to lower-level tables), so the new address space shares
  whatever's active with the original. Deliberate for the kernel-mapping side: interrupt/exception
  handlers always run in the kernel's context regardless of which address space was active when
  they fired, so the kernel must stay identically mapped and reachable no matter what. **Only
  safe to call while the active table's user-space content is empty** (true for `process::spawn`,
  called only against the kernel's own address space at boot) — calling it from an
  already-running process (as an early, broken version of `fork` did) silently aliases that
  process's *user* mappings into the "new" table too. `AddressSpace::fork`/`new_excluding_user`
  (see "Process abstraction, scheduler, and fork/exec/wait" below) exist specifically for the
  from-inside-a-running-process case, using a recursive, `USER_ACCESSIBLE`-flag-driven walk instead
  of a flat copy.
- **RSP0 must actually be writable — `static`, not `static mut`, silently isn't.** `gdt.rs`'s
  ring-0 stacks (both the double-fault IST stack and the new RSP0 stack for ring-3-triggered
  interrupts) are declared `static mut STACK: [u8; N]`, not `static STACK: [u8; N]`. This is
  load-bearing, not stylistic: with a plain `static` and only ever taking `&raw const` to it nowhere
  in Rust code ever forms a `&mut` to it — Rust/LLVM is free to (and did) intern the all-zero array
  into read-only `.rodata`. The actual writes happen entirely via the CPU pushing an interrupt frame
  in hardware, invisible to that analysis. The failure mode is exactly as confusing as it sounds: a
  `#GP`/`#PF` double-fault-cascading-to-triple-fault the instant any exception tries to use that
  stack, with a `CR2`/fault address landing inside the kernel's own `.text`/`.rodata` region. If a
  future stack (IST slot, per-process kernel stack, etc.) is added the same way, it needs the same
  `static mut` + `&raw mut` treatment.
- **Rule, not a one-off: every gate ring 3 triggers with a software interrupt needs `DPL = Ring3`
  explicitly.** Interrupt gates default to `DPL = Ring0`, and a *software*-invoked interrupt
  (`int n`, `int3`, `into`, `bound` — as opposed to a hardware exception or IRQ, which bypass the
  gate's `DPL` check entirely) additionally requires `CPL <= gate DPL`. This bit this project once
  already, on `idt.breakpoint` (for `int3`), which needs `.set_privilege_level(PrivilegeLevel::
  Ring3)`. It also used to apply to the native ABI's own syscall gate (`int 0x80`, before it moved
  to `SYSCALL`/`SYSRETQ` — see "Syscall ABI" below; `SYSCALL` isn't a software interrupt at all and
  has no IDT gate/DPL concept, so this rule no longer applies there). Any *future* software
  interrupt gate ring-3 code needs to trigger directly will need it too. Getting this wrong doesn't
  look like a permissions error: it's a `#GP` on the IDT entry itself (decode the error code's
  selector-index bits to confirm — they name the IDT vector).
- **`elf::load` must not re-map or re-zero a page two different `PT_LOAD` segments both touch.**
  Segments are aligned to `p_align`, not to each other, so small binaries routinely have e.g.
  `.text` and `.rodata` sharing a page. `load` tracks already-mapped pages in a
  `BTreeMap<Page, PhysFrame>` for the duration of one call: mapping the same page twice fails
  (`MapToError::PageAlreadyMapped`), and zeroing it twice would erase the earlier segment's bytes.
  Flags aren't unioned across segments sharing a page — fine while every segment maps
  `PRESENT | USER_ACCESSIBLE` and only conditionally adds `WRITABLE`, worth revisiting if that
  stops being true.
- **Known simplification:** ELF segments are mapped without `NO_EXECUTE`, even for non-executable
  segments. Enforcing that also requires setting `EFER.NXE`, which deserves its own focused pass
  rather than bundling into this already-large change.

## Syscall ABI (`src/syscall.rs`)

OxideBSD's own, native ABI — deliberately BSD-flavored, not Linux-flavored. `SYSCALL`/`SYSRETQ`;
syscall number in `RAX`, up to three arguments in `RDI`/`RSI`/`RDX` (`R10` reserved in the layout
for a future 4th argument, avoiding `RCX`/`R11` since `SYSCALL` itself clobbers them to save
`RIP`/`RFLAGS`). Success/failure is signaled via the **carry flag** — the traditional BSD/x86 Unix
convention: `CF = 0` on success with the return value in `RAX`, `CF = 1` on failure with the
*positive* `errno` in `RAX`. Syscall numbers for calls implemented before the musl port (`SYS_EXIT
= 1`, `SYS_READ = 3`, `SYS_WRITE = 4`, `SYS_OPEN = 5`, `SYS_CLOSE = 6`) match real FreeBSD's
long-stable values, as a nod to authenticity — not a claim of binary compatibility with real BSD
userland. Syscalls added *for* the musl port (`SYS_MMAP`/`SYS_MUNMAP`/`SYS_BRK`/
`SYS_SET_FS_BASE`/`SYS_WRITEV` — see "musl port" below) don't extend that convention: they're
OxideBSD's own invention, numbers and argument shapes chosen for what porting musl actually needed,
not copied from FreeBSD or Linux. errno values are *mostly* shared between Linux and the BSDs
(`EBADF`, `EINVAL` are identical) but not universally — `ENOSYS` is 38 on Linux, 78 on FreeBSD;
this file uses the FreeBSD value.

**This mechanism used to be two independent, deliberately different paths**: this one over
`int 0x80`, and a separate `src/linux_syscall.rs` that proved the `SYSCALL`/`SYSRETQ` mechanism in
isolation (Linux's numbering, negative-`RAX` error convention, aimed at eventually running
unmodified Linux binaries). That plan changed — musl is instead being ported to speak *this* ABI
directly (see "musl port" below) — so there was no longer a reason to keep two different syscall
conventions each tied to a different trap instruction. Since `IA32_LSTAR` can only point at one
entry stub, this ABI now **owns** `SYSCALL`/`SYSRETQ` outright; `src/linux_syscall.rs` and its
dedicated `userland/linux-syscall-smoke/` test are gone, having already served their purpose of
proving the mechanism (`IA32_STAR`/`LSTAR`/`SFMASK` setup, the GDT segment-ordering requirement,
the stack-switch-on-entry problem, all below) works at all.

**The number → handler mapping is a registry populated by a dynamically loaded module, not a
hardcoded `match`.** `modules/native_abi/` registers `SYS_EXIT`/`SYS_READ`/`SYS_WRITE`/etc. (and
`modules/fat32/` separately registers `SYS_OPEN`/`SYS_CLOSE`) via `oxidebsd_register_syscall` from
their own `module_init` — see "Dynamic kernel modules" below. What's genuinely still
kernel-resident, deliberately *not* moved into `native_abi`, is the actual `sys_exit`/`sys_read`/
`sys_write`/etc. *behavior*. `oxidebsd_sys_exit`/`oxidebsd_sys_read`/`oxidebsd_sys_write`/etc.
(thin FFI adapters over them) are what the module actually calls through.

- **`SYSRETQ`'s segment-selector scheme forced a GDT reorder.** `SYSRETQ` reconstructs target
  `CS`/`SS` from `IA32_STAR` bits `[63:48]` (call it `X`) as `SS = X+8`, `CS = X+16` — user *data*
  must sit immediately before user *code*, backwards from the natural ordering. `SYSCALL` needs
  `STAR[47:32]` (`Y`) as kernel `CS = Y`, `SS = Y+8`, which needed an explicit kernel data segment
  that didn't exist before (this kernel had never needed one — `SS` was never reloaded in ring 0,
  and long mode doesn't re-validate it unless you do). `src/gdt.rs`'s GDT is now, in order:
  kernel code, kernel data, an unused placeholder (historically 32-bit-compat user code, needed
  only so the offsets land right — its *contents* are never loaded since this kernel only uses
  `SYSRETQ`), user data, user code, TSS. Don't reorder or insert entries in this block without
  redoing the `STAR` arithmetic. `init` now also explicitly reloads `SS` on boot, closing that
  latent gap. Use `x86_64::registers::model_specific::Star::write` (not hand-rolled offset math)
  to program `IA32_STAR` — it validates the selectors' offsets and privilege levels against exactly
  this scheme and fails loudly (a `panic!`, not silent misprogramming) if the GDT ever regresses.
- **No automatic stack switch on `SYSCALL` entry, fixed via a per-process `CURRENT_RSP0` mirror,
  not a single global scratch stack.** Unlike an interrupt gate + TSS `RSP0`, `SYSCALL` does not
  switch stacks: control arrives at `syscall_entry` still on the *user's own* stack (now at CPL 0).
  An early design (inherited from `linux_syscall.rs`'s original one-shot-demo-only mechanism) used
  a single fixed global kernel scratch stack for this — *wrong* for the native ABI specifically,
  because `do_wait4` already blocks and reschedules mid-syscall (see "Process abstraction,
  scheduler, and fork/exec/wait" below), so a second process could enter its own syscall before
  the first one returns, corrupting a shared global stack. The fix: `gdt::CURRENT_RSP0` (a plain,
  directly asm-readable `static mut`, kept in sync by `gdt::set_kernel_stack` on every context
  switch, right alongside the real `TSS.RSP0` it mirrors) always names the *current* process's own
  kernel stack. The entry stub stashes the user's `RSP` in a transient global slot just long enough
  to `mov rsp, [CURRENT_RSP0]` and push it as the first field of *that process's own* `SyscallFrame`
  — from that point on it's exactly as per-process-safe as every other field already is, and the
  transient global slot is provably safe for that brief window since `SFMASK` keeps interrupts off
  for the whole entry sequence on this single-core kernel (at most one syscall can be *entering* at
  once). No per-CPU `swapgs`/`IA32_KERNEL_GS_BASE` (the real-kernel mechanism for finding a kernel
  stack under `SYSCALL`) — this kernel has no SMP/APIC multi-processor support at all yet; revisit
  if multiple cores ever show up.
- **`SyscallFrame` spans the pushed GPRs plus one new field, `user_rsp`, replacing what an
  interrupt gate's automatic frame push used to provide.** The first 15 fields are the stub's own
  pushed registers, same shape as the old `int 0x80` stub's; there's no separate `_rip`/`_cs`/
  `rflags`/`_rsp`/`_ss` block anymore since `SYSCALL`/`SYSRETQ` don't work that way. Two fields do
  double duty instead, both forced by `SYSCALL`'s own hardware contract: `rcx` holds the user `RIP`
  to resume at (`SYSCALL` clobbers real `RCX` with it on entry), and `r11` holds the user `RFLAGS`
  (same story) — `SYSRETQ` reads both back directly from registers, so `syscall_dispatch` flips bit
  0 of the saved `r11` to signal `CF`, the same trick that used to flip a dedicated `rflags` field
  for `iretq`. `user_rsp` is the one genuinely new field, needed because `SYSCALL` doesn't switch
  stacks the way an interrupt gate does (see above) — there's no GPR slot already carrying it, so
  the entry stub pushes it first (ends up as the *last* field/highest address, popped last via a
  literal `pop rsp` right before `sysretq`). Field order otherwise matches the stub's push order
  exactly (last pushed = lowest address = first field).
- **The actual number→handler dispatch lives in a small, pure `dispatch` function**, deliberately
  separated from `syscall_dispatch`'s raw-pointer/frame handling specifically so it stays directly
  unit-testable (see `test_syscall_dispatch_rejects_unknown_number` and
  `test_syscall_dispatch_routes_registered_handlers` in `src/lib.rs`, the latter registering a
  throwaway handler directly — no module loading or interrupt machinery needed) without needing a
  real `SyscallFrame`. It's now a lookup into `SYSCALL_TABLE` (an `alloc::collections::BTreeMap`,
  guarded by a `spin::Mutex`), populated at runtime by `oxidebsd_register_syscall` — see this
  file's module doc comment and "Dynamic kernel modules" below. An unregistered number logs
  `[boot] unrecognized syscall number N` before returning `ENOSYS` — the intended tool for
  iteratively discovering what a program's startup still needs (see "musl port" below).
- **A registered handler's own FFI convention (`SyscallHandler`) is a distinct wire format** from
  the public syscall ABI: a plain `i64`, negative for `-errno`, non-negative for a success value.
  Not the carry-flag convention this file's own ABI uses — purely the internal shape of the
  module↔kernel registration boundary, chosen because it's representable in a plain scalar return
  without needing a `#[repr(C)]` result struct passed across a module-relocation boundary.
- **The stub saves/restores every general-purpose register uniformly**, not just the System-V
  caller-saved set (the callee-saved ones — `RBX`/`RBP`/`R12`-`R15` — are technically already
  preserved across the `call` by the Rust ABI's own contract). Redundant for those five, but a
  uniform save/restore is simpler to get right than relying on which specific registers a given
  ABI happens to guarantee, and this is exactly the kind of place a subtle mistake shows up as
  silent post-syscall register corruption in a user program, not a loud crash.
- **`oxidebsd_sys_exit` goes through `process::do_exit`** — real, per-process termination that
  hands control to whatever the scheduler picks next, only falling back to a full `hlt_loop()` when
  nothing else is runnable.
- **`sys_write`/`sys_read`/`sys_writev` return `Result<u64, u64>`** (`Ok(value)` /
  `Err(positive errno)`) — the shared, canonical representation `syscall_dispatch` adapts into the
  carry-flag wire format.
- **`sys_write`/`sys_read` do not validate `[ptr, ptr+len)`** before dereferencing it as user
  memory — no check that the range is mapped, `USER_ACCESSIBLE`, or doesn't reach into kernel-only
  mappings. A bad pointer page-faults, which `page_fault_handler` already handles safely (log +
  `reboot()`) rather than corrupting kernel state, so this is a missing safety net for user
  programs, not a kernel soundness hole — but real validation (walking the active page table to
  confirm the range) is a natural follow-up, not yet implemented.
- **`sys_read` is non-blocking — a deliberate simplification, not (yet) converted to use the real
  scheduler that now exists.** A real `read` on an empty, non-`O_NONBLOCK` fd blocks the calling
  process and reschedules another one; `process::do_wait4` (see "Process abstraction, scheduler,
  and fork/exec/wait" below) proves this kernel can now do exactly that for a different syscall,
  but `sys_read` itself hasn't been converted to follow the same pattern (block + `scheduler::
  schedule()` on an empty stdin buffer instead of returning `Ok(0)` immediately) — real, separate
  follow-up work. Returning `Ok(0)` immediately when the buffer is empty pushes the polling loop
  into userland instead — see "Interactive shell" below for the caller side of that contract. This
  non-blocking property is specific to fd 0 (stdin) — a FAT32 file's `read`, routed through
  `crate::fd` below, always completes immediately regardless (there's no "not ready yet" state for
  an in-memory file), so `cat` in `stsh` doesn't need to busy-poll it the way reading from stdin
  does.
- **`sys_read`/`sys_write`, for any `fd` other than stdin/stdout, delegate to `crate::fd`'s
  registry** (`src/fd.rs`) rather than returning `EBADF` outright — the only channel two
  independently loaded modules have to coordinate (`modules/fat32/`'s open files register their
  read/write/close callbacks there; modules can't call each other directly, only module → kernel —
  see "Dynamic kernel modules" below). `EBADF` is still what comes back if `fd` isn't stdin/stdout
  *and* isn't registered in that table either.

## musl port (`third_party/musl`, `userland/musl-smoke/`, `src/user_stack.rs`, `src/fpu.rs`)

Rather than making the kernel Linux-syscall-compatible enough to run an unmodified musl/BusyBox
binary (the original plan — see "Syscall ABI" above for why that changed), musl itself is being
patched to speak OxideBSD's own native ABI directly. `userland/musl-smoke/main.c` — a real,
`printf`-using static musl binary, otherwise unmodified above the syscall layer — runs end to end:
`execve`'d by `stsh` (as `musl.elf`/`MUSL.ELF`, embedded in the FAT32 image exactly like
`SMOKE.ELF`), it prints its message and exits cleanly, and `stsh` regains control via `wait4`. musl
is explicitly a temporary/placeholder libc choice for this phase, not a long-term commitment.

- **`third_party/musl` is a git submodule pointing at a personal fork, not the canonical repo
  directly** — musl isn't hosted on GitHub at all (canonical is `git.musl-libc.org`), so the fork
  is of `ifduyue/musl` (an active, up-to-date unofficial GitHub mirror — confirmed to match the
  real `v1.2.6` tag's commit hash before forking it). OxideBSD's patches live on that fork's own
  `oxidebsd` branch, based on the `v1.2.6` tag — **not** on the fork's `master`, which tracks
  upstream. Pin/update the submodule by committing on that branch, pushing, then `git add
  third_party/musl` in this repo to move the tracked commit.
- **The entire patch surface is three small, targeted changes** — this is a syscall-layer port,
  not a from-scratch libc port; everything above `arch/x86_64/` is stock, unmodified musl:
  - `arch/x86_64/syscall_arch.h`: musl's `__syscallN` wrappers already emit a plain `syscall`
    instruction with the right argument registers (Linux and OxideBSD's own ABI happen to agree on
    register placement) — the only real difference is error signaling. A `jnc 1%=f; neg %%rax;
    1%=:` sequence right after `syscall` converts OxideBSD's carry-flag convention into the small-
    negative-value shape musl's `__syscall_ret` already expects, without touching anything above
    the trap site. `%=` (not a bare numeric label) is required since these are `static __inline`
    functions inlined at every call site within one translation unit — a bare label would collide.
  - `arch/x86_64/bits/syscall.h.in`: only the `__NR_*` entries musl's own startup/malloc/stdio path
    for a *static* binary can actually reach are remapped to OxideBSD's real registered numbers
    (`read`/`write`/`open`(*)/`close`/`getpid`/`fork`/`execve`(*)/`exit`/`exit_group`/`wait4`/
    `mmap`/`munmap`/`brk`/`writev` — see below); everything else keeps its original, inert Linux
    value until something actually needs it (reached, it cleanly `ENOSYS`s and logs the number —
    see "Syscall ABI" above — rather than silently miscompiling). (*) `open`/`execve` are
    deliberately **not** remapped despite OxideBSD registering `SYS_OPEN`/`SYS_EXECVE` under those
    same names — see the argument-convention mismatch note below; remapping just the number without
    fixing the arguments would be worse than leaving it unmapped.
  - `src/thread/x86_64/__set_thread_area.s`: real Linux sets the TLS base via
    `arch_prctl(ARCH_SET_FS, addr)` (syscall 158, subcommand `0x1002`); OxideBSD has no
    `arch_prctl` at all. This hand-written asm stub (bypasses the C wrappers above entirely, so it
    needs its own `jnc`/`neg` adaptation) now just does `movl $103, %eax` (`SYS_SET_FS_BASE`) —
    simpler than upstream's version even, since OxideBSD's invented call takes the base address
    directly with no subcommand to select.
- **New syscalls, all OxideBSD's own invention** (see "Syscall ABI" above for why these don't chase
  FreeBSD/Linux authenticity the way the pre-existing numbers do) — registered by
  `modules/native_abi/`, implemented in `src/syscall.rs`/`src/process.rs`:
  - **`SYS_SET_FS_BASE = 103`**: 1 arg, the TLS base address value itself (`x86_64::registers::
    model_specific::FsBase::write`, gated behind the already-enabled `"instructions"` `x86_64`
    crate feature — writes `IA32_FS_BASE` via `wrmsr`, no `FSGSBASE` CPU feature needed). Always
    succeeds.
  - **`SYS_MMAP = 100`**: `(addr_hint, len, prot)` — **not** `(len, prot, flags)` as an earlier
    design pass assumed. musl's `mmap()` always calls `__syscall6(SYS_mmap, addr, len, prot, flags,
    fd, off)`, so `addr`/`len`/`prot` land in `RDI`/`RSI`/`RDX` regardless of what this ABI itself
    would prefer — matching *musl's actual call site*, not an invented layout, was the only way to
    make this work at all. `addr_hint`/`prot` are read but ignored (OxideBSD always picks the
    address and always maps `PRESENT | WRITABLE | USER_ACCESSIBLE`, the same "every page writable
    regardless" simplification `src/module.rs`'s own loader already applies); `flags`/`fd`/`offset`
    (`R10`/`R8`/`R9`) aren't even readable at this ABI's 3-argument width, so every mapping is
    unconditionally anonymous+private — the only case musl's allocator needs. `do_mmap`
    (`src/process.rs`) bump-allocates from a fixed, never-reclaimed VA window
    (`0x_2000_0000_0000..0x_3000_0000_0000`, same idiom as `module::NEXT_MODULE_PAGE`), building a
    mapper over the *calling* process's own (currently active) address space.
  - **`SYS_MUNMAP = 101`**: `(addr, len)` — a no-op success, consistent with this codebase having
    no `FrameDeallocator` anywhere yet.
  - **`SYS_BRK = 102`**: `(addr)`, `0` = query without changing. `do_brk` grows a per-process heap
    region (`Process.brk`, starting at `Elf::highest_loaded_address()`) by mapping freshly zeroed
    pages from the first not-yet-mapped page onward (`me.brk` isn't necessarily page-aligned after
    a partial grow, so re-mapping the page it falls on must be skipped, not attempted again);
    shrinking just lowers the stored value, same no-reclaim simplification as `munmap`. Ceiling
    fixed at `0x_1000_0000` (matches `module::MODULE_VA_BASE`) so a growing heap can never collide
    with the kernel-mapped module region every address space shares.
  - **`SYS_WRITEV = 104`**: `(fd, iov_ptr, iovcnt)` — added only after a real, confusing bug (see
    below) revealed it was load-bearing, not optional. Reads real C `struct iovec` entries (16
    bytes each: `void *iov_base`, `size_t iov_len`) and calls `sys_write` once per entry,
    accumulating the total; matches real `writev`'s partial-write semantics (a failure after at
    least one successful entry returns the partial total, not an error — only the very first
    entry's failure propagates).
- **A real, previously-latent bug, found only by actually running the thing: `writev`/`getpid`
  numbering collision silently discarded all of `musl-smoke`'s output.** musl's *entire* stdio
  write path (`src/stdio/__stdio_write.c`) goes through `writev`, never plain `write` — a fact not
  obvious from reading musl's public API. `__NR_writev`'s original Linux value is `20`, which
  happens to be OxideBSD's own `SYS_GETPID`. Left unmapped, every `printf` silently invoked
  `getpid()` instead of ever writing — no crash, no error, `musl-smoke` still exited cleanly with
  code `0`, just with **zero** actual output. `cargo test`/`clippy`/`fmt` all stayed green through
  this; only booting in QEMU and looking at the serial log surfaced it. The general lesson (already
  called out elsewhere in this file, worth restating): passing tests are not the same as the
  feature working, especially across a port boundary where numbers can collide by coincidence
  rather than fail loudly.
- **`src/fpu.rs` (new): SSE was never actually enabled at the hardware level, and nothing noticed
  until now.** This kernel's own build target (`x86_64-oxidebsd.json`) disables SSE/MMX in its own
  codegen, so every userland crate written *for* that target (`ring3-smoke`, `stsh`,
  `fork-exec-smoke`) never emits an SSE instruction — but `CR0.EM`/`CR4.OSFXSR`/
  `CR4.OSXMMEXCPT_ENABLE` (the actual hardware switches that make SSE legal to execute *at all*,
  independent of what any given compilation target chooses to emit) were never touched anywhere in
  this codebase either. `musl-smoke`, built with an ordinary host `gcc` targeting plain x86_64
  (SSE2 baseline, per the standard ABI), is the first userland binary this kernel has ever run that
  wasn't built against its own no-SSE target — and it `#UD`'d on its very first `pxor` (inside
  musl's stdio buffer init) before this fix. `fpu::init()` (called from `lib.rs::init`, right after
  `gdt::init`) sets the standard "enable SSE" `CR0`/`CR4` sequence once, globally, at boot.
  Deliberately **not** paired with lazy save/restore (`CR0.TS` + `#NM`-triggered `fxsave`/
  `fxrstor`) or even eager save/restore: `context_switch::switch_context` still doesn't touch
  `XMM`/`x87` state at all across a context switch, which is fine only as long as at most one
  SSE-using process is ever actually mid-computation at a time — true today (no preemption, and
  only one process at a time genuinely exercises SSE), a real gap the moment two could interleave.
- **`src/user_stack.rs` (new): builds a real System V AMD64 initial-process stack** (argc, argv[] +
  NULL, envp[] + NULL, then auxv `(tag, value)` pairs terminated by `AT_NULL`, string bytes below
  all of that) — musl's `crt1`/`_start` reads this directly off the stack before `main` ever runs;
  nothing before this existed at all (`process::map_user_stack` just mapped bare pages). Wired into
  both `process::spawn` and `process::do_execve` (see "Process abstraction, scheduler, and
  fork/exec/wait" above) — safe for every *existing* binary too, since none of them (`stsh`,
  `ring3-smoke`, `fork-exec-smoke`) ever read their own stack for arguments.
  - **`AT_PHDR`'s "standard" derivation had to be made robust against this codebase's *own*
    minimal linker scripts, not just musl's.** The textbook formula is `(load bias) + e_phoff`,
    where load bias is the vaddr that file offset `0` maps to — normally the `p_vaddr` of whichever
    `PT_LOAD` segment has `p_offset == 0` (the segment containing the ELF header itself). But this
    codebase's own `userland/*/linker.ld` scripts (unlike an ordinary linker's default script)
    don't map the ELF header into any `PT_LOAD` segment at all — their first segment typically
    starts at file offset `0x1000`, not `0` — so requiring `p_offset == 0` exactly panicked the
    first time this was wired up (against `fork-exec-smoke`). Fixed by computing the load bias
    (`p_vaddr - p_offset`, constant across every well-formed `PT_LOAD` segment of one ELF) from
    whichever segment has the *smallest* `p_offset` instead — for musl-smoke (an ordinary linker
    script, headers included in the first segment) this is the same value as the textbook formula;
    for this codebase's own hand-linked binaries it computes a value that happens to point at
    unmapped memory, which is fine, since none of them ever read `AT_PHDR` anyway (see `elf.rs`'s
    `Elf::phdr_vaddr`).
  - **`AT_RANDOM`'s 16 bytes are a fixed placeholder, not real entropy** — this kernel has no
    entropy source at all yet; musl only requires the bytes be *present* (it uses them for the
    stack-protector canary and as an `arc4random` seed), not unpredictable, for now.
- **Two known real argument-convention mismatches, deliberately left unfixed rather than silently
  half-fixed:**
  - **`open`**: real `open()`/musl's own `__syscall3(SYS_open, path, flags, mode)` passes a
    null-terminated C string pointer plus `(flags, mode)`. OxideBSD's `SYS_OPEN`
    (`modules/fat32/fat32_open`) takes `(path_ptr, path_len, flags)` — a length-prefixed pointer,
    no null-terminator requirement, no `mode` argument at all. Fixing this needs either a musl-side
    `open()` override or an OxideBSD-side argument-convention change; neither done yet, so
    `__NR_open` stays unmapped (see above) — `musl-smoke` doesn't call `open()`, so this hasn't
    blocked anything yet.
  - **`execve`**: numerically unchanged from upstream (`59` happens to already be OxideBSD's real
    `SYS_EXECVE`), but not actually argument-compatible: musl's `execve()` passes `(path, argv,
    envp)` in `RDI`/`RSI`/`RDX`; OxideBSD's `SYS_EXECVE` (`process::do_execve`) expects `(path_ptr,
    path_len)` — no `argv`/`envp` support at all. Unreachable so far since nothing in this port's
    own test programs calls `execve()` on itself (only `stsh`, over the native ABI it already
    speaks correctly, `execve`'s *other* programs).
- **Two syscalls musl's startup reaches but this kernel doesn't implement, left unmapped
  (their original Linux numbers) rather than stubbed — both confirmed harmless for `musl-smoke`
  specifically, not fixed just because they were seen:** `set_tid_address` (`__init_tp`, right
  after TLS setup — failing just leaves an unused `tid` field with a bogus value) and `ioctl`
  (`__stdout_write`'s `TIOCGWINSZ` probe — failing just makes musl correctly conclude stdout isn't
  a sized terminal and proceed to the real write anyway). Both log `[boot] unrecognized syscall
  number N` (218 and 16 respectively) every run; a future binary that actually depends on either
  succeeding would need them implemented for real.
- **`userland/musl-smoke/main.c` and its build are unlike every other `userland/*` entry** — see
  "User-mode execution" above for the load-base/linker-flag side of this; on the build-system side,
  `build.rs`'s `build_musl_sysroot` runs musl's *own* build system directly (`configure`/`make`/
  `make install` into `target/musl-sysroot`, no Cargo/Rust involved), then `build_musl_smoke` shells
  out to the resulting `musl-gcc` for `main.c` — mirroring `build_userland_crate`/
  `build_module_crate`'s existing "no manual pre-step" philosophy, just against a different
  toolchain. **A real gotcha in invoking musl's `configure` from a build script**: musl's script
  derives its own source directory from `${0%/configure}` — invoking it as `sh configure ...`
  (rather than a path that itself literally ends in `/configure`, like `./configure`) leaves `$0`
  as the bare string `"configure"`, which the suffix-strip is a no-op on, so it then tries (and
  fails) to `cd` into a directory named `configure`. `Command::new("./configure")` (not
  `Command::new("sh").arg("configure")`) is the invocation shape that actually works.
  `build_musl_sysroot` skips re-running `configure` if `config.mak` already exists (it re-probes
  the host compiler from scratch every time otherwise, several seconds each `cargo build`); `make`/
  `make install` are already fast, idempotent no-ops when nothing changed.
- **`modules/fat32`'s `MAX_FILE_BUFFER` raised `16384` → `65536`** for the same reason it was
  raised once before (`4096` → `16384` for `SMOKE.ELF`): `musl-smoke`, a real compiled C binary
  against a real libc, is bigger (~23 KiB) than any hand-written demo binary this codebase had
  embedded before.
- **What's explicitly still out of scope, the same way real musl/BusyBox running was flagged
  out of scope before this port started:** a real, general-purpose libc-on-OxideBSD story (this is
  one hand-picked static binary exercising `printf`, not a validated general surface), `envp`
  passthrough across `execve`, fixing the `open`/`execve` argument mismatches above, and anything
  resembling BusyBox itself — real, substantial follow-up work, not attempted yet.

## Interactive shell (`src/stdin.rs`, `userland/stsh/`)

The first genuinely interactive userland program: unlike `ring3-smoke`, which prints a message and
exits, `stsh` ("stupidshell") loops forever, reading a line at a time from the keyboard and
dispatching a small set of built-ins (`help`, `echo <text>`, `exit`, `cat <name>`, `write <name>
<text>`, `cd [path]`, `ls [path]`, `mkdir <name>`) — anything else is treated as a real program to
run (`fork`+`execve`+`wait`, see "Process abstraction, scheduler, and fork/exec/wait" below) —
until told to exit. It's wired up by default — see "User-mode execution" above — and runs entirely
over OxideBSD's own native ABI. `cat`/`write`/`cd`/`ls`/`mkdir` exercise `modules/fat32/`'s
`SYS_OPEN`/`SYS_CLOSE`/`SYS_CHDIR`/`SYS_MKDIR` end to end — see "Dynamic kernel modules" and "FAT32
filesystem module" below.

- **`cd`/`ls`/`mkdir` are still built-in commands, not separate programs `stsh` loads and execs —
  but not because `exec` doesn't exist anymore.** Now that real `fork`/`execve`/`wait` exist (see
  below), the original reason these were built-ins (no way for a child to return control to the
  shell) no longer applies — but `cd` specifically still has to be a built-in regardless, for the
  same reason it always is in a real shell: a child process changing its own working directory can
  never affect its parent's, so "changing directory" only means anything if the shell does it to
  itself. `ls`/`mkdir` stayed built-ins too simply because nothing has motivated splitting them out
  into separate exec'd programs yet, not because of any remaining structural blocker.
- **`ls` needed no new syscall** — `modules/fat32/`'s `fat32_open`, when the resolved target is a
  directory, hands back a formatted listing through the exact same `OpenFile::Read`/`SYS_READ`
  path a real file's content would use, so `ls` is implemented identically to `cat` on the `stsh`
  side. See "FAT32 filesystem module" below for why this is a pragmatic simplification, not a
  claim of matching real `open()`-on-a-directory semantics.

- **The data path: keyboard IRQ → `src/stdin.rs` ring buffer → `SYS_READ` → userland.**
  `keyboard_interrupt_handler` (`src/interrupts.rs`) already decoded scancodes into `DecodedKey`s
  for echoing; it now also pushes each ASCII `DecodedKey::Unicode` byte into `stdin`'s
  fixed-capacity (`256`-byte) ring buffer (non-ASCII is silently dropped — a US layout won't
  produce it, and it keeps the buffer's contract to raw bytes, not full UTF-8, simple). `sys_read`
  (`src/syscall.rs`) drains that buffer into the caller's pointer.
- **The keyboard handler only echoes printable characters and `\n`/`\r` itself; every other ASCII
  byte is still pushed to stdin but left unechoed at the IRQ level.** Control bytes (backspace,
  delete, Ctrl+C, Ctrl+D, ...) are forwarded raw either way, but *how* one should look on screen —
  erasing a character, printing `^C`, doing nothing — is inherently a userland concern (see
  `read_line`'s own handling, below), and echoing them here unconditionally used to just produce
  VGA's placeholder glyph (`src/vga.rs`'s `write_string` maps anything outside `0x20..=0x7e`/`\n`
  to one) for every one of them, which wasn't useful for any. The keyboard handler also now
  constructs its `PS2Keyboard` with `HandleControl::MapLettersToUnicode`, not `Ignore` — the
  reason `stsh` can see Ctrl+C/Ctrl+D as the C0 control codes 0x03/0x04 at all, rather than
  Ctrl being silently dropped and the bare letter coming through instead.
- **`src/vga.rs`'s `Writer` special-cases a raw `0x08` (backspace) as "step the cursor back one
  column, draw nothing"**, rather than falling into its placeholder-glyph path for
  not-`0x20..=0x7e`/`\n` bytes. This exists specifically so the standard `"\x08 \x08"` terminal
  idiom (backspace, space, backspace) `stsh`'s `read_line` writes to erase a character works
  correctly on the VGA console too, not just over a real serial terminal that already understands
  backspace on its own. Doesn't cross a wrapped-row boundary — nothing in this kernel tracks
  cursor position across VGA's own line wrapping.
- **The ring buffer is a plain fixed array, not `alloc::collections::VecDeque`**, specifically to
  avoid needing to reason about whether allocating from inside an interrupt handler is sound here —
  sidestepping the question is simpler than answering it. When full, `push_byte` drops the newest
  incoming byte rather than growing or overwriting unread data.
- **The shared `spin::Mutex` around the ring buffer can't deadlock, despite being touched from both
  the keyboard IRQ and syscall code**, because `IA32_SFMASK` clears `IF` on every `SYSCALL` entry
  (see "Syscall ABI" above), so it's already clear for the entire duration of any syscall — the
  keyboard IRQ can never preempt a syscall in progress on this single core. The two sides of the
  buffer are mutually exclusive by construction, not because the lock itself provides it; this
  reasoning breaks if the kernel ever gains SMP, at which point the buffer needs a real
  cross-core-safe lock.
- **`sys_read` is non-blocking** (see "Syscall ABI" above for why it still is, even though a real
  scheduler now exists to block against — it just hasn't been converted yet), so `stsh` busy-polls
  it one byte at a time in a `spin_loop()` until a byte arrives. This is a
  userland concern, not a kernel one — a real scheduler now exists (see "Process abstraction,
  scheduler, and fork/exec/wait" below) and `process::do_wait4` already proves this kernel can
  block+reschedule for a different syscall, but `sys_read` itself hasn't been converted to follow
  suit yet, so this polling shim is still what `stsh` relies on today.
- **Basic line editing exists, but it's still not full readline** — no cursor movement (arrow
  keys) and no history; `read_line` silently discards bytes past its 128-byte `LINE_CAPACITY`
  rather than growing or erroring. What it does handle: Backspace (`0x08`) and Delete (`0x7f`)
  both erase the most recently typed byte and re-emit `"\x08 \x08"` to erase it on screen too
  (both keys are treated identically — there's no cursor position to distinguish "erase before"
  from "erase under", so this is the pragmatic choice, not an oversight); Ctrl+C (`0x03`) aborts
  the in-progress line, prints `^C` and a newline, and returns as if an empty line had been
  entered, so `run_command` no-ops and the main loop just reprompts; Ctrl+D (`0x04`) on an empty
  line signals EOF and exits the shell via `SYS_EXIT` (matching real shell convention), and is
  ignored on a non-empty line (again, no cursor position to delete "under"). Any other control
  byte is dropped rather than inserted literally into the line. This relies on the keyboard
  handler's `HandleControl::MapLettersToUnicode` setting (see above) to actually produce the
  Ctrl+C/Ctrl+D bytes in the first place.

## Process abstraction, scheduler, and fork/exec/wait (`src/process.rs`, `src/scheduler.rs`, `src/context_switch.rs`)

Adds real multi-process support to what used to be a one-binary-per-boot kernel: a dynamically
allocated process table (`src/process.rs`), a cooperative round-robin scheduler
(`src/scheduler.rs`), and the low-level kernel-thread-style context switch that moves execution
between per-process kernel stacks (`src/context_switch.rs`). `fork`/`execve`/`wait4`/`getpid` are
new native-ABI syscalls (real FreeBSD numbers: `SYS_FORK = 2`, `SYS_WAIT4 = 7`, `SYS_GETPID = 20`,
`SYS_EXECVE = 59`), registered by `modules/native_abi/` the same way `SYS_EXIT`/`SYS_READ`/
`SYS_WRITE` already were. `stsh` uses all of this for real now — see "Interactive shell" above.

No preemption (cooperative only — see the scheduler doc comment for the deliberate, deferred seam),
no copy-on-write fork (full eager copy), no SMP, no `envp` passthrough across `execve` (`argv` *is*
now real — see "musl port" below for `src/user_stack.rs`, which builds a real initial
argc/argv/envp/auxv stack for every process, added after this subsystem's first pass), and no
frame deallocation anywhere (reaping a zombie frees its Rust-heap PCB state correctly but leaks the
physical frames backing its page tables/user pages — consistent with this codebase's existing
total lack of a `FrameDeallocator`, not a new regression).

- **The process table (`process::Process`, `process::table()`) is a `Mutex<BTreeMap<Pid,
  Box<Process>>>`, `Box`-wrapped deliberately.** Letting a caller pull a raw `*mut Process` (or copy
  needed fields) out from under a short-held lock and drop that lock *before* doing anything that
  might call `scheduler::schedule()` is load-bearing, not a style choice: the `BTreeMap`'s internal
  tree nodes can move on insert/remove, but a `Box`'s own heap allocation never does. Holding this
  lock across a context switch would only ever get released whenever that exact stack next
  resumes — a real deadlock if the switched-to process needs the same lock, which it always will.
  Every function in `process.rs` that touches both the table and `scheduler::schedule()` follows
  this discipline explicitly.
- **The context switch (`context_switch::switch_context`) is a classic kernel-thread `swtch`, not a
  full GPR save** — only System V's callee-saved registers (`rbp`, `rbx`, `r12`-`r15`) plus `RSP`
  itself, via the ordinary `call`/`ret` mechanism. Everything else is either caller-saved (already
  safe on the Rust call stack of whichever function called `schedule()`) or, for a process's ring-3
  register state, already saved by `syscall_entry`'s own pushes on *that process's own* kernel
  stack (see "Syscall ABI" above). The restore side is exactly symmetric with the save side — that
  symmetry is what lets one primitive handle both "resume a process that yielded mid-syscall" (the
  final `ret` lands back inside `schedule()`'s own call site) and "start a process that's never run
  at all" (a hand-seeded fake stack frame with the same shape makes the final `ret` land in a
  trampoline instead).
- **Two first-run trampolines, deliberately asymmetric.** `spawn_trampoline_asm` (a process that's
  never run at all — pid 1, or any future non-forked `spawn`) defensively `and rsp, -16` before
  `call`ing into real Rust code, sidestepping hand-deriving the exact stack offset that would
  satisfy System V's call-entry alignment (easy to get subtly wrong, painful to debug).
  `fork_trampoline_asm` (forked children only) jumps straight into `syscall_entry`'s own
  GPR-pop-and-`sysretq` tail (labeled `syscall_return_tail` specifically so this can reach it) with
  **no** realignment at all — `seed_fork_frame` places a copy of the parent's `SyscallFrame`
  immediately below the fake register-save frame, so after `switch_context`'s pops and `ret`, `RSP`
  already points exactly where that tail expects it. Counter-intuitively the fork path needs *less*
  defensive code than the spawn path.
- **`fork` resumes the child as if returning from its own `fork()` call with `0`, by copying the
  parent's live `SyscallFrame` onto the child's fresh kernel stack.** `syscall::copy_frame_for_fork`
  does the copy, then explicitly forces both `rax = 0` **and clears the copy's `CARRY_FLAG` bit
  (in `r11`, which doubles as the saved `RFLAGS` — see "Syscall ABI" above)** — a real bug caught
  before it shipped: at the moment of copying, the parent's frame's `r11` still holds whatever `CF`
  happened to be *before* the parent ever executed `SYSCALL` for this call (ordinary instructions
  like `mov` don't touch `EFLAGS`, so it's just leftover state, not anything this syscall itself set
  yet — `syscall_dispatch`'s own CF-clearing for the *parent's* return happens later and only
  touches the parent's own live frame). Without the explicit clear, the child could spuriously see
  `Err` from a stale bit that predates the call entirely.
- **`SyscallFrame`'s fields are private to `src/syscall.rs`; `fork`/`execve` reach it through two
  narrow, explicitly-added accessors instead** (`copy_frame_for_fork`, `redirect_frame`), plus one
  `AtomicPtr<SyscallFrame>` (`CURRENT_FRAME`, set at the top of `syscall_dispatch`) exposed via
  `syscall::current_frame()`. `SyscallHandler`'s `(u64, u64, u64) -> i64` shape can't carry a frame
  pointer, but these two syscalls specifically need raw access to the live frame that no other
  syscall does — a deliberate, narrowly-scoped exception rather than changing every syscall's
  signature.
- **`AddressSpace::fork`/`AddressSpace::new_excluding_user` replace a naive "clone everything, then
  try to zero out the low addresses" approach that seemed obviously right and was completely wrong**
  — see "User-mode execution" above for the full story: this kernel has no higher-half split at
  all, so a PML4-index-based cut can't distinguish kernel from user content. Both instead
  recursively walk the currently active table (`address_space.rs`'s `copy_table_level`), using the
  `USER_ACCESSIBLE` flag itself as the only reliable signal at *any* level (the MMU's own
  hierarchical walk requires every level down to a user page to carry it, so a clear bit anywhere
  guarantees nothing user-facing exists beneath it, safe to alias as-is without recursing further).
  `fork` copies user leaves fresh (byte-for-byte, via the phys-mem-offset window, the same
  technique `elf::load` already uses); `new_excluding_user` (used by `execve`, which needs a wholly
  clean user address space, not the calling process's inherited one) just skips them.
- **`do_execve` reuses `syscall::dispatch` directly** to drive an internal open/read-loop/close
  against the same fd/fat32 machinery `stsh`'s `cat` already exercises through the public syscall
  path — no separate file-loading code. Every fallible step (open, each read, close, `Elf::parse`,
  the new `AddressSpace`, `elf::load`, mapping the user stack, building the initial argv/envp/auxv
  image — see "musl port" below for `src/user_stack.rs`) completes *before* any mutation of the
  live syscall frame, `CR3`, or the process's stored `AddressSpace` — real `execve(2)` semantics: a
  failure at any point must leave the calling program completely untouched. `Process.user_stack_top`
  — despite the name — now holds the *computed initial `RSP`* `user_stack::build` returns (the
  address of the argc/argv/envp/auxv image itself), not the bare top of the mapped stack region;
  `Process` also gained a `brk: VirtAddr` field (the current top of the `SYS_BRK`-managed heap
  region, initialized from `Elf::highest_loaded_address()`, copied — not reset — into a forked
  child, since the underlying pages are already deep-copied by `AddressSpace::fork`).
- **`do_exit` replaces `sys_exit` for the native ABI only** (see "Syscall ABI" above for the split
  from the Linux path's own, unconverted `sys_exit`): marks the caller `Zombie(code)`, wakes its
  parent if blocked waiting on it (or on any child), then yields to the scheduler — guaranteed to
  either switch to something else or `hlt_loop()` if nothing else is runnable, since a `Zombie` is
  never re-enqueued. Orphaned grandchildren are **not** reparented to a pid-1 "init" this pass —
  an accepted simplification, not required for fork/exec/wait correctness.
- **`do_wait4` blocks by looping**: find a `Zombie` child matching the requested pid (`-1` = any)
  and reap it immediately if one exists; if the caller has no matching child at all, `ECHILD`;
  otherwise mark the caller `Blocked` and call `scheduler::schedule()` (dropping the table lock
  first). `do_exit` only *wakes* the parent — it doesn't hand the reaped child's info across
  directly — so on resume the loop just re-checks from the top.
- **Kernel stack size needed real, empirically-found margin, not just "enough for the common
  case," and that margin is now a floor, not a bare constant.** `128` KiB was found the hard way,
  twice: `16` KiB overflowed on `ls` (`SYS_OPEN` on a directory is a deeper call chain than plain
  `SYS_READ`/`SYS_WRITE`); `32` KiB then overflowed on `fork` itself (`AddressSpace::fork`'s
  page-table walk → `AddressSpace::new`'s `PageTable::clone()` has a surprisingly large unoptimized
  stack frame in a **debug** build). There's no guard page (heap-allocated, not a dedicated
  mapped-with-a-gap region like `gdt.rs`'s own stacks), so overflow corrupts silently or
  double-faults rather than failing cleanly — this needs real headroom for debug builds
  specifically. `process::kernel_stack_size()` (a `spin::Lazy<usize>`, not `pub const
  KERNEL_STACK_SIZE` anymore) scales this up on RAM-rich boots but never below the `128` KiB floor,
  for the same reason `process::user_stack_pages()` never drops below its own old fixed value (4
  pages) — see "Memory management" above for the general RAM-scaling design shared by all three.
  `allocator`'s heap floor was
  raised in step (`100` KiB → `4` MiB, now itself just the floor of a RAM-scaled size) since every
  process's kernel stack, plus the process table itself, plus `execve`'s internal `Vec<u8>`, all
  come from this same heap.
- **`gdt::set_kernel_stack`** repoints `TSS.RSP0` — the stack the CPU auto-switches to on the next
  ring-3→ring-0 transition — on every context switch, right before `switch_context`. Since `spin::
  Lazy` has no `DerefMut`, this writes through a raw pointer derived from `TSS`'s own fixed address
  (never moves once forced) rather than trying to get a real `&mut` — sound because nothing else
  ever holds a live `&TaskStateSegment` across the call (single-core; the scheduler only calls this
  with interrupts disabled).
- **`memory::install_global_memory_state`/`with_frame_allocator`/`phys_mem_offset`** promote the
  frame allocator and `physical_memory_offset` from local `main.rs` variables to global state,
  since `spawn`/`fork`/`execve` all need them from arbitrary syscall contexts, not just at boot.
  Populated exactly once, right after module loading finishes and before `stsh` is spawned — the
  frame allocator is *moved* in, never cloned, since `BootInfoFrameAllocator`'s own bump-allocation
  state must stay singular.
- **Boot wiring**: `main.rs`'s `kernel_main` now calls `process::spawn` (building `stsh` as pid 1,
  same `AddressSpace::new` + `elf::load` + user-stack-mapping sequence the old one-shot demo path
  used) then `scheduler::start(pid1)` — never returns, the same one-way shape `jump_to_usermode`
  always had, just reached through the scheduler's own trampoline now instead of a direct call.
- **`tests/fork_wait.rs` + `userland/fork-exec-smoke/`**: an automated integration test for exactly
  this subsystem, since driving the real interactive shell through a keyboard-injected `fork`+
  `execve`+`wait` round trip isn't something `cargo test` can do. `fork-exec-smoke` is a minimal
  freestanding binary that forks, has its child write a marker and exit with a distinctive code
  (`77`), has its parent `wait4` and verify both the reaped pid and exit code, then reports
  pass/fail through a syscall number no real ABI uses (`9999`) — `tests/fork_wait.rs` registers a
  handler for that number directly against `oxidebsd::syscall::oxidebsd_register_syscall` (made
  `pub`, not `pub(crate)`, specifically so an external test crate can do this) that calls
  `qemu::exit_qemu`, sidestepping the fact that `scheduler::start`/`process::do_exit` never return
  control to a test's own `main` the way a normal QEMU-exit-based test does. Deliberately narrower
  than a full `stsh`-driven test: no `execve`, no filesystem, isolating the process/scheduler/
  context-switch machinery itself from FAT32/ELF-loading concerns. This test is exactly what
  caught the `PageAlreadyMapped` bug in an early, broken `AddressSpace::fork` before it shipped.

## Dynamic kernel modules (`src/module.rs`, `modules/*`)

Loads independently-compiled, relocatable (`ET_REL`) `#![no_std]` code into the kernel's own,
currently active address space at boot: relocates it, resolves the handful of symbols it
references against a small hand-curated kernel API, and calls its `module_init`. Not a static
compile-time registry — real runtime relocation and symbol resolution, closer in spirit to a
(much smaller, much more constrained) Linux kernel module than to a plugin system. Three modules
exist so far: `modules/hello/` (trivial, proves the mechanism), `modules/native_abi/` (populates
`src/syscall.rs`'s dispatch table — see "Syscall ABI" above), `modules/fat32/` (see "FAT32
filesystem module" below).

This is a genuinely different job from `src/elf.rs`'s userland loader: `elf.rs` loads a handful of
`PT_LOAD` segments from a statically-linked, non-relocatable `ET_EXEC` binary with zero
relocations. A relocatable object has no program headers at all — what it has instead is
potentially hundreds to thousands of small linker sections (one per function/global, before any
size optimization), a symbol table, and relocation entries that must be resolved and applied by
hand. `src/module.rs` shares only low-level "read an ELF64 field" helpers with `elf.rs`
(`crate::elf::read_u{16,32,64}`); its loading logic is independent, and is itself the largest new
subsystem this feature added (~500+ LOC, comparable in scope to `elf.rs` + `address_space.rs`
combined).

- **Module crates are plain `#![no_std]` `lib` crates** — no `_start`, no linker script, no final
  link, and (unlike `userland/*/`) no per-crate `build.rs` either. `build.rs`'s
  `build_module_crate` cross-builds each one via `cargo rustc --release --lib --target-dir
  target/modules -- --emit=obj -C codegen-units=1`, which produces exactly one relocatable
  (`ET_REL`) object and skips the link step entirely — confirmed empirically for this project's
  exact target/toolchain before relying on it.
- **`RUSTFLAGS="-C relocation-model=static"`, scoped to only this nested build (never the outer
  kernel build or `userland/*`'s own), eliminates GOT-indirected (`R_X86_64_GOTPCREL`)
  relocations almost everywhere** — including inside the precompiled `core`/`alloc` this nested
  `-Z build-std` invocation produces, which doesn't inherit the trailing `--emit=obj`-style flags,
  only `RUSTFLAGS`. In exchange, every module's mapped virtual-address region must stay within the
  low 2 GiB (`src/module.rs`'s `MODULE_VA_BASE = 0x_1000_0000`, `MODULE_REGION_CEILING =
  0x_8000_0000`) — the two absolute 32-bit relocation forms this makes everything resolve to
  (`R_X86_64_32`/`32S`) would otherwise silently truncate/corrupt. **"Almost everywhere" turned out
  to matter**: `core::panicking::panic_bounds_check`'s own internal panic-message formatting still
  references a numeric `Display::fmt` impl via `GOTPCREL`, unavoidably, in any module whose code
  does ordinary slice indexing — i.e. essentially all of them. Discovered when `modules/fat32/`'s
  first real (non-trivial) boot attempt needed it. Rather than eliminate this one further (not
  possible without avoiding all indexing, an unreasonable constraint), `src/module.rs`'s loader
  implements a **minimal, eagerly-populated GOT**: one 8-byte slot per `R_X86_64_GOTPCREL` site (no
  dedup — a module has at most a handful, not worth the bookkeeping), allocated in the module's own
  region right after its placed sections, populated with the already-resolved symbol address at
  relocation-application time (no lazy binding — nothing to defer, every symbol is resolved right
  there). `R_X86_64_GOTPCREL`'s formula (`G + GOT + A - P`) is then just `R_X86_64_PC32`'s own
  formula with the GOT slot's address standing in for the symbol's — `apply_relocation`'s `PC32`
  branch is reused directly rather than duplicated.
- **A mandatory build-time partial relink** (`rust-lld -flavor gnu -r`, a *relocatable* merge, not
  a final link) merges each module's own object against the exact `core`/`alloc`/
  `compiler_builtins` `.rlib`s that same `-Z build-std` invocation produced (paths discovered via
  `std::fs::read_dir` at build time — filenames carry a non-deterministic metadata hash). Without
  this, a module's undefined-symbol set is open-ended and code-content-dependent — confirmed by
  building real test modules and finding things like `core::fmt::write`,
  `<u32 as LowerHex>::fmt`, `memcpy`, and the panic-entry symbol itself, none of which a hand-
  curated kernel API table could practically enumerate in advance. Archives are wrapped in
  `--start-group`/`--end-group` since `core`/`alloc`/`compiler_builtins` reference each other and a
  single linker pass wouldn't otherwise guarantee a resolving order.
- **`--gc-sections -u module_init` on that same relink step is required, not an optional size
  optimization** — an earlier draft of this design assumed it could be deferred. Archive-member
  selection during a `-r` link is coarse (a whole `.o` file, and `-Z build-std`'s own precompiled
  `core`/`alloc` bundle many unrelated functions into a handful of such files) — referencing just
  one symbol from a bundled member pulls in everything else defined alongside it. Concretely: the
  `panic_bounds_check` reference above, left unpruned, pulled in most of `core::fmt`'s numeric and
  Unicode tables, ballooning `modules/fat32/`'s very first build to 3+ MB across ~2900 sections —
  and the kernel-side loader (which uses `alloc`/`BTreeMap` freely, unlike module code) exhausted
  the kernel's small 100 KiB heap just parsing that many section headers, crashing on the very
  first real boot attempt. `-u module_init` marks every module's sole real entry point as a GC
  root (a bare `-r` output has no executable's implicit entry point, so nothing is reachable by
  default without one); `--gc-sections` then prunes everything not transitively reachable from it.
  Brought that same object down to ~60-75 sections.
- **No `core::fmt::Write`/`write!` anywhere in module code — discovered the hard way, not a
  stylistic preference.** An earlier draft of `modules/fat32/`'s logging used `write!` into a
  custom `core::fmt::Write` sink for readability. `core::fmt::Write::write_fmt`'s default
  implementation calls `core::fmt::write(&mut dyn Write, ..)`, and constructing *that* trait
  object's vtable is what actually emits a `GOTPCREL` reference (a different, more severe
  occurrence than the panic_bounds_check one above, and — before `--gc-sections` was added —
  responsible for most of that 3+ MB/2900-section bloat on its own). None of the simpler
  `{}`/`{:x}`-on-a-primitive cases this GOT design was first validated against exercise this path.
  Modules use hand-rolled byte-level formatting instead (see `modules/fat32/`'s `ByteBuf`).
- **Modules do not use `alloc`/`Vec`/`BTreeMap`.** Not a hard technical impossibility, but avoids
  depending on the internal, unstable-ABI `__rust_alloc`-family symbols `#[global_allocator]`
  wires up (whether a relocated-by-hand module could safely resolve and call through those wasn't
  worth the risk to validate). Module state instead lives in fixed-size `static mut` arrays — same
  pattern already established as load-bearing for `gdt.rs`'s RSP0/IST stacks (see "User-mode
  execution" above), for a *different* underlying reason (see the next point). Kernel-side code
  that a module merely *calls into* (the loader itself, the syscall registry, the fd registry) is
  unaffected — `alloc` is fine there, since it's ordinary kernel code, not relocated module code.
- **A new `static mut` gotcha, distinct from `gdt.rs`'s own.** `gdt.rs`'s stacks need `static mut`
  because a plain `static`, never Rust-visibly written, gets interned into `.rodata` by the
  optimizer (the actual writes are hardware-only, invisible to that analysis). The risk found
  here is the *opposite* direction: a **private** `static mut` buffer that *is* written by real,
  Rust-visible code, but whose write is never observably read back through an *externally visible*
  function, can have that write (and the computation feeding it) deleted entirely as an
  unobservable dead store — confirmed via a controlled test. `modules/fat32/`'s `DISK` buffer
  (module_init copies the embedded read-only template into it once) avoids this because every read
  of it happens from within exported, syscall-registered handler functions whose results feed
  observably into `oxidebsd_log` calls or syscall return values — the optimizer can't prove any of
  that away. Any *future* module state with a similar "write once, read later" shape needs this
  same discipline: make sure something externally reachable actually reads it back.
- **Modules are mapped kernel-only (no `USER_ACCESSIBLE`), and every page gets `WRITABLE`
  regardless of section type.** Module code runs only in kernel context — invoked via
  `module_init` at load time, and (for `native_abi`/`fat32`) via syscall-registry/fd-registry
  callbacks the kernel itself calls into directly — never executed by ring-3 code, so no
  `USER_ACCESSIBLE` bit is needed even in address spaces that later shallow-copy the kernel's live
  table (see "Address spaces are a shallow copy" above). Every page (including ones backing
  `.text`-equivalent sections) is `WRITABLE` because relocation application must patch bytes
  inside them; this kernel doesn't implement `NO_EXECUTE`/W^X anywhere yet (same simplification
  `elf.rs`'s own doc comment already calls out), so there's no protection benefit to a stricter
  per-section split today.
- **Panic inside a module, concretely answered.** A `lib` crate can't define its own
  `#[panic_handler]` (only `bin` crates may), and `panic-strategy = "abort"` is the only strategy
  this target supports anyway — no unwinding to reason about. Every panicking-path function in a
  module's merged `core`/`alloc` code ultimately calls a fixed, compiler-synthesized symbol for the
  panic entry point (`core::panicking`'s own internal `rust_begin_unwind` declaration). Its exact
  mangled name embeds a toolchain-dependent crate-metadata hash not worth hardcoding — `build.rs`'s
  `discover_panic_symbol` finds it per-module via `llvm-nm --undefined-only` and a substring search
  for `rust_begin_unwind` (Rust's v0 mangling spells path components out as length-prefixed text,
  so the literal name still appears inside the hash-bearing mangled symbol). `src/module.rs`'s
  resolver has one fixed, non-optional entry pointing that symbol at `module_panic_trampoline`,
  which logs `[module] panic: <PanicInfo>` and calls `hlt_loop()` — a module panic is exactly as
  fatal as a kernel panic, just logged with a different prefix. `module_panic_trampoline` is
  declared `extern "Rust"` (not `"C"`) to match how `core::panicking` itself declares this symbol —
  relying on both sides being compiled by the very same rustc invocation's ABI for a plain
  single-reference-argument function, which isn't an officially stable guarantee but holds in
  practice within one compiler version.
- **The loader's own two-pass placement.** Pass 1 walks every `SHF_ALLOC` section (the ones that
  actually consume runtime memory — non-`SHF_ALLOC` sections like `.comment`/relocation/symbol-
  table sections are skipped entirely), assigning each a bump-allocated offset within the module's
  region respecting `sh_addralign`. Pass 2 maps a page-aligned region (via `allocate_region`, a
  bump allocator over the fixed low-2GiB range, mirroring `BootInfoFrameAllocator`'s own "hand out
  forward, never reuse" philosophy — no module unload/reload exists yet, so there's nothing to
  reclaim), then copies `SHT_PROGBITS` bytes in (`SHT_NOBITS`/`.bss`-equivalent sections are
  already zeroed by the fresh frame allocation, so nothing further is needed for them). Pages are
  mapped into the kernel's own **currently active** table (not a separate, not-yet-active one the
  way `elf.rs`'s userland loading targets) — so unlike `elf.rs`, relocation writes and section
  copies go through ordinary virtual pointers directly, no physical-memory-offset indirection
  needed.
- **The five other relocation types applied (`R_X86_64_64`, `R_X86_64_32`, `R_X86_64_32S`,
  `R_X86_64_PC32`, `R_X86_64_PLT32`) are the complete set observed empirically** across every
  module tried (plain calls/data references, `core::fmt`-heavy code including what
  `panic_bounds_check` itself references, large static-buffer fills/copies). `PLT32` is resolved
  exactly like `PC32` (direct-referencing the real target, no actual PLT/lazy binding — correct
  whenever, as here, every symbol is eagerly resolved at load time). The two absolute 32-bit forms
  validate that the computed value actually fits before writing — a computed address that doesn't
  losslessly fit returns `ModuleError::RelocationOverflow` rather than silently truncating and
  corrupting the write, the same "fail loud, not silent" discipline `Star::write`'s own validation
  already established elsewhere in this codebase. An unrecognized relocation type is reported the
  same way, not ignored — a module built with different codegen could plausibly need one this
  loader doesn't handle yet.
- **`serial_println!` can't use implicit `{name}`-style format-string captures — only
  `serial_print!` can.** Discovered while writing `src/module.rs`'s own logging:
  `serial_println!`'s macro expansion wraps its format string in `concat!($fmt, "\n")`, and
  `concat!`-produced format strings can't capture variables from the surrounding scope (a hard
  compiler error, not a lint) — `serial_print!` doesn't have this problem since it doesn't go
  through `concat!`. Every `serial_println!` call anywhere in this codebase (not just
  `module.rs`) already uses explicit positional arguments for this reason; new call sites should
  follow the same pattern rather than reaching for `{variable}` captures.
- **Known limitations, deliberate for this pass:** no module unload/reload, no versioning, no
  inter-module direct calls (only module → kernel, via each module's own resolved symbol table —
  this is *why* `src/fd.rs`'s registry exists at all, as the only coordination point two modules
  like `native_abi` and `fat32` have). The `--gc-sections`-driven object-size reduction above is
  itself further improvable (fewer, coarser-grained sections to begin with) but wasn't pursued
  further once it solved the actual crash it was needed for.

## FAT32 filesystem module (`modules/fat32/`, `src/fd.rs`)

A basic FAT32 filesystem, loaded as a dynamic kernel module (see above) and backed by a small,
build-time-generated, embedded in-memory disk image — there's no real block device driver yet, so
this is squarely a filesystem-*format* proof, not a storage-*driver* one. Read and write are both
implemented; writes mutate the in-memory working copy only and **do not persist across reboot**.

- **The disk image is hand-generated at build time, not `mkfs.fat`-produced.** `build.rs`'s
  `write_fat32_image` writes real FAT32 structures (BPB, FSInfo sector, 2 mirrored FAT copies with
  32-bit entries, root directory as a proper cluster chain rather than FAT16's fixed region) — but
  at ~2 MiB total, far below Microsoft's conventional FAT32 minimum-volume-size heuristic (real
  `mkfs.fat` wants tens of megabytes, to guarantee ≥65525 clusters — impractical to embed). Since
  only this module's own hand-rolled parser ever reads the image, deliberately violating that
  heuristic (while staying structurally correct otherwise) is safe — the same "authenticity nod,
  not compatibility claim" spirit already used for syscall numbers elsewhere in this codebase.
  Generating it with own code rather than shelling out to `mkfs.fat` also keeps the build hermetic
  — no new required host tool, consistent with `cargo build` needing no manual pre-step anywhere
  else in this repo.
- **Four files ship in the image**, embedded via the generator: `HELLO.TXT` (a short, single-
  cluster message), `BIG.TXT` (1224 bytes spanning 3 clusters, content generated by a formula —
  `b'A' + index % 26` — rather than a literal, so `modules/fat32/`'s own self-check can
  independently recompute the expected bytes instead of keeping a second copy of a large literal in
  sync by hand; specifically exercises cluster-chain-following, not just single-cluster reads),
  `SMOKE.ELF` (the built `userland/ring3-smoke/` binary, embedded at build time — see "Process
  abstraction, scheduler, and fork/exec/wait" above — so `stsh`'s `execve` support has a real,
  already-working file to run), and `MUSL.ELF` (the built `userland/musl-smoke/` binary — see
  "musl port" above). Both ELF files are chained across as many clusters as their actual built size
  needs (computed at image-generation time, `MUSL.ELF`'s first cluster chained right after however
  many `SMOKE.ELF` itself ends up needing) rather than a fixed cluster count like `BIG.TXT`'s.
- **Deliberate simplifications, all documented in the module's own doc comment rather than
  accidental:** 8.3 short names only (no VFAT/long-filename entries); no directory's own cluster
  chain is ever extended once full (`create_file`/`sys_mkdir` return `DirectoryFull`/`ENOSPC`
  instead — fine for this module's tiny demo scale, a real gap for heavier use); sequential reads
  via an internal cluster-chain walk (no `lseek`); writes only ever *create* a brand-new file with
  its complete contents in one logical operation — no append, no truncate, no rewriting an existing
  file (name collisions aren't even checked for at the FAT32-logic level for files; `SYS_OPEN`'s
  handler is responsible for that — `sys_mkdir` *does* check, returning `EEXIST`).
- **Subdirectories exist, but the path grammar is deliberately a single component at a time** —
  `resolve_dir`/`to_short_name` accept `""`/`"."` (current directory), `"/"` (root), `".."`
  (current directory's parent, read from its own `..` entry), or one plain name, optionally
  `/`-prefixed to resolve against root instead of the current directory — never a multi-level path
  like `a/b/c` in a single call (`to_short_name` rejects any embedded `/` outright, returning
  `EINVAL`). Real shells already build multi-level navigation out of repeated single-component
  `cd`s, so this isn't a real capability gap for `stsh`'s own use, just a bound on what any one
  syscall call understands. A subdirectory's `..` entry follows FAT32's own real (if surprising)
  convention: its cluster field is `0` when the parent is *root* specifically (inherited from
  FAT12/16, where root had no cluster number at all, being a separate fixed region), not root's
  actual cluster number — `parent_of` translates that back on read, `sys_mkdir` writes it correctly
  on create.
- **There is exactly one, kernel-wide "current directory" (`CURRENT_DIR_CLUSTER`), not a
  per-process one — a real, now-live limitation, not just a hypothetical.** Real processes exist
  now (see "Process abstraction, scheduler, and fork/exec/wait" below), so this is no longer "there
  was nothing to scope it to": one process's `cd` genuinely does affect every other process's
  relative paths today, kernel-wide global state shared across `fork`ed/`execve`d children exactly
  like it's shared with the shell itself. Not fixed this pass — flagged as a known limitation, not
  required for fork/exec/wait correctness, but a real gap for any future multi-process filesystem
  use beyond a single interactive shell. `static mut` for the same reason `DISK`/`OPEN_FILES` are
  (see below): every read of it happens from within this module's own exported, syscall-reachable
  functions, so the optimizer can't prove a write to it unobservable.
- **A real aliasing bug, found and fixed before it shipped: `module_init`'s own self-check must
  never call `sys_mkdir`/`sys_chdir` while its own `&mut DISK` reference is still "live" in the
  compiler's sense (i.e., used again afterward).** Both functions independently derive their own
  fresh `&mut DISK` internally; if the caller's own reference is considered live across that call
  (Rust's NLL tracks a binding's live range through to its *last actual use*, not its lexical
  scope, but if the outer binding *is* used again later, its live range spans the call regardless),
  that's two simultaneously-live exclusive references to the same static — real, LLVM-exploitable
  undefined behavior, not just a style nit, since `&mut` carries a noalias guarantee the optimizer
  is entitled to rely on. The subdirectory portion of `module_init`'s self-check calls
  `sys_mkdir`/`sys_chdir` and *then* derives a deliberately fresh reference (`disk2` in the source,
  not a reuse of the function's original `disk` binding) for everything after — the original
  binding's last use is earlier, in the root-level checks, so there's no overlap. Any future
  self-check code added in the same function needs the same discipline: once you're going to call
  another exported function that itself touches `DISK`, stop using your own outstanding reference
  to it, don't just assume "I'm not writing through it right now" is enough.
- **Writes are all-at-once-on-`close`, not incremental per `write` call.** `open(..., O_CREAT)` on
  a new name allocates an `OpenFile::Write` slot with a fixed `[u8; MAX_FILE_BUFFER]` (`65536`
  bytes — raised twice now: `4096` → `16384` once `execve` support needed to read `SMOKE.ELF`'s
  few-KB debug build back whole, then `16384` → `65536` for `MUSL.ELF` (~23 KiB, a real compiled C
  binary against a real libc — see "musl port" above); see "Process abstraction, scheduler, and
  fork/exec/wait" above) accumulator; each `SYS_WRITE` call appends into it; the file is only
  actually committed to
  `DISK` (cluster allocation, FAT chaining, directory-entry creation, all via `create_file`) at
  `close` time. A file opened for reading instead caches its *entire* contents into a same-sized
  fixed buffer at `open` time (rather than walking the cluster chain incrementally per `read`
  call) — simpler, and reuses the same "whole file into a fixed buffer" shape already established
  by this module's self-check, at the cost of capping both readable and writable file size at
  `MAX_FILE_BUFFER` (`open` returns `EFBIG` past that on the read side) — `execve`-ing something
  the size of `stsh` itself (tens of KB) would still need either a larger cap or a streaming read
  path, real follow-up work, not attempted here.
- **Testing note: no `#[test_case]` unit tests exist for this module's parsing logic, by
  necessity, not oversight.** `modules/fat32/` is compiled entirely independently of the kernel
  (see "Dynamic kernel modules" above) and only ever runs as relocated, loaded module code — the
  kernel's `#[test_case]`-based framework (`src/lib.rs`) has no way to reach into a separately-
  compiled module crate at all, and duplicating this parsing logic into a second, kernel-side copy
  purely for test coverage would risk the two silently drifting apart. Instead, `module_init` runs
  a self-check against its own real, already-loaded code (parses the embedded image, lists the
  root directory, reads `HELLO.TXT`/`BIG.TXT` back and compares, then exercises the write path by
  creating a throwaway file and reading it back) and logs `[fat32] self-check passed` or a specific
  `FAILED` reason over serial — the same "boot in QEMU and self-report" testing philosophy this
  codebase's "Test architecture" section already establishes for the kernel as a whole, just
  applied one level deeper.
- **`ls` reuses `open`/`read`/`close` rather than a dedicated syscall.** `fat32_open`, when the
  resolved target is a directory (including the current directory itself, or root), doesn't cache
  file content into the `OpenFile::Read` slot it registers — it formats a listing (name plus
  `<DIR>` or a byte count, one per line, `.`/`..` hidden) into that same buffer instead
  (`open_directory_listing`). Nothing downstream (`fat32_read`, `stsh`'s `ls`) needs to know or
  care that the bytes came from a listing rather than a real file — same fd-registry callbacks,
  same read loop. A pragmatic simplification, not a claim of matching real `open()`-on-a-directory
  semantics (which hands back a fd meant for `getdents`/`readdir`, not raw bytes).
- **Syscall integration and the fd registry (`src/fd.rs`).** `modules/fat32/` registers
  `SYS_OPEN`/`SYS_CLOSE`/`SYS_CHDIR`/`SYS_MKDIR` directly against the same syscall dispatch table
  `native_abi` uses (see "Syscall ABI" above). `SYS_READ`/`SYS_WRITE` themselves stay owned by
  `native_abi`/
  `src/syscall.rs`, though — for any fd that isn't stdin/stdout, they now delegate to `src/fd.rs`'s
  small kernel-owned registry, which this module's `fat32_open` populates (via
  `oxidebsd_alloc_fd`/`oxidebsd_register_fd_ops`) with per-fd read/write/close callbacks into its
  own code. This registry exists *specifically* because modules can't call each other directly —
  only module → kernel, via each module's own independently resolved symbol table — so routing a
  read/write for a fd `fat32` owns, issued through syscall machinery `native_abi` owns, has no
  path except through a kernel-resident coordination point. `SYS_CLOSE`'s handler delegates to the
  kernel's `oxidebsd_close_fd` (which removes the registry entry *then* invokes the module's own
  `fat32_close` callback), not directly to `fat32_close` — so a closed fd is also no longer
  reachable via `SYS_READ`/`SYS_WRITE` afterward, not just cleaned up on the FAT32 side. Like
  `BootInfoFrameAllocator` and `module::allocate_region`, `src/fd.rs`'s fd numbers are a simple
  bump counter — never reused, even after `close`.
- **`stsh`'s `cat`/`write`/`cd`/`ls`/`mkdir` (see "Interactive shell" above) are the end-to-end
  proof**: `cat` opens read-only and streams bytes via `SYS_READ` until it returns `0` (clean EOF —
  a FAT32 read never needs busy-polling the way stdin's non-blocking `SYS_READ` does); `write`
  opens with `O_CREAT`, issues one `SYS_WRITE`, then closes. Verified by booting in QEMU and
  driving `stsh` via injected keystrokes (same method used throughout this codebase's manual
  verification passes): `write foo hello world` → `wrote 11 bytes`; `cat foo` → `hello world`;
  `cat hello.txt` → the embedded demo file's real contents; `cat nope` → `errno 2` (`ENOENT`);
  `mkdir projects` then `cd projects` then `ls` shows an empty listing (a genuinely distinct
  directory, not an alias for root); `write notes hi there` inside it, followed by `ls`, shows
  `NOTES` there and *not* back in root's own listing after `cd ..`; `cat /hello.txt` from inside
  `projects` (root-relative, despite the current directory) still finds the original demo file.

## Dependency notes

- `x86_64` crate is pinned with `default-features = false, features = ["instructions",
  "abi_x86_interrupt"]`. The default feature set pulls in `step_trait`, which implements the
  unstable `core::iter::Step` trait — that trait's shape is a moving target on nightly and the
  crate has broken against newer nightlies before. `instructions` (port I/O, `hlt`, GDT/IDT/TSS
  loads, etc.) and `abi_x86_interrupt` (needed for `idt::Entry::set_handler_fn` and the
  `extern "x86-interrupt"` handler types used in `src/interrupts.rs`) are both needed explicitly —
  without `abi_x86_interrupt` those handler function types compile as opaque, non-constructable
  structs instead of real function pointers, since it's normally bundled into the (disabled)
  `nightly` feature. The `#![feature(abi_x86_interrupt)]` crate attribute in `src/lib.rs` is the
  separate, unstable-Rust half of the same requirement — the crate feature and the language
  feature are two different gates for the same thing.
- The `bootloader` crate is pinned to `0.9`, not the current `0.11+`. The newer API drops the
  `bootimage` tool in favor of cargo artifact-dependencies to embed the kernel into a separate
  builder crate, which is a bigger structural change; `0.9` was chosen to keep the setup in one
  crate for now. Revisit if artifact-dependencies become worth the migration.
- `bootloader` has the `map_physical_memory` feature enabled — without it, `BootInfo` has no
  `physical_memory_offset` field at all (it's `#[cfg]`'d out crate-side), and `src/memory.rs`
  can't get from a physical frame (e.g. the one `CR3` points at) to a virtual address it can
  dereference.
- `linked_list_allocator` is pinned with `default-features = false`, skipping its default
  `use_spin`/`spinning_top` features. Those features provide a ready-made `LockedHeap` type, but
  it's built on the `spinning_top` crate — a second, separate spinlock implementation alongside
  the `spin` crate already used everywhere else in this codebase (`serial.rs`, `vga.rs`,
  `interrupts.rs`'s `KEYBOARD`). `src/allocator.rs` instead wraps the crate's plain `Heap` type in
  a local `Locked<T>` built on `spin::Mutex`, so there's one spinlock crate in the dependency
  graph, not two.
- `pc-keyboard` 0.9's constructor type is `PS2Keyboard<L, S>` (older tutorials/blog posts online
  reference a `Keyboard<L, S>` type from pre-0.9 versions — that name no longer exists). Decoding a
  scancode is two steps, not one: `add_byte` turns a raw byte into a `KeyEvent` (key up/down plus
  which key), then `process_keyevent` turns that into a `DecodedKey` (a `char` or a raw `KeyCode`)
  using the keyboard's layout/modifier state — both must be called through the same locked
  `KEYBOARD` guard in `src/interrupts.rs`, not two separate `.lock()` calls, since `spin::Mutex`
  isn't reentrant.
- `pic8259` and `uart_16550` are deliberately *not* dependencies, unlike most from-scratch-OS
  tutorials that pull both in. Both wrap a handful of `outb`/`inb` calls against a well-documented,
  stable hardware protocol (see `src/pic.rs` and `src/serial.rs`) — small and mechanical enough
  that owning the code outweighs the dependency. This is different from `pc-keyboard` (hundreds of
  lines of scancode/layout tables) or `linked_list_allocator` (memory-safety-critical free-list
  logic), which stay external for the opposite reason.
