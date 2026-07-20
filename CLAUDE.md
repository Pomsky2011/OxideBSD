# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

OxideBSD is a 100% Rust-based BSD-like operating system. It is early-stage: a bootable, testable
`no_std` kernel skeleton exists (boots to a serial "kernel booting" message and halts), but no
kernel subsystems (memory management, scheduling, drivers, filesystem, syscalls, etc.) have been
built yet. Architecture decisions for those subsystems have not been made and should be discussed
with the user before large structural commitments are made.

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

## Dependency notes

- `x86_64` crate is pinned with `default-features = false, features = ["instructions"]`. The
  default feature set pulls in `step_trait`, which implements the unstable `core::iter::Step`
  trait — that trait's shape is a moving target on nightly and the crate has broken against newer
  nightlies before. Only `instructions` (port I/O, `hlt`, etc.) is actually used.
- The `bootloader` crate is pinned to `0.9`, not the current `0.11+`. The newer API drops the
  `bootimage` tool in favor of cargo artifact-dependencies to embed the kernel into a separate
  builder crate, which is a bigger structural change; `0.9` was chosen to keep the setup in one
  crate for now. Revisit if artifact-dependencies become worth the migration.
