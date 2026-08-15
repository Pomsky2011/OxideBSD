# OxideBSD Roadmap

OxideBSD is a 100% Rust BSD-like operating system. The plan is three phases, each a prerequisite
for the next.

## Phase 1 — Minimal environment: a running, interactive kernel

**Goal:** a kernel that boots, stays up, and gives you a shell to type into — not just a kernel
that boots and halts.

**Status:** done. GDT/TSS/IDT with a dedicated double-fault stack, PIC-driven interrupts (timer +
keyboard), a heap allocator, a VGA console, and a real interactive shell all exist — see
`CLAUDE.md` for full detail. (`stsh`, the original hand-written shell described below, has since
been superseded as pid 1 by BusyBox's `hush` — see Phase 2.)

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

Phase 1 is "done" when the kernel boots into that shell and stays responsive to input indefinitely
— met.

## Phase 2 — Getting Rust running on it

**Goal:** run actual Rust programs under OxideBSD — not the kernel binary itself, but separate
programs the kernel loads and executes. The end target of this phase is running `rustc`/`cargo`
themselves as userland programs.

**Status:** far along, but not "done" by this phase's own stated bar. Every milestone below is
built except the last — a C libc (musl), not a Rust `std` port, ended up being the actual
libc/userland story that got this phase moving (see `CLAUDE.md`'s musl-port section), and
`rustc`/`cargo` running as OxideBSD processes hasn't been attempted yet. Current `v0.2.x` work
(below) is deepening the C-toolchain side of userland (glibc, GCC/Clang, full POSIX syscall
coverage) rather than attacking `rustc`/`std` directly — a deliberate detour, not abandonment of
this phase's actual goal.

Depends on phase 1's interactivity, plus:

- **Paging / address spaces** — real virtual memory, one address space per process, page fault
  handling. Done.
- **User/kernel privilege separation** — ring 3 execution, a context switch between processes.
  Done.
- **ELF loading** — load a separate binary from somewhere and execute it as a process. Done.
- **Syscall ABI** — a defined interface for user programs to ask the kernel for services (I/O,
  memory, process control). Done — OxideBSD's own native ABI, see `CLAUDE.md`'s Syscall ABI
  section.
- **A filesystem** — at minimum something to load programs from; doesn't need to be persistent to
  start (an in-memory/initrd-style filesystem is a reasonable first cut). Done and then some —
  `oxfs`, a real Unix-shaped inode/block filesystem with optional disk persistence, superseded an
  earlier, more limited FAT32 implementation.
- **A libc/std story for userland** — either a `#![no_std]`-only userland to start, or porting
  `std` to a custom `x86_64-unknown-oxidebsd` target (the harder but more useful path, since
  `rustc`/`cargo` assume `std`). Landed differently than either option here: a real port of musl
  (a C libc) to this kernel's native syscall ABI, which in turn let BusyBox and a real C compiler
  (`tcc`) run as userland. A Rust `std` port remains undone and is what this phase's "done" bar
  below still actually requires.

Phase 2 is "done" when `rustc` can run as an OxideBSD process and compile a program — not yet met.

## Phase 3 — Self-hosting: OxideBSD builds itself

**Goal:** close the loop — an OxideBSD instance can build a new, bootable OxideBSD image using
only tools running under OxideBSD itself, with no host OS involved.

**Status:** not started on the Rust-toolchain side this phase originally describes. `v0.2.x`'s
goals (below) are a first step toward self-hosting from the C side instead — a real glibc port,
self-hosting C-side toolchain components, and retiring `tcc` for real GCC/Clang — ahead of, not
instead of, eventually closing this loop for `rustc`/`cargo` themselves.

- The full build toolchain (`rustc`, `cargo`, a linker, an assembler) running as userland programs.
- Enough of a POSIX/BSD-like surface (process spawning, file I/O, environment variables, pipes)
  for that toolchain to actually function, not just execute trivial programs.
- Build tooling to fetch/vendor the kernel and userland source trees and drive a full rebuild from
  within the running OS.
- A working bootstrap: boot an OxideBSD image, rebuild OxideBSD from source on it, boot the result.

## v0.2.x goals

Concrete near-term targets for the `v0.2.x` line (see `CLAUDE.md`'s TinyCC section for why this is
a much bigger lift than TinyCC — real subprocess pipelines, likely real dynamic linking and
threads, none of which this kernel supports yet):

- **A real glibc port**, alongside or eventually beside the existing native-ABI musl port.
- **Self-hosting C-side components** — real C toolchain pieces (not just `tcc`) built and able to
  run on-target, moving further into Phase 3's "build itself" goal from the C side first.
- **Retire `tcc` in favor of GCC and Clang** once both are real, working on-target ports — TinyCC
  was always the first/easiest target (see `CLAUDE.md`), not the intended long-term C compiler.
- **Full POSIX syscall coverage** — implement every POSIX-mandated syscall, even where this ABI's
  own number/shape (see `CLAUDE.md`'s Syscall ABI section) diverges from Linux's or any real BSD's.
  Not a promise to match Linux/BSD numbering or wire format, just POSIX-complete coverage under
  OxideBSD's own invented ABI.
- **Real text editors: `nano` and real `vim`** — BusyBox's roster today only has the small `vi`
  applet (see `docs/BUSYBOX_APPLETS.md`); `nano` and full (non-BusyBox) `vim` are separate ports,
  for meaningfully better on-target text editing than the current applet-only story.
