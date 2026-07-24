# CLAUDE.md

This file provides guidance to Claude Code when working with code in this repository.

## Project

OxideBSD is a 100% Rust-based BSD-like OS, x86_64 only (see `ROADMAP.md` for phase history).
Current state:

- Boots via `bootloader` v0.9 + `bootimage`/QEMU. GDT/TSS/IDT with a dedicated double-fault
  stack, PIC-driven interrupts (timer + PS/2 keyboard), a VGA console mirroring serial, a heap
  allocator over bootloader-provided paging.
- Separate per-process address spaces, ELF64 loading, ring-3 execution, and a native BSD-style
  syscall ABI over `SYSCALL`/`SYSRETQ` (`src/syscall.rs`) with carry-flag error signaling.
- A dynamic kernel module loader (`src/module.rs`) relocates `#![no_std]` code into the kernel at
  boot and resolves its symbol references against a hand-curated kernel API. Syscall handlers are
  registered by modules, not hardcoded: `modules/native_abi/` (core syscalls), `modules/
  posix_compat/` (pipe/dup2/ioctl/setpgid/...), `modules/signal/` (kill/sigaction/...),
  `modules/oxfs/` (the live filesystem).
- `modules/oxfs/` is a real in-memory Unix-shaped inode/block filesystem (real names,
  multi-component paths, per-process cwd, no fixed file-size cap) — replaced `modules/fat32/`
  (8.3 names, one path component per call, fixed file cap), which still builds/self-checks via
  `cargo build` but is no longer loaded at boot.
- A real process table + cooperative round-robin scheduler (`src/process.rs`, `src/scheduler.rs`,
  `src/context_switch.rs`) with `fork`/`execve`/`wait4`/`getpid`, real `argv`/`envp` passthrough,
  blocking pipes, and per-process signal delivery.
- pid 1 is BusyBox's `hush`, built against a patched musl fork — not the original hand-written
  `userland/stsh/` shell (still buildable, no longer wired up). 24 BusyBox applets run as
  standalone static binaries, `execve`'d individually (not a multi-call `busybox` binary
  dispatching on `argv[0]` — that passthrough exists now, but the roster hasn't been rebuilt to
  use it).

Known, deliberate gaps: no pointer validation in `sys_read`/`sys_write`, no module unload/reload,
no filesystem persistence, no preemption, no copy-on-write fork, no frame deallocation anywhere,
`sys_read` on stdin is non-blocking (busy-polled by userland), no real block device driver. See
"BusyBox gap analysis" below for what's needed to go further. Architecture decisions for remaining
subsystems haven't been made — discuss with the user before large structural commitments.

## Toolchain

- Nightly Rust, pinned via `rust-toolchain.toml`. Load-bearing unstable features: `-Z build-std`
  (no prebuilt std for the custom target), `-Z json-target-spec`, `-Z panic-abort-tests`.
- Requires `bootimage` (`cargo install bootimage`) and `qemu-system-x86_64` on `PATH`.
- `.cargo/config.toml` sets the default target to `x86_64-oxidebsd.json` and
  `runner = "bootimage runner"`.

## Commands

- `cargo build` — kernel ELF only
- `cargo bootimage` — bootable disk image
- `cargo run` — boot in QEMU, serial to stdio
- `cargo test` / `cargo test --test basic_boot` — each target boots its own QEMU instance (slow;
  no fast check path exists)
- `cargo clippy` / `cargo fmt`

These commands at the repo root only target the `oxidebsd` package. `userland/*` and `modules/*`
are separate workspace members that the root `build.rs` cross-builds as a side effect of building
`oxidebsd`. To build one directly: `--manifest-path <dir>/<name>/Cargo.toml --target-dir
target/userland` (or `target/modules`) — a separate target dir avoids a nested-cargo lock deadlock
against the outer build. `modules/fat32/` additionally needs `FAT32_IMAGE_PATH` set when built
this way (normally supplied by the root `build.rs`).

## Test architecture

No libtest — `no_std`, tests boot in QEMU and self-report via `src/qemu.rs` (writes to the
`isa-debug-exit` port; `test-success-exit-code` in `Cargo.toml` must stay in sync with
`QemuExitCode::Success`) and `src/serial.rs` (hand-rolled 16550 UART, read via `-serial stdio`).

- `src/lib.rs` defines `no_std` test scaffolding (`custom_test_frameworks`, `#[test_case]`) and
  boots itself under `#[cfg(test)]`.
- `tests/*.rs` integration tests use `harness = false` (so `#[test_case]` machinery doesn't
  apply) — each defines its own `fn main()` via `entry_point!` and calls `exit_qemu()` directly.
- `tests/fork_wait.rs` + `userland/fork-exec-smoke/`: since `scheduler::start`/`process::do_exit`
  never return to a test's own `main`, it registers a syscall number (`9999`) directly via
  `oxidebsd::syscall::oxidebsd_register_syscall` (kept `pub` for this) whose handler calls
  `exit_qemu`.

## Custom target spec (`x86_64-oxidebsd.json`)

- `target-pointer-width`/`target-c-int-width` must be numbers, not strings.
- Float returns need both `"features": "...,+soft-float"` and `"rustc-abi": "softfloat"`, or
  `core`/`compiler_builtins` fail to build.
- `panic-strategy: "abort"` is the only supported strategy — hence `-Z panic-abort-tests` in
  `.cargo/config.toml` (otherwise Cargo builds an unwind-based test harness and produces a second,
  ABI-incompatible `core`).
- SSE/MMX disabled, `disable-redzone: true` (interrupt handlers can't safely use either).

## Memory management (`src/memory.rs`, `src/allocator.rs`)

- `memory::init` walks `CR3` and adds `BootInfo::physical_memory_offset` to get a virtual pointer
  to the level-4 table (relies on the bootloader's `map_physical_memory` feature). Call at most
  once — hands out a `&'static mut`.
- `memory::BootInfoFrameAllocator` bump-allocates from `BootInfo::memory_map`'s `Usable` regions,
  never reuses a frame — no deallocation anywhere yet. Holds plain `(region_index, frame_number)`
  cursor state, not an iterator rebuilt from scratch on every call — that used to be `next: usize`
  with `allocate_frame` calling `self.usable_frames().nth(self.next)`, an O(n) cost per allocation
  (O(n²) total across n allocations) invisible at a few thousand frames but a real, measured
  multi-minute-plus stall once a single caller needed tens of thousands (raising the QEMU `-m` /
  heap size, or `modules/oxfs`'s object crossing that threshold once its BusyBox roster grew — see
  this file's BusyBox section). A boxed-iterator fix was tried first and is *wrong*, not just
  suboptimal: this allocator is constructed *before* `allocator::init_heap` (which needs it to map
  the heap's own pages), so any heap allocation in its own constructor reliably panics with no
  heap yet to satisfy it.
- `allocator::init_heap` and `module::map_region` both map freshly allocated pages with
  `.ignore()`, not `.flush()`, on the `MapperFlush` `map_to` returns — a page that's never been
  mapped before can't have a stale TLB entry to invalidate, so the flush (a real `invlpg`, trapped
  and emulated individually under QEMU's software TCG) is pure waste at scale. Secondary to the
  frame-allocator fix above, found investigating the same stall.
- The heap lives at a fixed VA (`allocator::HEAP_START`); its size scales with detected RAM
  (`memory::usable_ram_bytes()`), clamped between a proven floor and a ceiling. The same
  RAM-scaling pattern applies to `process::kernel_stack_size()`/`user_stack_pages()`. NOT scaled:
  `modules/fat32`'s embedded image size, and `module::MODULE_VA_BASE`/`MODULE_REGION_CEILING` (a
  VA-range limit from the relocation model, not RAM). QEMU's own RAM (`Cargo.toml`'s
  `[package.metadata.bootimage]` `-m`) is `1024` MiB, not QEMU's unstated ~128 MiB default — raised
  once `modules/oxfs`'s block pool grew to 32 MiB (see this file's BusyBox section); a real
  physical-memory commitment from module-load time on, not paged in on demand.
- The global allocator is `linked_list_allocator`'s `Heap` wrapped in a local `Locked<T>`
  (`spin::Mutex`), not the crate's own `LockedHeap` — avoids a second spinlock crate in the graph.

## User-mode execution (`src/address_space.rs`, `src/elf.rs`, `src/usermode.rs`)

`process::spawn` builds the first process this way at boot; `process::do_execve` builds every
later one the same way, mid-syscall.

- Userland crates (`userland/*`) are separate workspace members; `build.rs`'s
  `build_userland_crate` cross-builds each into `target/userland/` and exposes
  `<NAME>_ELF_PATH` via `cargo:rustc-env` for `include_bytes!`. Each crate's `linker.ld` forces a
  distinct load base clear of the kernel image, heap, phys-mem-offset window, **and** `bootloader`
  v0.9's own identity-mapped low-memory region. **This floor moves as the kernel image grows, and
  has already moved once** — `0x600000` was "confirmed clear" when the kernel was ~2.2 MiB, but
  silently stopped being clear once `modules/oxfs` started embedding every BusyBox applet's own
  ELF bytes (~300 of them — see this file's BusyBox section) via `include_bytes!`, which itself
  gets embedded into the *kernel's own* binary the same way, pushing the kernel past ~22.7 MiB and
  swallowing several fixed load addresses below it (`hush`'s pid-1 spawn was the first thing to
  actually hit this: `Elf(MappingFailed)`). The floor is now `0x4000000` (64 MiB, ~3x the kernel's
  size at the time this was written) — a binary placed below the *actual current* floor fails with
  `PageAlreadyMapped`/`MappingFailed`, and this only ever surfaces via `execve`/process spawn, not
  a one-shot boot demo, so **before adding a new binary or trusting this number**, re-derive the
  floor by hand: `readelf -l target/x86_64-oxidebsd/debug/oxidebsd | grep -A1 LOAD`, take the
  highest `VirtAddr + MemSiz`, round up with real headroom (not "barely enough" — that's exactly
  how this broke last time). `userland/musl-smoke/` isn't a Rust crate — built with `musl-gcc`,
  load base set via `-Wl,-Ttext-segment=`.
- `AddressSpace::new` shallow-copies all 512 L4 entries from the currently active table (this
  kernel has no higher-half split — kernel, heap, phys-mem window, and every user ELF's load
  address all share the low canonical range at different indices). Safe only when the active
  table's user-space content is empty (true only for `process::spawn` at boot).
  `AddressSpace::fork`/`new_excluding_user` (for a live process — `fork`/`execve`) instead
  recursively walk the table using the `USER_ACCESSIBLE` flag as the sole kernel-vs-user signal at
  any level.
- **`gdt.rs`'s ring-0 stacks must be `static mut`, not `static`.** A plain `static`, never written
  via a Rust `&mut`, gets interned into `.rodata` by the optimizer (the actual writes are
  CPU-hardware-only, invisible to that analysis) — causes a double/triple fault the instant an
  exception uses that stack. Any future stack added the same way needs the same treatment.
- **Every IDT gate a software interrupt (`int n`, `int3`, ...) can trigger from ring 3 needs
  `DPL = Ring3` explicitly** — gates default to `Ring0`, and software (not hardware/IRQ)
  interrupts additionally require `CPL <= gate DPL`. Wrong DPL manifests as a `#GP` on the IDT
  entry itself, not a permissions error.
- **`elf::load` tracks already-mapped pages in a `BTreeMap<Page, PhysFrame>` for one call** —
  `PT_LOAD` segments are aligned to `p_align`, not to each other, so small binaries routinely
  share a page across segments; mapping/zeroing it twice is a bug. Flags aren't unioned across
  segments sharing a page.
- Known simplification: no `NO_EXECUTE` on any ELF segment (would also need `EFER.NXE`).

## Syscall ABI (`src/syscall.rs`)

OxideBSD's own native, BSD-flavored ABI over `SYSCALL`/`SYSRETQ` — not Linux-compatible. Syscall
number in `RAX`, up to 4 args in `RDI`/`RSI`/`RDX`/`R10` (not `RCX`/`R11`, clobbered by `SYSCALL`
itself). Success/failure via the **carry flag** (`CF=0` success, value in `RAX`; `CF=1` failure,
positive errno in `RAX` — the traditional BSD/x86 Unix convention). Pre-musl-port syscalls
(`SYS_EXIT=1`, `SYS_FORK=2`, `SYS_READ=3`, `SYS_WRITE=4`, `SYS_OPEN=5`, `SYS_CLOSE=6`,
`SYS_WAIT4=7`, `SYS_GETPID=20`, `SYS_EXECVE=59`) match real FreeBSD numbers as an authenticity nod.
Everything added since (`SYS_MMAP=100`, `SYS_MUNMAP=101`, `SYS_BRK=102`, `SYS_SET_FS_BASE=103`,
`SYS_WRITEV=104`, `SYS_PIPE=105`, `SYS_DUP2=106`, `SYS_GETPPID=107`, `SYS_GETCWD=108`,
`SYS_UNLINK=109`, `SYS_RMDIR=110`, `SYS_RENAME=111`, `SYS_KILL=116`, `SYS_SIGACTION=117`,
`SYS_SIGPROCMASK=118`, `SYS_SIGRETURN=119`, `SYS_SETPGID=120`, `SYS_GETPGID=121`, `SYS_IOCTL=124`,
`SYS_DUP=125`, `SYS_FSTAT=126`, `SYS_STAT=127`, `SYS_LSTAT=128`, `SYS_GETDENTS=129`) is OxideBSD's
own invention —
numbers/shapes picked for what porting musl/BusyBox
actually needed, not copied from FreeBSD/Linux (a few, like `pipe`/`dup2`/signal numbers, happen to
match real wire formats anyway; check `src/syscall.rs` and module sources for the current highest
number before assigning a new one). errno uses FreeBSD's values where Linux/BSD diverge (e.g.
`ENOSYS=78`).

The number→handler mapping is a runtime registry (`SYSCALL_TABLE`, a `Mutex<BTreeMap>`) populated
by `oxidebsd_register_syscall` from each module's `module_init` — not a hardcoded `match`. An
unregistered number logs `[boot] unrecognized syscall number N` and returns `ENOSYS`, the main
tool for discovering what a ported program's startup still needs.

- **`SYSRETQ`'s selector scheme forces GDT order.** `SYSRETQ` derives `SS`/`CS` from
  `IA32_STAR[63:48]` as `+8`/`+16` — user data must sit immediately before user code.
  `src/gdt.rs`'s order is: kernel code, kernel data, an unused placeholder (needed only for offset
  spacing), user data, user code, TSS. Don't reorder without redoing the `STAR` arithmetic; use
  `x86_64::registers::model_specific::Star::write`, which validates this and panics loudly if the
  GDT regresses.
- **No automatic stack switch on `SYSCALL` entry.** Control arrives at `syscall_entry` still on
  the user's own stack. `gdt::CURRENT_RSP0` (a `static mut`, kept in sync by
  `gdt::set_kernel_stack` on every context switch) always names the current process's own kernel
  stack — required because a single shared scratch stack breaks the moment two processes can be
  mid-syscall at once (`do_wait4` already blocks/reschedules mid-syscall). No per-CPU `swapgs` —
  single-core only.
- `SyscallFrame`: the stub's pushed GPRs plus `user_rsp` (`SYSCALL` doesn't push a stack frame the
  way an interrupt gate does). `rcx`/`r11` double as saved `RIP`/`RFLAGS` (`SYSCALL`'s own
  hardware contract); `syscall_dispatch` flips bit 0 of `r11` to signal `CF`.
- `dispatch()` is a small, pure, directly unit-tested function separate from
  `syscall_dispatch`'s raw-pointer/frame handling (see `src/lib.rs` tests).
- A registered handler's own wire format (`SyscallHandler`) is a plain `i64` (negative =
  `-errno`) — distinct from the public carry-flag ABI, just the module↔kernel registration
  boundary's shape.
- `sys_write`/`sys_read` don't validate `[ptr, ptr+len)` before dereferencing — a bad pointer
  page-faults (handled safely by `page_fault_handler`: log + reboot), not a soundness hole, but no
  safety net for user programs yet.
- `sys_read` on stdin is non-blocking by design (returns `Ok(0)` on empty) — pushes polling into
  userland (see Interactive shell). Any other fd (oxfs files, pipes) delegates to `crate::fd`'s
  per-process `(Pid, fd)` registry.
- `sys_write`'s `fd == 2` (stderr) is an alias for `fd == 1` — no real second sink exists.

## musl port (`third_party/musl`, `userland/musl-smoke/`, `src/user_stack.rs`, `src/fpu.rs`)

musl is patched (not the kernel made Linux-compatible) to speak this native ABI directly.
`third_party/musl` is a submodule of a personal fork (`ifduyue/musl`, an active mirror of the
canonical `git.musl-libc.org`), patches on its own `oxidebsd` branch based on tag `v1.2.6`.
Pin/update by committing on that branch, pushing, then `git add third_party/musl` here. Patch
surface is deliberately small, entirely under `arch/x86_64/`:

- `syscall_arch.h`: a `jnc 1%=f; neg %%rax; 1%=:` after every `syscall` converts carry-flag errors
  into musl's expected small-negative-value convention.
- `bits/syscall.h.in`: only the `__NR_*` values musl's static-binary startup path actually reaches
  are remapped to OxideBSD's real numbers; everything else keeps its inert Linux value (cleanly
  `ENOSYS`s if reached). `open`/`execve` are patched at the call-site level instead of just
  remapped — see the argument-convention note below.
- `__set_thread_area.s`: TLS base is set via `SYS_SET_FS_BASE` (a bare base-address write, no
  `arch_prctl` subcommand).

Key gotchas:
- musl's entire stdio write path goes through `writev`, never plain `write` — `SYS_WRITEV` is
  load-bearing, not optional (its absence used to silently redirect all `printf` output into
  `getpid()` via a numbering collision — no crash, just zero output; only visible in QEMU's serial
  log, not from `cargo test`/clippy staying green).
- **Remapping a `__NR_*` macro isn't enough if a 64-bit-suffixed sibling exists**: `src/internal/
  syscall.h` has its own `#ifdef SYS_getdents64 / #undef SYS_getdents / #define SYS_getdents
  SYS_getdents64`, unconditionally preferring the 64-bit name whenever it's defined at all — real
  `readdir()` (`src/dirent/readdir.c`) calls the plain `SYS_getdents` macro, but that macro's own
  *value* silently became `SYS_getdents64`'s (left at its original, inert, real-Linux number) the
  moment both were defined, resurrecting the exact numeric collision the remap was meant to close.
  Confirmed live: `ls` ran clean but every real directory read inside it hit "unrecognized syscall
  number 217", not `SYS_GETDENTS`'s real `129`. Both `__NR_getdents` and `__NR_getdents64` carry
  OxideBSD's own number now, kept in sync — any future syscall with a same-shaped 64-bit sibling
  (`__NR_stat64`, `__NR_fstatat64`, ...) needs the same audit before trusting a single remap.
- SSE was never enabled at the hardware level (`CR0.EM`/`CR4.OSFXSR`/`OSXMMEXCPT`) — this kernel's
  own build target disables SSE codegen, so nothing had ever exercised it. `src/fpu.rs::init()`
  enables it once at boot. No save/restore across context switches — fine only as long as at most
  one SSE-using process is ever mid-computation (true today, no preemption).
- `src/user_stack.rs` builds a real System V argc/argv/envp/auxv initial stack (musl's `_start`
  reads it directly). `AT_PHDR` is derived from whichever `PT_LOAD` segment has the smallest
  `p_offset` (robust against this codebase's own linker scripts, which don't map the ELF header
  into any segment, unlike a normal linker). `AT_RANDOM` is a fixed placeholder (no entropy source
  exists).
- **`open`/`execve` argument-convention mismatches are fixed on the musl side**, not by remapping
  alone: real `open()`/`execve()` pass different argument shapes than OxideBSD's
  `(path_ptr, path_len, ...)`. `src/fcntl/open.c` computes `path_len` via `strlen()` directly;
  `src/process/execve.c` builds real `argv`/`envp` as length-prefixed `RawArgvEntry{ptr, len}`
  arrays (zero-entry-terminated) instead of NUL-terminated `char**`, using the real 4th syscall
  argument (`R10`) for `envp_ptr`. The same length-prefix pattern recurs for `unlink`/`rmdir`/
  `rename` (oxfs) and `chdir`/`mkdir` — **any future libc call ported here needs the same audit**:
  matching the syscall *number* isn't sufficient if the argument shape differs.
- Syscalls from this pass: `SYS_MMAP=100` (`(addr_hint, len, prot)` — matches musl's actual
  `mmap()` call site, not an idealized layout; always anonymous+private, bump-allocated from a
  fixed VA window, never reclaimed), `SYS_MUNMAP=101` (no-op success), `SYS_BRK=102` (grows/
  shrinks `Process.brk`, no reclaim on shrink), `SYS_SET_FS_BASE=103`, `SYS_WRITEV=104`.

## BusyBox port (`third_party/busybox`, `modules/posix_compat/`)

~300 applets run today, each its own standalone single-applet static binary (see Project above for
why). Vendored as a submodule (fork of `mirror/busybox`, tag `1_36_1`, an `oxidebsd` branch —
currently empty — same pin/update procedure as musl above). `build.rs`'s `build_busybox_applet`
runs BusyBox's own `allnoconfig` → flip one applet's Kconfig symbol → resolve any newly-revealed
sub-options via `make oldconfig` fed blank lines → build recipe, asserting `NUM_APPLETS == 1`;
`sh` additionally forces on `CONFIG_HUSH_INTERACTIVE`/`HUSH_JOB`/`FEATURE_EDITING`. Applets are
embedded into oxfs's inode table by `modules/oxfs/`'s `module_init` (data-driven from `build.rs`'s
`BUSYBOX_APPLETS`/`BUSYBOX_APPLETS_PASS2` lists; each new applet still needs one manual
`seed_file`/one-liner call added in oxfs).

**The roster grew from 24 to ~300 in one pass** once `SYS_STAT`/`SYS_FSTAT`/`SYS_LSTAT`/
`SYS_GETDENTS` existed (see this file's oxfs section): an exhaustive per-applet build probe was
run against every Kconfig applet symbol BusyBox's own `//applet:` source markers define, keeping
every one that built. **"Builds" is a much weaker bar than "works"** — musl provides a fairly
complete libc surface, so plenty of applets that make no sense on this kernel (networking, mount,
`/proc`-reading, uid/passwd-db tools) still compile and link cleanly, then fail cleanly at runtime
(usually `ENOSYS` from an unregistered syscall). `docs/BUSYBOX_APPLETS.md` is the full roster —
every built applet tagged with what it's still missing (`NEEDS_NETWORK`/`NEEDS_PROC`/
`NEEDS_CLOCK`/`NEEDS_UID`/`NEEDS_SYSCALL`/`NEEDS_HARDWARE`/`NEEDS_BLOCKDEV`/`NEEDS_INIT`, or
`WORKS` if it needs nothing this kernel doesn't already have), plus every candidate that didn't
build at all and why (almost entirely missing Linux uapi headers musl doesn't vendor — framebuffer/
VT/MTD/I2C/netlink ioctl tools with no portable equivalent). `build_busybox_applet` is also now
staleness-checked (skips its own `allnoconfig`/`oldconfig`/`make` sequence if that applet's already-
built binary is newer than both `third_party/busybox` and `build.rs` itself) and the whole roster
builds in parallel across a worker pool sized to `available_parallelism()` — both load-bearing at
this scale, not true at 24 applets. `modules/fat32`'s own embedded image dropped busybox applets
entirely over this same change (its 8.3-short-name format can't hold names over 8 characters, and
its fixed sector budget was sized for a much smaller roster) — harmless, since nothing loads that
image at boot.

- `hush` (pid 1) uses real `execvp()`/`$PATH` search — `process::spawn` passes a fixed `envp` of
  `PATH=/bin`, so musl's `__execvpe` always searches oxfs's `/bin` directory as an absolute path
  (`/bin/<name>`), independent of hush's current cwd. `modules/oxfs`'s `module_init` creates
  `/bin` explicitly (own inode, `.`/`..` entries, inserted into root) and seeds every applet
  there under its bare name (`ls`, `cat`, ...), not `.elf`-suffixed, so `ls` typed at any cwd
  resolves; `hello.txt`/`big.txt` (data, not executables) stay at root. (An earlier version used
  `PATH=/` with applets seeded directly at root — worked, but conflated executables with data
  files in one flat directory; before that, `PATH=` (present, empty) relied on musl's "empty
  component means search cwd" rule plus hush's cwd starting at root — worked only by
  coincidence, broke the moment cwd moved elsewhere via `cd`.)
- New kernel-resident pieces `sh` required: the real 4th syscall argument (`R10`, for `envp`),
  real blocking `pipe(2)`/`dup2(2)` (`src/pipe.rs` — an unbounded `VecDeque<u8>`; a read genuinely
  blocks via `BlockReason::WaitingForPipeData` + `scheduler::schedule()`, since busy-spinning would
  starve a single-core cooperative scheduler), and a **per-process** `(Pid, fd)` fd table
  (`src/fd.rs`) — a flat table broke real pipelines the moment a parent closed its own copy of a
  pipe fd out from under still-using children. `crate::fd::fork_inherit`/`close_all` implement
  real `fork`/`exit` fd semantics.
- **`IA32_FS_BASE` (TLS) is a single global MSR that `context_switch::switch_context` never
  saved/restored per-process** — a musl-linked parent (`hush`) resuming after a musl-linked child
  exited would silently run with the dead child's leftover TLS base and fault on its own
  stack-protector check. Fixed via `Process::fs_base`, restored on every switch by
  `scheduler::activate_and_prepare` (not `switch_context` itself, since it's a single global
  register invisible to the GPR/RSP-only context switch).
- `getcwd`/`getppid`/`chdir`/`mkdir` needed the same argument-convention fixes as `open` — only
  surfaced once `hush` was actually driven interactively (`cd`, `pwd`), not via `-c "cmd"` smoke
  tests alone.
- Real interactive `sh` (typing at a prompt, not just `-c "cmd"`) required a separate blocking
  stdin-read pass (`BlockReason::WaitingForStdin`, `scheduler::wait_for_ready`).
- musl's stdio calls `write(fd, buf, 0)`/`read(fd, buf, 0)` with a null/garbage `buf`, which is
  POSIX-legal (buf must not be touched at length 0) but crashed every fd callback's unconditional
  `slice::from_raw_parts` — fixed centrally in `src/fd.rs`'s `read`/`write` funnel functions, not
  per-callback.
- `SYS_IOCTL=124`/`SYS_DUP=125` (termios + `dup(2)`, `modules/posix_compat/`) — see Interactive
  shell below.
- New syscalls always go in a dedicated module (`modules/posix_compat/`, `modules/signal/`, ...),
  not `modules/native_abi/` — keeps the core ABI module small.

## Interactive shell (`src/stdin.rs`, `userland/stsh/`)

`stsh` ("stupidshell") is the original hand-written interactive userland program — still
buildable, no longer pid 1 (superseded by `hush`). Its design remains the reference for how this
kernel's stdin path works:

- Keyboard IRQ (`src/interrupts.rs`) decodes scancodes and pushes ASCII bytes into a fixed
  256-byte ring buffer (`src/stdin.rs`) — non-ASCII dropped, no allocation inside the interrupt
  handler by design (a plain array, not `VecDeque`). `sys_read` drains it. The keyboard handler
  only auto-echoes when `src/stdin.rs`'s global `TERMIOS.ECHO` bit is set (see `SYS_IOCTL` below)
  — otherwise a raw-mode reader would get every keystroke echoed twice.
- The `spin::Mutex` around the ring buffer can't deadlock between IRQ and syscall context
  specifically because `SFMASK` clears `IF` for the entire duration of a `SYSCALL` on this single
  core — breaks if SMP is ever added.
- `sys_read` is non-blocking, so `stsh` busy-polls it a byte at a time; a real scheduler exists
  now (`do_wait4`/pipe reads already block) but `sys_read` itself hasn't been converted.
- Line editing: backspace/delete both erase-and-reprint `"\x08 \x08"`; Ctrl+C aborts the line
  (prints `^C`, returns empty); Ctrl+D on an empty line exits via `SYS_EXIT`. No cursor movement,
  no history, 128-byte line cap.
- `src/vga.rs`'s `Writer` special-cases raw `0x08` as "step cursor back, draw nothing" so the same
  backspace idiom works on the VGA console.
- Real `SYS_IOCTL=124` (`src/stdin.rs`'s `RawTermios`, a single **global** — not per-session —
  `TERMIOS`) implements `TCGETS`/`TCSETS*`/`TIOCGWINSZ` (fixed `24x80`)/`TIOCSWINSZ` (accepted,
  discarded); everything else is `ENOTTY`. Only succeeds against the real console (checked via
  `crate::fd::real_fd_of`, so a pipe end `dup2`'d onto fd 0/1 correctly still reports non-tty —
  load-bearing for musl's `isatty()`). This is what lets `hush` run with
  `CONFIG_HUSH_INTERACTIVE`/`HUSH_JOB`/`FEATURE_EDITING` on and reach a real `/ #` prompt with
  line editing.
- No pty/foreground-process-group layer exists — `tcsetpgrp`/real job control (`bg`/`fg`) are
  unimplemented; `TIOCGPGRP` failing cleanly is what lets `HUSH_JOB` degrade gracefully to
  line-editing-only instead of crashing.

## Process abstraction, scheduler, and fork/exec/wait (`src/process.rs`, `src/scheduler.rs`, `src/context_switch.rs`)

Dynamically allocated process table, cooperative round-robin scheduler, kernel-thread-style
context switch between per-process kernel stacks. No preemption, no copy-on-write fork (full eager
copy), no SMP, no frame deallocation anywhere.

- **Process table is `Mutex<BTreeMap<Pid, Box<Process>>>`, `Box` is load-bearing** — a
  `BTreeMap`'s internal nodes can move on insert/remove, but a `Box`'s heap allocation never does;
  holding the table lock across a context switch would deadlock (the lock only releases when that
  exact stack next resumes). Every function touching both the table and `scheduler::schedule()`
  drops the lock first.
- `context_switch::switch_context` only saves System V callee-saved registers + `RSP` — everything
  else is either already caller-saved or already saved on that process's own kernel stack by
  `syscall_entry`. Two first-run trampolines: `spawn_trampoline_asm` (never-run process,
  defensively realigns `RSP`) and `fork_trampoline_asm` (forked child, jumps straight into
  `syscall_entry`'s GPR-pop/`sysretq` tail with no realignment — `seed_fork_frame` places the
  copied `SyscallFrame` at exactly the offset that tail expects).
- `fork` resumes the child via a copy of the parent's live `SyscallFrame` with `rax=0` and the
  copy's carry-flag bit explicitly cleared (the copied `r11`/RFLAGS is stale pre-syscall state,
  not anything meaningful — a real bug caught before shipping).
- `do_execve` builds everything (new `AddressSpace`, `elf::load`, user stack/argv/envp/auxv)
  *before* mutating the live frame/`CR3`/stored `AddressSpace` — a failure at any point must leave
  the caller untouched, matching real `execve(2)` semantics. `argv_ptr` now supplies the complete
  `argv[]` including a real caller-chosen `argv[0]` (length-prefixed `RawArgvEntry{ptr,len}`
  array, zero-entry-terminated — not real `execve`'s NUL-terminated `char**`).
- Per-process state carried across `fork` (copied) / `execve` (mostly preserved, some reset)
  alongside the obvious (pid, address space): `cwd` (preserved by execve), `brk` (copied by fork,
  not reset by execve), `fs_base` (copied by fork, reset to 0 by execve — new TLS layout), `pgid`
  (inherited by fork, untouched by execve), signal state (`sigactions` reset to `SIG_DFL` for
  caught handlers only on execve; `pending`/`blocked` untouched).
- Kernel stack size floor is `128` KiB — found empirically the hard way (16 KiB overflowed on
  `ls`'s deeper call chain; 32 KiB overflowed on `fork`'s debug-build `PageTable::clone()` stack
  frame). No guard page — overflow corrupts silently.
- `gdt::set_kernel_stack` repoints `TSS.RSP0` on every context switch, via a raw pointer (not
  `&mut`, since `spin::Lazy` has no `DerefMut`) — sound only because nothing else holds a live
  reference across the call (single-core, interrupts disabled during scheduling).
- `tests/fork_wait.rs` + `userland/fork-exec-smoke/` is the automated coverage for this subsystem
  (fork/wait4/exit round trip, no filesystem/execve involved) — see Test architecture above.

## Dynamic kernel modules (`src/module.rs`, `modules/*`)

Loads independently-compiled, relocatable (`ET_REL`) `#![no_std]` objects into the kernel's
currently-active address space at boot: relocates them, resolves referenced symbols against a
small hand-curated kernel API table, calls `module_init`. A genuinely different job from `elf.rs`
(which loads a non-relocatable `ET_EXEC` binary with zero relocations) — this is the largest
subsystem in the kernel (~500+ LOC).

- Module crates are plain `#![no_std]` `lib` crates, no `_start`/linker script/final link.
  `build.rs`'s `build_module_crate` runs `cargo rustc --release --lib -- --emit=obj` then a
  mandatory relocatable partial relink (`rust-lld -flavor gnu -r`) against the exact
  `core`/`alloc`/`compiler_builtins` `.rlib`s that build produced — an open-ended,
  code-content-dependent undefined-symbol set otherwise.
- `--gc-sections -u module_init` on that relink is **required, not an optional size
  optimization** — coarse archive-member selection during `-r` linking otherwise pulls in entire
  bundled `core`/`alloc` object files (one indexing-triggered `panic_bounds_check` reference once
  ballooned a module to 3+ MB/2900 sections and exhausted the kernel's boot-time heap just parsing
  headers).
- `RUSTFLAGS="-C relocation-model=static"` (scoped to this nested build only) keeps relocations to
  absolute 32-bit forms — in exchange every module must map inside the low 2 GiB
  (`MODULE_VA_BASE=0x10000000`, `MODULE_REGION_CEILING=0x80000000`). A few GOT-indirected
  references survive anyway (e.g. `panic_bounds_check`'s own formatting) — handled via a minimal,
  eagerly-populated per-relocation-site GOT the loader builds at load time.
- **No `core::fmt::Write`/`write!` in module code** — constructing that trait object's vtable
  emits a GOTPCREL reference and was the single largest source of bloat before `--gc-sections`.
  Modules use hand-rolled byte formatting instead.
- **Modules can't use `alloc`/`Vec`/`BTreeMap`** — avoids depending on `#[global_allocator]`'s
  unstable-ABI internals from relocated code. State lives in fixed-size `static mut` arrays
  instead.
- **A distinct `static mut` gotcha from the `gdt.rs` one**: a private `static mut` buffer that IS
  written by real Rust code, but never observably read back through an externally-reachable
  function, can have the write deleted as an unobservable dead store. Any module state needs to be
  read back from an exported/syscall-reachable function to survive optimization.
- Modules are mapped kernel-only (no `USER_ACCESSIBLE`) and every page is `WRITABLE` regardless of
  section (relocation must patch code bytes; no W^X anywhere in this kernel yet).
- A module panic is fatal, same as a kernel panic: `build.rs`'s `discover_panic_symbol` finds each
  module's toolchain-hashed panic-entry symbol via `llvm-nm`, and the loader's resolver points it
  at `module_panic_trampoline` (logs, `hlt_loop()`).
- `serial_println!` can't take implicit `{name}`-style captures (its `concat!`-based expansion
  blocks it) — always use explicit positional args; `serial_print!` doesn't have this restriction.
- Known limits: no module unload/reload, no versioning, no inter-module direct calls (only
  module→kernel, via each module's own resolved symbol table — this is why `src/fd.rs`'s registry
  exists at all, as the only coordination point between e.g. `oxfs` and `native_abi`).

## Filesystem: oxfs (live) and FAT32 (superseded)

**`modules/oxfs/`** is the live filesystem — a real Unix-shaped inode/block filesystem, in-memory
only (no block device driver exists). Fixed-size `static mut` pools: `NUM_BLOCKS=8192` ×
`BLOCK_SIZE=4096` (32 MiB total, raised from 4 MiB once the BusyBox roster grew to ~300 applets —
see this file's own BusyBox section — a real physical-memory commitment from module-load time on,
not paged in on demand; `Cargo.toml`'s QEMU `-m` was bumped to `1024` MiB at the same time),
`MAX_INODES=512` (raised from 64, same reason), each inode with 12 direct blocks + one
single-indirect block (max **single-file** size bounded by that per-inode block-pointer capacity,
~4 MiB — independent of `NUM_BLOCKS`, not an arbitrary per-file cap).
`NO_BLOCK = u32::MAX` is the "unallocated" sentinel (block `0` is a valid block, unlike FAT32's
cluster numbering). Directories are ordinary inodes holding fixed 32-byte records (real names,
`NAME_MAX=26`) that grow additional blocks on demand — no `DirectoryFull` dead end. `unlink`/
`rmdir` only clear a record's `used` byte (no dealloc, consistent with the rest of the kernel).
Root is fixed inode `0`, self-referencing `.`/`..`.

- Real multi-component path resolution (`resolve_path`/`resolve_parent` walk `/`-split
  components, handling `.`/`..`), unlike FAT32's one-component-per-call restriction.
- Real **per-process** cwd: `Process::cwd` (an opaque inode number the kernel never interprets),
  persisted across `fork` (copied)/`execve` (preserved). `oxidebsd_get_cwd`/`oxidebsd_set_cwd`
  resolve the current pid themselves; fall back to a `BOOT_CWD` static for pid `0` (module_init's
  own self-check, which runs before any real process exists).
- Open files stream directly from the block chain on read (`OpenFile::FileRead{inode, position}`,
  no whole-file buffering) — writes still accumulate in a fixed buffer
  (`MAX_WRITE_BUFFER=131072`) and commit to a real inode only at `close`.
- Syscalls: same numbers FAT32 used (`SYS_OPEN=5`, `SYS_CLOSE=6`, `SYS_CHDIR=12`, `SYS_MKDIR=136`,
  `SYS_GETCWD=108`) plus `SYS_UNLINK=109`, `SYS_RMDIR=110`, `SYS_RENAME=111` (4-arg, uses `R10`),
  and `SYS_FSTAT=126`/`SYS_STAT=127`/`SYS_LSTAT=128` — a byte-exact 144-byte musl `struct stat`
  (`MuslStat` in `modules/oxfs/src/lib.rs`, checked against `struct stat`'s real x86_64 layout via
  `third_party/musl/arch/x86_64/bits/stat.h`). `st_uid`/`st_gid`/timestamps are fixed placeholders
  (no uid model or clock source exists yet); `st_mode`'s permission bits are a fixed `0755` for
  every inode. `oxfs_lstat` is a plain alias of `oxfs_stat` — no symlinks exist in this filesystem,
  so there's no "don't follow the final component" behavior to differ on. `oxfs_fstat` resolves the
  caller's fd to oxfs's own `real_fd` via a new kernel-exported `oxidebsd_real_fd_of` (`src/fd.rs`)
  before looking it up in `OPEN_FILES` — the first module syscall handler that needs fd resolution
  done for it explicitly, rather than getting it for free the way `SYS_READ`/`SYS_WRITE` do via
  `crate::fd::read`/`write`. Plus `SYS_GETDENTS=129` — real `getdents(2)`'s own `(fd, buf, count)`
  wire format unchanged (no argument-convention patch needed, unlike `stat`/`open`/`chdir`, since
  there's no string argument to mismatch). Walks a directory's *live* records fresh on every call
  via `dir_nth_used_record` (`.`/`..` included, unlike `open_dir_listing`'s human-readable
  `cat`-a-directory summary) rather than the pre-formatted listing a directory's own open already
  builds, resuming from a per-open-file cursor (`OpenFile::DirListing::dirent_pos`) each call — a
  record that doesn't fully fit in the caller's buffer is left for the next call, matching real
  Linux's own "never split a record across two calls" contract. `d_type` is derived from the real
  inode kind (`DT_DIR`/`DT_REG`); `d_off` is a plain monotonic counter, not a real seek cookie (no
  ported applet calls `telldir`/`seekdir`).
- Seed files (`hello.txt`, `big.txt`, every BusyBox applet ELF) are embedded via
  `include_bytes!(env!(...))` in `module_init`, no build-time disk image needed (unlike FAT32).

**`modules/fat32/`** (superseded, kept for its own build/self-check, not loaded at boot): a
hand-generated (not `mkfs.fat`-produced) FAT32 image, 8.3 names only, one path component per call,
a directory that can never grow past its first cluster, one **kernel-wide** (not per-process) cwd,
whole-file-buffered reads capped at `MAX_FILE_BUFFER=131072`, no `unlink`/`rmdir`/`rename`.
Superseded specifically because these limits started actively blocking BusyBox work.

**`src/fd.rs`** (shared by both): a per-process `(Pid, fd)` scoped registry — the only
coordination channel between independently-loaded modules, since modules can't call each other
directly. Bump-allocated fd numbers, never reused even after close.

## Signal handling module (`modules/signal/`, `src/process.rs`, `src/syscall.rs`)

Real `kill(2)`/`sigaction(2)`/`sigprocmask(2)` + delivery (handler invocation + `sigreturn`).
`SYS_KILL=116`/`SYS_SIGACTION=117`/`SYS_SIGPROCMASK=118`/`SYS_SIGRETURN=119` — all four happen to
match real Linux/BSD wire formats exactly, so the musl-side patch is a pure 4-line number remap
(plus one hardcoded restorer-stub literal, `src/signal/x86_64/restore.s`). Real signal numbers
(`SIGHUP=1`...`SIGSYS=31`, no realtime signals) — unlike most of this ABI's inventions, there was
no reason to pick different ones here.

- `Process::sigactions: [SigAction; 32]` (real `SIG_DFL=0`/`SIG_IGN=1` sentinel convention) plus
  `pending_signals`/`blocked_signals` bitmasks and one `signal_saved_frame` snapshot (not a real
  signal stack — a second signal arriving during handler execution overwrites the snapshot rather
  than nesting; known gap).
- Delivery happens once, at the tail of `syscall_dispatch`, since every path back to userspace in
  this kernel finishes some syscall. `sigreturn` bypasses the normal `Ok`/`Err` carry-flag rewrite
  entirely (it must restore an arbitrary saved `CF`, which the normal convention can't reproduce)
  — the one syscall number not registered in `SYSCALL_TABLE` at all.
- `do_kill` cross-process: immediate for the common case (no handler installed → terminate right
  there, no scheduling needed — even against a currently-*blocked* target); deferred until
  next-scheduled only if the target has a custom handler. No process-group targeting, no
  permission checks (no uid model exists).
- Only 1-argument `void (*)(int)` handlers are supported — no `SA_SIGINFO`.

## BusyBox gap analysis: what's needed for more applets

Almost everything left needs one of a handful of missing kernel capabilities, each unlocking a
cluster of applets at once. New syscall numbers should continue from the highest currently
assigned (check `src/syscall.rs`/module sources rather than trusting stale numbers here).

**`docs/BUSYBOX_APPLETS.md` is the authoritative, per-applet detail behind every row below** —
generated by the exhaustive build probe described in this file's BusyBox section, it names the
exact applet(s) blocked by each gap, not just a count. The counts here (out of the 287 applets
that built at all) are a summary, not the full picture — an applet can need more than one of these
at once, so the rows aren't a clean partition.

| Gap | Status | Blocks (of 287 built) | Placement |
|---|---|---|---|
| `argv[0]` passthrough | done | — | — |
| Real signals | done | — | `modules/signal/` |
| Process groups (`setpgid`/`getpgid`) | done | — | `modules/posix_compat/` |
| termios/`ioctl` | done (no real pty layer) | — | `SYS_IOCTL` in `posix_compat` |
| `stat`/`fstat`/`lstat` | done | — | `modules/oxfs` (`SYS_STAT=127`/`SYS_FSTAT=126`/`SYS_LSTAT=128`) |
| `getdents`/`getdents64` | done | — | `modules/oxfs` (`SYS_GETDENTS=129`; **both** `__NR_getdents` and `__NR_getdents64` had to be remapped to it in musl, not just the former — see this file's musl section's "64-bit-suffixed sibling" gotcha) — real `ls`/`find`/`tree`/`du` confirmed working against it |
| Socket syscalls (`socket`/`bind`/`connect`/...) | not started | 38 (`NEEDS_NETWORK`) | no networking stack exists at any layer; the largest single blocked category — `wget`/`ftpd`/`telnet`/`nc`/`ping`/`route`/`ifconfig`/... |
| A specific missing syscall per applet (`chmod`/`chown`/`link`/`mknod`/`flock`/`fsync`/`ftruncate`/`fallocate`/SysV IPC/`setrlimit`/`statfs`/sched-priority/`reboot`/`sync`/`inotify`/`chroot`/namespaces) | not started | 36 (`NEEDS_SYSCALL`) | see `docs/BUSYBOX_APPLETS.md`'s own breakdown for which applet needs which — no single fix, a checklist of small ones |
| `/proc` filesystem — per-process (`stat`/`cmdline`/`status`, dir listing, `stat(2)`/`lstat(2)`) | done | — | special-cased path prefix inside `modules/oxfs` (no VFS layer exists to plug a separate procfs module into, and oxfs already owns `SYS_OPEN`/`SYS_GETDENTS`/`SYS_STAT`; see `oxfs`'s own `proc_open`/`proc_kind`), synthesizing content from new kernel-exported accessors (`src/process.rs`'s `oxidebsd_proc_exists`/`_pid_at`/`_stat_line`/`_cmdline`/`_status`) — no real inode/blocks (`write_proc_stat` fakes `st_mode` only; every other field, including `st_size`, is a fixed placeholder). Includes a minimal `/proc/<pid>/task/<tid>/` redirect (`tid == pid` only, this kernel has no real threading) since `pstree` unconditionally `opendir()`s it *and* `stat()`s it for uid/gid, silently skipping a pid entirely if either fails, rather than falling back to the plain per-pid files — confirmed live: without `stat()` support, `pstree` produced zero output, not a degraded one. Unlocks `pidof`/`pgrep`/`pkill`/`pstree`/`minips`. `chdir` into `/proc` and system-wide files (`/proc/meminfo`/`uptime`/`stat`, for `top`/`free`/`uptime`) remain explicit known gaps of this pass, not implemented |
| `/proc` filesystem — per-fd (`/proc/<pid>/fd/`) | not started | subset of the above | needs `src/fd.rs` to support enumeration (currently lookup-only) — would unlock `lsof`/`fuser` specifically |
| Console/VT ioctls, serial/tape/I2C hardware, syslog, real pty | not started | 24 (`NEEDS_HARDWARE`) | each needs a real device/driver this kernel doesn't model — not one gap, several unrelated small ones (see `docs/BUSYBOX_APPLETS.md`) |
| Real block device driver + mount table | not started | 20 (`NEEDS_BLOCKDEV`) | `mount`/`umount`/`fsck`/`mkswap`/`fdisk`/`blkid`/... — not applet-specific, would matter for actual persistence too, not just more applets |
| uid/passwd-db model | not started, trivial stub possible | 16 (`NEEDS_UID`) | fully module-able, no-op stubs get partway there (`modules/posix_compat`) — `adduser`/`passwd`/`su`/`login`/... need a real `/etc/passwd`-equivalent to go further |
| clock + `nanosleep` | not started | 9 (`NEEDS_CLOCK`) | monotonic tick read is a trivial module accessor; `nanosleep` blocking is kernel-resident (new `BlockReason::Sleeping`, woken from the timer IRQ handler); a real wall clock needs a new RTC driver (module code has no port-I/O primitive exposed yet — a smaller prerequisite) |
| Init-system/service-supervisor framework | not started, out of scope | 6 (`NEEDS_INIT`) | `runsv`/`svlogd`/`bootchartd`/... — this kernel has no init framework to plug into at all |
| `tcsetpgrp`/real job control | blocked on a pty/foreground-pgrp concept | — (folded into `NEEDS_HARDWARE`) | — |
| `uname`/`gethostname` | not started, trivial | — | fully module-able, fixed strings (`modules/posix_compat`) |

**83 more candidate applets didn't even build** — `docs/BUSYBOX_APPLETS.md` breaks those down too:
54 need real Linux kernel uapi headers (`linux/*.h`, `mtd/*.h`) musl deliberately doesn't vendor
(hardware/device-ioctl tools with no portable equivalent — not fixable without vendoring headers
this port has otherwise avoided needing), 25 need a companion Kconfig option a single-symbol flip
didn't resolve (SELinux infrastructure, utmp support, IPv6 feature-flag variants, alias applets
needing their parent enabled), 3 were docs/example files my own candidate-extraction grep
mismatched (never real applets at all), and 1 (`lzopcat`) is a genuine link error (an
undefined-symbol gap in how BusyBox's own compression-transformer infrastructure gets pulled in).

## Dependency notes

- `x86_64` crate: `default-features = false, features = ["instructions", "abi_x86_interrupt"]` —
  the default feature set pulls in `step_trait`, an unstable-API moving target that has broken
  this crate against newer nightlies before.
- `bootloader` pinned to `0.9` (not `0.11+`'s artifact-dependency API) — keeps the setup in one
  crate; `map_physical_memory` feature is required for `BootInfo::physical_memory_offset` to exist
  at all.
- `linked_list_allocator`: `default-features = false` — its default `LockedHeap` depends on
  `spinning_top`, a second spinlock crate alongside `spin` (used everywhere else here).
- `pc-keyboard` 0.9's type is `PS2Keyboard<L, S>`, not `Keyboard<L, S>` (older tutorials online
  reference the pre-0.9 name). Decoding is two calls through the *same* locked guard: `add_byte` →
  `KeyEvent`, then `process_keyevent` → `DecodedKey`.
- `pic8259`/`uart_16550` are deliberately **not** dependencies — both wrap a handful of
  `outb`/`inb` calls against a stable, well-documented protocol, small enough that owning the code
  (`src/pic.rs`, `src/serial.rs`) outweighs the dependency. Different call than `pc-keyboard`
  (hundreds of lines of scancode tables) or `linked_list_allocator` (safety-critical free-list
  logic), which stay external.
