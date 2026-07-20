# OxideBSD Roadmap

OxideBSD is a 100% Rust BSD-like operating system. The plan is three phases, each a prerequisite
for the next.

## Phase 1 — Minimal environment: a running, interactive kernel

**Goal:** a kernel that boots, stays up, and gives you a shell to type into — not just a kernel
that boots and halts.

**Status:** the bootable skeleton exists (custom `no_std` x86_64 target, QEMU-backed test harness
— see `CLAUDE.md`), but it currently boots, prints one line over serial, and halts. Everything
below is still to build.

Milestones, roughly in dependency order:

- **CPU structures** — GDT, TSS, IDT, with exception handlers and a separate stack for double
  faults (a bug here otherwise triple-faults and silently reboots the VM).
- **Interrupts** — PIC (or APIC) initialization, a timer tick (PIT or APIC timer), and a keyboard
  IRQ handler.
- **Heap allocation** — a global allocator so `alloc` (`Vec`, `String`, `Box`, ...) is usable; a
  lot of later work assumes this exists.
- **Console output** — VGA text-mode buffer as the primary display (serial has been the console so
  far and can remain the logging/debug channel).
- **Keyboard input** — scancode-to-keycode translation (e.g. via the `pc-keyboard` crate) feeding
  a line-editing input buffer.
- **Shell** — a command loop that reads a line, dispatches to a small set of built-ins (`help`,
  `echo`, memory/heap stats, a deliberate panic for testing the panic handler, etc.), and loops
  forever instead of halting.

Phase 1 is "done" when the kernel boots into that shell and stays responsive to input indefinitely.

## Phase 2 — Getting Rust running on it

**Goal:** run actual Rust programs under OxideBSD — not the kernel binary itself, but separate
programs the kernel loads and executes. The end target of this phase is running `rustc`/`cargo`
themselves as userland programs.

Depends on phase 1's interactivity, plus:

- **Paging / address spaces** — real virtual memory, one address space per process, page fault
  handling.
- **User/kernel privilege separation** — ring 3 execution, a context switch between processes.
- **ELF loading** — load a separate binary from somewhere and execute it as a process.
- **Syscall ABI** — a defined interface for user programs to ask the kernel for services (I/O,
  memory, process control).
- **A filesystem** — at minimum something to load programs from; doesn't need to be persistent to
  start (an in-memory/initrd-style filesystem is a reasonable first cut).
- **A libc/std story for userland** — either a `#![no_std]`-only userland to start, or porting
  `std` to a custom `x86_64-unknown-oxidebsd` target (the harder but more useful path, since
  `rustc`/`cargo` assume `std`).

Phase 2 is "done" when `rustc` can run as an OxideBSD process and compile a program.

## Phase 3 — Self-hosting: OxideBSD builds itself

**Goal:** close the loop — an OxideBSD instance can build a new, bootable OxideBSD image using
only tools running under OxideBSD itself, with no host OS involved.

- The full build toolchain (`rustc`, `cargo`, a linker, an assembler) running as userland programs.
- Enough of a POSIX/BSD-like surface (process spawning, file I/O, environment variables, pipes)
  for that toolchain to actually function, not just execute trivial programs.
- Build tooling to fetch/vendor the kernel and userland source trees and drive a full rebuild from
  within the running OS.
- A working bootstrap: boot an OxideBSD image, rebuild OxideBSD from source on it, boot the result.
