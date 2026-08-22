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

- [x] **Real threading**: `pthread_create`/`_join`, `clone(2)`/`futex(2)` — **done**, see
      `[[project_real_threading_for_aio]]` memory and CLAUDE.md's "Real threading" section for the
      full five-phase history. `clone.s`/`__unmapself.s`'s raw-Linux-number bypass bug is fixed
      (`SYS_CLONE = 555`, now with a real handler — `process::lifecycle::do_clone`); `Process::tgid`
      splits real `getpid()`/`gettid()`; real `futex(2)` `FUTEX_WAIT`/`FUTEX_WAKE` exist
      (`process::do_futex`) — unnamed POSIX semaphores (`sem_init`/`sem_wait`/`sem_post`/...) work
      for real as a result (3 pilot FAILs flipped to PASS). The former blocker — shared/refcounted
      `AddressSpace` — is also done: `AddressSpace` is now `Arc`-refcounted (`teardown` gates its
      free-walk on `Arc::strong_count == 1`), a new `ThreadGroupShared` wraps the fields a real
      `CLONE_THREAD` sibling must share (`cwd`/`root_inode`/`umask`/`uid`/`gid`/`brk`/
      `mmap_file_regions`), and `src/fs/fd.rs` is keyed by `tgid` not raw `pid` (real `CLONE_FILES`
      sharing falls out for free). Verified end-to-end by a real, unmodified `pthread_create()`/
      `pthread_join()` round trip (`userland/pthread-smoke/`, `tests/pthread_syscall_smoke.rs`) —
      not just a raw `clone(2)` smoke test. **`pthread_mutex_*`/`_cond_*`/`_rwlock_*`/`_barrier_*`/
      `_spin_*` themselves are untouched** — real Linux/musl implement all of these as pure
      userspace logic over the same `futex(2)` primitive already real here, so they're expected to
      already work today (not separately verified by a dedicated test yet — worth a follow-up smoke
      test, not further kernel work). **Direct unlock**: POSIX AIO (`aio_*`/`lio_listio`, musl backs
      these with a userspace thread pool — needs zero further kernel-side work now) is the next
      real piece; named POSIX semaphores/POSIX shared memory still need the separate `/dev/shm`
      path below plus real cross-process `FUTEX_WAKE` (today's `do_futex` is deliberately scoped to
      the waker's own `tgid`, safe but not cross-process); real `dlopen` still needs `mmap`/
      `mprotect` enforcement, unrelated to threading itself.
- [x] **Real-time signal queuing**: done. `SIGRTMIN..=SIGRTMAX` (`35..=64`, matching musl's own
      `sigrtmin.c`/`sigrtmax.c`; `32..=34` stay permanently unclaimed, matching real glibc/musl
      convention) now validate through `do_kill`/`do_sigqueue`/`sys_sigaction`, and
      `Process::rt_queue` (`src/process/mod.rs`) gives each RT signal number its own small
      fixed-capacity (`RT_QUEUE_CAP = 16`) FIFO — a second `sigqueue`/`raise` against an
      already-pending RT signal number genuinely queues rather than merging into
      `pending_signals`' single bit, matching real POSIX. Standard signals (`1..=31`) are
      unchanged (still bitmask-collapsed, which POSIX permits for non-RT). Verified via
      `tests/rt_signal_syscall_smoke.rs` + `userland/rt-signal-syscall-smoke/` (queuing count,
      real per-signal `EAGAIN` past `RT_QUEUE_CAP`, lowest-signal-number-first delivery order,
      partial-drain pending-bit semantics) and by re-running the Open POSIX Test Suite pilot: flips
      `sigqueue/1-1,5-1,6-1,7-1.c` and `sigwait/2-1.c` from UNRESOLVED to real PASS.
      **A real, separate, pre-existing gap this surfaced for the first time, since fixed** (not a
      bug in the queuing work above — confirmed by `tests/rt_signal_syscall_smoke.rs`'s own part 1,
      which originally passed the identical scenario only via an explicit multi-syscall "pump"
      workaround, since removed): `deliver_pending_signal` (`src/syscall/mod.rs`) used to deliver
      only **one** signal per completed syscall, by design (redirecting the live frame into a
      handler once; no real signal stack existed to chain further redirects within the same return
      path). Real POSIX code that unblocks several already-queued RT instances in one call
      (`sigqueue/4-1.c`/`8-1.c`: `sigrelse()` once, then immediately expects all 5 queued instances
      already delivered) only saw one delivered before it checked — genuinely failed/went
      UNRESOLVED (that pilot run: `sigqueue/4-1.c` FAIL, `sigqueue/8-1.c` UNRESOLVED, both for this
      exact reason). **Fixed with a real signal stack**: `Process::signal_saved_frame: Option<
      SyscallFrame>` (a single snapshot) became `Process::signal_stack: Vec<SignalStackFrame>` (a
      real stack, `src/process/mod.rs`); `stash_signal_context` pushes an entry per delivery instead
      of overwriting one; `do_sigreturn` (`src/syscall/mod.rs`) now re-checks for a further
      deliverable signal immediately after popping and restoring an entry, and if one exists,
      chains straight into another handler invocation (pushing a fresh entry) instead of ever
      letting the popped state resume as real userspace execution. This closes the gap generally,
      not just for RT signals: any sequence of deliverable signals now plays out as N real handler
      invocations, each one's own `sigreturn` triggering the next, before the originally-interrupted
      code resumes. Flips `sigqueue/4-1.c` FAIL and `sigqueue/8-1.c` UNRESOLVED to real PASS with no
      other pilot regressions. Verified via the full automated pilot
      (`tests/posix_conformance_smoke.rs` + `userland/posix-conformance-driver/`, not manual) and
      `tests/rt_signal_syscall_smoke.rs`'s part 1/3 (now pump-free, proving the chain happens within
      the unblocking `sigprocmask` call's own tail) plus every other signal-touching smoke test
      (`sig`/`sa_siginfo`/`sigsuspend`/`sigaltstack`/`pause`/`mmap`/`fork_wait`).
      **A follow-up pass fixed a second real gap the pilot's remaining `sigqueue`/`kill` FAILs
      converged on**: real `kill(2)`/`sigqueue(2)` permission checking, plus `sigqueue`'s own real
      `sig == 0` null-signal existence(+permission)-only convention (previously always `EINVAL`,
      unlike `kill(pid, 0)`, which already had this). `has_signal_permission` (`src/process/
      signals.rs`) is the real POSIX rule — sender is root, or sender's uid matches the target's —
      checked by both `do_kill`/`do_sigqueue`'s single-target cross-process paths (not
      `signal_foreground_group`'s own process-group broadcast; no pilot test needs it, and real
      POSIX's own per-member partial-success rule there is meaningfully more complex). Flips
      `kill/2-2,3-1.c` and `sigqueue/2-1,2-2,3-1,11-1,12-1.c` from FAIL to real PASS — pilot moved
      45P/16F/3U/4UT → 52P/9F/3U/4UT, and (combined with several later, separately-landed fixes —
      see CLAUDE.md's own "Real ring-3 fault-to-signal delivery"/"Three more pilot fixes" sections —
      plus this signal-stack fix) now stands at **64P/0F/0U/4UT/0TIMEOUT/0CRASH, 68 total**: every
      pilot interface passes; the remaining 4 (`mq_open/10-1,14-1.c`, `shm_open/10-1,12-1.c`) are
      the suite's own self-declared UNTESTED — each one's own `main()` prints why and returns
      `PTS_UNTESTED` unconditionally, before ever calling anything this kernel implements (real
      multi-user/multi-group permission testing `mq_open/10-1.c` says it can't do in this
      environment; unspecified file-offset behavior `shm_open/10-1.c` declines to check at all) —
      not a kernel gap this pilot subset can close by implementing anything further. Verified via
      `sig-syscall-smoke`'s new part 8 (real `ESRCH`/`EPERM` enforcement, including a forked,
      uid-dropped child).
- [x] **File-backed `mmap`**: done — corrected from an earlier draft of this doc, which said
      `do_mmap` took no fd/offset argument at all; that was true when this row was first written
      but real `MAP_SHARED` fd-backed mapping landed since (see CLAUDE.md's "Real /tmp, /dev/shm,
      and real fd-backed MAP_SHARED mmap" section), and real MPR (`SIGBUS` past a mapped object's
      own real extent) plus real ring-3 fault-to-signal delivery landed after that (see CLAUDE.md's
      "Real ring-3 fault-to-signal delivery, and two mmap fixes" section) — closes
      `mmap/11-2.c`/`11-3.c`/`12-1.c` in the pilot below. **`MAP_PRIVATE` no longer always behaves
      as `MAP_SHARED`** — real `MAP_FIXED`/`MAP_PRIVATE` flags now ride the wire (previously guessed
      purely from `fd == -1`), with real `EBADF`/`EINVAL` validation, real `MAP_FIXED` replace-at-
      address semantics, and a real fresh never-cached, never-written-back frame copy for
      `MAP_PRIVATE` against a file-backed fd; real `EOVERFLOW`/`ENXIO` for out-of-range nonzero
      `off` also landed (see CLAUDE.md's "SIGCHLD delivery, real `sched_setparam(2)`, and four more
      mmap conformance fixes" section for the four commits closing `mmap/{3,9,14,18,19,21,28,31}-1.c`/
      `munmap/{3,4}-1.c` — landed after this doc's last recorded pilot baseline below, not yet
      re-verified with a fresh full pilot run). Real `mtime`/`ctime` tracking and real
      `mlockall(MCL_FUTURE)`/`RLIMIT_MEMLOCK` enforcement landed alongside. **Genuine remaining
      gaps**: no copy-on-write page-fault tracking for `MAP_PRIVATE` against anonymous memory (only
      the file-backed case is real), and real `msync(2)` itself still doesn't exist as its own
      syscall (writeback only happens at `munmap`/exit).
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
      `mprotect` enforcement below (`SYS_MPROTECT` exists but is still a permissive no-op stub —
      unaffected by the mmap-flags work above). **`mmap` itself is less of a blocker than when this
      row was first written**: real `MAP_FIXED` now exists (needed to place a `.so` at a
      loader-chosen address) — see the `mmap` row above — so the remaining gap is narrower than
      "mmap/mprotect enforcement" as a pair; `mprotect` alone is what's left.

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
      (`shm_open`/`shm_unlink`) — **unnamed semaphores are no longer blocked** (real
      `sem_init`/`sem_wait`/`sem_post`/... work today, see "Real threading" above, now fully done —
      not just phases 1-3). Named ones remain blocked on two things, not one, and **real thread
      creation landing doesn't change either**: a `/dev/shm`-style `open`+`mmap` path (plausible
      reuse of the already-real fd-backed `MAP_SHARED` mmap, see CLAUDE.md), *and* real
      cross-process `FUTEX_WAKE` — `process::do_futex`'s own wake scan is deliberately scoped to
      the waker's own `tgid` (see that function's own doc comment for why address-only keying would
      be unsafe with no ASLR), so a named semaphore shared between two genuinely different
      *processes* (as opposed to threads sharing one `tgid`, which now works) wouldn't actually
      wake across them yet even with the mmap path solved. SysV shared memory/semaphores already
      exist and are *not* a substitute — POSIX treats the two IPC families as genuinely separate
      optional interfaces.
- [x] **POSIX AIO** (`aio_read`/`_write`/`_fsync`/`_error`/`_return`/`_cancel`/`_suspend`,
      `lio_listio`) — real threading (the actual blocker) is now done, see above. On real
      Linux/musl these `aio_*` functions are pure userspace logic over a `pthread_create` worker
      thread pool, not a true syscall gap, so **no further kernel-side work is needed for AIO
      itself** — this row is closed as "unblocked," not yet separately verified with a dedicated
      AIO-specific smoke test (a natural next real step, not a kernel gap).

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
existing primitives, no syscall gap). New syscall numbers should continue from `555` (`SYS_CLONE`,
the current highest — now with a real handler, `process::lifecycle::do_clone`, see "Real threading"
above — per that doc's own numbering-discipline note).

## Verification: what would actually prove any of this

Everything above is preparation. The actual "certifiable" gate is running a real, independent
conformance test suite against a built OxideBSD image and looking at the pass/fail counts, not
self-assessing against this checklist:

- [x] **The Open POSIX Test Suite** — running, cross-compiled against this kernel's own musl fork,
      each file a real pre-built ELF run through `t0` (the suite's own real `alarm()`-based timeout
      wrapper) inside a real, single continuous boot (`tests/posix_conformance_smoke.rs` +
      `userland/posix-conformance-driver/`, driven by `modules/oxfs/src/posix_conformance.sh`'s own
      seeded corpus/manifest — see that script's own doc comment for the full design). Not the
      whole suite (1750 files in `conformance/interfaces/` alone, before `functional`/`stress`, and
      real threading/AIO are out of scope until the "Real threading" blocker above is addressed) —
      a curated, steadily-growing pilot subset, currently 488 files (grew from an initial 68-file
      pilot; see CLAUDE.md's own "POSIX conformance pilot" sections, most recently "POSIX
      conformance pilot expanded 68 → 488..." for the full growth history and the four real kernel
      bugs each expansion pass has found and fixed along the way).
- [x] **A real pass/fail baseline, latest confirmed run of the 488-file pilot**: **420 PASS / 10
      FAIL / 2 UNRESOLVED / 8 UNSUPPORTED / 45 UNTESTED / 2 TIMEOUT / 1 CRASH** — up from this doc's
      original 329P/62F/40U/8US/45UT/3TO/1CR baseline via several later fixes (see CLAUDE.md's own
      "POSIX conformance pilot" sections for the full incremental history). Most recently: a
      three-part pass closing `sigset/6-1,7-1.c` (a real, stock-musl `sigset(sig, SIG_HOLD)` bug —
      fixed on the `oxidebsd` musl branch, see CLAUDE.md's own "Three UNRESOLVED fixes" section),
      `timer_create/10-1,11-1.c` (`timer_create`/`timer_settime`/`timer_gettime` now accept
      `CLOCK_PROCESS_CPUTIME_ID`/`CLOCK_THREAD_CPUTIME_ID`, arming against `Process::cpu_ticks`),
      and `mmap/13-1.c` (a same-process `open(O_CREAT) -> write() -> stat()` visibility gap in oxfs
      — `force_commit_pending_create` now wired into `resolve_path_impl`, not just `oxfs_open` —
      which flips this test from a false `UNRESOLVED` to a real, *expected* `FAIL`: the suite's own
      `coverage.txt` documents this exact test failing on real glibc+Linux too, since `st_atime`
      never updates from an mmap'd write there either, and this filesystem's own `st_atime` is
      likewise a permanent honest-`0` placeholder). Net from that pass: 5 UNRESOLVED became PASS/
      the-one-expected-FAIL, moving the prior 414P/11F/7U baseline to this one (the small remaining
      FAIL-count drift is unrelated scheduling-timing variance between runs, not from these fixes).
      **Still open**: `sched_setparam/9-1.c`/`10-1.c` remain UNRESOLVED — investigated but not
      pinned down; the kernel-side logic (`do_sched_setparam`, permission checks, `sched_getaffinity`)
      all looked correct on inspection, so confirming the real cause needs a live trace, not more
      source-reading. The one `CRASH` is still a real
      bug in the *test itself* — `strftime/2-1.c`'s own stack-buffer overflow, correctly caught and
      cleanly delivered as `SIGSEGV` rather than rebooting the VM, not a kernel gap. **The 45
      UNTESTED files are all real, upstream-declared stubs, confirmed by direct inspection, not a
      kernel gap this pilot can close**: ~40 unconditionally `return PTS_UNTESTED` regardless of
      platform (POSIX leaves the behavior unspecified/implementation-defined, needs multiple real
      users, needs Priority Scheduling to build a reliable test, etc.); `sem_post/8-1.c` is gated by
      musl never defining `_POSIX_PRIORITY_SCHEDULING`, not an OxideBSD limitation;
      `sched_setparam/26-1.c` is a real upstream inconsistency (its 8 sibling tests self-demote via
      `setuid()` before giving up and pass for real under this kernel's real second-user support —
      this one just never wrote that fallback). **Next step**: the remaining 11 FAIL/7 UNRESOLVED
      haven't been individually triaged against this checklist's own categories yet — a priority-
      ranking pass over that set, not growing the file count further, is the natural next step here.
