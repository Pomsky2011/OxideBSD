//! rlimit/priority/sched-policy syscalls -- split out of the original process.rs.



use crate::syscall::EINVAL;
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
const SCHED_FIFO: i32 = 1;
const SCHED_RR: i32 = 2;

/// Real `struct sched_param` on x86_64 -- a single `int sched_priority` field (real Linux's own
/// layout has no other members on this arch).
#[derive(Clone, Copy)]
#[repr(C)]
struct RawSchedParam {
    sched_priority: i32,
}

/// `SYS_SCHED_SETSCHEDULER`'s real logic -- stores `policy`/`param.sched_priority` verbatim, no
/// validation beyond what `param_ptr` itself provides (real Linux would range-check `sched_priority`
/// against `policy`'s own `sched_get_priority_min/max`, but nothing in this port's roster depends
/// on that rejection path firing). See `Process::sched_policy`'s own doc comment for why storing
/// this has no real RT-scheduling effect on this kernel's cooperative round-robin scheduler.
pub fn do_sched_setscheduler(
    caller_pid: Pid,
    pid: i64,
    policy: i32,
    param_ptr: u64,
) -> Result<u64, u64> {
    let target = resolve_target_pid(caller_pid, pid)?;
    // SAFETY: same known pointer-validation gap every other user-memory read in this codebase
    // already has.
    let param = unsafe { *(param_ptr as *const RawSchedParam) };
    let mut table = PROCESS_TABLE.lock();
    let proc = table
        .get_mut(&target)
        .expect("sched_setscheduler: target process missing from table");
    proc.sched_policy = policy;
    proc.sched_priority = param.sched_priority;
    Ok(0)
}

/// `SYS_SCHED_GETSCHEDULER`'s real logic -- echoes back the stored `Process::sched_policy`.
pub fn do_sched_getscheduler(caller_pid: Pid, pid: i64) -> Result<u64, u64> {
    let target = resolve_target_pid(caller_pid, pid)?;
    let table = PROCESS_TABLE.lock();
    Ok(table
        .get(&target)
        .expect("sched_getscheduler: target process missing from table")
        .sched_policy as u64)
}

/// `SYS_SCHED_GETPARAM`'s real logic -- writes the stored `Process::sched_priority` back into the
/// caller's `struct sched_param`.
pub fn do_sched_getparam(caller_pid: Pid, pid: i64, param_ptr: u64) -> Result<u64, u64> {
    let target = resolve_target_pid(caller_pid, pid)?;
    let table = PROCESS_TABLE.lock();
    let sched_priority = table
        .get(&target)
        .expect("sched_getparam: target process missing from table")
        .sched_priority;
    // SAFETY: same known pointer-validation gap every other user-memory write in this codebase
    // already has.
    unsafe { (param_ptr as *mut RawSchedParam).write(RawSchedParam { sched_priority }) };
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

/// `SYS_SCHED_GET_PRIORITY_MAX`/`SYS_SCHED_GET_PRIORITY_MIN`'s real logic -- fixed,
/// real-Linux-matching ranges per policy (`SCHED_FIFO`/`SCHED_RR` -> `1..=99`, everything else
/// (`SCHED_OTHER`/`SCHED_BATCH`/`SCHED_IDLE`/...) -> `0..=0`), not backed by any real scheduling
/// class this kernel implements -- pure functions, no `Process` state involved at all.
pub fn do_sched_get_priority_max(policy: i32) -> Result<u64, u64> {
    Ok(if policy == SCHED_FIFO || policy == SCHED_RR {
        99
    } else {
        0
    })
}

pub fn do_sched_get_priority_min(policy: i32) -> Result<u64, u64> {
    Ok(if policy == SCHED_FIFO || policy == SCHED_RR {
        1
    } else {
        0
    })
}
