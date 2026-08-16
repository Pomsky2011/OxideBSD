# CLAUDE.md

This file provides guidance to Claude Code when working with code in this repository.

## Project

OxideBSD is a 100% Rust-based BSD-like OS, x86_64 only (see `ROADMAP.md` for phase history).
Current state:

- Boots via `bootloader` v0.9 + `bootimage`/QEMU. GDT/TSS/IDT with a dedicated double-fault
  stack, PIC-driven interrupts (timer + PS/2 keyboard), a VGA console mirroring serial, a heap
  allocator over bootloader-provided paging.
- Separate per-process address spaces, ELF64 loading, ring-3 execution, and a native BSD-style
  syscall ABI over `SYSCALL`/`SYSRETQ` (`src/syscall/mod.rs`) with carry-flag error signaling.
- A dynamic kernel module loader (`src/module.rs`) relocates `#![no_std]` code into the kernel at
  boot and resolves symbol references against a hand-curated kernel API. Syscall handlers are
  registered by modules, not hardcoded: `modules/native_abi/` (core syscalls), `modules/
  posix_compat/` (pipe/dup2/ioctl/setpgid/...), `modules/signal/` (kill/sigaction/...),
  `modules/oxfs/` (the live filesystem).
- `modules/oxfs/` is a real in-memory Unix-shaped inode/block filesystem (real names,
  multi-component paths, per-process cwd, no fixed file-size cap) — replaced `modules/fat32/`
  (8.3 names, one path component per call, fixed file cap), which still builds/self-checks via
  `cargo build` but is no longer loaded at boot.
- A real process table + cooperative round-robin scheduler (`src/process/`) with
  `fork`/`execve`/`wait4`/`getpid`, real `argv`/`envp` passthrough, blocking pipes, and per-process
  signal delivery.
- pid 1 is BusyBox's `hush`, built against a patched musl fork — not the original hand-written
  `userland/stsh/` shell (still buildable, no longer wired up). 256 BusyBox applets run as
  standalone static binaries (curated down from 314 before v0.1 — see the BusyBox port section's
  own "Removed before v0.1" reference), `execve`'d individually (not a multi-call `busybox` binary
  dispatching on `argv[0]` — that passthrough exists now, but the roster hasn't been rebuilt to
  use it).
- A real networking stack (`src/drivers/pci.rs`, `src/net/*`, `modules/net/`): PCI + an rtl8139 driver,
  Ethernet/ARP/IPv4/ICMP, UDP/TCP/raw-ICMP sockets, `poll(2)`, and real hostname resolution over
  musl's own DNS stub resolver (no DNS protocol code of its own) — see "Real networking" below.
- A real, on-target C compiler (`third_party/tinycc`, vendored TinyCC) — `tcc` runs as an ordinary
  seeded `/bin` binary and can genuinely compile+link a real C file against a real, seeded
  `/usr/include`/`/usr/lib` musl tree, producing a real runnable ELF — see "TinyCC" below.
  GCC/Clang remain unstarted (real subprocess pipelines, likely real dynamic linking and threads —
  a much bigger lift than this kernel currently supports).

Known, deliberate gaps: no pointer validation in `sys_read`/`sys_write`, no module unload/reload,
no preemption, no copy-on-write fork, no frame deallocation anywhere, `sys_read` on stdin is
non-blocking (busy-polled by userland), no general block-device-agnostic VFS/mount-table layer (a
real ATA disk driver + oxfs mount/format persistence + a scoped bind/tmpfs mount table exist now —
see "Real disk persistence"/"Mount table" — but only for oxfs's own fixed backing store), no IPv6,
no real routing table (one default-gateway rule only). See "BusyBox gap analysis" below for what's
needed to go further. Architecture decisions for remaining subsystems haven't been made — discuss
with the user before large structural commitments.

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
`QemuExitCode::Success`) and `src/console/serial.rs` (hand-rolled 16550 UART, read via `-serial stdio`).

- `src/lib.rs` defines `no_std` test scaffolding (`custom_test_frameworks`, `#[test_case]`) and
  boots itself under `#[cfg(test)]`.
- `tests/*.rs` integration tests use `harness = false` — each defines its own `fn main()` via
  `entry_point!` and calls `exit_qemu()` directly.
- `tests/fork_wait.rs` + `userland/fork-exec-smoke/`: since `scheduler::start`/`process::do_exit`
  never return to a test's own `main`, it registers a syscall number (`9999`) directly via
  `oxidebsd::syscall::oxidebsd_register_syscall` (kept `pub` for this) whose handler calls
  `exit_qemu`.
- **Any test claiming to verify syscall-reachable code should spawn a real ELF and go through an
  actual `SYSCALL` instruction**, not call kernel handlers as plain Rust functions from a test's
  own `main()` — interrupts stay enabled and `ticks()` keeps advancing in the latter, hiding real
  bugs (see "Real networking"'s `hlt()`/`ticks()`-frozen-during-syscall entries below). This is
  the established pattern (`tests/*_syscall_smoke.rs` + `userland/*-syscall-smoke/`) for anything
  syscall-shaped added from here on.
- Anything needing live interactive keyboard input (real Ctrl+C→SIGINT, `su`/`login` prompts,
  `sulogin`/`getty` tty takeover, persistence surviving a real QEMU restart, any `reboot`/halt/
  poweroff success path) can't be scripted and is manual-QEMU-only — hand it to the user rather
  than trying to drive it via a backgrounded `cargo run`.

## Custom target spec (`x86_64-oxidebsd.json`)

- `target-pointer-width`/`target-c-int-width` must be numbers, not strings.
- Float returns need both `"features": "...,+soft-float"` and `"rustc-abi": "softfloat"`, or
  `core`/`compiler_builtins` fail to build.
- `panic-strategy: "abort"` is the only supported strategy — hence `-Z panic-abort-tests` in
  `.cargo/config.toml` (otherwise Cargo builds an unwind-based test harness and produces a second,
  ABI-incompatible `core`).
- SSE/MMX disabled, `disable-redzone: true` (interrupt handlers can't safely use either).

## Memory management (`src/memory/mod.rs`, `src/memory/allocator.rs`)

- `memory::init` walks `CR3` and adds `BootInfo::physical_memory_offset` to get a virtual pointer
  to the level-4 table (relies on the bootloader's `map_physical_memory` feature). Call at most
  once — hands out a `&'static mut`.
- `memory::BootInfoFrameAllocator` bump-allocates from `BootInfo::memory_map`'s `Usable` regions,
  never reuses a frame. Holds plain `(region_index, frame_number)` cursor state, not an iterator
  rebuilt from scratch each call — the old `next: usize` + `.nth(self.next)` approach was
  O(n)-per-allocation (O(n²) total), invisible at a few thousand frames but a real multi-minute
  stall past tens of thousands. **A boxed-iterator fix is *wrong*, not just suboptimal**: this
  allocator is constructed *before* `allocator::init_heap` (which needs it to map the heap's own
  pages), so any heap allocation inside its own constructor panics with no heap yet to satisfy it.
- `allocator::init_heap` and `module::map_region` map freshly allocated pages with `.ignore()`,
  not `.flush()` — a page that's never been mapped before has no stale TLB entry to invalidate,
  and `invlpg` is individually trapped/emulated under QEMU's software TCG (real cost at scale).
- The heap lives at a fixed VA (`allocator::HEAP_START`); its size scales with detected RAM
  (`memory::usable_ram_bytes()`), clamped between a proven floor and ceiling. Same RAM-scaling
  pattern for `process::kernel_stack_size()`/`user_stack_pages()`. NOT scaled: `modules/fat32`'s
  embedded image size, `module::MODULE_VA_BASE`/`MODULE_REGION_CEILING` (a VA-range limit from the
  relocation model, not RAM). QEMU's own RAM (`Cargo.toml`'s `[package.metadata.bootimage]` `-m`)
  is `1024` MiB — real physical-memory commitment from module-load time on, not paged in on demand.
- The global allocator is `linked_list_allocator`'s `Heap` wrapped in a local `Locked<T>`
  (`spin::Mutex`), not the crate's own `LockedHeap` — avoids a second spinlock crate in the graph.

## User-mode execution (`src/memory/address_space.rs`, `src/process/elf.rs`, `src/process/usermode.rs`)

`process::spawn` builds the first process this way at boot; `process::do_execve` builds every
later one the same way, mid-syscall.

- Userland crates (`userland/*`) are separate workspace members; `build.rs`'s
  `build_userland_crate` cross-builds each into `target/userland/` and exposes `<NAME>_ELF_PATH`
  via `cargo:rustc-env` for `include_bytes!`. Each crate's `linker.ld` forces a distinct load base
  clear of the kernel image, heap, phys-mem-offset window, and `bootloader` v0.9's own
  identity-mapped low-memory region. **This floor moves as the kernel image grows** — it has
  already had to move once as embedded content (BusyBox applet ELF bytes, TinyCC runtime tree,
  ...) pushed the kernel binary bigger and swallowed lower load addresses (surfaces as
  `Elf(MappingFailed)`/`PageAlreadyMapped` at `execve`/spawn time, not at build time). Currently
  `0x4000000` (64 MiB). **Before adding a new binary or trusting this number**, re-derive it:
  `readelf -l target/x86_64-oxidebsd/debug/oxidebsd | grep -A1 LOAD`, take the highest
  `VirtAddr + MemSiz`, round up with real headroom (not "barely enough"). `userland/musl-smoke/`
  isn't a Rust crate — built with `musl-gcc`, load base set via `-Wl,-Ttext-segment=`.
- `AddressSpace::new` shallow-copies all 512 L4 entries from the currently active table (no
  higher-half split — kernel, heap, phys-mem window, and every user ELF's load address share the
  low canonical range at different indices). Safe only when the active table's user-space content
  is empty (true only for `process::spawn` at boot). `AddressSpace::fork`/`new_excluding_user`
  (live process — `fork`/`execve`) instead recursively walk the table using `USER_ACCESSIBLE` as
  the sole kernel-vs-user signal at any level.
- **`gdt.rs`'s ring-0 stacks must be `static mut`, not `static`.** A plain `static`, never written
  via a Rust `&mut`, gets interned into `.rodata` by the optimizer (writes are CPU-hardware-only,
  invisible to that analysis) — causes a double/triple fault the instant an exception uses it. Any
  future stack added the same way needs the same treatment.
- **Every IDT gate a software interrupt (`int n`, `int3`, ...) can trigger from ring 3 needs
  `DPL = Ring3` explicitly** — gates default to `Ring0`, and software interrupts additionally
  require `CPL <= gate DPL`. Wrong DPL manifests as a `#GP` on the IDT entry itself.
- **`elf::load` tracks already-mapped pages in a `BTreeMap<Page, PhysFrame>` for one call** —
  `PT_LOAD` segments align to `p_align`, not to each other, so small binaries routinely share a
  page across segments; mapping/zeroing it twice is a bug. Flags aren't unioned across segments
  sharing a page — **found live**, not just theoretical: `userland/sa-siginfo-syscall-smoke`
  (the SA_SIGINFO handler-invocation smoke test) was the first userland crate with real writable
  globals (every earlier `*-syscall-smoke` crate kept mutable state on the stack instead), and its
  small total size let the RW `.got`/`.data`/`.bss` segment share a page with the RX `.text`/
  `.rodata` segment — the shared page kept only the first segment's R/E flags, so the very first
  static write page-faulted (`PROTECTION_VIOLATION | CAUSED_BY_WRITE | USER_MODE`). Worked around
  at the linker-script level for that one crate (`. = ALIGN(0x1000);` before the writable
  sections — see that crate's own `linker.ld` comment), not fixed in `elf.rs` itself — a real
  flag-union fix would benefit every future small binary with writable globals, not just this one,
  but was out of scope for the pass that found it.
- Known simplification: no `NO_EXECUTE` on any ELF segment (would also need `EFER.NXE`).

## Syscall ABI (`src/syscall/`)

OxideBSD's own native, BSD-flavored ABI over `SYSCALL`/`SYSRETQ` — not Linux-compatible. Syscall
number in `RAX`, up to 4 args in `RDI`/`RSI`/`RDX`/`R10` (not `RCX`/`R11`, clobbered by `SYSCALL`
itself). Success/failure via the **carry flag** (`CF=0` success, value in `RAX`; `CF=1` failure,
positive errno in `RAX` — traditional BSD/x86 Unix convention). Pre-musl-port syscalls
(`SYS_EXIT=1`, `SYS_FORK=2`, `SYS_READ=3`, `SYS_WRITE=4`, `SYS_OPEN=5`, `SYS_CLOSE=6`,
`SYS_WAIT4=7`, `SYS_LSEEK=8`, `SYS_GETPID=20`, `SYS_EXECVE=59`) match real FreeBSD numbers as an
authenticity nod. Everything since (`SYS_MMAP=100`, `SYS_MUNMAP=101`, `SYS_BRK=102`,
`SYS_SET_FS_BASE=103`, `SYS_WRITEV=104`, `SYS_PIPE=105`, `SYS_DUP2=106`, `SYS_GETPPID=107`,
`SYS_GETCWD=108`, `SYS_UNLINK=109`, `SYS_RMDIR=110`, `SYS_RENAME=111`, `SYS_KILL=116`,
`SYS_SIGACTION=117`, `SYS_SIGPROCMASK=118`, `SYS_SIGRETURN=119`, `SYS_SETPGID=120`,
`SYS_GETPGID=121`, `SYS_IOCTL=124`, `SYS_DUP=125`, `SYS_FSTAT=126`, `SYS_STAT=127`,
`SYS_LSTAT=128`, `SYS_GETDENTS=129`, `SYS_UNAME=137`, `SYS_CLOCK_GETTIME=138`,
`SYS_NANOSLEEP=139`, `SYS_SOCKET=140`, `SYS_BIND=141`, `SYS_SENDTO=142`, `SYS_RECVFROM=143`,
`SYS_SETSOCKOPT=144`, `SYS_CONNECT=145`, `SYS_LISTEN=146`, `SYS_ACCEPT=147`, `SYS_POLL=148`,
`SYS_SOCKETPAIR=149`, `SYS_SET_TID_ADDRESS=150`, `SYS_FCNTL=151`, `SYS_SHUTDOWN=152`,
`SYS_READV=153`, `SYS_READLINK=154`, `SYS_SYMLINK=155`, `SYS_SETITIMER=156`, `SYS_GETITIMER=157`,
`SYS_GETUID=158`, `SYS_GETEUID=159`, `SYS_GETGID=160`, `SYS_GETEGID=161`, `SYS_SETUID=162`,
`SYS_SETGID=163`, `SYS_GETGROUPS=164`, `SYS_CHMOD=165`, `SYS_CHOWN=166`, `SYS_UTIMENSAT=167`,
`SYS_SETSID=112`, `SYS_GETSID=177`, `SYS_SETGROUPS=178`, `SYS_MOUNT_BIND=174`,
`SYS_MOUNT_TMPFS=175`, `SYS_UMOUNT2=176`, `SYS_FSYNC=471`...`SYS_FSTATFS=477`,
`SYS_PRLIMIT64=478`...`SYS_REBOOT=486`, `SYS_UMASK=487`, `SYS_LINK=488`, `SYS_MKNOD=489`,
`SYS_CHROOT=490`, `SYS_GETRUSAGE=491`) is OxideBSD's own invention — numbers/shapes picked for
what porting musl/BusyBox actually needed, not copied from FreeBSD/Linux. **Check `src/syscall/`
and module sources for the current highest number before assigning a new one.**

**Before picking a new syscall number**: grep every still-inert real-Linux value in
`third_party/musl/arch/x86_64/bits/syscall.h.in` for a live musl caller before reusing it — this
bit twice: `SYS_KILL`'s invented number collided with real Linux's inert `setgroups` number, which
*did* have a live musl caller (`initgroups()`), silently misrouting a real `setgroups()` call into
`kill(2)` instead of `ENOSYS`ing cleanly; and a later batch continuing the `100-178` sequence
collided with real, still-referenced numbers further down the same file (`__NR_gettid` has a live
caller in `src/thread/synccall.c`). Since this musl fork is frozen at tag `v1.2.6`, `471`+ (past
the highest real-Linux number this file's own `bits/syscall.h.in` ever inspects) is *permanently*
collision-free — new invented numbers should continue from there, or from this ABI's own highest
already-assigned number, whichever is higher, not by filling gaps in the `100-178` range.

errno **is meant to** use FreeBSD's values where Linux/BSD diverge, but whatever this file returns
via the carry-flag ABI becomes musl's raw `errno` directly (see
`third_party/musl/arch/x86_64/syscall_arch.h`'s `jnc`/`neg` conversion) — it must match musl's own
compiled-in `bits/errno.h` (`third_party/musl/arch/generic/bits/errno.h`), not real FreeBSD.
`EBADF`/`EINVAL`/`ECHILD`/`ENOEXEC`/`EPIPE`/`ESRCH`/`ENOTTY` happen to be identical between
Linux/generic and FreeBSD, so those are accidentally fine. **Known, currently-wrong** (real
FreeBSD values that don't match musl, left as a flagged, deliberately deferred gap — discuss scope
with the user before a sweeping renumbering): `src/net/udp.rs`'s `ENOTSOCK=38` (musl: `88`),
`EDESTADDRREQ=39` (musl: `89`), `EADDRINUSE=48` (musl: `98`), `EHOSTUNREACH=65` (musl: `113`);
`src/net/tcp.rs`'s `EISCONN`/`ENOTCONN`/`ECONNREFUSED`/`ETIMEDOUT`/`EOPNOTSUPP`/`EADDRINUSE`/
`EHOSTUNREACH`. **Already fixed** (confirmed load-bearing by a live test, not preemptively):
`src/syscall/mod.rs`'s `EPROTONOSUPPORT=93`/`EAGAIN=11`/`ENOTSOCK=88`/`ENOSYS=38` (was FreeBSD's `78`;
blocked `su`'s real `ENOSYS`-fallback path), `tcp.rs`'s own `EAGAIN=11` (was `35`).

The number→handler mapping is a runtime registry (`SYSCALL_TABLE`, `Mutex<BTreeMap>`) populated by
`oxidebsd_register_syscall` from each module's `module_init` — not a hardcoded `match`. An
unregistered number logs `[boot] unrecognized syscall number N` and returns `ENOSYS`, the main
tool for discovering what a ported program's startup still needs.

- **`SYSRETQ`'s selector scheme forces GDT order.** `SYSRETQ` derives `SS`/`CS` from
  `IA32_STAR[63:48]` as `+8`/`+16` — user data must sit immediately before user code. `src/cpu/gdt.rs`
  order: kernel code, kernel data, unused placeholder (offset spacing), user data, user code, TSS.
  Don't reorder without redoing the `STAR` arithmetic; `x86_64::registers::model_specific::
  Star::write` validates this and panics loudly if the GDT regresses.
- **No automatic stack switch on `SYSCALL` entry.** Control arrives at `syscall_entry` still on
  the user's own stack. `gdt::CURRENT_RSP0` (`static mut`, kept in sync by
  `gdt::set_kernel_stack` on every context switch) always names the current process's own kernel
  stack — required since two processes can be mid-syscall at once (`do_wait4` already blocks/
  reschedules mid-syscall). No per-CPU `swapgs` — single-core only.
- `SyscallFrame`: the stub's pushed GPRs plus `user_rsp` (`SYSCALL` doesn't push a stack frame the
  way an interrupt gate does). `rcx`/`r11` double as saved `RIP`/`RFLAGS`; `syscall_dispatch`
  flips bit 0 of `r11` to signal `CF`.
- `dispatch()` is a small, pure, directly unit-tested function separate from
  `syscall_dispatch`'s raw-pointer/frame handling (see `src/lib.rs` tests).
- A registered handler's own wire format (`SyscallHandler`) is a plain `i64` (negative =
  `-errno`) — distinct from the public carry-flag ABI, just the module↔kernel boundary's shape.
- `sys_write`/`sys_read` don't validate `[ptr, ptr+len)` before dereferencing — a bad pointer
  page-faults (handled safely by `page_fault_handler`: log + reboot), not a soundness hole, but no
  safety net for user programs yet.
- `sys_read` on stdin is non-blocking by design (returns `Ok(0)` on empty) — pushes polling into
  userland. Any other fd delegates to `crate::fd`'s per-process `(Pid, fd)` registry.
- `sys_write`'s `fd == 2` (stderr) is an alias for `fd == 1` — no real second sink exists.

## musl port (`third_party/musl`, `userland/musl-smoke/`, `src/process/user_stack.rs`, `src/cpu/fpu.rs`)

musl is patched (not the kernel made Linux-compatible) to speak this native ABI directly.
`third_party/musl` is a submodule of a personal fork (`ifduyue/musl`), patches on its own
`oxidebsd` branch based on tag `v1.2.6`. Pin/update by committing on that branch, pushing, then
`git add third_party/musl` here. Patch surface is deliberately small, entirely under `arch/x86_64/`:

- `syscall_arch.h`: `jnc 1%=f; neg %%rax; 1%=:` after every `syscall` converts carry-flag errors
  into musl's expected small-negative-value convention.
- `bits/syscall.h.in`: only the `__NR_*` values musl's static-binary startup path actually reaches
  are remapped; everything else keeps its inert Linux value. `open`/`execve` are patched at the
  call-site level instead of just remapped (see argument-convention note below).
- `__set_thread_area.s`: TLS base set via `SYS_SET_FS_BASE` (a bare base-address write, no
  `arch_prctl` subcommand).

Key gotchas, each a real bug already hit and fixed — the same *class* of bug can recur for any
future syscall port, so re-check these when adding one:
- musl's entire stdio write path goes through `writev`, never plain `write` — `SYS_WRITEV` is
  load-bearing (its absence silently redirected all `printf` output into `getpid()` via a
  numbering collision — no crash, just zero output, invisible from `cargo test`/clippy).
- **Remapping a `__NR_*` macro isn't enough if a 64-bit-suffixed sibling exists.** `src/internal/
  syscall.h` unconditionally prefers `SYS_getdents64` over `SYS_getdents` whenever both are
  defined — real `readdir()` calls the plain macro, but its *value* silently became the 64-bit
  sibling's (left at its inert real-Linux number), resurrecting the exact collision the remap was
  meant to close. Both are now remapped and kept in sync — any future syscall with a same-shaped
  64-bit sibling (`__NR_stat64`, `__NR_fstatat64`, ...) needs the same audit.
- SSE was never enabled at the hardware level (`CR0.EM`/`CR4.OSFXSR`/`OSXMMEXCPT`); `src/fpu.rs::
  init()` enables it once at boot. No save/restore across context switches — fine only while at
  most one SSE-using process is ever mid-computation (true today, no preemption).
- `src/process/user_stack.rs` builds a real System V argc/argv/envp/auxv initial stack. `AT_PHDR` is
  derived from whichever `PT_LOAD` segment has the smallest `p_offset` (this project's own linker
  scripts don't map the ELF header into any segment). `AT_RANDOM` is a fixed placeholder.
- **`open`/`execve` argument-convention mismatches are fixed on the musl side**, not by remapping
  alone: `src/fcntl/open.c` computes `path_len` via `strlen()`; `src/process/execve.c` builds real
  `argv`/`envp` as length-prefixed `RawArgvEntry{ptr, len}` arrays (zero-entry-terminated), using
  the real 4th syscall arg (`R10`) for `envp_ptr`. Same length-prefix pattern for `unlink`/
  `rmdir`/`rename`/`readlink`/`symlink`/`chdir`/`mkdir`. **Any future libc call ported here needs
  the same audit** — matching the syscall *number* isn't sufficient if the argument shape differs.
- **A hand-written asm stub can bypass the `__NR_*` remap table entirely.** `src/process/x86_64/
  vfork.s` hardcodes the real Linux syscall number (`58`) directly, not `syscall(SYS_vfork, ...)`.
  Fixed by hardcoding OxideBSD's own `SYS_FORK` (`2`) in the asm instead (a real `fork()`, not true
  vfork share-until-exec semantics — POSIX-legal). **Any syscall with its own hand-written
  arch-specific asm stub (check `third_party/musl/src/*/x86_64/*.s`) needs this same direct-patch
  treatment, not just a header remap.**
- `utimensat` (`SYS_UTIMENSAT=167`) drops the always-`AT_FDCWD` `fd` arg and passes
  `(path_ptr, path_len, times_ptr, flags)`. Kernel side is existence-check only (`ENOENT`/success —
  oxfs has no per-inode timestamps) — still enough for `touch.c`'s real `ENOENT`→`O_CREAT`
  fallback.
- `SYS_MMAP=100` is `(addr_hint, len, prot)` (matches musl's actual call site) — always
  anonymous+private, bump-allocated, never reclaimed. `SYS_MUNMAP=101` no-op success. `SYS_BRK=102`
  grows/shrinks `Process.brk`, no reclaim on shrink.

## BusyBox port (`third_party/busybox`, `modules/posix_compat/`)

256 applets run today (24 original + 232 from the second-pass roster), each its own standalone
single-applet static binary (see Project above for why). Vendored as a submodule (fork of
`mirror/busybox`, tag `1_36_1`, `oxidebsd` branch — same pin/update procedure as musl). `build.rs`'s
`build_busybox_applet` runs BusyBox's own `allnoconfig` → flip one applet's Kconfig symbol →
`make oldconfig` → build, asserting `NUM_APPLETS == 1`; `sh` additionally forces on
`CONFIG_HUSH_INTERACTIVE`/`HUSH_JOB`/`FEATURE_EDITING` and hush's real control-flow symbols
(`HUSH_IF`/`HUSH_LOOPS`/`HUSH_CASE`/`HUSH_FUNCTIONS`/`HUSH_TICK`/`HUSH_TEST`/...) — `make
allnoconfig` writes an explicit "not set" for every Kconfig symbol up front, so the later
`oldconfig` pass never gets to apply hush's real `default y` for these; `configure_busybox_single_
applet` flips those lines directly. Applets are embedded into oxfs's inode table by
`modules/oxfs`'s `module_init` (data-driven from `build.rs`'s `BUSYBOX_APPLETS`/
`BUSYBOX_APPLETS_PASS2` lists; each new applet needs one manual `seed_file` call added in oxfs).
The roster grew from 24 to 290 (287 from an exhaustive per-applet build probe run once
`stat`/`fstat`/`lstat`/`getdents` existed, keeping every applet that built, plus `cryptpw`/`uname`/
`hostname` added individually afterward — **"builds" is a much weaker bar than "works"**, plenty of
applets that make no sense on this kernel still compile and fail cleanly at runtime, usually
`ENOSYS`), then was deliberately curated down to 232 (256 total with the original 24) before v0.1
by dropping 58 applets whose core function is structurally incapable of working under this
kernel's current architecture (not "not yet fixed" — see `docs/BUSYBOX_APPLETS.md`'s own "Removed
before v0.1" section for the full list and reasoning; a handful of others that looked similarly
blocked, like `chroot`/`mknod`/`link`, had their blocker close in the meantime and were fixed
forward into the roster instead). `docs/BUSYBOX_APPLETS.md` is the full roster (its own counts are
scoped to the original 287-applet probe cohort, so don't include `cryptpw`/`uname`/`hostname`),
tagged with what each applet still needs (`NEEDS_NETWORK`/`NEEDS_PROC`/`NEEDS_CLOCK`/`NEEDS_UID`,
or `WORKS`; the once-populated `NEEDS_SYSCALL`/`NEEDS_HARDWARE`/`NEEDS_BLOCKDEV` categories are now
empty — every applet in them was either fixed forward or cut), plus what didn't build and why
(almost entirely missing Linux uapi headers musl doesn't vendor). `modules/oxfs/src/test_busybox.sh`
(seeded at `/test_busybox.sh`, run via `sh /test_busybox.sh` at the hush prompt) started at ~40 real
applet/control-flow checks and was expanded pre-v0.1 to ~95, covering most of the curated roster
(what's deliberately left out — daemons, destructive/interactive/hardware-dependent applets — is
listed under that script's own "NOT EXERCISED" comment), with a real `PASS`/`FAIL` tally, and is
the tool that found the
`wait4`/exit-status and `kill(pid,0)` bugs documented below.

- `build_busybox_applet` is staleness-checked against `third_party/busybox`, `build.rs`, and
  `musl_sysroot`'s own `lib/libc.a` mtimes, and builds in parallel across
  `available_parallelism()` workers — both load-bearing at this roster size. **Two real staleness bugs
  found and fixed**: (1) `libc.a`'s own mtime wasn't originally in the freshness comparison, so a
  musl-side syscall fix left already-built applets linked against a stale libc silently. (2) Even
  with that fixed, BusyBox's own incremental build tracks its own sources but never musl's
  *installed sysroot headers* as a dependency — a real musl fix left ~172/177 object files in one
  applet's build directory unrecompiled despite the applet's own binary mtime looking fresh. **Only
  a full `rm -rf` of the stale `O=` out-of-tree build directory before rebuilding reliably fixes
  this** — trust neither BusyBox's own incremental tracking nor a binary's mtime alone. Expensive
  (~38 min for a full roster rebuild) but only triggers when something genuinely changed.
- `hush` (pid 1) uses real `execvp()`/`$PATH` search — `process::spawn` passes `envp` of
  `PATH=/bin`, so `/bin/<name>` resolves from any cwd. `modules/oxfs`'s `module_init` seeds every
  applet under its bare name in `/bin`; data fixtures (`hello.txt`, ...) stay at root.
- New kernel-resident pieces `sh` required: the real 4th syscall arg (`R10`, for `envp`), real
  blocking `pipe(2)`/`dup2(2)` (`src/fs/pipe.rs` — bounded at `PIPE_CAPACITY` (64 KiB, matching real
  Linux's own default) with a real blocking writer, blocks via `BlockReason::WaitingForPipeData`/
  `WaitingForPipeSpace` + `scheduler::schedule()`), and a **per-process** `(Pid, fd)` fd table
  (`src/fs/fd.rs`) — a flat table broke real pipelines the moment a parent closed its own copy of a
  pipe fd out from under still-using children.
- **Fixed: a producer whose own `write()` calls never block used to be able to OOM the kernel
  heap.** `yes | head -n 3` used to reliably panic (`memory allocation of 67239936 bytes failed`),
  found by `modules/oxfs/src/test_busybox.sh`. Root cause: `src/fs/pipe.rs`'s buffer used to be an
  unbounded `VecDeque<u8>` — `yes` never yields (no blocking syscall in its write loop), so with no
  preemption `head` never got scheduled to read three lines and close its end, and the buffer grew
  without limit. Not specific to `yes`/`head` — any pipeline where a non-blocking producer outpaces
  a consumer that stops reading early had the same failure mode. Fixed by bounding the buffer (see
  above) rather than adding preemptive scheduling — once full, `write_into` blocks the producer
  (`BlockReason::WaitingForPipeSpace`) until the consumer drains space or closes its read end
  (`EPIPE` for the still-blocked writer once woken, via the same `close_direction` codepath that
  already delivered `EOF` to a blocked reader on a write-end close).
- **`IA32_FS_BASE` (TLS) is a single global MSR `context_switch::switch_context` never
  saved/restored per-process** — a musl-linked parent resuming after a musl-linked child exited
  would silently run with the dead child's leftover TLS base and fault on its own stack-protector
  check. Fixed via `Process::fs_base`, restored on every switch by
  `scheduler::activate_and_prepare` (not `switch_context` itself — a single global register
  invisible to the GPR/RSP-only context switch).
- `getcwd`/`getppid`/`chdir`/`mkdir` needed the same argument-convention fixes as `open` — only
  surfaced once `hush` was driven interactively (`cd`, `pwd`), not via `-c "cmd"` alone.
- Real interactive `sh` needed a separate blocking stdin-read pass (`BlockReason::WaitingForStdin`,
  `scheduler::wait_for_ready`).
- musl's stdio calls `write(fd, buf, 0)`/`read(fd, buf, 0)` with a null/garbage `buf` (POSIX-legal
  at length 0) — crashed every fd callback's unconditional `slice::from_raw_parts`; fixed centrally
  in `src/fs/fd.rs`'s `read`/`write` funnel functions.
- New syscalls always go in a dedicated module (`modules/posix_compat/`, `modules/signal/`, ...),
  not `modules/native_abi/` — keeps the core ABI module small.

## Interactive shell (`src/console/stdin.rs`, `userland/stsh/`)

`stsh` ("stupidshell") is the original hand-written interactive userland program — still
buildable, no longer pid 1 (superseded by `hush`). Its design remains the reference for stdin:

- Keyboard IRQ (`src/cpu/interrupts.rs`) decodes scancodes into a fixed 256-byte ring buffer
  (`src/console/stdin.rs`) — non-ASCII dropped, no allocation in the interrupt handler. `sys_read` drains
  it. Auto-echo only when `TERMIOS.ECHO` is set.
- The `spin::Mutex` around the ring buffer can't deadlock between IRQ and syscall context
  specifically because `SFMASK` clears `IF` for a `SYSCALL`'s entire duration on this single core —
  breaks if SMP is ever added.
- `sys_read` is non-blocking; `stsh` busy-polls a byte at a time (`do_wait4`/pipe reads already
  block, `sys_read` itself hasn't been converted).
- `src/console/vga.rs`'s `Writer` is a true 2D-addressable console with a minimal ANSI/VT100 CSI escape
  parser (cursor moves, erase display/line, SGR colors) so full-screen applets (`vi`, `clear`,
  `reset`) render correctly; unrecognized sequences are parsed just enough to find the final byte
  then dropped.
- Real `SYS_IOCTL=124` (`src/console/stdin.rs`'s `RawTermios`, a single **global**, not per-session,
  `TERMIOS`) implements `TCGETS`/`TCSETS*`/`TIOCGWINSZ` (fixed `24x80`)/`TIOCSWINSZ`
  (accepted/discarded); else `ENOTTY`. Only succeeds against the real console (`crate::
  fd::real_fd_of`, so a `dup2`'d pipe end still reports non-tty — load-bearing for `isatty()`).
  This is what lets `hush` reach a real `/ #` prompt with line editing.
- No pty/foreground-process-group layer at this file's level — `tcsetpgrp`/`bg`/`fg` are driven
  entirely by the real session/controlling-tty model at the process level (see "Session,
  controlling-tty, and login authentication" and "Real job control" below), not by anything here;
  this file's only involvement is the Ctrl+C/Ctrl+Z keyboard intercepts in
  `interrupts::keyboard_interrupt_handler`.

## Process abstraction, scheduler, and fork/exec/wait (`src/process/`)

Dynamically allocated process table, cooperative round-robin scheduler, kernel-thread-style
context switch between per-process kernel stacks. No preemption, no copy-on-write fork (full eager
copy), no SMP, no frame deallocation anywhere.

- **Process table is `Mutex<BTreeMap<Pid, Box<Process>>>`, `Box` is load-bearing** — a
  `BTreeMap`'s internal nodes can move on insert/remove, but a `Box`'s heap allocation never does;
  holding the table lock across a context switch would deadlock. Every function touching both the
  table and `scheduler::schedule()` drops the lock first.
- `context_switch::switch_context` only saves System V callee-saved registers + `RSP`. Two
  first-run trampolines: `spawn_trampoline_asm` (never-run process) and `fork_trampoline_asm`
  (forked child, jumps straight into `syscall_entry`'s GPR-pop/`sysretq` tail — `seed_fork_frame`
  places the copied `SyscallFrame` at the offset that tail expects).
- `fork` resumes the child via a copy of the parent's live `SyscallFrame` with `rax=0` and the
  copy's carry-flag bit explicitly cleared (the copied `r11`/RFLAGS is stale pre-syscall state).
- `do_execve` builds everything (new `AddressSpace`, `elf::load`, user stack/argv/envp/auxv)
  *before* mutating the live frame/`CR3`/stored `AddressSpace` — a failure at any point must leave
  the caller untouched, matching real `execve(2)`. `argv_ptr` supplies the complete `argv[]`
  including a real caller-chosen `argv[0]` (length-prefixed `RawArgvEntry{ptr,len}`, not NUL-
  terminated `char**`).
- **Real `#!interpreter [arg]` shebang support**: `elf.rs` has no shebang awareness and BusyBox
  `hush` has no `ENOEXEC`-fallback logic of its own (real Unix relies on the kernel's own
  `binfmt_script`). `do_execve` peeks the target's first two bytes; if `#!`, parses
  `interpreter` + one optional trailing argument (never further word-split, matching real
  `binfmt_script`), re-targets the open/read loop at the interpreter, looping up to
  `MAX_SHEBANG_DEPTH = 4` (past which `ELOOP`). `argv` rebuilds as `[interpreter, optional-arg?,
  script-path] + original-argv[1..]` (caller's own `argv[0]` discarded); `Process::comm` takes the
  interpreter's basename, not the script's — only the interpreter is ever really `exec`'d.
- Per-process state across `fork` (copied)/`execve` (mostly preserved, some reset): `cwd`
  (preserved by execve), `brk` (copied by fork, not reset by execve), `fs_base` (copied by fork,
  reset to 0 by execve), `pgid` (inherited by fork, untouched by execve), signal state
  (`sigactions` reset to `SIG_DFL` for caught handlers only on execve; `pending`/`blocked`
  untouched), `uid`/`gid` (copied by fork, preserved by execve — no setuid-bit execve support),
  `sid` (own leader at spawn, inherited by fork, untouched by execve), `rlimits`/`nice`/
  `sched_policy`/`sched_priority`/`umask` (all copied by fork, preserved by execve — stored, not
  enforced), `root_inode` for chroot (copied by fork, untouched by execve). Itimer state (`real_
  timer_deadline`/`interval_ticks`) is the one exception: reset (not inherited) by `fork`,
  preserved by `execve`.
- Kernel stack size floor is `128` KiB — found empirically (16 KiB overflowed `ls`'s call chain;
  32 KiB overflowed `fork`'s debug-build `PageTable::clone()` frame). No guard page — overflow
  corrupts silently.
- `gdt::set_kernel_stack` repoints `TSS.RSP0` on every context switch via a raw pointer (`spin::
  Lazy` has no `DerefMut`) — sound only because nothing else holds a live reference across the
  call (single-core, interrupts disabled during scheduling).
- **`do_wait4`'s reported status is real `wait(2)`-encoded — `oxidebsd_sys_exit` shifts a normal
  exit code into bits 8-15** (`WEXITSTATUS`'s bit position), not the raw code. The old unshifted
  convention was internally consistent but broke real `WIFSIGNALED`/`WTERMSIG`-checking code
  (BusyBox `hush`'s `checkjobs()`): any nonzero normal exit was misread as "terminated by signal
  N", printing spurious "Hangup" and corrupting later commands in the same session. Signal-based
  termination (`do_kill`'s `Terminate` branch, `SignalDelivery::Terminate`) already passes a
  pre-encoded `128 + sig` value directly and must **not** be shifted — its low 7 bits already equal
  the real signal number.
- **`kill(pid, 0)`** — the real POSIX existence-check convention — used to be a flat `EINVAL`
  (`do_kill` only accepted `1..=31`). Fixed: `sig == 0` does a real existence-only check (self or
  cross-process; a zombie still counts as existing until reaped), bypassing the pending-signal
  bitmask entirely.
- `tests/fork_wait.rs` + `userland/fork-exec-smoke/` covers fork/wait4/exit (no filesystem/execve).
  `modules/oxfs/src/test_busybox.sh` is real, broader, hand-run coverage.

## Dynamic kernel modules (`src/module.rs`, `modules/*`)

Loads independently-compiled, relocatable (`ET_REL`) `#![no_std]` objects into the kernel's
currently-active address space at boot: relocates them, resolves referenced symbols against a
small hand-curated kernel API table, calls `module_init`. Distinct from `elf.rs` (loads a
non-relocatable `ET_EXEC` binary with zero relocations) — this is the largest subsystem in the
kernel.

- Module crates are plain `#![no_std]` `lib` crates. `build.rs`'s `build_module_crate` runs `cargo
  rustc --release --lib -- --emit=obj` then a mandatory relocatable partial relink (`rust-lld
  -flavor gnu -r`) against the exact `core`/`alloc`/`compiler_builtins` `.rlib`s that build
  produced.
- `--gc-sections -u module_init` on that relink is **required, not optional** — coarse archive-
  member selection during `-r` linking otherwise pulls in entire bundled `core`/`alloc` object
  files (one indexing-triggered `panic_bounds_check` reference once ballooned a module to 3+
  MB/2900 sections and exhausted the boot-time heap parsing headers).
- `RUSTFLAGS="-C relocation-model=static"` keeps relocations to absolute 32-bit forms — every
  module must map inside the low 2 GiB (`MODULE_VA_BASE=0x10000000`,
  `MODULE_REGION_CEILING=0x80000000`). A few GOT-indirected references survive anyway — handled via
  a minimal, eagerly-populated per-relocation-site GOT the loader builds at load time.
- **No `core::fmt::Write`/`write!` in module code** — constructing that trait object's vtable emits
  a GOTPCREL reference, the single largest source of bloat before `--gc-sections`. Modules use
  hand-rolled byte formatting instead.
- **Modules can't use `alloc`/`Vec`/`BTreeMap`** — avoids depending on `#[global_allocator]`'s
  unstable-ABI internals from relocated code. State lives in fixed-size `static mut` arrays.
- **A distinct `static mut` gotcha from `gdt.rs`'s**: a private `static mut` buffer written by real
  Rust code but never observably read back through an externally-reachable function can have the
  write deleted as an unobservable dead store. Module state needs to be read back from an
  exported/syscall-reachable function to survive optimization.
- Modules are mapped kernel-only (no `USER_ACCESSIBLE`) and every page is `WRITABLE` (relocation
  must patch code bytes; no W^X anywhere in this kernel yet).
- A module panic is fatal to that call (no unwinding). `build.rs`'s `discover_panic_symbol` finds
  each module's toolchain-hashed panic-entry symbol, and the loader's resolver points it at
  `module_panic_trampoline`, which picks one of two outcomes **per-module** via a `fatal_on_panic:
  bool` passed to `module::load` (`false` for every module except `oxfs`). `false` just
  `hlt_loop()`s; `true` (`oxfs` only) reboots the whole system — with a real disk attached, a panic
  mid-mount/mid-format risks a torn superblock/inode-table write, worse to resume past than a
  purely in-memory panic. `module::CURRENT_MODULE_FATAL` (`static mut`, same single-core reasoning
  as `gdt.rs`'s `CURRENT_RSP0`) is set before every call into module code and read by the
  trampoline if that call panics; defaults fatal wherever the answer can't be determined.
- `serial_println!` can't take implicit `{name}`-style captures (its `concat!`-based expansion
  blocks it) — always use explicit positional args; `serial_print!` has no such restriction.
- Known limits: no module unload/reload, no versioning, no inter-module direct calls (only
  module→kernel, via each module's own resolved symbol table — why `src/fs/fd.rs`'s registry exists
  at all, as the only coordination point between e.g. `oxfs` and `native_abi`).

## Filesystem: oxfs (live) and FAT32 (superseded)

**`modules/oxfs/`** is the live filesystem — a real Unix-shaped inode/block filesystem. In-memory
by default, with real optional persistence to an attached ATA disk — see "Real disk persistence"
below. Fixed-size `static mut` pools: `NUM_BLOCKS=8192` × `BLOCK_SIZE=4096` (32 MiB total),
`MAX_INODES=1024`, each inode with 12 direct blocks + one single-indirect block (max **single-
file** size ~4 MiB, independent of `NUM_BLOCKS`). `NO_BLOCK = u32::MAX` is the "unallocated"
sentinel (block `0` is valid, unlike FAT32's cluster numbering). Directories are ordinary inodes
holding fixed 32-byte records (real names, `NAME_MAX=26`) that grow additional blocks on demand.
`unlink`/`rmdir` only clear a record's `used` byte (no dealloc, consistent with the rest of the
kernel). Root is fixed inode `0`, self-referencing `.`/`..`.

- Real multi-component path resolution (`resolve_path`/`resolve_parent`, handling `.`/`..`).
- Real **per-process** cwd: `Process::cwd` (opaque inode number). `oxidebsd_get_cwd`/`_set_cwd`
  resolve the current pid themselves; fall back to `BOOT_CWD` for pid `0` (module_init's own
  self-check, before any real process exists).
- Open files stream directly from the block chain on read (no whole-file buffering); writes
  accumulate in a fixed buffer (`MAX_WRITE_BUFFER=131072`) and commit to a real inode at `close`.
- Syscalls: `SYS_OPEN=5`, `SYS_CLOSE=6`, `SYS_CHDIR=12`, `SYS_MKDIR=136`, `SYS_GETCWD=108`,
  `SYS_UNLINK=109`, `SYS_RMDIR=110`, `SYS_RENAME=111` (4-arg, uses `R10`), `SYS_FSTAT=126`/
  `SYS_STAT=127`/`SYS_LSTAT=128` (byte-exact 144-byte musl `struct stat`, `MuslStat`), plus
  `SYS_GETDENTS=129` (real wire format, `d_type` from real inode kind, `d_off` a plain monotonic
  counter — no `telldir`/`seekdir` support needed by any ported applet). `st_uid`/`st_gid`/
  timestamps are real (see "Permission model"); `st_mode` permission bits are real per-inode now
  too (see same section) — `oxfs_lstat` doesn't follow a final symlink, `oxfs_stat` does.
- Seed files (`hello.txt`, `big.txt`, every BusyBox applet ELF, the full musl/tcc runtime tree) are
  embedded via `include_bytes!(env!(...))` in `module_init`, no build-time disk image needed.

**`modules/fat32/`** (superseded, kept for its own build/self-check, not loaded at boot):
8.3 names only, one path component per call, a directory that can never grow past its first
cluster, one **kernel-wide** cwd, whole-file-buffered reads capped at `MAX_FILE_BUFFER=131072`, no
`unlink`/`rmdir`/`rename`. Superseded once these limits started actively blocking BusyBox work.

**`src/fs/fd.rs`** (shared by both): a per-process `(Pid, fd)` scoped registry — the only
coordination channel between independently-loaded modules. Bump-allocated fd numbers, never reused
even after close.

## Real disk persistence (`src/drivers/ata.rs`, `modules/oxfs`)

OxideBSD's first real block device driver, and what lets oxfs survive a reboot. Scoped
deliberately: real disk I/O and oxfs mount/format persistence, not a general VFS/mount-table layer.

- **`src/drivers/ata.rs`**: a hand-rolled ATA PIO driver, kernel-resident (boot-wired from `main.rs`) —
  classic legacy IDE, LBA28, **polling only, no IRQ**. Fixed legacy ports (0x1F0/0x3F6 primary,
  0x170/0x376 secondary — QEMU's default `i440fx` PIIX3 IDE controller). Every BSY/DRQ wait is
  bounded by a real `crate::tsc`-based deadline (never `hlt()`, never unbounded) — load-bearing
  since these functions are reachable from inside a real syscall handler with interrupts masked
  (see "Real networking"'s `hlt()`-in-syscall freeze entry).
- **One fixed target: secondary channel, master** (`bootimage` attaches the boot image itself as
  the primary master by default). `oxidebsd_block_read`/`_write`/`_device_present` are the
  kernel-exported API `modules/oxfs` calls. `run-args` points at `target/oxfs_disk.img` (created
  only if missing — how it survives across `cargo run`); `test-args` points at
  `target/oxfs_test_disk.img` (always freshly zeroed) — only `tests/ata_smoke.rs`/
  `tests/oxfs_persistence_syscall_smoke.rs` call `ata::init()` at all, so every other test's
  `oxidebsd_block_device_present()` stays `0`.
- **On-disk layout**: physical block `0` is the superblock (magic `b"OXFS"` + version +
  `NUM_BLOCKS`/`MAX_INODES`/root-inode); the packed inode table follows (fixed 128-byte stride,
  `INODE_STRIDE`); then the block-used bitmap (one block); real data starts after that (oxfs block
  `i` maps to physical block `metadata_blocks + i`). **Never a raw transmute/memcpy of `Inode`**
  (not `#[repr(C)]`) — `pack_inode`/`unpack_inode` serialize by hand.
- **Mount-or-format, decided once in `module_init`**: no disk → today's in-memory-only behavior,
  unchanged. Disk attached, superblock magic **and** stored layout (`SUPERBLOCK_VERSION`/
  `NUM_BLOCKS`/`MAX_INODES`) match this build → **mount**: load bitmap + inode table wholesale,
  eager-load only the data blocks the bitmap marks used. Magic mismatch, or magic matches but
  layout doesn't (a disk formatted under an older `MAX_INODES`, etc.) → **format**: reset the real
  in-memory pool to all-free first (a stale bitmap/inode-table load must never leak into a fresh
  format — found live, was silently causing false `DiskFull` panics), then run the original
  seed-everything + self-check, then `flush_all_to_disk` writes everything out once so the *next*
  boot mounts. The self-check only ever runs on the format path.
- **Write-through persistence, centralized at three functions**: `write_block`, `write_inode`,
  `set_block_used` are the *only* functions that ever touch `BLOCKS`/`INODES`/`BLOCK_USED` — no
  dirty-tracking/sync-on-shutdown scheme needed.
- **`PERSISTENCE_READY`** (`static mut` gate): `false` for the entire duration of both formatting
  and mounting (both call the write-through functions heavily as part of loading/seeding, which
  must not each trigger a redundant disk write) — `module_init` sets it `true` once, right after
  its mount-or-format branch completes, before any real syscall becomes reachable.
- **Known, accepted limitation: mount-time load is bitmap-filtered, not true lazy fault-in** — the
  pinned `x86_64` crate (0.15.5) has no `rep insw`/`outsw` wrapper, so every 512-byte sector
  transfer is 256 individually-trapped port reads under QEMU's TCG.
- **`fatal_on_panic` stays `true` for oxfs** — see "Dynamic kernel modules" above.
- **No raw block device is exposed to userland** — the disk is purely internal to oxfs's own
  persistence.
- Verified via `tests/ata_smoke.rs` (direct sector read/write round-trip) and
  `tests/oxfs_persistence_syscall_smoke.rs` + `userland/oxfs-persistence-syscall-smoke/` (a real
  spawned ELF through genuine `SYSCALL`/`SYSRETQ`). **Not covered by automated testing**:
  persistence actually surviving a real QEMU restart — manual, user-driven only.
- **Operational gotcha, applies to any future fix touching seeded content** (a BusyBox applet,
  `tcc`, a fixture file): mounting never re-syncs seeded content against the kernel's current
  embedded bytes — only a fresh *format* does. A fix to seeded content needs the target disk
  actually deleted (`target/oxfs_disk.img`, destructive to anything created at the hush prompt —
  ask the user before doing this) and reformatted on the next `cargo run` to take effect.

## Mount table (`modules/oxfs/`)

A real, but deliberately scoped, mount table — `mount --bind`/`mount -t tmpfs` only, not a general
pluggable-filesystem-type VFS (there's exactly one real block device/filesystem, nothing else to
plug in).

- **A second, purely in-memory inode/block pool for tmpfs**: `BLOCKS`/`BLOCK_USED`/`INODES` are
  extended with a tail region (`TMPFS_NUM_BLOCKS=1024`/`TMPFS_MAX_INODES=128`, 4 MiB). Block
  allocation for a write picks the real vs. tmpfs pool via `inode_ensure_block_at`'s
  `inode_num >= MAX_INODES` test; new-inode allocation for `mkdir`/`symlink`/`O_CREAT` uses the
  shared `alloc_inode_in(parent)` chokepoint (found live: the three call sites used to call plain
  `alloc_inode()` unconditionally, so a file created *inside* a tmpfs mount got a real-pool inode
  anyway, wrongly persisted to disk). Never reclaimed on unmount (matches this module's "no
  deallocation anywhere" stance).
- **The mount table itself** (`MountEntry`/`MOUNTS`, `MAX_MOUNTS=8`): each entry records the real
  inode a mountpoint resolved to (shadowed while active) and where lookups redirect instead (source
  dir's inode for a bind mount, a fresh tmpfs root for a tmpfs mount). `resolve_path_impl` checks
  `active_mount_for` right after each component's `dir_lookup` — applies to every component, not
  just the last. Scanned from the end (LIFO stacking/unstacking).
  - Tmpfs mount root's `..` points at the mountpoint's real parent — `cd ..` escapes cleanly.
  - Bind mount reuses the source directory's own real inode directly — known limitation: `cd ..`
    from inside it follows the *source's* real parent, not the mountpoint's (no target applet needs
    this to be otherwise).
  - `st_dev` is `1` (real fs) or `2` (tmpfs pool) — a bind mount deliberately keeps `st_dev == 1`
    (same underlying superblock, matching real Linux), so `mountpoint` can't distinguish a
    bind-mounted directory from an ordinary one.
  - **The redirect only fires where `resolve_path_impl`'s per-component loop actually runs** —
    covers every intermediate component and, for handlers using `resolve_path` directly for the
    whole path, the final component too. It does **not** cover a handler using `resolve_parent` +
    its own bare `dir_lookup(parent, leaf)` (correct for `mkdir`/`symlink`'s EEXIST check or
    `unlink`/`rmdir`'s raw mutation, wrong for `open`'s "existing path" branch — found live, fixed
    with the same redirect applied to `oxfs_open`'s own lookup result specifically).
- **`SYS_MOUNT_BIND=174`/`SYS_MOUNT_TMPFS=175`/`SYS_UMOUNT2=176`**. Real `mount(2)` takes 5
  conceptual args; `third_party/musl/src/linux/mount.c` is patched to dispatch to one of these two
  based on its own `fstype`/`flags` args (the only two shapes BusyBox's `util-linux/mount.c`
  issues). Landed on real Linux's `create_module`/`init_module`/`delete_module` slots (174-176,
  long-obsolete on real Linux, unreferenced anywhere in this tree) rather than continuing past
  `SYS_UTIMENSAT=167` — those next three slots (168-170) are real, live `swapoff`/`reboot`/
  `sethostname` numbers, already-seeded BusyBox applets that would have been silently misrouted.
- **`/proc/mounts`** (`ProcSysFile::Mounts`): backed by no kernel FFI accessor — a local formatter
  produces standard mtab-shaped lines directly from the mount table's own state.
- Verified via `tests/mount_syscall_smoke.rs` + `userland/mount-syscall-smoke/`.
- **Not covered**: a real block-device-agnostic mount table (`pivot_root`/`switch_root`), anything
  needing a real partition table or multiple on-disk formats (`blkid`/`fdisk`/`fsck`/`mkswap`/...).

## Permission model (`src/process/`, `modules/oxfs/`, `modules/posix_compat/`)

Real uid/gid, real per-inode `mode`/`uid`/`gid`, real `chmod`/`chown`, real `open()` permission
enforcement. Scoped deliberately: only one uid has ever existed (root, `0`) until "Session,
controlling-tty, and login authentication" below adds a real second user.

- **`Process` gains `uid`/`gid`** — no separate saved/effective pair (no setuid-bit execve
  support, so real vs. effective never diverges). `0` at spawn; copied by fork; preserved by
  execve.
- New syscalls: `SYS_GETUID=158`/`SYS_GETEUID=159`/`SYS_GETGID=160`/`SYS_GETEGID=161`/
  `SYS_SETUID=162`/`SYS_SETGID=163`/`SYS_GETGROUPS=164` (`posix_compat`) and `SYS_CHMOD=165`/
  `SYS_CHOWN=166` (`oxfs`). `chmod`/`chown` needed the open/chdir/rename-style argument-convention
  patch on the musl side (`(path_ptr, path_len, mode)`/`(path_ptr, path_len, uid, gid)` — `chown`
  is the second syscall, after `rename`/`symlink`, needing all four ABI registers, using `R10` for
  `gid`).
- **`process::do_setuid`/`do_setgid`**: real POSIX rule — root (`uid==0`) may become any uid/gid;
  anyone else may only "become" the uid/gid they already are (a real no-op success, not privilege
  escalation); any other target is `EPERM`.
- **`process::do_getgroups`** reports a single-element list (the caller's own `gid`) for both the
  `size==0` and `size>=1` call shapes — no supplementary-group concept exists.
- **`modules/oxfs`'s `Inode` gains real `mode`/`uid`/`gid`** (default `FIXED_PERM=0o755`/`0`/`0`).
  `write_stat` is now backed by these. A freshly **created** file is owned by its real creator, not
  always root (`OpenFile::Write` gained `owner_uid`, captured at `open()`, applied at `close()`).
- **`check_access(inode, uid, gid, want_write)`**: `uid==0` bypasses read/write bits entirely;
  otherwise picks owner/group/other rwx by comparing against the inode's own `uid`/`gid` (first
  match wins). Wired into `oxfs_open`: an existing file/dir needs the real bit its
  `O_WRONLY`/`O_RDWR`/`O_RDONLY` intent asks for; creating a new file needs write permission on the
  *parent directory*. `do_execve`'s ELF-loading read goes through this same path, always
  read-only, doubling as an approximate execute-permission check — harmless while every seeded
  file's default mode (`0o755`) sets both bits identically.
- **Real write-to-an-existing-file support**: `oxfs_open` used to always return a read-only fd for
  a path that already existed, regardless of open flags — a file could only ever be written once
  in its whole lifetime. `OpenFile::Write` gained `existing_inode: Option<u32>`: `None` is
  create-new (fresh inode + new dir entry at close); `Some(inode)` overwrites that inode's content
  in place. `O_APPEND` preloads the write buffer with the file's real existing content;
  `O_WRONLY`/`O_RDWR` (with or without `O_TRUNC`) starts empty — this filesystem's only write
  primitive always replaces a file's complete contents in one shot, so "open for writing but don't
  truncate until the first write" isn't distinguishable, and nothing needs it to be. Opening a
  directory with `O_WRONLY`/`O_RDWR` is a real `EISDIR`.
- **`oxfs_chmod`**: owner or root only; follows a final symlink. **`oxfs_chown`**: root-only
  unconditionally (no group-membership concept); supports real POSIX `(uid_t)-1`/`(gid_t)-1`
  "leave unchanged"; follows a final symlink (`lchown` unimplemented, unneeded).
- **`oxidebsd_current_uid`/`_gid`** (exported to modules) — how oxfs learns the caller's identity
  (modules can't call each other directly). `pid == 0` reports root (module's own boot self-check).
- **`/etc/passwd`/`/etc/group`** — seeded alongside `/etc/resolv.conf`: a single `root:x:0:0:root:
  /:/bin/sh` / `root:x:0:` entry. No new syscall needed for `whoami`/`id` — musl's own
  `getpwuid`/`getpwnam`/`getgrgid`/`getgrnam` parse these files directly via plain `fopen`/`fgets`.
- Verified via `tests/uid_syscall_smoke.rs`/`userland/uid-syscall-smoke` — root identity +
  getgroups, a chmod/chown/stat round trip, a forked child that `setuid(1)`s, confirms `setuid(0)`
  now `EPERM`s, confirms `setuid(1)` (self) still succeeds, then attempts `open` on a `0o600` file
  owned by a different uid (the one real access-denial check in the test).
- **Not covered by this pass**: real login/session auth (`su`/`login`/`sulogin`/`getty` — see
  below), mutating `/etc/passwd`/`/etc/group` (applet-level gap, both files already real/writable),
  `lchown`/`fchown`, setuid/setgid/sticky bits (`oxfs_chmod` masks input to `0o777`). `fchmod` is
  done (`modules/oxfs`'s `oxfs_fchmod`, registered directly at real Linux's own unremapped
  `__NR_fchmod = 91` — found live post-v0.1: `uudecode` restores a decoded file's mode via a real
  `fchmod(fd, mode)` call on its still-open output fd, not a path-based `chmod()`).

## Session, controlling-tty, and login authentication (`src/process/`, `src/console/stdin.rs`, `src/cpu/interrupts.rs`, `modules/posix_compat/`, `modules/oxfs/`)

Closes `su`/`login`/`sulogin`/`getty`. Split: `su`/`login` needed only a real second user + real
password verification; `sulogin`/`getty` additionally needed a real session/controlling-tty/
foreground-process-group model.

- **A real second user**: `/etc/passwd` gains `user:x:1000:1000:User:/home/user:/bin/sh` (real
  `/home/user`, owned `1000:1000`, mode `0700`) — a root-only passwd could never exercise real
  auth, since both `su`/`login` skip the password check entirely when the caller is already root.
  **A real `/etc/shadow`** (mode `0600`, root-owned) holds real SHA-512 (`$6$`) `crypt(3)` hashes
  for both accounts (password equals username). `third_party/musl/src/crypt/*.c` needed zero code
  changes — plain portable C with no SIMD, already worked once real accounts existed to exercise
  it.
- **A real session model**: `Process` gains `sid: Pid` (same shape as `pgid`) — a spawned process
  becomes its own session leader; forked child inherits parent's `sid`; execve leaves it untouched.
  Two new **single, not per-session**, globals in `src/console/stdin.rs`: `CONTROLLING_SESSION:
  Option<Pid>`, `FOREGROUND_PGID: Option<Pid>` — this kernel has exactly one real console.
  - **`SYS_SETSID=112`** (real x86_64 Linux's own value). `process::do_setsid` enforces `EPERM` if
    the caller is already a process-group leader; on success makes it leader of a fresh
    session+pgroup at once.
  - **`SYS_GETSID=177`** (invented — real Linux's `124` already means `SYS_IOCTL` here). Exists for
    `getty`'s real fallback: when `setsid()` fails (the common case when launched as a job-control
    shell's foreground child, real documented BusyBox behavior), it calls `getsid(0)` to double
    check.
  - **`SYS_IOCTL` gains `TIOCSCTTY`/`TIOCNOTTY`/`TIOCGPGRP`/`TIOCSPGRP`**, gated like `TCGETS` (real
    console fd only). `TIOCSCTTY` requires session-leader status unless `force` is set (no
    capability model to gate `force` itself, same "collapses to always-allowed" reasoning as
    `do_setpgid`). `TIOCGPGRP`/`TIOCSPGRP` gated on the caller's session owning the controlling tty;
    `TIOCGPGRP` falls back to the session id itself when nothing's called `TIOCSPGRP` yet (real Unix
    convention — verified against `hush.c`'s own job-control startup as what avoids a spurious
    `SIGTTIN` retry loop).
  - **Real Ctrl+C → `SIGINT` to the foreground process group**: `interrupts::
    keyboard_interrupt_handler` intercepts ASCII ETX (`0x03`) before the stdin ring buffer, only
    when `ISIG` is set **and** `FOREGROUND_PGID` has actually been claimed. `FOREGROUND_PGID` *is*
    claimed for the common interactive case now — see "Real job control" below for how pid 1 gets
    a controlling tty automatically and what that unlocks; `process::signal_foreground_group`
    reuses `do_kill`'s own Discard/Terminate/Stop/SetPending logic, applied to every process
    sharing that `pgid`.
  - Ctrl+Z/`SIGTSTP`-driven stop/continue is covered by "Real job control" below, not here — this
    bullet originally shipped Ctrl+C only; that section is the up-to-date reference for both.
  - Verified via `tests/session_syscall_smoke.rs` + `userland/session-syscall-smoke/`, run as a
    *forked child* of pid 1 (pid 1 is already its own pgroup leader, so `setsid()` on it directly
    would always `EPERM` before reaching anything interesting). Real Ctrl+C delivery itself is
    manual-QEMU-only (driven by a real PS/2 IRQ, unscriptable).

**Two real bugs found live-testing `su` interactively, both fixed, both worth remembering for any
future syscall-number choice**:
1. **A real syscall-number collision**, independent of the session work above: this ABI's own
   invented `SYS_KILL` number happened to equal real Linux's still-inert `setgroups` number, which
   *does* have a live musl caller (`initgroups()` → `setgroups()`, called unconditionally by `su`).
   A real `setgroups()` call didn't cleanly `ENOSYS` — it silently invoked the real `kill(2)`
   handler, reinterpreting `(count, gid_list_ptr)` as `(pid, sig)` (harmless here only by luck, a
   real latent landmine). Fixed by giving `setgroups` its own number (`SYS_SETGROUPS=178`) with a
   real handler (root-only genuine no-op; `EPERM` otherwise — no supplementary-group concept to
   populate). **Confirms the syscall-ABI section's own rule above: always grep every real-Linux
   inert value in `bits/syscall.h.in` for a live musl caller before reusing a number.**
2. **The already-documented `ENOSYS` mismatch (see Syscall ABI above), found concretely breaking
   real functionality**: BusyBox's `change_identity()` has a real upstream fallback — if
   `initgroups()` fails with real `ENOSYS` *and* the target uid already equals the caller's, treat
   it as a harmless no-op (root→root case for `su`). Never fired because this kernel's `ENOSYS`
   (FreeBSD's `78`) didn't match musl's compiled-in value (`38`). Fixed by correcting the
   constant — deliberately scoped to just this one, not a preemptive sweep of the other still-
   flagged errno mismatches.

## Signal handling module (`modules/signal/`, `src/process/signals.rs`, `src/syscall/mod.rs`)

Real `kill(2)`/`sigaction(2)`/`sigprocmask(2)` + delivery (handler invocation + `sigreturn`).
`SYS_KILL=116`/`SYS_SIGACTION=117`/`SYS_SIGPROCMASK=118`/`SYS_SIGRETURN=119` — all four happen to
match real Linux/BSD wire formats, so the musl patch is a pure 4-line number remap (plus one
hardcoded restorer-stub literal, `src/signal/x86_64/restore.s`). Real signal numbers (`SIGHUP=1`
...`SIGSYS=31`, no realtime signals).

- `Process::sigactions: [SigAction; 32]` (real `SIG_DFL=0`/`SIG_IGN=1`) plus `pending_signals`/
  `blocked_signals` bitmasks and one `signal_saved_frame` snapshot (not a real signal stack — a
  second signal during handler execution overwrites the snapshot rather than nesting; known gap).
- Delivery happens once, at the tail of `syscall_dispatch`. `sigreturn` bypasses the normal
  `Ok`/`Err` carry-flag rewrite entirely (must restore an arbitrary saved `CF`) — the one syscall
  number not registered in `SYSCALL_TABLE` at all.
- `do_kill` cross-process: immediate for the common case (no handler → terminate right there, even
  against a blocked target); deferred until next-scheduled only if the target has a custom handler.
  No permission checks. **Real process-group targeting** (`target_pid == 0`/`< 0`, POSIX
  `kill(-pgrp, sig)`) — see "Real job control" below.
- Only 1-argument `void (*)(int)` handlers — no `SA_SIGINFO`.

## Real job control: Ctrl+C/Ctrl+Z, colored tty, `kill(-pgrp)` (`src/process/`, `src/cpu/interrupts.rs`, `build.rs`)

Closes the "Session, controlling-tty..." section's own `FOREGROUND_PGID`/Ctrl+C machinery being
practically dead code, and the `tcsetpgrp`/real-job-control gap-analysis row below (previously
"blocked on a pty/foreground-pgrp concept" — it wasn't; see the root-cause finding directly below).
Landed on both `master` and `v0.1.x`, split by scope: both branches get real Ctrl+C, colors, and
`kill(-pgrp)`; only `master` (0.2.0-dev) gets real Ctrl+Z suspend/resume — `v0.1.x` stays
bugfix-only per its own branch charter (see "v0.2.x goals" in `ROADMAP.md`).

**Root cause, and why the fix needed no BusyBox source patch**: `third_party/busybox/shell/hush.c`
(`CONFIG_HUSH_JOB`/`CONFIG_HUSH_INTERACTIVE` already forced on for `hush` in `build.rs`, see the
BusyBox port section above) has always shipped a complete, real job-control startup sequence
(`tcgetpgrp`/loop-until-foreground/`bb_setpgrp`/`tcsetpgrp`) that activates itself automatically —
*if* it ever discovers it has a controlling tty. It never did: pid 1's stdin/stdout are wired
directly to the console in `process::spawn`, never through a real `open()` syscall — the exact path
that would, on a real kernel, auto-associate a session leader's first tty use as its controlling
terminal. **Fix**: `process::spawn` now calls `crate::console::stdin::set_controlling_session(pid)`
directly right after inserting pid 1 into the table (kernel-internal, bypassing the `ioctl` path),
mirroring what a real kernel does for a console-attached init process. That one call is what makes
`FOREGROUND_PGID` actually get claimed (via `hush`'s own subsequent real `TIOCSPGRP` call), which is
what `interrupts::keyboard_interrupt_handler`'s pre-existing Ctrl+C interception was gated on all
along.

- **Colors**: `TERM=linux` and a colored `PS1` (`\[\e[1;32m\]\u@\h\[\e[0m\]:\[\e[1;34m\]\w\[\e[0m\]\$
  `) added to pid 1's `envp` in `process::spawn` (previously just `PATH=/bin`) —
  `CONFIG_FEATURE_EDITING_FANCY_PROMPT` was already forced on for `hush`, so the `\[`/`\]`/`\u`/
  `\h`/`\w`/`\$`/`\e` escapes `lineedit.c`'s own `parse_prompt` expands already worked, this just
  needed the env var. Real `ls --color` needed its own per-applet Kconfig flip in `build.rs`
  (`CONFIG_LONG_OPTS`/`CONFIG_FEATURE_LS_COLOR`/`CONFIG_FEATURE_LS_COLOR_IS_DEFAULT` — same
  "`allnoconfig` writes an explicit not-set line before the parent symbol is even visible" story as
  every other per-applet flip in that function). This BusyBox fork's `grep` has no color feature at
  all — not in scope.
- **Real `kill(-pgrp, sig)` process-group broadcast** (`process::do_kill`'s `target_pid <= 0`
  branch — `0` = caller's own group, `< 0` = group `|target_pid|`): previously an unconditional
  `EINVAL` (documented as "no process-group/broadcast targeting" in the Signal handling module
  section above) — found live, not preemptively fixed: `hush`'s own `fg`/`bg` builtins
  (`kill(-pgrp, SIGCONT)`) and its job-cleanup path (`kill(-pgrp, SIGHUP)`/`kill(-pgrp, SIGCONT)`)
  both depend on it, and were unreachable dead code until the controlling-tty fix above made
  `hush`'s job control activate for the first time. Reuses `signal_foreground_group`'s exact
  per-process action resolution unchanged — it already only needed a plain `pgid`, "foreground" was
  never load-bearing to its own logic, only to its one prior caller.
- **Master-only: real `SIGSTOP`/`SIGTSTP`/`SIGCONT`** (genuine Ctrl+Z suspend/`bg`/`fg` resume, not
  just backgrounding-while-still-running):
  - **`ProcState::Stopped(u64)`** (payload = stopping signal, for `WSTOPSIG`) — a new top-level
    `ProcState` variant, deliberately **not** nested under `BlockReason`: every existing block/wake
    site (`console::stdin::read`, `fs::pipe`, `do_wait4`) already re-checks its own real condition
    fresh after `scheduler::schedule()` returns rather than trusting "state==Ready now" to mean "my
    specific event happened," so resuming a stopped process is just flipping `state` back to
    `Ready`/re-enqueueing — nothing needs to remember which `BlockReason` (if any) it had. A
    cross-process stop targeting a `Ready` process still sitting in `READY_QUEUE` also needs
    dequeuing first (new `scheduler::remove_ready`), or the scheduler would run it next turn
    regardless of the state flip.
  - **`DefaultDisposition::Stop`** (`SIGSTOP`/`SIGTSTP` split out of the old blanket `Ignore`
    bucket — `SIGCONT` stays `Ignore`, see below). `SIGSTOP` is always immediate and uncatchable
    (`sys_sigaction` already rejected installing a handler for it, so `default_disposition` is
    always consulted); default-disposition `SIGTSTP` is also immediate, matching real Ctrl+Z. A
    `SIGTSTP` with a real installed handler still falls into the pre-existing `Handler`/
    `SetPending` arms unmodified — `hush.c` itself installs a real `SIGTSTP` catch-handler for the
    interactive shell process, so Ctrl+Z with no foreground job correctly does *not* stop the shell,
    free from this design.
  - **`SIGCONT`** gets its own pre-dispatch step at every cross-process-capable call site, *before*
    the normal disposition lookup: an actually-`Stopped` target always resumes (state → `Ready`,
    `Process::cont_notify_pending = true`) regardless of its own `SIGCONT` disposition, real POSIX
    semantics, then still falls through to the normal dispatch for whether a caught handler
    *additionally* fires. `Process::stop_notify_pending` is cleared the moment `SIGCONT` resumes a
    process — a deliberate simplification from real Linux (which would still let a parent observe
    the stop after the fact post-resume): `ProcState::Stopped`'s payload is the only place the
    stopping signal number lives, and it's gone the instant `state` flips back, so reporting a stop
    after resume would need a second field just to remember which signal it was — not worth it for a
    real shell's own polling cadence.
  - **`do_wait4` real `WUNTRACED`/`WCONTINUED`/`WNOHANG`** — `oxidebsd_sys_wait4` used to discard
    `options` entirely. `WNOHANG` is the load-bearing half, not just the stop/continue reporting:
    `hush.c`'s own `checkjobs(NULL, 0)` (background job-status polling, called throughout its
    interactive main loop) always passes `WUNTRACED | WNOHANG`, and this kernel never delivers a
    real `SIGCHLD` to a parent at all — without real `WNOHANG`, any such call would block the whole
    interactive shell instead of just missing a status update. New wire status shape (confirmed
    disjoint from the two already in use — normal exit's `(code & 0xff) << 8` always has low byte
    `0x00`, signal-termination's `128 + sig` always has low byte `0x81..0x9f`): `WIFSTOPPED` writes
    `0x7f | (stopsig << 8)`; `WIFCONTINUED` writes the literal `0xffff`.
  - **Ctrl+Z keyboard intercept**: `interrupts::keyboard_interrupt_handler` gained a second
    intercepted byte (ASCII SUB, `0x1a`), mirroring the existing Ctrl+C block exactly (same `ISIG`/
    `FOREGROUND_PGID` gating) but sending `SIGTSTP` instead of `SIGINT`.
  - **A real regression found live, not by review**: `process::timers::do_nanosleep` was the one
    blocking call in this codebase that didn't loop and re-check its own wake condition after
    `scheduler::schedule()` returns (every other one — pipe reads, `do_wait4`, stdin — already did).
    It got away with that because historically the *only* thing that ever set a sleeping process
    back to `Ready` was the timer IRQ handler itself finding the deadline actually passed. Real
    `SIGCONT` broke that assumption silently: it unconditionally wakes a `Stopped` process
    regardless of what it was blocked on, so `bg`-ing a Ctrl+Z-stopped `sleep 100` woke it almost
    immediately instead of at its real ~100s deadline — found by the user noticing the timing was
    off, in an already-clean `cargo build`/`cargo clippy`. Fixed by looping and re-checking
    `ticks() < deadline`, the same self-correcting pattern as everywhere else — which also gets real
    Linux semantics for free: `ticks()` is a global counter that keeps advancing while a process is
    `Stopped`, so time spent stopped still counts toward the sleep. **Any future mechanism that can
    force an arbitrary process back to `Ready` cross-process needs the same audit**: every
    non-looping `scheduler::schedule()` call site must be re-checked for this exact assumption.
  - Not covered: real `SIGTTIN`/`SIGTTOU`-driven job control (still `Ignore` disposition — only
    `SIGINT`/`SIGTSTP` are ever delivered to a foreground group).
- **Verification**: `cargo build`/`cargo clippy` clean on both branches (no new warnings). Ctrl+C/
  Ctrl+Z/`fg`/`bg`/`jobs`/colored `ls`/prompt all live-verified interactively by the user
  (manual-QEMU-only, real PS/2 IRQ, unscriptable — same as every other Ctrl+C-shaped feature in this
  codebase). No new automated smoke test for the stop/continue machinery specifically, given the
  Ctrl+Z trigger is inherently interactive — a `SYS_TEST_*`-style synthetic-signal-injection test
  (same pattern as the existing `*_syscall_smoke` tests) could exercise
  `do_kill(SIGSTOP)`/`wait4(WUNTRACED)`/`SIGCONT` without a real keypress if this needs regression
  coverage later.

## Real-time clock (`modules/clock/`, `src/cpu/pit.rs`, `src/cpu/rtc.rs`, `src/syscall/`)

`SYS_CLOCK_GETTIME=138` — real `clock_gettime(2)`'s exact wire format, only the number needed
remapping. `time()`/`gettimeofday()` are plain musl wrappers around it, so one remap unlocks both.

- **`src/cpu/pit.rs`** reprograms the 8253/8254 PIT channel 0 to a fixed `TIMER_HZ=100` at boot —
  before this, `TICKS` incremented at the BIOS power-on default (~18.2 Hz), never a rate this
  kernel actually configured. 100 Hz, not 1000, deliberately (every extra IRQ has real overhead
  under QEMU's software TCG).
- **`src/cpu/rtc.rs`** reads the CMOS/MC146818 RTC fresh on every `CLOCK_REALTIME` request. Doesn't
  wait out an in-progress RTC update (rare, self-correcting) and assumes the 21st century. `tv_nsec`
  is always `0`.
- `CLOCK_MONOTONIC` converts `ticks()` against `TIMER_HZ`. Any other `clockid` is `EINVAL`.
- **`SYS_NANOSLEEP=139`** — `nanosleep(2)`'s exact wire format (`sleep()`/`usleep()` are wrappers).
  `do_nanosleep` converts to an absolute wake-up tick deadline, blocks
  (`ProcState::Blocked(BlockReason::Sleeping(deadline))`), calls `scheduler::schedule()` — woken by
  `interrupts::timer_interrupt_handler` scanning `process::table()` on every tick. Rounds up to a
  whole tick; `{0,0}` returns immediately. `rem_ptr` always zeroed (no signal-interrupts-sleep path
  exists). Doesn't implement `clock_nanosleep(2)`'s absolute-deadline/other-clock cases.

## Real networking (`src/drivers/pci.rs`, `src/net/*`, `modules/net/`)

A real, phased networking stack: PCI enumeration, an IRQ-driven rtl8139 driver, Ethernet/ARP/
IPv4/ICMP, UDP/TCP sockets, raw ICMP sockets, `poll(2)`, and no DNS protocol code of its own — real
hostname resolution works by making musl's own stub resolver (`third_party/musl/src/network/`)
function correctly over this ABI.

- **`src/drivers/pci.rs`**: legacy I/O-port config-space access, flat scan of all 256 buses (QEMU puts
  everything on bus 0).
- **`src/net/rtl8139.rs`**: brought up unconditionally at boot, absence logged not fatal. `src/
  interrupts.rs`'s generic `IRQ_HANDLERS` table (IRQ2-15) lets a driver whose IRQ isn't known until
  PCI probe time still claim a vector.
- **`src/net/{ethernet,arp,ipv4,icmp}.rs`**: real frame/packet construction and checksums, no
  fragmentation, no IP options. `ipv4::next_hop` is the *only* routing rule (send anything outside
  `GUEST_IP`'s `/24` to `GATEWAY_IP`'s MAC — QEMU SLIRP only answers ARP for its own virtual IPs).
- **`src/net/udp.rs`, `src/net/tcp.rs`**: real sockets behind `SYS_SOCKET=140`/`SYS_BIND=141`/
  `SYS_SENDTO=142`/`SYS_RECVFROM=143`/`SYS_SETSOCKOPT=144` (UDP) and `SYS_CONNECT=145`/
  `SYS_LISTEN=146`/`SYS_ACCEPT=147` (TCP; once `Established`, plain `SYS_READ`/`SYS_WRITE`). TCP is
  stop-and-wait (one segment in flight, fixed 536-byte MSS, no window/congestion control, no
  TIME_WAIT). `oxidebsd_sys_socket` masks off `SOCK_CLOEXEC`/`SOCK_NONBLOCK` before matching.
- **`src/net/icmp.rs`**'s raw sockets (`SOCK_RAW`+`IPPROTO_ICMP`) exist for real `ping`. Not
  port-addressed — every inbound ICMP fans out to every open raw socket (app filters by
  `icmp_id`/type itself, matching real `ping.c`), delivery includes the real IP header prepended.
- **`SYS_POLL=148`**: added to unblock musl's real DNS resolver. Real Linux's `__NR_poll` (`7`)
  collides with this ABI's own `SYS_WAIT4`, so remapped to `148`. Reports `POLLIN` only; an fd not
  owned by udp/tcp/icmp is always reported ready.
- **Real DNS resolution**: `/etc/resolv.conf` seeded with `nameserver 10.0.2.3` (SLIRP's DNS
  relay). Needed `recvmsg`/`sendmsg` patched to delegate to `recvfrom`/`sendto` for the single-
  iovec/no-ancillary-data shape musl's resolver actually uses (multi-iovec/control-message callers
  get a clean error — unneeded by this port).

**Architectural gotchas, both real and both apply to any future syscall-reachable busy-wait**:
1. **QEMU needs `-accel kvm -accel tcg`** (two repeated flags — this QEMU build rejects
   `kvm:tcg`) in both `run-args`/`test-args`, or every boot runs pure-software TCG (can stretch a
   ~4s boot past a minute under host load — looks like a hang, isn't). Falls back to `tcg` cleanly
   with no `/dev/kvm`.
2. **`hlt()` inside a syscall handler can freeze the CPU permanently.** `SFMASK` clears
   `RFLAGS::INTERRUPT_FLAG` for a syscall's *entire* duration — `hlt()` only wakes on an unmasked
   interrupt/NMI, and no timer tick can fire to advance `ticks()` either, so a tick-bounded deadline
   can never even be re-evaluated. Any syscall-reachable retry loop must use
   `core::hint::spin_loop()`, never `hlt()`, and must gate its deadline on **`src/cpu/tsc.rs`**
   (`RDTSC`-based, calibrated once at boot against `TIMER_HZ`, immune to `IF`) — **never
   `crate::interrupts::ticks()`**, which is frozen for a syscall's whole duration and can never
   elapse a deadline checked from inside one (found live: `poll()` hung solid on what should have
   been a 200ms timeout). Current spin-loop-with-tsc-deadline call sites: `ipv4::
   resolve_with_retry` (ARP wait), `tcp::oxidebsd_sys_connect` (handshake wait), `net::
   oxidebsd_sys_poll`. **This entire class of bug was invisible to every test that calls kernel
   handlers as plain Rust functions instead of through a real `SYSCALL`** — see the Test
   architecture section's own rule above.
3. **`tcp_read` blocks on spin-loop, deliberately not `crate::pipe`'s `BlockReason`/
   `scheduler::schedule()` pattern** — incoming-packet processing is pull-based, driven only by
   whichever process happens to call `net::poll()`; yielding to the scheduler here would mean
   nothing services this connection once the only process that cares about it stops running (a
   real permanent hang). `tcp_read` only returns real EOF (`0`) once the peer has actually FIN'd
   (`ConnState::CloseWait`/`FinWait2`/`Closed`), not merely when the buffer is momentarily empty —
   the old behavior looked like an immediate peer close to any real remote (not synthetic-peer)
   TCP exchange.

Real-`SYSCALL` counterparts exist for every network smoke test (`tests/{udp,poll,ping,socketpair,
tcp}_syscall_smoke.rs` + `userland/*-syscall-smoke/`), each spawning a small ELF as pid 1 driving
the scenario through genuine `SYSCALL`/`SYSRETQ`, using test-only syscalls (`SYS_TEST_EXIT=9999`,
`SYS_TEST_INJECT_UDP_FRAME=9998`, `SYS_TEST_TCP_STEP=9997`) to script inbound-packet/handshake
triggers without the functions under test ever running outside a real syscall.

**Known gaps, current**:
- `alarm()`/`setitimer()` — done. `SYS_SETITIMER=156`/`SYS_GETITIMER=157` (`modules/clock/`), a
  per-process `real_timer_deadline`/`real_timer_interval_ticks` pair, checked by the timer IRQ
  handler. Only `ITIMER_REAL` (`EINVAL` for `VIRTUAL`/`PROF` — no per-process CPU time tracked).
  Expiry only sets `pending_signals` (not the stronger immediate-termination `do_kill` cross-
  process path uses) — the timer IRQ already holds `process::table()`'s lock, re-locking would
  deadlock. Not inherited by fork; preserved across execve.
- `socketpair(AF_UNIX, SOCK_STREAM, ...)` — `SYS_SOCKETPAIR=149` (`posix_compat`), built on
  `src/fs/pipe.rs`'s existing blocking-buffer machinery (two cross-wired buffers, not a real `AF_UNIX`
  abstraction). Getting `wget` HTTPS working end-to-end through this needed five real fixes in
  sequence, each found only after live-retesting the previous one: `SYS_SET_TID_ADDRESS=150`
  (every musl program calls this at startup/after fork — was silently `ENOSYS`); `SYS_FCNTL=151`
  (`F_GETFL`/`F_SETFL`(`O_NONBLOCK` only)/`F_SETFD`(no-op)/`F_DUPFD`/`F_DUPFD_CLOEXEC` — only
  `crate::pipe::blocking_read` honors `O_NONBLOCK`, tracked per-`real_fd` via `crate::
  fd::is_nonblocking`); `SYS_SHUTDOWN=152` (real half-close for a `crate::pipe`-backed socketpair
  endpoint only, `ENOTSOCK` otherwise — forced `src/fs/pipe.rs`'s `close_direction` to stop
  `.expect()`-panicking on an already-removed buffer); a synthetic `/dev/{u}random,null,zero`
  path (`modules/oxfs`'s `dev_open`) backed by **`src/random.rs`** — a real SHA-256(RustCrypto
  `sha2`)-seeded ChaCha20(RustCrypto `chacha20`) generator, not a throwaway PRNG (deliberate: gathers
  `RDTSC`/PIT/RTC/a call counter/a stack address/`RDRAND`+`RDSEED` when available, hashes into a
  32-byte key, generates output as a ChaCha20 keystream under an all-zero nonce — safe since a key
  is never reused, guaranteed by a strictly-monotonic call counter folded into every seed). **Also
  gains a real, persistent `ENTROPY_POOL`** (`spin::Mutex<[u8;32]>`, `without_interrupts`-guarded
  against the single-core IRQ-vs-syscall-context deadlock the same way `scheduler.rs`/`vga.rs`
  already are) — the original per-call-only design left the generator with almost nothing
  genuinely hard to predict under QEMU/TCG without `RDRAND` (RTC has 1s granularity, PIT ticks are
  a fixed 100 Hz, the stack-address sample is near-constant call to call). `mix_entropy` folds real
  externally-triggered IRQ timing jitter (the exact `RDTSC` value a keyboard or rtl8139 IRQ
  happened to land at) into the pool from those two handlers' own call sites — deliberately not the
  periodic timer IRQ, too predictable and too hot to be worth the lock traffic. `gather_seed` folds
  the pool into every output and re-mixes it with a fresh `RDTSC` sample in the same critical
  section, so it keeps evolving even across calls with no intervening IRQ activity. Still an honest
  gap: a pool starts all-zero, so the very first read at boot (before any keystroke/packet) still
  leans on the original weak per-call sources — this raises the floor for sustained use, not that
  first call. **`RDRAND`/`RDSEED` are only trusted on real bare metal** — both VT-x and SVM let a
  hypervisor trap either instruction and substitute any value it wants while the guest still sees
  a clean success, undetectable from inside the guest; `CPUID` reporting the instruction supported
  only proves the physical CPU has it, not that the specific hypervisor is passing it through
  honestly. `running_under_hypervisor()` checks the standard "hypervisor present" bit (`CPUID`
  leaf `1`, `ECX` bit `31`) and both `rdrand_available`/`rdseed_available` treat the instruction as
  unavailable whenever it's set — under any detected hypervisor, including this project's own QEMU
  dev/test loop (QEMU sets this bit regardless of the `-accel kvm`/`-accel tcg` backend), the
  `ENTROPY_POOL` IRQ-jitter accumulation is the real floor instead, never a possibly-fabricated
  hardware DRNG value. Both crates need `default-features = false` + soft-float-equivalent backend flags for
  this SSE-disabled target (`sha2`'s `force-soft` feature, `chacha20`'s `--cfg
  chacha20_backend="soft"` via `.cargo/config.toml` rustflags). Finally, a real `tcp_read` EOF-vs-
  empty bug (see gotcha 3 above). `SYS_READV=153` (`native_abi`, mirrors `SYS_WRITEV` for the
  buffered-`fread`/`fgets` read path). Confirmed live end-to-end: a full HTTPS download via `wget`.
- No real routing table, no IPv6 anywhere in the stack.
- BusyBox's vendored TLS client (`networking/tls.c`) does not validate certificate chains — a
  limitation of that vendored code itself, not fixable kernel-side.

## Filesystem/process misc syscalls: fsync, ftruncate, fallocate, flock, statfs, prlimit64, nice, chrt, reboot (`modules/oxfs`, `modules/posix_compat`, `src/reboot.rs`)

Closes most of the `NEEDS_SYSCALL` gap-table row: `fsync`/`sync`/`truncate`/`fallocate`/`flock`/
`df`'s `statvfs()`/`nice`/`chrt`/`halt`/`poweroff`/`reboot`. Deliberately scoped — `link`/`mknod`/
SysV IPC/`chroot`/namespaces/`inotify`/ext2 `ioctl`s/`xattr` are a distinct, unstarted (`link`/
`mknod`/`chroot` since done — see below) gap; namespaces don't fit this kernel's single-address-
space model at all.

- **All sixteen numbers land at `471`-`486`** — see the Syscall ABI section's own collision-
  avoidance rule above for why (a first attempt continuing the `179`+ sequence collided with real,
  still-live `__NR_gettid` etc.). `oxfs`'s `SYS_FSYNC=471`...`SYS_FSTATFS=477`, `posix_compat`'s
  `SYS_PRLIMIT64=478`...`SYS_REBOOT=486`.
- **`SYS_FSYNC`/`SYS_SYNC`** are real, not stubs — this fs only commits a file's write buffer to
  its inode at `close()`, so `fsync()` needed a shared `commit_write_buffer` (refactored out of
  `oxfs_close`) callable for one fd (`fsync`) or swept across every open write fd (`sync`).
  Idempotent via `existing_inode`.
- **`SYS_FTRUNCATE`/`SYS_FALLOCATE`** resize directly at the block level (`resize_inode_data`), not
  via a whole-content buffer (the 128 KiB kernel-stack floor can't hold a ~4 MiB file). Growing
  zero-fills only the new region; shrinking touches no block content. Both resolve a fd still
  `OpenFile::Write`-in-progress against a pre-existing inode via `existing_inode` directly.
  `fallocate`'s `mode` is ignored (always zero-extend-if-shorter, never shrinks).
- **`SYS_FLOCK`** is a real per-inode `LOCK_SH`/`LOCK_EX`/`LOCK_UN` advisory table (`FLOCKS`, fixed
  16 entries), released on close. **A conflicting request fails `EAGAIN` immediately even without
  `LOCK_NB`** — no scheduler-yield primitive is reachable from a module syscall handler, and a real
  spin-wait here would permanently deadlock against a holder that could never run to release it.
- **`SYS_STATFS`/`SYS_FSTATFS`** report a real musl-layout `struct statfs` (`MuslStatfs`, 120
  bytes, `write_unaligned`) from live block/inode-usage counts, scanned fresh each call (separately
  for real vs. tmpfs pool).
- **`SYS_PRLIMIT64`** backs both `getrlimit(2)`/`setrlimit(2)` (musl's wrapper tries `prlimit64`
  first unconditionally). `Process::rlimits: [(u64,u64); 16]` (`RLIM_INFINITY` default) — **stored,
  never enforced**, same honest-gap tier as `O_NONBLOCK` on a TCP socket. Real Linux's
  `__NR_setrlimit=160` would collide with this ABI's own `SYS_GETGID=160` if that legacy fallback
  were ever reached — avoided only because `prlimit64` always succeeds first.
- **`SYS_SETPRIORITY`/`SYS_GETPRIORITY`** (`nice`) — `Process::nice: i32` (default `0`), copied by
  fork/preserved by execve. `getpriority`'s real return convention is `20 - nice` (musl's own
  wrapper un-shifts client-side). No real scheduling effect.
- **`SYS_SCHED_SETSCHEDULER`/`_GETSCHEDULER`/`_GETPARAM`/`_GET_PRIORITY_MAX`/`_MIN`** (`chrt`) —
  BusyBox calls these via raw `syscall()`, bypassing musl's permanently-stubbed library wrappers,
  so only the number needed remapping. `Process::sched_policy`/`sched_priority: i32` stored/echoed
  honestly, no real effect. Priority-range functions are pure functions of `policy` alone.
- **`SYS_REBOOT`** (+ `src/reboot.rs`'s `poweroff`/`halt`) matches real Linux's magic
  `RB_AUTOBOOT`/`RB_HALT_SYSTEM`/`RB_POWER_OFF` values. `RB_AUTOBOOT` reuses the existing 8042-pulse
  reset; `RB_HALT_SYSTEM` is a permanent `hlt_loop()`; `RB_POWER_OFF` writes QEMU's ACPI PM
  shutdown port (`0x604`, value `0x2000`), falling back to a plain halt. No permission check — no
  capability model to gate against. **Every success path halts/resets/powers off the VM** —
  manual-QEMU-only, `isa-debug-exit` can't distinguish that from a hang.
- **`SYS_UMASK = 487`** (added later, continuing past `486` rather than filling a gap inside the
  batch). `Process::umask: u32` (default `0o022`) — real per-process state (`umask()` can't fail
  and returns the *previous* mask), copied by fork/preserved by execve, stored/returned honestly
  but not actually consulted anywhere oxfs creates a new inode. Found live: BusyBox's
  `libbb/parse_mode.c` (used by `chmod +x`, `mkdir`, `install`, `cp`'s own symbolic-mode parsing)
  calls `umask(0)`/`umask(old)` unconditionally, so every symbolic-mode operation was silently
  computing against a garbage `ENOSYS`-derived value before this existed.
- Verified via `tests/needs_syscall_smoke.rs` + `userland/needs-syscall-smoke/` (except
  `reboot`/`umask`, manual-only).

## Real hard links, device nodes, per-process chroot, and getrusage/wait4 rusage (`modules/oxfs`, `modules/posix_compat`, `src/process/`, `src/syscall/ffi.rs`)

Closes most of the rest of `NEEDS_SYSCALL`: `link`/`ln`, `mknod`/`makedevs`, `chroot`, `time`'s
`getrusage`/`wait4`-rusage dependency. SysV IPC and real namespaces stay out of scope.

- **`SYS_LINK=488`/`SYS_MKNOD=489`/`SYS_CHROOT=490`** (`oxfs`), **`SYS_GETRUSAGE=491`**
  (`posix_compat`) — continues past `491`, same collision-avoidance discipline as `471`-`487`.
- **Real hard links**: `Inode` gains `nlink: u16` (packed into previously-unused stride padding).
  `oxfs_link` resolves `existing` via symlink-following `resolve_path` (hard-linking a symlink
  *entry* itself isn't supported — POSIX leaves this implementation-defined), rejects
  `InodeKind::Dir` (`EPERM`) and linking across the real/tmpfs pool boundary (`EXDEV`), then
  `dir_insert`s + bumps `nlink`. `oxfs_unlink` decrements `nlink` for `File`/`Device` before
  clearing the directory record (still never actually freed). A pre-existing on-disk inode from
  before this field existed decodes `nlink=0` — `write_stat` floors this back to `1`.
- **Real device nodes**: new `InodeKind::Device`, `Inode::rdev: u32`/`device_char: bool`. Unlike
  the existing magic-path `/dev/{random,urandom,null,zero}` interception (never backed by an
  inode), `mknod` creates a real, listable/stat-able inode with a real `st_rdev` — but there's
  still no general device-driver framework: `oxfs_open`'s `Device` dispatch only actually services
  major:minor pairs matching those same four real devices (real Linux's standard values); any other
  major:minor is a real node that honestly fails `ENXIO` on open. `mknod` also supports `S_IFREG`
  (immediately committed empty file); `S_IFIFO`/`S_IFSOCK` are `EINVAL`. Device-node creation is
  root-only (`EPERM` otherwise); `S_IFREG` only needs parent write access.
- **Real per-process `chroot`**: `Process::root_inode: u64` mirrors `cwd`'s design exactly (`0`
  doubles as real root inode *and* "never chrooted"). Copied by fork, untouched by execve.
  `resolve_path_impl` gains a `root_inode` parameter — used for the absolute-path start and, more
  importantly, as containment: a `..` component stays put when `current == root_inode`.
  `oxfs_chroot` is root-only and deliberately doesn't also `chdir` (matches real `chroot(2)` —
  BusyBox's own `chroot` applet calls `chdir("/")` itself right after). **Known, accepted gap**: a
  chrooted-into directory later removed/renamed out from under the process isn't specially handled.
- **`SYS_GETRUSAGE` + real `wait4` rusage**: no per-process CPU-time/memory accounting exists, so
  both report a real, correctly-shaped, all-zero `struct rusage` (`RawRusage`, 272 bytes,
  `write_unaligned`) rather than inventing numbers. `do_wait4` gained a 4th `rusage_ptr` param.
- **A real regression found immediately by the full test suite**: making `wait4`'s 4th arg (`R10`)
  suddenly meaningful broke `tests/fork_wait.rs` — `SYSCALL` doesn't clear `R10`, and several
  hand-written userland smoke-test crates had their own local 3-argument `syscall()` asm helper
  that never touched it, reading garbage the instant a 4th argument became live. **Any future
  syscall that upgrades from 3 to 4 real arguments needs an audit of every existing userland
  crate's own hand-rolled `syscall()`/`syscall3` helper for whether it zeroes `r10`**, not just a
  kernel-side signature check.
- Verified via `modules/oxfs`'s own boot self-check and `tests/needs_syscall2_smoke.rs` +
  `userland/needs-syscall2-smoke/`.

## Two syscalls found live by the expanded `test_busybox.sh` post-v0.1, both real Linux numbers used directly (`modules/oxfs`, `src/process/limits.rs`)

Running the pre-v0.1-expanded `sh /test_busybox.sh` against a real boot for the first time (the
script's own comment had flagged it as never yet run live) surfaced two `[boot] unrecognized
syscall number N]` lines — both real, live musl callers this ABI hadn't registered, not script
bugs. Both landed at their real, unremapped Linux `__NR_*` values directly rather than an invented
number: `third_party/musl/arch/x86_64/bits/syscall.h.in` never touches either one, so musl already
calls `syscall(91, ...)`/`syscall(204, ...)` expecting real semantics, and neither value was
already claimed anywhere else in this ABI's own registry (checked against every `SYS_*` constant in
`src/`/`modules/` first — same collision-avoidance discipline as inventing a number, just run in
reverse).

- **`fchmod` (`modules/oxfs`'s `oxfs_fchmod`, real `__NR_fchmod = 91`)**: found via `uudecode`,
  which restores a decoded file's mode with a real `fchmod(fd, mode)` call on its still-open output
  fd, not a path-based `chmod()`. Silently `ENOSYS`'d before this — `uudecode` ignores the return
  value, so the roundtrip test still passed on content alone; the restored mode was just silently
  wrong. Resolves the fd the same way `oxfs_fstat`/`oxfs_ftruncate` already do (`resolve_write_fd_
  inode`, covering a pre-existing or freshly-`O_CREAT`'d write fd, not just a plain read fd), then
  the same owner-or-root check and `0o777` mask as `oxfs_chmod`.
- **`sched_getaffinity` (`src/process/limits.rs`'s `do_sched_getaffinity`, real `__NR_sched_getaffinity =
  204`)**: found via `nproc`, which calls this to count set bits in the affinity mask — but silently
  falls back to `count = 1` on any failure, so `nproc` printed the right answer by coincidence even
  while this was a flat `ENOSYS`; the gap only showed up as a logged unrecognized-syscall line, not
  a wrong result. Implemented anyway (single-core, so the mask is always just bit 0 set) rather than
  left as a silently-tolerated `ENOSYS`, since a future `taskset`/affinity-querying caller landing on
  `ENOSYS` isn't obviously as safe as `nproc`'s specific fallback. Real raw-syscall return-value
  convention (bytes written, not glibc/musl's 0-or-error wrapper convention — see
  `third_party/musl/src/sched/affinity.c`'s own `do_getaffinity`), so this only ever writes
  `min(cpusetsize, 8)` real bytes and returns that count; musl's own wrapper zero-fills the rest of
  the caller's buffer itself.

## TinyCC: a real, on-target C compiler (`third_party/tinycc`, `modules/oxfs`, `build.rs`)

OxideBSD's first C compiler that runs *on* the target itself. GCC/Clang are a much bigger lift
(real multi-process pipelines, usually real dynamic linking, often threads — none of which this
kernel has: `elf.rs` has zero `PT_INTERP` support, `clone`/`futex`/`mprotect` unregistered). TinyCC
is a single monolithic static binary with real, maintained upstream musl support
(`--config-musl`) — the natural first target.

- Vendored as a submodule (`Pomsky2011/tinycc-oxidebsd`, `oxidebsd` branch, tag `release_0_9_27`).
- `build_tinycc()` cross-builds via `musl-gcc` against the existing musl sysroot, plain `./configure
  --config-musl` + `make` (no per-applet Kconfig dance). Needs explicit `--crtprefix=/usr/lib
  --libpaths=/usr/lib --sysincludepaths=/usr/include` — tcc's own `configure` otherwise probes the
  *build host's* `/usr/lib64` layout, unrelated to the musl sysroot's flat `/usr/lib`.
- `libtcc1.a` (tcc's own runtime helper lib) is built via tcc's `<target>-libtcc1-usegcc=yes`
  escape hatch instead of self-hosting — the just-built `tcc` is linked against this project's
  *patched* musl, so running it directly on the host silently does nothing (misinterpreted syscalls
  under the real host ABI). Safe here since `libtcc1.a`'s sources are pure freestanding numeric
  helpers with no syscalls.
- Built in-place inside the submodule (no `O=` out-of-tree mechanism exists) — staleness tracking
  (`tinycc_source_mtime`/`is_tinycc_build_output`/`clean_tinycc_build_outputs`) shares one
  exclusion list so it can't drift, and **each real source file walked gets its own
  `cargo:rerun-if-changed`** — found live, the hard way: computing staleness without registering
  the watch made a real source edit completely invisible to cargo's decision to re-run `build.rs`
  at all.

**Two real, deep bugs found getting a genuine `tcc -static -o hello.elf hello.c && ./hello.elf`
working — both root-caused via raw ELF/GOT/PLT byte inspection, not guessed**:
1. This dev machine's host `gcc` defaults to PIE, and musl's own `configure` auto-detects that and
   lets PIE-style GOT-indirect codegen leak into every musl object it builds including `crt1.o`.
   Every *other* consumer of the sysroot links via real GNU ld, which silently performs GOTPCRELX
   link-time relaxation, hiding this — TinyCC's own linker doesn't implement that relaxation, so
   the first `tcc`-produced binary faulted on an instruction fetch through a reserved-but-never-
   written GOT slot. **Fixed by forcing non-GOT-indirect codegen for the whole musl build**
   (`./configure` gains `CFLAGS=-fno-pie -fno-PIC`), requiring a full `make distclean` + fresh
   sysroot rebuild (and, transitively, every BusyBox applet relinking against the rebuilt libc.a).
2. **TinyCC generates real PLT/GOT indirection for calls to any default-visibility external
   function, even under a fully `-static` link.** PLT/lazy-binding has no meaning without a dynamic
   loader to ever populate a GOT slot — this target has none. Fixed by patching
   `third_party/tinycc/tccelf.c`'s `build_got_entries` to extend its existing hidden-visibility/
   `STB_LOCAL` no-PLT carve-out to also fire whenever `s1->static_link` is true — matching what a
   real linker (GNU ld) already does automatically for every other static binary here.
3. **A real, separate crash found testing further**: a bare `tcc hello.c` (no `-static`) produced a
   dynamically-linked `a.out` that page-faulted at `VirtAddr(0x0)` on its first indirect call
   (`elf.rs` has zero `PT_INTERP`/dynamic-relocation support, so PLT/GOT slots stay zero-
   initialized forever). Fixed at the root: `third_party/tinycc/libtcc.c`'s `tcc_new()` now
   defaults `s->static_link = 1` unconditionally — every on-target `tcc` invocation links
   statically regardless of flags.

- **`SYS_LSEEK = 8`** (real x86_64 Linux's own value, confirmed still inert) — tcc's own
  object-file loader needs a real file size upfront (`fseek`/`ftell`) before parsing ELF/`ar`
  headers; without it, every crt/lib file it opened reported `invalid object file`. Only
  `{FileRead,DirListing,ProcRead,ProcDir}` `OpenFile` variants have a real seek position; `Write`
  and the synthetic `/dev/*` variants report `ESPIPE`.
- **A real, generated on-target runtime tree**: `/usr/include` (musl's full header tree),
  `/usr/lib` (`crt1.o`/`crti.o`/`crtn.o` + `libc.a` + 8 musl stub archives — PIE-only crt variants
  and the host-only `musl-gcc.specs` deliberately skipped), `/usr/lib/tcc` (`libtcc1.a` + tcc's 5
  bundled compiler-magic headers). Generated by `build.rs`'s `write_tcc_runtime_manifest` into
  `target/generated/tcc_runtime_manifest.rs` (`pub static X: &[(&str, &[u8])]`, `include!`'d once
  into `modules/oxfs`) — literal absolute `include_bytes!` paths, deliberately not the
  `env!()`-per-file pattern every other embedded ELF uses (no reason to invent ~235 one-off names
  for data with no other identity need; the generated file is itself host-specific already).
- `MAX_INODES` raised `512 → 1024`, measured against the real built sysroot (~250 new inodes: 217
  headers, ~10 subdirs, 12 lib/crt files, tcc's 5 headers, `libtcc1.a`, `tcc` itself).
- `modules/oxfs` gained real directory-tree seeding infra: `ensure_dir(parent, name)` (idempotent —
  looks up an existing child first, since a manifest-driven tree walk re-enters the same parent
  many times) and `seed_tree(root, files)` (splits `/`-separated paths, walks/creates intermediates,
  seeds the leaf) — the first content seeded shaped like a real nested tree.
- A seeded `/hello.c` fixture (real `printf`, not a bare `return`) for both automated and manual
  testing.
- Verified via `tests/tcc_syscall_smoke.rs` + `userland/tcc-syscall-smoke/` — real
  `fork`+`execve` `tcc -static -o /hello.elf /hello.c`, `wait4`, then `fork`+`execve` the produced
  `/hello.elf` itself and check its exit status (proving the *output* is a real runnable ELF, not
  just that `tcc` exits `0`), then the same round trip via a bare `tcc -o /hello2.elf /hello.c`
  (no `-static`, exercising bug 3's fix).
- **Not covered**: `-run` (in-memory JIT execution), self-hosting.

**A real, three-layered disk-persistence bug found testing against an already-formatted real
disk** (not the always-freshly-zeroed test disk every automated test uses) — the `MAX_INODES`
512→1024 bump changed the on-disk inode-table size, and a disk formatted under the old layout
broke in three separately-real ways:
1. **`reset_real_pool_for_fresh_format()`** — `mount_from_disk`'s bitmap/inode-table load ran to
   completion (and got left live in memory) *before* the later per-block data read where a real
   layout mismatch actually got caught, so a fallback to `format_fresh_filesystem` inherited a
   "mostly full" bitmap from a filesystem that no longer existed — `alloc_block`/`alloc_inode`'s
   own linear free-slot scans then panicked `DiskFull` seeding `/etc`. Fixed: resets the real
   bitmap/inode table to all-free immediately before any format run that might follow a partial
   mount attempt.
2. **`OXFS_METADATA_BLOCKS` in `build.rs`** — a second, independently hand-duplicated copy of the
   on-disk metadata-block-count math (a `build.rs` can't import oxfs's own constants), left at a
   stale hardcoded value after `MAX_INODES` changed, silently sizing every *newly created*
   `oxfs_disk.img` too small. Fixed: computed from the real constants instead of a magic number.
3. **`mount_from_disk`'s superblock check was magic-only, not layout-aware** — a disk formatted
   under the old layout still has the right magic, just physically different content at every
   offset. Before this, a stale disk merely *usually* failed loudly partway through mounting (an
   incidental out-of-bounds read), not guaranteed. Fixed: reads back the superblock's own stored
   `SUPERBLOCK_VERSION`/`NUM_BLOCKS`/`MAX_INODES` right after the magic check and falls back to
   reformat on any mismatch — a real check, not an incidental crash.
4. **`write_data_disk_images()` only ever created the persistent dev disk if missing, never grew an
   undersized existing one** — bug 2's fix sizes new disks correctly, but an existing-but-too-small
   disk file stayed physically short regardless, so mount's own reads would run off the end before
   bug 3's check ever got the chance to decide mount-vs-format. Fixed: an existing dev disk smaller
   than the current expected size gets grown in place (zeros appended, real bytes untouched).

## Real `getrandom(2)`: first of the pre-reserved batch (`modules/posix_compat`, `src/syscall/ffi.rs`)

`docs/MISSING_POSIX_SYSCALLS.md` tracks POSIX conformance beyond musl's live call graph, including
a 28-syscall batch (`526`-`553`) pre-reserved with permanent OxideBSD-invented numbers ahead of
having real handlers — see that doc's own "Pre-reserved ahead of implementation" section for why
(this ABI doesn't want its own planned syscalls sitting at borrowed real-Linux numbers just because
the slot happens to be free today). `getrandom` (`SYS_GETRANDOM = 526`) is item 1 of that batch and
the first to get a real handler.

- **Thin plumbing, no new primitive**: `modules/posix_compat`'s `handle_getrandom` →
  `src/syscall/ffi.rs`'s `sys_getrandom` → `src/random.rs`'s `oxidebsd_random_bytes` — the same
  generator already backing `/dev/random`/`/dev/urandom` (see that module's own doc comment for
  the persistent entropy pool and the `RDRAND`/`RDSEED` hypervisor-distrust gate). Real
  `(buf_ptr, buflen, flags)` wire format, no musl call-site patch needed beyond the number remap
  already sitting in `bits/syscall.h.in`.
- **Only reachable via `getentropy()`** in this port's roster (BusyBox's `seedrng` applet doesn't
  build here — missing `linux/random.h`), which caps `len` at `256` and loops calling `getrandom()`
  itself until satisfied — this handler can safely always fill the whole request in one shot
  (never partial, never blocking; the generator itself never fails or waits on an entropy
  threshold), so that loop always exits after its first iteration.
- **`flags`** (`GRND_NONBLOCK`/`GRND_RANDOM`) are accepted but make no behavioral difference — no
  blocking-on-low-entropy distinction exists to honor either against, matching `/dev/random`/
  `/dev/urandom` already sharing one source. Any other bit is a real `EINVAL`.
- Verified via `tests/getrandom_syscall_smoke.rs` + `userland/getrandom-syscall-smoke/` — a real
  spawned ELF through genuine `SYSCALL`/`SYSRETQ` (`tests/random_smoke.rs` already covers the
  generator's own cryptographic logic directly; this test's job is narrower — proving the syscall
  plumbing and argument convention, the class of bug this codebase's musl-port section documents
  catching repeatedly). Four parts: a real 32-byte request succeeds and isn't degenerate, two
  consecutive requests differ, `len == 0` is a harmless no-op, and flag handling is correct.

## BusyBox gap analysis: what's needed for more applets

Almost everything left needs one of a handful of missing kernel capabilities, each unlocking a
cluster of applets at once. New syscall numbers should continue from the highest currently
assigned (check `src/syscall/`/module sources rather than trusting stale numbers here).

**`docs/BUSYBOX_APPLETS.md` is the authoritative, per-applet detail behind every row below** — the
counts here (out of the 287 applets that built at all) are a summary, not the full picture. A
pre-v0.1 pass cut 58 of those 287 from the roster entirely (not "not started" — structurally
incapable of working under this kernel's current architecture: no VT/console/serial/framebuffer/
syslog device model, namespaces, SysV IPC, ext2 ioctl/xattr, FIFOs, or partition-table/swap/
hw-profile support) rather than continuing to carry them as dead weight; see that doc's own
"Removed before v0.1" section for the full list and per-applet reasoning. 229 remain seeded.

| Gap | Status | Blocks (of 287 built) | Placement |
|---|---|---|---|
| `argv[0]` passthrough | done | — | — |
| Real signals | done | — | `modules/signal/` |
| Process groups (`setpgid`/`getpgid`) | done | — | `modules/posix_compat/` |
| termios/`ioctl` | done (no real pty layer) | — | `SYS_IOCTL` in `posix_compat` |
| `stat`/`fstat`/`lstat` | done | — | `modules/oxfs` |
| `getdents`/`getdents64` | done | — | `modules/oxfs` (`SYS_GETDENTS=129`; both `__NR_getdents`/`__NR_getdents64` remapped — see musl section's 64-bit-sibling gotcha) |
| Socket syscalls + real DNS | done | most of 38 (`NEEDS_NETWORK`) | see "Real networking" — `ping`/`nslookup`/`wget` (plain HTTP) confirmed live |
| `socketpair`/`fcntl`/`shutdown`/`set_tid_address`/`readv` + `/dev/{u}random,null,zero` + real `tcp_read` EOF fix | done — `wget` HTTPS confirmed live end to end | `wget` HTTPS specifically | see "Real networking" known-gaps entry |
| `alarm`/`setitimer` | done | `ping`'s receive-loop timeout | `SYS_SETITIMER=156`/`SYS_GETITIMER=157` in `modules/clock` |
| `chmod`/`chown`/`chgrp` | done | — | `SYS_CHMOD=165`/`SYS_CHOWN=166` in `modules/oxfs` (`chown()` restricted to the group field covers `chgrp` too); `chattr`/`fatattr`/`lsattr`/`setfattr` needed a distinct ext2-`ioctl`/`xattr` gap this kernel doesn't implement — those 4 applets were removed from the roster before v0.1 rather than left as dead weight |
| `fsync`/`sync`/`ftruncate`/`fallocate`/`flock`/`statfs`/`setrlimit`/sched-priority/`reboot` | done | was subset of 33 (`NEEDS_SYSCALL`) | `SYS_FSYNC=471` through `SYS_REBOOT=486` |
| `link`/`ln`, `mknod`/`makedevs`, `chroot`, `getrusage`/`time` | done | was subset of `NEEDS_SYSCALL` | `SYS_LINK=488`/`SYS_MKNOD=489`/`SYS_CHROOT=490`/`SYS_GETRUSAGE=491` |
| SysV IPC, namespaces (`unshare`/`nsenter`/`setarch`/`setpriv`), `inotify`, ext2 `ioctl`s/`xattr` | not started, and no longer blocking anything — the applets that needed these (`ipcrm`/`ipcs`, `linux32`/`linux64`/`nsenter`/`setarch`/`setpriv`/`unshare`, `inotifyd`, `chattr`/`fatattr`/`lsattr`/`setfattr`) were removed from the roster before v0.1 | 0 remaining (`NEEDS_SYSCALL` fully resolved: done or removed) | namespaces don't fit this kernel's single-address-space model at all |
| `/proc` — per-process (`stat`/`cmdline`/`status`, dir listing, `stat(2)`) | done | — | special-cased path prefix in `modules/oxfs` (no VFS layer to plug a separate module into), synthesized from `src/process/procfs.rs` accessors. Unlocks `pidof`/`pgrep`/`pkill`/`pstree`/`minips` |
| `/proc` — system-wide (`meminfo`/`uptime`/`stat`) + `chdir(2)` into `/proc` | done | — | `MemFree`/`MemAvailable` == `MemTotal` (no dealloc tracking); `/proc/stat`'s `cpu` line all-zero except `idle`. `free`/`uptime` call `sysinfo(2)` for their primary numbers — a distinct, unimplemented gap these don't unblock |
| `/proc` — per-fd (`/proc/<pid>/fd/`) | done (enumeration only, not real symlinks) | subset of the above | needs a cross-module "describe this fd" mechanism to go further (oxfs doesn't know what a pipe/socket fd is) |
| Real symlinks (`SYS_SYMLINK`/`SYS_READLINK`) | done | `ln -s`/`readlink` | new `InodeKind::Symlink`; `resolve_path_impl` follows for every intermediate component always, final component only when `follow_last` — `MAX_SYMLINK_DEPTH=8`, `ELOOP=40` (musl's real value) |
| Console/VT ioctls, serial/tape/I2C hardware, syslog, real pty | not started, and no longer blocking anything in the kept roster — `cttyhack`/`setsid` turned out to already work (real session/tty syscalls exist) and moved to WORKS; the other 22 applets in this category were removed before v0.1 | 0 remaining (was 24, `NEEDS_HARDWARE`) | several unrelated small gaps, see `docs/BUSYBOX_APPLETS.md` |
| Real block device driver + oxfs persistence | done | — | see "Real disk persistence" — still a fixed, non-mountable backing store outside the scoped mount table below |
| Mount table (`mount --bind`/`mount -t tmpfs`) | done | `mount`/`umount`/`mountpoint` | see "Mount table" — doesn't unblock `pivot_root`/`switch_root` or anything needing a real partition table; that whole remaining `NEEDS_BLOCKDEV` bucket (17 applets) was removed from the roster before v0.1 rather than left blocked indefinitely |
| uid/passwd-db model | done | most of 16 (`NEEDS_UID`) | see "Permission model". `adduser`/`chpasswd`/`passwd`/... still need real *mutation* of `/etc/passwd`/`/etc/group` (applet-level gap now) |
| real login/session auth (`su`/`login`/`sulogin`/`getty`) | done | 4 (subset of `NEEDS_UID`) | see "Session, controlling-tty, and login authentication" |
| `clock_gettime`/`gettimeofday`/`time` | done | — | `SYS_CLOCK_GETTIME=138` in `modules/clock` |
| `nanosleep` | done | 9 (`NEEDS_CLOCK`) | `SYS_NANOSLEEP=139` |
| Init-system/service-supervisor framework | not started, out of scope | 2 kept anyway (`bootchartd`/`start_stop_daemon` — their real mechanics don't need an init framework, just fork/exec/setsid/kill/pidfile this kernel already has); 4 removed before v0.1 (`runsv`/`runsvdir`/`svlogd`/`svok` — genuinely dead, the runit family needs FIFOs oxfs doesn't have) | no init framework to plug into at all |
| `tcsetpgrp`/real job control | done | — (its old `NEEDS_HARDWARE` category is now empty — see above) | see "Real job control" — real Ctrl+C/`kill(-pgrp)`/colors on both branches, real Ctrl+Z stop/continue on `master` only |
| `uname` | done | — | `SYS_UNAME=137` in `modules/posix_compat` |
| `gethostname` | done | — | no new syscall — musl's `gethostname()` wraps `uname()` |

**83 more candidate applets didn't even build**: 54 need real Linux kernel uapi headers musl
doesn't vendor, 25 need a companion Kconfig option a single-symbol flip didn't resolve, 3 were
docs/example files mismatched by candidate-extraction, 1 (`lzopcat`) is a genuine link error. See
`docs/BUSYBOX_APPLETS.md` for the full breakdown.

## Dependency notes

- `x86_64` crate: `default-features = false, features = ["instructions", "abi_x86_interrupt"]` —
  the default feature set pulls in `step_trait`, an unstable-API moving target that has broken this
  crate against newer nightlies before.
- `bootloader` pinned to `0.9` (not `0.11+`'s artifact-dependency API) — keeps the setup in one
  crate; `map_physical_memory` feature is required for `BootInfo::physical_memory_offset` to exist.
- `linked_list_allocator`: `default-features = false` — its default `LockedHeap` depends on
  `spinning_top`, a second spinlock crate alongside `spin` (used everywhere else here).
- `pc-keyboard` 0.9's type is `PS2Keyboard<L, S>`, not `Keyboard<L, S>` (older tutorials reference
  the pre-0.9 name). Decoding is two calls through the *same* locked guard: `add_byte` →
  `KeyEvent`, then `process_keyevent` → `DecodedKey`.
- `pic8259`/`uart_16550` are deliberately **not** dependencies — both wrap a handful of
  `outb`/`inb` calls against a stable protocol, small enough that owning the code (`src/cpu/pic.rs`,
  `src/console/serial.rs`) outweighs the dependency. Different call than `pc-keyboard` (hundreds of lines
  of scancode tables) or `linked_list_allocator` (safety-critical free-list logic), which stay
  external.
- `sha2`/`chacha20` (`src/random.rs`, backing `/dev/random`/`/dev/urandom`): `default-features =
  false`, `sha2` additionally needs `features = ["force-soft"]` and `chacha20` needs `--cfg
  chacha20_backend="soft"` via `.cargo/config.toml`'s rustflags — both otherwise try to compile a
  SIMD-capable backend this target's disabled SSE/MMX can't lower (a real `rustc-LLVM ERROR`).
  Crypto primitives are the one place this codebase deliberately prefers a vetted dependency over
  hand-rolling — the opposite call from `pic8259`/`uart_16550` above.
