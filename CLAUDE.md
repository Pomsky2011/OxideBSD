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
no *kernel-mode* preemption (real ring-3/user-mode preemption exists — see "Real preemptive
scheduling" below), no copy-on-write fork, no frame deallocation for module-loaded code/SysV-shm-
or-`MAP_SHARED`-owned frames (real reclaim exists for the common case — a discarded process's own
private address-space frames — see "POSIX conformance pilot expanded 68 → 488..." below), `sys_read` on stdin is
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
- SSE was never enabled at the hardware level (`CR0.EM`/`CR4.OSFXSR`/`OSXMMEXCPT`); `src/cpu/
  fpu.rs::init()` enables it once at boot. Real per-process `FXSAVE`/`FXRSTOR` save/restore across
  every context switch now exists (`Process::fpu_state`) — see "Real preemptive scheduling" below
  for why this became load-bearing rather than optional once ring-3 preemption landed.
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

Dynamically allocated process table, cooperative round-robin scheduler (real ring-3 preemption
layered on top later — see "Real preemptive scheduling" below), kernel-thread-style context switch
between per-process kernel stacks. No copy-on-write fork (full eager copy), no SMP, no frame
deallocation for module-loaded code/SysV-shm-or-`MAP_SHARED`-owned frames (real reclaim exists for
a discarded process's own private address-space frames — see "POSIX conformance pilot expanded
68 → 488..." below).

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

Real `kill(2)`/`sigaction(2)`/`sigprocmask(2)` + delivery (handler invocation + `sigreturn`), plus
`sigtimedwait(2)`/`sigwaitinfo(2)`/`sigwait(3)`/`sigqueue(2)`. `SYS_KILL=116`/`SYS_SIGACTION=117`/
`SYS_SIGPROCMASK=118`/`SYS_SIGRETURN=119` — all four happen to match real Linux/BSD wire formats, so
the musl patch is a pure 4-line number remap (plus one hardcoded restorer-stub literal,
`src/signal/x86_64/restore.s`). `SYS_SIGTIMEDWAIT=495`/`SYS_SIGQUEUE=496` are real, unclaimed
`__NR_rt_sigtimedwait`/`__NR_rt_sigqueueinfo` values, used directly, no invented number needed.
Real signal numbers (`SIGHUP=1`...`SIGSYS=31`, no realtime signals).

- `Process::sigactions: [SigAction; 32]` (real `SIG_DFL=0`/`SIG_IGN=1`) plus `pending_signals`/
  `blocked_signals` bitmasks, `pending_siginfo: [QueuedSigInfo; 32]` (real per-signal sender
  `pid`/`uid`/`si_code`/`sigqueue` value — see below), and a real `signal_stack: Vec<
  SignalStackFrame>` — a genuine signal stack, not a single snapshot: a second signal becoming
  deliverable while a handler is already running pushes a further entry and chains into another
  handler invocation instead of clobbering the first (see "Real signal-stack chaining" below for
  the fix and what it closed).
- Delivery happens once, at the tail of `syscall_dispatch`. `sigreturn` bypasses the normal
  `Ok`/`Err` carry-flag rewrite entirely (must restore an arbitrary saved `CF`) — the one syscall
  number not registered in `SYSCALL_TABLE` at all.
- `do_kill` cross-process: immediate for the common case (no handler → terminate right there, even
  against a blocked target — this immediate path does **not** consult `blocked_signals` at all, an
  intentional simplification: a target with no handler installed for a signal always resolves its
  default disposition right there regardless of whether that signal happens to be blocked);
  deferred until next-scheduled only if the target has a custom handler. **Real permission
  checking** (`has_signal_permission`, see "Real-time signal queuing" below): sender must be root
  or share the target's uid, else `EPERM` — checked by `do_kill`/`do_sigqueue`'s own single-target
  paths only, not `signal_foreground_group`'s process-group broadcast (no live caller needs it
  there, and real POSIX's own per-member partial-success rule is meaningfully more complex).
  **Real process-group targeting** (`target_pid == 0`/`< 0`, POSIX `kill(-pgrp, sig)`) — see "Real
  job control" below.
- **Real `SA_SIGINFO` handler invocation**: a handler installed with that flag is invoked as a
  genuine 3-argument `void (*)(int, siginfo_t *, void *)` — `RawSiginfo`/`RawUcontext`/`RawMcontext`
  (`src/process/mod.rs`/`src/syscall/mod.rs`) are real, correctly-sized structures built on the
  handler's own stack frame, with real general-purpose registers (`uc_mcontext.gregs`, from the
  interrupted syscall's own saved frame) and real `uc_sigmask`. A plain (non-`SA_SIGINFO`) handler
  still gets the simpler 1-argument `void (*)(int)` invocation.
- **Real per-signal sender identity/payload** (`Process::pending_siginfo`, `QueuedSigInfo`): every
  place that sets a signal pending (`do_kill`'s self/cross-process paths, `signal_foreground_group`,
  `do_sigqueue`) records real `si_code` (`SI_USER` for `kill`-shaped, `SI_QUEUE` for
  `sigqueue`-shaped — `0`/honest-zero sender only for `signal_foreground_group`'s own two callers, a
  keyboard-generated Ctrl+C/Ctrl+Z or a `kill(-pgrp, sig)` broadcast, neither of which has one real
  sender process to attribute), real sender `pid`/`uid`, and the real `sigqueue`-supplied value.
  Feeds both the `SA_SIGINFO` handler-invocation path above and `sigtimedwait`'s own real readback
  below.
- **`sigtimedwait`/`sigwaitinfo`/`sigwait`** (`process::signals::do_sigtimedwait`, one handler backs
  all three musl library entry points) — a genuinely different primitive from `pause`/`sigsuspend`'s
  own `BlockReason::WaitingForSignal`: real POSIX semantics directly *consume* a pending signal
  matching the caller's `wait_set` and return its number, **bypassing handler invocation entirely**
  even if one is installed (`BlockReason::WaitingForSpecificSignal`). Real relative timeout, `EAGAIN`
  on expiry. **A signal used this way must be blocked via `sigprocmask` first** (POSIX leaves
  behavior unspecified otherwise — an unblocked signal races `deliver_pending_signal`'s own normal
  tail, which runs at the end of the very syscall that made it pending) **and, for cross-process
  delivery specifically, needs a real handler installed too** (`do_kill`/`do_sigqueue`'s own
  immediate-terminate path for a no-handler target doesn't consult `blocked_signals` at all, per the
  `do_kill` bullet above) — found live while writing this feature's own smoke test, not a kernel bug.
- **`sigqueue`** (`process::signals::do_sigqueue`) — real `(pid, sig, siginfo_ptr)`, single-target
  only (no process-group broadcast shape exists for it in real POSIX either). Reuses `do_kill`'s own
  disposition-resolution shape, duplicated rather than factored out (matches `signal_foreground_group`'s
  own established precedent for this exact enum/match shape).

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
(real multi-process pipelines, often threads — `clone`/`futex` still unregistered — and, at the
time this section was first written, real dynamic linking too; **milestone 1 of that has since
landed, see "Dynamic linking" below** — TinyCC itself still doesn't need it, see bug 3 below).
TinyCC is a single monolithic static binary with real, maintained upstream musl support
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
   (`elf.rs` had zero `PT_INTERP`/dynamic-relocation support at the time — since added, see
   "Dynamic linking" below, but TinyCC's own output was never revisited to actually use it; this
   fix keeps every `tcc` invocation static instead). Fixed at the root: `third_party/tinycc/libtcc.c`'s `tcc_new()` now
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

## Dynamic linking: milestone 1, real `PT_INTERP` (`src/process/elf.rs`, `src/process/lifecycle.rs`, `build.rs`, `modules/oxfs`)

A real, working `fork`+`execve` of a genuinely dynamically-linked ELF, resolved and relocated by
musl's own real `ld.so` running as the interpreter — not this kernel doing the linking itself.
Landed in `e72fc7d` but never written up here until now — this section existed only in
`docs/MISSING_POSIX_SYSCALLS.md`'s own `dlopen` row and code comments; every other mention of
`PT_INTERP`/dynamic linking elsewhere in this file predates it and said "zero support" — now fixed
at each of those mentions to point here instead.

- **A second, fully separate `-fPIC`/shared musl build** (`build.rs`'s `build_musl_sysroot_shared`,
  kept isolated from the existing static sysroot every other consumer — TinyCC, BusyBox, the
  original `musl-smoke` — still depends on) produces a real `libc.so`. Matches musl's own real
  convention: there's no separate `ld.so` binary — `/lib/ld-musl-x86_64.so.1` is a symlink to
  `libc.so` itself, which is both the C library and its own dynamic linker.
- **`elf.rs` accepts `ET_DYN` alongside `ET_EXEC`** — solely for a `PT_INTERP` interpreter image,
  which is always `ET_DYN`, never a real position-independent load (this kernel does no
  relocation-at-arbitrary-base anywhere else). `elf::load` gained a real, kernel-chosen additive
  `bias` parameter, applied to every segment's `p_vaddr` — necessary arithmetic, not optional.
- **Found the hard way, via a real page fault, why a fixed link-time base doesn't work**: the
  original plan was linking `libc.so` directly at `INTERP_LOAD_BASE`. That bakes already-absolute
  addresses into the interpreter's own `.rela.dyn`/`DT_RELA` table, but musl's own self-relocation
  bootstrap (`ldso/dlstart.c`) always computes `real_addr = AT_BASE + stored_value`, expecting
  `stored_value` to already be zero-based — a fixed-base link double-counts the base and produces a
  wild pointer. Fixed by linking `libc.so` at its own natural (near-zero) base and applying the
  real bias in `elf::load` instead, matching how a real kernel loads a `PT_INTERP` interpreter.
  `INTERP_LOAD_BASE = 0xc000000` (`src/process/lifecycle.rs`) is one fixed VA for the interpreter
  load, not per-binary — nothing here needs more than one interpreter resident at once.
- **`do_execve` loads the interpreter alongside the main binary** when one is present (a real
  `PT_INTERP` segment's NUL-terminated content, read via `elf.rs`'s own interpreter-path accessor),
  both sharing the same fresh address space (unlike every other fixed-base userland binary in this
  codebase, which never coexists with another image at once) — the real jump target becomes the
  interpreter's own entry point, not the main binary's, and `user_stack::build` reports the real
  bias as `AT_BASE` (`ld.so` reads this to find itself).
- **A permissive `SYS_MPROTECT = 492` stub** (`process::do_mprotect`, same shape `do_munmap`
  already established) — `ld.so`'s own RELRO-protection step calls real `mprotect` even in this
  single-interpreter case, so it needed to stop `ENOSYS`ing, but doesn't yet enforce anything (no
  `NO_EXECUTE`/write-protection actually applied — matches this kernel's existing "no W^X anywhere"
  stance). Also needed a matching `__NR_mprotect` remap in musl's own `bits/syscall.h.in` (real
  Linux's inert-until-now `10`, pushed on the `oxidebsd` musl branch separately) — the same
  numbering discipline every other syscall addition here follows.
- **Verified end-to-end**: `tests/dynlink_syscall_smoke.rs` + `userland/dynlink-syscall-smoke/` —
  a real `fork`+`execve` of `/dynlink-smoke.elf` (built by `build_dynlink_smoke` against the shared
  sysroot, a trivial one-`write()`-call fixture) through a genuine `PT_INTERP` load: real
  self-relocation, real symbol resolution against `libc.so`, and a real libc call (not just "the
  loader didn't crash") all round-trip correctly.
- **Milestone 2, not started**: `dlopen`/`dlsym`/`dlclose`/`dlerror` — loading a *second*,
  independently-chosen shared object at runtime, the way a real `dlopen()` call would. musl's own
  implementation of that is pure userspace logic over `mmap`/`mprotect`/relocation processing once
  a `.so` is already mapped — not a new syscall gap, but genuinely blocked on `mmap`/`mprotect`
  actually enforcing real placement/protection (both are still permissive no-ops/bump-allocators —
  `do_mmap` is anonymous-only, no fd-backed mapping exists at all, and the `mprotect` stub above
  enforces nothing). Milestone 1's own single-interpreter case never needed either capability for
  real: the interpreter's own load is kernel-driven (`do_execve`), not `dlopen`-driven.

## Real `getrandom(2)`/`sysinfo(2)`/`sigaltstack(2)`/`pause(2)`/`sigsuspend(2)`/POSIX timers/POSIX message queues/SysV IPC (message queues, semaphores, shared memory): all 28 items of the pre-reserved batch (`modules/posix_compat`, `modules/signal`, `modules/clock`, `src/syscall/ffi.rs`, `src/process/signals.rs`, `src/process/timers.rs`, `src/fs/mqueue.rs`, `src/fs/sysv_msg.rs`, `src/fs/sysv_sem.rs`, `src/fs/sysv_shm.rs`, `src/fs/sysv_ipc.rs`)

`docs/MISSING_POSIX_SYSCALLS.md` tracks POSIX conformance beyond musl's live call graph, including
a 28-syscall batch (`526`-`553`) pre-reserved with permanent OxideBSD-invented numbers ahead of
having real handlers — see that doc's own "Pre-reserved ahead of implementation" section for why
(this ABI doesn't want its own planned syscalls sitting at borrowed real-Linux numbers just because
the slot happens to be free today, and for the full per-item implementation write-up/test list this
section only summarizes). All 28 items now have real handlers, landed across six sessions in real
POSIX/SysV implementation order except that the three SysV IPC sub-batches (17-28) landed
message queues (25-28) before semaphores (21-24) before shared memory (17-20) — the reverse of
their own item numbering, since each turned out to need more novel machinery than the last:

- **1-4**: `getrandom` (`526`), `sysinfo` (`527`), `sigaltstack` (`528`), `pause` (`529`) — thin
  plumbing (`getrandom`/`sysinfo`/`sigaltstack`) plus `pause`'s own new block/wake-on-signal
  primitive (`BlockReason::WaitingForSignal`).
- **5**: `sigsuspend` (`530`) — reuses `pause`'s primitive plus a temporary `blocked_signals` swap,
  restored via `Process::sigsuspend_restore_mask` once `deliver_pending_signal` knows how the woken
  signal resolved.
- **6-10**: `timer_create`/`timer_settime`/`timer_gettime`/`timer_getoverrun`/`timer_delete`
  (`531`-`535`, `src/process/timers.rs`) — real per-process POSIX timers (`Process::posix_timers`,
  up to `MAX_POSIX_TIMERS = 8`), relative and `TIMER_ABSTIME` arming against
  `CLOCK_MONOTONIC`/`CLOCK_REALTIME`, real overrun accounting, delivered from
  `interrupts::timer_interrupt_handler` alongside the existing `ITIMER_REAL` scan. Not inherited by
  `fork`; disarmed and deleted by `execve`.
- **11-16**: `mq_open`/`mq_unlink`/`mq_timedsend`/`mq_timedreceive`/`mq_notify`/`mq_getsetattr`
  (`536`-`541`, `src/fs/mqueue.rs`) — real POSIX message queues, a separate name→queue namespace
  (not backed by `oxfs`), real priority-ordered delivery, real bounded blocking send/receive with a
  real `TIMER_ABSTIME`-shaped deadline (`BlockReason::WaitingForMqData`/`WaitingForMqSpace`, woken
  either by the matching send/receive or by the same timer-IRQ deadline scan the POSIX-timer batch
  added), and real `mq_notify`/`SIGEV_SIGNAL` delivery via `process::do_kill` directly (no bespoke
  delivery path — always a genuine self-signal, `caller_pid == target_pid`, so `do_kill`'s own
  cross-process permission check, see "Real-time signal queuing..." below, never applies here).
  `mq_close` isn't
  its own syscall: real `mq_close(3)` is a bare `syscall(SYS_close, mqd)`, so an mqd rides the
  ordinary `crate::fs::fd` registry exactly like a pipe/socketpair end.
  `third_party/musl/src/mq/mq_timedsend.c`/`mq_timedreceive.c` needed a real call-site patch (
  `oxidebsd` branch) — real Linux needs 5 syscall args and this ABI only carries 4, so `mqd`/`len`
  are packed into one register (high 32 bits = `len`, low 32 bits = `mqd`) rather than the usual
  "drop a redundant argument" trick, since nothing here is redundant to drop.
- **25-28**: `msgget`/`msgsnd`/`msgrcv`/`msgctl` (`550`-`553`, `src/fs/sysv_msg.rs`) — real SysV
  message queues, a genuinely different subsystem from `mq_*` above despite the similar name: an
  integer-`key_t`-addressed, fd-less namespace (`msgget` returns a bare id, not a real fd — no
  `crate::fs::fd` involvement at all; a queue lives from `msgget` until an explicit
  `msgctl(IPC_RMID)`, not tied to any process closing anything), real `ipc_perm` rwx-style
  permission checks on `msgsnd`/`msgrcv` plus a stricter owner/creator/root check on
  `msgctl(IPC_SET/IPC_RMID)`, real `msgtyp` selection semantics (positive exact match, negative
  smallest-type-within-bound, zero FIFO-any, `MSG_EXCEPT`), and real (not honest-zero) `stime`/
  `rtime`/`ctime` off `crate::cpu::rtc::unix_epoch_seconds()`. No timeout concept exists in real
  `msgsnd`/`msgrcv` at all (unlike `mq_timedsend`), so no timer-IRQ deadline scan was needed here.
  `third_party/musl/src/ipc/msgrcv.c` needed the same kind of call-site patch as `mq_timedreceive`
  (5 real args, packs `q`/`flag` into one register). **A real bug found and fixed during this
  session's own testing**: an early draft removed a matched message from the queue *before*
  checking whether the caller's buffer was big enough, silently destroying it on a real `E2BIG` —
  fixed by peeking the message's length first and only removing it once actually returning
  successfully, matching real Linux's "a too-big message stays queued for a retry" semantics.

- **21-24**: `semget`/`semop`/`semctl`/`semtimedop` (`546`-`549`, `src/fs/sysv_sem.rs`) — real SysV
  semaphores. Same `key_t` → id namespace shape `msgget` established (`KEYS`/`SETS`,
  `IPC_PRIVATE`/`IPC_CREAT`/`IPC_EXCL`), factored through a new shared `src/fs/sysv_ipc.rs`
  (`RawIpcPerm` + the owner/group/other rwx permission check every SysV IPC subsystem needs, now
  that a second one existed). **`semop`/`semtimedop` apply a whole `sembuf` array atomically**:
  simulated against a scratch copy of every touched value first — if every op can proceed, all
  commit at once; if any op would block, nothing applies and the caller either fails
  (`IPC_NOWAIT`) or blocks on that one op, retrying the entire array from scratch once woken (same
  block/`schedule()`/loop-and-recheck pattern every blocking primitive here already follows). A
  negative `sem_op` decrements (blocking below zero), positive always succeeds, zero blocks until
  exactly zero. `semtimedop`'s timeout is real-*relative* (unlike `mq_timedsend`'s absolute
  `at`), converted the same way `do_nanosleep` already does. Real `SEM_UNDO`: `Process::
  sysv_sem_undo` accumulates a signed adjustment per `(semid, semnum)`, applied back automatically
  on process termination via `apply_undo_for_exit` (called from `process::lifecycle::
  terminate_process` *before* `PROCESS_TABLE` is locked — that function takes the same lock itself
  to drain the list). A new `BlockReason::WaitingForSemOp(semid, semnum, waiting_for_zero,
  deadline)` backs real (not fabricated) `semctl(GETNCNT)`/`semctl(GETZCNT)` counts — found live
  by this sub-batch's own smoke test: an earlier draft woke every blocked waiter on *any*
  successful op regardless of which semaphore it touched, which broke `GETNCNT`/`GETZCNT`'s own
  accounting (a still-genuinely-blocked process on an untouched semaphore would transiently read
  back `Ready`) even though it was harmless to `semop`'s own correctness (a spuriously woken
  process just re-blocks). Verified via `tests/sysv_sem_syscall_smoke.rs` +
  `userland/sysv-sem-syscall-smoke/` — `IPC_CREAT`/`IPC_EXCL`/`ENOENT`/nsems-mismatch `EINVAL`;
  `SETVAL`/`GETVAL`/`SETALL`/`GETALL` plus `EFBIG`; a real atomic two-op `semop` plus `GETPID`;
  `IPC_NOWAIT` `EAGAIN`; a real `semtimedop` `ETIMEDOUT`; a `fork()`-driven block/wake pair
  exercising `GETNCNT`/`GETZCNT` against genuinely blocked processes plus real `SEM_UNDO`
  (auto-reversed on a child's exit without it ever explicitly undoing); `IPC_STAT`/`IPC_SET`/
  `IPC_RMID` with real post-removal `EIDRM`.
- **17-20, last of the batch**: `shmget`/`shmat`/`shmctl`/`shmdt` (`542`-`545`,
  `src/fs/sysv_shm.rs`) — real SysV shared memory, the one sub-batch needing genuine
  memory-management plumbing rather than just a `BTreeMap` namespace. `shmget` eagerly allocates a
  fixed `Vec<PhysFrame>` up front (same "hand out forward, never reclaim" policy `src/process/
  mm.rs`'s own `do_mmap` already establishes), zero-filled once (matching anonymous `mmap`'s own
  guarantee). **`shmat` is the actual proof of real shared memory**: every attach against the same
  `id`, by any process, maps *those exact same frames* (not fresh ones) into the caller's own page
  table at a freshly bump-allocated VA (`SHM_REGION_BASE = 0x_4000_0000_0000`, a window distinct
  from `mm.rs`'s own `MMAP_REGION_BASE`) — a write through one process's mapping is genuinely
  visible through another's. `shmaddr` is always ignored (`do_mmap`'s own `addr_hint`
  simplification); `SHM_RDONLY` omits `WRITABLE` on the caller's own mapping; the real mapped
  address is returned directly (this ABI's carry-flag success path already puts a handler's return
  value in `RAX`, matching real `void *shmat(...)`'s own convention with no special-casing).
  `shmdt` is the one syscall in this whole 28-item batch that actually touches page tables on the
  way out (`Mapper::unmap`), found via a new per-process `Process::sysv_shm_attach: Vec<(u64,
  i32)>` list of `(addr, shmid)` pairs. Real `IPC_RMID`-while-attached lifecycle: the key is
  unlinked from `KEYS` immediately (a fresh `shmget` on it can never find this segment again) but
  the segment and its real frames survive until the last attachment actually detaches. **Known,
  accepted simplification: not inherited across `fork`** — this kernel's own `fork` is a full
  eager address-space copy, not real copy-on-write, so a forked child's own page-table entries at
  a parent's shm addresses already point at freshly-copied private frames regardless of what this
  module does; `Process::sysv_shm_attach` simply starts empty in a forked child (same "starts
  fresh across fork" precedent `sysv_sem_undo` already established) rather than pretending to
  support real Linux's "child inherits attached segments" behavior. Real implicit detach — not
  just on process exit (`detach_all_for_exit`, same "before `PROCESS_TABLE` is locked" placement
  `apply_undo_for_exit` already established) but also on `execve`, right after the new
  `AddressSpace` is committed (the old one, and everything mapped into it, just became
  unreachable — matching real Linux's own `execve(2)` destroying the old address space).
  `RawShmidDs` (112 bytes) verified via the same direct `musl-gcc`/`sizeof`/`offsetof` probe rigor
  as every other `Raw*` wire struct in this batch. Verified via `tests/sysv_shm_syscall_smoke.rs`
  + `userland/sysv-shm-syscall-smoke/` — `shmget`'s `IPC_CREAT`/`IPC_EXCL`/`ENOENT`/oversized-size
  `EINVAL`; a real `shmat` mapping with `IPC_STAT` reporting real state (plus a safe read-only
  `SHM_RDONLY` exercise); **the core cross-process proof** — a `fork()`, then the child performs
  its own independent `shmat` against the same id (not an inherited mapping, per the
  simplification above) and reads back the parent's exact pattern, then writes its own pattern
  back before exiting, after which the parent confirms it sees the child's write through its own
  original mapping (real bidirectional sharing); `nattch` correctly unwound after both an explicit
  `shmdt` and a real process exit; a real `IPC_RMID`-while-attached lifecycle (a fresh `shmget` on
  the same key immediately gets a genuinely different id) followed by real `EIDRM` once the last
  attachment detaches; and a final `shmdt` to `nattch == 0` triggering immediate removal, followed
  by real `ENOENT`/`EINVAL`/`EIDRM`.

This closes out the whole 28-syscall pre-reserved batch (all 28 of 28 items done) — see
`docs/MISSING_POSIX_SYSCALLS.md`'s own "Pre-reserved batch: first" through "sixth implementation"
sections for the complete per-item write-up.

- **`getrandom`, thin plumbing, no new primitive**: `modules/posix_compat`'s `handle_getrandom` →
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
- **`sysinfo`, the confirmed live blocker for `free`/`uptime`'s primary numbers**
  (`procps/{free,uptime}.c`, per `docs/BUSYBOX_APPLETS.md`'s own `NEEDS_PROC` section) — not a
  POSIX interface at all (Linux-specific), but tracked in the same doc/batch for completeness.
  `modules/posix_compat`'s `handle_sysinfo` → `src/syscall/ffi.rs`'s `sys_sysinfo`/`RawSysinfo`.
  Real `(info_ptr)` wire format, no musl call-site patch needed.
- **`RawSysinfo` is 368 bytes** — confirmed via a direct C `offsetof`/`sizeof` probe against
  musl's real `struct sysinfo` rather than assumed from Rust `repr(C)` layout rules alone (an
  8-byte gap after `procs`/`pad` before `totalhigh` is real `unsigned long` alignment padding, not
  a missing field). `[u8; 256]` doesn't implement `Default` in this toolchain (only small arrays
  do), so the struct is built as a full field-literal rather than `#[derive(Default)]` +
  struct-update syntax.
- **Same honesty tier as `RawRusage`/`RawTms`**: real `uptime` (`ticks() / TIMER_HZ`, the same
  conversion `/proc/uptime`/`CLOCK_MONOTONIC` already use), real `totalram`
  (`memory::usable_ram_bytes()`, with `mem_unit = 1` so no further scaling is needed), real
  `procs` (the live process table's own length). `freeram` is set equal to `totalram` — same
  "no deallocation tracking exists anywhere in this kernel" reasoning `/proc/meminfo`'s own
  `MemFree == MemTotal` placeholder already documents. `loads`/`sharedram`/`bufferram`/
  `totalswap`/`freeswap`/`totalhigh`/`freehigh` are honest `0` — no load-average/page-cache/swap
  tracking exists, matching `/proc/meminfo`'s own `Buffers`/`Cached`/`SwapTotal`/`SwapFree`.
- Verified via `tests/sysinfo_syscall_smoke.rs` + `userland/sysinfo-syscall-smoke/` — same real-
  `SYSCALL` pattern. Three parts: a real call's fields, `uptime` non-decreasing across two calls,
  `totalram` stable across two calls.
- **`sigaltstack`, bookkeeping only, no new primitive**: `modules/signal`'s `handle_sigaltstack` →
  `src/syscall/ffi.rs`'s `sys_sigaltstack` → `src/process/signals.rs`'s `do_sigaltstack`, backed
  by a new `Process::altstack: AltStack` field (`src/process/mod.rs`). Real `(ss_ptr, old_ptr)`
  wire format, no musl call-site patch needed — musl's own `sigaltstack()` wrapper already filters
  a caller-supplied `SS_ONSTACK` bit and an undersized `ss_size` client-side before ever issuing
  the syscall (same "only reachable via X" precedent `getrandom`/`getentropy()` already has).
- **No signal is ever actually delivered at this stack's own address** — `SA_ONSTACK` still isn't
  honored by `deliver_pending_signal` (`src/syscall/mod.rs`), which always builds a handler's frame
  off the interrupted context's live `user_rsp` regardless. This is now a distinct gap from real
  handler *nesting*, which is fixed — see "Real signal-stack chaining" below — a real `sigaltstack`
  doesn't fix `SA_ONSTACK` by itself either way. `SS_ONSTACK` is therefore always reported unset on
  read-back, an honest reflection of what this kernel actually does rather than a fabricated "yes,
  active" answer.
- **Copied by `fork`** (a real `fork()` duplicates the whole address space, so the alt stack's own
  address stays valid in the child); **reset to disabled by `execve`** (the old program's address
  is meaningless in the new image, same reasoning `Process::fs_base` already uses).
- Verified via `tests/sigaltstack_syscall_smoke.rs` + `userland/sigaltstack-syscall-smoke/` — a
  real raw `syscall()` call, deliberately bypassing musl's own wrapper to exercise the kernel's
  `EINVAL` path directly. Five parts: the real POSIX startup state is disabled, installing a real
  alt stack succeeds, reading it back matches, a combined set+read-old call reports the prior
  state, and an invalid flag bit is `EINVAL` while disabling correctly zeroes `sp`/`size`.
- **`pause`, the first item in the batch needing a genuine new primitive**: `modules/signal`'s
  `handle_pause` → `src/syscall/ffi.rs`'s `sys_pause` → `src/process/signals.rs`'s `do_pause`.
  Real zero-argument wire format, no musl call-site patch needed. Unlike `getrandom`/`sysinfo`/
  `sigaltstack` (all thin plumbing over existing state), `pause(2)`'s real semantics — block until
  a signal is delivered that either terminates the process or invokes a caught handler — needed an
  actual new block/wake primitive: a new top-level `BlockReason::WaitingForSignal` variant, plus a
  new `wake_if_paused` helper called from both `do_kill`'s and `signal_foreground_group`'s own
  `Action::SetPending` arm (the only action shape that corresponds to "a caught handler will run"
  — an ignored signal or one with default-`Terminate`/`Stop` disposition already has its own
  existing immediate path, unaffected by the caller's `ProcState`).
- **`do_pause` checks the real wake condition (`pending_signals & !blocked_signals != 0`) before
  ever blocking** (avoiding a lost wakeup, same principle every other blocking primitive here
  follows) and **loops and re-checks after every wake** — the same discipline this codebase
  already audits for any cross-process force-wake mechanism (see the real Ctrl+Z/`nanosleep`
  regression the "Real job control" section above documents catching from the identical gap).
- **Real POSIX ordering falls out for free**: once `do_pause` finds a deliverable signal and
  returns `Err(EINTR)`, this codebase's own existing "deliver pending signals at the tail of every
  completed syscall" design (`deliver_pending_signal`) redirects the live frame into the caught
  handler *before* the caller's own `pause()` call site is ever observed to "return" — the handler
  runs, calls `sigreturn`, and only then does control resume at the original call site with
  `EINTR` already in `RAX`/`CF` — exactly matching real POSIX's "the handler runs before pause()
  returns" contract, using the exact same hijack mechanism `sa-siginfo-syscall-smoke` already
  proved for `tkill`.
- Verified via `tests/pause_syscall_smoke.rs` + `userland/pause-syscall-smoke/` — forks; the
  parent immediately calls `pause()` (genuinely blocks, forcing the scheduler to run the freshly
  forked child next); the child sends the parent a caught-disposition `SIGUSR1` (exercising the
  wake hook against a process actually sitting `Blocked(WaitingForSignal)`), then exits; the
  parent's `pause()` returns `EINTR` only after the handler has already run, and it reaps the
  child's clean exit via `wait4`. **Found and fixed live along the way**: this crate's own real
  writable static (`HANDLER_RAN: AtomicBool`) hit the exact same `elf.rs` PT_LOAD-segment-sharing-
  a-page issue documented above for `sa-siginfo-syscall-smoke` — same linker-script
  `ALIGN(0x1000)` workaround applied.

## Real preemptive scheduling (`src/process/scheduler.rs`, `src/cpu/interrupts.rs`, `src/cpu/fpu.rs`)

The scheduler is no longer purely cooperative. A process still leaves `Running` voluntarily by
calling `scheduler::schedule()` itself (`do_exit`/`do_wait4`/every other blocking primitive,
unchanged) — but it can now also be preempted: `interrupts::timer_interrupt_handler` calls that
same `schedule()` directly whenever it catches a process executing ring-3 (user-mode) code and a
time quantum (`PREEMPT_QUANTUM_TICKS = 4` ticks = 40ms at `TIMER_HZ = 100`) has elapsed since the
last check.

- **Deliberately scoped to ring-3 only, not full kernel preemption.** Checked via the interrupted
  frame's own `code_segment` RPL bits (`stack_frame.code_segment.0 & 0x3 == 3`), not any software
  flag. Kernel/syscall/module code is never preempted — `IA32_SFMASK` already clears `IF` for a
  `SYSCALL`'s entire duration (see the Syscall ABI section above), and nothing else in this
  codebase runs with `IF` set for long. This is the load-bearing scoping decision: user-mode code
  never holds a kernel `spin::Mutex` or touches module static state, so limiting preemption to
  ring-3 means **no existing critical section anywhere in this codebase needed auditing for
  preemption-safety** — the alternative (real kernel-mode preemption) would have required exactly
  that, a far larger and riskier undertaking.
- **The mechanism is unchanged `scheduler::schedule()`, called from a new site.** No raw-asm timer
  entry point was needed, despite `scheduler.rs`'s own module doc comment once predicting one would
  be (that prediction is now corrected in place) — `schedule()`/`switch_context` is just a
  stack-pointer-swap primitive agnostic to *why* the caller is yielding. Calling it directly from
  the existing `extern "x86-interrupt" fn timer_interrupt_handler` works unmodified: the preempted
  process's own compiler-generated interrupt-return sequence (culminating in a real `iretq`) sits
  dormant on *its own* kernel stack, below wherever `switch_context`'s `ret` suspended it, until
  this exact pid is picked again — at which point unwinding back up through `schedule()` and this
  handler's own call site reaches that dormant `iretq` naturally, restoring the interrupted ring-3
  context exactly as if the call had returned immediately. Each process having its own private
  kernel stack (already true before this work) is what makes this sound: nothing else ever touches
  that dormant memory while the process waits its turn.
- **EOI is sent before the possible `schedule()` call, not after** — load-bearing, not stylistic.
  `schedule()` can switch away to a different process for an arbitrary stretch of wall-clock time
  before this exact call returns; until the EOI is sent, the PIC still considers IRQ0 "in service"
  and won't deliver *any* further timer interrupt to *anyone* — freezing not just future preemption
  but every other `ticks()`-gated wakeup in `interrupts.rs` (sleepers, POSIX timers, `SIGALRM`,
  mqueue/semaphore timeouts, ...) permanently.
- **A real, previously-flagged correctness gap this closed**: `cpu::fpu.rs` had long documented
  that SSE/x87 register state was never saved/restored across a context switch, "fine only while at
  most one process is ever actually using SSE at a time without yielding mid-computation — true
  under the old cooperative-only scheduler... a real gap the moment two SSE-using processes could
  interleave." Cooperative yielding was always safe without this: a process only ever gave up the
  CPU at a real function-call boundary (a syscall), where the SysV ABI already forces the compiler
  to spill any XMM state it cares about to its own stack before the call. Real preemption breaks
  that: the timer can now interrupt a process at literally any instruction, including
  mid-computation with live XMM register content the compiler never spilled (no call boundary
  requires it). Fixed by adding `Process::fpu_state` (a 16-byte-aligned 512-byte `FXSAVE`/`FXRSTOR`
  area, `cpu::fpu::FxSaveArea`) — `scheduler::schedule`/`start` now `fxsave` the outgoing process
  and `fxrstor` the incoming one on **every** switch, not just a preemptive one (simpler and
  cheaper to reason about than special-casing cooperative vs. preemptive switches). A freshly
  spawned/forked process starts from `cpu::fpu::clean_state()` — a real CPU-reset image captured
  once via a genuine `fninit`+`fxsave` at boot (right after `fpu::init()` enables SSE), not a
  hand-guessed all-zero buffer (the x87 control word and `MXCSR` both have real nonzero hardware
  reset defaults, so an all-zero image isn't actually legal state). A forked child copies the
  parent's live `fpu_state` (real `fork()` semantics); `execve` currently leaves it untouched
  (real Linux resets FP state on `exec` for hygiene — not done here, a known minor gap, not a
  correctness issue since nothing depends on it).
- **A real regression found and fixed by this session's own full test-suite pass, not by review**:
  `userland/sysv-sem-syscall-smoke`'s part 6 (block/wake across `fork()`, exercising
  `GETNCNT`/`GETZCNT`) started failing intermittently — its own pre-existing source comment
  explicitly assumed the old cooperative guarantee ("the parent stays Ready, not yet re-scheduled,
  since this cooperative kernel only switches at the next blocking point"). Real preemption
  breaks exactly that assumption: a process waking another (a real `semop` V operation) no longer
  guarantees the waker keeps running uninterrupted until its own next blocking call — the woken
  process can now genuinely race ahead if the waker gets preempted first. Root-caused by
  reproducing reliably on this branch (100% pass on master, intermittent failure here) and tracing
  the exact `READY_QUEUE` reordering a preemption mid-sequence causes. Fixed in the test itself
  (not the kernel): the two one-shot `GETNCNT`/`GETZCNT` checks that depended on strict
  before/after ordering are now bounded polling loops (`poll_until`) — a pattern that was actually
  *unsound* under the old pure-cooperative scheduler (a busy-spinning process would never yield the
  CPU to let the other side make progress) and only becomes valid now that the timer forces
  fairness regardless of what a ring-3 loop is doing. No other test in the suite carried this same
  assumption (checked via a source-wide grep for similarly-worded scheduling-order comments before
  concluding this was the only one) — every other cross-process test already synchronized through a
  real blocking primitive rather than an implicit ordering assumption, and passed unmodified.
- **Verification**: `cargo build`/`cargo clippy` clean, no new warnings. Every integration test in
  the suite passes (two pre-existing, unrelated compile failures — `rtl8139_smoke`/`icmp_smoke`/
  `ping_smoke` reference a stale `oxidebsd::interrupts` import path that predates this session and
  needs `oxidebsd::cpu::interrupts` — were already broken on `master` before this work and are out
  of this session's scope), including `fork_wait`/every `*_syscall_smoke` test exercising real
  fork/signals/IPC/networking/musl/BusyBox/TinyCC userland through genuine preemptible execution.
  Not covered by automated testing: real-world responsiveness/fairness under sustained concurrent
  CPU-bound load (e.g. a background `yes > /dev/null &` while using the shell) — manual-QEMU-only,
  same category as every other interactive/timing-sensitive claim in this file.

## Real-time signal queuing and kill/sigqueue permission checking (`src/process/mod.rs`, `src/process/signals.rs`, `src/syscall/ffi.rs`)

Closes the Open POSIX Test Suite pilot's own "Real-time signal queuing" architecture blocker (see
`docs/POSIX_COMPLIANCE_CHECKLIST.md`), landed in two passes.

- **`SIGRTMIN..=SIGRTMAX` (`35..=64`)** — matches musl's own `sigrtmin.c`/`sigrtmax.c` exactly
  (`SIGRTMIN` hardcoded to `35`; `SIGRTMAX = _NSIG - 1 = 64`); `32..=34` stay permanently unclaimed,
  matching real glibc/musl convention of reserving a few RT numbers below `SIGRTMIN` for
  libc-internal use. `do_kill`/`do_sigqueue`/`sys_sigaction` all extend their range checks to accept
  it alongside the existing `1..=31` standard range. `Process::sigactions` grew `[SigAction; 32]` →
  `[SigAction; 65]` to cover the new indices.
- **`Process::pending_signals`/`blocked_signals` (plain `u64` bitmasks) needed zero changes** — bit
  `34..63` already worked with the existing `1 << (sig - 1)` arithmetic. The real gap was genuine
  multi-instance *queuing*: **`Process::rt_queue: [Vec<QueuedSigInfo>; RT_SIGNAL_COUNT]`** gives
  each RT signal number its own small fixed-capacity (`RT_QUEUE_CAP = 16`,
  `src/process/signals.rs`) FIFO — a second `sigqueue`/`raise` against an already-pending RT signal
  genuinely queues rather than merging into a single bit the way a standard signal still does (POSIX
  requires this distinction; standard signals staying bitmask-collapsed is explicitly permitted).
  Not copied by `fork` (starts empty, same "no meaningful state to carry over" reasoning
  `pending_siginfo` already has); untouched by `execve`.
- **`record_pending`** (`src/process/signals.rs`) is now RT-aware and fallible: `sig >= SIGRTMIN`
  pushes/pops `rt_queue` instead of overwriting `pending_siginfo`'s single slot, and returns
  `Err(EAGAIN)` once that signal's own queue is full — the real, documented `sigqueue(2)` errno for
  this case. `do_sigqueue` propagates it to the caller; `do_kill`/`signal_foreground_group` (whose
  own callers have no path to observe it, and for whom the pending bit is already set regardless)
  drop the excess instance instead. `take_deliverable_signal`/`do_sigtimedwait` only clear an RT
  signal's `pending_signals` bit once its own queue is actually empty — the key semantic difference
  from a standard signal's unconditional clear-on-consume.
- **Real `kill(2)`/`sigqueue(2)` permission checking** (`has_signal_permission`): sender must be
  root, or its own uid must match the target's (this kernel's single, non-diverging `Process::uid`
  covers the real POSIX "real or effective... shall match the real or saved" rule in one equality
  check) — checked by both `do_kill`/`do_sigqueue`'s single-target cross-process paths, **not**
  `signal_foreground_group`'s own process-group broadcast (no pilot test exercises group-kill
  permission semantics, and real POSIX's own per-member partial-success rule there is meaningfully
  more complex than a flat allow/deny). `do_sigqueue` also gained real `sig == 0` handling — the
  same null-signal existence(+permission)-only convention `do_kill`'s own `kill(pid, 0)` already had
  (previously a flat `EINVAL`, since `0` fell outside `do_sigqueue`'s own `1..=31` range check).
- **Verified**: `tests/rt_signal_syscall_smoke.rs` + `userland/rt-signal-syscall-smoke/` (multi-
  instance queuing/delivery count, real `EAGAIN` past `RT_QUEUE_CAP` + real FIFO drain via
  `sigtimedwait`, lowest-signal-number-first delivery order, partial-drain pending-bit semantics,
  `EINVAL` boundary validation) and `userland/sig-syscall-smoke/`'s own part 8 (real `ESRCH`/`EPERM`
  enforcement, including a forked, uid-dropped child). Pilot moved 40P/16F/8U/4UT → 52P/9F/3U/4UT
  across both passes (`sigqueue/1-1,2-1,2-2,3-1,5-1,6-1,7-1,11-1,12-1.c`, `sigwait/2-1.c`, and
  `kill/2-2,3-1.c` all UNRESOLVED/FAIL → PASS).
- **A real, separate, pre-existing gap surfaced by the RT work, not caused by it — since fixed, see
  "Real signal-stack chaining" below**: `deliver_pending_signal` used to only ever deliver **one**
  signal per completed syscall (no real signal stack to chain further handler redirects within one
  return path). Real POSIX code that unblocks several already-queued RT instances in a single call
  (`sighold()`/one `sigrelse()`, then immediately checking a counter) only ever saw one delivered —
  `sigqueue/4-1.c` (real FAIL) and `sigqueue/8-1.c` (UNRESOLVED) both hit this, confirmed not a
  queuing bug (`rt-signal-syscall-smoke`'s own part 1 originally passed the identical scenario only
  by explicitly working around this with extra pump syscalls, since removed now that the real fix
  makes them unnecessary).

## Real ring-3 fault-to-signal delivery, and two mmap fixes (`src/cpu/interrupts.rs`, `src/process/fault_trampoline.rs`, `src/process/mm.rs`, `modules/oxfs/`)

Closes the Open POSIX Test Suite pilot's `mmap/11-2.c`/`11-3.c`/`12-1.c` FAILs, and a much bigger
standing gap those tests happened to surface: **`interrupts::page_fault_handler` used to reboot the
whole kernel on any page fault, ring-3 or not.** A wild pointer deref in any userland/BusyBox/
TinyCC-compiled program took the entire VM down with it — no `SIGSEGV`, no isolation, nothing.

- **Real MPR-correct partial mapping**: `mm::do_mmap_file_backed` used to map every page in the
  caller's requested `len`, zero-filled, regardless of the underlying object's own real size — so a
  reference into a whole page mapped past a file's real (page-rounded) extent just silently
  succeeded against a zero page instead of raising `SIGBUS`, real POSIX "Memory Protection" (MPR)
  behavior. Fixed: `covered_pages = real_size.div_ceil(4096).min(page_count)` is the only range ever
  actually backed by real frames and mapped into the page table; `[covered_pages, page_count)` is
  deliberately left with **no page-table entry at all**. `MmapFileRegion` gained `mapped_pages`
  (frozen at `mmap()` time, not re-checked against the object's live size on every fault — a real
  Linux dynamically re-checks current inode size on each fault, letting a later `ftruncate()` grow
  retroactively into what SIGBUSed before; no pilot test needs that).
- **Real fault-to-signal delivery, from scratch**: `interrupts::page_fault_handler` now checks the
  interrupted frame's own CS RPL (`& 0x3 == 3`, same technique `timer_interrupt_handler`'s own
  preemption check already established) — a ring-0 fault is still an unconditional reboot (a real
  bug in this kernel itself, unchanged safety net), but a ring-3 fault now resolves a real signal
  (`mm::signal_for_user_fault`: `SIGBUS` for a reference into a live mapping's own reserved-but-
  unbacked tail, `SIGSEGV` for everything else) and records it pending via `do_kill`'s own
  self-signal path, then redirects the interrupted context to a **real, kernel-authored, user-
  executable trampoline page** (`process::fault_trampoline`, mapped at a fixed VA —
  `0x_1FFF_FFFF_F000`, directly below `mm::MMAP_REGION_BASE` — in every fresh address space,
  `process::spawn`/`do_execve`; a forked child gets its own copy for free via `AddressSpace::fork`'s
  existing eager copy).
  - **Why the fault handler can't just invoke the process's own handler directly, right there**:
    `extern "x86-interrupt"`'s compiler-generated entry/exit saves and restores every clobbered GPR
    itself, with zero Rust-visible fields for them — unlike `syscall::SyscallFrame`, built by hand
    in `syscall_entry`'s own raw asm, every GPR an explicit mutable field. A real handler call needs
    `RDI = signum` (and `RSI`/`RDX` for `SA_SIGINFO`) set directly; `InterruptStackFrame::as_mut()`
    only exposes `instruction_pointer`/`stack_pointer`/`cpu_flags`/the segment selectors, nothing
    else.
  - **The fix**: the trampoline is just `mov eax, SYS_FAULT_PUMP` (`554`, this ABI's own invention,
    continuing past `SYS_MSGCTL = 553`, never issued by real userland) `; syscall ; ud2`. Redirecting
    `instruction_pointer` there forces the process straight back through a **real** `SYSCALL`
    instruction — landing in `syscall_entry`'s already-correct, GPR-complete capture, and
    `syscall_dispatch`, which special-cases `SYS_FAULT_PUMP` exactly like `SYS_SIGRETURN` (bypasses
    `SYSCALL_TABLE` entirely, just calls `deliver_pending_signal` directly). That function then finds
    the signal the fault just queued and redirects the *real* `SyscallFrame` — the exact same
    machinery already proven correct for `kill()`-shaped delivery, reused verbatim. Default
    disposition (no handler) naturally resolves to `do_exit` from there too — a real fix in its own
    right, not just plumbing for the handler case: any ring-3 fault with no handler installed now
    cleanly terminates just the one offending process instead of rebooting the VM. The `ud2` tail is
    a real safety net (should never execute — a fault always queues something deliverable first) and
    also why a handler that "returns" or `sigreturn`s from a fault-delivered signal resumes into a
    hard stop rather than the (never actually valid) original faulting instruction — matches real
    POSIX's own "undefined behavior to return from a `SIGSEGV`/`SIGBUS` handler" stance; a real
    handler is expected to `_exit`/`longjmp` out, exactly what both the pilot's own
    `sigbus_handler` and this session's own smoke test's handler do.
- **A real, separate bug found chasing `mmap/12-1.c`, not actually about mmap at all**:
  `open(O_CREAT)` on a brand-new path defers allocating a real inode/directory entry until the fd's
  first commit (`ftruncate`/`fsync`/`close` — `OpenFile::Write`'s own `existing_inode: None` design,
  see the Filesystem section above). `unlink()`ing the path *before* that first commit used to find
  nothing to remove (no entry exists yet) and silently no-op — then the deferred commit went ahead
  and inserted the entry anyway, resurrecting a name real POSIX says must stay gone
  (`open -> unlink -> ftruncate -> close -> open` must `ENOENT` on the last `open`). Fixed:
  `OpenFile::Write` gained `unlinked: bool` — `oxfs_unlink`, finding no directory entry, now scans
  `OPEN_FILES` for a still-uncommitted `Write` fd matching `(parent_inode, name)` and sets this flag
  instead of `ENOENT`ing; `commit_write_buffer` still allocates a real inode and writes real content
  (the fd, and any mapping of it, keeps working — real Unix "unlinked but still open" semantics) but
  skips `dir_insert` when set.
- **Verified**: `tests/mmap_syscall_smoke.rs` + `userland/mmap-syscall-smoke/` — part 1 reproduces
  the `open -> unlink -> ftruncate -> mmap -> close` race and confirms the path stays `ENOENT`
  while the live mapping keeps working; parts 2-4 each run in their own forked child (a fault is
  fundamentally disruptive to whichever process it hits, so each gets an isolated child whose real
  `wait4`-reported exit status is the pass/fail signal): a real installed `SIGBUS` handler actually
  runs with the correct signal number (part 2), default-disposition `SIGBUS` on the same
  reserved-but-unbacked mmap tail cleanly terminates the child via signal `7` (part 3), and
  default-disposition `SIGSEGV` on an ordinary wild pointer cleanly terminates via signal `11`
  (part 4) — proving the `SIGSEGV` fallback independent of the mmap-specific path. `cargo build`/
  `cargo clippy` clean; the full existing test suite (`fork_wait`, `sig_syscall_smoke`,
  `rt_signal_syscall_smoke`, `needs_syscall_smoke`, `sysv_shm_syscall_smoke`,
  `session_syscall_smoke`, `mount_syscall_smoke`, `sa_siginfo_syscall_smoke`,
  `pause_syscall_smoke`) still passes unmodified.
- **Not covered by this pass**: `SA_SIGINFO` handler invocation from a fault (only the plain
  1-argument shape is exercised — no pilot test or real caller needs `siginfo_t`/`ucontext_t` from a
  fault yet, and the machinery `deliver_pending_signal` already has for that case applies unchanged
  once one does); real dynamic re-check of a grown file's size against an already-`mmap()`'d
  region's own `mapped_pages`.

## Three more pilot fixes: signal-interruptible `nanosleep`, real `FD_CLOEXEC`, real per-process CPU-time clocks (`src/cpu/rtc.rs`, `src/process/timers.rs`, `src/process/signals.rs`, `src/fs/fd.rs`, `modules/oxfs/`, `src/syscall/ffi.rs`, `src/process/mod.rs`, `src/cpu/interrupts.rs`)

Continued working the same 68-file Open POSIX Test Suite pilot subset after the mmap pass above —
three more real, independent gaps, closing `nanosleep/1-1.c`/`1-3.c`/`2-1.c`,
`shm_open/11-1.c`/`13-1.c`, and `clock_gettime/4-1.c`.

- **Real sub-second `CLOCK_REALTIME`** (`src/cpu/rtc.rs`'s `unix_epoch_now_precise`): `tv_nsec` used
  to be hardcoded `0` (whole-second RTC precision only), so `nanosleep/1-1.c`/`2-1.c` — which sleep
  for as little as a handful of nanoseconds and then check that `clock_gettime` observed *some*
  elapsed time — failed regardless of whether the real sleep duration was correct, since the RTC's
  own 1 Hz second boundary almost never happened to land inside the sleep. Fixed by calibrating a
  fixed `ticks() -> real seconds` offset against the RTC exactly once (lazily, `spin::Once`), then
  deriving every later `CLOCK_REALTIME` reading purely from `interrupts::ticks()` against that base
  — the same technique `CLOCK_MONOTONIC` already uses, just shifted by a real wall-clock epoch.
  Deliberately *not* a fresh RTC read on every call any more: mixing a fresh RTC second-boundary
  read with a `ticks()`-derived sub-second offset that isn't phase-locked to that same boundary
  would make `tv_sec`/`tv_nsec` disagree with each other (a real backward jump whenever they do) —
  a single calibration point avoids that by construction. `unix_epoch_seconds` (whole seconds) is
  kept as-is for SysV IPC's own `stime`/`rtime`/`ctime`, which never needed sub-second precision.
- **Real signal-interrupts-sleep** (`process::timers::do_nanosleep`): real POSIX requires a signal
  that will invoke a caught handler to interrupt `nanosleep()` early (`EINTR`, real remaining time
  written back), not just sit blocked until the deadline naturally passes — `nanosleep/1-3.c` forks
  a child that sleeps 30 real seconds and expects a `SIGABRT` sent 1 second in to cut that short.
  `do_nanosleep`'s own blocking loop now checks `pending_signals & !blocked_signals != 0` right
  before each re-block (same "avoid a lost wakeup" reasoning `do_pause`'s identical check already
  establishes) and returns `EINTR` with the real remaining time if so — `deliver_pending_signal`
  (this syscall's own tail) then resolves what actually happens next. This alone wasn't enough:
  `record_pending`'s `Action::SetPending` path only ever set the bit, leaving a genuinely `Blocked`
  process waiting out its full deadline regardless (the loop's own check never gets a chance to run
  again until the process is rescheduled). Fixed with a new `signals::wake_if_sleeping` (mirroring
  `wake_if_paused`'s existing shape exactly), wired into the same three `Action::SetPending` call
  sites `wake_if_paused`/`wake_if_sigwaiting` already are (`do_kill`, `signal_foreground_group`,
  `do_sigqueue`).
- **Real per-`(pid, fd)` `FD_CLOEXEC`** (`src/fs/fd.rs`'s `CLOEXEC` set, `set_cloexec`/`is_cloexec`,
  the new `oxidebsd_set_fd_cloexec` kernel-API export): `F_GETFD`/`F_SETFD` used to be a pure no-op
  (`fcntl`'s own doc comment used to say so explicitly) — `shm_open/11-1.c` reads the flag back via
  `fcntl(fd, F_GETFD)` right after `shm_open()`, which (per `third_party/musl/src/mman/shm_open.c`)
  always passes real `O_CLOEXEC` straight to `open()`. Real per-`(pid, fd)` scoping (not per-`real_fd`
  the way `O_NONBLOCK` is) — POSIX defines this as a property of the descriptor itself, not the
  underlying open file description — so `dup`/`dup2` deliberately don't copy it (only
  `F_DUPFD_CLOEXEC`, now real, sets it on the fresh fd explicitly) while `fork_inherit` does. `oxfs_open`
  sets it directly from the real `O_CLOEXEC` open flag (`modules/oxfs`'s own `O_CLOEXEC = 0o2000000`
  constant, distinct from `fcntl`'s `FD_CLOEXEC = 1`). **Real enforcement, not just a readback**:
  `do_execve` now calls a new `fs::fd::close_cloexec` right after the new program image is
  committed — the first real close-on-exec behavior this kernel has ever had.
- **Real per-fd access-mode enforcement on write/`ftruncate`/`fallocate`** (`OpenFile::Write::readonly`
  in `modules/oxfs`): `shm_open/13-1.c` opens a *brand-new* object `O_RDONLY|O_CREAT` and expects
  `ftruncate()` on it to fail `EINVAL` — but `oxfs_open`'s create-path branch has always
  unconditionally produced a real, writable `OpenFile::Write` regardless of the caller's actual
  requested access mode (needed regardless, to support the deferred-commit design for a brand-new
  file — see `existing_inode`'s own doc comment), and nothing re-checked that mode again later. Fixed
  by recording the caller's real requested mode (`flags & O_ACCMODE == 0` for `O_RDONLY`) in a new
  `readonly` field, checked by `oxfs_write`/`oxfs_ftruncate`/`oxfs_fallocate` (`EBADF`/`EINVAL`
  respectively) — the existing-path branch's own `want_write` check already covered the
  already-exists case correctly, only the create-path branch was gapped.
  **A real regression found immediately by the full test suite, not by review**: several existing
  userland smoke-test crates (`access-syscall-smoke`, `mount-syscall-smoke`, `needs-syscall-smoke`,
  `needs-syscall2-smoke`, `oxfs-persistence-syscall-smoke`, `uid-syscall-smoke`) and `stsh`'s own
  `write` command all called `open(path, O_CREAT)` with no explicit `O_WRONLY`/`O_RDWR` bit,
  relying on this kernel's own prior permissiveness to still get a writable fd back — real,
  latent bugs in each (real POSIX/Linux `open(O_CREAT)` alone genuinely means read-only, no
  exceptions; no real production C code, including everything BusyBox/musl/tcc actually ship,
  makes this mistake) that this fix's own correctness surfaced for the first time. Fixed by adding
  the missing explicit access-mode bit to each of those six real call sites.
- **Real per-process CPU-time accounting** (`Process::cpu_ticks`): `clock_gettime/4-1.c` went
  `UNRESOLVED`, not the legitimate `UNSUPPORTED` its own `sysconf(_SC_CPUTIME) == -1` escape hatch
  would have produced — musl's own `sysconf()` unconditionally reports `_SC_CPUTIME` as supported
  (a compile-time claim, `third_party/musl/src/conf/sysconf.c`, not a runtime capability probe), so
  the test proceeded to call `clock_gettime(CLOCK_PROCESS_CPUTIME_ID, ...)` and hit a real,
  until-now-unhandled `EINVAL`. Fixed with a new `Process::cpu_ticks: u64`, incremented by exactly
  `1` on every timer tick where that process is the one actually `Running` when the tick lands
  (`interrupts::timer_interrupt_handler`, alongside its own preemption check) — tick-granularity,
  doesn't distinguish user/kernel time or subtract time spent `Blocked`/`Stopped`, the same honesty
  tier `ticks()`-derived `CLOCK_MONOTONIC` already sets, but enough for what this pilot test (and
  any real caller) actually checks: successive reads non-decreasing under real work.
  `CLOCK_PROCESS_CPUTIME_ID`/`CLOCK_THREAD_CPUTIME_ID` both read this same counter (no real
  threading exists — a process's only "thread" is itself, same simplification `/proc/<pid>/task`
  already uses). Starts at `0` for both `spawn` and a forked child (real POSIX: CPU time never
  carries over from a parent); preserved by `execve`.
- **Verified**: `cargo build`/`cargo clippy` clean. Re-ran the full pilot after each of the three
  fixes — `nanosleep/1-1,1-3,2-1.c` and `shm_open/11-1,13-1.c` and `clock_gettime/4-1.c` all flip
  FAIL/UNRESOLVED → PASS with zero regressions elsewhere each time (byte-for-byte identical
  FAIL/UNRESOLVED/UNTESTED lines otherwise). Pilot moved 52P/9F/3U/4UT (this doc's own prior
  baseline) → **61P/1F/2U/4UT/0TIMEOUT/0CRASH** across this whole session (the mmap pass above plus
  these three). Also re-ran `itimer_syscall_smoke`/`posix_timer_syscall_smoke` (both touch real
  timer/clock machinery) and the broader fd/exec-touching suite (`needs_syscall_smoke`,
  `needs_syscall2_smoke`, `mount_syscall_smoke`, `uid_syscall_smoke`,
  `oxfs_persistence_syscall_smoke`, `access_syscall_smoke`, `fork_wait`) — all still pass.

## Real signal-stack chaining, closing the pilot's last 2 (`src/process/mod.rs`, `src/process/signals.rs`, `src/syscall/mod.rs`)

Closes the "one signal per completed syscall" gap the RT-signal-queuing and mmap/fault-delivery
sections above both flagged and left unfixed — the pilot's remaining `1F/2U` (`sigqueue/4-1.c`
FAIL, `sigqueue/8-1.c` UNRESOLVED). Both tests `sighold()` (block) `SIGRTMIN`, `sigqueue()` it 5
times (all genuinely queued — RT queuing already worked), then `sigrelse()` (unblock, one single
`sigprocmask` syscall) and immediately check that the handler ran all 5 times, with **no syscall
in between** the unblock and the check. `deliver_pending_signal` only ever delivered one signal
per completed syscall, so only the first of the 5 already-queued instances was ever running by the
time the test's own check executed.

- **`Process::signal_saved_frame: Option<SyscallFrame>` + a separate `signal_saved_blocked: u64`
  became a real stack**: `Process::signal_stack: Vec<SignalStackFrame>` (`SignalStackFrame { saved:
  SyscallFrame, blocked_before: u64 }`, `src/process/mod.rs`). `stash_signal_context` now pushes an
  entry per `Handler`-disposition delivery instead of overwriting a single slot; `take_signal_saved_
  frame` (`sigreturn`'s own logic) pops exactly one. Copied (`.clone()`, a `Vec` of `Copy` structs)
  by `fork`, same "child gets its own independent copy of in-progress-handler bookkeeping" reasoning
  the old single-slot field already had; untouched by `execve`, also unchanged from before (the old
  program's own in-progress handler state is meaningless post-exec, but nothing ever reads it once
  the new image is running either, so leaving it stale is harmless).
- **The actual chaining mechanism**: `do_sigreturn` (`src/syscall/mod.rs`) used to be a hard early
  return — restore the one saved frame, done, bypassing `syscall_dispatch`'s normal
  `deliver_pending_signal` tail entirely. It now calls `deliver_pending_signal(frame)` itself,
  immediately after popping and restoring an entry, *before* treating that restored state as final.
  If another signal is deliverable right now (another already-queued instance of the signal whose
  handler just returned, or a different signal that only became unblocked once this handler's own
  extra `blocked_signals` bits were lifted by the pop), `deliver_pending_signal` redirects `*frame`
  into that next handler instead — pushing a fresh `signal_stack` entry on top, exactly like the
  first delivery did. Only once a `sigreturn` finds genuinely nothing else deliverable does the
  popped state actually resume as real userspace execution. Concretely, for the two failing tests:
  the unblocking `sigprocmask` syscall's own tail delivers instance 1 (pushing the post-`sigprocmask`
  return state); handler 1 runs and `sigreturn`s; that `sigreturn` finds instance 2 still queued and
  chains into handler 2 instead of resuming; this repeats through all 5 instances; only the 5th
  `sigreturn` finds the queue empty and actually resumes execution right after `sigprocmask`
  returned — with the handler already having run 5 times by then. No new primitive, no recursion
  (`do_sigreturn` and `deliver_pending_signal` are two flat function calls, not mutually recursive
  through the call stack) — just `Process::signal_stack` growing and shrinking on the kernel heap
  across what's now potentially several real `SYSCALL`/`SYSRETQ` round trips per original syscall
  return, each one a genuine trip through a real userspace handler.
- **`set_signal_saved_blocked_override`** (the `do_sigsuspend`-wakeup-into-a-caught-handler special
  case — see the Signal handling module section) now overrides the *top* `signal_stack` entry's
  `blocked_before` field (`.last_mut()`) instead of a single struct field — it's always called
  immediately after `stash_signal_context` pushed that exact entry, so the target is unambiguous.
- **Same fix, no special-casing needed, for the general "second signal during handler execution"
  gap** the Signal handling module section used to flag separately (a second signal becoming
  deliverable while a handler from a *different* signal is still running, not just multiple
  instances of the same RT signal) — that was always the same single-slot-vs-stack problem under a
  different trigger, and falls out of this fix for free: `deliver_pending_signal` is called from
  both `syscall_dispatch`'s own tail and now `do_sigreturn`, so any deliverable signal found at
  either call site chains the same way, regardless of whether it's the same signal number repeating
  or a genuinely different one.
- **Verified**: pilot moved 61P/1F/2U/4UT (this doc's own prior baseline) →
  **64P/0F/0U/4UT/0TIMEOUT/0CRASH, 68 total** — `sigqueue/4-1.c`/`8-1.c` both flip to real PASS,
  zero regressions elsewhere (checked via the automated `tests/posix_conformance_smoke.rs` +
  `userland/posix-conformance-driver/`, not a manual QEMU run — this pilot has been fully
  automated since that driver crate landed, superseding the interactive
  `sh /posix_conformance.sh` path `modules/oxfs/src/posix_conformance.sh`'s own doc comment still
  describes as the only way to run it). `userland/rt-signal-syscall-smoke/`'s own part 1/3 no
  longer need the explicit multi-syscall "pump" workaround they originally shipped with (removed) —
  the check right after the unblocking `sigprocmask` call, with zero syscalls in between, now
  passes directly, proving the chain happens within that one call's own tail rather than needing
  extra syscalls to keep draining it. `cargo build`/`cargo clippy` clean (no new warnings beyond
  three pre-existing ones unrelated to this change). Full existing signal-touching suite re-run
  clean: `rt_signal_syscall_smoke`, `sig_syscall_smoke`, `sa_siginfo_syscall_smoke`,
  `sigsuspend_syscall_smoke`, `sigaltstack_syscall_smoke`, `pause_syscall_smoke`,
  `mmap_syscall_smoke` (exercises the fault-to-signal `Terminate` path through the same
  `deliver_pending_signal`), `fork_wait`.
- **Not covered by this pass**: a bound on `signal_stack`'s own depth. Real POSIX doesn't bound
  nesting depth either, and genuine growth is already bounded by how many signal instances can be
  pending at once (`Process::rt_queue`'s own fixed `RT_QUEUE_CAP` per RT signal, bitmask collapse
  for standard ones) — a process can't manufacture unbounded chain depth by re-raising signals from
  inside their own handlers faster than that, but this wasn't specifically stress-tested.

## POSIX conformance pilot expanded 68 → 488, plus real frame reclaim and four real bugs it found (`build.rs`, `Cargo.toml`, `src/memory/`, `src/process/`, `src/fs/mqueue.rs`, `src/fs/sysv_shm.rs`, `src/cpu/interrupts.rs`, `modules/oxfs/`, `modules/posix_compat/`)

Grew the Open POSIX Test Suite pilot (see "POSIX conformance pilot" sections above and
`docs/POSIX_COMPLIANCE_CHECKLIST.md`'s own "Verification" section) from its original curated
68-file subset to 488, then fixed four real, independent kernel bugs the larger corpus's own real
`fork`/`execve`/signal/IPC traffic surfaced — none of them reachable by the smaller original set.
Final baseline: **329 PASS / 62 FAIL / 40 UNRESOLVED / 8 UNSUPPORTED / 45 UNTESTED / 3 TIMEOUT /
1 CRASH, 488 total** (the one CRASH is `strftime/2-1.c`'s own real stack-buffer overflow — an
upstream test bug, correctly caught by musl's stack protector and now cleanly delivered as
`SIGSEGV`, see below — not a kernel bug).

- **Corpus expansion methodology**: every non-`pthread_*`, non-`aio_*`/`lio_listio` directory under
  `conformance/interfaces/` (real POSIX threading/AIO are tied to this project's still-unstarted
  "Real threading" foundational blocker, see the compliance checklist) was probed — every `.c` file
  not referencing `pthread_create`/`testfrmw.h` compiled clean against the real static `musl-gcc`
  sysroot. Most of these directories are machine-generated per-assertion families (`gentests.pl`)
  where `N-2.c`, `N-3.c`, ... only vary *which* signal/parameter the same assertion `N` is checked
  against (confirmed by diffing `sigaction/8-2.c` against `8-3.c` — identical assertion, different
  signal) — deduplicated to the lowest-numbered variant per assertion number (`sigaction` alone
  drops from 420 candidate files to 16 this way). Files ending `-buildonly.c`/`-core-buildonly.c`
  are also excluded — they expect a real `argv[1]` selecting a sub-case, normally supplied by the
  suite's own multi-invocation driver script this pilot doesn't have; run with none they just
  return `PTS_UNRESOLVED` unconditionally. See `build.rs`'s own `POSIX_TEST_PILOT_FILES` doc
  comment for the full list of newly-covered interfaces (every basic signal-set/mask/action
  function, POSIX per-process timers, the rest of the message-queue family, `munmap`/`shm_unlink`,
  the `sched_*` field-accessor family, unnamed/anonymous `sem_*`, `fsync`/`killpg`, the rest of the
  `clock_*` family, plain time-conversion libc functions, and `mlock`/`mlockall`/`munlock`/
  `munlockall` as expected-failure controls).
- **Real per-address-space frame reclaim, closing the "no frame deallocation anywhere" gap for the
  common case** — not directly one of the four bugs below, but the necessary prerequisite:
  hundreds of real `fork`+`execve`+`exit` cycles in one continuous boot (several per pilot file)
  exhausted the old 128 MiB heap ceiling around the ~140th file, `alloc::alloc::handle_alloc_error`
  on an 80 MiB request against an increasingly fragmented heap. Real, permanent frame reuse was
  the actual fix (a bigger heap/RAM ceiling alone was tried first and genuinely helped, kept as
  real headroom, but the underlying leak is now real):
  - **`memory::BootInfoFrameAllocator` gained a real `FrameDeallocator` impl** — an intrusive,
    singly-linked free list stored *in the freed frames themselves* (each freed frame's own first
    8 bytes, viewed through the phys-mem-offset window, hold the previous free-list head's address,
    or `u64::MAX` as a real "list ends here" sentinel), not a `Vec<PhysFrame>` — deliberately: this
    allocator is constructed *before* `allocator::init_heap` (see its own doc comment's
    already-documented chicken-and-egg trap), and only ever needs to *store* into the free list
    well after boot (real teardown only happens during process exit) — an intrusive list is
    unconditionally heap-free by construction rather than "safe in practice."
  - **`memory::address_space::AddressSpace::teardown`** walks and frees every `USER_ACCESSIBLE`
    frame beneath an about-to-be-discarded address space's own level-4 table — both leaf data
    (ELF image/stack/heap/private-anon-mmap) and every intermediate page-table structure frame
    (PDPT/PD/PT) — then the level-4 frame itself. Safe because every page-table *structure* frame
    at any level is always freshly allocated per address space, never shared (`copy_table_level`'s
    own doc comment already establishes this — a kernel-only, non-`USER_ACCESSIBLE` entry is the
    only thing ever aliased across address spaces, and this walk never recurses into one), and
    fork is a real eager copy, never COW, so a private leaf is never shared with a parent either.
  - **`SHARED_LEAF`** (a repurposed hardware-ignored PTE bit, `PageTableFlags::BIT_9`) is what
    makes this safe for the two real exceptions that *do* alias a leaf across address spaces: SysV
    `shmat` (`fs::sysv_shm::do_shmat`) and real fd-backed `MAP_SHARED` mmap
    (`process::mm::do_mmap_file_backed`) both mark every leaf they map with it; `teardown`'s own
    walk checks and skips any leaf carrying it. Deliberately *not* relying on those regions being
    already-unmapped by the time teardown runs — `fs::sysv_shm::detach_all_for_exit`'s own doc
    comment explicitly documents that a real process exit never unmaps shm PTEs at all (only
    `nattch`-decrements), so a page-table walk can genuinely still find them present.
  - Wired into both real discard sites: `process::lifecycle::do_execve`'s old-address-space
    discard (captured via `core::mem::replace` right after `new_address_space.activate()` already
    switched `CR3` away — always safe, never the active table) and process reaping in `do_wait4`
    (a reaped process is, by definition, not currently `Running` — it already stopped running back
    when it called `do_exit`).
  - Verified against the full regression suite, not just the pilot — `mmap_syscall_smoke`,
    `sysv_shm_syscall_smoke`, `dynlink_syscall_smoke` (heavy real `PT_INTERP`/`execve` churn), and
    `tcc_syscall_smoke` all still pass unmodified.
  - **Also bumped, same session, real headroom not just enough to barely finish**:
    `allocator::HEAP_SIZE_CEILING` 128 → 1024 MiB, `Cargo.toml`'s QEMU `-m` 1024 → 8192 MiB (paired,
    same 1/8-of-RAM scaling), `modules/oxfs`'s `NUM_BLOCKS` 8192 → 16384 (32 → 64 MiB block pool —
    the expanded pilot corpus's own embedded content alone runs ~14 MiB) and `MAX_INODES` 1024 →
    2048 (the corpus adds ~500 new files/dirs under `/posix-tests/bin/`). `build.rs`'s own mirrored
    `OXFS_NUM_BLOCKS`/`OXFS_MAX_INODES` constants (real disk image sizing) kept in sync, same
    "two things that must agree, flagged rather than shared" discipline as always. `Cargo.toml`'s
    `test-timeout` 1800 → 7200 (7x more files, real headroom for a corpus this size).
- **Bug 1 — a ring-3 `#GP` rebooted the whole VM instead of just killing one process.** Found via
  `strftime/2-1.c`, which has a real stack-buffer overflow (declares `char text[20]` but tells
  `strftime` it can write up to 256 bytes — an upstream test bug, not this kernel's). Correctly
  caught by musl's real stack-protector, whose `__stack_chk_fail` on x86 is a bare `hlt` (real,
  portable, deliberate musl design — `a_crash()` — relying on the OS turning a privileged-
  instruction fault into a signal). `general_protection_fault_handler` never had the ring-3 check
  `page_fault_handler` already has; fixed by giving it the identical treatment — ring-3 `#GP` now
  records a real `SIGSEGV` via `do_kill`'s self-signal path and redirects through
  `process::fault_trampoline`, the same machinery "Real ring-3 fault-to-signal delivery" above
  already established for page faults. No fault-specific signal distinction needed here the way
  page faults split `SIGBUS`/`SIGSEGV` — real Linux maps every userland `#GP` to `SIGSEGV`
  uniformly.
- **Bug 2 — `mq_receive`/`mq_send`'s blocking wait had no signal-interrupt path at all.** Found via
  `mq_receive/13-1.c`: parent blocks in a plain (non-timed) `mq_receive()` on an empty queue, child
  sends `SIGABRT` after 2s expecting `EINTR` — instead, a permanent hang (blocking on `u64::MAX`,
  see `BlockReason::WaitingForMqData`'s own doc comment), invisible to `t0`'s own `alarm(40)`
  rescue since the *process* never returns from the syscall at all for that to interrupt. Fixed
  exactly like `do_pause`/`do_nanosleep` already are: `do_mq_timedreceive`/`do_mq_timedsend` now
  check `pending_signals & !blocked_signals != 0` before (re-)blocking and return `EINTR`, plus a
  new `signals::wake_if_mq_waiting` hook (mirroring `wake_if_sleeping`'s exact shape) wired into
  all three `Action::SetPending` call sites (`do_kill`, `signal_foreground_group`, `do_sigqueue`).
- **Bug 3 — real `futex(2)` support doesn't exist, and musl's own retry logic turns that into an
  infinite busy-loop, not a clean error.** Found via `sem_wait/7-1.c` (a real *unnamed* POSIX
  semaphore block, distinct from the SysV `semop`-backed semaphores this kernel already implements
  for real — see `fs::sysv_sem`) — 100% CPU, no progress, for as long as it was left running.
  Root-caused through `third_party/musl/src/thread/__timedwait.c`: `sem_timedwait` issues exactly
  one raw `futex(FUTEX_WAIT, ...)` syscall and only treats the result as a real failure if it's
  `EINTR`/`ETIMEDOUT`/`ECANCELED` — any *other* value, including the `ENOSYS` this number fell
  through to unregistered, silently folds into `0` ("spurious wake, try again"), and the outer loop
  immediately retries forever with no actual blocking syscall or scheduler yield anywhere in it.
  Invisible to `t0`'s own rescue alarm for the same doubled reason `sigwait/4-1.c`'s exclusion
  already documents (`fork` never inherits a pending alarm) *plus* `wait4()`'s own missing
  signal-interrupt-wake gap (same bug class as Bug 2, left open here — no live pilot test currently
  needs it fixed). Fixed with a minimal, **honest failure stub**, not real futex support (that's
  tied to the same unstarted real-threading work `docs/POSIX_COMPLIANCE_CHECKLIST.md` already
  flags) — `process::do_futex` (`src/process/limits.rs`), registered directly at real Linux's own
  unclaimed `__NR_futex = 202` (`modules/posix_compat`, same "confirmed unassigned in this ABI's
  own registry" reasoning `SYS_SCHED_GETAFFINITY` already established): `FUTEX_WAIT` returns a real
  `ETIMEDOUT` (a genuine, unremarkable Linux return value) instead of a fake `0`, which is exactly
  what makes `__timedwait_cp` treat it as a real failure instead of retrying forever. `FUTEX_WAKE`
  and anything else just succeeds — no real caller in this musl fork ever checks that return value.
- **Bug 4 — a cross-process signal-default-terminate could panic the whole kernel via a stale
  scheduler ready-queue entry.** Found via `sigaction/9-1.c`: `select(0, NULL, NULL, NULL, NULL)`
  isn't implemented (`select`/`pselect` deliberately skipped, `poll` already covers every live
  caller — see `docs/MISSING_POSIX_SYSCALLS.md`) and `ENOSYS`s immediately rather than truly
  blocking, so the test's forked child cycles through `Ready` far more often than a real system's
  `select()` ever would — Bug 3's own `futex` fix made an *unrelated* narrow race far easier to
  hit, not the root cause itself. `do_kill`'s cross-process `Action::Terminate` branch (a
  default-disposition signal killing a target the caller isn't currently running) called
  `terminate_process` directly, unlike the adjacent `Action::Stop` branch right below it, which
  already dequeues a `Ready` target from `scheduler::READY_QUEUE` first (see that branch's own
  comment). If the target was `Ready` and queued at the exact moment the terminate fired, it
  became `Zombie` while still queued; if its parent (`do_wait4`) reaped it before the scheduler
  ever popped that stale entry, `PROCESS_TABLE` no longer had the pid at all by the time
  `scheduler::activate_and_prepare` tried to switch to it — a real panic (`"pid missing from
  table"`), not just a logic bug, taking the whole VM down. Fixed by adding the same
  `scheduler::remove_ready(pid)` call directly into the shared `terminate_process` (unconditional,
  not gated on `state == Ready` the way `Action::Stop`'s own check is — cheap, and `do_exit`'s own
  self-termination call site is never `Ready` when it calls this, so it's a harmless no-op there).
- **Two more pilot-corpus exclusions, same "structurally incompatible with this pilot's own
  infrastructure" precedent `sigwait/4-1.c` already established** (not kernel bugs):
  - `timer_settime/2-1.c`/`6-1.c`/`9-1.c` all `sigprocmask(SIG_BLOCK, {SIGALRM}, NULL)` before
    their own real `sigwait()`-based test loop — blocking `SIGALRM` in-process blocks *both* the
    signal-under-test *and* `t0`'s own outer rescue alarm, since they share one process/signal mask
    once `t0` `execvp`s straight into the test. Confirmed hanging past a real 9+ minute ceiling.
    `1-1.c`/`3-1.c`/`5-1.c`/`13-1.c` use `SIGALRM` too but only via a real *caught handler*, which
    still runs and lets the test's own state machine complete normally — only the three blocking
    ones are excluded.
  - `shm_open/23-1.c` unconditionally forks `NPROCESS = 1000` real child processes with no
    CPU-count scaling (unlike `sched_setparam`'s own `nb_cpu = get_ncpu()`-scaled fork loops
    elsewhere in this corpus, harmless on this single-core kernel). Confirmed live: this test
    genuinely times out under `t0`'s 40s alarm, real behavior for a kernel this small — but when
    the *parent* dies to that alarm's default-`Terminate` `SIGALRM`, any children it had already
    spawned become permanent orphans (this kernel has no init-style orphan reparenting/reaping —
    `hush` never adopts or waits on a process it didn't itself fork), each still holding whatever
    real `shm_open()` fd it successfully opened. `modules/oxfs`'s own deliberately-small
    `MAX_OPEN_FILES = 8` pool permanently exhausted as a result, breaking every subsequent pilot
    file needing to open *anything* for the rest of that run (`hush` itself started failing its own
    `> /posix-tests/run-out.txt` redirect with a real `EMFILE`). Not a kernel fd-leak — `close_all`
    correctly releases every fd the *parent* itself held; the real gap is orphaned *children* this
    kernel has no mechanism to ever reap, a structural limitation, not a bug to fix here.
- **Verification**: `cargo build`/`cargo clippy` clean throughout (no new warnings beyond the
  existing pre-session baseline). Full regression suite re-run clean after every fix, not just at
  the end: `fork_wait`, `uid_syscall_smoke`, `mmap_syscall_smoke`, `sysv_shm_syscall_smoke`,
  `dynlink_syscall_smoke`, `tcc_syscall_smoke`, `session_syscall_smoke`, `mount_syscall_smoke`,
  `sig_syscall_smoke`, `rt_signal_syscall_smoke`, `sysv_sem_syscall_smoke`, `pause_syscall_smoke`,
  `mq_syscall_smoke`, `posix_timer_syscall_smoke`. The pilot itself needed 11 full runs to reach a
  clean completion (each real hangs/crashes/panics found and fixed in turn, not deferred) —
  `target/posix-pilot-logs/` holds the real, complete serial output of every run for direct
  inspection, not just this summary's own tallies.
- **Not covered by this pass**: root-causing *why* so many `FAIL`/`UNRESOLVED` results remain
  (62 + 40 — some are real, already-documented honest gaps this doc's own other sections already
  flag as expected controls — `mlock`/`clock_settime`/`clock_nanosleep`/unenforced `sched_*` fields
  — others are newly surfaced and not yet individually triaged); `wait4()`'s own missing
  signal-interrupt-wake gap (Bug 3's own doc comment flags this explicitly, left open); a bound on
  how large this pilot corpus could still grow (81 non-pthread/non-aio interface directories exist
  in the suite total, several already fully covered by this pass, some intentionally left thin).

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
| `/proc` — system-wide (`meminfo`/`uptime`/`stat`) + `chdir(2)` into `/proc` | done | — | `MemFree`/`MemAvailable` == `MemTotal` (no dealloc tracking); `/proc/stat`'s `cpu` line all-zero except `idle`. `free`/`uptime` also call `sysinfo(2)` for their primary numbers — see "Real `getrandom(2)`/`sysinfo(2)`" below, now implemented |
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
