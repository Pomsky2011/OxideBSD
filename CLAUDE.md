# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

OxideBSD is a 100% Rust-based BSD-like operating system. Phase 1 of `ROADMAP.md` (a running,
interactive kernel) is essentially done: GDT/TSS/IDT with a dedicated double-fault stack, fatal
exceptions reboot the machine, PIC-driven hardware interrupts with a timer tick and a PS/2 keyboard
IRQ handler (decodes scancodes and echoes typed characters — no line editing or shell yet), a VGA
text-mode console mirroring serial output, and a heap allocator backed by bootloader-provided
paging. Phase 2 has started: the kernel can build a *separate* address space (`src/address_space.rs`),
load a static ELF64 binary into it (`src/elf.rs`), and jump into ring 3 (`src/usermode.rs`) — see
the "User-mode execution" section below for the current, deliberate limits of that (no syscalls, no
return path, one demo binary). Scheduling, a real process abstraction, a filesystem, and a syscall
ABI do not exist yet. Architecture decisions for those subsystems have not been made and should be
discussed with the user before large structural commitments are made.

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
target the `oxidebsd` package, even though it's now a workspace — `userland/ring3-smoke` is a
separate member cargo doesn't build by default in a "root package" workspace like this one; the
root package's own `build.rs` cross-builds it as a side effect of building `oxidebsd` (see "User-mode
execution" below). To build/lint/format it directly: append
`--manifest-path userland/ring3-smoke/Cargo.toml` (and, to avoid the target-directory-locking
gotcha described below, `--target-dir target/userland`).

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
- The heap lives at a fixed virtual range (`allocator::HEAP_START`/`HEAP_SIZE`, currently 100 KiB
  at `0x_4444_4444_0000`) chosen to be far from anything else mapped; `allocator::init_heap` maps
  that range page-by-page before handing it to the global allocator.
- The `#[global_allocator]` is `linked_list_allocator`'s `Heap` wrapped in a project-local
  `Locked<T>` (a thin `spin::Mutex` newtype), not the crate's own `LockedHeap` — see Dependency
  notes below for why.
- `oxidebsd::init` now takes `&'static BootInfo` (previously took nothing) because heap setup
  needs `physical_memory_offset` and `memory_map` from it. All three entry points
  (`src/main.rs`, `src/lib.rs`'s `#[cfg(test)]` entry, `tests/basic_boot.rs`) pass their
  `BootInfo` straight through.

## User-mode execution (`src/address_space.rs`, `src/elf.rs`, `src/usermode.rs`)

Proves paging beyond the kernel's own address space, address-space separation, and ELF loading all
work, by loading a tiny embedded userland binary and jumping into ring 3. Deliberately does *not*
include a syscall ABI yet (next up in `ROADMAP.md`'s phase 2) — see `src/main.rs`'s
`run_ring3_smoke_demo` and `userland/ring3-smoke/src/main.rs` for what that means in practice: the
demo binary does some arithmetic, executes `int3`, and the kernel's ordinary (unmodified)
`breakpoint_handler` logs the exception — `code_segment.rpl == Ring3` there is the actual proof.
After the breakpoint returns, the demo just spins forever; there's no way back into the kernel
without a syscall, so this is a one-shot demo, not a process that "finishes."

- **The userland demo build.** `userland/ring3-smoke/` is a separate workspace member (root
  `Cargo.toml` gained a `[workspace]` table; the root package is still its own workspace member,
  same as before). The root package's `build.rs` cross-builds it into `target/userland/` — a
  **separate** target directory from the outer build's own `target/`, not the shared one:
  invoking a nested `cargo build` against the *same* target directory a still-running outer cargo
  invocation already holds a lock on (build scripts run as part of that outer build) can deadlock.
  The resulting ELF's path is exposed via `cargo:rustc-env=RING3_SMOKE_ELF_PATH=...`, so
  `src/main.rs` can `include_bytes!(env!("RING3_SMOKE_ELF_PATH"))` — this keeps `cargo build`/
  `cargo run`/`cargo test` working with no manual pre-step. `userland/ring3-smoke/linker.ld` forces
  a `0x400000` load base, clear of the kernel's own image, heap, and phys-memory-offset window (see
  below); its own tiny `build.rs` passes that script as a link arg to just its own binary, not via
  `RUSTFLAGS`, which would apply to (and force a rebuild of) the `-Z build-std`-compiled
  core/alloc/compiler_builtins shared with the outer build.
- **Address spaces are a shallow copy.** `AddressSpace::new` allocates a fresh frame for a new
  level 4 table and copies all 512 entries from the kernel's currently-active one into it — not a
  "higher half only" split, and not a deep clone: the copy is just raw entries (pointers to
  lower-level tables), so the new address space shares the kernel's code, heap, stacks, and
  phys-memory-offset window with the original. This is deliberate: interrupt/exception handlers
  always run in the kernel's context, regardless of which address space was active when they
  fired, so the kernel must stay identically mapped and reachable no matter what.
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
- **Software interrupt gates need `DPL = Ring3` explicitly.** `int3` executed from ring 3 needs
  `idt.breakpoint`'s gate `.set_privilege_level(PrivilegeLevel::Ring3)` — `set_handler_fn` defaults
  every gate's DPL to `Ring0`. This only matters for *software*-invoked interrupts (`int n`,
  `int3`, `into`, `bound`); hardware-generated exceptions and IRQs bypass the gate's DPL check
  entirely. Getting this wrong doesn't look like a permissions error: it's a `#GP` on the IDT entry
  itself (decode the error code's selector-index bits to confirm — they name the IDT vector).
- **Known simplification:** ELF segments are mapped without `NO_EXECUTE`, even for non-executable
  segments. Enforcing that also requires setting `EFER.NXE`, which deserves its own focused pass
  rather than bundling into this already-large change.

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
