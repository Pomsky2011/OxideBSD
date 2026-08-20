//! rlimit/priority/sched-policy syscalls -- split out of the original process.rs.



use crate::syscall::{EINVAL, EPERM};
use super::*;

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
    let mut table = PROCESS_TABLE.lock();
    let proc = table
        .get_mut(&caller_pid)
        .expect("umask: current process missing from table");
    let old_mask = proc.umask;
    proc.umask = new_mask & 0o777;
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
        .uid;
    let proc = table
        .get_mut(&target)
        .expect("sched_setscheduler: target process missing from table");
    if !has_sched_permission(caller_uid, proc.uid) {
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
        .uid;
    let proc = table
        .get(&target)
        .expect("sched_getscheduler: target process missing from table");
    if !has_sched_permission(caller_uid, proc.uid) {
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
        .uid;
    let proc = table
        .get(&target)
        .expect("sched_getparam: target process missing from table");
    if !has_sched_permission(caller_uid, proc.uid) {
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
/// `do_sched_getaffinity`'s own doc comment above already establishes). **Not real futex support**
/// -- real futex semantics need genuine per-address cross-process wait queues, tied to this
/// kernel's still-unstarted real-threading work (see `docs/POSIX_COMPLIANCE_CHECKLIST.md`'s own
/// "Real threading" foundational blocker) -- this is a minimal, honest *failure* stub, in the same
/// spirit as `process::mm::do_mprotect`'s own permissive no-op: it exists only to stop a real,
/// silent infinite-busy-loop this kernel could otherwise trigger in any real caller.
///
/// **Found live, not preemptively**: the expanded POSIX conformance pilot's own `sem_wait/7-1.c`
/// (a real, single-process-reachable POSIX *unnamed* semaphore block -- distinct from the SysV
/// `semop`-backed semaphores this kernel already implements for real, see
/// `fs::sysv_sem`'s own module doc comment) hung the whole pilot run indefinitely, consuming 100%
/// CPU the entire time. Root cause, traced through `third_party/musl/src/thread/__timedwait.c`:
/// `sem_timedwait` calls `__timedwait_cp`, which issues exactly one raw `futex(FUTEX_WAIT, ...)`
/// syscall and only treats the result as a real failure if it's `EINTR`/`ETIMEDOUT`/`ECANCELED` --
/// any *other* value (including the `ENOSYS` this syscall number previously fell through to,
/// unregistered) is silently folded into `0` ("spurious wake, try again"). `sem_timedwait`'s own
/// outer loop then immediately retries `sem_trywait` -> fails again (nothing ever posts the
/// semaphore) -> calls `__timedwait_cp` again -- a real, unbounded, tight retry loop with no actual
/// blocking syscall or scheduler yield anywhere in it, invisible to `t0`'s own `alarm(40)` timeout
/// mechanism only because the *forked child* where this actually happens has no timer of its own
/// (`fork` never inherits a pending itimer/alarm -- real POSIX, and this kernel's own documented
/// behavior) and the *parent*'s own blocking `wait4()` has this exact same "no signal-interrupt
/// wake" gap `signals::wake_if_mq_waiting`'s own doc comment already documents fixing for `mq_*` --
/// a real, separate, not-yet-fixed instance of that same bug class, left open here (no live pilot
/// test currently depends on it, unlike the `mq_receive` case).
///
/// Returning `ETIMEDOUT` for `FUTEX_WAIT` (real Linux's own valid, unremarkable return value for a
/// timed-out wait) is what makes `__timedwait_cp` treat this as a genuine, real failure instead of
/// silently retrying forever -- `sem_timedwait` then correctly reports `errno = ETIMEDOUT` and
/// returns `-1`, a clean, real, if not spec-accurate, outcome instead of an unbounded spin.
/// `FUTEX_WAKE` (`sem_post`'s own call, via `__wake`) always succeeds -- its return value is never
/// checked by any real caller in this musl fork, so `0` is exactly as honest as any other answer.
/// Any other real `futex_op` (`FUTEX_CMP_REQUEUE`/`FUTEX_WAKE_OP`/...) also just succeeds -- none of
/// them are reachable from the plain `sem_*`/(excluded) `pthread_*` call sites this ABI's own
/// syscall table can currently be reached from.
pub fn do_futex(op: u64) -> Result<u64, u64> {
    const FUTEX_WAIT: u64 = 0;
    const FUTEX_PRIVATE: u64 = 128;
    if op & !FUTEX_PRIVATE == FUTEX_WAIT {
        return Err(crate::syscall::ETIMEDOUT);
    }
    Ok(0)
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
