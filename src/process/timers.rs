//! nanosleep/itimer syscalls -- split out of the original process.rs.



use crate::syscall::EINVAL;
use super::*;

/// musl's own `struct timespec` on x86_64 -- see `src/syscall.rs`'s `RawTimespec` (duplicated
/// here rather than shared, same "no shared crate across this internal ABI boundary" convention
/// this file's own `SYS_OPEN`/`SYS_READ`/`SYS_CLOSE` constants above already follow).
#[derive(Clone, Copy)]
#[repr(C)]
struct RawTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

/// `SYS_NANOSLEEP` — matches real `nanosleep(2)`'s exact `(req_ptr, rem_ptr)` wire format (musl's
/// own `nanosleep()`, calling through `__clock_nanosleep(CLOCK_REALTIME, 0, req, rem)`, reduces to
/// exactly this shape whenever `flags == 0`, the common case), so only the number needed
/// remapping.
///
/// Converts the requested duration into an absolute wake-up tick deadline (`src/pit.rs`'s
/// `TIMER_HZ` ground truth, the same one `src/syscall.rs`'s `sys_clock_gettime` already uses for
/// `CLOCK_MONOTONIC`), blocks the caller (`BlockReason::Sleeping`), and lets
/// `interrupts::timer_interrupt_handler` do the actual waking once that many ticks have passed —
/// the same block-then-`scheduler::schedule()` shape `WaitingForPipeData`/`WaitingForStdin`
/// already established, just woken by a timer IRQ instead of another process's syscall. Rounds the
/// requested duration *up* to a whole tick (never sleeps for less than asked, consistent with real
/// `nanosleep`'s own "at least this long" contract) — a `{0, 0}` request returns immediately
/// without ever blocking at all.
///
/// `rem_ptr` (if non-null) is always zeroed on return — this kernel has no signal-delivery-during-
/// sleep interruption path yet (a real early wake with a meaningful "how much time was left"), so
/// a sleep always either runs to its full requested duration or (via `SIGKILL`'s own immediate,
/// no-handler termination path) never returns at all.
pub fn do_nanosleep(pid: Pid, req_ptr: u64, rem_ptr: u64) -> Result<u64, u64> {
    // SAFETY: same known pointer-validation gap every other user-memory read in this codebase
    // already has -- req_ptr isn't checked against the caller's actual mappings first.
    let req = unsafe { *(req_ptr as *const RawTimespec) };
    if req.tv_sec < 0 || !(0..1_000_000_000).contains(&req.tv_nsec) {
        return Err(EINVAL);
    }

    let hz = crate::cpu::pit::TIMER_HZ as u64;
    let whole_second_ticks = req.tv_sec as u64 * hz;
    let sub_second_ticks = (req.tv_nsec as u64 * hz).div_ceil(1_000_000_000);
    let requested_ticks = whole_second_ticks + sub_second_ticks;

    if requested_ticks > 0 {
        let deadline = crate::cpu::interrupts::ticks() + requested_ticks;
        {
            let mut table = PROCESS_TABLE.lock();
            table.get_mut(&pid).unwrap().state =
                ProcState::Blocked(BlockReason::Sleeping(deadline));
        } // lock dropped before schedule() -- see process::table()'s own doc comment
        crate::process::scheduler::schedule();
    }

    if rem_ptr != 0 {
        // SAFETY: same known pointer-validation gap as above, for a write this time.
        unsafe {
            *(rem_ptr as *mut RawTimespec) = RawTimespec {
                tv_sec: 0,
                tv_nsec: 0,
            }
        };
    }
    Ok(0)
}

/// Real `struct itimerval` layout (`{ struct timeval it_interval; struct timeval it_value; }`,
/// each `timeval` a `{ i64 tv_sec; i64 tv_usec; }` pair on this LP64 target — no padding, matching
/// what musl's own `setitimer()`/`getitimer()` (`third_party/musl/src/signal/{set,get}itimer.c`)
/// read/write directly, since `sizeof(time_t) > sizeof(long)` is false here and both fall straight
/// through to a plain 3-register syscall with no repacking).
#[derive(Clone, Copy)]
#[repr(C)]
struct RawItimerval {
    it_interval_sec: i64,
    it_interval_usec: i64,
    it_value_sec: i64,
    it_value_usec: i64,
}

const ITIMER_REAL: u64 = 0;

/// `sec`/`usec` -> ticks, rounded up (never fires *earlier* than requested) — same rounding
/// discipline `do_nanosleep` already established for its own `TIMER_HZ` conversion.
fn timeval_to_ticks(sec: i64, usec: i64) -> Option<u64> {
    if sec < 0 || !(0..1_000_000).contains(&usec) {
        return None;
    }
    let hz = crate::cpu::pit::TIMER_HZ as u64;
    let whole = sec as u64 * hz;
    let frac = (usec as u64 * hz).div_ceil(1_000_000);
    Some(whole + frac)
}

/// Inverse of `timeval_to_ticks` — floored, since this reports *remaining* time (rounding up here
/// would claim more time is left than actually is).
fn ticks_to_timeval(ticks: u64) -> (i64, i64) {
    let hz = crate::cpu::pit::TIMER_HZ as u64;
    ((ticks / hz) as i64, ((ticks % hz) * 1_000_000 / hz) as i64)
}

/// `SYS_SETITIMER`'s real logic — real `setitimer(2)`'s exact `(which, new_ptr, old_ptr)` wire
/// format (musl's own `setitimer()` passes this straight through unmodified on a 64-bit `time_t`
/// target, and `alarm()` is itself just a thin wrapper around `setitimer(ITIMER_REAL, ...)` — see
/// `third_party/musl/src/unistd/alarm.c` — so this one syscall backs both real libc entry points).
///
/// Only `ITIMER_REAL` is supported (`EINVAL` otherwise) — `ITIMER_VIRTUAL`/`ITIMER_PROF` are
/// defined in terms of a process's own CPU time, which this kernel doesn't track at all (see
/// CLAUDE.md's real-time-clock section: "no per-process/per-thread CPU time is tracked").
///
/// A zero `it_value` (both fields `0`) disarms any pending timer, matching real `setitimer`'s own
/// "cancel" convention. `it_interval` (if the resulting deadline is ever reached and is nonzero)
/// reloads the deadline instead of leaving it disarmed — real periodic-timer semantics, checked by
/// `interrupts::timer_interrupt_handler` alongside its `BlockReason::Sleeping` scan, which raises
/// `SIGALRM` the same simple way `do_kill`'s self-targeting case already does (just sets the
/// pending bit — see that function's own doc comment for why that's sufficient here: unlike an
/// arbitrary cross-process `kill`, delivery only needs the *next* syscall this exact process makes
/// to complete, and `ping`'s own real usage pattern — a tight loop of individually non-blocking
/// `recvfrom` calls — guarantees that happens promptly).
pub fn do_setitimer(pid: Pid, which: u64, new_ptr: u64, old_ptr: u64) -> Result<u64, u64> {
    if which != ITIMER_REAL {
        return Err(EINVAL);
    }
    if new_ptr == 0 {
        return Err(EINVAL);
    }
    // SAFETY: same known pointer-validation gap every other user-memory read in this codebase
    // already has.
    let new = unsafe { *(new_ptr as *const RawItimerval) };
    let value_ticks = timeval_to_ticks(new.it_value_sec, new.it_value_usec).ok_or(EINVAL)?;
    let interval_ticks =
        timeval_to_ticks(new.it_interval_sec, new.it_interval_usec).ok_or(EINVAL)?;

    let mut table = PROCESS_TABLE.lock();
    let proc = table
        .get_mut(&pid)
        .expect("setitimer: current process missing from table");

    if old_ptr != 0 {
        let (value_sec, value_usec) = match proc.real_timer_deadline {
            Some(deadline) => ticks_to_timeval(deadline.saturating_sub(crate::cpu::interrupts::ticks())),
            None => (0, 0),
        };
        let (interval_sec, interval_usec) = ticks_to_timeval(proc.real_timer_interval_ticks);
        // SAFETY: same known pointer-validation gap as above, for a write this time.
        unsafe {
            (old_ptr as *mut RawItimerval).write(RawItimerval {
                it_interval_sec: interval_sec,
                it_interval_usec: interval_usec,
                it_value_sec: value_sec,
                it_value_usec: value_usec,
            })
        };
    }

    if value_ticks == 0 {
        proc.real_timer_deadline = None;
        proc.real_timer_interval_ticks = 0;
    } else {
        proc.real_timer_deadline = Some(crate::cpu::interrupts::ticks() + value_ticks);
        proc.real_timer_interval_ticks = interval_ticks;
    }
    Ok(0)
}

/// `SYS_GETITIMER`'s real logic — real `getitimer(2)`'s exact `(which, old_ptr)` wire format. Same
/// `ITIMER_REAL`-only restriction as `do_setitimer`.
pub fn do_getitimer(pid: Pid, which: u64, old_ptr: u64) -> Result<u64, u64> {
    if which != ITIMER_REAL {
        return Err(EINVAL);
    }
    if old_ptr == 0 {
        return Err(EINVAL);
    }
    let table = PROCESS_TABLE.lock();
    let proc = table
        .get(&pid)
        .expect("getitimer: current process missing from table");

    let (value_sec, value_usec) = match proc.real_timer_deadline {
        Some(deadline) => ticks_to_timeval(deadline.saturating_sub(crate::cpu::interrupts::ticks())),
        None => (0, 0),
    };
    let (interval_sec, interval_usec) = ticks_to_timeval(proc.real_timer_interval_ticks);
    // SAFETY: same known pointer-validation gap every other user-memory write in this codebase
    // already has.
    unsafe {
        (old_ptr as *mut RawItimerval).write(RawItimerval {
            it_interval_sec: interval_sec,
            it_interval_usec: interval_usec,
            it_value_sec: value_sec,
            it_value_usec: value_usec,
        })
    };
    Ok(0)
}
