# Missing POSIX syscalls

Tracks OxideBSD's syscall surface against POSIX.1-2017 (Issue 7)'s System Interfaces volume,
not just against musl's live call graph (a narrower, earlier pass — still cited here where
relevant, since a POSIX-mandated interface with no live caller in our current userland is a much
lower priority than one BusyBox/TinyCC/hush already calls). Source list: the Open Group's own
function index, filtered down to the subset that's genuinely syscall-backed on a real Unix kernel
(process control, file I/O, directories, signals, IPC, sockets, time/clocks, threads, memory
mapping, users/groups/permissions, resource limits) — pure libc/userspace interfaces (`string.h`,
`math.h`, `ctype.h`, most of buffered stdio above `open`/`read`/`write`/`close`, locale, `regex.h`,
`wordexp`, POSIX tracing) are excluded; they never reach a syscall on any real Unix and don't
belong in this doc.

**Numbering discipline** (restated because it's real bug history, not boilerplate — see below):
before assigning any new number, grep `third_party/musl/arch/x86_64/bits/syscall.h.in` for a live
`__NR_*` caller in `third_party/musl/src/`. If musl already calls a real, unremapped Linux number
directly, use that number — don't invent one. Otherwise continue OxideBSD's own invented sequence
from the current highest, **554** (see the full sweep below, then the planned-implementation-order
pre-reservation batch further down — this range moved three times across two sessions). This
project has been bitten by number collisions twice before (`SYS_KILL`/real `setgroups`,
`__NR_getdents64` sibling) — and a great many more times, found and **fixed** by a full sweep of
the header, documented below.

**Deliberate exception to the discipline above, as of the batch in "Pre-reserved ahead of
implementation" further down**: this project has since decided it doesn't want its own planned
future syscalls sitting at borrowed real Linux numbers even when they're safely unclaimed today —
OxideBSD is its own ABI, not obligated to reuse real Linux's numbering just because a slot happens
to be free. So for syscalls with a **planned** implementation (not just a theoretical future one),
claim a permanent OxideBSD-invented number now, ahead of writing the handler, rather than waiting
until implementation time. This is a one-time cost (one musl-submodule bump, one full BusyBox
relink) paid once for a whole batch, instead of once per syscall spread across future sessions —
see that section for the full reasoning.

## Fixed: a full sweep of every real/invented numeric collision in the header

Not inferred — confirmed directly, then fixed. C allows two `__NR_*` macros with different names
but identical values with no warning, so nothing catches this at compile time: a program calling
the still-real side silently invoked OxideBSD's handler for the numerically-colliding invented
side, misinterpreting its arguments. A full pass over every `#define __NR_*` line in
`bits/syscall.h.in`, cross-referenced against every one of OxideBSD's own invented `SYS_*` values
(`src/`, `modules/*/src/`), found **33 real collisions** beyond the original four (`SYS_KILL`/
`setgroups`, `__NR_getdents64` sibling). All 33 have been redirected to fresh, verified-unclaimed
numbers starting at 493; **no kernel-side handler existed for any of the real-side names before
this fix**, so every one of these was a pure safety fix (silent misroute → clean `ENOSYS`), not a
functional regression — nothing that worked before still needs to work the same way.

| Real, formerly-unremapped macro | Old value → new | Collided with (OxideBSD's own) | Live caller | Effect if hit (now fixed) |
|---|---|---|---|---|
| `times` | 100 → 493 | `SYS_MMAP = 100` | `src/time/times.c:6` | Reachable via hush's `times` builtin — would have created a bogus anonymous mapping and returned garbage as a `clock_t`. |
| `ptrace` | 101 → 497 | `SYS_MUNMAP = 101` | none confirmed live | Dormant, fixed anyway. |
| `getpgrp` | 111 → 498 | `SYS_RENAME = 111` | none — musl's `getpgrp()` is emulated via `getpgid(0)`, never issues this syscall | Genuinely dead macro, fixed for hygiene. |
| `setresuid`/`seteuid` | 117 → 499 | `SYS_SIGACTION = 117` | `src/unistd/setresuid.c`, `seteuid.c` | Would have invoked `sigaction` with `(ruid,euid,suid)` misread as a signal-handler installation — high severity if ever hit. |
| `getresuid` | 118 → 500 | `SYS_SIGPROCMASK = 118` | `src/misc/getresuid.c` | Would have invoked `sigprocmask` instead. |
| `setresgid`/`setegid` | 119 → 501 | `SYS_SIGRETURN = 119` | `src/unistd/setresgid.c`, `setegid.c` | Highest-severity of the batch: `sigreturn` restores raw CPU state from a signal frame — a stray call here could have corrupted execution state, not just misrouted arguments. |
| `getresgid` | 120 → 502 | `SYS_SETPGID = 120` | `src/misc/getresgid.c` | Would have invoked `setpgid` instead. |
| `capget` | 125 → 503 | `SYS_DUP = 125` | `src/linux/cap.c` | Would have invoked `dup` instead. |
| `capset` | 126 → 504 | `SYS_FSTAT = 126` | `src/linux/cap.c` | Would have invoked `fstat` instead. |
| `ustat` | 136 → 505 | `SYS_MKDIR = 136` | none confirmed live (obsolete real syscall) | Dead, fixed for hygiene. |
| `sysfs` | 139 → 506 | `SYS_NANOSLEEP = 139` | none confirmed live (obsolete real syscall) | Dead, fixed for hygiene. |
| `sched_setparam` | 142 → 507 | `SYS_SENDTO = 142` | `src/thread/pthread_setschedprio.c` | Only reachable via real threading (not yet implemented) — fixed anyway since `SYS_SENDTO` is live (wget/DNS path). |
| `sched_rr_get_interval` | 148 → 508 | `SYS_POLL = 148` | `src/sched/sched_rr_get_interval.c` | `SYS_POLL` is heavily live (musl's DNS resolver) — this was the highest-traffic collision partner in the batch even though `sched_rr_get_interval` itself has no confirmed caller in the current roster. |
| `mlock` | 149 → 509 | `SYS_SOCKETPAIR = 149` | `src/mman/mlock.c` | `SYS_SOCKETPAIR` is live (wget HTTPS path). |
| `munlock` | 150 → 510 | `SYS_SET_TID_ADDRESS = 150` | `src/mman/munlock.c` | `SYS_SET_TID_ADDRESS` fires on *every* musl program's startup — highest-frequency collision partner in the batch. |
| `mlockall` | 151 → 511 | `SYS_FCNTL = 151` | `src/mman/mlockall.c` | `SYS_FCNTL` is live (`O_NONBLOCK` path). |
| `munlockall` | 152 → 512 | `SYS_SHUTDOWN = 152` | `src/mman/munlockall.c` | `SYS_SHUTDOWN` is live (wget HTTPS path). |
| `vhangup` | 153 → 513 | `SYS_READV = 153` | `src/linux/vhangup.c` | `SYS_READV` is live (buffered `fread`/`fgets` path). |
| `modify_ldt` | 154 → 514 | `SYS_READLINK = 154` | none confirmed live | `SYS_READLINK` is live (symlink work); fixed anyway. |
| `pivot_root` | 155 → 525 | `SYS_SYMLINK = 155` | `src/linux/pivot_root.c` | `SYS_SYMLINK` is live; `pivot_root`'s own BusyBox applet was already cut from the roster before v0.1, but the macro itself was still live in musl. |
| `_sysctl` | 156 → 515 | `SYS_SETITIMER = 156` | none confirmed live (obsolete real syscall) | `SYS_SETITIMER` is live (`ping`'s receive-loop timeout); fixed anyway. |
| `prctl` | 157 → 516 | `SYS_GETITIMER = 157` | `src/linux/prctl.c` (no BusyBox-roster caller — grepped, only comment references in `pgrep.c`/`pidof.c`) | `SYS_GETITIMER` is live; fixed anyway since musl's own `prctl()` wrapper does contain a real call site even if unreferenced today. |
| `arch_prctl` | 158 → 517 | `SYS_GETUID = 158` | `src/linux/arch_prctl.c` only — confirmed **not** used by this port's real TLS setup, which goes through the patched `__set_thread_area.s` asm stub instead | `SYS_GETUID` is extremely live; `arch_prctl.c` itself is genuinely dead code but fixed for hygiene given how frequently 158 would otherwise dispatch to `getuid`. |
| `adjtimex` | 159 → 518 | `SYS_GETEUID = 159` | `src/linux/clock_adjtime.c` (only under `CLOCK_REALTIME`, no confirmed applet path) | `SYS_GETEUID` is live; fixed anyway. |
| `setrlimit` | 160 → 519 | `SYS_GETGID = 160` | none reachable — musl's own `setrlimit()` wrapper always tries `prlimit64` first and never falls through | **Reverses a previously-*accepted* risk** (see CLAUDE.md's own Syscall ABI section, which flagged this exact collision as "avoided only because `prlimit64` always succeeds first"). Now genuinely impossible instead of merely masked. |
| `acct` | 163 → 520 | `SYS_SETGID = 163` | `src/unistd/acct.c` (no process-accounting applet in roster) | `SYS_SETGID` is live; fixed anyway. |
| `settimeofday` | 164 → 521 | `SYS_GETGROUPS = 164` | `src/internal/syscall.h`'s `time32` alias (no confirmed applet calls it directly) | `SYS_GETGROUPS` is live (the `su`/`setgroups` work); fixed anyway. |
| `swapon` | 167 → 522 | `SYS_UTIMENSAT = 167` | `src/linux/swap.c` — no `swapon`/`swapoff` applet is currently seeded (`grep` of `build.rs`/oxfs seeding confirms neither is built) | `SYS_UTIMENSAT` is live (`touch`'s `ENOENT`→`O_CREAT` fallback); fixed anyway since a future roster change could reintroduce `swapon`. |
| `get_kernel_syms` | 177 → 523 | `SYS_GETSID = 177` | none confirmed live (obsolete real syscall) | `SYS_GETSID` is live (`getty`'s real fallback path); fixed anyway. |
| `query_module` | 178 → 524 | `SYS_SETGROUPS = 178` | none confirmed live (obsolete real syscall) | Ironic: 178 was the landing number chosen to *fix* the original `SYS_KILL`/`setgroups` collision, without checking it against this file's own still-inert `query_module` value at the time. Not currently dangerous (no live caller) but fixed for consistency now that a full sweep was done anyway. |

**Deliberately left alone** (confirmed intentional, not oversights):
- `__NR_exit` / `__NR_exit_group` (both 1), `__NR_fork` / `__NR_vfork` (both 2), `__NR_getdents` /
  `__NR_getdents64` (both 129) — documented in-file as real aliases: exit/exit_group have no
  "just this thread" distinction to preserve on a kernel with no threads; vfork aliases to real
  `fork()` per POSIX's own explicitly-allowed implementation; getdents/getdents64 are the
  already-fixed 64-bit-sibling case from CLAUDE.md's musl-port section.
- `__NR_mount` (165, collides with `SYS_CHMOD`) / `__NR_umount2` (166, collides with `SYS_CHOWN`) —
  `third_party/musl/src/linux/mount.c`'s own comment already states both macros are "unreferenced
  from here on": `mount()`/`umount()`/`umount2()` are patched at the call-site level to issue
  `SYS_create_module`/`SYS_init_module`/`SYS_delete_module` directly (see "Mount table" below),
  bypassing these macros entirely. Left untouched to match that file's own explicit, already-made
  decision rather than second-guessing it.

None of the 33 fixed collisions had been hit by the test suite or `test_busybox.sh` (no seeded
applet calls most of the real-side names directly), which is exactly why they were still latent —
same invisibility class as the `SYS_KILL`/`setgroups` bug before `su` was tested interactively.
`times`, `setresuid`/`seteuid`, `setresgid`/`setegid`, and `getresuid`/`getresgid` are the ones with
a real, if narrow, live path today. No kernel-side handler exists yet for any of the real-side
names (`times`, `sigpending`, `sigtimedwait`, `sigqueue`, `setresuid`, etc.) — this sweep only
guarantees each now lands on a clean, unclaimed number and `ENOSYS`s honestly instead of
misrouting; implementing any of them is separate future work, tracked in the tables below.

## Implemented this session

Four of the cheap, no-new-primitive entries from the table below now have real handlers:

| POSIX interface(s) | Number | Handler | Notes |
|---|---|---|---|
| `raise`, `abort`, `pthread_kill`, `pthread_cancel`, `timer_delete` | `SYS_TKILL = 200` (real, unclaimed) | `modules/signal` → `src/syscall/ffi.rs`'s `sys_tkill` → `process::do_kill` | Thin wrapper — `tkill(tid,sig)` is exactly `kill(tid,sig)` since `SYS_SET_TID_ADDRESS` already returns the real pid as `tid`. `abort()`/`assert()` now deliver real `SIGABRT` instead of trapping. |
| `times` | `SYS_TIMES = 493` | `modules/posix_compat` → `sys_times`/`RawTms` | Honest all-zero `struct tms` (same tier as `getrusage`'s `RawRusage`); return value is the real `ticks()` counter, not fabricated. |
| `sigpending` | `SYS_SIGPENDING = 494` | `modules/signal` → `sys_sigpending` → `process::do_sigpending` | Direct readback of the existing `pending_signals` bitmask. |
| `fchdir` | `SYS_FCHDIR = 81` (real, unclaimed) | `modules/oxfs`'s `oxfs_fchdir` | Resolves the fd via the existing `resolve_write_fd_inode`, rejects non-directories with `ENOTDIR`, reuses `oxfs_chdir`'s own `set_current_cwd_real` tail. |

`SYS_TKILL`/`SYS_TIMES`/`SYS_SIGPENDING`/`SYS_FCHDIR` were verified with a fast, scoped `cargo check`
on the individual module crates plus a full `cargo build` (0 warnings, 0 errors, 27m01s — the musl
header sweep's own rebuild cascade). `SYS_TKILL`/`SYS_SIGPENDING` additionally now have a real
end-to-end `SYSCALL` smoke test (`tests/sa_siginfo_syscall_smoke.rs` +
`userland/sa-siginfo-syscall-smoke/`, see immediately below — it exercises both directly). `SYS_TIMES`/
`SYS_FCHDIR` still don't have a dedicated test of their own.

**`SA_SIGINFO` handler invocation** (`src/syscall/mod.rs`'s `deliver_pending_signal`,
`RawSiginfo`/`RawUcontext`/`RawMcontext`) is also now real, closing the "No `SA_SIGINFO` support"
gap that function's own doc comment used to list. A handler installed with `SA_SIGINFO` is invoked
as a genuine 3-argument `void (*)(int, siginfo_t *, void *)` — `rsi`/`rdx` point at a correctly
sized/shaped `siginfo_t`/`ucontext_t` built on the handler's own stack frame, not `NULL`.
Faithfully populated: `si_signo`, `si_code` (always `SI_USER` — every signal here arrives via
`kill`/`tkill`, never a real hardware fault), the real general-purpose registers in
`uc_mcontext.gregs` (from the interrupted syscall's own saved frame), and `uc_sigmask` (the real
pre-handler `blocked_signals`). Honestly zeroed, not fabricated: `si_pid`/`si_uid` (no sender
identity is tracked anywhere in `pending_signals`), `si_value` (nothing populates it until
`sigqueue` itself has a handler — see below), FPU state (never saved anywhere on this kernel),
`uc_stack` (`sigaltstack` isn't implemented). This needed **zero module-side changes** — `sigaction`
already threaded `flags` through to `Process::sigactions`; only the kernel-resident delivery path
needed to consult it.

**Verified end-to-end**: `tests/sa_siginfo_syscall_smoke.rs` + `userland/sa-siginfo-syscall-smoke/`
installs a real `SA_SIGINFO` handler for `SIGUSR1`, `tkill`s itself, and confirms — from inside the
handler, into global statics — that `signum`/`si_signo`/`si_code`/a non-`NULL` `ucontext`/
`uc_sigmask` all arrived correctly, then confirms (after the `tkill` syscall itself returns, proving
a real `sigreturn` round trip actually resumed the interrupted instruction stream) that
`sigpending()` no longer reports the signal. **Passes.** This also incidentally found a real,
previously-only-theoretical bug: `elf.rs`'s "flags aren't unioned across `PT_LOAD` segments sharing
a page" gap (already documented in CLAUDE.md) page-faulted this crate's very first static write,
since it's the first userland crate with real writable globals — worked around at the linker-script
level for this one crate, not fixed in `elf.rs` itself; see CLAUDE.md's own updated note on that gap.

## Pre-reserved batch: first implementation

Items 1-5 of the 28-syscall "pre-reserved ahead of implementation" batch further below now have
real handlers:

| POSIX interface(s) | Number | Handler | Notes |
|---|---|---|---|
| `getrandom` (only reachable via `getentropy()`) | `SYS_GETRANDOM = 526` | `modules/posix_compat` → `sys_getrandom` → `src/random.rs`'s existing generator | Real `(buf_ptr, buflen, flags)` wire format, no musl call-site patch needed. Delegates straight to the generator already backing `/dev/random`/`/dev/urandom` — inherits its persistent entropy pool and `RDRAND`/`RDSEED` hypervisor-distrust gate for free. `flags` outside real `GRND_NONBLOCK`/`GRND_RANDOM` is `EINVAL`; both accepted bits make no behavioral difference since this generator has no blocking-on-low-entropy distinction to honor them against. |
| `sysinfo` (non-POSIX, see footnote below) | `SYS_SYSINFO = 527` | `modules/posix_compat` → `sys_sysinfo`/`RawSysinfo` | Real `(info_ptr)` wire format, no musl call-site patch needed. `RawSysinfo` is 368 bytes, verified via a direct C `offsetof`/`sizeof` probe against musl's real `struct sysinfo` rather than assumed from Rust `repr(C)` layout rules alone. Real `uptime`/`totalram`/`procs`; `freeram == totalram` (same tier `/proc/meminfo`'s own `MemFree` placeholder already uses — no deallocation tracking exists anywhere in this kernel); `loads`/`sharedram`/`bufferram`/`totalswap`/`freeswap`/`totalhigh`/`freehigh` honestly zero (no load-average/page-cache/swap tracking exists). |
| `sigaltstack` | `SYS_SIGALTSTACK = 528` | `modules/signal` → `sys_sigaltstack` → `process::do_sigaltstack`/`AltStack` | Real `(ss_ptr, old_ptr)` wire format, no musl call-site patch needed (musl's own wrapper filters `SS_ONSTACK`/an undersized `ss_size` client-side). Bookkeeping only — no signal is ever actually delivered on the alt stack, `SA_ONSTACK` still isn't honored by `deliver_pending_signal`, matching `modules/signal`'s already-documented "no real signal stack" gap. `SS_ONSTACK` is always reported unset on read-back, an honest reflection of that. Copied by `fork` (the duplicated address space keeps the alt stack's address valid); reset to disabled by `execve` (the old address is meaningless in the new image). |
| `pause` | `SYS_PAUSE = 529` | `modules/signal` → `sys_pause` → `process::do_pause` | Real zero-argument wire format, no musl call-site patch needed. The first item in the batch to need a genuine new block/wake primitive (`BlockReason::WaitingForSignal`), not just a state field — see `do_pause`'s own doc comment for the full real-POSIX-ordering reasoning (a caught handler runs before the caller ever observes `pause()` "returning"). Woken by `do_kill`/`signal_foreground_group`'s own `Action::SetPending` arm via a new `wake_if_paused` helper; an ignored or blocked signal correctly leaves it parked. |
| `sigsuspend` | `SYS_SIGSUSPEND = 530` | `modules/signal` → `sys_sigsuspend` → `process::do_sigsuspend` | Real `(mask_ptr, sigsetsize)` wire format, no musl call-site patch needed. Reuses `do_pause`'s own `BlockReason::WaitingForSignal`/`wake_if_paused` primitive unchanged, adding a temporary, atomic swap of `blocked_signals` around the same wait (atomicity falls out for free from this kernel's single-core, no-preemption design). The one real new wrinkle: the temporary mask must **not** be restored as soon as a deliverable signal is found (the woken signal is very often blocked under the *original* mask — the canonical `sigsuspend` use case — so restoring early would hide it from `take_deliverable_signal`), but real POSIX semantics also require the *original* mask back once the wait is over, not the temporary one. Solved with a new `Process::sigsuspend_restore_mask` handoff: `do_sigsuspend` records the mask to restore instead of applying it, and `deliver_pending_signal` (`src/syscall.rs`) consumes it once it knows how the woken signal actually resolved — immediately, if no handler runs (`Terminate`/`Stop`/nothing left deliverable); deferred until `sigreturn`, if one does (via a new `set_signal_saved_blocked_override`, so the *original* mask is what `sigreturn` restores, not the temporary one `stash_signal_context` would otherwise have captured). |

**Verified end-to-end**: `tests/getrandom_syscall_smoke.rs` + `userland/getrandom-syscall-smoke/` —
a real spawned ELF through genuine `SYSCALL`/`SYSRETQ` (not a plain Rust function call --
`tests/random_smoke.rs` already covers the generator's own cryptographic logic directly, this test's
job is proving the syscall plumbing itself). Four parts: a real 32-byte request succeeds and isn't
degenerate, two consecutive requests differ, `len == 0` is a harmless no-op, and flag handling
(`GRND_NONBLOCK`/`GRND_RANDOM` accepted, any other bit `EINVAL`) is correct. **Passes.**

**Verified end-to-end**: `tests/sysinfo_syscall_smoke.rs` + `userland/sysinfo-syscall-smoke/` — same
real-`SYSCALL` pattern. Three parts: a real call's fields (`mem_unit == 1`, `totalram > 0`,
`freeram == totalram`, `procs >= 1`, every untracked field honestly zero), `uptime` non-decreasing
across two calls, and `totalram` stable across two calls. **Passes.**

**Verified end-to-end**: `tests/sigaltstack_syscall_smoke.rs` + `userland/sigaltstack-syscall-smoke/`
— same real-`SYSCALL` pattern, deliberately bypassing musl's own `sigaltstack()` wrapper (a raw
`syscall()` call) to exercise the kernel's own `EINVAL` path directly. Five parts: the real POSIX
startup state is disabled, installing a real alt stack succeeds, reading it back matches
(`flags == 0`, not `SS_DISABLE`/`SS_ONSTACK`), a combined set+read-old call reports the state from
just before, and an invalid flag bit is `EINVAL` while disabling correctly zeroes `sp`/`size`.
**Passes.**

**Verified end-to-end**: `tests/pause_syscall_smoke.rs` + `userland/pause-syscall-smoke/` — same
real-`SYSCALL` pattern. Forks; the parent immediately calls `pause()` (genuinely blocks, forcing
the scheduler to run the freshly forked child); the child sends the parent a caught-disposition
`SIGUSR1` (the wake hook fires against a process actually sitting in
`Blocked(WaitingForSignal)`), then exits; the parent's `pause()` returns `EINTR` only after the
handler has already run, and it reaps the child's clean `exit(0)` via `wait4`. **Passes.** (Found
and fixed live along the way: this crate's own real writable static — `HANDLER_RAN: AtomicBool`,
set from inside the signal handler — hit the exact same `elf.rs` PT_LOAD-segment-sharing-a-page
issue `sa-siginfo-syscall-smoke` first found; same linker-script `ALIGN(0x1000)` workaround
applied, see this crate's own `linker.ld`.)

**Verified end-to-end**: `tests/sigsuspend_syscall_smoke.rs` + `userland/sigsuspend-syscall-smoke/`
— same real-`SYSCALL` pattern (same `ALIGN(0x1000)` writable-global workaround, this time a real
`AtomicU32` handler counter). Blocks `SIGUSR1` via `sigprocmask`, forks; the parent immediately
calls `sigsuspend(&empty_mask)` (genuinely blocks, forcing the scheduler to run the freshly forked
child); the child sends the parent `SIGUSR1` — blocked under the parent's *original* mask, but not
under `sigsuspend`'s temporary empty one — then exits; the parent's `sigsuspend()` returns `EINTR`
only after the caught handler has already run once. Then, the specific correctness property
`do_sigsuspend`'s own doc comment exists for: a `sigprocmask` readback confirms `SIGUSR1` is
blocked *again* (the original mask, not left at the temporary empty one); a self-`kill(pid,
SIGUSR1)` while blocked again is held pending (`sigpending()`) without invoking the handler a
second time; unblocking it and issuing one more ordinary syscall then delivers it for real (handler
count reaches `2`) via the normal `deliver_pending_signal` tail. Finally reaps the child's clean
`exit(0)` via `wait4`. **Passes.**

## Pre-reserved batch: second implementation

Items 6-10 of the 28-syscall "pre-reserved ahead of implementation" batch above -- the real POSIX
per-process timer sub-batch -- now have real handlers too:

| POSIX interface(s) | Number | Handler | Notes |
|---|---|---|---|
| `timer_create` | `SYS_TIMER_CREATE = 531` | `modules/clock` -> `src/syscall/ffi.rs`'s `oxidebsd_sys_timer_create` -> `process::do_timer_create`/`process::PosixTimer` | Real `(clockid, evp_ptr, timerid_ptr)` wire format. `evp_ptr == 0` matches real POSIX's own default (`SIGEV_SIGNAL`/`SIGALRM`); an explicit `evp` supports `SIGEV_SIGNAL`/`SIGEV_NONE` only -- `SIGEV_THREAD`/`SIGEV_THREAD_ID` are real `EINVAL` (this kernel has no `clone(2)`, so musl's own `SIGEV_THREAD` path never reaches the syscall in the first place). A caller's own opaque `timer_t` is just an index into a new fixed `Process::posix_timers: [Option<PosixTimer>; MAX_POSIX_TIMERS]` array (`MAX_POSIX_TIMERS = 8`, no live caller to size this against) -- `EAGAIN` once every slot is in use, matching real Linux. |
| `timer_settime` | `SYS_TIMER_SETTIME = 532` | `process::do_timer_settime` | Real `(timerid, flags, new_ptr, old_ptr)` wire format (a bare 4-argument syscall on this LP64 target -- no `*64`-suffixed sibling exists, see `bits/syscall.h.in`'s own comment on the batch). Supports both relative (`flags == 0`) and `TIMER_ABSTIME` arming; `TIMER_ABSTIME` resolves the absolute target against whichever `clockid` the timer was created with (`CLOCK_MONOTONIC` converts directly since `ticks()` already *is* that domain; `CLOCK_REALTIME` anchors off `src/cpu/rtc.rs`'s live CMOS read). An all-zero `it_value` disarms regardless of `TIMER_ABSTIME`, matching real semantics. |
| `timer_gettime` | `SYS_TIMER_GETTIME = 533` | `process::do_timer_gettime` | Real `(timerid, val_ptr)` wire format, same remaining-time/floored-readback shape `getitimer`'s own `RawItimerval` handling already established, just nanosecond- (`RawItimerspec`) instead of microsecond-precision. |
| `timer_getoverrun` | `SYS_TIMER_GETOVERRUN = 534` | `process::do_timer_getoverrun` | Real single-argument `(timerid)` wire format -- unlike the other three, the overrun count itself *is* the syscall's return value, no output pointer. A real, if simplified, count: an expiry whose signal is still pending (undelivered) from a previous expiry increments it instead of being silently lost; resets to `0` on a fresh (non-overlapping) expiry or on rearming. **Known, accepted gap**: two timers sharing one `signo` can't be told apart by this bookkeeping (both observe the same process-wide `pending_signals` bit) -- no live caller to exercise this. |
| `timer_delete` | `SYS_TIMER_DELETE = 535` | `process::do_timer_delete` | Real single-argument `(timerid)` wire format. Just frees the slot -- no dealloc beyond that, consistent with this kernel's "no deallocation anywhere" stance. |

Delivery is a new scan inside `interrupts::timer_interrupt_handler`, alongside the existing
`ITIMER_REAL`/`real_timer_deadline` check: same simple "just set the pending bit" design (no
forced cross-process wake), now also computing the overrun count above. **Real, not just planned,
`fork`/`execve` semantics**: `Process::posix_timers` is *not* inherited by `fork` (matching real
`timer_create(2)`'s own NOTES section) and, unlike `real_timer_deadline`/`ITIMER_REAL`, *is* reset
(disarmed and deleted) by `execve` too -- a POSIX timer's whole purpose (notifying the program that
created it) means nothing to a new program image, the same reasoning `AltStack`'s own `execve`
reset already established.

**Verified end-to-end**: `tests/posix_timer_syscall_smoke.rs` + `userland/posix-timer-syscall-
smoke/` -- same real-`SYSCALL` pattern (same `ALIGN(0x1000)` writable-global workaround as
`pause-syscall-smoke`, this time two `AtomicU32` handler-run counters). Eight parts: an invalid
`clockid` is `EINVAL`; a default-`evp` (`SIGALRM`) relative one-shot timer fires exactly once and
reads back disarmed; an explicit `SIGEV_SIGNAL`/`SIGUSR1` periodic timer fires repeatedly and
`timer_delete` genuinely stops it; `TIMER_ABSTIME` against both `CLOCK_MONOTONIC` and
`CLOCK_REALTIME` (the latter specifically exercising the CMOS-RTC-anchored branch); overrun
accounting (block the signal, let a periodic timer expire several times undelivered, confirm a
nonzero overrun, unblock and confirm exactly one delivery); `EAGAIN` once all `MAX_POSIX_TIMERS`
slots are in use plus slot reuse after `timer_delete`; and `EINVAL` for an out-of-range or
already-deleted `timerid` across all four of `timer_settime`/`timer_gettime`/`timer_getoverrun`/
`timer_delete`. **Passes.**

## Pre-reserved batch: third implementation

Items 11-16 of the 28-syscall "pre-reserved ahead of implementation" batch above -- the real POSIX
message-queue sub-batch -- now have real handlers too, closing out the whole batch's first three
sub-batches (SysV IPC, items 17-28, remains unimplemented).

| POSIX interface(s) | Number | Handler | Notes |
|---|---|---|---|
| `mq_open` (also backs `mq_send`/`mq_receive`/`mq_getattr`'s own `mqd_t`) | `SYS_MQ_OPEN = 536` | `modules/posix_compat` -> `src/syscall/ffi.rs`'s `sys_mq_open` -> `src/fs/mqueue.rs`'s `do_mq_open` | Real `(name, flags, mode, attr)` wire format, no musl call-site patch needed -- `name` is a raw NUL-terminated pointer (not this codebase's usual length-prefixed convention: real `mq_open(2)` already fills all 4 register slots, leaving no room for a `path_len`, and real Linux doesn't length-prefix it either). A real, separate name -> queue namespace (`NAMES`/`QUEUES`), not backed by `oxfs`. Returns a real fd from `crate::fs::fd` -- an mqd rides the ordinary fd registry exactly like a pipe/socketpair end, since real `mq_close(3)` is a bare `syscall(SYS_close, mqd)` (no distinct `mq_close` number exists in this batch). |
| `mq_unlink` | `SYS_MQ_UNLINK = 537` | `do_mq_unlink` | Real `(name)` wire format, no patch needed. Removes the name immediately; the queue itself survives until every open descriptor closes, matching real POSIX. |
| `mq_timedsend` (also backs `mq_send`) | `SYS_MQ_TIMEDSEND = 538` | `do_mq_timedsend` | Real Linux needs 5 syscall args (`mqd, msg, len, prio, at`); this ABI only carries 4. `third_party/musl/src/mq/mq_timedsend.c` is patched (`oxidebsd` branch) to pack `mqd`/`len` into one register (high 32 = len, low 32 = mqd) rather than dropping an argument the way `utimensat` drops its always-`AT_FDCWD` `fd` -- nothing here is redundant to drop. Real priority-ordered insertion (highest priority first, FIFO among ties), a real bounded block (`BlockReason::WaitingForMqSpace`) once `mq_maxmsg` is reached, real `EMSGSIZE` past `mq_msgsize`, and real `mq_notify` delivery on an empty-to-non-empty transition with no receiver already waiting. |
| `mq_timedreceive` (also backs `mq_receive`) | `SYS_MQ_TIMEDRECEIVE = 539` | `do_mq_timedreceive` | Same 4-register packing as `mq_timedsend`. Real blocking (`BlockReason::WaitingForMqData`) on an empty queue, real `EMSGSIZE` if the caller's buffer is smaller than the queue's own `mq_msgsize`, real priority readback via the (optional) `prio_ptr` out-argument. |
| `mq_notify` | `SYS_MQ_NOTIFY = 540` | `do_mq_notify` | Real `(mqd, sev_ptr)` wire format, no patch needed (`third_party/musl/src/mq/mq_notify.c` issues this raw syscall directly for every notify kind except `SIGEV_THREAD`, which never reaches the syscall boundary at all -- handled entirely in userspace over a real `AF_NETLINK` socket this port doesn't have). `SIGEV_SIGNAL` delivery reuses `process::do_kill` directly (no permission check to bypass, real disposition-respecting delivery) rather than a bespoke path. `SIGEV_THREAD`/`SIGEV_THREAD_ID` are real `EINVAL`. `si_value` is read but never delivered -- same already-documented gap `sigqueue`'s own "Missing, live caller confirmed" entry below has (`pending_signals` has nowhere to carry a payload). |
| `mq_getsetattr` (also backs `mq_getattr`/`mq_setattr`) | `SYS_MQ_GETSETATTR = 541` | `do_mq_getsetattr` | Real `(mqd, new, old)` wire format, no patch needed. Only `mq_flags`'s `O_NONBLOCK` bit is actually settable (`mq_maxmsg`/`mq_msgsize` are fixed at creation and silently ignored if passed in `new`, matching real Linux); `old` is always filled with the queue's real current state (`mq_curmsgs` included). |

Real timeout support falls out of a genuine new dual-wake shape: `BlockReason::WaitingForMqData`/
`WaitingForMqSpace` carry a deadline (`u64::MAX` for the plain, non-timed `mq_send`/`mq_receive`
wrapper's null-`at` case -- never realistically reached within this kernel's lifetime, so no
`Option` wrapper is needed), woken either by the matching send/receive draining the condition or by
`interrupts::timer_interrupt_handler`'s own deadline scan (extended alongside its existing
`Sleeping`/`real_timer_deadline`/`posix_timers` checks). `resolve_deadline` converts the `at`
`timespec` via `process::abstime_to_ticks` (now `pub(crate)`, reused from the per-process-timer
batch above), always against `CLOCK_REALTIME` -- real POSIX `mq_timedsend`/`mq_timedreceive`'s own
timeout is never configurable the way `timer_settime`'s `clockid` is.

Two hard caps this port enforces that real Linux doesn't (no privileged-override/`rlimit`-driven
ceiling exists here): `mq_maxmsg <= 256`, `mq_msgsize <= 65536` -- `EINVAL` past either. Each queue
is a plain heap-backed `Vec`, not block-allocator-bounded the way `oxfs` is; an unbounded
`maxmsg * msgsize` would be the same "unbounded heap growth reachable from userspace" bug
`fs/pipe.rs`'s own `PIPE_CAPACITY` was already added to close for pipes.

**Verified end-to-end**: `tests/mq_syscall_smoke.rs` + `userland/mq-syscall-smoke/` -- same real-
`SYSCALL` pattern. Eight parts: `O_CREAT | O_EXCL` open then a real `EEXIST`/`ENOENT`; priority-
ordered delivery (three sends at priorities `1, 5, 1` come back `5, 1, 1`); `EMSGSIZE` both
directions; filling to `mq_maxmsg` then a real `O_NONBLOCK` `EAGAIN`, confirmed by an
`mq_getsetattr` readback of `mq_curmsgs`/`mq_maxmsg`/`mq_msgsize`/`mq_flags`; a real
`TIMER_ABSTIME`-shaped deadline actually expiring `ETIMEDOUT`; a real block/wake pair across
`fork()` (the parent genuinely blocks in `mq_timedreceive` on an empty queue, forcing the freshly
forked child to run, which sends the message that wakes it); `mq_notify`/`SIGEV_SIGNAL` firing
exactly once on an empty-to-non-empty transition with nothing already blocked, then *not* firing
again on a second send (one-shot, nothing re-registered); and `mq_unlink` removing the name while
the already-open descriptor keeps working, torn down via a real `close()`. **Passes.**

## Pre-reserved batch: fourth implementation

Items 25-28 of the 28-syscall "pre-reserved ahead of implementation" batch above -- the real SysV
message-queue sub-batch -- now have real handlers too. Implemented out of the batch's own planned
order (ahead of items 17-24, the shm/sem sub-batches) since it's the more directly useful half of
SysV IPC and shares no code with the shm/sem work.

| POSIX interface(s) | Number | Handler | Notes |
|---|---|---|---|
| `msgget` | `SYS_MSGGET = 550` | `modules/posix_compat` -> `src/syscall/ffi.rs`'s `sys_msgget` -> `src/fs/sysv_msg.rs`'s `do_msgget` | Real `(key, flag)` wire format, no musl call-site patch needed. A fundamentally different lifecycle from `crate::fs::mqueue`'s POSIX message queues (see that module's own doc comment for the contrast): identified by an integer `key_t` via a separate `KEYS`/`QUEUES` namespace, `IPC_PRIVATE` always allocates a fresh unfindable-by-key queue, real `IPC_CREAT`/`IPC_EXCL` semantics. Returns a bare integer id, not a real fd -- no `crate::fs::fd` involvement at all (a SysV queue has no "open"/"close" step; it lives from `msgget` until an explicit `msgctl(IPC_RMID)`). |
| `msgsnd` | `SYS_MSGSND = 551` | `do_msgsnd` | Real `(q, m, len, flag)` wire format, already fits this ABI's 4 real register args, no patch needed. `m` points at a real `{long mtype; char mtext[len];}` buffer; `mtype < 1` is `EINVAL`. Real bounded blocking (`BlockReason::WaitingForSysvMsgSend`) once the queue's own `qbytes` cap is reached, `IPC_NOWAIT` `EAGAIN` otherwise; `len` alone exceeding `qbytes` is `EINVAL` (can never fit regardless of occupancy). Real `ipc_perm` rwx-style permission check (`check_access`), same shape a file's own mode bits use. |
| `msgrcv` | `SYS_MSGRCV = 552` | `do_msgrcv` | Real Linux needs 5 syscall args (`q, m, len, type, flag`); this ABI only carries 4. `third_party/musl/src/ipc/msgrcv.c` is patched (`oxidebsd` branch) to pack `q`/`flag` into one register (high 32 = flag, low 32 = q), same shape `mq_timedsend`/`mq_timedreceive`'s own patches already established. Real `msgtyp` selection matching `msgrcv(2)`'s exact documented semantics -- `0` = oldest message any type, `> 0` = oldest message of that exact type (or, with `MSG_EXCEPT`, oldest message of any *other* type), `< 0` = among messages with `mtype <= |msgtyp|`, the oldest message of the *smallest* such type (`find_matching_index`). Real `E2BIG`/`MSG_NOERROR`: a message peeked (not yet removed) that's too big for the caller's buffer without `MSG_NOERROR` returns `E2BIG` **without consuming the message** -- found live during this session's own testing, not preemptively: an earlier draft removed the message before checking size, silently destroying it on a failed receive. Real bounded blocking (`BlockReason::WaitingForSysvMsgRecv`), `IPC_NOWAIT` `ENOMSG` otherwise. |
| `msgctl` | `SYS_MSGCTL = 553`, last of the batch | `do_msgctl` | Real `(q, cmd, buf)` wire format, no patch needed. `cmd` always arrives with real glibc/musl's `IPC_64` bit (`0x100`) OR'd in (`third_party/musl/src/ipc/msgctl.c`'s own `IPC_CMD()` macro) -- masked off before matching. `IPC_STAT` (real permission-checked readback, including live `cbytes`/`qnum` computed fresh), `IPC_SET` (owner/creator/root only -- a stricter check than plain write permission, real SysV distinction from `msgsnd`/`msgrcv`'s rwx check), `IPC_RMID` (owner/creator/root only; removes the queue and wakes every blocked sender/receiver, which each re-check `QUEUES` from scratch and find the id gone -- real `EIDRM`, no distinct wake signal needed). `MSG_STAT`/`MSG_STAT_ANY`/`IPC_INFO`/`MSG_INFO` (real `/proc`-introspection-shaped commands) are honest `EINVAL` -- no live caller, not silently no-op'd. |

**Real, not honest-zero, timestamps**: `stime`/`rtime`/`ctime` are real `crate::cpu::rtc::
unix_epoch_seconds()` reads at creation and every successful `msgsnd`/`msgrcv`/`msgctl(IPC_SET)` --
cheap (the same CMOS read `CLOCK_REALTIME` already uses) and meaningfully more useful than a
placeholder for a struct whose whole job is reporting these three timestamps, a deliberate
departure from the "honest zero" tier `RawRusage`/`RawTms`/`RawSysinfo` use for concepts this
kernel genuinely doesn't track at all.

**No timeout concept, unlike the POSIX `mq_*` batch**: real `msgsnd`/`msgrcv` only ever block
indefinitely or (`IPC_NOWAIT`) fail immediately -- no `semtimedop`-style deadline argument exists
to plumb through, so no timer-IRQ deadline scan was needed here the way the POSIX-timer/`mq_*`
batches needed one.

`RawIpcPerm`/`RawMsqidDs` (48/120 bytes) were verified via a direct `musl-gcc`/`sizeof`/`offsetof`
probe against musl's real `struct ipc_perm`/`struct msqid_ds` on this arch, same rigor
`RawSysinfo` already established, rather than assumed from Rust `repr(C)` layout rules alone.

**Verified end-to-end**: `tests/sysv_msg_syscall_smoke.rs` + `userland/sysv-msg-syscall-smoke/` --
same real-`SYSCALL` pattern. Seven parts: `IPC_CREAT | IPC_EXCL` then a real `EEXIST`/`ENOENT`; a
plain send/receive round trip preserving `mtype`; real `msgtyp` selection (positive exact match out
of FIFO order, negative smallest-type-within-bound, zero FIFO-any, plus `MSG_EXCEPT`); real
`E2BIG` that doesn't consume the message, then a `MSG_NOERROR` receive that does (truncated);
`msgctl(IPC_SET)` shrinking `qbytes` then a real `EINVAL`/`IPC_NOWAIT` `EAGAIN`/`IPC_NOWAIT`
`ENOMSG`; a real block/wake pair across `fork()` (the parent genuinely blocks in `msgrcv` on an
empty queue, forcing the freshly forked child to run, which sends the message that wakes it); and
`msgctl(IPC_STAT)` reporting real state followed by `IPC_RMID` and confirmation that
`msgsnd`/`msgrcv`/`msgctl` against the removed `msqid` are all real `EIDRM`. **Passes.**

This brings the batch to 20 of 28 items done (1-16, 25-28) -- only the shm/sem sub-batches (items
17-24, `542-549`) remain, the biggest and most novel remaining subsystem with no live caller today.

## Missing, live caller confirmed

Interfaces musl's own C source calls directly (grepped, not inferred) that have no registered
OxideBSD handler.

| POSIX interface(s) | Backing syscall | Live caller | Suggested number | Notes |
|---|---|---|---|---|
| `sigtimedwait`, `sigwaitinfo` | `rt_sigtimedwait(2)` | `src/signal/sigtimedwait.c:19,22` | `495` (already remapped, see sweep above — no handler registered yet) | No blocking-on-signal primitive exists yet; would need a new `BlockReason` variant, more than a number remap. |
| `sigqueue` | `rt_sigqueueinfo(2)` | `src/signal/sigqueue.c:19` | `496` (already remapped, see sweep above — no handler registered yet) | `SA_SIGINFO` handler invocation is now real (see "Implemented this session" above) — `RawSiginfo::si_value` already exists as a landing spot. Still needs: a real `SYS_SIGQUEUE` handler (currently unregistered), and `pending_signals` would need to grow from a plain bitmask into something that can carry one queued `union sigval` per signal number, since today's bitmask has nowhere to stash a payload between `do_kill`-style setting and `take_deliverable_signal`-style draining. |

## Missing, POSIX-mandated, no live caller yet in ported userland

Real POSIX interfaces with no confirmed call site anywhere in `third_party/musl/src` reachable from
the current roster (BusyBox, TinyCC, hush). Worth tracking, not worth building ahead of a real
need — same "don't build for a hypothetical caller" discipline this codebase already applies
elsewhere.

| POSIX interface(s) | Backing concept | Notes |
|---|---|---|
| `mq_open`, `mq_close`, `mq_unlink`, `mq_send`, `mq_receive`, `mq_timedsend`, `mq_timedreceive`, `mq_notify`, `mq_getattr`, `mq_setattr` | POSIX message queues | Pre-reserved at `536-541` (see "Pre-reserved ahead of implementation" below — no handler registered yet). No seeded applet uses these; `src/fs/pipe.rs`'s existing blocking-buffer machinery (already reused for `socketpair`) is the natural backing if ever needed. |
| `sem_init`, `sem_destroy`, `sem_wait`, `sem_trywait`, `sem_timedwait`, `sem_post`, `sem_getvalue` (unnamed semaphores) | futex-backed | Needs a real `futex(2)` first (see "structurally inapplicable" below for why that's currently out of scope) — blocked on the same prerequisite as thread sync primitives generally. No distinct syscall of its own to reserve a number for — not part of the pre-reservation batch below. |
| `sem_open`, `sem_close`, `sem_unlink` (named semaphores) | `/dev/shm`-backed `open`+`mmap` | No live caller; would also want the `shm_open` path below first. Not a distinct syscall either — not part of the pre-reservation batch below. |
| `shm_open`, `shm_unlink` | POSIX shared memory | No live caller in roster. Implemented via plain `open`/`mkdir` on real Linux, not a distinct syscall — not part of the pre-reservation batch below. |
| `shmget`, `shmat`, `shmctl`, `shmdt`, `msgget`, `msgctl`, `msgrcv`, `msgsnd`, `semget`, `semctl`, `semop`, `semtimedop` | SysV IPC | Pre-reserved at `542-553` (see "Pre-reserved ahead of implementation" below — no handler registered yet). `ipcrm`/`ipcs` were already cut from the BusyBox roster before v0.1 specifically because this doesn't exist — see CLAUDE.md's own gap-analysis table. |
| `aio_read`, `aio_write`, `aio_fsync`, `aio_error`, `aio_return`, `aio_cancel`, `aio_suspend`, `lio_listio` | POSIX async I/O | On real Linux, musl implements these via a userspace thread pool (`src/aio/aio.c`), not a true async-I/O syscall — not meaningfully "missing" at the kernel level at all; would only become relevant once real threading exists. Not part of the pre-reservation batch below. |
| `timer_create`, `timer_settime`, `timer_gettime`, `timer_getoverrun`, `timer_delete` | POSIX per-process timers | Pre-reserved at `531-535` (see "Pre-reserved ahead of implementation" below — no handler registered yet). Distinct from the already-implemented `setitimer`/`getitimer` (`ITIMER_REAL` only). No live caller confirmed; `timer_delete.c` does call `tkill` internally (see Priority 1 above) but only after a real `timer_create` has ever succeeded, which can't happen yet. |
| `select`, `pselect` | fd readiness | `poll(2)` already exists and covers every confirmed live caller (musl's DNS resolver). `src/select/poll.c` doesn't route through `pselect6` on this build (confirmed: `SYS_poll` is used directly). The only BusyBox callers of raw `select` (`inetd`, `telnetd`, `dhcprelay`, `fdisk`, ...) are already cut from the roster. Not part of the pre-reservation batch below — genuinely not needed. |
| `posix_spawn`, `posix_spawnp` + the `posix_spawnattr_*`/`posix_spawn_file_actions_*` family | process creation | musl implements `posix_spawn` entirely in userspace on top of `vfork`/`execve` (`src/process/posix_spawn.c`) — both of those already exist here (see CLAUDE.md's `vfork.s` note). Not a missing syscall at all, just unexercised library code. |
| `fexecve` | `execveat`-style exec by fd | musl's `fexecve` falls back to `/proc/self/fd/<n>` + `execve` when `execveat` is unavailable — would work today given real per-fd `/proc` entries exist, modulo the "not a real symlink" limitation already documented in `docs/BUSYBOX_APPLETS.md`'s `NEEDS_PROC` section. |

## Structurally inapplicable to this kernel's current architecture

Not "not yet built" — genuinely doesn't fit until a prerequisite this codebase has explicitly
deferred exists.

| POSIX interface(s) | Why inapplicable today |
|---|---|
| `pthread_create`, `pthread_join`, `pthread_detach`, `pthread_mutex_*`, `pthread_cond_*`, `pthread_rwlock_*`, `pthread_barrier_*`, `pthread_spin_*`, `clone`(underlying) | No real thread-creation syscall exists (`clone`/`futex`/`set_robust_list` are all unregistered — confirmed via grep, zero registered handlers). Two hand-written asm stubs (`src/thread/x86_64/clone.s`, `__unmapself.s`) hardcode raw Linux numbers directly, the same bypass-the-remap-table bug class already fixed once for `vfork.s`, but genuinely dead code until threading is attempted. This kernel's single-core, no-preemption, single-global-`fs_base`-per-context-switch design (see CLAUDE.md's musl-port section on `IA32_FS_BASE`) has no notion of two live threads sharing one address space at all yet — a real prerequisite, not a missing syscall number. |
| `sched_yield`, `sched_setparam`, `sched_rr_get_interval` | Partially covered — `sched_setscheduler`/`sched_getscheduler`/`sched_getparam`/`sched_get_priority_max`/`_min` already exist (`modules/posix_compat`), stored/echoed honestly with no real scheduling effect, matching `nice`'s own honesty tier. `sched_yield` specifically has no meaning without preemption to yield *to* — this kernel's scheduler only switches at syscall/blocking boundaries today. |
| `mlockall`, `munlockall`, `mlock`, `munlock`, `posix_madvise`, `msync` | This kernel never pages anything out (no swap, no reclaim of any kind anywhere — a documented, deliberate gap per CLAUDE.md's memory-management section) — "lock this page so it can't be swapped" is a no-op by construction, not a missing capability. `msync`'s only real job (flushing a `MAP_SHARED` mapping back to its file) doesn't apply either: `mmap` here is always anonymous+private (confirmed, `src/process/mm.rs`'s own doc comment). |
| `fattach`, `fdetach`, `isastream`, `putmsg`, `getmsg`, `putpmsg`, `getpmsg` | STREAMS — an XSI option almost no modern Unix (including real Linux) implements at all; not a gap specific to this kernel. |
| `posix_trace_*` (the whole family) | POSIX tracing — an optional XSI extension real Linux/glibc/musl don't implement either; nothing to port against. |
| `dlopen`, `dlsym`, `dlclose`, `dlerror` | musl's real implementation is pure userspace logic over `mmap`/`mprotect`/relocation processing once a `.so` is already mapped — not a syscall gap itself, but blocked on `mmap`/`mprotect` actually enforcing real placement/protection (currently both are permissive no-ops/bump-allocators, per CLAUDE.md's PT_INTERP milestone notes) before a second real shared object could be loaded correctly. Tracked as the actual blocker under "milestone 2" in this conversation's prior research, not re-litigated here. |
| `getlogin`, `getlogin_r`, `ttyname`, `ttyname_r`, `tcgetsid` | Real Linux backs these via `/proc/self/fd/N` symlink resolution + `ioctl(TIOCGSID)`-adjacent lookups against `utmp`, not a single dedicated syscall — `tcgetsid` specifically has no distinct number on this ABI at all (would fold into the existing `TIOCGPGRP`-style `SYS_IOCTL` gate, not a new registration). No live caller in the current roster. |
| `pathconf`, `fpathconf`, `sysconf` (the syscall-shaped subset) | On real Linux these are pure libc constant tables, not syscalls at all — musl's own implementation never issues one. Not a gap; correctly out of scope for this doc. |

## Pre-reserved ahead of implementation: a planned-implementation-order batch

Distinct from the collision sweep above (that batch fixed real bugs — a still-real macro silently
misrouting into an OxideBSD handler with mismatched arguments). **Nothing in this batch was
colliding with anything** — every one of these 28 syscalls was already confirmed sitting at a
safe, unclaimed real Linux value, and would have `ENOSYS`'d cleanly forever if left alone. This
batch exists purely because of a deliberate architectural choice (see the numbering-discipline
note at the top of this doc): OxideBSD is its own ABI, and syscalls it actually plans to implement
shouldn't sit at borrowed real-Linux numbers just because the slot happened to be free — they get
a permanent OxideBSD-invented number instead, claimed now, so the eventual implementation pass is
a pure kernel-side change (register a handler at the already-claimed number) with **zero further
musl-submodule edits**. Landed in one commit
(`third_party/musl`'s `oxidebsd` branch, `bd7f66d3`) covering the whole batch at once — one
submodule bump, one full BusyBox relink, instead of paying that cost once per syscall spread
across future sessions.

**Implementation order** (update the checkbox when a syscall in this batch gets a real kernel-side
handler — this is the tracking list for the batch, not just historical numbering rationale):

- [x] 1. `getrandom` (`526`) — `modules/posix_compat`'s `handle_getrandom` -> `src/syscall/ffi.rs`'s
      `sys_getrandom`, delegating to `src/random.rs`'s existing generator. Verified via
      `tests/getrandom_syscall_smoke.rs` + `userland/getrandom-syscall-smoke/`.
- [x] 2. `sysinfo` (`527`) — `modules/posix_compat`'s `handle_sysinfo` -> `src/syscall/ffi.rs`'s
      `sys_sysinfo`/`RawSysinfo` (368 bytes, verified via a direct C `offsetof`/`sizeof` probe
      against musl's real `struct sysinfo`). Real `uptime`/`totalram`/`procs`; `freeram ==
      totalram` (same "no deallocation tracking" tier `/proc/meminfo`'s `MemFree` already uses);
      `loads`/`sharedram`/`bufferram`/`totalswap`/`freeswap`/`totalhigh`/`freehigh` honestly zero
      (no load-average/page-cache/swap tracking exists). Verified via
      `tests/sysinfo_syscall_smoke.rs` + `userland/sysinfo-syscall-smoke/`.
- [x] 3. `sigaltstack` (`528`) — `modules/signal`'s `handle_sigaltstack` -> `src/syscall/ffi.rs`'s
      `sys_sigaltstack` -> `src/process/signals.rs`'s `do_sigaltstack`/`AltStack`. Real
      `(ss_ptr, old_ptr)` wire format; bookkeeping only (`SA_ONSTACK` still isn't honored by
      signal delivery, matching the still-open "no real signal stack" gap) -- `SS_ONSTACK` is
      always reported unset on read-back. Copied by `fork`, reset to disabled by `execve`.
      Verified via `tests/sigaltstack_syscall_smoke.rs` + `userland/sigaltstack-syscall-smoke/`.
- [x] 4. `pause` (`529`) — `modules/signal`'s `handle_pause` -> `src/syscall/ffi.rs`'s `sys_pause`
      -> `src/process/signals.rs`'s `do_pause`. A genuine new block/wake-on-signal primitive
      (`ProcState::Blocked(BlockReason::WaitingForSignal)`), not just a state field — woken by
      `do_kill`/`signal_foreground_group`'s own `Action::SetPending` arm via a new shared
      `wake_if_paused` helper. Always returns `EINTR`; real POSIX ordering (a caught handler runs
      before the caller ever observes `pause()` "returning") falls out for free from this
      codebase's existing "deliver pending signals at the tail of every completed syscall" design.
      Verified via `tests/pause_syscall_smoke.rs` + `userland/pause-syscall-smoke/`.
- [x] 5. `sigsuspend` (`530`)
- [x] 6. `timer_create` (`531`)
- [x] 7. `timer_settime` (`532`)
- [x] 8. `timer_gettime` (`533`)
- [x] 9. `timer_getoverrun` (`534`)
- [x] 10. `timer_delete` (`535`)
- [x] 11. `mq_open` (`536`)
- [x] 12. `mq_unlink` (`537`)
- [x] 13. `mq_timedsend` (`538`)
- [x] 14. `mq_timedreceive` (`539`)
- [x] 15. `mq_notify` (`540`)
- [x] 16. `mq_getsetattr` (`541`)
- [ ] 17. `shmget` (`542`)
- [ ] 18. `shmat` (`543`)
- [ ] 19. `shmctl` (`544`)
- [ ] 20. `shmdt` (`545`)
- [ ] 21. `semget` (`546`)
- [ ] 22. `semop` (`547`)
- [ ] 23. `semctl` (`548`)
- [ ] 24. `semtimedop` (`549`)
- [x] 25. `msgget` (`550`)
- [x] 26. `msgsnd` (`551`)
- [x] 27. `msgrcv` (`552`)
- [x] 28. `msgctl` (`553`)

Numbers assigned in the batch's own planned implementation order (cheapest / most build on
existing primitives first, most architecturally novel last):

| Order | Number | Syscall | Old (real, unclaimed) value | Why this position |
|---|---|---|---|---|
| 1 | `526` | `getrandom` | `318` | Near-direct backing already exists (`src/random.rs`'s real ChaCha20 generator, already serving `/dev/urandom`). |
| 2 | `527` | `sysinfo` | `99` | Non-POSIX footnote (see below) but same tier — honest all-zero-except-real-fields struct, same pattern as `getrusage`/`times`. |
| 3 | `528` | `sigaltstack` | `131` | One small `Process` state addition. |
| 4 | `529` | `pause` | `34` | Thin wrapper, reuses whatever primitive `sigsuspend` ends up needing. |
| 5 | `530` | `sigsuspend` | `130` (`rt_sigsuspend`) | Needs a genuinely new block/wake-on-signal primitive, not just a state field. |
| 6-10 | `531-535` | `timer_create`/`timer_settime`/`timer_gettime`/`timer_getoverrun`/`timer_delete` | `222-226` | Natural extension of the already-implemented `setitimer`/`getitimer` infrastructure once per-timer-id tracking exists. |
| 11-16 | `536-541` | `mq_open`/`mq_unlink`/`mq_timedsend`/`mq_timedreceive`/`mq_notify`/`mq_getsetattr` | `240-245` | Needs a new blocking primitive, but `src/fs/pipe.rs`'s existing blocking-buffer machinery (already reused for `socketpair`) is the natural backing. |
| 17-28 | `542-553` | SysV IPC: `shmget`/`shmat`/`shmctl`/`shmdt`, `semget`/`semop`/`semctl`/`semtimedop`, `msgget`/`msgsnd`/`msgrcv`/`msgctl` | `29-31`/`64-71`/`220` | Biggest, most novel subsystem, no live caller today, and `ipcrm`/`ipcs` are already cut from the BusyBox roster — lowest priority of the batch. |

No kernel-side handler exists for any of these 28 yet — this batch is purely the number
reservation, matching the collision-sweep's own "safety fix, not a functional regression" framing
above. Implementing any of them is separate future work.

## Non-POSIX interfaces worth a footnote

`sysinfo(2)` isn't a POSIX interface at all (Linux-specific), but it's the confirmed live blocker
for `free`/`uptime`'s primary numbers (`procps/{free,uptime}.c`) per prior research and
`docs/BUSYBOX_APPLETS.md`'s own `NEEDS_PROC` section. Pre-reserved at `527` (see "Pre-reserved
ahead of implementation" above) — previously sat at its real, unclaimed Linux number `99`. Tracked
here for completeness since it'll come up in the same implementation pass as several POSIX entries
above, not because it belongs in a POSIX-conformance doc on its own merits.
