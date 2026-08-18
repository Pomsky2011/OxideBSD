# POSIX compliance checklist

Tracks what stands between OxideBSD today and a genuinely POSIX.1-2017 (Issue 7)-conformant
system — broader scope than `docs/MISSING_POSIX_SYSCALLS.md`, which only tracks the syscall
surface. This doc covers everything else certifiable conformance actually depends on: whole
subsystems the syscall doc marks "structurally inapplicable," the Shell & Utilities volume,
locale/timezone data, and the test suite that would actually prove any of it.

## What "certifiable" means, honestly

Two different things get called "POSIX compliance," and they have very different bars:

1. **Technical conformance to POSIX.1-2017** — the system's interfaces behave the way the
   standard describes. This is the achievable, meaningful target for a project like this, and
   what the rest of this doc tracks.
2. **Official "UNIX" trademark certification** — the Open Group's actual VSX-PCTS (Platform
   Conformance Test Suite) process: a paid, formal submission that licenses the "UNIX" mark itself,
   run against a specific frozen release, re-certified per version. This is a legal/business
   process layered on top of #1, not an engineering task — **out of scope for this project**;
   nothing below tracks toward it, and reaching every item on this list still wouldn't grant the
   trademark without that separate process.

So "certifiable" here means: pass a real, independent POSIX conformance test suite (see
"Verification" at the bottom) against genuine technical conformance — not the trademark.

## Already conformant (recap — see `docs/MISSING_POSIX_SYSCALLS.md` for the syscall-level detail)

Process control (`fork`/`execve`/`wait4`/`exit`/signals incl. `sigtimedwait`/`sigqueue`), file I/O
(`open`/`read`/`write`/`lseek`/`stat` family/`access`/hard+symlinks), directories (`getdents`,
real multi-component paths, per-process cwd), permissions (real uid/gid/mode, `chmod`/`chown`),
sockets (UDP/TCP/raw ICMP, `poll`), time/clocks (`clock_gettime`, `nanosleep`, POSIX per-process
timers, `setitimer`), all three IPC families (POSIX message queues, SysV message
queues/semaphores/shared memory — see the memory note this closed out), resource limits/scheduling
*fields* (`prlimit64`, `nice`, `sched_*` — stored/echoed, not enforced, see below),
`getrandom`/`sysinfo`. All backed by real end-to-end `SYSCALL` smoke tests.

## Foundational architecture blockers

The big, invasive ones — each blocks a cluster of other items, so they're worth sequencing first
if this becomes real future work rather than just a tracking exercise.

- [ ] **Real threading**: `pthread_create`/`_join`/`_detach`, `pthread_mutex_*`/`_cond_*`/
      `_rwlock_*`/`_barrier_*`/`_spin_*`, backing `clone(2)`/`futex(2)`/`set_robust_list(2)`.
      **Why:** POSIX.1-2008 folded threads into the base spec's conformance surface — a real
      conformance test suite exercises them, not just the legacy `_POSIX_THREADS`-optional framing
      Issue 6 used. Also the direct prerequisite for named POSIX semaphores, POSIX shared memory,
      POSIX AIO (musl backs `aio_*` with a userspace thread pool), and real `dlopen`. **Blocked
      on:** this kernel's single-core, no-preemption, single-global-`fs_base`-per-context-switch
      design has no notion of two live threads sharing one address space at all yet — see
      CLAUDE.md's musl-port section on `IA32_FS_BASE`. The two existing hand-written asm stubs
      (`src/thread/x86_64/clone.s`, `__unmapself.s`) already hardcode raw Linux numbers directly
      (same bypass-the-remap-table bug class already fixed once for `vfork.s`) — dead code until
      this is attempted, a real landmine if threading work starts without re-patching them first.
- [ ] **Real-time signal queuing**: `SIGRTMIN`..`SIGRTMAX` (POSIX requires `_POSIX_RTSIG_MAX >= 8`
      distinct RT signal numbers) plus genuine multi-instance queuing per signal — a second
      `sigqueue`/`sigqueue`d instance of the *same* signal number must not merge with a first still
      pending. **Why it's not done today:** `Process::pending_signals` is a single `u64` bitmask —
      confirmed directly (`src/process/mod.rs`), not inferred — so even standard signals only ever
      track "pending or not," and real signal numbers stop at `SIGSYS = 31` (no RT range at all).
      Real POSIX conformance for `sigqueue(2)` explicitly requires queuing to not silently drop a
      second instance for RT signals — a real gap, not just an untested corner. Needs a genuine
      per-signal queue (e.g. a small fixed-capacity ring per RT signal number, standard signals can
      stay bitmask-collapsed since POSIX allows that for non-RT), not just growing the bitmask
      width.
- [ ] **File-backed `mmap`**: `MAP_SHARED`/`MAP_PRIVATE` against a real file descriptor, plus real
      `msync(2)`. **Why:** POSIX's base `mmap(2)` is not optional, and its most common real use
      (mapping a file, not just anonymous scratch memory) doesn't exist here — confirmed directly:
      `src/process/mm.rs`'s `do_mmap(caller_pid, addr_hint, len, prot)` takes no fd/offset
      argument at all, always anonymous. `msync`/`mlock`/`munlock`/`mlockall` are correctly
      no-ops-by-construction *for anonymous memory* (no swap, nothing ever paged out — see
      `docs/MISSING_POSIX_SYSCALLS.md`'s "structurally inapplicable" table) but that framing
      stops being true the moment a real file-backed mapping needs to flush back to its file. Real
      enforcement here (not just file-backing) is also the milestone-2 dynamic-linking blocker
      immediately below — the same gap shows up from two directions.
- [x] **Milestone 1 done, milestone 2 open** — corrected from an earlier draft of this doc, which
      wrongly said no `PT_INTERP` support existed at all; CLAUDE.md itself was stale on this same
      point until this pass. **Milestone 1 (done, `e72fc7d`)**: a real `fork`+`execve` of a
      genuinely dynamically-linked ELF works end to end — `elf.rs` accepts `ET_DYN`, loads a real
      `PT_INTERP` interpreter (musl's own `ld.so`, i.e. `libc.so` itself) alongside the main binary,
      and that interpreter performs real self-relocation and symbol resolution, verified by a real
      libc call round-tripping (`tests/dynlink_syscall_smoke.rs`). See CLAUDE.md's "Dynamic
      linking" section for the full design. **Milestone 2 (not started)**: `dlopen`/`dlsym`/
      `dlclose`/`dlerror` — loading a *second*, independently-chosen shared object at runtime.
      musl's own implementation of that is pure userspace logic over `mmap`/`mprotect`/relocation
      processing once a `.so` is mapped, not a new syscall gap — but genuinely blocked on real
      `mmap`/`mprotect` enforcement below, since milestone 1's kernel-driven single-interpreter load
      never needed either capability for real (`SYS_MPROTECT` exists now but is a permissive
      no-op stub).

## Filesystem / IPC gaps

- [ ] **`fcntl` POSIX record locking**: `F_SETLK`/`F_SETLKW`/`F_GETLK` — confirmed absent
      (`SYS_FCNTL`'s current handler only covers `F_GETFL`/`F_SETFL`(`O_NONBLOCK`)/`F_SETFD`(no-op)/
      `F_DUPFD`/`F_DUPFD_CLOEXEC`). **This is distinct from the `flock(2)` support that already
      exists** (`SYS_FLOCK`, a real per-inode `LOCK_SH`/`LOCK_EX`/`LOCK_UN` advisory table) — flock
      is a BSD extension, not POSIX; POSIX itself mandates the `fcntl`-based locking API. A real
      conformance suite tests `fcntl` locking specifically, and `flock`'s existing "conflicting
      request fails `EAGAIN` immediately even without the non-blocking flag, no scheduler-yield
      primitive reachable from a module syscall handler" limitation (see CLAUDE.md) would need
      solving here too for `F_SETLKW`'s real blocking semantics.
- [ ] **FIFOs** (`mkfifo(3)`, `S_IFIFO` nodes): confirmed still `EINVAL` in `mknod` (`modules/
      oxfs`). A real base POSIX interface, and the one BusyBox subsystem this doc's own gap-table
      already flagged as blocked on it (the `runit` family — `runsv`/`runsvdir`/`svlogd`/`svok` —
      was cut from the roster specifically for lacking this). Backing is plausible reuse of
      `src/fs/pipe.rs`'s existing blocking-buffer machinery, opened via a real path instead of
      `pipe(2)`'s anonymous fd pair.
- [ ] **Named POSIX semaphores** (`sem_open`/`sem_close`/`sem_unlink`) and **POSIX shared memory**
      (`shm_open`/`shm_unlink`) — blocked on real threading above (unnamed semaphores are
      futex-backed; named ones additionally want a `/dev/shm`-style `open`+`mmap` path). SysV
      shared memory/semaphores already exist and are *not* a substitute — POSIX treats the two
      IPC families as genuinely separate optional interfaces.
- [ ] **POSIX AIO** (`aio_read`/`_write`/`_fsync`/`_error`/`_return`/`_cancel`/`_suspend`,
      `lio_listio`) — on real Linux/musl these are userspace logic over a thread pool, not a true
      syscall gap; blocked on real threading above, not meaningfully separate work once that
      lands.

## Terminal / job control

- [ ] **Real pty layer** (`posix_openpt`/`grantpt`/`unlockpt`/`ptsname`, `/dev/pts`): this kernel
      has exactly one physical console tty — real job control (`setsid`/`TIOCSCTTY`/Ctrl+C/Ctrl+Z,
      see CLAUDE.md's "Real job control" section) already works *for that one console*, but
      anything needing a second, program-allocated terminal (a real terminal emulator, `script`,
      `ssh`/`telnet` servers, `expect`-style automation) has nothing to allocate.
- [ ] **`SIGTTIN`/`SIGTTOU` delivery**: still `Ignore` disposition unconditionally — confirmed in
      CLAUDE.md's job-control section as an explicit known gap (only `SIGINT`/`SIGTSTP` are ever
      delivered to a foreground group). Real background-process-writing-to-the-controlling-tty
      semantics depend on this.
- [ ] **`getlogin`/`getlogin_r`/`ttyname`/`ttyname_r`/`tcgetsid`**: real Linux backs these via
      `/proc/self/fd/N` symlink resolution plus `ioctl(TIOCGSID)`-adjacent `utmp` lookups, not one
      dedicated syscall each — `tcgetsid` specifically would fold into the existing
      `TIOCGPGRP`-style `SYS_IOCTL` gate rather than a new registration. No live caller in the
      current roster; low priority relative to the pty gap above, but a real conformance suite
      will still probe them.

## Honest-but-unenforced state (may or may not block conformance — needs a real test-suite run to know)

These already have real, correctly-shaped return values — the open question is whether a
conformance suite merely checks the interface *exists and round-trips*, or actually depends on the
enforced *effect*. Listed separately from the hard blockers above because closing them is smaller,
more mechanical work if a real test run says they matter:

- `rlimits` (`Process::rlimits`) — stored/returned via `prlimit64`, never actually enforced against
  real resource usage.
- `sched_policy`/`sched_priority` — stored/echoed via the `sched_*` family, no real scheduling
  effect (single-core, cooperative round-robin only).
- `sched_yield(2)` — not registered at all; has no real meaning without preemption to yield *to*,
  per `docs/MISSING_POSIX_SYSCALLS.md`'s own note — but a conformance suite may still expect a
  successful no-op rather than `ENOSYS`.
- Real per-process CPU-time accounting — `times(2)`/`getrusage(2)` report honest all-zero `struct
  tms`/`struct rusage` rather than fabricated numbers; a conformance suite checking that CPU time
  actually *increases* under load would fail here regardless of wire-format correctness.
- `clock_settime(2)` — only `clock_gettime` exists; setting the clock isn't implemented.
- `CLOCK_PROCESS_CPUTIME_ID`/`CLOCK_THREAD_CPUTIME_ID` — `clock_gettime`'s `clockid` handling
  covers `CLOCK_REALTIME`/`CLOCK_MONOTONIC` only (any other id is `EINVAL`), per CLAUDE.md's
  real-time-clock section.

## Locale, timezone, and the Shell & Utilities volume

Out of `docs/MISSING_POSIX_SYSCALLS.md`'s scope entirely (pure userspace/libc, no syscall
involved) but very much in POSIX.1-2017's scope as a whole:

- [ ] **Real locale data beyond `C`/`POSIX`**: musl itself supports locales, but nothing in this
      port seeds real locale definition files or `LC_*` category data — every process effectively
      runs in the `C` locale regardless of environment. A conformance suite that exercises
      `setlocale`/collation/`LC_TIME` formatting will need at least one real non-`C` locale
      available.
- [ ] **Real timezone database** (`/usr/share/zoneinfo`, `TZ` handling beyond a fixed offset) —
      `src/cpu/rtc.rs` reads CMOS directly and assumes the 21st century with no leap-second or
      timezone-conversion logic; `tv_nsec` is always `0`. Fine for this kernel's own internal use,
      not for `tzset(3)`/`localtime(3)` conformance.
- [ ] **A genuinely POSIX-conformant shell**: pid 1 is BusyBox's `hush`, which targets broad
      compatibility, not line-by-line POSIX `sh` conformance (job control, `set -o` options,
      here-documents, etc. — most already work, but this hasn't been checked against the Shell
      volume's own conformance requirements specifically).
- [ ] **The Shell & Utilities (XCU) volume's mandated utility set**: this project's coreutils-shape
      today is BusyBox applets (see CLAUDE.md's BusyBox port section) — broad functional coverage,
      but never audited option-by-option against POSIX's exact mandated behavior/exit-status/option
      set per utility. A real conformance pass would need to run the utility-level test suite (see
      "Verification" below), not just confirm each utility exists.

## Remaining syscall-level items

Tracked in full in `docs/MISSING_POSIX_SYSCALLS.md` — not duplicated here. As of the 28-syscall
pre-reserved batch landing (see memory: all 28 items done), that doc's own "Missing, live caller
confirmed" table is empty and "Missing, POSIX-mandated, no live caller yet" is down to items
already covered by the architecture blockers above (`mq_*`'s row there is stale — mq_open through
mq_getsetattr are actually implemented, see that doc's "third implementation" section — worth a
follow-up correction pass on that doc, not repeated here) plus `select`/`pselect` (deliberately
skipped, `poll` already covers every live caller) and `fexecve`/`posix_spawn` (already work via
existing primitives, no syscall gap). New syscall numbers should continue from `554` (the current
highest, per that doc's own numbering-discipline note).

## Verification: what would actually prove any of this

Everything above is preparation. The actual "certifiable" gate is running a real, independent
conformance test suite against a built OxideBSD image and looking at the pass/fail counts, not
self-assessing against this checklist:

- [ ] **The Open POSIX Test Suite** (originally from the Open Group/Linux Standard Base effort,
      long-since open-sourced) — the closest free equivalent to the paid VSX-PCTS suite mentioned
      above. Needs cross-compiling against this kernel's musl fork and a real way to run its test
      binaries under QEMU and collect pass/fail output (this project's own `tests/*_syscall_smoke.rs`
      pattern — spawn a real ELF through genuine `SYSCALL`/`SYSRETQ` — is the natural model, just
      at a much larger scale: hundreds of small test binaries instead of a handful of hand-written
      ones).
- [ ] **A real pass/fail baseline before investing further** — running the suite *now*, against
      today's OxideBSD, before closing any gap above, would concretely rank which of these items
      actually matter (a suite section might already pass despite an "honest-but-unenforced" gap
      above, or might fail for a reason not yet on this list at all). Cheaper than guessing
      priority order from this doc alone.
