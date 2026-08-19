//! Process abstraction: PID allocation, the process control block (PCB), and the global process
//! table. Companion to `crate::process::scheduler` (which owns *when* a process runs) and
//! `crate::process::context_switch` (which owns *how* control moves from one kernel stack to
//! another) -- this file owns process *state* itself (the `Process` struct, `ProcState`,
//! `PROCESS_TABLE`). The real syscall logic that mutates that state is split across this
//! directory's other files: `lifecycle` (spawn/fork/execve/wait/exit/reboot), `signals`
//! (kill/sigaction/sigprocmask/delivery), `identity` (uid/gid/pgid/sid), `limits`
//! (rlimit/priority/sched), `timers` (nanosleep/itimer), `mm` (mmap/munmap/mprotect/brk), `procfs`
//! (`/proc` formatting). All re-exported here so `crate::process::X` keeps resolving exactly where
//! it always has.
//!
//! See CLAUDE.md's process/scheduler section for the full design rationale.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use spin::{Lazy, Mutex};
use x86_64::VirtAddr;

use crate::memory::address_space::AddressSpace;
use crate::memory::{self};
use crate::syscall::SyscallFrame;

pub type Pid = u64;
/// Floor: 128 KiB -- much larger than the 20 KiB the single, old, static RSP0 stack (`gdt.rs`'s
/// original fixed stack, before every process got its own) used to be. Found empirically, the hard
/// way: 16 KiB overflowed on `ls` (`SYS_OPEN` on a directory -> `modules/fat32`'s
/// `open_directory_listing`, deeper than plain `SYS_READ`/`SYS_WRITE`); 32 KiB then overflowed on
/// `fork()` (`do_fork_from_current` -> `AddressSpace::fork`'s 4-level page-table walk ->
/// `AddressSpace::new` -> `PageTable::clone()` -- in an unoptimized debug build, cloning a
/// 512-entry array through the generic `try_from_fn` machinery has a surprisingly large unoptimized
/// stack frame). There's no guard page (heap-allocated, not a dedicated mapped-with-a-gap region
/// like `gdt.rs`'s stacks), so a stack overflow here corrupts silently or double-faults rather than
/// failing cleanly -- this needs real margin for debug builds specifically, not just "enough for
/// the common case observed once," which is why RAM-constrained boots keep exactly this floor
/// rather than shrinking further (see `kernel_stack_size` below).
const KERNEL_STACK_SIZE_FLOOR: usize = 128 * 1024;
/// Ceiling: purely a bound on how much a RAM-rich boot hands each process for free (more headroom
/// against deeper call chains, at essentially no cost against a multi-GiB usable-RAM pool) -- not
/// something any code path has been observed to need.
const KERNEL_STACK_SIZE_CEILING: usize = 512 * 1024;

/// Scales the per-process kernel stack size to `memory::usable_ram_bytes()`, clamped to
/// `[KERNEL_STACK_SIZE_FLOOR, KERNEL_STACK_SIZE_CEILING]`. `spin::Lazy` (not a plain `const`)
/// because the real value depends on the memory map read at boot; safe to compute lazily since
/// every caller (`KernelStack::new`) only ever runs after `memory::BootInfoFrameAllocator::init`
/// has already populated `usable_ram_bytes` (process creation happens well after boot's memory
/// setup completes).
static KERNEL_STACK_SIZE: Lazy<usize> = Lazy::new(|| {
    let scaled = (memory::usable_ram_bytes() / 256) as usize; // 1/256th of usable RAM
    scaled.clamp(KERNEL_STACK_SIZE_FLOOR, KERNEL_STACK_SIZE_CEILING)
});

fn kernel_stack_size() -> usize {
    *KERNEL_STACK_SIZE
}

/// Fixed for every process, same constant `src/main.rs`'s old one-shot demo path used. `execve`
/// reuses it too — a fresh program image gets a fresh stack at the same fixed top — which is fine
/// since this only needs to be unique *within* one process's own address space, not across the
/// whole system; different address spaces don't share user-space mappings the way they share the
/// kernel half.
pub const USER_STACK_TOP: u64 = 0x_5000_0000_0000;

/// Floor: 4 pages (16 KiB) -- the fixed size this kernel always mapped, proven sufficient for
/// `stsh` and every program it execs so far.
const USER_STACK_PAGES_FLOOR: u64 = 4;
/// Ceiling: 64 pages (256 KiB) -- bounds how much a RAM-rich boot maps per process; nothing today
/// needs more.
const USER_STACK_PAGES_CEILING: u64 = 64;

/// Scales the per-process user stack size the same way `kernel_stack_size` scales the kernel-side
/// one, off the same `memory::usable_ram_bytes()` reading.
static USER_STACK_PAGES: Lazy<u64> = Lazy::new(|| {
    let scaled = memory::usable_ram_bytes() / (256 * 4096); // 1/256th of usable RAM, in pages
    scaled.clamp(USER_STACK_PAGES_FLOOR, USER_STACK_PAGES_CEILING)
});

fn user_stack_pages() -> u64 {
    *USER_STACK_PAGES
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProcState {
    Ready,
    Running,
    Blocked(BlockReason),
    Zombie(i32),
    /// Real `SIGSTOP`/`SIGTSTP` job-control stop — payload is the stopping signal (`WSTOPSIG`'s
    /// source). A top-level variant, not a `BlockReason`, deliberately: every existing block/wake
    /// site (`console/stdin.rs::read`, `fs/pipe.rs`, `do_wait4`) already re-checks its own real
    /// condition fresh after `scheduler::schedule()` returns rather than trusting "state==Ready
    /// now" to mean "my specific event happened" — so resuming a stopped process is just flipping
    /// this back to `Ready`/re-enqueueing; nothing needs to remember which `BlockReason` (if any)
    /// it had. See `do_kill`/`signal_foreground_group`'s own `Action::Stop`/`Action::Cont` arms and
    /// `do_wait4`'s `WUNTRACED`/`WCONTINUED` handling.
    Stopped(u64),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockReason {
    /// `None` = waiting for any child (`wait4(-1, ...)`); `Some(pid)` = waiting for one specific
    /// child.
    WaitingForChild(Option<Pid>),
    /// Blocked in a `crate::pipe` read on an empty, still-open pipe (identified by that pipe's own
    /// id, not by fd — a pipe's read end can be `dup2`'d to other fds, but there's still only one
    /// underlying pipe). See `crate::pipe`'s module doc comment for why a pipe read has to
    /// genuinely block (not just return `Ok(0)`/`EAGAIN`) for a pipeline to work at all on this
    /// single-core, cooperatively-scheduled kernel.
    WaitingForPipeData(u64),
    /// Blocked in a `crate::pipe` write on a full, still-open pipe (identified by pipe id, same
    /// convention as `WaitingForPipeData`) — the write-side counterpart, now that
    /// `crate::fs::pipe::PipeBuffer` is bounded rather than an unboundedly-growable `VecDeque`. Woken
    /// by a reader draining space or by the read side closing (`EPIPE` once woken, if so).
    WaitingForPipeSpace(u64),
    /// Blocked in `crate::console::stdin::read` on an empty keyboard ring buffer. Unlike every other
    /// `BlockReason`, nothing *schedulable* ever wakes this one — the only thing that ever will is
    /// the keyboard IRQ handler itself, which is why `scheduler::schedule()`'s own "nothing
    /// runnable" fallback had to grow a real interrupts-enabled idle wait (`wait_for_ready`)
    /// instead of spinning forever with interrupts masked. See `crate::stdin`'s module doc comment
    /// for the full story — this is what makes `sh.elf` (BusyBox's `hush`), run with no `-c`
    /// argument, able to actually block reading a line from the keyboard instead of seeing an
    /// instant EOF.
    WaitingForStdin,
    /// Blocked in `do_nanosleep` until `interrupts::ticks()` reaches this absolute deadline (not a
    /// duration). Unlike `WaitingForPipeData`/`WaitingForChild` (woken by another schedulable
    /// process's own syscall), the only thing that ever wakes this one is
    /// `interrupts::timer_interrupt_handler` itself — the same "IRQ handler reaches directly into
    /// `process::table()`" shape `crate::console::stdin::push_byte`'s own `wake_blocked_readers` already
    /// established for `WaitingForStdin`, just driven by the timer IRQ instead of the keyboard one.
    Sleeping(u64),
    /// Blocked in `do_pause` (`SYS_PAUSE`) until a deliverable signal (pending and not blocked)
    /// exists — real POSIX `pause(2)` semantics: waits for a signal that either terminates the
    /// process or invokes a caught handler. Woken by `do_kill`/`signal_foreground_group`'s own
    /// `Action::SetPending` arm (the only action shape that corresponds to "a caught handler will
    /// run" — `Action::Discard` is an ignored signal, which must *not* wake a pause; `Action::
    /// Terminate` doesn't need a wake hook at all, since `terminate_process` transitions straight
    /// to `Zombie` regardless of prior state, same "another schedulable process's own syscall
    /// reaches directly into `process::table()`" shape `WaitingForChild`/`WaitingForPipeData`
    /// already established.
    WaitingForSignal,
    /// Blocked in `crate::fs::mqueue`'s `do_mq_timedreceive` on an empty queue, still bounded by a
    /// real deadline (identified by mq id, same convention `WaitingForPipeData`'s pipe id already
    /// establishes) -- `u64::MAX` means "no timeout" (the plain `mq_receive()` wrapper, which
    /// always passes a null `at`). Woken either by `do_mq_timedsend` inserting into a
    /// previously-empty queue, or by `interrupts::timer_interrupt_handler`'s own deadline scan
    /// once `now >= deadline` -- the same dual "another schedulable process's syscall" /
    /// "timer IRQ reaches directly into `process::table()`" shape `Sleeping` already established,
    /// just combined into one wait instead of split across two distinct primitives. A wake from
    /// either source just flips this back to `Ready`; `do_mq_timedreceive`'s own loop re-checks
    /// the real condition (message available, or deadline passed) from scratch either way, same
    /// "never trust state==Ready alone" discipline every other blocking primitive here follows.
    WaitingForMqData(u64, u64),
    /// The write-side counterpart to `WaitingForMqData` -- blocked in `do_mq_timedsend` on a full
    /// queue. Same id/deadline convention, woken by `do_mq_timedreceive` draining a message or by
    /// the same timer-IRQ deadline scan.
    WaitingForMqSpace(u64, u64),
    /// Blocked in `crate::fs::sysv_msg`'s `do_msgsnd` on a real SysV message queue (`msgget`/
    /// `msgctl`, not `crate::fs::mqueue`'s POSIX one) that's currently at its own `qbytes` limit
    /// -- identified by `msqid`, real Linux's own SysV IPC id, not a `crate::fs::fd` real_fd (a
    /// SysV queue has no fd at all, see that module's own doc comment). No deadline -- real
    /// `msgsnd`/`msgrcv` only ever block indefinitely or fail immediately (`IPC_NOWAIT`), unlike
    /// `mq_timedsend`'s real timeout.
    WaitingForSysvMsgSend(u64),
    /// The read-side counterpart to `WaitingForSysvMsgSend` -- blocked in `do_msgrcv` with no
    /// message matching its own real `msgtyp` selection available yet.
    WaitingForSysvMsgRecv(u64),
    /// Blocked in `crate::fs::sysv_sem`'s `do_semop`/`do_semtimedop` on a real SysV semaphore set
    /// (`semid`, real Linux's own SysV IPC id) -- a whole `sembuf` array is applied atomically, so
    /// exactly one op in it is ever the one that couldn't proceed; `semnum`/`waiting_for_zero`
    /// identify *that* op (real semantics: an op with `sem_op < 0` blocks waiting for the
    /// semaphore's value to increase, `sem_op == 0` blocks waiting for it to reach zero -- these
    /// two payload fields exist specifically so `semctl(GETNCNT)`/`semctl(GETZCNT)` can report a
    /// real, not fabricated, count by scanning `process::table()` for a match, the same "IRQ/
    /// syscall handler reaches directly into `process::table()`" shape every other `BlockReason`
    /// here already uses for its own wake/introspection). Woken by any successful `semop`/
    /// `semctl(SETVAL/SETALL)`/`semctl(IPC_RMID)` against this `semid`, or by
    /// `interrupts::timer_interrupt_handler`'s own deadline scan for a real `semtimedop` timeout
    /// (final `u64` field; `u64::MAX` for the plain, non-timed `semop` wrapper's case, same
    /// sentinel convention `WaitingForMqData` already established) -- always re-checks the *whole*
    /// op array from scratch on wake, never trusts the wake reason alone, same discipline every
    /// blocking primitive in this codebase already follows.
    WaitingForSemOp(u64, u16, bool, u64),
    /// Blocked in `do_sigtimedwait` (`SYS_SIGTIMEDWAIT`, backing real `sigtimedwait(2)`/
    /// `sigwaitinfo(2)`/`sigwait(3)`) waiting for any signal in the carried `wait_set` bitmask to
    /// become pending. Deliberately **not** the same primitive `WaitingForSignal` (`pause`/
    /// `sigsuspend`) uses: those wake up so the *normal* delivery machinery
    /// (`take_deliverable_signal`/`deliver_pending_signal`) can invoke a real handler or terminate
    /// the process; `sigtimedwait` instead directly *consumes* a pending signal itself and returns
    /// its number synchronously -- no handler is ever invoked, even if one is installed, matching
    /// real POSIX semantics. `deadline` is the same `u64::MAX`-sentinel "no timeout" convention
    /// `WaitingForSemOp`'s own final field already establishes (the plain `sigwaitinfo`/`sigwait`
    /// case, both of which pass a null timeout). Woken by `do_kill`/`signal_foreground_group`/
    /// `do_sigqueue`'s own `Action::SetPending` arm (`wake_if_sigwaiting`, the same shape
    /// `WaitingForSignal`'s own `wake_if_paused` already establishes) or by `interrupts::
    /// timer_interrupt_handler`'s own deadline scan; `do_sigtimedwait`'s own loop always re-checks
    /// `pending_signals & wait_set` fresh after waking, never trusts the wake reason alone.
    WaitingForSpecificSignal(u64, u64),
}
/// Real signal numbers (Linux/BSD-shared low range) -- the classic, standard `1..=31` range;
/// real-time signals (`SIGRTMIN..=SIGRTMAX`, `35..=64`) are handled separately, see `SIGRTMIN`'s
/// own doc comment. Using real numbers here (rather than inventing OxideBSD-own ones, unlike most of
/// this ABI's own syscalls) is deliberate: a signal *number* is just a plain argument value, not a
/// syscall number, so there's no ABI collision risk the way `open`/`execve` had -- and using the
/// real values is what let `bits/syscall.h.in`'s own remap of `SYS_kill`/`SYS_rt_sigaction`/
/// `SYS_rt_sigprocmask` be the *only* musl-side patch this feature needed (see that file's own
/// comment on the musl fork): musl's `kill.c`/`sigaction.c`/`sigprocmask.c`/`pthread_sigmask.c`
/// already build exactly the wire format `do_kill`/`do_sigaction`/`do_sigprocmask` below expect,
/// unmodified.
pub const SIGHUP: u64 = 1;
pub const SIGINT: u64 = 2;
pub const SIGQUIT: u64 = 3;
pub const SIGILL: u64 = 4;
pub const SIGTRAP: u64 = 5;
pub const SIGABRT: u64 = 6;
pub const SIGBUS: u64 = 7;
pub const SIGFPE: u64 = 8;
pub const SIGKILL: u64 = 9;
pub const SIGUSR1: u64 = 10;
pub const SIGSEGV: u64 = 11;
pub const SIGUSR2: u64 = 12;
pub const SIGPIPE: u64 = 13;
pub const SIGALRM: u64 = 14;
pub const SIGTERM: u64 = 15;
pub const SIGCHLD: u64 = 17;
pub const SIGCONT: u64 = 18;
pub const SIGSTOP: u64 = 19;
pub const SIGTSTP: u64 = 20;
pub const SIGTTIN: u64 = 21;
pub const SIGTTOU: u64 = 22;
pub const SIGURG: u64 = 23;
pub const SIGXCPU: u64 = 24;
pub const SIGXFSZ: u64 = 25;
pub const SIGVTALRM: u64 = 26;
pub const SIGPROF: u64 = 27;
pub const SIGWINCH: u64 = 28;
pub const SIGIO: u64 = 29;
pub const SIGSYS: u64 = 31;
/// Real-time signal range (musl's own `sigrtmin.c`/`sigrtmax.c`: `SIGRTMIN` hardcoded to `35`,
/// `SIGRTMAX = _NSIG - 1 = 64` -- `32..=34` stay permanently unclaimed, matching real glibc/musl
/// convention of reserving a few RT numbers below `SIGRTMIN` for libc-internal use). Unlike the
/// standard range above, multiple `sigqueue`/`raise` instances of the same RT signal number must
/// remain separately queued rather than collapsing to "pending or not" -- see `Process::rt_queue`'s
/// own doc comment for the real per-signal FIFO this requires.
pub const SIGRTMIN: u64 = 35;
pub const SIGRTMAX: u64 = 64;
/// How many RT signal numbers exist (`SIGRTMIN..=SIGRTMAX`) -- `Process::rt_queue`'s own length.
pub(crate) const RT_SIGNAL_COUNT: usize = (SIGRTMAX - SIGRTMIN + 1) as usize;
/// One process's disposition for one signal, indexed `1..=64` into `Process::sigactions`
/// (index `0`, and `32..=34`, unused -- see `SIGRTMIN`'s own doc comment). Mirrors real POSIX
/// `sigaction`'s own `handler` sentinel convention directly -- `0` = `SIG_DFL`, `1` = `SIG_IGN`,
/// anything else is a real handler address -- which is why `do_sigaction`'s own wire-format
/// read/write needs no translation: musl's real `SIG_DFL`/`SIG_IGN` macros already expand to these
/// exact values.
#[derive(Clone, Copy)]
pub struct SigAction {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

impl SigAction {
    const DEFAULT: SigAction = SigAction {
        handler: 0,
        flags: 0,
        restorer: 0,
        mask: 0,
    };
}

/// Real Linux `si_code` values for the two ways a signal can become pending on this kernel --
/// `third_party/musl/include/bits/siginfo.h`'s own `SI_USER = 0`/`SI_QUEUE = -1`. Every signal
/// this kernel delivers arrives via `kill`/`tkill` (`SI_USER`) or `sigqueue` (`SI_QUEUE`) -- never
/// a real hardware fault turned into `SIGSEGV`/`SIGBUS`/... (`page_fault_handler` just logs and
/// reboots, see CLAUDE.md's memory-management section), and never `SI_TKILL` specifically (`do_kill`/
/// `take_deliverable_signal` don't distinguish how a signal was raised -- self, `kill`, or `tkill`
/// all funnel through the same `pending_signals` bitmask, a real, documented simplification).
pub(crate) const SI_USER: i32 = 0;
pub(crate) const SI_QUEUE: i32 = -1;

/// Real, per-pending-signal sender identity/payload -- `Process::pending_siginfo`'s own element
/// type (and, for the real-time range, `Process::rt_queue`'s own element type too). Backs both the
/// `SA_SIGINFO` handler-invocation path's `si_pid`/`si_uid`/`si_value` fields (`src/syscall/mod.rs`'s
/// `deliver_pending_signal`, previously honest zeros -- see `RawSiginfo`'s own doc comment history)
/// and `sigtimedwait`/`sigwaitinfo`'s own real `siginfo_t` readback (`process::signals::
/// do_sigtimedwait`). Real semantics: a second `kill`/`sigqueue` against an already-pending
/// *standard* (`1..=31`) signal simply overwrites `pending_siginfo`'s slot -- POSIX explicitly
/// permits collapsing non-realtime signals to "at most one pending instance." Real-time signals
/// (`SIGRTMIN..=SIGRTMAX`) don't get this collapse -- see `Process::rt_queue`.
#[derive(Clone, Copy, Default)]
pub struct QueuedSigInfo {
    /// `SI_USER` or `SI_QUEUE` -- see those constants' own doc comment.
    pub code: i32,
    /// The real sending process's pid -- `0` only for `signal_foreground_group`'s own two callers
    /// (a real keyboard-generated Ctrl+C/Ctrl+Z, or `do_kill`'s own `kill(-pgrp, sig)` broadcast),
    /// neither of which has one specific real sender process to attribute (an accepted
    /// simplification, not a live-caller-exercised gap -- no seeded applet inspects `si_pid` for a
    /// process-group-broadcast signal).
    pub pid: Pid,
    /// The real sending process's uid -- same "`0` only for the two broadcast callers" story as
    /// `pid` above.
    pub uid: u32,
    /// The real `union sigval` payload -- only ever nonzero for a signal that arrived via
    /// `sigqueue(3)` (`SI_QUEUE`); always `0` for a plain `kill`-shaped (`SI_USER`) signal, which
    /// carries no payload in real POSIX either.
    pub value: u64,
}

/// musl's own `siginfo_t` on x86_64 (`third_party/musl/include/signal.h`): three `int`s
/// (`si_signo`/`si_code`/`si_errno`, no `__SI_SWAP_ERRNO_CODE` on this arch), padded to 8-byte
/// alignment, then the `__si_common` union's two sub-unions -- `si_pid`/`si_uid` (the
/// `__piduid`/user-signal-generated shape, the only one this kernel ever actually fills; the
/// `si_timerid`/`si_overrun` alternative in the same union slot is never populated, no real
/// per-process timer identity exists to report) followed by `si_value`/`si_status`+`si_utime`+
/// `si_stime` (also never populated), then padding out to the real, fixed 128-byte `siginfo_t` size
/// shared across every real Linux architecture. Shared by two real consumers -- the `SA_SIGINFO`
/// handler-invocation path (`src/syscall/mod.rs`'s `deliver_pending_signal`) and `sigtimedwait`/
/// `sigqueue`'s own read/write (`process::signals`) -- rather than duplicated a second time, since
/// getting a wire-format struct's exact layout right in two places independently is exactly the
/// kind of divergence risk this codebase's own "verified via a direct probe" rigor exists to avoid.
#[repr(C)]
pub(crate) struct RawSiginfo {
    pub si_signo: i32,
    pub si_code: i32,
    pub si_errno: i32,
    pub _pad0: i32,
    /// `__si_fields.__si_common.__first.__piduid.si_pid` -- real sender identity now (`QueuedSigInfo::pid`),
    /// not the honest-zero placeholder this field used to always be.
    pub si_pid: i32,
    /// `__si_fields.__si_common.__first.__piduid.si_uid` -- same "real now" story as `si_pid` above.
    pub si_uid: i32,
    /// `__si_fields.__si_common.__second.si_value` (`union sigval`, `sival_ptr`-sized) -- real
    /// `sigqueue(2)` payload now (`QueuedSigInfo::value`), not the honest-zero placeholder this
    /// field used to always be.
    pub si_value: u64,
    pub _tail: [u8; 128 - 4 * 4 - 2 * 4 - 8],
}

const _: () = assert!(core::mem::size_of::<RawSiginfo>() == 128);

/// Real `sigaltstack(2)` flag bits (`third_party/musl/include/signal.h`). `SS_ONSTACK` is a
/// read-only status bit a caller can observe but never set (musl's own `sigaltstack()` wrapper
/// already rejects `ss_flags & SS_ONSTACK` client-side before ever issuing the syscall — see
/// `docs/MISSING_POSIX_SYSCALLS.md`'s own note on this being "only reachable via `sigaltstack()`"
/// the same way `getrandom` is only reachable via `getentropy()`).
pub const SS_ONSTACK: i32 = 1;
pub const SS_DISABLE: i32 = 2;

/// Backing store for `SYS_SIGALTSTACK` (`do_sigaltstack`, `src/process/signals.rs`) — real POSIX
/// per-process alternate-signal-stack bookkeeping. **Bookkeeping only**: no signal is ever
/// actually delivered on this stack — `SA_ONSTACK` isn't honored by `deliver_pending_signal`
/// (`src/syscall/mod.rs`), matching `modules/signal`'s own already-documented "no real signal
/// stack" gap (a second signal during handler execution still overwrites `signal_saved_frame`
/// rather than nesting; a real `sigaltstack` doesn't fix that by itself, see
/// `docs/MISSING_POSIX_SYSCALLS.md`). Since this kernel never actually executes a handler on the
/// alt stack, `SS_ONSTACK` is always reported as unset on read-back — an honest reflection of
/// what actually happens, not a fabricated "yes, active" answer. `Default` is the real POSIX
/// startup state: no alternate stack established (`flags = SS_DISABLE`).
///
/// Copied by `fork` (a real `fork()` duplicates the whole address space, so the alt stack's own
/// address stays valid in the child, same reasoning `cwd`/`brk`/`fs_base` already document); reset
/// to `Default` by `execve` (the old program's address is meaningless in the new image, same
/// reasoning `Process::fs_base` already uses for the identical situation).
#[derive(Clone, Copy)]
pub struct AltStack {
    pub sp: u64,
    pub flags: i32,
    pub size: u64,
}

impl Default for AltStack {
    fn default() -> Self {
        AltStack {
            sp: 0,
            flags: SS_DISABLE,
            size: 0,
        }
    }
}

/// `SYS_TIMER_CREATE`'s max concurrently-armed timers per process (`docs/
/// MISSING_POSIX_SYSCALLS.md`'s pre-reserved batch, items 6-10) -- no live caller in the current
/// roster to size this against (see that doc's own note on the whole POSIX-timer sub-batch), so a
/// small, plausible number rather than real Linux's much larger practical limit. Bumping it later
/// is a pure array-size change with no wire-format implications -- a caller's own opaque `timer_t`
/// is just this array's index.
pub const MAX_POSIX_TIMERS: usize = 8;

/// One `timer_create(2)`-allocated POSIX per-process timer slot -- distinct from the single
/// `ITIMER_REAL`-only `real_timer_deadline`/`real_timer_interval_ticks` pair `do_setitimer` above
/// already owns: a process can hold several of these at once, each independently armed and each
/// free to target any deliverable signal (not just `SIGALRM`). A caller's own opaque `timer_t` is
/// just this slot's index into `Process::posix_timers` -- stable for the timer's whole lifetime,
/// close enough to real Linux's own "small per-process integer" `timer_t` representation that no
/// translation layer is needed.
#[derive(Clone, Copy)]
pub struct PosixTimer {
    /// The `clockid_t` given to `timer_create` (`CLOCK_REALTIME`/`CLOCK_MONOTONIC` only, same
    /// restriction `sys_clock_gettime` already enforces) -- remembered so a later `TIMER_ABSTIME`
    /// `timer_settime` call knows which clock domain its absolute timestamp is expressed in.
    pub clockid: u64,
    /// Real signal number (`1..=31`) to raise on expiry, or `0` for `SIGEV_NONE` (armed, but no
    /// notification is ever delivered -- still real POSIX semantics, just polled via
    /// `timer_gettime`/`timer_getoverrun` instead).
    pub signo: u64,
    /// Absolute `interrupts::ticks()` deadline, `None` while disarmed -- same shape as
    /// `real_timer_deadline` above, just one of several rather than the process's only one.
    pub deadline: Option<u64>,
    /// Reload value in ticks for a periodic timer; `0` means one-shot -- same convention
    /// `real_timer_interval_ticks` above already uses.
    pub interval_ticks: u64,
    /// `timer_getoverrun`'s backing store: how many additional expirations this timer has hit
    /// while its previous expiry's signal was still pending (undelivered) -- resets to `0` the
    /// moment a fresh (not-already-pending) expiry sets the pending bit again. A real, if
    /// simplified, overrun count matching this kernel's already-simple `SIGALRM`-via-
    /// `pending_signals` delivery model, not real Linux's own siginfo-attached per-expiry
    /// accounting. **Known, accepted gap**: two timers sharing the same `signo` can't be told
    /// apart by this bookkeeping, since both would observe the same process-wide pending bit --
    /// no live caller to exercise this today (see `docs/MISSING_POSIX_SYSCALLS.md`).
    pub overrun: u32,
}

/// `SA_NODEFER` -- the one `sa_flags` bit `deliver_pending_signal`'s own mask computation
/// consults (real Linux/x86_64 value; every other flag, `SA_RESTART` included, is accepted but has
/// no observable effect on this kernel -- there's no blocking-syscall-restart machinery to hook it
/// into).
const SA_NODEFER: u64 = 0x40000000;
/// `SA_SIGINFO` (real Linux/x86_64 value) -- consulted by `deliver_pending_signal`
/// (`src/syscall/mod.rs`) to decide whether to invoke the handler as a real 3-argument
/// `void (*)(int, siginfo_t *, void *)` (constructing a real `siginfo_t`/`ucontext_t` on the
/// handler's own stack frame) or the plain 1-argument `void (*)(int)` form. See that function's
/// own doc comment for what is and isn't faithfully populated.
pub(crate) const SA_SIGINFO: u64 = 0x00000004;
/// What `default_disposition` says happens to a signal nothing has installed a handler for (or
/// that's been explicitly reset to `SIG_DFL`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum DefaultDisposition {
    Terminate,
    Ignore,
    /// Real `SIGSTOP`/`SIGTSTP` job-control stop -- see `ProcState::Stopped`'s own doc comment.
    Stop,
}

/// Real POSIX default dispositions for the signals this ABI recognizes (`1..=31`) -- this kernel
/// has no core-dump concept (`Terminate` covers both "term" and "core" defaults identically).
/// `SIGSTOP`/`SIGTSTP` now get a real `Stop` outcome (see `ProcState::Stopped`) rather than the
/// `Ignore` this used to collapse them to before real job control existed; `SIGTTIN`/`SIGTTOU`
/// stay `Ignore` (no real background-terminal-I/O-stop model here -- only `SIGINT`/`SIGTSTP` are
/// ever actually delivered to a foreground group, see `interrupts::keyboard_interrupt_handler`).
/// `SIGCONT` also stays `Ignore` here: waking a `Stopped` target is handled as a pre-dispatch step
/// at every call site *before* this function is even consulted (see `do_kill`/
/// `signal_foreground_group`'s own doc comments) -- a `SIGCONT` that reaches this function at all
/// (self-signal, or a target that wasn't actually stopped) really does just mean "ignore" by
/// default, matching real POSIX. Real-time signals (`SIGRTMIN..=SIGRTMAX`) fall through to the
/// catch-all `Terminate` arm below, matching real POSIX's own RT-signal default -- none of the
/// named constants matched above are ever in that range.
fn default_disposition(sig: u64) -> DefaultDisposition {
    match sig {
        SIGSTOP | SIGTSTP => DefaultDisposition::Stop,
        SIGCHLD | SIGCONT | SIGURG | SIGWINCH | SIGIO | SIGTTIN | SIGTTOU => {
            DefaultDisposition::Ignore
        }
        _ => DefaultDisposition::Terminate,
    }
}
/// `take_deliverable_signal`'s own result -- what `deliver_pending_signal`
/// (`src/syscall.rs`) should actually do about the signal it picked.
pub(crate) enum SignalDelivery {
    /// Terminate the process with this exit code (`128 + signum`, the real shell/POSIX
    /// convention for "died from a signal").
    Terminate(i32),
    /// Real self-targeted `SIGSTOP`/`SIGTSTP` (default disposition) -- payload is the stopping
    /// signal. Only reachable via a self `kill(getpid(), SIGSTOP/SIGTSTP)`; the interactive
    /// Ctrl+Z path (`interrupts::keyboard_interrupt_handler` -> `signal_foreground_group`) always
    /// targets a *different*, not-currently-running process, so it goes through `do_kill`'s own
    /// cross-process `Action::Stop` arm instead of this one. See `process::do_stop_self`.
    Stop(u64),
    /// Redirect the live frame into this handler.
    Handler {
        signum: u64,
        handler: u64,
        restorer: u64,
        /// The blocked-signal mask to install (on top of whatever's already blocked) for the
        /// duration of the handler -- `action.mask` plus the signal's own bit, unless
        /// `SA_NODEFER` was set.
        mask_to_add: u64,
        /// The installed `sa_flags`, forwarded so `deliver_pending_signal` can check
        /// `SA_SIGINFO` without a second table lookup.
        flags: u64,
        /// Real sender identity/payload for this exact signal, snapshotted at the moment it was
        /// picked as deliverable (`Process::pending_siginfo`) -- what the `SA_SIGINFO` case's own
        /// constructed `siginfo_t` (`RawSiginfo`) now populates `si_pid`/`si_uid`/`si_value`/
        /// `si_code` from, in place of the honest zeros this used to always report.
        siginfo: QueuedSigInfo,
    },
}
/// A process's own kernel stack: heap-allocated (not a fixed-size `static`/`static mut` array like
/// `gdt.rs`'s single RSP0 stack, since the number of processes isn't fixed) via a raw
/// `alloc`/`dealloc` pair rather than `Vec<u8>`/`Box<[u8]>`, neither of which guarantees the
/// 16-byte alignment `context_switch::SwitchFrame` needs.
struct KernelStack {
    base: *mut u8,
    layout: core::alloc::Layout,
}

impl KernelStack {
    fn new() -> Self {
        let stack_size = kernel_stack_size();
        let layout =
            core::alloc::Layout::from_size_align(stack_size, 16).expect("bad kernel stack layout");
        // SAFETY: layout has non-zero size (stack_size >= KERNEL_STACK_SIZE_FLOOR > 0).
        let base = unsafe { alloc::alloc::alloc_zeroed(layout) };
        assert!(!base.is_null(), "out of memory allocating a kernel stack");
        KernelStack { base, layout }
    }

    fn top(&self) -> VirtAddr {
        VirtAddr::from_ptr(self.base) + self.layout.size() as u64
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        // SAFETY: base/layout are exactly what alloc_zeroed returned in `new`; KernelStack is
        // never cloned or shared, so this is the sole owner.
        unsafe { alloc::alloc::dealloc(self.base, self.layout) };
    }
}

// SAFETY: KernelStack owns its allocation exclusively -- conceptually equivalent to `Box<[u8]>`'s
// own `Unique<u8>`, which is `Send` for the same reason. Needed because `PROCESS_TABLE` (below) is
// a `Mutex<BTreeMap<Pid, Box<Process>>>` static, which requires `Process` (and transitively this
// raw-pointer-holding field) to be `Send` for `Mutex<..>` to be `Sync`.
unsafe impl Send for KernelStack {}
pub struct Process {
    pub pid: Pid,
    pub parent: Option<Pid>,
    pub children: Vec<Pid>,
    pub state: ProcState,
    pub address_space: AddressSpace,
    #[allow(dead_code)] // kept alive for its Drop impl; never read after construction
    kernel_stack: KernelStack,
    pub kernel_stack_top: VirtAddr,
    /// Saved outgoing RSP for `context_switch::switch_context`, valid only while
    /// `state != Running` (i.e. only while this process isn't the one currently executing).
    pub rsp: u64,
    /// Consulted only by `spawn_trampoline_inner`, the first time a never-run process starts.
    pub entry_point: VirtAddr,
    /// Despite the name, this is the *initial `RSP`* the process starts running with — the
    /// `user_stack::build`-computed address of the argc/argv/envp/auxv image's own start, not the
    /// bare top of the mapped stack region (`process::USER_STACK_TOP`). Handed straight to
    /// `usermode::jump_to_usermode`/`syscall::redirect_frame` as the stack pointer.
    pub user_stack_top: VirtAddr,
    /// The current top of this process's `SYS_BRK`-managed heap region — see `do_brk`. Starts at
    /// the loaded ELF's `Elf::highest_loaded_address()` (page-aligned), grows/shrinks from there.
    pub brk: VirtAddr,
    /// This process's own `IA32_FS_BASE` value (see `SYS_SET_FS_BASE`), restored into the real MSR
    /// on every context switch into this process (`scheduler::activate_and_prepare`) — `IA32_FS_BASE`
    /// is a single global MSR, not something `context_switch::switch_context` saves/restores the
    /// way it does `RSP`/callee-saved GPRs, so without this every musl-linked process (which uses
    /// `%fs`-relative TLS, including the stack-protector canary check at `%fs:0x28`) would silently
    /// clobber every *other* process's TLS base the instant it set its own. A real, previously-
    /// latent bug: never surfaced by `true`/`echo`/`cat`/`musl-smoke` (each run directly by `stsh`,
    /// which isn't musl-linked and never touches `%fs` itself, and none of them survives long enough
    /// for a *different* still-running musl-linked process to resume and read a stale value) --
    /// only found once `sh` (BusyBox's `hush`, itself musl-linked) forked and `execve`'d another
    /// musl-linked child and then kept running *after* that child exited: `hush`'s own next
    /// stack-protector check read through the dead child's own leftover `FS_BASE`, page-faulting on
    /// whatever happened to be at that address in `hush`'s own (unrelated) address space. Starts at
    /// `0` for a freshly spawned/`execve`'d process (no TLS set up yet); a forked child inherits the
    /// parent's live value (real `fork()` semantics — TLS state is copied, not reset).
    pub fs_base: u64,
    /// This process's current-working-directory, as an opaque inode number `modules/oxfs` (not the
    /// kernel) assigns meaning to — the kernel never interprets it, just persists/restores it per
    /// process on `fork`/`spawn` exactly like `brk`/`fs_base` already do. `0` is oxfs's own root
    /// inode number, which conveniently doubles as "unset" for a freshly spawned process. Fixes the
    /// "one, kernel-wide current directory" limitation `modules/fat32` had (see CLAUDE.md) — now
    /// that real processes exist, cwd is genuinely per-process. `do_execve` deliberately leaves
    /// this untouched (real `execve()` preserves the caller's cwd, unlike `fs_base`, which an
    /// exec'd program's own TLS layout makes meaningless to keep).
    pub cwd: u64,
    /// This process's own root directory (`SYS_CHROOT`) -- same "opaque inode number `modules/oxfs`
    /// assigns meaning to, kernel just persists/restores it" shape as `cwd` immediately above,
    /// right down to `0` doubling as both oxfs's real root inode number *and* "never chrooted".
    /// Copied by `fork` (real `chroot(2)` semantics: a forked child stays confined to whatever root
    /// its parent already had); left untouched by `execve` (real `chroot(2)` persists across
    /// `exec` too -- unlike `fs_base`, there's no reason a new program's own layout would make this
    /// meaningless to keep).
    pub root_inode: u64,
    /// Bitmask, bit `N-1` = signal `N` is pending delivery. Set by `do_kill`; drained by
    /// `take_deliverable_signal` (called once, at the tail of every completed syscall — see
    /// `src/syscall.rs`'s `deliver_pending_signal`).
    pub pending_signals: u64,
    /// Bitmask, bit `N-1` = signal `N` is currently blocked (won't be delivered even if pending,
    /// until unblocked) — set by `SYS_SIGPROCMASK`, and temporarily grown by
    /// `stash_signal_context` for the duration of a handler's own execution (restored by
    /// `take_signal_saved_frame` on `sigreturn`).
    pub blocked_signals: u64,
    /// Indexed `1..=64` (index `0`, and `32..=34`, unused) — see `SigAction`'s own doc comment.
    pub sigactions: [SigAction; (SIGRTMAX + 1) as usize],
    /// The interrupted context a `Handler`-disposition delivery snapshotted, restored verbatim by
    /// `sigreturn` (`take_signal_saved_frame`) once the handler itself returns. `None` whenever
    /// this process isn't currently inside a signal handler.
    pub(crate) signal_saved_frame: Option<SyscallFrame>,
    /// `blocked_signals`'s value from just before the handler above was entered — restored
    /// alongside `signal_saved_frame` on `sigreturn`. Meaningless while `signal_saved_frame` is
    /// `None`.
    pub signal_saved_blocked: u64,
    /// `SYS_SIGALTSTACK`'s backing store — see `AltStack`'s own doc comment for the real
    /// bookkeeping-only semantics and fork/execve treatment.
    pub altstack: AltStack,
    /// Unused today; reserved so a future priority scheduler doesn't need a PCB layout change.
    #[allow(dead_code)]
    pub priority: u8,
    /// This process's process-group ID — an ordinary `Pid`, not a distinct namespace (real POSIX
    /// process groups are just pids reinterpreted). Persisted/restored per process on
    /// `fork`/`spawn` exactly like `cwd`/`fs_base`/`brk` already are. A freshly `spawn`ed process
    /// (pid 1 today — no parent to inherit a group from) becomes its own group leader
    /// (`pgid == pid`), matching real init/session-leader convention; a forked child inherits the
    /// parent's live `pgid` (real `fork()` semantics — a child stays in its parent's group until it
    /// calls `setpgid` itself). `do_execve` deliberately leaves this untouched, same reasoning as
    /// `cwd` — real `execve()` preserves the caller's process group.
    pub pgid: Pid,
    /// This process's session ID — same "ordinary `Pid`, not a distinct namespace" shape as `pgid`.
    /// A freshly `spawn`ed process becomes its own session leader (`sid == pid`), same as `pgid`;
    /// a forked child inherits the parent's live `sid` unchanged (real `fork()` semantics — a child
    /// stays in its parent's session until it calls `setsid` itself, and `setsid` itself refuses a
    /// caller that's already a process-group leader — see `do_setsid`). `do_execve` leaves this
    /// untouched, same reasoning as `cwd`/`pgid`. Paired with `stdin::CONTROLLING_SESSION` (a
    /// single global, not a per-session table — this kernel has exactly one real console/tty, so
    /// "which session owns the controlling tty" only ever needs one slot, the same simplification
    /// already accepted for `stdin::TERMIOS`).
    pub sid: Pid,
    /// Executable basename (no path, no parens) — populated by `do_execve`/`spawn`, copied
    /// verbatim by `fork` (real `fork()` semantics: a child keeps its parent's `comm`/`cmdline`
    /// until it execs its own). Backs `/proc/[pid]/stat`'s `(comm)` field and `status`'s `Name:`.
    pub comm: Vec<u8>,
    /// Real `/proc/[pid]/cmdline` wire format: each `argv` entry followed by one NUL byte, no
    /// trailing separator beyond that. Same copy-on-fork/reset-on-exec semantics as `comm`.
    pub cmdline: Vec<u8>,
    /// `SYS_SETITIMER`'s `ITIMER_REAL` state (`do_setitimer`/`do_getitimer`) — the absolute
    /// `interrupts::ticks()` deadline this process's next `SIGALRM` fires at, `None` while
    /// disarmed. Checked by `interrupts::timer_interrupt_handler` alongside its existing
    /// `BlockReason::Sleeping` scan. Real `alarm(2)` is musl's own `setitimer()` wrapper (see
    /// `third_party/musl/src/unistd/alarm.c`) — no separate kernel mechanism needed for it.
    /// **Not inherited by `fork`** (`None`/`0` in a freshly forked child) — matches real POSIX
    /// itimer semantics (a child starts with its own disarmed timer, unlike `cwd`/`brk`/`fs_base`,
    /// which *are* copied). `do_execve` leaves both fields untouched (preserved, like `cwd`/`pgid`
    /// — real `execve()` doesn't reset a pending itimer).
    pub real_timer_deadline: Option<u64>,
    /// Reload value in ticks for a periodic (`it_interval != 0`) real-timer; `0` means one-shot —
    /// the expiry handler clears `real_timer_deadline` instead of rearming it. Meaningless while
    /// `real_timer_deadline` is `None`.
    pub real_timer_interval_ticks: u64,
    /// `SYS_TIMER_CREATE`-allocated slots (batch items 6-10, `docs/MISSING_POSIX_SYSCALLS.md`) —
    /// `None` = free slot; see `PosixTimer`'s own doc comment. Checked by
    /// `interrupts::timer_interrupt_handler` alongside the `real_timer_deadline` scan just above.
    /// **Not inherited by `fork`, disarmed and deleted by `execve`** — real POSIX semantics
    /// (`timer_create(2)`'s own NOTES section: "not inherited... disarmed and deleted during
    /// execve(2)"), the same "starts fresh" story `real_timer_deadline` already tells for `fork`
    /// — though unlike that field, `execve` resets this one too (matching `altstack`'s own reset):
    /// a POSIX timer's whole purpose, notifying the very program that created it, means nothing to
    /// the new image.
    pub posix_timers: [Option<PosixTimer>; MAX_POSIX_TIMERS],
    /// Real uid/gid — no distinct saved/effective pair, since this kernel has no setuid-bit
    /// `execve` support to ever make them diverge (see `do_execve`'s own doc comment: it never
    /// touches these fields at all, matching real `execve()`'s "preserved unless the setuid/setgid
    /// bit is set" semantics collapsing to plain preservation when that bit can never be set).
    /// `SYS_GETEUID`/`SYS_GETEGID` just echo these same fields back. `0` (root) at `spawn` — this
    /// kernel has no login mechanism, so pid 1 (and everything downstream) starts as root, matching
    /// a real single-user embedded/init system more than a real multi-user boot. Copied by `fork`
    /// (real `fork()` semantics, same as `cwd`/`pgid`/`brk`).
    pub uid: u32,
    pub gid: u32,
    /// `SYS_PRLIMIT64`'s backing store -- one `(rlim_cur, rlim_max)` pair per real `RLIMIT_*`
    /// resource (`RLIM_NLIMITS = 16` on real Linux), `u64::MAX` (`RLIM_INFINITY`) in every slot by
    /// default. Copied by `fork` (real POSIX rlimit/fork semantics, same as `uid`/`gid`/`pgid`);
    /// preserved by `execve` (untouched, same as `cwd`/`pgid`). **Stored, never enforced** — an
    /// honest, documented gap, the same tier as several other accepted-but-unenforced fields
    /// already in this codebase (e.g. `O_NONBLOCK` on a TCP socket, see `src/net/tcp.rs`).
    pub rlimits: [(u64, u64); 16],
    /// `SYS_SETPRIORITY`/`SYS_GETPRIORITY`'s backing store — real `nice` range is `-20..=19`, `0`
    /// by default. Stored and echoed back honestly; this kernel's cooperative round-robin
    /// scheduler has no priority concept at all, so it has no real scheduling effect. Copied by
    /// `fork`; preserved by `execve` (same reasoning as `rlimits` above).
    pub nice: i32,
    /// `SYS_SCHED_SETSCHEDULER`/`SYS_SCHED_GETSCHEDULER`'s backing store — real Linux
    /// `SCHED_RR = 2` by default (matching BusyBox `chrt`'s own default when no `-r`/`-f`/`-o`/
    /// `-b`/`-i` flag is given). No real RT scheduling class exists here — stored and echoed back
    /// honestly, same tier as `nice` above. Copied by `fork`; preserved by `execve`.
    pub sched_policy: i32,
    /// `SYS_SCHED_SETSCHEDULER`/`SYS_SCHED_GETPARAM`'s backing store — the `sched_priority` field
    /// of a real `struct sched_param`, `0` by default. Same "stored, not enforced" tier as
    /// `sched_policy` above. Copied by `fork`; preserved by `execve`.
    pub sched_priority: i32,
    /// `SYS_UMASK`'s backing store — real POSIX `umask()` always succeeds and returns the
    /// *previous* mask, so this has to be real per-process state, not a pure function. `0o022`
    /// (the standard real Unix default) rather than `0`, since a plausible ambient value matters
    /// here in a way it doesn't for `nice`/`sched_policy`: BusyBox's `libbb/parse_mode.c` reads it
    /// back via `umask(0)`/`umask(old)` to compute symbolic mode changes (`chmod +x`, ...) — found
    /// live testing `chmod +x` on a real script, before this field existed, computing garbage from
    /// the `ENOSYS` this ABI used to return for the then-unmapped `umask(2)` syscall (see
    /// `third_party/musl/arch/x86_64/bits/syscall.h.in`'s own `__NR_umask` comment). Same
    /// "stored, not enforced" tier as `rlimits`/`nice`/`sched_policy` otherwise — oxfs doesn't
    /// consult this anywhere it creates a new inode. Copied by `fork`; preserved by `execve`.
    pub umask: u32,
    /// Set when this process transitions into `ProcState::Stopped` (real `SIGSTOP`/`SIGTSTP`),
    /// cleared once a parent's `wait4(WUNTRACED)` observes it — the real POSIX "report a stop
    /// once" contract. Also cleared the moment a `SIGCONT` resumes this process (a deliberate
    /// simplification from real Linux, which would still let a parent observe the stop after the
    /// fact even post-resume): `ProcState::Stopped`'s payload -- the only place the stopping
    /// signal number is stored -- is gone the instant `state` flips back to `Ready`, so reporting
    /// a stop after resume would need a second field just to remember which signal it was. Not
    /// worth it for a real practical shell's own polling cadence (`hush`'s own `checkjobs()`
    /// only reaches `fg`/`bg`'s `SIGCONT` after the user has already seen the job listed as
    /// stopped). Not copied by `fork` (a freshly forked child is never born stopped); not touched
    /// by `execve`.
    pub stop_notify_pending: bool,
    /// Set when a `SIGCONT` actually resumes this process from `Stopped` -- same "report once,
    /// cleared by a matching `wait4(WCONTINUED)`" shape as `stop_notify_pending` above. Not copied
    /// by `fork`; not touched by `execve`.
    pub cont_notify_pending: bool,
    /// `SYS_SIGSUSPEND`'s own transient handoff -- the mask to restore once `do_sigsuspend`'s
    /// temporarily-swapped-in mask has done its job (see that function's own doc comment for why
    /// the restore can't happen inside `do_sigsuspend` itself). `Some` only for the brief window
    /// between `do_sigsuspend` returning `Err(EINTR)` and `deliver_pending_signal` consuming it,
    /// both within the same `syscall_dispatch` call -- never observable at a `fork`/`execve`
    /// boundary (no other syscall can be issued by this process in between), so both just start a
    /// fresh child/keep it `None` rather than threading it through either constructor's own
    /// parent-state tuple.
    pub sigsuspend_restore_mask: Option<u64>,
    /// Real SysV `SEM_UNDO` bookkeeping (`crate::fs::sysv_sem`) -- one accumulated adjustment per
    /// `(semid, semnum)` this process has ever `semop`'d with `SEM_UNDO` set, applied (added back)
    /// automatically on process termination (`terminate_process`, before the table lock is taken --
    /// see that function's own doc comment on why) and cleared afterward. Real POSIX semantics:
    /// **not inherited by `fork`** (a child starts with its own empty undo list -- real Linux's
    /// `semadj` is `fork`-local); **preserved across `execve`** (untouched here since `do_execve`
    /// mutates the live `Process` in place and never assigns this field, the same "leave it alone"
    /// treatment `cwd`/`pgid`/`rlimits` already get).
    pub sysv_sem_undo: Vec<(i32, u16, i32)>,
    /// Real SysV shared-memory attachment bookkeeping (`crate::fs::sysv_shm`) -- one `(addr,
    /// shmid)` pair per successful `shmat`, in call order, so `shmdt(addr)` can find (and remove)
    /// the matching entry and `crate::fs::sysv_shm::detach_all_for_exit` can decrement every
    /// still-attached segment's own `nattch` on real process termination. **Not inherited by
    /// `fork`** -- a deliberate, documented simplification tied to this kernel's own "no
    /// copy-on-write fork" limitation, not an independent design choice; see `crate::fs::
    /// sysv_shm`'s own doc comment for why inheriting the list wouldn't actually preserve real
    /// sharing anyway. **Real implicit detach on `execve`** — `do_execve` calls the exact same
    /// `crate::fs::sysv_shm::detach_all_for_exit` a real process exit does (after committing the
    /// new `AddressSpace`, which is what actually makes every prior `shmat`'s own mapping
    /// unreachable): matches real Linux, where `execve(2)` destroying the old address space is
    /// itself a real implicit detach of everything that was attached to it.
    pub sysv_shm_attach: Vec<(u64, i32)>,
    /// Real fd-backed `MAP_SHARED` mappings still live in this process's own address space (see
    /// `mm::MmapFileRegion`'s own doc comment) — one entry per successful `mmap(fd >= 0, ...)`,
    /// removed by a matching `munmap` or by exit/`execve` cleanup (`mm::
    /// cleanup_mmap_file_regions_for_exit`). **Not inherited by `fork`** — same documented
    /// simplification, and the same underlying reason, as `sysv_shm_attach` immediately above.
    pub mmap_file_regions: Vec<mm::MmapFileRegion>,
    /// Real per-standard-signal-number sender identity/payload, indexed `1..=31` same as
    /// `sigactions` (index `0` unused) -- see `QueuedSigInfo`'s own doc comment. Not copied by
    /// `fork` (a forked child starts with `pending_signals == 0` too, so there's nothing
    /// meaningful to carry over); untouched by `execve` (same "leave it alone" treatment
    /// `pending_signals`/`blocked_signals` already get there). Real-time signals use `rt_queue`
    /// below instead -- this array is never indexed past `31`.
    pub pending_siginfo: [QueuedSigInfo; 32],
    /// Real per-real-time-signal-number FIFO queue, indexed `sig - SIGRTMIN` (`0..RT_SIGNAL_COUNT`).
    /// Unlike `pending_siginfo` above, a real-time signal must not collapse multiple queued
    /// instances into one -- POSIX explicitly requires `sigqueue`/`raise` against an already-pending
    /// RT signal number to add a *separate* queued instance, not overwrite it (what
    /// `sigqueue/4-1.c`/`sigwait/2-1.c` in the Open POSIX Test Suite pilot actually check). Each
    /// signal number's own `Vec` is capped at `signals::RT_QUEUE_CAP` -- exceeding it is a real,
    /// documented `sigqueue(2)` `EAGAIN` case, not a soundness concern (small and fixed, same tier
    /// as every other bounded queue in this codebase). Pushed to the back by `signals::
    /// record_pending`, popped from the front (real FIFO delivery order within one signal number)
    /// by `take_deliverable_signal`/`do_sigtimedwait` -- `Process::pending_signals`' own bit for
    /// this signal number stays set as long as this queue is non-empty, only clearing once the last
    /// queued instance is actually consumed. Not copied by `fork`, same reasoning as
    /// `pending_siginfo`; untouched by `execve`.
    pub rt_queue: [Vec<QueuedSigInfo>; RT_SIGNAL_COUNT],
    /// This process's saved `FXSAVE`/`FXRSTOR` (SSE/x87) register state — see `cpu::fpu`'s own
    /// module doc comment for why this exists at all (real preemption, unlike the old
    /// cooperative-only scheduler, can interrupt a process mid-computation with live XMM state the
    /// compiler never spilled). Saved by `scheduler::schedule`/`start` right before switching away
    /// from this process, restored right before switching into it — on *every* switch, not just a
    /// preemptive one, so there's no cooperative/preemptive special case to get wrong. Starts as
    /// `cpu::fpu::clean_state()` (a real CPU-reset image, not zeroed) for a freshly spawned/forked
    /// process; a forked child copies the parent's live state (real `fork()` semantics).
    pub fpu_state: crate::cpu::fpu::FxSaveArea,
    /// Real per-process CPU-time accounting, in `interrupts::TICKS`-cadence ticks (`TIMER_HZ`,
    /// same clock `CLOCK_MONOTONIC` already converts from) -- incremented by exactly `1` on every
    /// timer tick where this process is the one actually `Running` when the tick lands
    /// (`interrupts::timer_interrupt_handler`, right alongside its own preemption check). Backs
    /// `sys_clock_gettime`'s `CLOCK_PROCESS_CPUTIME_ID`/`CLOCK_THREAD_CPUTIME_ID` (no real
    /// threading exists -- a process's only "thread" is itself, same simplification `/proc/<pid>/
    /// task` already uses -- so both clockids read this same counter). Found live: `clock_gettime/
    /// 4-1.c` (the Open POSIX Test Suite pilot) went UNRESOLVED, not the legitimate `UNSUPPORTED`
    /// its own `sysconf(_SC_CPUTIME) == -1` escape hatch would have produced -- musl's own
    /// `sysconf()` unconditionally reports `_SC_CPUTIME` as supported (`third_party/musl/src/conf/
    /// sysconf.c`, a compile-time claim, not a runtime capability probe), so the test proceeded to
    /// call `clock_gettime(CLOCK_PROCESS_CPUTIME_ID, ...)` and got a real, until-now-unhandled
    /// `EINVAL` instead. Not real, precise CPU-time accounting (it's tick-granularity, and doesn't
    /// distinguish user/kernel time or subtract time spent `Blocked`/`Stopped`) -- but it's the
    /// same honesty tier `ticks()`-derived `CLOCK_MONOTONIC` already sets, and is enough to satisfy
    /// what any real caller here actually checks (this pilot test only requires successive reads to
    /// be non-decreasing under real work, not wall-clock-accurate). Starts at `0` for spawn *and*
    /// a forked child (real POSIX: a process's own CPU time never carries over from a parent);
    /// preserved by `execve` (the same process, still accumulating, just running a new image).
    pub cpu_ticks: u64,
}
static NEXT_PID: AtomicU64 = AtomicU64::new(1);
static PROCESS_TABLE: Mutex<BTreeMap<Pid, Box<Process>>> = Mutex::new(BTreeMap::new());

fn alloc_pid() -> Pid {
    NEXT_PID.fetch_add(1, Ordering::Relaxed)
}

/// `Box<Process>` (not `Process` by value) is load-bearing, not stylistic: it lets callers pull a
/// raw `*mut Process`/copy out needed fields from under a short-held lock and drop that lock
/// *before* doing anything that might call `scheduler::schedule()` — the `BTreeMap`'s internal
/// tree nodes can move on insert/remove, but a `Box`'s own heap allocation never does. Holding this
/// lock across a context switch would only be released whenever that exact stack next resumes —
/// a real deadlock risk if the switched-to process needs this same lock, which it always will.
pub(crate) fn table() -> &'static Mutex<BTreeMap<Pid, Box<Process>>> {
    &PROCESS_TABLE
}
#[derive(Debug)]
pub enum SpawnError {
    Elf(elf::ElfError),
}

pub use lifecycle::*;
pub use signals::*;
pub use identity::*;
pub use limits::*;
pub use timers::*;
pub use mm::*;
// `pub(crate) use`, not `pub use`: every item in `procfs` is itself `pub(crate)` (its
// `oxidebsd_proc_*` functions are resolved as `crate::process::oxidebsd_proc_*` by
// `src/module.rs`'s FFI table, which is in-crate but not `procfs`'s own module) -- matching that
// exact visibility avoids both a hollow `pub` claim (nothing outside the crate can see these
// anyway) and a plain private `use`, which wouldn't re-export far enough for `module.rs` to see.
pub(crate) use procfs::*;

pub mod identity;
pub mod lifecycle;
pub mod limits;
pub mod mm;
pub mod procfs;
pub mod signals;
pub mod timers;

// Pre-existing, already-standalone modules moved into this directory unsplit (see the reorg plan)
// -- unlike the seven above, these were never part of process.rs's own namespace, so they keep
// their own explicit submodule path (`crate::process::scheduler::X`, not flattened into
// `crate::process::X`) rather than being re-exported.
pub mod context_switch;
pub mod elf;
pub mod fault_trampoline;
pub mod scheduler;
pub mod user_stack;
pub mod usermode;
