# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

OxideBSD is a 100% Rust-based BSD-like operating system. Phase 1 of `ROADMAP.md` (a running,
interactive kernel) is essentially done: GDT/TSS/IDT with a dedicated double-fault stack, fatal
exceptions reboot the machine, PIC-driven hardware interrupts with a timer tick and a PS/2 keyboard
IRQ handler (decodes scancodes and echoes typed characters — no line editing or shell yet), a VGA
text-mode console mirroring serial output, and a heap allocator backed by bootloader-provided
paging. Phase 2 is underway: the kernel can build a *separate* address space
(`src/address_space.rs`), load a static ELF64 binary into it (`src/elf.rs`), jump into ring 3
(`src/usermode.rs`), and user-mode code can call back into the kernel via two independent syscall
mechanisms — this kernel's own `int 0x80` ABI (`src/syscall.rs`) and a real Linux-compatible
`SYSCALL`/`SYSRET` path (`src/linux_syscall.rs`), aimed specifically at eventually running
unmodified musl/BusyBox binaries. See "User-mode execution", "Syscall ABI", and "Linux-compatible
syscall mechanism" below for the current, deliberate limits (no process abstraction/scheduler, one
demo binary at a time, `sys_write` doesn't validate its pointer, and — for the Linux path — only
`write`/`exit`/`exit_group` are implemented; musl's actual startup needs many more syscalls and is
real follow-up work, not done). Scheduling, a real process abstraction, and a filesystem do not
exist yet. Architecture decisions for those subsystems have not been made and should be discussed
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

`cargo build`/`cargo run`/`cargo test`/`cargo clippy`/`cargo fmt` at the repo root only ever
target the `oxidebsd` package, even though it's now a workspace — `userland/ring3-smoke` and
`userland/linux-syscall-smoke` are separate members cargo doesn't build by default in a "root
package" workspace like this one; the root package's own `build.rs` cross-builds both as a side
effect of building `oxidebsd` (see "User-mode execution" below). To build/lint/format one of them
directly: append `--manifest-path userland/<name>/Cargo.toml` (and, to avoid the
target-directory-locking gotcha described below, `--target-dir target/userland`).

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
work, by loading a tiny embedded userland binary and jumping into ring 3. `src/main.rs`'s
`run_userland_demo` is generic over which binary — `kernel_main` currently points it at
`userland/linux-syscall-smoke/` (see "Linux-compatible syscall mechanism" below); the older
`userland/ring3-smoke/` (this kernel's own `int 0x80` ABI, see "Syscall ABI" below) still works,
it's just not what's wired up by default at the moment, since only one demo can run per boot
(`SYS_EXIT` idles the whole system — there's no scheduler to hand control to something else
afterward). Whichever binary runs, the pattern is the same: it does something observable (prints a
message, exits with a distinctive code), and that output over serial/VGA is the actual end-to-end
proof paging/ELF-loading/ring-3/syscalls all work together.

- **The userland demo build.** `userland/ring3-smoke/` and `userland/linux-syscall-smoke/` are
  separate workspace members (root `Cargo.toml` gained a `[workspace]` table; the root package is
  still its own workspace member, same as before). The root package's `build.rs` cross-builds both
  (via a shared `build_userland_crate` helper) into `target/userland/` — a **separate** target
  directory from the outer build's own `target/`, not the shared one: invoking a nested
  `cargo build` against the *same* target directory a still-running outer cargo invocation already
  holds a lock on (build scripts run as part of that outer build) can deadlock. Each resulting
  ELF's path is exposed via `cargo:rustc-env=<NAME>_ELF_PATH=...`, so `src/main.rs` can
  `include_bytes!(env!("..._ELF_PATH"))` — this keeps `cargo build`/`cargo run`/`cargo test`
  working with no manual pre-step. Each userland crate's `linker.ld` forces its own distinct load
  base (`0x400000` for `ring3-smoke`, `0x500000` for `linux-syscall-smoke` — not currently required
  since only one demo loads at a time, but keeps them from colliding if that changes), clear of the
  kernel's own image, heap, and phys-memory-offset window (see below); each crate's own tiny
  `build.rs` passes its script as a link arg to just its own binary, not via `RUSTFLAGS`, which
  would apply to (and force a rebuild of) the `-Z build-std`-compiled core/alloc/compiler_builtins
  shared with the outer build.
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
- **Rule, not a one-off: every gate ring 3 triggers with a software interrupt needs `DPL = Ring3`
  explicitly.** Interrupt gates default to `DPL = Ring0`, and a *software*-invoked interrupt
  (`int n`, `int3`, `into`, `bound` — as opposed to a hardware exception or IRQ, which bypass the
  gate's `DPL` check entirely) additionally requires `CPL <= gate DPL`. This has bitten this
  project twice now: first `idt.breakpoint` (for `int3`), then `idt[syscall::SYSCALL_VECTOR]` (for
  `int 0x80`) — both need `.set_privilege_level(PrivilegeLevel::Ring3)`. Any *future* software
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

`int 0x80`; syscall number in `RDI`, up to three arguments in `RSI`/`RDX`/`RCX`, return value in
`RAX`. Deliberately *not* the more traditional "number in `RAX`": System V already passes a
function's first four integer arguments in `RDI`/`RSI`/`RDX`/`RCX`, so choosing the same order for
the syscall convention means `syscall_entry` (the hand-written `global_asm!` stub — the first raw
assembly in this codebase beyond `usermode.rs`'s `iretq` frame) can `call syscall_dispatch` with
zero register shuffling; whatever's already in those registers at `int 0x80` is exactly what the
Rust dispatcher's `extern "C" fn(number, arg0, arg1, arg2)` expects.

- **The gate is installed via `set_handler_addr`, not `set_handler_fn`.** `syscall_entry` is a raw
  symbol, not an `extern "x86-interrupt" fn` — that ABI doesn't expose general-purpose registers to
  Rust code at all, only the hardware-pushed `InterruptStackFrame`, which is useless for a
  register-based syscall convention. `set_handler_addr` (unsafe, but not gated behind
  `HandlerFuncType`/`abi_x86_interrupt`) takes any `VirtAddr`.
- **The stub saves/restores every general-purpose register uniformly**, not just the System-V
  caller-saved set (the callee-saved ones — `RBX`/`RBP`/`R12`-`R15` — are technically already
  preserved across the `call` by the Rust ABI's own contract). Redundant for those five, but a
  uniform save/restore is simpler to get right than relying on which specific registers a given
  ABI happens to guarantee, and this is exactly the kind of place a subtle mistake shows up as
  silent post-syscall register corruption in a user program, not a loud crash.
- **Stack alignment was reasoned through, not just eyeballed:** the CPU 16-byte-aligns `RSP` before
  pushing the 5-word interrupt frame (a value not itself a multiple of 16), and the stub then
  pushes exactly 15 general-purpose registers (also not a multiple of 16) — the two odd-alignment
  pushes cancel out, landing exactly on the 16-byte boundary System V requires at the `call`
  instruction. If the pushed-register count or the frame shape ever changes, redo this arithmetic;
  don't assume it still lines up.
- **`sys_exit` doesn't return control to anything.** There's no process abstraction or scheduler,
  so "exiting" means the kernel logs the code and calls `hlt_loop()` — it's the whole system's
  stopping point, not a per-process one.
- **`sys_write` does not validate `[ptr, ptr+len)`** before dereferencing it as user memory — no
  check that the range is mapped, `USER_ACCESSIBLE`, or doesn't reach into kernel-only mappings. A
  bad pointer page-faults, which `page_fault_handler` already handles safely (log + `reboot()`)
  rather than corrupting kernel state, so this is a missing safety net for user programs, not a
  kernel soundness hole — but real validation (walking the active page table to confirm the range)
  is a natural follow-up, not yet implemented.

## Linux-compatible syscall mechanism (`src/linux_syscall.rs`)

A second, independent syscall entry point from `src/syscall.rs` — aimed at compatibility with
*real* x86_64 Linux binaries (which is what an unmodified musl/BusyBox userland actually is)
rather than this kernel's own ABI. Real x86_64 Linux binaries never use `int 0x80` — that's the
32-bit legacy path, and musl's x86_64 target has no fallback to it at all — they use the dedicated
`SYSCALL`/`SYSRETQ` instruction pair: number in `RAX`, up to six arguments in
`RDI`/`RSI`/`RDX`/`R10`/`R8`/`R9` (`R10`, not `RCX`, for the 4th argument, specifically because
`SYSCALL` itself clobbers `RCX`/`R11` to save `RIP`/`RFLAGS`), return value in `RAX`, and Linux's
own syscall numbers (`write` = 1, `exit` = 60, `exit_group` = 231 — the only three implemented so
far; verified against the real kernel headers' well-known values, not guessed).

**Explicitly out of scope here:** an actual musl or BusyBox binary running. musl's startup reads
the auxiliary vector off the initial stack (this kernel's ELF loader/`jump_to_usermode` don't set
one up), needs `arch_prctl` for TLS, needs `mmap`/`brk` for its allocator, and calls a number of
other syscalls only really discoverable by trying and watching what fails — real, substantial
follow-up work, not attempted yet. Verification here is a hand-written raw-assembly test program
(`userland/linux-syscall-smoke/`, no libc at all) that calls `write` then `exit` directly, proving
the *mechanism* in isolation first.

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
- **No automatic stack switch, and no per-CPU `swapgs` — single-core simplification.** Unlike an
  interrupt gate + TSS `RSP0`, `SYSCALL` does not switch stacks: control arrives at
  `linux_syscall_entry` still on the *user's own* stack (now at CPL 0). Real kernels use `swapgs` +
  a per-CPU `IA32_KERNEL_GS_BASE`-anchored structure to safely find a kernel stack; this kernel has
  no SMP/APIC multi-processor support at all yet, so a single global scratch stack
  (`KERNEL_RSP_TOP`/`USER_RSP_SCRATCH`) swapped in by the entry stub is legitimate for now —
  revisit if multiple cores ever show up.
- **The entry stub passes a frame pointer, not loose arguments — Linux's argument registers don't
  line up with System V's.** `src/syscall.rs`'s `int 0x80` stub can `call` straight into Rust
  because its *own* convention was chosen to match System V exactly. Linux's real convention can't:
  its 4th argument is `R10`, System V's 4th parameter register is `RCX`. Rather than shuffle
  registers by hand, `linux_syscall_entry` pushes all saved registers to its stack and passes a
  single pointer (`&mut SyscallFrame`, matching System V's first argument register, `RDI`) to
  `linux_syscall_dispatch`, which reads/writes fields on it directly (`frame.rax` doubles as both
  the incoming syscall number and the outgoing return value slot). `SyscallFrame`'s field order
  must match the stub's push order exactly (last pushed = lowest address = first field).
- **Stack alignment needed explicit padding here, unlike `src/syscall.rs`'s stub.** There, the
  CPU's own 40-byte interrupt-frame push left `RSP` off by 8 before the stub's 15 register pushes,
  and the two odd offsets canceled out. Here, `SYSCALL` doesn't push anything, so `RSP` starts
  exactly 16-aligned at `KERNEL_RSP_TOP`; 15 pushes (120 bytes, not a multiple of 16) alone would
  leave `RSP` misaligned for `call linux_syscall_dispatch`. The stub does an explicit `sub rsp, 8`
  before the pushes (and matching `add rsp, 8` after) to compensate. If the register set or push
  count changes, redo this arithmetic — it's specific to this stub's exact shape, not a general
  rule.
- **`ScratchStack` is `#[repr(align(16))]`**, not a bare `[u8; N]` (which only guarantees 1-byte
  alignment) — needed so its computed top is guaranteed 16-aligned for the reason above. (The
  older RSP0/IST stacks in `src/gdt.rs` aren't similarly annotated and have worked fine in
  practice, but that's not a *guarantee* — worth tightening if they ever become a problem.)
- **Unrecognized syscall numbers return `-ENOSYS` (`-38`, two's-complement in `RAX`)**, matching
  real Linux error-return convention, and log the number — that log line is the intended tool for
  iteratively discovering what musl's startup needs in the follow-up milestone.

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
