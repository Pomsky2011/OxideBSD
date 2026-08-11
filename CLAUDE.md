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
- A real networking stack (`src/pci.rs`, `src/net/*`, `modules/net/`): PCI + an rtl8139 driver,
  Ethernet/ARP/IPv4/ICMP, UDP/TCP/raw-ICMP sockets, `poll(2)`, and real hostname resolution over
  musl's own DNS stub resolver (no DNS protocol code of its own) — see this file's own "Real
  networking" section.

Known, deliberate gaps: no pointer validation in `sys_read`/`sys_write`, no module unload/reload,
no preemption, no copy-on-write fork, no frame deallocation anywhere,
`sys_read` on stdin is non-blocking (busy-polled by userland), no real mount table/VFS layer (a
real ATA disk driver and real oxfs mount/format persistence exist now — see this file's own "Real
disk persistence" section — but only for oxfs's own fixed backing store, not a general
block-device-agnostic filesystem interface),
no IPv6, no real routing table (one default-gateway rule
only). See "BusyBox gap analysis" below for what's needed to go further. Architecture decisions for
remaining subsystems haven't been made — discuss with the user before large structural commitments.

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
`SYS_DUP=125`, `SYS_FSTAT=126`, `SYS_STAT=127`, `SYS_LSTAT=128`, `SYS_GETDENTS=129`,
`SYS_UNAME=137`, `SYS_CLOCK_GETTIME=138`, `SYS_NANOSLEEP=139`, `SYS_SOCKET=140`, `SYS_BIND=141`,
`SYS_SENDTO=142`, `SYS_RECVFROM=143`, `SYS_SETSOCKOPT=144`, `SYS_CONNECT=145`, `SYS_LISTEN=146`,
`SYS_ACCEPT=147`, `SYS_POLL=148`, `SYS_SOCKETPAIR=149`, `SYS_SET_TID_ADDRESS=150`, `SYS_FCNTL=151`,
`SYS_SHUTDOWN=152`, `SYS_READV=153`, `SYS_READLINK=154`, `SYS_SYMLINK=155`, `SYS_SETITIMER=156`,
`SYS_GETITIMER=157`, `SYS_GETUID=158`, `SYS_GETEUID=159`, `SYS_GETGID=160`, `SYS_GETEGID=161`,
`SYS_SETUID=162`, `SYS_SETGID=163`, `SYS_GETGROUPS=164`, `SYS_CHMOD=165`, `SYS_CHOWN=166`) is
OxideBSD's own invention —
numbers/shapes picked for what porting musl/BusyBox
actually needed, not copied from FreeBSD/Linux (a few, like `pipe`/`dup2`/signal numbers, happen to
match real wire formats anyway; check `src/syscall.rs` and module sources for the current highest
number before assigning a new one). errno **is meant to** use FreeBSD's values where Linux/BSD
diverge — **but this was never actually completed on the musl side, and is currently broken for at
least `EPROTONOSUPPORT`/`ENOTSOCK`/`EDESTADDRREQ`/`EADDRINUSE`/`EHOSTUNREACH` in `src/net/udp.rs`
(`ENOSYS` itself was the same story until the Session/controlling-tty pass below fixed it — see
that section's own "two more real bugs" entry for why and how).**
Whatever this file returns via the carry-flag ABI becomes musl's raw `errno` value directly (see
`third_party/musl/arch/x86_64/syscall_arch.h`'s `jnc`/`neg` conversion) — so it must match whatever
number musl's *own* compiled-in `bits/errno.h` (`third_party/musl/arch/generic/bits/errno.h`, since
no x86_64-specific override exists) actually defines that symbolic name as, not a real-BSD nod that
only lives on the kernel side. `EBADF`/`EINVAL`/`ECHILD`/`ENOEXEC`/`EPIPE`/`ESRCH`/`ENOTTY` happen
to be identical between Linux/generic and FreeBSD numbering, so those are accidentally fine; `src/
net/udp.rs`'s `ENOTSOCK=38`/`EDESTADDRREQ=39`/`EADDRINUSE=48`/`EHOSTUNREACH=65` are real FreeBSD
values that musl's own header does **not** share (its real values are `88`/`89`/`98`/`113`
respectively — note musl's real `ENOSYS` **was** this codebase's own wrong `ENOTSOCK` value before
the fix below, a doubly-confusing collision while both were live) — found while tracing an
unrelated `wget` HTTPS failure, fixed only for the two new-that-session constants that pass
actually introduced (`src/syscall.rs`'s `EPROTONOSUPPORT=93`/`EAGAIN=11`/`ENOTSOCK=88`, now correct
and matching musl), not yet fixed for the pre-existing four still in `udp.rs` — deliberately left as
a flagged, known bug rather than a silent sweeping renumbering; discuss scope with the user before
touching it further. `ENOSYS` was fixed in a later pass (see below) once a live test showed the
mismatch concretely breaking real functionality, not as part of this same sweep.

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
  `rename`/`readlink`/`symlink` (oxfs) and `chdir`/`mkdir` — **any future libc call ported here
  needs the same audit**: matching the syscall *number* isn't sufficient if the argument shape
  differs.
- **A hand-written asm stub can bypass the `__NR_*` remap table entirely.** `src/process/x86_64/
  vfork.s` doesn't go through `syscall(SYS_vfork, ...)` at all — it's raw x86_64 asm hardcoding the
  real Linux syscall number (`58`) directly (`third_party/musl` ships several architectures' worth
  of hand-written `vfork.s`, this project only touches the `x86_64` one). Editing
  `bits/syscall.h.in`'s `__NR_vfork` entry has zero effect on it — found live via `xargs` (whose
  own child-spawn path calls `vfork()` directly), failing with `ENOSYS`/`echo: No child process`.
  Fixed by hardcoding OxideBSD's own `SYS_FORK` number (`2`) directly in the asm instead — a real
  fork(), not true vfork() share-until-exec semantics, but a real, POSIX-legal implementation
  vfork() is explicitly allowed to be. **Any future syscall with its own hand-written arch-specific
  asm stub (check `third_party/musl/src/*/x86_64/*.s`) needs this same direct-patch treatment, not
  just a header remap.**
- **`utimensat` (`SYS_UTIMENSAT=167`, `modules/oxfs`)** — same call-site patch pattern as `chown`/
  `rename`: `src/stat/utimensat.c` drops the always-`AT_FDCWD` `fd` argument (this port has no real
  dirfd-relative resolution, same limitation every other `*at()`-shaped call already accepts) and
  computes `path_len` via `strlen()`, passing `(path_ptr, path_len, times_ptr, flags)`. The kernel
  side is a real *existence check* only (`ENOENT` for a path that doesn't resolve, success
  otherwise) — oxfs has no per-inode timestamp fields at all yet, so there's nothing to actually
  update. That's still enough to unblock BusyBox's `touch.c`: it treats `ENOENT` specifically as
  "must create the file" and falls back to `open(O_CREAT)`, which already worked — an
  unconditional-success stub (no existence check at all) would have silently broken that fallback
  instead, making `touch newfile` a no-op rather than creating anything.
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
VT/MTD/I2C/netlink ioctl tools with no portable equivalent). `modules/oxfs/src/test_busybox.sh`
(seeded into oxfs at `/test_busybox.sh`, run by hand via `sh /test_busybox.sh` at the hush prompt)
exercises ~40 real applet/control-flow checks against hand-written expected output and found three
real, now-fixed bugs this way, none visible from `cargo test`/`clippy` staying green: the
`wait4`/exit-status encoding bug and the `kill(pid, 0)` gap (see this file's own Process/scheduler
section) and the missing `vfork`/`utimensat` syscalls (see this file's own musl-port section). It
was originally written as a flat sequence of real applet invocations, not an if/for-based test
harness, because this kernel's actual `sh` build had no shell control flow at all -- `make
allnoconfig` writes an explicit "not set" line for every symbol in the whole BusyBox Kconfig tree
up front, so the later `make oldconfig` pass (which only fills in symbols with *no* prior answer)
never got a chance to apply hush's real `default y` for `HUSH_IF`/`HUSH_LOOPS`/`HUSH_CASE`/
`HUSH_FUNCTIONS`/`HUSH_TICK`/`HUSH_TEST`/... -- they'd stayed off ever since `HUSH_INTERACTIVE`/
`HUSH_JOB` were first turned on. Fixed in `build.rs`'s `configure_busybox_single_applet` (flips
those lines directly, the same way it already did for `HUSH_INTERACTIVE`/`HUSH_JOB` -- see its own
doc comment), and the script itself rewritten to match: real `if`/`for`/`while`/`until`/`case`,
functions with `local` variables, `$((...))` arithmetic, both forms of command substitution, and a
real `PASS`/`FAIL` tally via shell arithmetic instead of `ok_<name>`/`bad_<name>` marker files
counted with `ls | wc -l`. `build_busybox_applet` is also now
staleness-checked (skips its own `allnoconfig`/`oldconfig`/`make` sequence if that applet's already-
built binary is newer than `third_party/busybox`, `build.rs` itself, **and** `musl_sysroot`'s own
`lib/libc.a`) and the whole roster builds in parallel across a worker pool sized to
`available_parallelism()` — both load-bearing at this scale, not true at 24 applets. `modules/
fat32`'s own embedded image dropped busybox applets entirely over this same change (its 8.3-short-
name format can't hold names over 8 characters, and its fixed sector budget was sized for a much
smaller roster) — harmless, since nothing loads that image at boot.

**Two real bugs found in this staleness check itself, both live, both now fixed:**
1. An earlier version only compared `busybox_source_mtime`/`build_rs_mtime` — a real musl-side
   syscall fix (the `getdents`/`getdents64` one documented in this file's own musl-port section)
   silently left every already-built applet linked against a stale `libc.a`. Fixed by adding
   `musl_sysroot`'s own `lib/libc.a` mtime into the same `freshness_floor` comparison.
2. **Found much later, live, testing `su`**: even with that fix, deciding "stale, rebuild" and
   re-running `make allnoconfig` + `make` against an applet's *same, already-populated* `O=`
   out-of-tree directory doesn't guarantee every object file actually gets recompiled — BusyBox's
   own incremental build tracks its own source files, but never musl's *installed* sysroot headers
   (`target/musl-sysroot/include/...`) as a dependency. Confirmed live: after a real musl
   syscall-number fix (the `setgroups`/`SYS_KILL` collision — see the Session/controlling-tty
   section below), `su`'s own `libbb/change_identity.o` still had a five-day-old mtime predating
   the fix entirely, one of ~172 (of 177) object files in that applet's build directory that never
   got recompiled — the linked binary's own mtime looked "freshly rebuilt" by the outer check, but
   silently still ran the old, broken code. **Only a full `rm -rf` of the stale `O=` directory
   before re-running the build reliably fixes this** — trusting BusyBox's own incremental tracking
   here has already been proven wrong twice. More expensive than a true incremental rebuild (every
   affected applet rebuilds fully from scratch, ~38 minutes for the whole ~300-applet roster the one
   time this triggered), but only runs when something genuinely changed, not on every `cargo build`.

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
- `src/vga.rs`'s `Writer` special-cases raw `0x08`/`0x7f` as "step cursor back, erase" so the same
  backspace idiom works on the VGA console. The `Writer` is a true 2D-addressable console with a
  minimal ANSI/VT100 CSI escape parser (cursor position/relative moves, erase display/line, SGR
  colors) — added so full-screen BusyBox applets (`vi`, `clear`, `reset`) render correctly instead
  of printing raw escape bytes; unrecognized sequences are parsed just enough to find the final
  byte, then dropped silently.
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
- **Real `#!interpreter [arg]` shebang support**, added after live testing found `sh /test_busybox.sh`
  worked but the far more natural `./test_busybox.sh`/`/test_busybox.sh` (a `+x` script invoked
  directly, the normal way anyone actually runs a script) failed `ENOEXEC` ("Exec format error") —
  this kernel's `elf::load` has no shebang awareness at all, and BusyBox `hush` has no userspace
  `ENOEXEC`-fallback-to-script-interpretation logic of its own either (real Unix shells generally
  don't need one, since a real kernel's own `binfmt_script` already handles `#!`; confirmed by
  grepping `third_party/busybox/shell/hush.c` for `ENOEXEC` — no hits at all). Before
  `Elf::parse` is even attempted, `do_execve` peeks at the target's first two bytes; if `#!`, it
  parses the rest of the line as `interpreter` + an optional single trailing argument (real
  `binfmt_script` semantics: everything after the interpreter path, once leading whitespace is
  trimmed, becomes *one* argument, never further word-split), then re-targets the open/read loop
  at the interpreter instead — looping (capped at `MAX_SHEBANG_DEPTH = 4`, past which it's a real
  `ELOOP`) to follow a chain of interpreter scripts, matching real Linux's own bounded recursion.
  `argv` is rebuilt as `[interpreter, optional-arg?, script-path] + original-argv[1..]` (the
  caller's own `argv[0]` is always discarded, matching real Linux — the two need not be equal in
  the first place, see `RawArgvEntry`'s own doc comment two paragraphs up), and `Process::comm`
  (`/proc/[pid]/comm`) is taken from the *actual* interpreter's own basename rather than the
  script's, matching real Linux: only the interpreter is ever really `exec`'d at the kernel level,
  the script path is just data handed to it as an argument. The one already-open-user-address-space
  subtlety this relies on: an interpreter path parsed out of file content lives only in a kernel
  heap `Vec<u8>`, not the caller's own user-space buffer the way the *original* exec target does —
  passing a raw pointer to that kernel allocation into the same open/read syscall path still works
  correctly regardless of which process's `CR3` is currently active, since kernel-heap virtual
  addresses are mapped identically in every address space's page table (see this file's own
  `AddressSpace::new`/`fork` notes above). Confirmed live: `./test_busybox.sh`/`/test_busybox.sh`
  (both `#!/bin/sh` — the interpreter resolves to the already-seeded `/bin/sh`) now run directly
  and report all-pass, the same self-test `sh /test_busybox.sh` already did.
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
- **`do_wait4`'s reported status is real `wait(2)`-encoded now — `oxidebsd_sys_exit` shifts a
  normal exit code into bits 8-15** (`WEXITSTATUS`'s own bit position; low 7 bits zero, matching
  `WIFEXITED`), not the raw code. Found live, the hard way, via `modules/oxfs/src/test_busybox.sh`
  (a real applet self-test run interactively through `hush`): the old, unshifted convention was
  self-consistent against every prior test here (both the write side and every test's own read
  side agreed on it), but real POSIX-compliant code checking status via `WIFSIGNALED`/`WTERMSIG`
  the standard way — BusyBox `hush`'s own `checkjobs()` in `third_party/busybox/shell/hush.c` — got
  it completely wrong: *any* applet exiting normally with a nonzero code (`touch`/`tee`/`rm`/...
  erroring for their own ordinary reasons, extremely common) had its raw low byte misread as
  "terminated by signal N" (`exit(1)` decoded as `WTERMSIG == SIGHUP`), printing a spurious
  "Hangup" after nearly any failing command and — per `checkjobs`' own default-signal-death
  handling — corrupting later commands in the same interactive session, cascading into
  increasingly garbled output. Signal-based termination (`process::do_kill`'s `Terminate` branch,
  and `SignalDelivery::Terminate`'s self-delivery path in `src/syscall.rs`) already passes
  `terminate_process`/`do_exit` a pre-encoded `128 + sig` value directly — *not* shifted by this
  fix, and must stay that way: its low 7 bits already equal the real signal number, everything
  `WIFSIGNALED`/`WTERMSIG` actually look at, so it was already real-wait-status-compatible.
  `oxidebsd_sys_exit` is the *only* place a genuine user `exit(code)` value becomes a `Zombie`
  status, which is why the shift belongs there and not inside `do_exit`/`terminate_process`
  themselves (shared by both conventions). `userland/fork-exec-smoke`'s own status check
  (`CHILD_EXIT_CODE = 77`) updated to `77 << 8` to match; `uid-syscall-smoke`'s `status == 0` check
  needed no change (shift-invariant).
- **`kill(pid, 0)`** — the real POSIX "does this process exist" existence-check convention (send no
  signal, just check) used to be a flat `EINVAL` (`do_kill` only accepted `1..=31`). Found the same
  way, via the same self-test's own `kill -0 $$` check. Fixed: `sig == 0` now does a real
  existence-only check (self or cross-process; a zombie still counts as "exists" until reaped),
  bypassing the pending-signal bitmask entirely rather than computing a bogus `1 << (0 - 1)`.
- `tests/fork_wait.rs` + `userland/fork-exec-smoke/` is the automated coverage for this subsystem
  (fork/wait4/exit round trip, no filesystem/execve involved) — see Test architecture above.
  `modules/oxfs/src/test_busybox.sh` (see the BusyBox section below) is a real, much broader
  applet-level self-test meant to be run by hand at the hush prompt (`sh /test_busybox.sh`) — it
  found both bugs above and can't be automated the way `fork_wait.rs` is, since OxideBSD's stdin is
  real PS/2 keyboard input only, with no way to script keystrokes into a real interactive session.

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
- A module panic is always fatal to that call (no unwinding, `panic-strategy = "abort"`):
  `build.rs`'s `discover_panic_symbol` finds each module's toolchain-hashed panic-entry symbol via
  `llvm-nm`, and the loader's resolver points it at `module_panic_trampoline`, which logs and then
  picks one of two outcomes — **per-module, via a `fatal_on_panic: bool` passed to
  `module::load`** (`false` for every module except `oxfs`). `false` just `hlt_loop()`s (no
  per-module restart exists yet, but nothing else about kernel state is known to be untrustworthy
  either). `true` (`oxfs` only) reboots the whole system instead: with no disk attached, a
  filesystem module's entire state *is* the in-memory filesystem, and resuming past an unrecovered
  panic there would mean silently reverting every file to empty; with a real disk attached (see
  this file's own "Real disk persistence" section), a panic mid-mount/mid-format risks a torn
  superblock/inode-table write instead — worse to resume past than a purely in-memory panic ever
  was, so `true` is if anything more justified once a backing store exists, not less. Either way, a
  clean reboot beats resuming into untrusted state.
  `module::CURRENT_MODULE_FATAL` (a `static mut`, same single-core/no-concurrent-writer reasoning
  as `gdt.rs`'s `CURRENT_RSP0`) is set immediately before every call into module code — `load`'s
  own `module_init` call, and `syscall::dispatch`'s call into a registered handler (via
  `module::mark_active_module_by_address`, which maps the handler's function pointer back to
  whichever loaded module's `[base, base+size)` range contains it, since dispatch only has the
  pointer, not which module registered it) — and read by the trampoline if that call panics.
  Defaults fatal (reboot) wherever the answer can't be determined, the safe direction for a case
  that "can't happen." The same `true` treatment will apply to `fat32`/a future `ext4`/`xfs`/...
  once any of them load at boot again.
- `serial_println!` can't take implicit `{name}`-style captures (its `concat!`-based expansion
  blocks it) — always use explicit positional args; `serial_print!` doesn't have this restriction.
- Known limits: no module unload/reload, no versioning, no inter-module direct calls (only
  module→kernel, via each module's own resolved symbol table — this is why `src/fd.rs`'s registry
  exists at all, as the only coordination point between e.g. `oxfs` and `native_abi`).

## Filesystem: oxfs (live) and FAT32 (superseded)

**`modules/oxfs/`** is the live filesystem — a real Unix-shaped inode/block filesystem. In-memory
by default, with real optional persistence to an attached ATA disk now — see this file's own "Real
disk persistence" section, below. Fixed-size `static mut` pools: `NUM_BLOCKS=8192` ×
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
  every inode. `oxfs_lstat` was a plain alias of `oxfs_stat` until real symlinks landed (see this
  file's own gap table's "Real symlinks" row) — now the one place `stat`/`lstat` actually differ:
  `oxfs_stat` follows a final symlink component, `oxfs_lstat` doesn't. `oxfs_fstat` resolves the
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

## Real disk persistence (`src/ata.rs`, `modules/oxfs`)

OxideBSD's first real block device driver, and the mechanism that finally lets oxfs survive a
reboot — the last piece missing before "install OxideBSD" meant anything. Scoped deliberately:
this closes real disk I/O and real oxfs mount/format persistence specifically, not a general VFS/
mount-table layer (`mount`/`umount`/a real block-device-agnostic filesystem interface remain
unstarted — see this file's own BusyBox gap table).

- **`src/ata.rs`**: a hand-rolled ATA PIO (Programmable I/O) driver, kernel-resident (boot-wired
  from `main.rs`, like `rtl8139::init()`, not a dynamically loaded `modules/*` crate) — classic
  legacy IDE, LBA28, **polling only, no IRQ**. Fixed legacy ports (0x1F0/0x3F6 primary,
  0x170/0x376 secondary — QEMU's default `i440fx` PIIX3 IDE controller exposes these regardless of
  PCI enumeration, so unlike `rtl8139` there's no PCI probing at all). Every BSY/DRQ wait is
  bounded by a real `crate::tsc`-based deadline (never `hlt()`, never an unbounded poll) —
  load-bearing, not defensive: `oxidebsd_block_read`/`_write` (below) are reachable from inside a
  real syscall handler with interrupts masked, exactly the class of context CLAUDE.md's own
  "Real networking" section already documents two real freeze bugs for (`hlt()`-in-syscall,
  `ticks()`-frozen-during-syscall) — PIO's synchronous, CPU-driven nature (no DMA, the CPU itself
  shuttles every word) is what makes polling-only correct here at all, an IRQ-driven design would
  need the same `IRQ_HANDLERS`/`without_interrupts` treatment `rtl8139` already established instead.
- **One fixed target: secondary channel, master.** `bootimage` always attaches this kernel's own
  boot image as `-drive format=raw,file=<image>` with no explicit `if=`, which QEMU resolves to the
  *primary* IDE master by default — so `oxidebsd_block_read`/`_write`/`_device_present` (the
  kernel-exported API `modules/oxfs` calls, registered in `src/module.rs`'s
  `resolve_external_symbol` the same way `oxidebsd_alloc_fd`/`oxidebsd_proc_*` already are) target
  the secondary channel's master drive specifically, and `Cargo.toml`'s `run-args`/`test-args` pin
  the data disk there explicitly (`-drive if=none,id=oxfsdisk,... -device
  ide-hd,drive=oxfsdisk,bus=ide.1,unit=0`), never a second bare `-drive` relying on QEMU's own
  index-assignment heuristics. `run-args` points at `target/oxfs_disk.img` (`build.rs`, created
  **only if it doesn't already exist** — the entire mechanism by which it survives across `cargo
  run` invocations); `test-args` points at `target/oxfs_test_disk.img` (`build.rs`, always freshly
  zeroed), applied globally like the existing `-nic` device — every test gets it, only
  `tests/ata_smoke.rs`/`tests/oxfs_persistence_syscall_smoke.rs` call `ata::init()` at all, so every
  other existing test's `oxidebsd_block_device_present()` stays `0` and behaves exactly as before
  this pass.
- **On-disk layout**: physical disk block `0` is the superblock (magic `b"OXFS"` + version +
  `NUM_BLOCKS`/`MAX_INODES`/root-inode fields); blocks `1..17` are the packed inode table (512
  inodes at a fixed 128-byte stride — real content is 67 bytes, rounded up to a stride that divides
  `BLOCK_SIZE` evenly — exactly 16 blocks); block `17` is the block-used bitmap (`NUM_BLOCKS` bits =
  1024 bytes, one block). Real data starts at block `18`: oxfs's own in-memory block number `i` maps
  to physical disk block `18 + i`. **Never a raw transmute/memcpy of `Inode`** — it isn't
  `#[repr(C)]` and `InodeKind` has no explicit discriminant, so true layout isn't guaranteed across
  compiler versions/profiles; `pack_inode`/`unpack_inode` serialize by hand instead, the same
  raw-byte-offset idiom `write_dir_record`/`dir_record_inode` already established for directory
  records.
- **Mount-or-format, decided once in `module_init`**: no disk attached (most tests) → today's
  exact original in-memory-only behavior, byte for byte unchanged. Disk attached, superblock magic
  matches → **mount**: load the bitmap + inode table wholesale, then eager-load only the data
  blocks the bitmap marks used (not an unconditional sweep of all `NUM_BLOCKS` — see the
  performance note below), skipping the entire seed-everything/self-check path. Disk attached,
  magic mismatch (fresh/zeroed disk, including every real first boot) → **format**: run the
  original seed-everything + self-check exactly as before, then `flush_all_to_disk` writes the
  superblock, full bitmap, full inode table, and every used data block out once, so the *next* boot
  mounts instead of reformatting. The self-check (asserts `hello.txt`'s literal bytes, `big.txt`'s
  formula, chmod/chown round trips, ...) only ever runs on the format path — deliberate: mount never
  re-validates seeded content, so a real user session that later edits/deletes those files is
  expected and fine on every subsequent boot.
- **Write-through persistence, centralized at three confirmed choke points**: `write_block`,
  `write_inode`, `set_block_used` (`modules/oxfs/src/lib.rs`) are the *only* functions that ever
  touch `BLOCKS`/`INODES`/`BLOCK_USED` — verified exhaustively before this landed, no bypass exists
  anywhere in the file — so persisting there catches every mutation (`dir_insert`, `unlink`,
  `oxfs_close`, `chmod`/`chown`, `mkdir`, `symlink`, ...) with no dirty-tracking/sync-on-shutdown
  scheme needed (there's no clean shutdown hook to hang one off anyway). `write_inode`/
  `set_block_used`'s own persist helpers repack the *entire* inode-table block / bitmap fresh from
  the in-memory arrays (already the complete source of truth) rather than a real read-modify-write —
  no disk read needed on a write at all.
- **`PERSISTENCE_READY`** (`modules/oxfs/src/lib.rs`, a `static mut` gate alongside device
  presence): deliberately `false` for the *entire* duration of both `format_fresh_filesystem`'s
  seeding and `mount_from_disk`'s own load, even though both call `write_block`/`write_inode`/
  `set_block_used` heavily — without this, formatting (~300 applets' worth of churn) and mounting
  (reading data back into these exact same structures) would each trigger thousands of redundant
  block-sized disk writes: data already correct on disk, or not yet meant to be there at all.
  `module_init` sets it `true` exactly once, right after its own mount-or-format branch completes
  and before any real syscall becomes reachable — from that point on, every further write really is
  a live mutation from a running process and belongs on disk immediately.
- **Known, accepted limitation: mount-time load is bitmap-filtered, not true lazy fault-in.** The
  `x86_64` crate (0.15.5, this project's pinned version) has no `rep insw`/`outsw` wrapper, so every
  512-byte sector transfer is 256 individually-trapped port reads under QEMU's software TCG —
  comparable in class to the O(n²) frame-allocator and `invlpg` stalls this file's own Memory
  Management section documents. Loading only used blocks (not an unconditional full 32 MiB sweep)
  is this pass's mitigation; true on-demand fault-in (extending `read_block` itself with a
  per-block "loaded yet" flag) is the natural next step if that still measures badly.
- **`fatal_on_panic` stays `true` for oxfs** (see this file's own Dynamic Kernel Modules section) —
  if anything more justified now than before real persistence existed: a panic mid-mount/mid-format
  risks a torn superblock/inode-table write, worse to resume past than a purely in-memory panic ever
  was.
- **No raw block device is exposed to userland** (no `/dev/sda`-style node) — the disk is purely an
  internal implementation detail of oxfs's own persistence for now.
- Verified via `tests/ata_smoke.rs` (direct-function-call sector read/write round-trip at several
  LBAs plus a byte-order sweep — no real syscall surface exists for raw block I/O in this phase) and
  `tests/oxfs_persistence_syscall_smoke.rs` + `userland/oxfs-persistence-syscall-smoke/` (a real
  spawned ELF via genuine `SYSCALL`/`SYSRETQ` that creates+writes+closes a file, proving the
  write-through path is safe to run with interrupts masked — the exact class of bug this file's own
  `hlt()`-in-syscall/`ticks()`-frozen-during-syscall history was only ever caught by real-`SYSCALL`
  tests, never a plain-Rust-function one). **Not covered by automated testing**: persistence
  actually surviving a real QEMU restart, inherently two separate `cargo run` invocations — a
  manual, user-driven check (create a file at the hush prompt, exit QEMU, `cargo run` again, confirm
  the file is still there), the same "hand off anything needing a live interactive QEMU session"
  precedent this project already follows elsewhere.

## Mount table (`modules/oxfs/`)

A real, but deliberately scoped, mount table — `mount --bind`/`mount -t tmpfs` only, closing the
`mount`/`umount`/`mountpoint` row of the BusyBox gap table. Not a general pluggable-filesystem-type
VFS: there is exactly one real block device and one real filesystem in this kernel, and modules
can't call each other directly, so there's nothing else to plug in. `mount`/`umount`/`mountpoint`
were already-seeded, already-built BusyBox applets before this — the driver/persistence half of
"real block device" was already done (see "Real disk persistence" above); only the mount-table
concept itself was missing.

- **A second, purely in-memory inode/block pool for tmpfs**, reusing oxfs's existing accessors
  rather than threading a "which pool" parameter through the file: `BLOCKS`/`BLOCK_USED`/`INODES`
  are simply extended with a tail region (`TMPFS_NUM_BLOCKS=1024`/`TMPFS_MAX_INODES=128`, 4 MiB) —
  every whole-pool disk-persistence loop already iterated `0..NUM_BLOCKS`/`0..MAX_INODES` by the
  named constant rather than `BLOCKS.len()`/`INODES.len()` (verified before this landed), so that
  code stays correctly bounded to the real, persisted range with zero changes. `write_block`/
  `write_inode`'s own persist hooks (`persist_data_block_if_ready`/`persist_inode_block_if_ready`)
  get a one-line early return for an index past the real range — a tmpfs mount is never written to
  disk at all, matching real tmpfs semantics (nothing to persist it to). *Block* allocation only
  needs to know this second pool exists in one place, `inode_ensure_block_at` (confirmed via grep
  to be the sole caller of `alloc_block`/`alloc_indirect_block`, covering both real file writes and
  directory growth) — it picks `alloc_tmpfs_block`/`alloc_tmpfs_inode` instead based on whether
  `inode_num >= MAX_INODES`. *Inode* allocation for a newly **created** entry needed its own
  chokepoint, `alloc_inode_in(parent)`, added only after `tests/mount_syscall_smoke.rs` caught a
  real bug live: `oxfs_mkdir`/`oxfs_symlink`/`oxfs_open`'s `O_CREAT`-via-`oxfs_close` commit path
  all called plain `alloc_inode()` unconditionally, so a file created *inside* a tmpfs-mounted
  directory still got a real-pool inode — reporting the wrong `st_dev` and (worse) being silently
  write-through-persisted to disk despite living inside a supposedly non-persistent tmpfs mount.
  `alloc_inode_in` picks `alloc_inode`/`alloc_tmpfs_inode` by the same `parent >= MAX_INODES` test,
  now the single shared call site for all three "create a new named entry" operations. Every other
  function (`read_inode`, `dir_lookup`, `dir_insert`, `resolve_path_impl`, ...) works unmodified
  over the unified index space. Deliberately **never reclaimed on unmount** (a tmpfs mount's
  inodes/blocks stay marked used forever) — matches this
  module's existing "no deallocation anywhere" stance (`unlink`/`rmdir` already only clear a
  directory record's `used` byte).
- **The mount table itself** (`MountEntry`/`MOUNTS`, `MAX_MOUNTS=8`): each entry records the real
  inode a mountpoint path resolved to (`mountpoint_inode`, shadowed while the mount is active) and
  where lookups redirect to instead (`target_root_inode` — the source directory's own inode for a
  bind mount, a freshly allocated tmpfs root directory for a tmpfs mount). `resolve_path_impl`
  gets exactly one inserted line, checking `active_mount_for` right after each component's
  `dir_lookup` and before the existing symlink-follow check — applies to every component, not just
  the last, matching real Unix (`stat`ing a mountpoint itself reports the mounted fs's root).
  Scanned from the end, so a mount stacked on top of an already-mounted directory wins and
  unmounting removes the most recent one first (real LIFO stacking).
  - **Tmpfs mount root** gets real `.`/`..` directory records the same way `oxfs_mkdir` already
    creates them, with `..` pointing at the *mountpoint's own real parent* — `cd ..` from inside a
    tmpfs mount therefore escapes back to the real tree for free, no special-casing anywhere else.
  - **Bind mount** reuses the source directory's own real inode directly — no new inode allocated.
    Known, documented limitation: since it reuses the source's own real directory records verbatim,
    `cd ..` from inside a bind-mounted directory follows the *source's* real parent, not the new
    mountpoint's parent (real Linux gets this right via a per-mount dentry view this design doesn't
    build). Acceptable — no target applet needs it.
  - `st_dev` (`write_stat`) is `1` for the real, persisted filesystem and `2` for anything in the
    tmpfs pool — derivable from the inode number alone, and just enough for `mountpoint`'s real
    `st_dev(path) != st_dev(parent)` check to detect a tmpfs mount. A bind mount deliberately keeps
    `st_dev == 1` (same underlying superblock, matching real Linux's own same-filesystem
    bind-mount behavior), so `mountpoint` can't distinguish a bind-mounted directory from an
    ordinary one — a known, honest limitation, not fixable without a real per-mount identity this
    design doesn't build.
  - **The redirect only fires where `resolve_path_impl`'s own per-component loop actually runs.**
    That covers every *intermediate* path component unconditionally, and a *final* component too
    for any handler that resolves the whole path via `resolve_path`/`resolve_path_nofollow_last`
    directly (`oxfs_stat`/`oxfs_lstat`/`oxfs_chdir` all already do). It does **not** cover a handler
    that instead calls `resolve_parent` (which only walks the *parent* portion through
    `resolve_path`) and then does its own bare `dir_lookup(parent, leaf)` for the final component —
    correct for an EEXIST-style presence check (`oxfs_mkdir`/`oxfs_symlink`) or a raw-entry mutation
    (`oxfs_unlink`/`oxfs_rmdir`, which must act on the real, unmounted directory record), but wrong
    for `oxfs_open`'s "existing path" branch, which needs the *resolved* target. Found live via
    `tests/mount_syscall_smoke.rs`: `open("/mnttest/f")` worked (`"mnttest"` is an *intermediate*
    component there, redirected inside `resolve_parent`'s own internal `resolve_path` call), but
    `open("/mnttest")` alone (`"mnttest"` as the *leaf*) returned the real, shadowed directory
    instead of the tmpfs one — a real `getdents` on the mountpoint's own path silently showed the
    wrong content. Fixed with the same one-line `active_mount_for` redirect, applied to `oxfs_open`'s
    own `dir_lookup(parent, leaf)` result specifically (not a change to `dir_lookup` itself, which
    stays a true raw lookup for the mutation call sites that need it).
- **`SYS_MOUNT_BIND=174`/`SYS_MOUNT_TMPFS=175`/`SYS_UMOUNT2=176`** (`modules/oxfs`, registered
  alongside `SYS_UTIMENSAT=167`). Real `mount(2)` takes 5 conceptual args (`special, dir, fstype,
  flags, data`), which doesn't fit this ABI's 4 registers — rather than force one idealized shape,
  `third_party/musl/src/linux/mount.c`'s `mount()` is patched to dispatch to one of two syscalls
  based on its own real `fstype`/`flags` arguments, the only two shapes BusyBox's own
  `util-linux/mount.c` actually issues (`mount(source, target, NULL, MS_BIND, NULL)` for `--bind`,
  `mount("tmpfs", target, "tmpfs", flags, options)` for `-t tmpfs`); any other shape fails with
  `ENODEV` in musl itself, never reaching the kernel. **Deliberately not the next three numbers
  after `SYS_UTIMENSAT=167`** (168/169/170), despite every prior addition otherwise just continuing
  that sequence: those three real-Linux slots are `swapoff`/`reboot`/`sethostname`, all three
  already-seeded, already-built BusyBox applets in this port's roster that currently `ENOSYS`
  cleanly and honestly — claiming those numbers would have silently misrouted a real call from any
  of them into the mount table instead, caught during review rather than live, but the exact bug
  class this file's own musl-port section documents at length (`getdents`/`getdents64`, `ENOSYS`'s
  real value, ...). Landed on real Linux's `create_module`/`init_module`/`delete_module` (174-176)
  instead — real syscalls, but ones no code in this tree references at all (`insmod`/`rmmod`/
  `modprobe` never even became BusyBox build candidates in this port, see
  `docs/BUSYBOX_APPLETS.md`'s own "missing-from-this-list" note on `lsmod`), and all three are
  themselves long-obsolete on modern real Linux too. `umount()`/`umount2()` are patched the same
  way, reusing `delete_module`'s slot — this file's own, original `__NR_umount2` macro (166) stays
  at its inert real-Linux value, unreferenced from here on. Both syscalls' own `bits/syscall.h.in`
  comments avoid writing the literal `__NR_` prefix in prose (using `SYS_`-style naming instead) —
  the file's own sed-based `__NR_`-to-`SYS_` generator (the Makefile rule that builds
  `obj/include/bits/syscall.h`) matches any line *containing* that substring, including inside a
  comment, and duplicates it verbatim into the generated header outside any comment block; a prose
  mention of a real macro name broke the build this way while this passed landed, found immediately
  via `cargo build`, not live.
- **`/proc/mounts`** (`ProcSysFile::Mounts`, alongside `Meminfo`/`Uptime`/`Stat`/`Modules`): unlike
  those four, backed by no kernel FFI accessor at all — the mount table is oxfs's own state, so a
  local formatter produces standard mtab-shaped lines directly. Read by BusyBox's own bare `mount`
  (no arguments, listing current mounts) and by `mountpoint`/`umount`'s own real mtab-lookup
  conventions.
- Verified via `tests/mount_syscall_smoke.rs` + `userland/mount-syscall-smoke/` (a real spawned ELF
  through genuine `SYSCALL`/`SYSRETQ`, same reasoning every other real-`SYSCALL` smoke test in this
  codebase documents): a tmpfs mount's real create/write/read/`getdents`/`st_dev` round trip, a
  bind mount exposing a real BusyBox applet, `/proc/mounts` showing both while active, and both
  reverting cleanly on `umount` (the real, empty directories underneath reappear untouched, a
  repeat `umount` of the same path fails `EINVAL`, the bind-mounted applet is no longer reachable).
- **Not covered by this pass**: a real, block-device-agnostic mount table (`pivot_root`/
  `switch_root` need this — this design only ever redirects within oxfs's own single, already-
  mounted filesystem, never a second real device or on-disk format), and everything in
  `docs/BUSYBOX_APPLETS.md`'s `NEEDS_BLOCKDEV` row that needs a real partition table or multiple
  on-disk filesystem formats (`blkid`/`fdisk`/`fsck`/`mkswap`/...).

## Permission model (`src/process.rs`, `modules/oxfs/`, `modules/posix_compat/`)

Real uid/gid, real per-inode `mode`/`uid`/`gid`, real `chmod`/`chown`, and real `open()`
permission enforcement — closing this file's own long-standing "uid/passwd-db model" gap (see the
BusyBox gap analysis table below). Scoped deliberately: this kernel still has exactly one uid
that's ever existed (root, `0`) since there's no login mechanism, so every enforcement path this
adds is real, exercised logic that happens to always evaluate the same way *until* something
actually calls `setuid` to drop privilege — which the test coverage below does, specifically to
prove the enforcement isn't just plumbing.

- **`Process` gains `uid`/`gid` fields** (`src/process.rs`) — no separate saved/effective pair,
  since this kernel has no setuid-bit `execve` support to ever make real vs. effective diverge;
  `SYS_GETEUID`/`SYS_GETEGID` just echo the same fields `SYS_GETUID`/`SYS_GETGID` do. `0` (root) at
  `spawn` (pid 1 onward); copied by `fork` (real `fork()` semantics, same as `cwd`/`pgid`/`brk`);
  preserved by `execve` (which never touches these fields at all, same as `cwd`/`pgid`).
- **New syscalls**, continuing from `SYS_SYMLINK=155`: `SYS_GETUID=158`/`SYS_GETEUID=159`/
  `SYS_GETGID=160`/`SYS_GETEGID=161`/`SYS_SETUID=162`/`SYS_SETGID=163`/`SYS_GETGROUPS=164`
  (`modules/posix_compat` — process-attribute syscalls, same placement `setpgid`/`getpgid` already
  established) and `SYS_CHMOD=165`/`SYS_CHOWN=166` (`modules/oxfs` — filesystem-owned data, same
  placement `stat`/`fstat`/`lstat` already established). All seven `posix_compat` ones are real
  zero/one/two-plain-integer-argument Linux/generic wire formats — only the number needed
  remapping in musl. `chmod`/`chown` needed the same argument-convention patch `open`/`chdir`/
  `rename` already did: `third_party/musl/src/stat/chmod.c`/`src/unistd/chown.c` now compute
  `strlen(path)` explicitly and pass `(path_ptr, path_len, mode)`/`(path_ptr, path_len, uid, gid)`
  — `chown` is the second syscall (after `rename`/`symlink`) whose real argument count needed all
  four of this ABI's registers, using `R10` for `gid`.
- **`process::do_setuid`/`do_setgid`** enforce the real POSIX rule: a caller already running as
  root (`uid == 0`) may become any uid/gid; anything else may only "become" the uid/gid it already
  is — a real no-op success, not a privilege-escalation path, matching real `setuid()`'s own
  allowance for that specific case. Any other target is `EPERM` (`src/syscall.rs`'s new `EPERM=1`
  constant, identical on Linux/BSD/musl — no divergence to worry about, unlike most of this file's
  other errno constants).
- **`process::do_getgroups`** reports a single-element list (the caller's own `gid`) for both the
  real POSIX `size == 0` ("just the count") and `size >= 1` ("write the list") call shapes — this
  kernel has no supplementary-group concept at all, so the caller's primary group is the complete,
  correct answer.
- **`modules/oxfs`'s `Inode` gains real `mode`/`uid`/`gid` fields** (default `FIXED_PERM=0o755`/
  `0`/`0`, matching what every inode used to report unconditionally before this field existed, so a
  freshly seeded boot file behaves identically to the old hardcoded scheme until something actually
  calls `chmod`/`chown`). `write_stat`'s `st_mode`/`st_uid`/`st_gid` are now backed by these real
  fields instead of the old fixed placeholders. A freshly **created** file (`oxfs_open` with
  `O_CREAT`) is owned by its real creator, not always root — `OpenFile::Write` gained an
  `owner_uid` field, captured at `open()` time and applied to the new inode at `close()` (the point
  a real inode first gets allocated, same "commit on close" model this filesystem already used for
  content).
- **`check_access(inode, uid, gid, want_write)`** (`modules/oxfs`) — real Unix permission logic:
  `uid == 0` bypasses read/write bits entirely (root); otherwise picks the owner/group/other rwx
  triplet by comparing against the inode's own `uid`/`gid` (first match wins — being in the owning
  group doesn't fall through to "other" just because the group bits happen to deny it), then checks
  the one requested bit. Wired into `oxfs_open`: an existing file/dir needs the real bit its
  `O_WRONLY`/`O_RDWR`/`O_RDONLY` intent actually asks for (`want_write = flags & O_ACCMODE != 0`)
  — creating a new file needs write permission on the *parent directory*, not the (not-yet-
  existing) file itself. `do_execve`'s own ELF-loading read goes through this exact `oxfs_open`
  path (see "User-mode execution" above) always read-only, so it still doubles as an approximate
  execute-permission check (the read bit, not a separate execute check) — a known, documented
  simplification, harmless while every seeded file's default mode (`0o755`) sets both bits
  identically.
- **Real write-to-an-existing-file support** (`oxfs_open`/`oxfs_close`, found live via
  `modules/oxfs/src/test_busybox.sh`'s own applet self-test): `oxfs_open` used to always return a
  read-only fd for a path that already existed, regardless of `O_WRONLY`/`O_RDWR`/`O_APPEND`/
  `O_TRUNC` — meaning a file could only ever be written once, for its entire lifetime, from the
  moment it was created; a plain `echo x >> file` a second time (or even a second plain `echo x >
  file`) silently got a read-only fd and failed the write with `EBADF`. Confirmed live: BusyBox
  `hush`'s own real `WIFSIGNALED`/`WTERMSIG`-based job reporting made this externally visible for
  the first time (see the wait-status fix two entries below) once a script's *second* write to the
  same path started failing outright instead of just producing wrong content. `OpenFile::Write`
  gained an `existing_inode: Option<u32>` field: `None` is the original create-a-new-file path
  (allocate a fresh inode, insert a new directory entry at `close`); `Some(inode)` means `close`
  overwrites that *existing* inode's content in place via `write_inode_data` instead — same inode
  number, same directory entry, same owner/mode, just new content, avoiding either a duplicate-name
  directory entry or an orphaned original. `O_APPEND` preloads the write buffer with the file's
  real existing content (via `read_inode_at`) before any new `write()` calls land; plain
  `O_WRONLY`/`O_RDWR` (with or without an explicit `O_TRUNC`) starts the buffer empty — this
  filesystem's only write primitive (`write_inode_data`) always replaces a file's *complete*
  contents in one shot (matching `modules/fat32`'s own original simplification), so there's no way
  to support "open for writing but don't truncate until the first actual write" as a distinct case,
  and nothing in this port's roster needs that distinction. Opening a directory with
  `O_WRONLY`/`O_RDWR` is now a real `EISDIR` instead of silently succeeding as a directory listing.
- **`oxfs_chmod`**: only the inode's own owner or root may change its permission bits (`EPERM`
  otherwise); follows a final symlink (real `chmod(2)` semantics — there's no `lchmod` in POSIX at
  all). **`oxfs_chown`**: root-only unconditionally (this kernel has no group-membership concept to
  support real Unix's narrower "owner may change the group to one they belong to" case), real POSIX
  `(uid_t)-1`/`(gid_t)-1` "leave this field unchanged" convention (`u32::MAX` once truncated through
  this ABI's `u64` register) so a caller can change just one of the two fields; also follows a final
  symlink (`lchown` isn't implemented — no target applet in the current roster calls it).
- **`oxidebsd_current_uid`/`oxidebsd_current_gid`** (`src/process.rs`, exported to modules via
  `src/module.rs`'s `resolve_external_symbol`) — how `modules/oxfs` learns the calling process's
  identity without a direct cross-module call (modules can only call kernel-exported functions, see
  "Dynamic kernel modules" above). `pid == 0` (a module's own boot-time self-check, before any real
  process exists) reports root, the same identity that self-check's own `chmod`/`chown`/`open`
  calls need to succeed unconditionally — mirrors `oxidebsd_get_cwd`'s existing `BOOT_CWD` fallback
  shape.
- **`/etc/passwd`/`/etc/group`** — seeded by `modules/oxfs`'s own `module_init`, right alongside
  the existing `/etc/resolv.conf`: a single `root:x:0:0:root:/:/bin/sh` / `root:x:0:` entry, the
  complete and honest picture on a kernel with no login mechanism. No new syscall needed for
  `whoami`/`id`-style lookups at all — musl's own `getpwuid`/`getpwnam`/`getgrgid`/`getgrnam`
  (`third_party/musl/src/passwd/*.c`) parse these files directly via plain `fopen`/`fgets`, the
  same "port libc's real code, don't reimplement its logic kernel-side" philosophy this file's own
  musl-port section already documents for DNS resolution.
- Verified via `tests/uid_syscall_smoke.rs`/`userland/uid-syscall-smoke` (a real spawned pid 1
  driving all of it through genuine `SYSCALL`/`SYSRETQ`, same reasoning every other real-`SYSCALL`
  smoke test in this codebase already documents — this feature's correctness depends entirely on
  `Process::uid`/`gid` being resolved fresh via `scheduler::current_pid()` on every call, exactly
  the class of per-process state a plain-Rust-function test can't exercise): root identity +
  `getgroups`, a `chmod`/`chown`/`stat` round trip, then a `fork()`ed child that calls
  `setuid(1)` to actually drop privilege, confirms `setuid(0)` now fails `EPERM`, confirms
  `setuid(1)` (becoming itself) still succeeds as a real no-op, and then attempts to `open` a
  `0o600` file owned by a different uid — the one check in the whole test that exercises
  `check_access` actually *denying* something, not just the always-true root-bypass path every
  earlier step ran through. The parent's `wait4()` confirms the child's real exit code.
- **Not covered by this pass**: real login/session authentication (`su`/`login`/`sulogin`/`getty`
  — **done in a later pass, see "Session, controlling-tty, and login authentication" below**),
  mutating `/etc/passwd`/`/etc/group` (`adduser`/`passwd`/`chpasswd`/... — an applet-level gap now,
  not a kernel one, since both files are already real, writable oxfs files), `lchown`, `fchmod`/
  `fchown` (fd-based variants — no target applet in the current roster calls them; `chmod.c`/
  `chown.c` in BusyBox both call the path-based syscalls directly), and setuid/setgid/sticky mode
  bits (`oxfs_chmod` masks its input to `0o777`, silently dropping anything above that — nothing in
  this port's roster sets or checks them).

## Session, controlling-tty, and login authentication (`src/process.rs`, `src/stdin.rs`, `src/interrupts.rs`, `modules/posix_compat/`, `modules/oxfs/`)

Closes the `su`/`login`/`sulogin`/`getty` row the Permission model pass above left open. Split into
two genuinely separate pieces: `su`/`login` needed only a real second user + real password
verification (no kernel session concept at all); `sulogin`/`getty` additionally needed a real
session/controlling-tty/foreground-process-group model this kernel had never had any concept of.

- **A real second user.** `/etc/passwd` now has `user:x:1000:1000:User:/home/user:/bin/sh`
  alongside `root`, with a real `/home/user` directory (owned `1000:1000`, mode `0700`) — a
  root-only `/etc/passwd` could never exercise real `su`/`login` authentication at all, since both
  always skip the password check entirely when the caller is already root (see BusyBox's own
  `loginutils/su.c`: `ask_and_check_password` is only called `if (cur_uid != 0)`, and
  `loginutils/login.c`'s own `setsid()`/`ioctl(TIOCSCTTY)` calls are — independently —  already
  commented out in BusyBox's own upstream source, so `login` never needed the session work below at
  all). **A real `/etc/shadow`** (mode `0600`, root-owned — locked down immediately after seeding,
  since `seed_file` always creates at the default `FIXED_PERM=0o755`) holds real SHA-512 (`$6$`)
  `crypt(3)` hashes for both accounts (password equals the username — fine for a kernel with no
  external network exposure and no real multi-user threat model), generated with `openssl passwd
  -6` and verified against musl's own `crypt_sha512.c` — no code changes were needed there at all:
  `third_party/musl/src/crypt/*.c` is plain portable C with no SIMD/vector ops (unlike `sha2`/
  `chacha20` in `src/random.rs`, which needed real `force-soft`-style fixes for this target), so
  `crypt()`/`getspnam()` already worked the moment real accounts existed to exercise them —
  musl's own `getpwnam`/`getspnam`/`fopen`/`fgets` do all the real work, same "port libc's real
  code" philosophy as everywhere else in this port.

- **A real session model.** `Process` gains an `sid: Pid` field, the exact same shape as the
  already-existing `pgid: Pid` (an ordinary `Pid`, not a distinct namespace): a freshly `spawn`ed
  process becomes its own session leader (`sid == pid`, same as `pgid`); a forked child inherits
  the parent's live `sid` unchanged (real `fork()` semantics); `execve` leaves it untouched (same
  as `cwd`/`pgid`). Paired with two new globals in `src/stdin.rs` — `CONTROLLING_SESSION:
  Option<Pid>` and `FOREGROUND_PGID: Option<Pid>` — **deliberately single globals, not per-session
  tables**, the same simplification already accepted for `TERMIOS`: this kernel has exactly one
  real console, so "which session owns the controlling tty" and "which pgroup is in its
  foreground" only ever need one slot each, not a real multi-tty table.
  - **`SYS_SETSID`** is real x86_64 Linux's own `__NR_setsid` value (`112`, confirmed directly
    against `third_party/musl/arch/x86_64/bits/syscall.h.in`, not assumed from a generic/other-arch
    table — the exact class of mismatch this file's syscall-ABI section warns about elsewhere).
    `third_party/musl/src/unistd/setsid.c` is a bare no-argument `syscall(SYS_setsid)` — registering
    a handler at `112` was the complete fix, no musl-side patch needed at all, unlike almost
    everything else in this ABI. `process::do_setsid` enforces the real POSIX rule (`EPERM` if the
    caller is already a process-group leader) and, on success, makes the caller leader of a fresh
    session *and* a fresh process group at once (`sid = pgid = caller_pid`) — deliberately doesn't
    touch `CONTROLLING_SESSION` at all: since that global is keyed by session id, landing on a
    brand-new `sid` automatically means "not the current owner" with no explicit release step, and
    the *old* session (and any other processes still in it) keeps whatever tty claim it already
    had, matching real `setsid()`'s effect on the caller only.
  - **`SYS_GETSID = 177`** (an *invented* number — unlike `SETSID`, real x86_64 Linux's own
    `__NR_getsid` is `124`, which already means `SYS_IOCTL` in this ABI, so it needed the usual
    musl-side remap). Exists specifically for `getty`'s own real, documented fallback path
    (`third_party/busybox/loginutils/getty.c`): when `setsid()` fails — the common case for `getty`
    launched as a job-control shell's own foreground child, since `hush`'s `HUSH_JOB` support
    already puts a new foreground child in its own fresh pgroup *before* that child gets to call
    `setsid()` itself (real, documented BusyBox behavior, not a bug — see that file's own comment
    on why `getty 115200 /dev/tty2` typed directly at an interactive job-control shell is *expected*
    to fail this way on real Linux too, and needs either a non-interactive invocation or the
    `true | getty ...` trick to actually reach a session leader) — real `getty` calls `getsid(0)` to
    double check whether it's already the leader it needs before giving up loudly.
  - **`SYS_IOCTL` gains `TIOCSCTTY`/`TIOCNOTTY`/`TIOCGPGRP`/`TIOCSPGRP`** (`0x540E`/`0x5422`/
    `0x540F`/`0x5410`), gated the same way `TCGETS`/`TIOCGWINSZ` already are (only the real console
    fd ever answers). `TIOCSCTTY` requires the caller be a session leader (`sid == caller_pid`)
    unless `force` (the raw `argp` value itself, matching real `ioctl(fd, TIOCSCTTY, arg)`'s
    by-value argument convention for this one request) is set — this kernel has no permission model
    gating `force` itself (real Linux requires `CAP_SYS_ADMIN`), so any caller may force-steal the
    tty, the same "collapses to always-allowed on a kernel with no capability model" reasoning
    `do_setpgid`'s own doc comment already uses. `TIOCGPGRP`/`TIOCSPGRP` are gated on the caller's
    *session* currently owning the controlling tty (`ENOTTY` otherwise, matching real Linux);
    `TIOCGPGRP` falls back to reporting the session id itself when nothing has explicitly called
    `TIOCSPGRP` yet — the real Unix convention that a session's initial foreground group is its own
    leader's group, and (verified by tracing through `hush.c`'s own job-control startup sequence)
    exactly what lets a real `getty`→`login`→`hush` chain reach a working prompt on the first try
    with no spurious `SIGTTIN` retry loop, since `pgid`/`sid` stay equal end-to-end through that
    whole `execve` chain until `hush` itself first calls `setpgid`+`tcsetpgrp`.
  - **Real Ctrl+C → `SIGINT` to the foreground process group.** `interrupts::
    keyboard_interrupt_handler` now intercepts ASCII ETX (`0x03`) *before* pushing it to `stdin`'s
    ring buffer, but only when the console's own `ISIG` termios bit is set **and** a real
    `FOREGROUND_PGID` has actually been claimed — otherwise (today's common case: nothing besides a
    future `sulogin`/`getty` chain ever calls `TIOCSCTTY`/`TIOCSPGRP`) it falls through unchanged to
    the original behavior, the raw byte pushed to `stdin` for userland to handle itself exactly as
    before this pass (`stsh`'s own `read_line`, BusyBox `hush`'s line editor). When it does fire,
    `process::signal_foreground_group(pgid, SIGINT)` reuses the exact same Discard/Terminate/
    SetPending per-target logic `do_kill`'s own cross-process branch already established, just
    applied to every live process sharing that `pgid` instead of one.
  - **Not covered**: real `SIGTSTP`/`SIGTTIN`/`SIGTTOU`-driven job control (`^Z`, background/
    foreground job switching) — those signals' default disposition is still `Ignore` (see
    `default_disposition`'s own doc comment), a real, documented simplification predating this
    pass; only the `SIGINT` (`^C`) path was wired to the new foreground-group concept.
  - **Verified via `tests/session_syscall_smoke.rs` + `userland/session-syscall-smoke/`** — a real
    spawned ELF driven through genuine `SYSCALL`/`SYSRETQ` (same reasoning every other real-`SYSCALL`
    smoke test in this codebase documents), running as a *forked child* of pid 1 rather than pid 1
    itself (`process::spawn` already makes pid 1 its own process-group leader, so `setsid()` on pid
    1 directly would always `EPERM` before reaching anything interesting — a forked child inherits
    its parent's `pgid` unchanged, exactly the "not yet its own leader" shape `setsid()` requires,
    and the same shape a real job-control-shell-spawned child like `getty` is in): `TIOCSCTTY`
    correctly `EPERM`s before `setsid()` and succeeds after, `setsid()` itself succeeds once and
    then `EPERM`s on a second call, `getsid(0)` tracks the change, `TIOCGPGRP` is `ENOTTY` before
    `TIOCSCTTY` and reports the right value after (both the `sid` fallback and after an explicit
    `TIOCSPGRP`), and `TIOCNOTTY` releases the claim cleanly. **Not covered by this test**: real
    Ctrl+C→`SIGINT` delivery itself, since that's driven by a real PS/2 keyboard IRQ, not a
    syscall, and this kernel has no way to script keystrokes into a test session (same "hand off
    anything needing live interactive input" precedent the disk-persistence section already
    documents) — only manually verifiable at a real QEMU prompt.

**Two more real bugs found live testing `su` interactively at the `hush` prompt (not caught by any
automated test, since neither is syscall-shaped in a way the smoke test above exercises), both now
fixed:**

1. **A real, dangerous syscall-number collision, independent of and predating the session work
   above.** `su`'s own `initgroups()` → `setgroups()` call (`libbb/change_identity.c`, issued
   unconditionally right before privilege drop) failed with a bizarre `Protocol not supported`.
   Tracing it found `third_party/musl/arch/x86_64/bits/syscall.h.in` had left the real setgroups
   syscall number at its *inert real Linux value* — which this fork had **independently and
   separately** chosen as OxideBSD's own invented `SYS_KILL` number, in an unrelated, much earlier
   pass, with no reason at the time to cross-check it against every other still-inert real Linux
   number in the file. The result: a real `setgroups()` call didn't cleanly `ENOSYS` — it silently
   invoked the real `kill(2)` handler instead, reinterpreting `(count, gid_list_ptr)` as
   `(pid, sig)` (lucky here, since a raw pointer value almost never survives `do_kill`'s own
   `0..=31` signal-range check, but a real latent landmine, not just a bad error message — the same
   bug *class* this file's own musl-port section already documents at length for `getdents`/
   `getdents64`/`vfork`, just never actually instantiated for a real-Linux-numbered, not
   OxideBSD-invented, syscall before). Fixed by giving `setgroups` its own invented number instead
   (`178`, continuing past `SYS_GETSID = 177`) and adding a real, minimal `SYS_SETGROUPS` handler
   (`process::do_setgroups`, `modules/posix_compat`) — root-only, a genuine no-op (this kernel still
   has no supplementary-group concept to actually store a list in, same reasoning `do_getgroups`
   already documents), `EPERM` for anyone else (real `setgroups(2)`'s own unconditional
   `CAP_SYS_ADMIN`-equivalent requirement — unlike `setuid`/`setgid`, there's no "become what you
   already are" no-op allowance for a non-root caller, since a group *list* isn't a single value to
   trivially compare for equality). **Any future syscall number chosen for this ABI needs to be
   checked against every real-Linux-inert value still sitting in `bits/syscall.h.in`, not just
   against this ABI's own already-invented numbers** — this collision existed for an unknown number
   of prior passes without ever being exercised, since nothing before `su`/`login` called
   `setgroups()` at all.
2. **The already-documented `ENOSYS` mismatch (see this file's syscall-ABI section), now fixed —
   found concretely blocking real functionality, not just a review nitpick.** Real BusyBox
   `change_identity()` has an upstream fallback: if `initgroups()` fails with real `ENOSYS` *and*
   the target uid already equals the caller's own uid, it's treated as a harmless no-op (real
   Linux's own convention for a kernel built without multiuser support) — exactly `su`'s root→root
   case. That fallback never fired because this kernel's `ENOSYS` constant (`78`, FreeBSD's value,
   picked for authenticity like most of this file's other errno choices) didn't match musl's own
   compiled-in value (`38`) — so the raw errno musl's syscall wrapper actually produced never
   equaled the `ENOSYS` symbol musl's own C code compares it against, and `su` died outright instead
   of degrading gracefully. Fixed by correcting `ENOSYS` to musl's real value (`38`) — deliberately
   *not* bundled with a wider sweep of this file's other still-flagged, still-wrong errno constants
   (`udp.rs`'s four, `net/tcp.rs`'s several — see this file's syscall-ABI/real-networking sections),
   which remain a known, deliberately deferred gap: this fix was scoped to the one constant a live
   test just demonstrated was load-bearing, not a preemptive audit of everything adjacent.

Both found by testing `su` interactively at a real `hush` prompt after the session/`su`/`login`
work above landed — a concrete reminder that a clean `cargo test` run (which exercises no BusyBox
applet against real, interactively-typed input at all) doesn't substitute for actually running the
thing.

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

## Real-time clock (`modules/clock/`, `src/pit.rs`, `src/rtc.rs`, `src/syscall.rs`)

`SYS_CLOCK_GETTIME=138` — real `clock_gettime(2)`'s exact `(clockid, timespec_ptr)` wire format, so
only the number needed remapping in musl. `time()`/`gettimeofday()` (`third_party/musl/src/time/
time.c`/`gettimeofday.c`) are both plain C-level wrappers around `clock_gettime(CLOCK_REALTIME,
...)`, not separate syscalls, so this one remap unlocks all three.

- **`src/pit.rs`** reprograms the 8253/8254 PIT's channel 0 to a known, fixed `TIMER_HZ=100`
  (traditional old-Linux `HZ`) at boot — before this, `src/interrupts.rs`'s `TICKS` counter (used
  by `CLOCK_MONOTONIC`) incremented once per IRQ at whatever rate the BIOS happened to leave the
  chip at (its power-on default, ~18.2065 Hz, is why it already worked at all, but was never a rate
  this kernel actually configured or could rely on for real time arithmetic). 100 Hz, not a finer
  1000 Hz some modern kernels use, deliberately — every extra timer IRQ has real, measured overhead
  under QEMU's software TCG emulation (same class of concern as `src/memory.rs`'s own `invlpg`-cost
  note), and nothing here needs sub-10ms resolution yet.
- **`src/rtc.rs`** reads the CMOS/MC146818 RTC (ports `0x70`/`0x71`) fresh on every
  `CLOCK_REALTIME` request rather than caching a boot-time baseline — the chip is always there and
  cheap to read, so there's no drift/staleness tradeoff to make the way deriving wall-clock time
  from tick count would have. Known simplifications: doesn't wait out an in-progress RTC update
  before reading (rare, self-correcting, off-by-up-to-a-second failure mode — not worth the
  standard double-read-until-stable dance here) and assumes the 21st century (`2000 + CMOS year`,
  no century register read). `tv_nsec` is always `0` for `CLOCK_REALTIME` — no sub-second
  resolution offered.
- `CLOCK_MONOTONIC` converts `ticks()` against `TIMER_HZ` — seconds since boot, not wall-clock time
  (matches real `CLOCK_MONOTONIC` semantics: unspecified epoch, only meaningful as a delta between
  two readings). Any other `clockid` (`CLOCK_PROCESS_CPUTIME_ID`, ...) is `EINVAL` — no per-process/
  per-thread CPU time is tracked.
- **`SYS_NANOSLEEP=139`** — real `nanosleep(2)`'s exact `(req_ptr, rem_ptr)` wire format (musl's own
  `sleep()`/`usleep()`, `third_party/musl/src/unistd/sleep.c`/`usleep.c`, are both plain wrappers
  around `nanosleep()`, so this one remap unlocks both). `src/process.rs`'s `do_nanosleep` converts
  the requested duration to an absolute wake-up tick deadline against `TIMER_HZ`, blocks the caller
  (`ProcState::Blocked(BlockReason::Sleeping(deadline))`), and calls `scheduler::schedule()` — the
  same block-then-`schedule()` shape `WaitingForPipeData`/`WaitingForStdin` already established,
  just woken by `interrupts::timer_interrupt_handler` itself (scanning `process::table()` for any
  expired deadline on every tick) instead of another process's syscall. Safe for the same reason
  `crate::stdin::push_byte`'s own table-locking from the keyboard IRQ handler already is: every
  other place `process::table()` is locked runs either inside a `SYSCALL` (where `SFMASK` clears
  `IF` for its entire duration) or inside `scheduler::schedule()`'s own `without_interrupts` section
  — never somewhere this timer IRQ could actually preempt mid-hold. Rounds the requested duration
  *up* to a whole tick (never sleeps for less than asked); a `{0, 0}` request returns immediately
  without blocking at all. `rem_ptr` is always zeroed on return — no signal-delivery-during-sleep
  interruption path exists yet, so a sleep either runs to its full duration or (via `SIGKILL`'s
  immediate, no-handler termination path) never returns.
- Doesn't implement `clock_nanosleep(2)` itself for any case beyond what plain `nanosleep()`
  reduces to (`CLOCK_REALTIME`, no `TIMER_ABSTIME`) — no BusyBox applet in the roster calls it
  directly with a different clock or absolute-deadline flag.

## Real networking (`src/pci.rs`, `src/net/*`, `modules/net/`)

A real, phased networking stack: PCI enumeration, an IRQ-driven rtl8139 NIC driver, Ethernet/ARP/
IPv4/ICMP, UDP/TCP sockets, raw ICMP sockets (for `ping`), `poll(2)`, and — critically — no DNS
protocol code of its own at all: real hostname resolution works by making musl's own real stub
resolver (`third_party/musl/src/network/`) function correctly over this ABI, the same "port
libc's real code, don't reimplement its logic kernel-side" philosophy the rest of this musl port
already follows (`open`/`execve`/`stat`/...).

- **`src/pci.rs`**: legacy I/O-port config-space access (`0xCF8`/`0xCFC`), a flat scan of all 256
  buses (no bridge traversal — QEMU puts everything on bus 0). `find_by_class`/`find_by_id` are the
  generic lookups any driver can use.
- **`src/net/rtl8139.rs`**: brought up unconditionally at boot (`main.rs`, before any module
  loads), not gated on whether a NIC is actually present — absence is logged, not fatal.
  `src/interrupts.rs` gained a generic `IRQ_HANDLERS` dispatch table (IRQ2–15; `IRQ0`/`1` stay
  hardcoded for timer/keyboard) so a driver whose IRQ line isn't known until PCI probe time can
  still claim a vector without the static IDT changing shape (`register_irq_handler`/
  `src/pic.rs`'s `unmask_irq`).
- **`src/net/{ethernet,arp,ipv4,icmp}.rs`**: real frame/packet construction and checksums,
  no fragmentation, no IP options. `ipv4::next_hop` is the *only* routing rule that exists — not a
  real routing table: an address outside `GUEST_IP`'s own `/24` (i.e. any real internet
  destination) is sent to `GATEWAY_IP`'s MAC instead of `dest_ip`'s own, since QEMU SLIRP only
  answers ARP for its own virtual IPs (`GATEWAY_IP`=`10.0.2.2`, `DNS_SERVER_IP`=`10.0.2.3`) and
  never for an arbitrary off-link address. Without this rule nothing beyond SLIRP's own gateway/
  DNS-relay addresses could ever be reached at all.
- **`src/net/udp.rs`, `src/net/tcp.rs`**: real sockets behind `SYS_SOCKET=140`/`SYS_BIND=141`/
  `SYS_SENDTO=142`/`SYS_RECVFROM=143`/`SYS_SETSOCKOPT=144` (UDP) and `SYS_CONNECT=145`/
  `SYS_LISTEN=146`/`SYS_ACCEPT=147` (TCP; once `Established`, data flows over plain
  `SYS_READ`/`SYS_WRITE` instead). TCP is stop-and-wait (one segment in flight, fixed 536-byte MSS,
  no window/congestion control, no TIME_WAIT). `oxidebsd_sys_socket` masks off `SOCK_CLOEXEC`/
  `SOCK_NONBLOCK` (real Linux/musl OR these into `type` directly — musl's own DNS resolver does
  exactly this) before matching, rather than rejecting the call outright.
- **`src/net/icmp.rs`**'s raw sockets (`SOCK_RAW`+`IPPROTO_ICMP`, dispatched from
  `udp::oxidebsd_sys_socket`) exist specifically so a *real* `ping` binary can work, not just the
  kernel's own echo-request/reply logic (`tests/icmp_smoke.rs`'s target). Unlike UDP/TCP, a raw
  socket isn't port-addressed — every inbound ICMP packet fans out to *every* open raw socket
  (matching real Linux; the app filters by `icmp_id`/type itself, exactly what BusyBox's own
  `ping.c` does), and delivery includes the *real IP header* prepended (`ping.c`'s `unpack4` reads
  `iphdr->ihl`/`ttl` straight out of the receive buffer) — the one place this stack's delivery
  shape differs from UDP/TCP's payload-only convention.
- **`SYS_POLL=148`** (`src/net/mod.rs`'s `oxidebsd_sys_poll`): added purely to unblock musl's real
  DNS resolver, which multiplexes nameserver retries with a real `poll()`. Real Linux's own
  `__NR_poll` value (`7`) was left unmapped in musl on purpose — it collides with this ABI's own
  `SYS_WAIT4` (the same class of numeric collision this file's musl section already warns about
  for `getdents`/`getdents64`) — remapped to `148` instead. Reports `POLLIN` only (the one event
  class any fd here has real blocking semantics for); an fd not owned by udp/tcp/icmp (a regular
  oxfs file, a pipe, stdin) is always reported ready, matching real POSIX behavior for regular
  files and a reasonable stand-in for everything else.
- **Real DNS resolution**: `modules/oxfs`'s `module_init` seeds `/etc/resolv.conf` with
  `nameserver 10.0.2.3` (SLIRP's built-in DNS relay, forwarding to whatever the host itself uses).
  That, plus `SYS_POLL` and the `SOCK_CLOEXEC`/`SOCK_NONBLOCK` masking above, was *nearly* enough —
  the last gap was musl's `recvmsg`/`sendmsg` (`third_party/musl/src/network/`), which its
  resolver (`res_msend.c`) uses instead of `recvfrom`/`sendto` even though it only ever needs a
  single iovec and no ancillary data — exactly the shape the already-patched `recvfrom`/`sendto`
  handle. Neither was remapped or patched before, so every real reply the kernel correctly
  delivered got silently dropped (`ENOSYS`) and never consumed. Fixed by patching `recvmsg`/
  `sendmsg` themselves to delegate straight to `recvfrom`/`sendto` for that shape (multi-iovec/
  control-message callers get a clean error instead — nothing in this port's roster needs either).
  No new syscall numbers were needed for this part at all.

**Two real bugs found getting this far, both worth remembering (one general, one specific to any
future syscall-reachable busy-wait):**

1. **QEMU was never asked to accelerate.** Neither `test-args` nor `run-args` in `Cargo.toml` ever
   passed `-accel` — every boot silently ran pure-software TCG, whose per-instruction speed is
   entirely at the mercy of host CPU contention. Normally fast enough to go unnoticed (~4s boot),
   but under load this could stretch past a minute — indistinguishable from a genuine hang, and
   confirmed (via `git stash` bisection) to predate the networking work entirely, not caused by
   it. Fixed by adding `-accel kvm -accel tcg` (two repeated flags — this QEMU build rejects the
   single-flag `kvm:tcg` list syntax) to both arg lists; falls back to `tcg` cleanly on a host with
   no `/dev/kvm`.
2. **`hlt()` inside a syscall handler can freeze the CPU permanently.** `src/syscall.rs`'s SFMASK
   setup clears `RFLAGS::INTERRUPT_FLAG` for a syscall's *entire* duration, not just its entry (see
   this file's own Syscall ABI section). `hlt()` only wakes on an unmasked interrupt or an NMI —
   with `IF` cleared, an ordinary timer/NIC IRQ can't wake it at all, and *no* timer tick can fire
   to advance `ticks()` either, so a tick-bounded deadline check can never even be re-evaluated.
   Three real syscall-reachable busy-wait loops called `hlt()` between retries: `ipv4::
   resolve_with_retry` (ARP wait, reachable from any real `sendto()`), `tcp::oxidebsd_sys_connect`'s
   handshake wait, and `net::oxidebsd_sys_poll` itself. Each would freeze solid the instant the
   awaited condition wasn't already true *before* the loop started — timing-dependent, not
   deterministic, which is exactly why it looked so inconsistent from the outside. Fixed by
   replacing all three with `core::hint::spin_loop()` (packet arrival in the NIC's ring is a
   hardware DMA-like effect, not gated on this core's interrupt-enable state, so a real reply is
   still found the moment it lands — only the "give up after N ticks" bound goes uneven for a
   genuinely unreachable destination, spinning for the syscall's whole duration instead of timing
   out cleanly, a real remaining limitation, not something this fix addresses).
   **The entire automated test suite (`icmp_smoke`/`udp_smoke`/`tcp_smoke`/`ping_smoke`/
   `poll_smoke`) missed this completely** — every one of them calls the kernel-side handlers
   directly as plain Rust functions from a test's own `main()` (interrupts enabled throughout,
   never inside a real `SYSCALL`), so none of them ever exercised the code path where the bug
   actually lived. A real blind spot in how these tests are built, not just this one bug — any
   future test claiming to verify syscall-reachable code should spawn a real ELF and go through an
   actual `SYSCALL` instruction, the way `tests/fork_wait.rs` already does for `fork`/`wait4`.

**Known gaps, current as of this pass:**

- **`alarm()`/`setitimer()` — done.** `SYS_SETITIMER = 156`/`SYS_GETITIMER = 157` (`modules/
  clock/`), backed by `process::do_setitimer`/`do_getitimer` and a new per-process
  `real_timer_deadline`/`real_timer_interval_ticks` pair on `Process`, checked by
  `interrupts::timer_interrupt_handler` alongside its existing `BlockReason::Sleeping` scan. Only
  `ITIMER_REAL` is supported (`EINVAL` for `ITIMER_VIRTUAL`/`ITIMER_PROF` — this kernel tracks no
  per-process CPU time at all, see this file's real-time-clock section). musl's own `alarm()`
  (`third_party/musl/src/unistd/alarm.c`) is just a thin wrapper around `setitimer(ITIMER_REAL,
  ...)`, so this one syscall backs both real libc entry points — no separate `SYS_ALARM` needed,
  and `__NR_alarm` stays at its inert real-Linux value in the remapped `syscall.h.in`, unreferenced
  by any musl C code.

  Expiry only ever sets the target's `pending_signals` bit (the same simple pattern `do_kill`'s
  own *self*-targeting case already used), not the stronger immediate-termination path `do_kill`'s
  *cross-process* branch uses for a no-handler target — deliberately, since that path re-locks
  `process::table()` (or calls `terminate_process`, which does its own locking), which would
  deadlock against `spin::Mutex`'s non-reentrant guarantee from inside the timer IRQ handler's own
  already-held table lock. Sufficient for the concrete case this closes: `ping`'s own real usage
  pattern (`networking/ping.c`, both its simple and `FEATURE_FANCY_PING` builds) is a real blocking
  `recv()` around a tight `while(1)`, expecting `EINTR`-by-signal to break out — but this kernel's
  own `recvfrom` (`udp::oxidebsd_sys_recvfrom`, and raw-ICMP's `icmp::recvfrom`) is already
  non-blocking-with-self-poll (returns `0` immediately rather than actually blocking), so that
  loop is really a tight sequence of *individually completing* syscalls in userland, each one
  passing through `syscall::deliver_pending_signal`'s dispatch-tail check — pending delivery is
  picked up essentially immediately. A process genuinely blocked elsewhere
  (`BlockReason::Sleeping`/`WaitingForPipeData`/...) when its alarm fires won't see it promptly —
  the same accepted, documented gap `do_kill`'s own doc comment already calls out for a
  handler-installed cross-process signal.

  Not inherited by `fork` (`None`/`0` in a freshly forked child, matching real POSIX itimer/fork
  semantics — unlike `cwd`/`brk`/`fs_base`, which *are* copied); preserved across `execve` (real
  `execve()` doesn't reset a pending itimer, same as `cwd`/`pgid`).

  Verified via `tests/itimer_syscall_smoke.rs` + `userland/itimer-syscall-smoke/` — a real spawned
  ELF driven through genuine `SYSCALL`/`SYSRETQ` (this feature's own correctness depends entirely
  on the timer IRQ still firing across a syscall boundary, exactly the class of bug this section's
  own `hlt()`/`ticks()`-frozen-during-syscall bugs were only ever caught by real-`SYSCALL` tests,
  never a plain-Rust-function one): a `setitimer`/`getitimer` arm/read-back/disarm round trip, then
  a `fork()`ed child with no `SIGALRM` handler installed, spinning through individually
  non-blocking `SYS_GETPID` calls until the kernel's own default-disposition delivery actually
  terminates it — the parent's `wait4()` confirms via the real BSD/Linux wait-status convention
  (`status & 0x7f == SIGALRM`).
- **`socketpair(AF_UNIX, SOCK_STREAM, ...)` — `SYS_SOCKETPAIR = 149`, `modules/posix_compat/`.**
  Real `socketpair(2)`'s exact `(domain, type, protocol, sv_ptr)` shape already fits this ABI's
  4-register width whole (like `bind`/`connect`/`listen` before it), so only a plain `__NR_*`
  remap was needed on the musl side, no call-site patch. Not a real `AF_UNIX` abstraction (this
  kernel has no socket address-family concept beyond UDP/TCP/raw-ICMP's own `AF_INET`) — built
  from the exact same blocking `PipeBuffer` machinery `pipe(2)` already uses (`src/pipe.rs`), just
  two buffers cross-wired into a full-duplex pair, which is why it lives in `posix_compat`
  (pipe-shaped) rather than `modules/net/` (never touches the actual network stack). Verified via
  `tests/socketpair_smoke.rs`: both directions of the pair, plus real EOF/EPIPE on close.

  **A live retest surfaced three more missing syscalls, all now fixed in the same pass**: `wget`
  reached its HTTPS path but got no response at all (`fgets`-returning-NULL, no useful errno) --
  the TLS helper child was dying before ever writing a decrypted byte back through the socketpair.
  Tracing it (`src/network/wget.c`/`tls.c` in `third_party/busybox`) found:
  - **`SYS_SET_TID_ADDRESS = 150`** (`modules/native_abi/`) — called unconditionally by *every*
    musl program at startup (`__init_tls.c`) and after every real `fork()` (`_Fork.c`); previously
    unregistered, so every process's own `tid` silently held `-ENOSYS` the whole time. No real
    threading exists here, so `tid` and `pid` are the same concept — `sys_set_tid_address` just
    echoes `scheduler::current_pid()` back.
  - **`SYS_FCNTL = 151`** (`modules/posix_compat/`) — BusyBox's `libbb/xfuncs.c` (`ndelay_on`/
    `ndelay_off`, used by `wget`'s own progress-bar/timeout loop) calls real `fcntl(F_GETFL/
    F_SETFL)` to toggle `O_NONBLOCK`; previously unregistered, so the fd never actually became
    non-blocking (harmless for `wget` specifically, since `src/pipe.rs`'s already-blocking reads
    still eventually return real data, just without the progress-bar/timeout niceties — but a real
    correctness gap for anything that depends on real `EAGAIN`). Only `F_GETFL`/`F_SETFL`
    (`O_NONBLOCK` only)/`F_SETFD` (no-op, no close-on-exec enforcement exists)/`F_DUPFD`/
    `F_DUPFD_CLOEXEC` (delegate to `crate::fd::dup`) are implemented — everything else is
    `EINVAL`. `crate::pipe::blocking_read` is the *only* reader that honors `O_NONBLOCK`
    (`crate::fd::is_nonblocking`, tracked per-`real_fd` — the correct POSIX "open file description"
    scope) — a TCP/UDP socket's own read already returns promptly on "no data yet" by a different,
    pre-existing convention (`src/net/tcp.rs`'s `tcp_read`), so setting `O_NONBLOCK` there is
    accepted and tracked but doesn't change behavior, a known simplification.
  - **`SYS_SHUTDOWN = 152`** (`modules/posix_compat/`) — `wget.c` itself calls
    `shutdown(fileno(sfp), SHUT_WR)` right after sending the request, on exactly the kind of
    socketpair endpoint `SYS_SOCKETPAIR` provides. Real half-close semantics only for a
    `crate::pipe`-backed socketpair endpoint (`ENOTSOCK` for anything else, including a real TCP/
    UDP socket) — `SHUT_WR` marks that one direction's buffer closed (future writes on it fail,
    the peer still sees real EOF once drained) without tearing down the fd itself, so the *other*
    direction of the same pair keeps working. This is also what forced `close_direction`
    (`src/pipe.rs`) to stop `.expect()`-panicking on an already-removed buffer — a partial
    `shutdown()` followed later by the fd's real `close()` can legitimately hit the same buffer
    twice, once the peer has also gone away in between.

  All three verified via the same `tests/socketpair_smoke.rs`.

  **A live retest with all four of the above still got no response at all** (`fgets` returning
  `NULL`, a stale/misleading errno from something unrelated -- likely musl's resolver quietly
  finding no `/etc/hosts`, which this kernel doesn't seed at all; harmless to resolution itself,
  but `fgets` doesn't clear `errno` on a clean EOF, so whatever was last set gets printed). Tracing
  it further found the *real* cause: BusyBox's vendored TLS code
  (`networking/tls.c`'s `tls_get_random`) needs real random bytes from `/dev/urandom` for its
  ClientHello nonce and key generation --
  ```c
  void FAST_FUNC tls_get_random(void *buf, unsigned len)
  {
      if (len != open_read_close("/dev/urandom", buf, len))
          xfunc_die();
  }
  ```
  This kernel had no `/dev` at all, so that `open()` failed and `xfunc_die()` silently exited the
  TLS helper child before it ever wrote a single decrypted byte back through the socketpair --
  independent of, and hiding behind, the misleading stale-errno text above.

  **Fixed**: `modules/oxfs/` gained a second synthetic path prefix alongside `/proc` (`dev_open`,
  called from `oxfs_open` for any `/dev/...` path) covering `/dev/random`, `/dev/urandom`,
  `/dev/null`, `/dev/zero` -- `open()`+`read()`/`write()`+`close()` only, no directory-listing/
  `stat()` support (nothing in this port's roster needs it yet, a known gap the same tier `/proc`
  itself went through incrementally). `random`/`urandom` share one `OpenFile::DevRandom` variant --
  this kernel has no real entropy-pool concept, so there's no meaningful "blocks until enough
  entropy" distinction to make between the two, matching modern Linux's own post-5.6 stance.

  The actual bytes come from a new kernel-resident module, **`src/random.rs`**, deliberately built
  as *real* cryptography rather than a fast throwaway PRNG (the user's own call, given `/dev/
  random`'s traditional "must actually be secure" expectation): every call gathers several
  independent, jittery values (the CPU cycle counter via `RDTSC`, PIT tick count, RTC wall-clock, a
  monotonic call counter, a stack address, and real hardware `RDRAND` output when `CPUID` reports
  it available), hashes them all together with SHA-256 into a 32-byte key, then generates the
  requested output as a ChaCha20 keystream under that key -- the same "gather a pile of diverse
  state, hash it into a seed, drive a real algorithm with it" design Pokemon Black/White's own RNG
  made famous, just with a real stream cipher instead of that game's own LCG. No persistent DRBG
  state to reseed on a schedule -- each call derives an independent key from scratch, so a fixed,
  all-zero ChaCha20 nonce is safe (the algorithm's real requirement is "never reuse a
  `(key, nonce)` pair," and a key is never reused across two different calls).

  **Crypto primitives are vetted external crates (RustCrypto's `sha2`/`chacha20`), not hand-rolled**
  -- a deliberate exception to this codebase's usual "own the small stuff" bias (`src/pic.rs`
  instead of `pic8259`, ...): a subtle bug in a hand-written hash/cipher is far harder to catch
  than in a driver, the same reasoning `linked_list_allocator`/the `x86_64` crate already followed
  for different safety-critical logic. Both needed real build fixes to even compile for this
  target: `sha2` needs its own `force-soft` feature (this target disables SSE/MMX entirely, and
  without it `rustc`'s vector-op legalization can't lower the crate's default SIMD-capable
  compress function -- a real `rustc-LLVM ERROR: Do not know how to split the result of this
  operator!`, not a warning); `chacha20` has no equivalent Cargo feature, so `.cargo/config.toml`
  now passes `--cfg chacha20_backend="soft"` via `[target.x86_64-oxidebsd] rustflags` to force the
  same thing for its own runtime-`cpufeatures`-detected AVX2/SSE2 backend selection.

  Verified via `tests/random_smoke.rs` (the generator itself: no crash across buffer sizes
  including `0`, two consecutive calls produce different output, output isn't a degenerate
  single-byte pattern) -- `modules/oxfs`'s own `/dev` path routing can only really be exercised by
  a real process actually `open()`ing it, which needs a full module-loading boot, so that part
  isn't covered by an automated test yet.

  **A fourth live-retest round got past the handshake's own random-data needs, and found a real
  bug in `src/net/tcp.rs` itself**: `wget` failed with `got bad TLS record (len:0) while expecting
  handshake record`. `tcp_read` used to return `0` (real EOF, by POSIX contract) the instant
  `recv_buf` was momentarily empty, *regardless of whether the connection was still open* --
  indistinguishable from a real peer close to any caller. BusyBox's TLS code (`tls_xread_record`)
  correctly-by-its-own-logic treated that early `0` as "abrupt EOF, no TLS shutdown" and gave up,
  even though the real server's ServerHello simply hadn't arrived yet. Plain HTTP never triggered
  this in practice (by the time `wget`'s own body-reading loop gets there, there's usually already
  been enough round-trip latency for data to already be sitting in the buffer), but it was always
  a real, latent race -- the first real remote (not synthetic-peer) TCP exchange this stack had
  ever actually driven.

  **Fixed**: `tcp_read` now genuinely waits while the connection is still open and simply has
  nothing buffered *yet*, only returning `0` once the peer has actually signaled closure
  (`ConnState::CloseWait`/`FinWait2`/`Closed`, reached via a real FIN -- not just an empty buffer).
  **Deliberately does *not* reuse `crate::pipe`'s own `BlockReason`/`scheduler::schedule()`
  pattern** -- that would be a *worse* bug than the one it replaces: incoming-packet processing on
  this kernel is pull-based, driven entirely by whichever process happens to call `net::poll()`
  (the rtl8139 IRQ handler itself does no heap allocation and touches no protocol state, just sets
  a flag -- see `src/net/rtl8139.rs`'s own doc comment). Yielding to the scheduler here would mean
  nothing ever calls `poll()` again on this connection's behalf once the only process that cares
  about it stops running -- a real, permanent hang. Instead it spins (`core::hint::spin_loop()`),
  the same reasoning already established for `oxidebsd_sys_connect`'s own handshake wait and
  `ipv4::resolve_with_retry`'s ARP wait (see this section's own two-real-bugs entry on the
  `hlt()`-in-syscall freeze those two were fixed for) -- ordinary interrupts (timer, keyboard)
  still fire throughout, only a voluntary yield to another *schedulable* process doesn't happen.
  No timeout, unlike connection *establishment*: blocking indefinitely for more data on an
  already-open connection is correct, ordinary blocking-`read()` behavior, the same accepted
  "spins for the syscall's whole duration against a genuinely unresponsive peer" tradeoff already
  documented for that class of wait.

  This also needed real `O_NONBLOCK` support on TCP fds (`crate::fd::is_nonblocking`, real
  `EAGAIN` instead of blocking) so a caller that explicitly asks for non-blocking behavior still
  gets it -- and, while touching this, `tcp.rs`'s own local `EAGAIN` constant was corrected from
  `35` (a real FreeBSD value, but not what musl's own compiled header actually defines) to `11`,
  the same "must match musl's own macro value" fix already applied to `src/syscall.rs`'s and
  `src/net/udp.rs`'s own copies (see this file's syscall-ABI section's own errno note --
  `EISCONN`/`ENOTCONN`/`ECONNREFUSED`/`ETIMEDOUT`/`EOPNOTSUPP`/`EADDRINUSE`/`EHOSTUNREACH` in this
  same file have the identical latent mismatch, not yet audited/fixed).

  Verified via `tests/tcp_smoke.rs`: a real peer FIN now produces real EOF; an `O_NONBLOCK` fd with
  an empty-but-still-open connection returns real `EAGAIN` immediately rather than spinning
  forever (which would have hung the test itself, not just been slow, if the fix regressed).

  **A fifth live-retest round got a real HTTPS download actually flowing** — `wget` reached
  `saving to 'README'` and real response bytes came through (confirmed by the file's own real
  content afterward: `cat README` printed the actual `torvalds/linux` README text) — then failed
  mid-download with `[boot] unrecognized syscall number 19` / `wget: read error: No error
  information`. Syscall `19` is real Linux's `readv`. Tracing it found the read-side mirror of the
  `writev` gap already fixed for `printf`:
  ```c
  // third_party/musl/src/stdio/__stdio_read.c
  cnt = iov[0].iov_len ? syscall(SYS_readv, f->fd, iov, 2)
      : syscall(SYS_read, f->fd, iov[1].iov_base, iov[1].iov_len);
  ```
  Any buffered `fread()`/`fgets()` call — not specific to `wget`, or even to networking — goes
  through `readv`, not plain `read`, whenever the `FILE*` has real internal buffering. Nothing had
  exercised a buffered read against a real, slow-arriving data source until this exact download.

  **Fixed**: `SYS_READV = 153` (`modules/native_abi/`, right next to `SYS_WRITEV`) — a thin
  per-`iovec` loop over `sys_read`, matching real `readv`'s own short-read contract (a partially
  filled iovec ends the call there, it doesn't move on to the next iovec expecting more to somehow
  still be available). Verified via `tests/readv_smoke.rs`.

  **Confirmed live**: `wget https://raw.githubusercontent.com/torvalds/linux/master/README`
  completed a full HTTPS download end to end, real content verified via `cat README` afterward.
  Five real bugs found and fixed across five live-retest rounds to get there, not just the
  original `socketpair()` one — this was the harder, more valuable path than stopping at "the
  syscall exists": `SYS_SOCKETPAIR=149`/`SYS_SET_TID_ADDRESS=150`/`SYS_FCNTL=151`/
  `SYS_SHUTDOWN=152`/`SYS_READV=153`, a synthetic `/dev` backed by a real SHA-256/ChaCha20
  generator, and a real `tcp_read` correctness bug (false EOF instead of blocking) that had never
  been exercised before since every prior TCP test used a synthetic in-process peer, never a real
  remote one.
- **No real routing table** — `ipv4::next_hop`'s single default-gateway rule is the entire routing
  decision this stack makes. No multi-hop routes, no interface selection, no route metrics.
- **No IPv6 anywhere in the stack.**
- BusyBox's own built-in TLS client (`networking/tls.c`, once `socketpair()` unblocks it) does not
  validate certificate chains — a limitation of that vendored code itself (it logs a "note", not an
  error, and proceeds), not something a kernel-side fix could address.

**Real-`SYSCALL` counterparts to the network smoke tests, and a sixth real bug found closing that
gap.** Every network smoke test above (`icmp_smoke`, `udp_smoke`, `tcp_smoke`, `ping_smoke`,
`poll_smoke`, `socketpair_smoke`) calls kernel handlers (`oxidebsd_sys_socket`, `_sendto`, ...) as
plain Rust functions from its own `main()` — interrupts enabled throughout, never inside a real
`SYSCALL` with `RFLAGS::INTERRUPT_FLAG` actually masked. That's exactly the blind spot that let the
`hlt()`-in-syscall freeze (this section, above) ship undetected — only `tests/fork_wait.rs` (a real
spawned ELF) ever exercised the genuine ring-3 path. Added five real-`SYSCALL` counterparts —
`tests/{udp,poll,ping,socketpair,tcp}_syscall_smoke.rs`, each spawning a small freestanding ELF
(`userland/*-syscall-smoke/`) as pid 1 that drives the identical scenario through genuine
`SYSCALL`/`SYSRETQ`, reporting pass/fail via the same test-only `SYS_TEST_EXIT = 9999` convention
`tests/fork_wait.rs` established. `icmp_smoke.rs` has no real-`SYSCALL` counterpart: it calls
`icmp::send_echo_request`/`take_echo_reply` directly, with no syscall surface at all —
`ping_syscall_smoke` (via a real `SOCK_RAW`+`IPPROTO_ICMP` socket) is its actual counterpart.
Where a test needs to simulate an inbound packet or a synthetic peer's handshake (the old tests
already did this via `ethernet::handle_frame`/ARP-reply injection from their own linear `main()`),
that trigger now lives behind a **test-only** syscall the child calls at the scripted moment
(`SYS_TEST_INJECT_UDP_FRAME = 9998`; TCP's own multi-step script, needing `tcp::debug_send_next`/
`debug_connection_for` to build valid segments, behind a step-dispatched `SYS_TEST_TCP_STEP =
9997`) — the functions actually under test still only ever run via the child's own real `SYSCALL`.

This conversion immediately caught a real, previously-invisible bug: `tests/poll_syscall_smoke.rs`
hung solid on a `poll()` call that should have timed out after 200ms. `net::oxidebsd_sys_poll`'s
deadline (and, identically, `ipv4::resolve_with_retry`'s ARP wait and `tcp::
oxidebsd_sys_connect`'s handshake wait — all three already converted from `hlt()` to
`spin_loop()` for the freeze bug above) checked `crate::interrupts::ticks()`, which only advances
via the timer IRQ — an IRQ that cannot fire for a syscall's *entire* duration once `SFMASK` has
masked it. Called as a plain Rust function (every prior test's style), `ticks()` advances
normally; called via a genuine `SYSCALL`, it's frozen at whatever value it had when the syscall
began, so a tick-based deadline inside one can never elapse — not "goes uneven," as
`resolve_with_retry`'s own comment used to say, but a permanent hang against any target that never
answers. Fixed by **`src/tsc.rs`**: a `RDTSC`-based (plain CPU cycle counter, immune to
`RFLAGS::INTERRUPT_FLAG`, same "no CPUID gate needed" property `src/random.rs` already uses)
deadline, calibrated once at boot (`crate::init`, right after interrupts are enabled) against
`ticks()`'s own known `TIMER_HZ` rate. All three call sites now use `crate::tsc::now()`/
`ms_to_cycles()` instead of `ticks()` for their own spin-deadlines. Confirmed fixed: the same test
now passes; the full existing suite re-verified green afterward (this bug was latent in
`resolve_with_retry`/`connect` too, just never triggered there since every prior test's ARP/
handshake happened to succeed before hitting its own deadline).

## Filesystem/process misc syscalls: fsync, ftruncate, fallocate, flock, statfs, prlimit64, nice, chrt, reboot (`modules/oxfs`, `modules/posix_compat`, `src/reboot.rs`)

Closes most of the BusyBox gap table's `NEEDS_SYSCALL` row in one pass: `fsync`/`sync`/`truncate`/
`fallocate`/`flock` (`df`'s own `statvfs()`)/`nice`/`chrt`/`halt`/`poweroff`/`reboot`. Deliberately
scoped to syscalls needing no new on-disk format or data model beyond a couple of small fixed-size
tables — `link`/`mknod`/SysV IPC/`chroot`/namespaces/`inotify`/ext2 `ioctl`s/`xattr` remain a
distinct, unstarted gap (namespaces in particular don't fit this kernel's single-address-space
model at all; faking them would be theater, not a real syscall).

- **All sixteen new syscall numbers landed at `471`-`486`, not a continuation of this ABI's
  existing `100`-`178` invented sequence.** A first attempt continuing that sequence (`179`-`194`)
  silently collided with a *second* set of real, still-inert Linux syscalls sharing those same low
  numbers further down `third_party/musl/arch/x86_64/bits/syscall.h.in` (real `__NR_quotactl=179`,
  `__NR_gettid=186`, `__NR_setxattr=188`, ...) — and `__NR_gettid` in particular has a real caller
  inside musl itself (`src/thread/synccall.c`), a live landmine, not a hypothetical one, caught
  only by grepping musl's own source tree before trusting the number, the same collision class
  this file's own `SYS_SETGROUPS`/musl-port notes already document at length. Since this vendored
  musl fork is frozen at tag `v1.2.6` and won't be re-pinned casually, `471`+ (right past this
  file's own real, highest-numbered entry, `__NR_listns=470`) is *permanently* collision-free, not
  just collision-free today — `modules/oxfs`'s own `SYS_FSYNC=471` through `SYS_FSTATFS=477`, then
  `modules/posix_compat`'s `SYS_PRLIMIT64=478` through `SYS_REBOOT=486`. **Any future syscall
  number chosen for this ABI needs the same check** (grep every real, still-inert value in
  `bits/syscall.h.in` for a live musl caller before reusing it, not just check against this ABI's
  own already-invented numbers) — this file's own prose comments explaining these 16 numbers
  deliberately never write the literal `__NR_` prefix (using bare names instead), matching this
  file's own documented sed-based `__NR_`-to-`SYS_` generator gotcha (see its comment on
  `SYS_MOUNT_BIND`/`SYS_UMOUNT2`): that generator matches any *line* containing `__NR_`, including
  inside a comment, and duplicates it verbatim outside any comment block in the generated header —
  broke the build this way while this pass landed, found immediately via `cargo build`, not live.
- **`SYS_FSYNC`/`SYS_SYNC`** (`modules/oxfs`) are real, not stubs: this filesystem's write model
  normally only commits a file's accumulated write buffer to its real inode at `close()` (see
  `OpenFile::Write`'s own doc comment), so a naive always-succeed `fsync()` would be a lie for
  anything still open. `commit_write_buffer` (refactored out of `oxfs_close`'s own body, which now
  just calls it) is the shared real logic — `oxfs_fsync` calls it for one still-open fd,
  `oxfs_sync` sweeps every currently-open write fd. Idempotent: a fresh file's first commit
  allocates the real inode and inserts its directory entry, then records that inode back into the
  open file's own `existing_inode` field so a later `fsync`/`close` on the same fd re-commits in
  place instead of double-inserting.
- **`SYS_FTRUNCATE`/`SYS_FALLOCATE`** (`modules/oxfs`) resize a file directly at the block level
  (`resize_inode_data`), not by materializing the whole old-or-new content into one buffer the way
  this filesystem's only other write primitive (`write_inode_data`) does — this filesystem's
  per-file cap is ~4 MiB, far past what this kernel's 128 KiB kernel-stack floor could hold as a
  local buffer inside a module's own syscall handler. Growing zero-fills only the newly-added
  region; shrinking touches no block content at all (bytes past the new `size` are simply
  unreachable, matching `read_inode_at`'s own size-bounded reads). Both also resolve a fd that's
  still `OpenFile::Write`-in-progress but already refers to a real, pre-existing inode (`O_WRONLY`
  on an existing path, BusyBox `truncate`'s own common case) — `inode_of_open_file` alone reports
  `None` for *every* `Write` variant, even that one, so `inode_for_resize` falls through to check
  `existing_inode` directly first. `fallocate`'s `mode` argument is ignored (always the default
  zero-extend-if-shorter behavior, never shrinks) — no `FALLOC_FL_KEEP_SIZE`/`FALLOC_FL_PUNCH_HOLE`
  support, a known simplification no applet in this port's roster needs past.
- **`SYS_FLOCK`** (`modules/oxfs`) is a real per-inode `LOCK_SH`/`LOCK_EX`/`LOCK_UN` advisory-lock
  table (`FLOCKS`, a fixed 16-entry `[Option<(inode, holder_real_fd, exclusive)>; 16]`), released
  on `close()` (real `flock()` semantics). **A conflicting request fails `EAGAIN` immediately even
  without `LOCK_NB`**, rather than genuinely blocking — this module has no scheduler-yield
  primitive reachable from a syscall handler (only kernel-resident code like `src/pipe.rs` can call
  `scheduler::schedule()`), and a real spin-wait here would permanently deadlock this single-core,
  non-preemptive kernel against a lock holder that could never run to release it. A documented,
  deliberate simplification, not an oversight.
- **`SYS_STATFS`/`SYS_FSTATFS`** (`modules/oxfs`) report a real musl-layout `struct statfs`
  (`MuslStatfs`, 120 bytes, `#[repr(C)]` + `write_unaligned`, same idiom `MuslStat` already uses)
  built from this filesystem's own live block/inode-usage counts, scanned fresh on every call
  (separately for the real vs. tmpfs pool, by the same `inode_num >= MAX_INODES` split `st_dev`
  already uses) — no cached running total to go stale. Backs `df`'s real `statvfs(3)` call
  (`third_party/musl/src/stat/statvfs.c`'s `__statfs`/`__fstatfs` needed no call-site patch at all,
  just the number remap — no separate `statfs64` syscall exists on a 64-bit arch).
- **`SYS_PRLIMIT64`** (`modules/posix_compat`) backs both real `getrlimit(2)`/`setrlimit(2)` —
  musl's own wrapper for both tries `prlimit64` first unconditionally, and only falls back to the
  legacy `getrlimit`/`setrlimit` syscalls if that returns `ENOSYS`, which it no longer does (see
  `third_party/musl/src/misc/getrlimit.c`/`setrlimit.c`). `Process::rlimits: [(u64, u64); 16]`
  (`RLIM_INFINITY = u64::MAX` default) is real per-process state, copied by `fork`/preserved by
  `execve` (same tier as `uid`/`gid`/`pgid`) — but **stored, never enforced**, an honest, documented
  gap the same tier as several other accepted-but-unenforced fields already in this codebase (e.g.
  `O_NONBLOCK` on a TCP socket). Real Linux's own `__NR_getrlimit=97`/`__NR_setrlimit=160` stay at
  their original inert values in `bits/syscall.h.in` — `__NR_setrlimit`'s real value would collide
  with this ABI's own `SYS_GETGID=160` if that fallback path were ever actually reached, a real
  landmine avoided (not triggered) by `prlimit64` succeeding first.
- **`SYS_SETPRIORITY`/`SYS_GETPRIORITY`** (`modules/posix_compat`, `nice`) store a real
  `Process::nice: i32` (default `0`), copied by `fork`/preserved by `execve`. `getpriority`'s real
  return-value convention is `20 - nice` (never negative, since the raw syscall ABI can't otherwise
  distinguish a real negative nice value from an error) — musl's own `getpriority()` wrapper
  un-shifts this client-side, so the raw kernel-side value must already be shifted the same way.
  No real scheduling effect at all — this kernel's cooperative round-robin scheduler has no
  priority concept to hook it into.
- **`SYS_SCHED_SETSCHEDULER`/`SYS_SCHED_GETSCHEDULER`/`SYS_SCHED_GETPARAM`/
  `SYS_SCHED_GET_PRIORITY_MAX`/`SYS_SCHED_GET_PRIORITY_MIN`** (`modules/posix_compat`, `chrt`) —
  BusyBox `chrt.c` calls all three of the first group directly via a raw `syscall()`, bypassing
  musl's own library wrappers entirely (`sched_setscheduler()`/`sched_getscheduler()`/
  `sched_getparam()` are all three permanently stubbed to return `ENOSYS` in this musl fork, per
  upstream's own real-Linux-noncompliance stance — see `third_party/musl/src/sched/
  sched_{setscheduler,getscheduler,getparam}.c`), so only the number needed remapping, no
  call-site patch. `Process::sched_policy`/`sched_priority: i32` (defaults `SCHED_RR`/`0`) are
  stored and echoed back honestly — same "no real effect" tier as `nice` above.
  `sched_get_priority_max`/`_min` are pure functions of `policy` alone (no `Process` state): a
  fixed, real-Linux-matching range (`SCHED_FIFO`/`SCHED_RR` → `1..=99`, everything else → `0..=0`),
  not backed by any real scheduling class this kernel implements.
- **`SYS_REBOOT`** (`modules/posix_compat`, plus `src/reboot.rs`'s new `poweroff`/`halt`) matches
  real Linux's own magic `RB_AUTOBOOT`/`RB_HALT_SYSTEM`/`RB_POWER_OFF` values (`reboot.c`'s musl
  wrapper passes these through unmodified — no call-site patch needed, this ABI's 4-register width
  already holds all three real arguments whole). `RB_AUTOBOOT` reuses the existing 8042-pulse
  reset (previously only called from fatal exception handlers); `RB_HALT_SYSTEM` is a plain
  permanent `hlt_loop()`; `RB_POWER_OFF` writes QEMU's own ACPI PM shutdown port (`0x604`, value
  `0x2000` — the standard "system_powerdown" trick for QEMU's default `i440fx`/PIIX4 machine, the
  same machine this file's own "Real disk persistence" section already assumes fixed ATA ports
  for), falling back to a plain halt if nothing acts on it. No permission check — this kernel has
  no capability model to gate real Linux's own `CAP_SYS_BOOT` requirement against, the same
  "collapses to always-allowed" reasoning `do_setpgid`/`TIOCSCTTY`'s own `force` flag already use.
  **Not covered by automated testing** (`tests/needs_syscall_smoke.rs` covers every other syscall
  in this section): every success path halts, resets, or powers off the whole VM, which
  `isa-debug-exit` can't distinguish from a hang — manual-QEMU-only, same "hand off anything that
  can't be scripted this way" precedent this file already follows for real interactive-keyboard
  cases.
- **`SYS_UMASK = 487`** (`modules/posix_compat`) — added in a later pass, not part of the original
  471-486 batch (continues one past `SYS_REBOOT=486` rather than reusing a gap inside it, same
  collision-avoidance discipline as the rest of this batch). Found live, not by design: testing the
  real-control-flow `hush` fix (see this file's BusyBox section) by running `chmod +x` on a real
  script hit `[boot] unrecognized syscall number 95` — BusyBox's `libbb/parse_mode.c` calls
  `umask(0)`/`umask(old_mask)` unconditionally whenever it parses a symbolic mode change (`+x`,
  `-w`, `u+rwx`, ...), to compute the result relative to the ambient umask, and real POSIX
  `umask()` can't fail, so musl's own wrapper (`third_party/musl/src/stat/umask.c`) returns
  whatever the raw syscall gives back with no error check at all — meaning every symbolic chmod
  across the *entire* BusyBox roster (not just `chmod` itself; `mkdir`/`install`/`cp` share the
  same `parse_mode.c`) was silently computing its result against a garbage `ENOSYS`-derived value.
  `Process::umask: u32` (default `0o022`, the standard real Unix default) is real per-process
  state — real `umask()` semantics require it, since the syscall always succeeds and returns the
  *previous* mask — copied by `fork`/preserved by `execve`, same tier as `rlimits`/`nice`/
  `sched_policy` otherwise: stored and returned honestly, never actually consulted anywhere oxfs
  creates a new inode. Confirmed live: `chmod +x`/full applet self-test (see this file's BusyBox
  section) reported all-pass after this landed.
- Verified via `tests/needs_syscall_smoke.rs` + `userland/needs-syscall-smoke/` (a real spawned
  pid 1 driving all of it — except `reboot`/`umask`, see above — through genuine `SYSCALL`/
  `SYSRETQ`, the same reasoning every other real-`SYSCALL` smoke test in this codebase already
  documents): a still-open fd's `fsync`'d content visible to a concurrent independent reader before
  the writer closes, `sync`, real `flock` exclusion between two independent opens plus
  release-on-close, a real `ftruncate` shrink and `fallocate` zero-extend round-tripped through a
  fresh reopen, `statfs`/`fstatfs` agreeing on the same live filesystem, a `prlimit64`
  read-old/write-new/read-back round trip against its `RLIM_INFINITY` default,
  `setpriority`/`getpriority`'s real `20 - nice` convention, and a
  `sched_setscheduler`/`sched_getscheduler`/`sched_getparam`/`sched_get_priority_max`/`_min` round
  trip. `umask` itself isn't in that automated test (added in a later pass, see its own bullet
  above) — confirmed live instead, the same "hand off anything only proven by real interactive
  BusyBox usage" precedent as `reboot`.

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
| Socket syscalls (`socket`/`bind`/`connect`/`sendto`/`recvfrom`/`poll`/...) + real DNS | done | most of 38 (`NEEDS_NETWORK`) | see this file's own "Real networking" section for the full architecture and the two real bugs (QEMU acceleration, `hlt()`-in-syscall freeze) found getting it working. `ping`/`nslookup`/`wget` (plain HTTP) confirmed working live, including real hostname resolution over musl's own DNS stub resolver; `docs/BUSYBOX_APPLETS.md`'s `NEEDS_NETWORK` list is now stale for those three specifically, not yet regenerated wholesale |
| `socketpair(AF_UNIX, SOCK_STREAM, ...)` + `fcntl`/`shutdown`/`set_tid_address`/`readv` + `/dev/{u}random`/`null`/`zero` + a real `tcp_read` EOF-vs-empty fix | **done, confirmed live** — `wget` HTTPS completed a real end-to-end download | `wget` HTTPS specifically (plain HTTP already worked) | see "Real networking" section's own known-gaps entry — `SYS_SOCKETPAIR=149`/`SYS_SET_TID_ADDRESS=150`/`SYS_FCNTL=151`/`SYS_SHUTDOWN=152`/`SYS_READV=153` in `posix_compat`/`native_abi`, built on `src/pipe.rs`'s existing blocking-buffer machinery; a synthetic `/dev` in `modules/oxfs` backed by a real SHA-256/ChaCha20 generator (`src/random.rs`); and a real bug in `src/net/tcp.rs`'s own `tcp_read` (returned false EOF instead of blocking) — five real bugs, each surfaced only once the previous fix got live-retested |
| `alarm`/`setitimer` | done | `ping`'s own real receive-loop timeout (chief motivator); any other program relying on a real `SIGALRM` to bound an otherwise-indefinite wait | `SYS_SETITIMER=156`/`SYS_GETITIMER=157` in `modules/clock` — see "Real networking" section's own known-gaps entry for the full design (why only `ITIMER_REAL`, why expiry only sets `pending_signals` rather than immediately terminating a blocked target, fork/execve semantics) and `tests/itimer_syscall_smoke.rs`'s own real-`SYSCALL` coverage |
| `chmod`/`chown`/`chgrp` | done | — | `SYS_CHMOD=165`/`SYS_CHOWN=166` in `modules/oxfs` — see this file's own "Filesystem: oxfs" section. BusyBox's own `chown.c` implements `chgrp` as the same `chown()` call restricted to the group field, so one pair of syscalls unblocks all three; `chattr`/`fatattr`/`lsattr`/`setfattr` are a distinct, still-unstarted gap (real ext2 `ioctl`s / `setxattr`, not chmod/chown, despite `docs/BUSYBOX_APPLETS.md`'s original probe bucketing them together — corrected there this pass) |
| `fsync`/`sync`/`ftruncate`/`fallocate`/`flock`/`statfs`/`setrlimit`/sched-priority/`reboot` | done | subset of 33 (down to 22, `NEEDS_SYSCALL`) | see this file's own "Filesystem/process misc syscalls" section — `SYS_FSYNC=471` through `SYS_REBOOT=486` in `modules/oxfs`/`modules/posix_compat`. Unblocks `fsync`/`flock`/`fallocate`/`truncate`/`chrt`/`halt`/`nice`/`poweroff`/`sync`/`softlimit`/`df` |
| A specific missing syscall per remaining applet (`link`/`mknod`/SysV IPC/`chroot`/namespaces/`getrusage`/`inotify`/ext2 `ioctl`s/`xattr`) | not started | 22 (`NEEDS_SYSCALL`) | see `docs/BUSYBOX_APPLETS.md`'s own breakdown for which applet needs which — no single fix, a checklist of small ones |
| `/proc` filesystem — per-process (`stat`/`cmdline`/`status`, dir listing, `stat(2)`/`lstat(2)`) | done | — | special-cased path prefix inside `modules/oxfs` (no VFS layer exists to plug a separate procfs module into, and oxfs already owns `SYS_OPEN`/`SYS_GETDENTS`/`SYS_STAT`; see `oxfs`'s own `proc_open`/`proc_kind`), synthesizing content from new kernel-exported accessors (`src/process.rs`'s `oxidebsd_proc_exists`/`_pid_at`/`_stat_line`/`_cmdline`/`_status`) — no real inode/blocks (`write_proc_stat` fakes `st_mode` only; every other field, including `st_size`, is a fixed placeholder). Includes a minimal `/proc/<pid>/task/<tid>/` redirect (`tid == pid` only, this kernel has no real threading) since `pstree` unconditionally `opendir()`s it *and* `stat()`s it for uid/gid, silently skipping a pid entirely if either fails, rather than falling back to the plain per-pid files — confirmed live: without `stat()` support, `pstree` produced zero output, not a degraded one. Unlocks `pidof`/`pgrep`/`pkill`/`pstree`/`minips` |
| `/proc` filesystem — system-wide files (`/proc/meminfo`/`uptime`/`stat`) + `chdir(2)` into `/proc` | done | — | three new system-wide (not per-pid) kernel accessors (`oxidebsd_proc_meminfo`/`_uptime`/`_stat_global` in `src/process.rs`), routed as siblings of the numeric pid entries at `/proc`'s own top level. `MemFree`/`MemAvailable` are set equal to `MemTotal` (no free-memory/dealloc tracking exists anywhere in this kernel); `/proc/stat`'s `cpu` line is all-zero except `idle` (`ticks()` itself — no per-tick user/system accounting exists), so any CPU%-computing tool (`top`) reads permanently ~0% used, an honest placeholder, not a bug. `chdir(2)` into `/proc` needed a real sentinel encoding for `Process::cwd` (still a fully kernel-opaque `u64`, zero kernel-side changes) — the top bit tags it as a synthetic `/proc` location (`CWD_PROC_TAG`) rather than a real inode number, reusing `ProcDirKind` itself as the "which synthetic directory" representation; every path-taking oxfs syscall handler (`open`/`chdir`/`getcwd`/`stat`/`lstat`/`readlink`, plus a hard `-EROFS` reject for `mkdir`/`unlink`/`rmdir`/`rename` attempted relative to a synthetic cwd) got a matching branch. **Load-bearing fix along the way**: `current_cwd()`/`set_current_cwd()` used to truncate the kernel's own `u64` cwd value to `u32` immediately — harmless while `cwd` only ever held a small real inode index, but would have silently discarded this new tag; both were replaced by a `Cwd` enum-returning pair. Confirmed via `modules/oxfs`'s own boot self-check (system files, chdir in/out of `/proc`, the `EROFS` guard) and `tests/proc_smoke.rs`/`userland/proc-smoke` (a real spawned pid 1 driving all of it through genuine `SYSCALL`/`SYSRETQ`, since the boot self-check itself runs as pid 0 before any real process exists and can't exercise `/proc/<pid>/...` navigation). BusyBox's own `procps/top.c` source confirms `top` is the concrete applet this combination targets (`chdir("/proc")` once at startup, then relative `open("stat")`/`open("meminfo")`) — not yet confirmed live end to end. `free`/`uptime` turn out to call the Linux-only `sysinfo(2)` for their *primary* numbers (confirmed via BusyBox's own `procps/{free,uptime}.c` source) — these two new files don't unblock them; `sysinfo(2)` itself remains a distinct, unimplemented gap |
| `/proc` filesystem — per-fd (`/proc/<pid>/fd/`) | done (enumeration only) | subset of the above | `src/fd.rs`'s new `oxidebsd_fd_at(pid, index)` (mirrors `oxidebsd_proc_pid_at`'s own "loop until -1" shape, a bounded range scan over the `(pid, fd)`-keyed fd table) backs a new `ProcDirKind::FdList`. Real directory listing, real fd numbers — but each entry is a plain placeholder (`DT_REG`, no real content), **not** a real symlink to its target, since real Linux's own `/proc/<pid>/fd/N` entries are symlinks and this kernel had zero symlink support at all until this same pass added one (see the `SYS_SYMLINK`/`SYS_READLINK` row below). Making these entries real target-bearing symlinks needs a separate, cross-module "describe this fd" mechanism (oxfs doesn't know what a pipe/socket fd actually is; only `src/pipe.rs`/`src/net/*` do) — a known, deliberate limitation, not solved by guessing. Unlocks the enumeration half of `lsof`/`fuser`, not the full readlink-target behavior |
| Real symlinks (`SYS_SYMLINK`/`SYS_READLINK`) | done | `ln -s`/`readlink` in `docs/BUSYBOX_APPLETS.md`'s `NEEDS_SYSCALL` row | added alongside the `/proc` work above once it became clear `lsof`/`fuser`'s own real need was `readlink(2)`, not just directory enumeration — steered by an explicit user call to prioritize real POSIX primitives over narrowly chasing one BusyBox applet's own behavior. New `InodeKind::Symlink` in `modules/oxfs` (target string stored exactly like a regular file's content — no new storage mechanism); `resolve_path`/`resolve_path_nofollow_last` (a shared `resolve_path_impl(cwd, path, follow_last, depth)`) now transparently follow a symlink for every intermediate path component always, and for the final component only when `follow_last` is set — the one real difference between `stat(2)`/`open(2)` (follow) and `lstat(2)`/`readlink(2)` (don't), bounded by `MAX_SYMLINK_DEPTH=8` (`-ELOOP=40`, musl's real compiled value — not FreeBSD's `62`, the same errno-divergence class this file's syscall-ABI section already warns about). `SYS_SYMLINK=155`/`SYS_READLINK=154` (next after `SYS_READV=153`), wire format `(target_ptr, target_len, linkpath_ptr, linkpath_len)`/`(path_ptr, path_len, buf_ptr, bufsize)` — `third_party/musl/src/unistd/{symlink,readlink}.c` patched the same way `rename.c` already was, to compute string lengths explicitly. Confirmed live via `modules/oxfs`'s own boot self-check: create, `readlink` round-trip, `stat` follows to the real target while `lstat` reports `S_IFLNK`/the link's own size, `open` follows and reads the target's real content, `unlink` removes only the link |
| Console/VT ioctls, serial/tape/I2C hardware, syslog, real pty | not started | 24 (`NEEDS_HARDWARE`) | each needs a real device/driver this kernel doesn't model — not one gap, several unrelated small ones (see `docs/BUSYBOX_APPLETS.md`) |
| Real block device driver + oxfs persistence | done | subset of 17 (`NEEDS_BLOCKDEV`, down from 20) | see this file's own "Real disk persistence" section for the full design — `src/ata.rs` (ATA PIO), real superblock/inode-table/bitmap format, mount-or-format boot logic, write-through persistence. oxfs's own disk is still a fixed, hardcoded backing store, not a mountable one — see the mount-table row below for what that did and didn't unblock |
| Mount table (`mount --bind`/`mount -t tmpfs`) | done | `mount`/`umount`/`mountpoint` in `docs/BUSYBOX_APPLETS.md`'s `NEEDS_BLOCKDEV` row | see this file's own "Mount table" section for the full design — `SYS_MOUNT_BIND`/`SYS_MOUNT_TMPFS`/`SYS_UMOUNT2` in `modules/oxfs`, a second in-memory-only tmpfs inode/block pool, `resolve_path_impl`'s redirect hook, `/proc/mounts`. Deliberately scoped, not a general pluggable-filesystem-type VFS (there's exactly one real block device/filesystem, nothing else to plug in) — doesn't unblock `pivot_root`/`switch_root` (need a real block-device-agnostic mount table) or anything needing a real partition table/multiple on-disk formats (`blkid`/`fdisk`/`fsck`/`mkswap`/...) |
| uid/passwd-db model (process-attribute half + `/etc/passwd`+`/etc/group` lookups) | done | most of 16 (`NEEDS_UID`) | see this file's own "Permission model" section below for the full design. `whoami`/`groups`/`logname` should now work end-to-end (musl's `getpwuid`/`getgrgid` parse the new `/etc/passwd`/`/etc/group` in pure userspace, no new syscall needed for that half); `adduser`/`chpasswd`/`passwd`/`mkpasswd`/`addgroup`/`delgroup`/`remove_shell`/`envuidgid`/`setuidgid` still need real *mutation* of those two files (an applet-level gap now, not a kernel one — both files are already real, writable oxfs files) |
| real login/session-authentication flow (`su`/`login`/`sulogin`/`getty`) | done | 4 (subset of `NEEDS_UID`) | see this file's own "Session, controlling-tty, and login authentication" section — a real second user + `/etc/shadow` (`su`/`login`), plus a real session/controlling-tty/foreground-pgroup model and real Ctrl+C→`SIGINT` delivery (`sulogin`/`getty`). Confirmed live via `tests/session_syscall_smoke.rs` for the syscall-reachable surface; `su`/`login` interactive password-prompt behavior and `sulogin`/`getty`'s own tty takeover are manual-QEMU-only (same precedent as everywhere else needing live keyboard input) |
| `clock_gettime`/`gettimeofday`/`time` | done | — | `SYS_CLOCK_GETTIME=138` in `modules/clock` — see this file's own "Real-time clock" section |
| `nanosleep` | done | 9 (`NEEDS_CLOCK`), several also needed the clock read above | `SYS_NANOSLEEP=139` — see this file's own "Real-time clock" section. `date`/`hwclock`/`rtcwake`/`adjtimex`/`crond`/`crontab` needed the clock read more than a real sleep; not re-probed against the current roster to confirm which now fully work end to end |
| Init-system/service-supervisor framework | not started, out of scope | 6 (`NEEDS_INIT`) | `runsv`/`svlogd`/`bootchartd`/... — this kernel has no init framework to plug into at all |
| `tcsetpgrp`/real job control | blocked on a pty/foreground-pgrp concept | — (folded into `NEEDS_HARDWARE`) | — |
| `uname` | done | — | `SYS_UNAME=137` in `modules/posix_compat` — real `uname(2)`'s exact single-pointer wire format, fixed `struct utsname` fields (`src/syscall.rs`'s `sys_uname`); `release` is this crate's own `CARGO_PKG_VERSION`, everything else a fixed placeholder (no real hostname-config or build-timestamp mechanism exists) |
| `gethostname` | done | — | no new syscall needed -- musl's `gethostname()` (`src/unistd/gethostname.c`) is a pure wrapper around `uname()`. Wired up the `hostname` applet (`CONFIG_HOSTNAME`'s own `//applet:` marker was missed by the same extraction gap as `uname`'s) so there's something real to exercise it; its `-d`/`-f`/`-i` flags stay `NEEDS_NETWORK` (real DNS resolution) |

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
- `sha2`/`chacha20` (`src/random.rs`, backing `/dev/random`/`/dev/urandom` — see this file's "Real
  networking" known-gaps section): `default-features = false`, `sha2` additionally needs
  `features = ["force-soft"]` and `chacha20` needs `--cfg chacha20_backend="soft"` passed via
  `.cargo/config.toml`'s `[target.x86_64-oxidebsd] rustflags` — both crates otherwise try to
  compile a SIMD-capable backend this target's disabled SSE/MMX can't lower (a real
  `rustc-LLVM ERROR`, not a lint). Crypto primitives are the one place this codebase deliberately
  prefers a vetted dependency over hand-rolling — the opposite call from `pic8259`/`uart_16550`
  above, same reasoning `linked_list_allocator`/`x86_64` already get for different safety-critical
  logic.
