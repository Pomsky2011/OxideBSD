//! rlimit/priority/sched-policy syscalls -- split out of the original process.rs.



use crate::syscall::{EAGAIN, EINTR, EINVAL, EPERM, ETIMEDOUT};
use super::*;

/// musl's own `struct timespec` on x86_64 -- see `src/syscall/ffi.rs`'s/`src/process/timers.rs`'s
/// own `RawTimespec` (duplicated here rather than shared, same "no shared crate across this
/// internal ABI boundary" convention every other copy already follows).
#[derive(Clone, Copy)]
#[repr(C)]
struct RawTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

/// Real `struct rlimit` on x86_64 -- two plain `u64`s (`rlim_cur`, `rlim_max`), `RLIM_INFINITY`
/// wire-compatible with `u64::MAX` (musl's own `getrlimit.c`/`setrlimit.c` already special-case
/// that exact value, see `SYSCALL_RLIM_INFINITY`'s own `FIX()` macro in those files).
#[derive(Clone, Copy)]
#[repr(C)]
struct RawRlimit {
    rlim_cur: u64,
    rlim_max: u64,
}

/// Real Linux's own `RLIM_NLIMITS` -- the number of `Process::rlimits` slots that exist.
const RLIM_NLIMITS: u64 = 16;

/// `SYS_PRLIMIT64`'s real logic -- real `prlimit64(pid, resource, new_limit, old_limit)`: writes
/// the resource's *old* value to `old_ptr` first (if non-null), matching real Linux's own
/// "read-then-write, atomically" contract, then applies `new_ptr`'s value (if non-null). Either
/// pointer may be null independently (a bare read, a bare write, or both). See
/// `Process::rlimits`'s own doc comment for why nothing here is actually enforced.
pub fn do_prlimit64(
    caller_pid: Pid,
    pid: i64,
    resource: u64,
    new_ptr: u64,
    old_ptr: u64,
) -> Result<u64, u64> {
    if resource >= RLIM_NLIMITS {
        return Err(EINVAL);
    }
    let target = resolve_target_pid(caller_pid, pid)?;
    let mut table = PROCESS_TABLE.lock();
    let proc = table
        .get_mut(&target)
        .expect("prlimit64: target process missing from table");
    if old_ptr != 0 {
        let (cur, max) = proc.rlimits[resource as usize];
        // SAFETY: same known pointer-validation gap every other user-memory write in this
        // codebase already has.
        unsafe {
            (old_ptr as *mut RawRlimit).write(RawRlimit {
                rlim_cur: cur,
                rlim_max: max,
            })
        };
    }
    if new_ptr != 0 {
        // SAFETY: same known pointer-validation gap every other user-memory read in this
        // codebase already has.
        let new = unsafe { *(new_ptr as *const RawRlimit) };
        proc.rlimits[resource as usize] = (new.rlim_cur, new.rlim_max);
    }
    Ok(0)
}

/// `SYS_SETPRIORITY`'s real logic -- only `PRIO_PROCESS` (`0`) is supported (`PRIO_PGRP`/
/// `PRIO_USER` are `EINVAL`; no target applet in this port's roster uses either). See
/// `Process::nice`'s own doc comment for why storing it has no real scheduling effect.
pub fn do_setpriority(caller_pid: Pid, which: u64, who: i64, prio: i32) -> Result<u64, u64> {
    if which != PRIO_PROCESS {
        return Err(EINVAL);
    }
    let target = resolve_target_pid(caller_pid, who)?;
    let mut table = PROCESS_TABLE.lock();
    table
        .get_mut(&target)
        .expect("setpriority: target process missing from table")
        .nice = prio.clamp(-20, 19);
    Ok(0)
}

/// `SYS_GETPRIORITY`'s real logic -- real Linux's own `getpriority(2)` returns `20 - nice`
/// (never negative, since the real syscall ABI can't otherwise distinguish "nice value `-1`" from
/// "error"); musl's own `getpriority()` wrapper un-shifts this client-side (`third_party/musl/
/// src/misc/getpriority.c`), so the raw value returned here must already be shifted the same way.
pub fn do_getpriority(caller_pid: Pid, which: u64, who: i64) -> Result<u64, u64> {
    if which != PRIO_PROCESS {
        return Err(EINVAL);
    }
    let target = resolve_target_pid(caller_pid, who)?;
    let table = PROCESS_TABLE.lock();
    let nice = table
        .get(&target)
        .expect("getpriority: target process missing from table")
        .nice;
    Ok((20 - nice) as u64)
}

/// `SYS_UMASK`'s real logic -- real POSIX `umask(2)` always succeeds and returns the *previous*
/// mask. Masked to `0o777` (the only bits a real file-permission mask can meaningfully hold),
/// matching real Linux/musl's own `umask()` wrapper's implicit contract. See `Process::umask`'s
/// own doc comment for why this needed to be real per-process state rather than a stub, and why
/// it's stored/returned honestly without actually being consulted anywhere oxfs creates a file.
pub fn do_umask(caller_pid: Pid, new_mask: u32) -> Result<u64, u64> {
    let table = PROCESS_TABLE.lock();
    let proc = table
        .get(&caller_pid)
        .expect("umask: current process missing from table");
    let mut shared = proc.shared.lock();
    let old_mask = shared.umask;
    shared.umask = new_mask & 0o777;
    Ok(old_mask as u64)
}

const PRIO_PROCESS: u64 = 0;

/// Real Linux's own `SCHED_RR` value -- `Process::sched_policy`'s default, matching BusyBox
/// `chrt`'s own default policy when none of `-r`/`-f`/`-o`/`-b`/`-i` is given.
pub(crate) const SCHED_RR_DEFAULT: i32 = 2;
const SCHED_OTHER: i32 = 0;
const SCHED_FIFO: i32 = 1;
const SCHED_RR: i32 = 2;
const SCHED_BATCH: i32 = 3;
const SCHED_IDLE: i32 = 5;
const SCHED_DEADLINE: i32 = 6;

/// Real-Linux-matching priority range per policy (`SCHED_FIFO`/`SCHED_RR` -> `1..=99`, every other
/// *known* policy -> `0..=0`) -- shared by `do_sched_setscheduler`'s own validation and
/// `do_sched_get_priority_max`/`_min`, which is exactly what real POSIX/Linux use to define this
/// range in the first place (`man 2 sched_setscheduler` documents the valid range as literally
/// `sched_get_priority_min(policy)..=sched_get_priority_max(policy)`). Callers are expected to have
/// already rejected an unknown policy via `is_known_sched_policy` -- this function has no opinion
/// on that, it just describes every known policy's own range.
fn sched_priority_range(policy: i32) -> (i32, i32) {
    if policy == SCHED_FIFO || policy == SCHED_RR {
        (1, 99)
    } else {
        (0, 0)
    }
}

/// Real `struct sched_param` on x86_64 -- a single `int sched_priority` field (real Linux's own
/// layout has no other members on this arch).
#[derive(Clone, Copy)]
#[repr(C)]
struct RawSchedParam {
    sched_priority: i32,
}

/// Real POSIX permission rule shared by `sched_setscheduler(2)`/`sched_getscheduler(2)`/
/// `sched_getparam(2)`: the caller must be root, or its own uid must match the target's -- same
/// shape `process::signals::has_signal_permission` already establishes for `kill`/`sigqueue`,
/// duplicated rather than shared across this module boundary (matches this codebase's own
/// established precedent for this exact shape, see that function's own doc comment). Targeting
/// self (`pid == 0`, resolved to the caller's own pid by `resolve_target_pid` before this is ever
/// consulted) always passes trivially, since a process's own uid always equals itself.
fn has_sched_permission(caller_uid: u32, target_uid: u32) -> bool {
    caller_uid == 0 || caller_uid == target_uid
}

/// Real Linux's own set of *defined* scheduling policies -- `SCHED_ISO` (`4`) was reserved but
/// never actually shipped, so `3`/`5`/`6` aren't contiguous with `0..=2`. Anything outside this set
/// is a real `EINVAL` from `sched_setscheduler`/`sched_get_priority_max`/`_min`, not silently
/// treated as some other policy's own range.
fn is_known_sched_policy(policy: i32) -> bool {
    matches!(policy, SCHED_OTHER | SCHED_FIFO | SCHED_RR | SCHED_BATCH | SCHED_IDLE | SCHED_DEADLINE)
}

/// `SYS_SCHED_SETSCHEDULER`'s real logic. Real permission checking
/// (`has_sched_permission`) -- found live: `sched_setscheduler/20-1.c` expects `EPERM` targeting
/// pid `1` (root-owned) from a real non-root euid, previously unreachable since nothing here ever
/// checked the target's uid at all. Real priority-range validation against `policy`'s own
/// `sched_get_priority_min`/`_max` -- found live: `sched_setscheduler/19-1.c` expects `EINVAL` for
/// an out-of-range priority, previously accepted verbatim with no validation at all. `ESRCH` (via
/// `resolve_target_pid`) is checked first, before either of the above -- `sched_setscheduler/21-1.c`
/// targets an already-reaped pid and must see `ESRCH` regardless of the (here, valid) priority it
/// supplies.
pub fn do_sched_setscheduler(
    caller_pid: Pid,
    pid: i64,
    policy: i32,
    param_ptr: u64,
) -> Result<u64, u64> {
    let target = resolve_target_pid(caller_pid, pid)?;
    if !is_known_sched_policy(policy) {
        return Err(EINVAL);
    }
    // SAFETY: same known pointer-validation gap every other user-memory read in this codebase
    // already has.
    let param = unsafe { *(param_ptr as *const RawSchedParam) };
    let mut table = PROCESS_TABLE.lock();
    let caller_uid = table
        .get(&caller_pid)
        .expect("sched_setscheduler: caller process missing from table")
        .shared
        .lock()
        .uid;
    let proc = table
        .get_mut(&target)
        .expect("sched_setscheduler: target process missing from table");
    if !has_sched_permission(caller_uid, proc.shared.lock().uid) {
        return Err(EPERM);
    }
    let (min, max) = sched_priority_range(policy);
    if !(min..=max).contains(&param.sched_priority) {
        return Err(EINVAL);
    }
    proc.sched_policy = policy;
    proc.sched_priority = param.sched_priority;
    Ok(0)
}

/// `SYS_SCHED_GETSCHEDULER`'s real logic -- echoes back the stored `Process::sched_policy`. Real
/// permission checking (`has_sched_permission`), same reasoning `do_sched_setscheduler`'s own doc
/// comment gives -- closes `sched_getscheduler/7-1.c`.
pub fn do_sched_getscheduler(caller_pid: Pid, pid: i64) -> Result<u64, u64> {
    let target = resolve_target_pid(caller_pid, pid)?;
    let table = PROCESS_TABLE.lock();
    let caller_uid = table
        .get(&caller_pid)
        .expect("sched_getscheduler: caller process missing from table")
        .shared
        .lock()
        .uid;
    let proc = table
        .get(&target)
        .expect("sched_getscheduler: target process missing from table");
    if !has_sched_permission(caller_uid, proc.shared.lock().uid) {
        return Err(EPERM);
    }
    Ok(proc.sched_policy as u64)
}

/// `SYS_SCHED_GETPARAM`'s real logic -- writes the stored `Process::sched_priority` back into the
/// caller's `struct sched_param`. Real permission checking (`has_sched_permission`), same
/// reasoning `do_sched_setscheduler`'s own doc comment gives.
pub fn do_sched_getparam(caller_pid: Pid, pid: i64, param_ptr: u64) -> Result<u64, u64> {
    let target = resolve_target_pid(caller_pid, pid)?;
    let table = PROCESS_TABLE.lock();
    let caller_uid = table
        .get(&caller_pid)
        .expect("sched_getparam: caller process missing from table")
        .shared
        .lock()
        .uid;
    let proc = table
        .get(&target)
        .expect("sched_getparam: target process missing from table");
    if !has_sched_permission(caller_uid, proc.shared.lock().uid) {
        return Err(EPERM);
    }
    let sched_priority = proc.sched_priority;
    drop(table);
    // SAFETY: same known pointer-validation gap every other user-memory write in this codebase
    // already has.
    unsafe { (param_ptr as *mut RawSchedParam).write(RawSchedParam { sched_priority }) };
    Ok(0)
}

/// `SYS_SCHED_RR_GET_INTERVAL` -- real Linux's own unclaimed `508` (already pre-reserved in
/// `third_party/musl/arch/x86_64/bits/syscall.h.in`, no kernel handler existed until now). Reports
/// this kernel's own real, honest round-robin quantum (`interrupts::PREEMPT_QUANTUM_TICKS` at
/// `TIMER_HZ`, see "Real preemptive scheduling" in CLAUDE.md) rather than a fabricated value --
/// every process shares the same quantum here, so this is a pure function of nothing but
/// `TIMER_HZ`/`PREEMPT_QUANTUM_TICKS`, not `Process` state, once `resolve_target_pid` has confirmed
/// the target is real. `ESRCH` for a nonexistent pid closes `sched_rr_get_interval/3-1.c`; the
/// `pid == 0`-means-self / `pid == getpid()` equivalence `sched_rr_get_interval/1-1.c` checks falls
/// straight out of `resolve_target_pid`'s own existing behavior.
pub fn do_sched_rr_get_interval(caller_pid: Pid, pid: i64, ts_ptr: u64) -> Result<u64, u64> {
    let _target = resolve_target_pid(caller_pid, pid)?;
    let nsec = 1_000_000_000u64 * crate::cpu::interrupts::PREEMPT_QUANTUM_TICKS
        / crate::cpu::pit::TIMER_HZ as u64;
    // SAFETY: same known pointer-validation gap every other user-memory write in this codebase
    // already has.
    unsafe {
        (ts_ptr as *mut RawTimespecForSchedRr).write(RawTimespecForSchedRr {
            tv_sec: 0,
            tv_nsec: nsec as i64,
        })
    };
    Ok(0)
}

/// musl's own `struct timespec` on x86_64 -- duplicated here rather than shared, same "no shared
/// crate across this internal ABI boundary" convention every other `Raw*` wire struct in this
/// codebase already follows.
#[repr(C)]
struct RawTimespecForSchedRr {
    tv_sec: i64,
    tv_nsec: i64,
}

/// `SYS_SCHED_YIELD` -- real Linux's own unclaimed `24` (unremapped in `bits/syscall.h.in`, musl's
/// own `sched_yield()` already calls straight through). This kernel's cooperative round-robin
/// scheduler makes this a genuine, not fabricated, yield: `scheduler::schedule()` is the exact
/// primitive every voluntary block in this codebase already uses to hand off the CPU, just called
/// here with no actual wait condition -- the caller re-enqueues as `Ready` and simply waits its
/// turn again like any other runnable process. Always succeeds, matching real POSIX.
pub fn do_sched_yield() -> Result<u64, u64> {
    crate::process::scheduler::schedule();
    Ok(0)
}

/// `SYS_SCHED_GETAFFINITY`'s real logic, registered directly at real Linux's own `__NR_sched_
/// getaffinity = 204` (no invented number, no musl remap needed -- same "confirmed unassigned in
/// this ABI's own registry, so the real value is safe to use directly" reasoning as `modules/
/// oxfs`'s `SYS_FCHMOD`; see that constant's own doc comment). This kernel is single-core (see
/// CLAUDE.md's own "no SMP" gap), so every process's affinity mask is always just bit 0 set.
///
/// Found live: BusyBox's `nproc` calls this to count set bits, but silently falls back to
/// `count = 1` on any failure -- so `nproc` reported the right answer by coincidence even while
/// this was a flat `ENOSYS`, and the gap only surfaced as a logged `unrecognized syscall number`
/// line during a real boot/test run, not a wrong result. Implemented anyway rather than left as a
/// silently-tolerated `ENOSYS`, since a real `taskset`/`pmap`-style CPU-affinity query landing on
/// `ENOSYS` isn't obviously safe for every future caller the way `nproc`'s specific fallback is.
///
/// The raw syscall's real return value (not the glibc/musl *wrapper*'s 0-or-error convention --
/// see `third_party/musl/src/sched/affinity.c`'s own `do_getaffinity`) is the number of bytes
/// actually written into the caller's mask; musl's wrapper zero-fills the remainder of the
/// caller's buffer itself, so this only ever needs to write `min(cpusetsize, size_of::<u64>())`
/// real bytes and return that count. `EINVAL` for a zero-sized mask, matching real Linux (no mask
/// fits in zero bytes).
pub fn do_sched_getaffinity(
    caller_pid: Pid,
    pid: i64,
    cpusetsize: u64,
    mask_ptr: u64,
) -> Result<u64, u64> {
    let _target = resolve_target_pid(caller_pid, pid)?;
    if cpusetsize == 0 {
        return Err(EINVAL);
    }
    let mask: u64 = 1; // single core -- bit 0 set, every other bit clear
    let to_write = (cpusetsize as usize).min(core::mem::size_of::<u64>());
    let bytes = mask.to_ne_bytes();
    // SAFETY: same known pointer-validation gap every other user-memory write in this codebase
    // already has.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), mask_ptr as *mut u8, to_write);
    }
    Ok(to_write as u64)
}

/// `SYS_FUTEX`, registered directly at real Linux's own `__NR_futex = 202` (no invented number, no
/// musl remap needed -- same "confirmed unassigned in this ABI's own registry" reasoning
/// `do_sched_getaffinity`'s own doc comment above already establishes). Phase 3 of the real
/// threading work behind real POSIX AIO (see `[[project_real_threading_for_aio]]` memory / the
/// prior two sessions' `clone.s`/`__unmapself.s` fix and `Process::tgid` split) -- a genuinely
/// real `FUTEX_WAIT`/`FUTEX_WAKE`, not the earlier honest-failure stub this replaces.
///
/// **Was previously a deliberate stub** (`FUTEX_WAIT` always returned an immediate `ETIMEDOUT`,
/// `FUTEX_WAKE` always a no-op success) to stop a real, silent infinite-busy-loop
/// (`third_party/musl/src/thread/__timedwait.c`'s `sem_timedwait`/`__timedwait_cp` folds any
/// syscall return other than `EINTR`/`ETIMEDOUT`/`ECANCELED` into "spurious wake, try again" and
/// retries with no actual blocking or scheduler yield -- an unregistered/`ENOSYS`'d futex used to
/// hang the whole POSIX conformance pilot). **That stub's own doc comment claimed
/// `sem_wait/7-1.c` was "currently PASS" and load-bearing -- confirmed false by direct trace
/// through the test's own source (`third_party/posixtestsuite/conformance/interfaces/sem_wait/
/// 7-1.c`) and the archived pilot logs (`target/posix-pilot-logs/`): it was, and until this
/// change remained, a real, confirmed `FAIL`** (the test wants a genuinely blocked `sem_wait` in a
/// forked child to observe real `EINTR` after the parent sends `SIGABRT` with a caught handler
/// installed -- an immediate `ETIMEDOUT` makes the child observe the wrong errno and exit
/// `CHILDFAIL`, which this test's own confusingly-inverted parent-side `WEXITSTATUS` check turns
/// into an overall `FAIL`, not the `PASS` the prior doc comment assumed). This correction matters
/// for scoping this change: there was no passing test to protect, only a real, if narrow, avenue
/// to fix a genuinely failing one.
///
/// **Verified safe against every pilot test that can reach this function** before writing it, not
/// just after (`sem_wait/{1,3,5,7,11,12}-1.c`, `sem_timedwait/{1,2,3,4,6,7,9,10,11}-1.c` --
/// `fork/1-1.c` calls `sem_timedwait` too but only after a real `pthread_create`, so it's excluded
/// from the pilot corpus regardless): every test that currently passes either never reaches this
/// function at all (`sem_trywait`'s atomic compare-and-swap fast path never issues a syscall, and
/// several tests only ever exercise that), or reaches it with a deadline real blocking resolves
/// within single-digit real seconds (well inside `t0`'s own 40s per-test alarm) -- see this
/// change's own session notes for the full per-test trace. `sem_wait/7-1.c`/`sem_timedwait/9-1.c`
/// (the direct EINTR-after-signal analogs) and `sem_timedwait/10-1.c` (a real elapsed-wall-time
/// precision check, the same class of fix `nanosleep`'s own sub-second `CLOCK_REALTIME` work
/// already established) are expected to flip `FAIL` -> `PASS`.
///
/// **Real `FUTEX_WAIT`**: atomically compares the real 32-bit word at `addr` against `val` (real
/// futex words are always `int`, not this ABI's usual 64-bit register width -- matches musl's own
/// `sem_t.__val[0]` usage) -- a mismatch is `EAGAIN` immediately, no block, matching real Linux
/// (the caller raced a concurrent poster and should just retry). A real `to` (relative, not
/// absolute -- confirmed via `__timedwait_cp`'s own `to.tv_sec = at->tv_sec - now.tv_sec`
/// conversion before ever calling `__futex4_cp`) converts to an absolute tick deadline the same
/// way `do_nanosleep` already does; a null `to` is `u64::MAX`, the same "no timeout" sentinel
/// `WaitingForMqData`/`WaitingForSemOp` already establish. Blocks via `BlockReason::
/// WaitingForFutex(tgid, addr, deadline)` -- see that variant's own doc comment for why `tgid`,
/// not raw `pid`, is the correctness-required scope. Checks for a deliverable signal before ever
/// blocking (avoiding a lost wakeup, same discipline every blocking primitive here already
/// follows) and once more after waking, in that priority order, then a deadline check -- **and
/// then, if neither, a plain success**: unlike every other blocking primitive in this codebase,
/// this deliberately does **not** loop and re-verify its own condition (the futex word's value)
/// after waking. That's not an oversight -- see `BlockReason::WaitingForFutex`'s own doc comment:
/// real `FUTEX_WAIT` permits genuinely spurious wakeups by spec, and every real caller (musl's own
/// `sem_timedwait`) already re-verifies via its own userspace retry loop before ever trusting a
/// zero return. Re-verifying here too would be redundant, not more correct, and would depart from
/// what a real Linux kernel's own `futex_wait` actually does.
///
/// **Real `FUTEX_WAKE`**: scans `process::table()` for every process genuinely `Blocked` on
/// `WaitingForFutex` with a matching `tgid` (the *waker's* own tgid -- real futex wake, like wait,
/// is scoped to addresses meaningful within the caller's own address space) and the exact same
/// `addr`, flips up to `val` of them back to `Ready` (real Linux's own "max waiters to wake" `val`
/// argument -- `sem_post`'s own call, via `__wake`, passes a small fixed count), and returns the
/// real number actually woken (musl's own `__wake` never checks this return value, so `0` would
/// have been just as honest, but a real count costs nothing extra and is a genuinely correct
/// primitive, not a shortcut).
///
/// Any other real `futex_op` (`FUTEX_CMP_REQUEUE`/`FUTEX_WAKE_OP`/...) still just succeeds -- none
/// of them are reachable from the plain `sem_*`/(excluded) `pthread_*` call sites this ABI's own
/// syscall table can currently be reached from.
pub fn do_futex(pid: Pid, addr: u64, op: u64, val: u64, to: u64) -> Result<u64, u64> {
    const FUTEX_WAIT: u64 = 0;
    const FUTEX_WAKE: u64 = 1;
    const FUTEX_PRIVATE: u64 = 128;
    let base_op = op & !FUTEX_PRIVATE;

    if base_op == FUTEX_WAIT {
        // SAFETY: same known pointer-validation gap every other user-memory read in this codebase
        // already has -- a bad `addr` page-faults, handled safely by the real ring-3
        // fault-to-signal delivery machinery (see CLAUDE.md's "Real ring-3 fault-to-signal
        // delivery" section), not a soundness hole.
        let current = unsafe { *(addr as *const u32) };
        if current != val as u32 {
            return Err(EAGAIN);
        }

        let deadline = if to == 0 {
            u64::MAX
        } else {
            // SAFETY: same gap as above, for `to` instead of `addr`.
            let ts = unsafe { *(to as *const RawTimespec) };
            if ts.tv_sec < 0 || !(0..1_000_000_000).contains(&ts.tv_nsec) {
                return Err(EINVAL);
            }
            let hz = crate::cpu::pit::TIMER_HZ as u64;
            let whole_second_ticks = ts.tv_sec as u64 * hz;
            let sub_second_ticks = (ts.tv_nsec as u64 * hz).div_ceil(1_000_000_000);
            crate::cpu::interrupts::ticks() + whole_second_ticks + sub_second_ticks
        };
        if crate::cpu::interrupts::ticks() >= deadline {
            return Err(ETIMEDOUT);
        }

        {
            let mut table = PROCESS_TABLE.lock();
            let proc = table.get_mut(&pid).unwrap();
            if proc.pending_signals & !proc.blocked_signals != 0 {
                return Err(EINTR);
            }
            let tgid = proc.tgid;
            proc.state = ProcState::Blocked(BlockReason::WaitingForFutex(tgid, addr, deadline));
        } // lock dropped before schedule() -- see process::table()'s own doc comment
        crate::process::scheduler::schedule();

        let table = PROCESS_TABLE.lock();
        let proc = table.get(&pid).unwrap();
        if proc.pending_signals & !proc.blocked_signals != 0 {
            return Err(EINTR);
        }
        if crate::cpu::interrupts::ticks() >= deadline {
            return Err(ETIMEDOUT);
        }
        return Ok(0);
    }

    if base_op == FUTEX_WAKE {
        let waker_tgid = {
            let table = PROCESS_TABLE.lock();
            table.get(&pid).map(|p| p.tgid).unwrap_or(pid)
        };
        return Ok(wake_futex(waker_tgid, addr, val));
    }

    Ok(0)
}

/// The real `FUTEX_WAKE` scan (`do_futex`'s own branch above), factored out so
/// `process::lifecycle::terminate_process`'s own real `CLONE_CHILD_CLEARTID` handling (a genuine
/// kernel-driven futex wake at a real, unmodified musl-issued `clone(2)`'s `ctid` address, at real
/// task-exit time -- see that function's own doc comment) can reuse the exact same primitive
/// rather than duplicating this scan. `tgid`-scoped, same reasoning `BlockReason::WaitingForFutex`'s
/// own doc comment already gives.
pub(crate) fn wake_futex(tgid: Pid, addr: u64, max_waiters: u64) -> u64 {
    let mut woken = 0u64;
    let mut table = PROCESS_TABLE.lock();
    for (&waiter_pid, proc) in table.iter_mut() {
        if woken >= max_waiters {
            break;
        }
        if let ProcState::Blocked(BlockReason::WaitingForFutex(wtgid, waddr, _)) = proc.state
            && wtgid == tgid
            && waddr == addr
        {
            proc.state = ProcState::Ready;
            crate::process::scheduler::enqueue_ready(waiter_pid);
            woken += 1;
        }
    }
    woken
}

/// `SYS_SCHED_GET_PRIORITY_MAX`/`SYS_SCHED_GET_PRIORITY_MIN`'s real logic -- fixed,
/// real-Linux-matching ranges per policy (`sched_priority_range`), not backed by any real
/// scheduling class this kernel implements -- pure functions, no `Process` state involved at all.
/// **Real `EINVAL` for an unrecognized policy** (`is_known_sched_policy`) -- found live:
/// `sched_get_priority_max/2-1.c`/`sched_get_priority_min/2-1.c` both pass `-1` and expect `EINVAL`,
/// previously silently treated the same as `SCHED_OTHER` (returning `0`, a real success) since
/// nothing here ever validated the policy at all.
pub fn do_sched_get_priority_max(policy: i32) -> Result<u64, u64> {
    if !is_known_sched_policy(policy) {
        return Err(EINVAL);
    }
    Ok(sched_priority_range(policy).1 as u64)
}

pub fn do_sched_get_priority_min(policy: i32) -> Result<u64, u64> {
    if !is_known_sched_policy(policy) {
        return Err(EINVAL);
    }
    Ok(sched_priority_range(policy).0 as u64)
}
