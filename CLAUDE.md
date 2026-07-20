# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

OxideBSD is a 100% Rust-based BSD-like operating system. It is early-stage, working through
`ROADMAP.md`'s phase 1 (a running, interactive kernel with a shell). So far: GDT/TSS/IDT with a
dedicated double-fault stack, fatal exceptions reboot the machine, PIC-driven hardware interrupts
with a timer tick and a PS/2 keyboard IRQ handler (decodes scancodes and echoes typed characters —
no line editing or shell yet), a VGA text-mode console mirroring serial output, and a heap allocator
backed by bootloader-provided paging. Scheduling, drivers beyond keyboard/timer/serial/VGA, a
filesystem, and syscalls do not exist yet. Architecture decisions for those subsystems have not been
made and should be discussed with the user before large structural commitments are made.

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

There is no `cargo check`/`cargo test` fast path that skips QEMU — every test target is its own
bootable kernel image that QEMU actually boots, so `cargo test` is slow (each target rebuilds a
bootimage and launches an emulator instance).

## Test architecture

There is no libtest/`#[test]` — the kernel is `no_std` and has no OS to run a normal test binary
under, so tests instead boot in QEMU and self-report:

- `src/qemu.rs`: writes to the `isa-debug-exit` QEMU device (port `0xf4`) to make QEMU exit with a
  code that encodes pass/fail. `test-success-exit-code` in `Cargo.toml` (`33`) must stay in sync
  with `QemuExitCode::Success` — QEMU maps a written value `v` to process exit code `(v << 1) | 1`.
- `src/serial.rs`: a serial port (COM1) writer, used for all kernel/test output — read via
  `-serial stdio` (set in `Cargo.toml`'s `[package.metadata.bootimage]`).
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
  `interrupts.rs`'s `PICS`). `src/allocator.rs` instead wraps the crate's plain `Heap` type in a
  local `Locked<T>` built on `spin::Mutex`, so there's one spinlock crate in the dependency graph,
  not two.
- `pc-keyboard` 0.9's constructor type is `PS2Keyboard<L, S>` (older tutorials/blog posts online
  reference a `Keyboard<L, S>` type from pre-0.9 versions — that name no longer exists). Decoding a
  scancode is two steps, not one: `add_byte` turns a raw byte into a `KeyEvent` (key up/down plus
  which key), then `process_keyevent` turns that into a `DecodedKey` (a `char` or a raw `KeyCode`)
  using the keyboard's layout/modifier state — both must be called through the same locked
  `KEYBOARD` guard in `src/interrupts.rs`, not two separate `.lock()` calls, since `spin::Mutex`
  isn't reentrant.
