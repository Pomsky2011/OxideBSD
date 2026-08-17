//! nanosleep/itimer syscalls -- split out of the original process.rs.



use crate::syscall::{EAGAIN, EINVAL};
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
        // Loops and re-checks the real deadline, the same self-correcting pattern
        // `crate::fs::pipe`/`crate::console::stdin::read` already use -- normally the timer IRQ
        // handler is the only thing that ever sets this process back to `Ready` (only once the
        // real deadline has actually passed), so a single `schedule()` call used to be enough.
        // Real `SIGCONT` resuming a `Stopped` process (see `ProcState::Stopped`'s own doc comment)
        // broke that assumption: it unconditionally flips `state` back to `Ready` regardless of
        // what `BlockReason` it's interrupting -- found live (Ctrl+Z-ing a `sleep`, then `bg`,
        // resumed it far short of its real deadline). Re-blocking on the same deadline if it
        // hasn't actually passed yet also gets real semantics for free: `ticks()` keeps advancing
        // while `Stopped` (it's a global counter, not paused per-process), so time spent stopped
        // still counts toward the sleep, matching real Linux's own wall-clock-deadline behavior.
        while crate::cpu::interrupts::ticks() < deadline {
            {
                let mut table = PROCESS_TABLE.lock();
                table.get_mut(&pid).unwrap().state =
                    ProcState::Blocked(BlockReason::Sleeping(deadline));
            } // lock dropped before schedule() -- see process::table()'s own doc comment
            crate::process::scheduler::schedule();
        }
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

/// Real, architecture-generic `clockid_t` values -- duplicated from `src/syscall/ffi.rs`'s own
/// copy rather than shared across this internal module boundary, same "no shared crate across this
/// internal ABI boundary" convention this file's own `RawTimespec`/`RawItimerval` already follow.
const CLOCK_REALTIME: u64 = 0;
const CLOCK_MONOTONIC: u64 = 1;

/// Real Linux `TIMER_ABSTIME` (`third_party/musl/include/time.h`) -- the one `timer_settime(2)`
/// `flags` bit this ABI understands; any other bit is `EINVAL`.
const TIMER_ABSTIME: u64 = 1;

/// musl's own `struct ksigevent` (`third_party/musl/src/time/timer_create.c`) -- the real,
/// kernel-facing shape `timer_create(2)`'s `evp` argument actually points at (musl's userspace
/// `struct sigevent` is translated into this smaller struct before the syscall for every
/// notification kind reachable here -- see `do_timer_create`'s own doc comment for why
/// `SIGEV_THREAD`/`SIGEV_THREAD_ID` never really reach this kernel in practice). `union sigval` is
/// 8 bytes on this LP64 target (its largest member is a pointer); `sigev_value`/`sigev_tid` are
/// never read (only `sigev_notify`/`sigev_signo` matter here) but stay in the struct for correct
/// field-offset layout matching the real one musl builds.
#[derive(Clone, Copy)]
#[repr(C)]
struct RawKSigevent {
    #[allow(dead_code)]
    sigev_value: u64,
    sigev_signo: i32,
    sigev_notify: i32,
    #[allow(dead_code)]
    sigev_tid: i32,
}

const SIGEV_SIGNAL: i32 = 0;
const SIGEV_NONE: i32 = 1;

/// Real `struct itimerspec` layout (`{ struct timespec it_interval; struct timespec it_value; }`,
/// nanosecond precision) -- distinct from `RawItimerval` above (`setitimer`'s own microsecond-
/// precision `timeval` pair). What `timer_settime(2)`/`timer_gettime(2)` actually read/write.
#[derive(Clone, Copy)]
#[repr(C)]
struct RawItimerspec {
    it_interval_sec: i64,
    it_interval_nsec: i64,
    it_value_sec: i64,
    it_value_nsec: i64,
}

/// `sec`/`nsec` -> ticks, rounded up (never fires *earlier* than requested) -- same rounding
/// discipline `timeval_to_ticks` above already established, just nanosecond- instead of
/// microsecond-precision.
fn timespec_to_ticks(sec: i64, nsec: i64) -> Option<u64> {
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        return None;
    }
    let hz = crate::cpu::pit::TIMER_HZ as u64;
    let whole = sec as u64 * hz;
    let frac = (nsec as u64 * hz).div_ceil(1_000_000_000);
    Some(whole + frac)
}

/// Inverse of `timespec_to_ticks` -- floored, same "never claim more time is left than actually
/// is" reasoning `ticks_to_timeval` above already established.
fn ticks_to_timespec(ticks: u64) -> (i64, i64) {
    let hz = crate::cpu::pit::TIMER_HZ as u64;
    ((ticks / hz) as i64, ((ticks % hz) * 1_000_000_000 / hz) as i64)
}

/// Converts a `TIMER_ABSTIME` `timer_settime` target (a point in `clockid`'s own domain, not a
/// duration) into an absolute `interrupts::ticks()` deadline. `CLOCK_MONOTONIC`'s domain already
/// *is* "seconds since boot at `TIMER_HZ` resolution" -- exactly what `ticks()` counts -- so a
/// requested absolute timestamp converts the same way a relative one would (`timespec_to_ticks`),
/// just used directly as the deadline instead of added to `ticks()`. `CLOCK_REALTIME`'s domain is
/// wall-clock time (`src/cpu/rtc.rs`'s live CMOS read, `tv_nsec` always `0`) -- converted via the
/// delta between the requested wall-clock second and *now*, then applied against the current tick
/// count. A target already in the past (delta `<= 0`) collapses to "fire on the very next timer
/// IRQ" (`now_ticks`) rather than underflowing, matching real semantics for an already-elapsed
/// absolute deadline.
fn abstime_to_ticks(clockid: u64, sec: i64, nsec: i64) -> u64 {
    let hz = crate::cpu::pit::TIMER_HZ as u64;
    let frac_ticks = (nsec as u64 * hz).div_ceil(1_000_000_000);
    let now_ticks = crate::cpu::interrupts::ticks();
    if clockid == CLOCK_MONOTONIC {
        sec.max(0) as u64 * hz + frac_ticks
    } else {
        let now_wall = crate::cpu::rtc::unix_epoch_seconds();
        let delta_secs = sec - now_wall;
        if delta_secs <= 0 {
            now_ticks
        } else {
            now_ticks + delta_secs as u64 * hz + frac_ticks
        }
    }
}

/// `SYS_TIMER_CREATE` (`531`, item 6 of `docs/MISSING_POSIX_SYSCALLS.md`'s own 28-syscall
/// pre-reserved batch) -- matches real `timer_create(2)`'s exact `(clockid, evp_ptr, timerid_ptr)`
/// wire format (musl's own `timer_create()` translates its `struct sigevent` argument into the
/// smaller `struct ksigevent` -- `RawKSigevent` above -- before issuing the raw syscall for every
/// `SIGEV_SIGNAL`/`SIGEV_NONE`/`SIGEV_THREAD_ID` notification kind; `SIGEV_THREAD` is handled
/// entirely in userspace via a real `pthread_create()` first, which always fails on this kernel
/// -- no `clone(2)` -- before ever reaching this syscall, so only `SIGEV_SIGNAL`/`SIGEV_NONE` are
/// real, reachable cases here; a caller bypassing musl's own wrapper to request `SIGEV_THREAD_ID`
/// directly gets a real `EINVAL`, this kernel having no notion of a specific thread to target).
///
/// `evp_ptr == 0` matches real POSIX's own default: `SIGEV_SIGNAL` with `sigev_signo = SIGALRM`.
/// Finds the first free `Process::posix_timers` slot (`EAGAIN` if all `MAX_POSIX_TIMERS` are
/// already in use, matching real Linux's own resource-exhaustion errno for this call) and writes
/// its index back as the real, opaque `timer_t` value.
pub fn do_timer_create(pid: Pid, clockid: u64, evp_ptr: u64, timerid_ptr: u64) -> Result<u64, u64> {
    if clockid != CLOCK_REALTIME && clockid != CLOCK_MONOTONIC {
        return Err(EINVAL);
    }

    let signo = if evp_ptr == 0 {
        SIGALRM
    } else {
        // SAFETY: same known pointer-validation gap every other user-memory read in this codebase
        // already has.
        let evp = unsafe { *(evp_ptr as *const RawKSigevent) };
        match evp.sigev_notify {
            SIGEV_NONE => 0,
            SIGEV_SIGNAL => {
                let sig = evp.sigev_signo as u64;
                if !(1..=31).contains(&sig) {
                    return Err(EINVAL);
                }
                sig
            }
            _ => return Err(EINVAL),
        }
    };

    let mut table = PROCESS_TABLE.lock();
    let proc = table
        .get_mut(&pid)
        .expect("timer_create: current process missing from table");

    let Some(slot) = proc.posix_timers.iter().position(Option::is_none) else {
        return Err(EAGAIN);
    };
    proc.posix_timers[slot] = Some(PosixTimer {
        clockid,
        signo,
        deadline: None,
        interval_ticks: 0,
        overrun: 0,
    });
    drop(table);

    // SAFETY: same known pointer-validation gap as above, for a write this time.
    unsafe { (timerid_ptr as *mut i32).write(slot as i32) };
    Ok(0)
}

/// `SYS_TIMER_SETTIME` (`532`, item 7 of the same batch) -- matches real `timer_settime(2)`'s
/// exact `(timerid, flags, new_ptr, old_ptr)` wire format (musl's own `timer_settime()` reduces to
/// a bare 4-argument `syscall` on this LP64 target -- no `*64`-suffixed sibling exists for
/// `time_t` this size, see `third_party/musl/arch/x86_64/bits/syscall.h.in`'s own comment on the
/// batch). `timerid` doubles as `Process::posix_timers`'s own index; an out-of-range or currently-
/// unallocated one is `EINVAL`, matching real Linux.
///
/// Setting `it_value` to all-zero disarms the timer regardless of `TIMER_ABSTIME` -- real
/// `timer_settime(2)` semantics, checked before either the relative or absolute interpretation
/// below. Otherwise: relative mode (`flags == 0`) arms `ticks() + value_ticks`, same shape
/// `do_setitimer` already uses; `TIMER_ABSTIME` mode resolves the absolute target via
/// `abstime_to_ticks`, using the `clockid` this timer was created against. Rearming always clears
/// `overrun` -- a stale overrun count from before this call has no real meaning against a freshly
/// (re)armed timer.
pub fn do_timer_settime(
    pid: Pid,
    timerid: u64,
    flags: u64,
    new_ptr: u64,
    old_ptr: u64,
) -> Result<u64, u64> {
    if flags & !TIMER_ABSTIME != 0 {
        return Err(EINVAL);
    }
    if new_ptr == 0 {
        return Err(EINVAL);
    }
    let timerid = timerid as usize;

    // SAFETY: same known pointer-validation gap every other user-memory read in this codebase
    // already has.
    let new = unsafe { *(new_ptr as *const RawItimerspec) };
    let value_ticks = timespec_to_ticks(new.it_value_sec, new.it_value_nsec).ok_or(EINVAL)?;
    let interval_ticks =
        timespec_to_ticks(new.it_interval_sec, new.it_interval_nsec).ok_or(EINVAL)?;

    let mut table = PROCESS_TABLE.lock();
    let proc = table
        .get_mut(&pid)
        .expect("timer_settime: current process missing from table");
    let clockid = proc
        .posix_timers
        .get(timerid)
        .and_then(|s| s.as_ref())
        .ok_or(EINVAL)?
        .clockid;

    if old_ptr != 0 {
        let slot = proc.posix_timers[timerid].as_ref().unwrap();
        let (value_sec, value_nsec) = match slot.deadline {
            Some(deadline) => {
                ticks_to_timespec(deadline.saturating_sub(crate::cpu::interrupts::ticks()))
            }
            None => (0, 0),
        };
        let (interval_sec, interval_nsec) = ticks_to_timespec(slot.interval_ticks);
        // SAFETY: same known pointer-validation gap as above, for a write this time.
        unsafe {
            (old_ptr as *mut RawItimerspec).write(RawItimerspec {
                it_interval_sec: interval_sec,
                it_interval_nsec: interval_nsec,
                it_value_sec: value_sec,
                it_value_nsec: value_nsec,
            })
        };
    }

    let slot = proc.posix_timers[timerid].as_mut().unwrap();
    if new.it_value_sec == 0 && new.it_value_nsec == 0 {
        slot.deadline = None;
        slot.interval_ticks = 0;
    } else if flags & TIMER_ABSTIME != 0 {
        slot.deadline = Some(abstime_to_ticks(clockid, new.it_value_sec, new.it_value_nsec));
        slot.interval_ticks = interval_ticks;
    } else {
        slot.deadline = Some(crate::cpu::interrupts::ticks() + value_ticks);
        slot.interval_ticks = interval_ticks;
    }
    slot.overrun = 0;
    Ok(0)
}

/// `SYS_TIMER_GETTIME` (`533`, item 8) -- matches real `timer_gettime(2)`'s exact
/// `(timerid, val_ptr)` wire format. Same `ok_or(EINVAL)` invalid-id handling as
/// `do_timer_settime`.
pub fn do_timer_gettime(pid: Pid, timerid: u64, val_ptr: u64) -> Result<u64, u64> {
    if val_ptr == 0 {
        return Err(EINVAL);
    }
    let timerid = timerid as usize;
    let table = PROCESS_TABLE.lock();
    let proc = table
        .get(&pid)
        .expect("timer_gettime: current process missing from table");
    let slot = proc
        .posix_timers
        .get(timerid)
        .and_then(|s| s.as_ref())
        .ok_or(EINVAL)?;

    let (value_sec, value_nsec) = match slot.deadline {
        Some(deadline) => {
            ticks_to_timespec(deadline.saturating_sub(crate::cpu::interrupts::ticks()))
        }
        None => (0, 0),
    };
    let (interval_sec, interval_nsec) = ticks_to_timespec(slot.interval_ticks);
    // SAFETY: same known pointer-validation gap every other user-memory write in this codebase
    // already has.
    unsafe {
        (val_ptr as *mut RawItimerspec).write(RawItimerspec {
            it_interval_sec: interval_sec,
            it_interval_nsec: interval_nsec,
            it_value_sec: value_sec,
            it_value_nsec: value_nsec,
        })
    };
    Ok(0)
}

/// `SYS_TIMER_GETOVERRUN` (`534`, item 9) -- matches real `timer_getoverrun(2)`'s exact
/// single-argument `(timerid)` wire format; unlike every other timer call here, the overrun count
/// itself *is* the real return value (no output pointer) -- see `PosixTimer::overrun`'s own doc
/// comment for what this counts and its one known, accepted simplification.
pub fn do_timer_getoverrun(pid: Pid, timerid: u64) -> Result<u64, u64> {
    let timerid = timerid as usize;
    let table = PROCESS_TABLE.lock();
    let proc = table
        .get(&pid)
        .expect("timer_getoverrun: current process missing from table");
    let slot = proc
        .posix_timers
        .get(timerid)
        .and_then(|s| s.as_ref())
        .ok_or(EINVAL)?;
    Ok(slot.overrun as u64)
}

/// `SYS_TIMER_DELETE` (`535`, item 10, the last of the POSIX-timer sub-batch) -- matches real
/// `timer_delete(2)`'s exact single-argument `(timerid)` wire format. Just frees the slot (`None`)
/// -- no dealloc beyond that to do, consistent with the rest of this kernel's "no deallocation
/// anywhere" stance for every other resource.
pub fn do_timer_delete(pid: Pid, timerid: u64) -> Result<u64, u64> {
    let timerid = timerid as usize;
    let mut table = PROCESS_TABLE.lock();
    let proc = table
        .get_mut(&pid)
        .expect("timer_delete: current process missing from table");
    let slot = proc.posix_timers.get_mut(timerid).ok_or(EINVAL)?;
    if slot.is_none() {
        return Err(EINVAL);
    }
    *slot = None;
    Ok(0)
}
