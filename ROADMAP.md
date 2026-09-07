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
`rustc`/`cargo` running as OxideBSD processes hasn't been attempted yet. Current work (v0.2.0
closing the POSIX pilot gap, v0.3.0/v0.4.0 deepening the C-toolchain side of userland with
GCC/Clang and glibc — see "Release sequence" below) is deepening the existing C-based userland
story rather than attacking `rustc`/`std` directly — a deliberate detour, not abandonment of this
phase's actual goal.

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

**Status:** not started on the Rust-toolchain side this phase originally describes. The v0.3.0/
v0.4.0 goals below are a first step toward self-hosting from the C side instead — self-hosting
C-side toolchain components, retiring `tcc` for real GCC/Clang, and a real glibc port — ahead of,
not instead of, eventually closing this loop for `rustc`/`cargo` themselves.

- The full build toolchain (`rustc`, `cargo`, a linker, an assembler) running as userland programs.
- Enough of a POSIX/BSD-like surface (process spawning, file I/O, environment variables, pipes)
  for that toolchain to actually function, not just execute trivial programs.
- Build tooling to fetch/vendor the kernel and userland source trees and drive a full rebuild from
  within the running OS.
- A working bootstrap: boot an OxideBSD image, rebuild OxideBSD from source on it, boot the result.

## Release sequence: v0.2.0 → v0.3.0 → v0.4.0

As of 2026-09-04, the old single "v0.2.x goals" bucket below is split into three separate,
sequential releases — each ships standalone rather than bundling everything into one v0.2.0:

- **v0.2.0 — POSIX pilot compliance.** The current focus. Close as much of the gap as practical
  between OxideBSD's own Open POSIX Test Suite pilot run and a mature glibc/Linux baseline, using
  the full ~1687-file corpus (not a curated subset — see `CLAUDE.md`'s "POSIX pilot: full corpus
  expansion" section) as the measuring stick. Latest measured OxideBSD baseline (2026-09-07, a
  fresh `--reset` full-corpus run, `scripts/run_posix_pilot_supervised.sh`): **87.4%** raw pass
  rate / **92.8%** excluding UNTESTED (1474 PASS / 1686 total; 22 FAIL / 39 UNRESOLVED / 14 CRASH /
  11 TIMEOUT / 29 UNSUPPORTED / 97 UNTESTED — `shm_open/23-1.c` needed excluding again, a known,
  real single-core scheduling-throughput limit, not a new bug). Last real host-side comparison
  (2026-09-06, not re-run this session — `scripts/run_posix_pilot_host.sh`, manual/root-only): a
  mature glibc/Linux host at **89.5%** / **94.3%**, a ~2-point gap. Closing it means triaging the
  full corpus's own remaining FAIL/UNRESOLVED set, not growing the corpus further — it's already
  complete. Two clusters ruled out this session as real bugs (see `CLAUDE.md`'s own history and
  session memory for detail): `sigaction/17-{2,10,20,25,26}.c`'s FAILs were transient host-load
  timing flakiness (all 5 clean `PASS` in this same fresh run); `aio_suspend`'s 6 and `aio_cancel`'s
  4 UNRESOLVED are a real oxfs file-size-cap gap and an inherent test-timing race respectively,
  neither a quick fix. **Full POSIX syscall coverage** (every POSIX-mandated syscall, even where
  this ABI's own number/shape — see `CLAUDE.md`'s Syscall ABI section — diverges from Linux's or
  any real BSD's; not a promise to match Linux/BSD numbering or wire format) falls out of this same
  push, not a separate goal.
- **v0.3.0 — GCC and Clang self-hosted ports.** What v0.2.0 used to target before the 2026-09-04
  re-scope (see `CLAUDE.md`'s TinyCC section for why this is a much bigger lift than TinyCC — real
  subprocess pipelines, likely real dynamic linking and threads beyond what exists today):
  self-hosting C-side toolchain components running on-target (moving further into Phase 3's "build
  itself" goal from the C side first), then retiring `tcc` once both GCC and Clang are real,
  working on-target ports — TinyCC was always the first/easiest target, never the intended
  long-term C compiler.
- **v0.4.0 — a real glibc port**, alongside (not replacing) the existing native-ABI musl port.

A separate idea — replacing some BusyBox utilities with Rust `uutils` ahead of GCC/Clang — was
raised and set aside: not a real dependency of GCC/Clang bring-up (unrelated subsystems), just a
possible future nice-to-have, not currently sequenced into this list.

**Real text editors: `nano` and real `vim`** — BusyBox's roster today only has the small `vi`
applet (see `docs/BUSYBOX_APPLETS.md`); `nano` and full (non-BusyBox) `vim` are separate ports, for
meaningfully better on-target text editing than the current applet-only story — not yet slotted
into a specific release above.
