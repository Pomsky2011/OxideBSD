//! Signal syscalls (kill/sigaction/sigprocmask) and delivery bookkeeping -- split out of the original process.rs.

use alloc::vec::Vec;


use crate::syscall::{EAGAIN, EINTR, EINVAL, EPERM, ESRCH, SyscallFrame};
use super::*;

/// Real POSIX `kill(2)`/`sigqueue(2)` permission rule: the sender must either be root, or its own
/// uid must match the target's -- "the real or effective user ID of the sending process shall
/// match the real or saved set-user-ID of the receiving process, unless the sending process has
/// appropriate privileges" (POSIX). This kernel has no separate real/effective/saved uid triple
/// (see `Process::uid`'s own doc comment: no setuid-bit `execve` support to ever make them
/// diverge), so a single equality check against the target's one `uid` field covers the whole real
/// rule. Checked by both `do_kill`/`do_sigqueue`'s single-target cross-process paths -- **not**
/// `signal_foreground_group`'s own process-group broadcast (no pilot test exercises group-kill
/// permission semantics, and real POSIX's own per-member-partial-success rule there is
/// meaningfully more complex than a flat allow/deny).
fn has_signal_permission(caller_uid: u32, target_uid: u32) -> bool {
    caller_uid == 0 || caller_uid == target_uid
}

/// `SYS_KILL`'s real logic. Signals `1..=31` (standard) or `SIGRTMIN..=SIGRTMAX` (real-time) only;
/// anything else is `EINVAL`, matching real `kill()`'s own validation. Real permission checking now
/// exists (`has_signal_permission`) for the single-target case -- `target_pid == 0`/`< 0` are
/// real POSIX process-group broadcasts
/// (`0` = the caller's own group, `< 0` = group `|target_pid|`) — see the `target_pid <= 0` branch
/// below for that path; everything past this doc comment's remaining bullets describes the
/// positive, single-target case.
///
/// Sending to *self* just sets the pending bit and returns — actual delivery happens naturally at
/// this exact syscall's own dispatch tail (`src/syscall.rs`'s `deliver_pending_signal`), since the
/// caller is, by definition, the currently running process.
///
/// Sending to a *different* process (never the currently running one — only one process is ever
/// `Running` at a time here) is where this gets a real, documented simplification: this kernel has
/// no way to force an arbitrary, not-currently-scheduled process to notice a new signal the
/// instant it arrives (no `EINTR`, no forced wakeup of a blocked `wait4`/pipe-read/stdin-read).
/// - If the target's *current* disposition for this signal is `Ignore`, it's discarded right now
///   — matches real semantics (a signal ignored at delivery time is simply dropped).
/// - If the target's disposition is the default `Terminate` (no custom handler installed), it's
///   terminated *immediately*, right here — this is the case that actually matters for real use
///   (`kill`-ing a runaway process like `yes.elf`, which installs no handler at all), and doesn't
///   need the target to ever be scheduled again to take effect.
/// - If the target has a custom handler installed, the pending bit is set and delivery is
///   deferred until the target is next naturally scheduled and completes a syscall of its own (or,
///   if currently blocked, until whatever it's blocked on resolves on its own) — a real, narrower
///   gap than the `Terminate` case above, since a process sitting in a long/indefinite block with
///   a handler installed won't see the signal promptly. Acceptable for this pass: the common,
///   high-value case (killing something with no handler) works correctly and immediately.
/// - Real `SIGSTOP`/default-disposition `SIGTSTP` are also *immediate* (uncatchable for `SIGSTOP`
///   -- `sys_sigaction` already rejects installing a handler for it -- and job-control-critical for
///   `SIGTSTP`, matching real Ctrl+Z): the target's `state` flips straight to `ProcState::Stopped`
///   right here, the same "doesn't need the target to ever be scheduled again" property `Terminate`
///   has. `SIGCONT` gets its own pre-dispatch step *before* any of the above: an actually-`Stopped`
///   target always resumes regardless of its own `SIGCONT` disposition, real POSIX semantics.
pub fn do_kill(caller_pid: Pid, target_pid: i64, sig: i64) -> Result<u64, u64> {
    if !((0..=31).contains(&sig) || (SIGRTMIN as i64..=SIGRTMAX as i64).contains(&sig)) {
        return Err(EINVAL);
    }
    let caller_uid = oxidebsd_current_uid() as u32;

    if target_pid <= 0 {
        // Real POSIX process-group broadcast: `0` targets the caller's own group, `< 0` targets
        // group `|target_pid|` -- both real `kill(2)` shapes, not this ABI's own invention. Found
        // live, not preemptively added: hush's own `fg`/`bg` builtins (`kill(-pgrp, SIGCONT)`) and
        // its job-cleanup path (`kill(-pgrp, SIGHUP)`/`kill(-pgrp, SIGCONT)`) both depend on this
        // -- previously unreachable dead code until real job control (`process::spawn`'s
        // controlling-tty auto-claim, see CLAUDE.md's session/controlling-tty notes) made hush's
        // own job-control startup actually activate for the first time. Reuses
        // `signal_foreground_group`'s exact per-process Discard/Terminate/Stop/SetPending
        // resolution unchanged -- it already just takes a plain `pgid`, "foreground" was never
        // load-bearing to its own logic, only to its one existing caller.
        let pgid = if target_pid == 0 {
            let table = PROCESS_TABLE.lock();
            table
                .get(&caller_pid)
                .expect("kill: current process missing from table")
                .pgid
        } else {
            target_pid.checked_neg().ok_or(EINVAL)? as u64
        };
        let has_target = {
            let table = PROCESS_TABLE.lock();
            table
                .values()
                .any(|p| p.pgid == pgid && !matches!(p.state, ProcState::Zombie(_)))
        };
        if !has_target {
            return Err(ESRCH);
        }
        // sig == 0: real kill(pgrp, 0) is an existence-only check, same convention as the
        // single-target sig == 0 case below -- has_target above already established that.
        if sig != 0 {
            signal_foreground_group(pgid, sig as u64);
        }
        return Ok(0);
    }
    let target = target_pid as u64;

    // Real kill(pid, 0): no signal is actually sent -- only existence and permission are checked.
    // The standard POSIX idiom for "is this pid still alive" (`kill -0 $pid`) depends on this, and
    // would otherwise always fail EINVAL since 0 isn't a real signal number.
    if sig == 0 {
        if target == caller_pid {
            return Ok(0);
        }
        let table = PROCESS_TABLE.lock();
        return match table.get(&target) {
            Some(proc) if has_signal_permission(caller_uid, proc.shared.lock().uid) => Ok(0), // zombie counts too -- still "exists" until reaped
            Some(_) => Err(EPERM),
            None => Err(ESRCH),
        };
    }
    let sig = sig as u64;

    if target == caller_pid {
        let mut table = PROCESS_TABLE.lock();
        let me = table
            .get_mut(&caller_pid)
            .expect("kill: current process missing from table");
        // Real kill(2) has no path to report a queue-full EAGAIN back to a caller sending itself a
        // signal -- see record_pending's own doc comment.
        let _ = record_pending(me, sig, SI_USER, caller_pid, caller_uid, 0);
        return Ok(0);
    }

    enum Action {
        Discard,
        Terminate,
        Stop,
        SetPending,
    }

    let action = {
        // Scoped so this lock is dropped before terminate_process/the Stop/SetPending branches
        // below re-lock the table -- spin::Mutex isn't reentrant (same discipline table()'s own
        // doc comment already establishes for every other function here).
        let mut table = PROCESS_TABLE.lock();
        let proc = table.get(&target).ok_or(ESRCH)?;
        if !has_signal_permission(caller_uid, proc.shared.lock().uid) {
            return Err(EPERM);
        }
        if matches!(proc.state, ProcState::Zombie(_)) {
            return Ok(0); // still "exists" until reaped, but there's nothing left to signal
        }
        // Real SIGCONT semantics: always resumes an actually-stopped target regardless of its own
        // disposition for SIGCONT (SIG_IGN/a caught handler still get to additionally fire below,
        // via the normal dispatch this falls through to). See ProcState::Stopped's own doc comment
        // for why a plain state flip + re-enqueue is sufficient here.
        if sig == SIGCONT && matches!(proc.state, ProcState::Stopped(_)) {
            let proc = table.get_mut(&target).unwrap();
            proc.state = ProcState::Ready;
            proc.cont_notify_pending = true;
            proc.stop_notify_pending = false; // see Process::stop_notify_pending's own doc comment
            proc.pending_signals &= !((1 << (SIGSTOP - 1)) | (1 << (SIGTSTP - 1)));
            scheduler::enqueue_ready(target);
        }
        let proc = table.get(&target).unwrap();
        match proc.sigactions[sig as usize].handler {
            1 => Action::Discard, // SIG_IGN
            0 => match default_disposition(sig) {
                DefaultDisposition::Ignore => Action::Discard,
                DefaultDisposition::Terminate => Action::Terminate,
                DefaultDisposition::Stop => Action::Stop,
            },
            _ => Action::SetPending,
        }
    };

    match action {
        Action::Discard => {}
        Action::Terminate => terminate_process(target, 128 + sig as i32),
        Action::Stop => {
            let mut table = PROCESS_TABLE.lock();
            if let Some(proc) = table.get_mut(&target) {
                // Real SIGSTOP is uncatchable and never reaches Action::Stop with a target that
                // isn't Blocked/Ready -- Zombie already returned above, and Running is never a
                // valid cross-process stop target (only the caller can be Running). A Ready target
                // must also be dequeued, or the scheduler would still pop and run it next turn
                // regardless of this state flip.
                if proc.state == ProcState::Ready {
                    scheduler::remove_ready(target);
                }
                proc.state = ProcState::Stopped(sig);
                proc.stop_notify_pending = true;
                proc.pending_signals &= !(1 << (SIGCONT - 1));
                wake_parent_if_waiting(&mut table, target);
            }
        }
        Action::SetPending => {
            let mut table = PROCESS_TABLE.lock();
            if let Some(proc) = table.get_mut(&target) {
                let _ = record_pending(proc, sig, SI_USER, caller_pid, caller_uid, 0);
                wake_if_paused(target, proc, sig);
                wake_if_sigwaiting(target, proc, sig);
                wake_if_sleeping(target, proc, sig);
                wake_if_mq_waiting(target, proc, sig);
                wake_if_futex_waiting(target, proc, sig);
            }
        }
    }
    Ok(0)
}

/// Delivers `sig` to every live (non-zombie) process whose `pgid` equals `pgid` — the real tty
/// "INTR/SUSP character signals the whole foreground process group" contract, driven by
/// `interrupts::keyboard_interrupt_handler`'s own Ctrl+C/Ctrl+Z handling once a real foreground
/// group has actually been claimed (`TIOCSCTTY`/`TIOCSPGRP` — see CLAUDE.md's session/
/// controlling-tty notes). Reuses the exact same Discard/Terminate/Stop/SetPending logic `do_kill`'s
/// own cross-process branch already established per target (including its own documented gap: an
/// installed-handler target only gets a deferred pending bit, not forced-immediate delivery) —
/// just applied to every matching pid instead of one. Two passes (collect targets+actions under one
/// lock, then act) for the same reason `do_kill` drops its own read lock before
/// `terminate_process`/re-locking for `Stop`/`SetPending`: those re-lock the table themselves, and
/// `spin::Mutex` isn't reentrant.
pub fn signal_foreground_group(pgid: Pid, sig: u64) {
    enum Action {
        Discard,
        Terminate,
        Stop,
        SetPending,
    }

    let targets: Vec<(Pid, Action)> = {
        // Mutable, not just read-locked, so the real SIGCONT-resume pre-check below (same
        // "always wakes an actually-stopped target regardless of its own disposition" semantics
        // do_kill's own cross-process branch establishes) can flip a Stopped member's state
        // in-place before the action-resolution pass below reads it back.
        let mut table = PROCESS_TABLE.lock();
        let matching: Vec<Pid> = table
            .iter()
            .filter(|(_, p)| p.pgid == pgid && !matches!(p.state, ProcState::Zombie(_)))
            .map(|(&pid, _)| pid)
            .collect();

        if sig == SIGCONT {
            for &pid in &matching {
                if let Some(proc) = table.get_mut(&pid)
                    && matches!(proc.state, ProcState::Stopped(_))
                {
                    proc.state = ProcState::Ready;
                    proc.cont_notify_pending = true;
                    proc.stop_notify_pending = false; // see Process::stop_notify_pending's own doc comment
                    proc.pending_signals &= !((1 << (SIGSTOP - 1)) | (1 << (SIGTSTP - 1)));
                    scheduler::enqueue_ready(pid);
                }
            }
        }

        matching
            .into_iter()
            .map(|pid| {
                let proc = table.get(&pid).unwrap();
                let action = match proc.sigactions[sig as usize].handler {
                    1 => Action::Discard, // SIG_IGN
                    0 => match default_disposition(sig) {
                        DefaultDisposition::Ignore => Action::Discard,
                        DefaultDisposition::Terminate => Action::Terminate,
                        DefaultDisposition::Stop => Action::Stop,
                    },
                    _ => Action::SetPending,
                };
                (pid, action)
            })
            .collect()
    };

    for (pid, action) in targets {
        match action {
            Action::Discard => {}
            Action::Terminate => terminate_process(pid, 128 + sig as i32),
            Action::Stop => {
                let mut table = PROCESS_TABLE.lock();
                if let Some(proc) = table.get_mut(&pid) {
                    // Same Ready-must-be-dequeued reasoning as do_kill's own Action::Stop arm.
                    if proc.state == ProcState::Ready {
                        scheduler::remove_ready(pid);
                    }
                    proc.state = ProcState::Stopped(sig);
                    proc.stop_notify_pending = true;
                    proc.pending_signals &= !(1 << (SIGCONT - 1));
                    wake_parent_if_waiting(&mut table, pid);
                }
            }
            Action::SetPending => {
                let mut table = PROCESS_TABLE.lock();
                if let Some(proc) = table.get_mut(&pid) {
                    // No real single sending process to attribute -- see QueuedSigInfo::pid's own
                    // doc comment for why this is an honest 0, not a fabricated one. No caller here
                    // can observe a queue-full EAGAIN either -- same reasoning as do_kill's own
                    // cross-process arm.
                    let _ = record_pending(proc, sig, SI_USER, 0, 0, 0);
                    wake_if_paused(pid, proc, sig);
                    wake_if_sigwaiting(pid, proc, sig);
                    wake_if_sleeping(pid, proc, sig);
                    wake_if_mq_waiting(pid, proc, sig);
                    wake_if_futex_waiting(pid, proc, sig);
                }
            }
        }
    }
}

/// Real per-real-time-signal-number instance cap -- small and fixed, same tier as every other
/// bounded queue in this codebase (pipe/mqueue/flock table); every RT-signal pilot test only ever
/// queues up to 5 instances (`sigqueue/4-1.c`'s own `NUMCALLS`). Exceeding it is a real, documented
/// `sigqueue(2)` `EAGAIN` case (`man 3p sigqueue`: "No resources are available to queue the
/// signal"), not a soundness concern.
pub(crate) const RT_QUEUE_CAP: usize = 16;

/// Sets `sig` pending on `proc`, recording real sender identity/payload alongside the bitmask --
/// see `QueuedSigInfo`'s own doc comment for why this replaces every bare `proc.pending_signals |=
/// 1 << (sig - 1)` this codebase used to have. `code` is `SI_USER` for a plain `kill`/`tkill`-
/// shaped signal, `SI_QUEUE` for one that arrived via `do_sigqueue`.
///
/// Real-time signals (`sig >= SIGRTMIN`) get genuine multi-instance queuing via `Process::rt_queue`
/// instead of `pending_siginfo`'s single-slot "last write wins" collapse -- POSIX requires a second
/// `sigqueue`/`raise` against an already-pending RT signal number to queue separately, not merge.
/// `Err(EAGAIN)` once that signal's own queue is full (`RT_QUEUE_CAP`) -- `do_sigqueue` propagates
/// this to its caller; `do_kill`/`signal_foreground_group` (whose own `kill(2)`/broadcast callers
/// have no path to observe a failure here, and for whom the pending bit is already set regardless)
/// just drop the excess instance.
fn record_pending(
    proc: &mut Process,
    sig: u64,
    code: i32,
    sender_pid: Pid,
    sender_uid: u32,
    value: u64,
) -> Result<(), u64> {
    proc.pending_signals |= 1 << (sig - 1);
    let info = QueuedSigInfo {
        code,
        pid: sender_pid,
        uid: sender_uid,
        value,
    };
    if sig >= SIGRTMIN {
        let q = &mut proc.rt_queue[(sig - SIGRTMIN) as usize];
        if q.len() >= RT_QUEUE_CAP {
            return Err(EAGAIN);
        }
        q.push(info);
    } else {
        proc.pending_siginfo[sig as usize] = info;
    }
    Ok(())
}

/// Wakes `pid` if it's blocked in `do_sigtimedwait` and `sig` is a member of the set it's waiting
/// on -- shared by every `Action::SetPending` arm (`do_kill`/`signal_foreground_group`/
/// `do_sigqueue`), the same shape `wake_if_paused` below already establishes for `do_pause`/
/// `do_sigsuspend`. Spurious wakes aren't possible: `do_sigtimedwait`'s own loop re-checks
/// `pending_signals & wait_set` fresh after waking regardless of why it woke.
pub(crate) fn wake_if_sigwaiting(pid: Pid, proc: &mut Process, sig: u64) {
    if let ProcState::Blocked(BlockReason::WaitingForSpecificSignal(wait_set, _)) = proc.state
        && wait_set & (1 << (sig - 1)) != 0
    {
        proc.state = ProcState::Ready;
        scheduler::enqueue_ready(pid);
    }
}

/// Wakes `pid` if it's blocked in `do_pause` and `sig` isn't currently blocked by it -- shared by
/// `do_kill`/`signal_foreground_group`'s own `Action::SetPending` arms (the only action shape that
/// corresponds to real POSIX `pause(2)`'s own wake condition: a signal that will invoke a caught
/// handler; see `BlockReason::WaitingForSignal`'s own doc comment for why `Action::Discard`/
/// `Action::Terminate`/`Action::Stop` don't need this hook at all). Spurious wakes aren't possible
/// here (unlike `wake_parent_if_waiting`'s `WUNTRACED`/`WCONTINUED` case) since this is the exact
/// condition `do_pause`'s own loop re-checks after waking anyway.
pub(crate) fn wake_if_paused(pid: Pid, proc: &mut Process, sig: u64) {
    if proc.state == ProcState::Blocked(BlockReason::WaitingForSignal)
        && proc.blocked_signals & (1 << (sig - 1)) == 0
    {
        proc.state = ProcState::Ready;
        scheduler::enqueue_ready(pid);
    }
}

/// Wakes `pid` if it's blocked in `process::timers::do_nanosleep` and `sig` isn't currently
/// blocked by it -- same shape `wake_if_paused` above already establishes, for the identical real
/// POSIX reason: `nanosleep(2)` must be interrupted early by a signal that will invoke a caught
/// handler, not just have its pending bit set while remaining genuinely `Blocked` (which would
/// leave it waiting out the full deadline regardless -- `do_nanosleep`'s own loop only re-checks
/// deliverability right before each re-block, so without this hook nothing would ever notice
/// early). Found live via the Open POSIX Test Suite pilot's `nanosleep/1-3.c`.
pub(crate) fn wake_if_sleeping(pid: Pid, proc: &mut Process, sig: u64) {
    if let ProcState::Blocked(BlockReason::Sleeping(_)) = proc.state
        && proc.blocked_signals & (1 << (sig - 1)) == 0
    {
        proc.state = ProcState::Ready;
        scheduler::enqueue_ready(pid);
    }
}

/// Wakes `pid` if it's blocked in `fs::mqueue::do_mq_timedreceive`/`do_mq_timedsend` and `sig`
/// isn't currently blocked by it -- same shape `wake_if_sleeping` above already establishes, for
/// the identical real POSIX reason: a blocking `mq_receive(3)`/`mq_send(3)` must be interrupted
/// early by a signal that will invoke a caught handler, not wait out its own deadline (`u64::MAX`
/// for the plain, non-timed wrapper shape -- see `BlockReason::WaitingForMqData`'s own doc comment
/// -- meaning without this hook a plain `mq_receive()` on an empty queue could never be interrupted
/// at all, a permanent hang, not just a missed-early-wake"). Found live, not preemptively: the
/// expanded POSIX conformance pilot's own `mq_receive/13-1.c` hung the entire pilot run past any
/// of `t0`'s own 40s per-test bound -- a parent blocked in a plain `mq_receive()` never woke for a
/// child's `kill(pid, SIGABRT)`, and (since a genuinely stuck syscall blocks the whole process, not
/// just the one call) even `t0`'s own outer `SIGALRM` had nothing to interrupt either, since
/// `do_mq_timedreceive`'s loop only re-checks the real queue state, never a deliverable signal.
pub(crate) fn wake_if_mq_waiting(pid: Pid, proc: &mut Process, sig: u64) {
    if let ProcState::Blocked(
        BlockReason::WaitingForMqData(_, _) | BlockReason::WaitingForMqSpace(_, _),
    ) = proc.state
        && proc.blocked_signals & (1 << (sig - 1)) == 0
    {
        proc.state = ProcState::Ready;
        scheduler::enqueue_ready(pid);
    }
}

/// Wakes `pid` if it's blocked in `process::do_futex`'s real `FUTEX_WAIT` and `sig` isn't
/// currently blocked by it -- same shape `wake_if_sleeping`/`wake_if_mq_waiting` above already
/// establish, for the identical real POSIX reason: a blocking `FUTEX_WAIT` (reached via
/// `sem_timedwait`'s own real futex call) must be interrupted early by a signal that will invoke a
/// caught handler, not wait out its own deadline (`u64::MAX` for the plain, no-timeout case --
/// see `BlockReason::WaitingForFutex`'s own doc comment). `do_futex`'s own post-wake check re-
/// verifies deliverability fresh, same discipline every blocking primitive here already follows.
pub(crate) fn wake_if_futex_waiting(pid: Pid, proc: &mut Process, sig: u64) {
    if let ProcState::Blocked(BlockReason::WaitingForFutex(_, _, _)) = proc.state
        && proc.blocked_signals & (1 << (sig - 1)) == 0
    {
        proc.state = ProcState::Ready;
        scheduler::enqueue_ready(pid);
    }
}

/// `SYS_PAUSE`'s real logic (`529`, item 4 of `docs/MISSING_POSIX_SYSCALLS.md`'s own 28-syscall
/// pre-reserved batch). Real POSIX semantics: blocks until a signal is delivered that either
/// terminates the process (in which case this never returns at all) or invokes a caught handler
/// -- always returns `EINTR` otherwise (`pause(2)` has no successful return). An ignored signal,
/// a default-`Terminate`/`Stop` signal is handled by its own existing immediate path (`do_kill`'s
/// `Action::Terminate`/`Action::Stop`, unaffected by this process's `ProcState`), and a signal
/// blocked via `sigprocmask` don't wake this loop at all -- it just keeps blocking, matching real
/// semantics.
///
/// Checks the real wake condition (`pending_signals & !blocked_signals != 0`) *before* ever
/// blocking, same "avoid a lost wakeup" reasoning every other blocking primitive in this codebase
/// already follows -- a signal could already be pending-but-deferred from before this call (e.g.
/// delivery deferred while blocked, since unblocked). Loops and re-checks after every wake, same
/// established pattern this codebase already audits for (`ScheduleWakeup`-worthy: any cross-
/// process force-wake mechanism needs every `scheduler::schedule()` call site to loop-and-recheck,
/// not trust "state == Ready now" to mean "my specific event happened" -- see
/// `ProcState::Stopped`'s own doc comment for the identical reasoning).
///
/// Once a deliverable signal exists, this returns `Err(EINTR)` and control resumes normally at
/// this exact syscall's own dispatch tail (`src/syscall/mod.rs`'s `deliver_pending_signal`, called
/// once at the tail of every completed syscall) -- which is what actually invokes the handler
/// (redirecting the live frame) before the caller ever observes `pause()` "returning" `-1`/`EINTR`
/// at its own call site, matching real POSIX behavior where a caught signal's handler runs before
/// the interrupted `pause()` call is ever seen to return.
pub fn do_pause(pid: Pid) -> Result<u64, u64> {
    loop {
        {
            let mut table = PROCESS_TABLE.lock();
            let proc = table
                .get_mut(&pid)
                .expect("pause: current process missing from table");
            if proc.pending_signals & !proc.blocked_signals != 0 {
                return Err(EINTR);
            }
            proc.state = ProcState::Blocked(BlockReason::WaitingForSignal);
        }
        scheduler::schedule();
    }
}

/// `SYS_SIGSUSPEND`'s real logic (`530`, item 5 of `docs/MISSING_POSIX_SYSCALLS.md`'s own
/// 28-syscall pre-reserved batch). Real POSIX semantics: atomically replaces `blocked_signals`
/// with `*mask_ptr` for the duration of a single wait, blocks until a signal is delivered that
/// either terminates the process (never returns) or invokes a caught handler, then restores the
/// original mask once that's resolved -- always returns `EINTR` otherwise (`sigsuspend(2)` has no
/// successful return). This is exactly `do_pause` above plus a temporary, atomic mask swap around
/// the same loop (musl's own `pause()` is separate, unrelated code -- not implemented in terms of
/// this syscall on this ABI, unlike on some real Unixes). The atomicity POSIX requires (no window
/// where a signal sent between reading the old mask and blocking could be missed) falls out for
/// free from this kernel's single-core, no-preemption design: nothing else can run between the
/// mask swap below and the loop's first deliverability check.
///
/// `blocked_signals` is deliberately left at the *temporary* mask, not restored here, for as long
/// as a deliverable signal has only just been detected -- restoring immediately, before
/// `deliver_pending_signal` (`src/syscall.rs`, called right after this returns, still within the
/// same `syscall_dispatch` invocation) gets to act on it, would be wrong: the very signal that
/// just woke this call is very often blocked under the *original* mask (this is in fact the
/// canonical `sigsuspend` use case -- temporarily unblocking a signal that's normally blocked, to
/// atomically wait for it) and would become invisible to `take_deliverable_signal` right when
/// it's needed. Instead, `Process::sigsuspend_restore_mask` records the mask to restore, consumed
/// by `deliver_pending_signal` itself once it knows how the signal was actually resolved
/// (immediately, if no handler runs; deferred until `sigreturn`, if one does) -- see that
/// function's own doc comment for the three-way split.
pub fn do_sigsuspend(pid: Pid, mask_ptr: u64) -> Result<u64, u64> {
    // SIGKILL/SIGSTOP can never be blocked -- same masking sigprocmask's SIG_SETMASK already
    // applies.
    let unblockable = (1u64 << (SIGKILL - 1)) | (1u64 << (SIGSTOP - 1));
    // SAFETY: same known pointer-validation gap sys_read/sys_write already document.
    let requested = unsafe { (mask_ptr as *const u64).read() };

    let original_mask = {
        let mut table = PROCESS_TABLE.lock();
        let proc = table
            .get_mut(&pid)
            .expect("sigsuspend: current process missing from table");
        let original = proc.blocked_signals;
        proc.blocked_signals = requested & !unblockable;
        original
    };

    loop {
        {
            let mut table = PROCESS_TABLE.lock();
            let proc = table
                .get_mut(&pid)
                .expect("sigsuspend: current process missing from table");
            if proc.pending_signals & !proc.blocked_signals != 0 {
                proc.sigsuspend_restore_mask = Some(original_mask);
                return Err(EINTR);
            }
            proc.state = ProcState::Blocked(BlockReason::WaitingForSignal);
        }
        scheduler::schedule();
    }
}

/// Consumes `Process::sigsuspend_restore_mask` -- `None` unless the process currently finishing a
/// syscall is doing so via a freshly-woken `do_sigsuspend` above. Called once, by
/// `deliver_pending_signal` (`src/syscall.rs`), immediately after `take_deliverable_signal`.
pub(crate) fn take_sigsuspend_restore_mask(pid: Pid) -> Option<u64> {
    let mut table = PROCESS_TABLE.lock();
    table.get_mut(&pid)?.sigsuspend_restore_mask.take()
}

/// Restores `blocked_signals` directly -- used by `deliver_pending_signal` for every
/// `do_sigsuspend` wakeup outcome that *won't* pass through `sigreturn` (no handler ran, or the
/// process terminated/stopped instead of catching it) -- the `Handler` case instead defers via
/// `set_signal_saved_blocked_override` below, since a real handler is about to run under the
/// temporary mask and must not have it restored out from under it.
pub(crate) fn restore_blocked_signals(pid: Pid, mask: u64) {
    let mut table = PROCESS_TABLE.lock();
    if let Some(proc) = table.get_mut(&pid) {
        proc.blocked_signals = mask;
    }
}

/// Overrides the mask `sigreturn` (`take_signal_saved_frame`) will restore once the handler
/// `deliver_pending_signal` just redirected into finishes -- used only for a `do_sigsuspend`
/// wakeup that resolved to a real caught handler. Real POSIX semantics restore `sigsuspend`'s
/// *original* pre-call mask once the handler returns, not the temporary one `do_sigsuspend`
/// swapped in for the wait itself (already folded into the handler's own `mask_to_add` via
/// `stash_signal_context`'s normal path, which this call runs immediately before -- so the entry
/// being overridden is always the one `signal_stack` just gained).
pub(crate) fn set_signal_saved_blocked_override(pid: Pid, mask: u64) {
    let mut table = PROCESS_TABLE.lock();
    if let Some(proc) = table.get_mut(&pid)
        && let Some(top) = proc.signal_stack.last_mut()
    {
        top.blocked_before = mask;
    }
}

/// `SYS_SIGACTION`'s real logic (`sig` already validated — `1..=31` or `SIGRTMIN..=SIGRTMAX`, not
/// `SIGKILL`/`SIGSTOP` — by
/// `src/syscall.rs`'s `sys_sigaction` before this is reached). Reads/writes a real musl
/// `struct k_sigaction` (`handler`, `flags`, `restorer`, then `mask` as a plain `u64` — matching
/// what musl's own `_NSIG/8 == 8`-byte mask width already is on this ABI, see `SigAction`'s own
/// doc comment) directly at `act_ptr`/`oldact_ptr` — no translation needed, since real `SIG_DFL`/
/// `SIG_IGN` already are `0`/`1`.
pub fn do_sigaction(pid: Pid, sig: u64, act_ptr: u64, oldact_ptr: u64) -> Result<u64, u64> {
    #[repr(C)]
    struct RawSigAction {
        handler: u64,
        flags: u64,
        restorer: u64,
        mask: u64,
    }

    let mut table = PROCESS_TABLE.lock();
    let proc = table
        .get_mut(&pid)
        .expect("sigaction: current process missing from table");

    if oldact_ptr != 0 {
        let old = proc.sigactions[sig as usize];
        let raw = RawSigAction {
            handler: old.handler,
            flags: old.flags,
            restorer: old.restorer,
            mask: old.mask,
        };
        // SAFETY: same known pointer-validation gap sys_read/sys_write already document.
        unsafe { (oldact_ptr as *mut RawSigAction).write(raw) };
    }
    if act_ptr != 0 {
        // SAFETY: same known pointer-validation gap sys_read/sys_write already document.
        let raw = unsafe { &*(act_ptr as *const RawSigAction) };
        proc.sigactions[sig as usize] = SigAction {
            handler: raw.handler,
            flags: raw.flags,
            restorer: raw.restorer,
            mask: raw.mask,
        };
    }
    Ok(0)
}

/// `SYS_SIGALTSTACK`'s real logic (`528`, item 3 of `docs/MISSING_POSIX_SYSCALLS.md`'s own
/// 28-syscall pre-reserved batch). Reads/writes a real musl `struct sigaltstack`/`stack_t`
/// (`ss_sp`, `ss_flags`, `ss_size` — see `AltStack`'s own doc comment for the "bookkeeping only,
/// never actually switched to" scope). Both `ss_ptr`/`old_ptr` are optional (either may be `0`),
/// matching real `sigaltstack(2)`'s own semantics — a call can read the current state, set a new
/// one, or both in one shot.
///
/// `ss_ptr != 0`: real Linux rejects any `ss_flags` bit outside `SS_DISABLE` with `EINVAL` — a
/// caller can never *set* `SS_ONSTACK` (it's a read-only status bit, see `AltStack`'s own doc
/// comment), and musl's own `sigaltstack()` wrapper already filters that client-side before ever
/// reaching this syscall. `SS_DISABLE` set discards `ss_sp`/`ss_size` (matches real Linux: a
/// disabled alt stack's address/size are meaningless); otherwise the real `(sp, size)` pair is
/// stored verbatim — no minimum-size enforcement here since this kernel never actually switches
/// to this stack, and musl's own wrapper already enforces `_SC_MINSIGSTKSZ` client-side before
/// this is ever reached.
pub fn do_sigaltstack(pid: Pid, ss_ptr: u64, old_ptr: u64) -> Result<u64, u64> {
    #[repr(C)]
    struct RawSigaltstack {
        sp: u64,
        flags: i32,
        size: u64,
    }

    let mut table = PROCESS_TABLE.lock();
    let proc = table
        .get_mut(&pid)
        .expect("sigaltstack: current process missing from table");

    if old_ptr != 0 {
        let raw = RawSigaltstack {
            sp: proc.altstack.sp,
            // This kernel never actually executes a handler on the alt stack -- SS_ONSTACK is
            // always reported unset, an honest reflection of what actually happens (see
            // AltStack's own doc comment).
            flags: proc.altstack.flags & !SS_ONSTACK,
            size: proc.altstack.size,
        };
        // SAFETY: same known pointer-validation gap sys_read/sys_write already document.
        unsafe { (old_ptr as *mut RawSigaltstack).write_unaligned(raw) };
    }
    if ss_ptr != 0 {
        // SAFETY: same known pointer-validation gap sys_read/sys_write already document.
        let raw = unsafe { (ss_ptr as *const RawSigaltstack).read_unaligned() };
        if raw.flags & !SS_DISABLE != 0 {
            return Err(EINVAL);
        }
        proc.altstack = if raw.flags & SS_DISABLE != 0 {
            AltStack::default()
        } else {
            AltStack {
                sp: raw.sp,
                flags: 0,
                size: raw.size,
            }
        };
    }
    Ok(0)
}

/// `SYS_SIGPROCMASK`'s real logic. `set`/`old` are read/written as a plain `u64` (see
/// `do_sigaction`'s own doc comment for why that's the right width here) rather than iterating
/// musl's own wider in-memory `sigset_t` — `sigsetsize` (always `8` from musl) tells the real
/// kernel how many bytes to actually exchange, and this ABI just always treats that as "one
/// `u64`," matching what musl's callers always pass anyway.
pub fn do_sigprocmask(pid: Pid, how: u64, set_ptr: u64, oldset_ptr: u64) -> Result<u64, u64> {
    const SIG_BLOCK: u64 = 0;
    const SIG_UNBLOCK: u64 = 1;
    const SIG_SETMASK: u64 = 2;

    let mut table = PROCESS_TABLE.lock();
    let proc = table
        .get_mut(&pid)
        .expect("sigprocmask: current process missing from table");

    if oldset_ptr != 0 {
        // SAFETY: same known pointer-validation gap sys_read/sys_write already document.
        unsafe { (oldset_ptr as *mut u64).write(proc.blocked_signals) };
    }
    if set_ptr != 0 {
        // SAFETY: same known pointer-validation gap sys_read/sys_write already document.
        let requested = unsafe { (set_ptr as *const u64).read() };
        // SIGKILL/SIGSTOP can never be blocked, matching real sigprocmask()'s own silent masking.
        let unblockable = (1u64 << (SIGKILL - 1)) | (1u64 << (SIGSTOP - 1));
        match how {
            SIG_BLOCK => proc.blocked_signals |= requested & !unblockable,
            SIG_UNBLOCK => proc.blocked_signals &= !requested,
            SIG_SETMASK => proc.blocked_signals = requested & !unblockable,
            _ => return Err(EINVAL),
        }
    }
    Ok(0)
}

/// `SYS_SIGPENDING`'s real logic — a direct readback of `pending_signals`, the same field
/// `do_kill` already sets and `deliver_pending_signal` already drains; no new state needed. `set`
/// is written as a plain `u64`, same "always one `u64`, matching what musl's callers always pass
/// anyway" story as `do_sigprocmask`'s own `blocked_signals` readback above.
pub fn do_sigpending(pid: Pid, set_ptr: u64) -> Result<u64, u64> {
    let table = PROCESS_TABLE.lock();
    let proc = table
        .get(&pid)
        .expect("sigpending: current process missing from table");
    if set_ptr != 0 {
        // SAFETY: same known pointer-validation gap sys_read/sys_write already document.
        unsafe { (set_ptr as *mut u64).write(proc.pending_signals) };
    }
    Ok(0)
}

/// Called once, at the tail of every completed syscall (`src/syscall.rs`'s
/// `deliver_pending_signal`) for whichever process is currently running. Picks the
/// lowest-numbered pending, unblocked signal (if any), removes it from `pending_signals`, and
/// resolves it against that signal's *current* disposition — looping past any that turn out to be
/// `SIG_IGN`/default-`Ignore` (discarded silently) rather than returning early, so a single call
/// still surfaces the next real (`Terminate`/`Handler`) signal if one's also pending.
pub(crate) fn take_deliverable_signal(pid: Pid) -> Option<SignalDelivery> {
    let mut table = PROCESS_TABLE.lock();
    let proc = table.get_mut(&pid)?;
    loop {
        let deliverable = proc.pending_signals & !proc.blocked_signals;
        if deliverable == 0 {
            return None;
        }
        let signum = deliverable.trailing_zeros() as u64 + 1;
        // Real-time signals pop their own FIFO's front instance and only clear the pending bit
        // once that queue is actually empty -- a second (or third, ...) still-queued instance must
        // stay deliverable, unlike a standard signal's single-slot collapse. See
        // Process::rt_queue's own doc comment.
        let siginfo = if signum >= SIGRTMIN {
            let q = &mut proc.rt_queue[(signum - SIGRTMIN) as usize];
            let info = q.remove(0);
            if q.is_empty() {
                proc.pending_signals &= !(1 << (signum - 1));
            }
            info
        } else {
            proc.pending_signals &= !(1 << (signum - 1));
            proc.pending_siginfo[signum as usize]
        };

        let action = proc.sigactions[signum as usize];
        match action.handler {
            1 => continue, // SIG_IGN
            0 => match default_disposition(signum) {
                DefaultDisposition::Ignore => continue,
                DefaultDisposition::Terminate => {
                    return Some(SignalDelivery::Terminate(128 + signum as i32));
                }
                DefaultDisposition::Stop => return Some(SignalDelivery::Stop(signum)),
            },
            handler_addr => {
                let mut mask_to_add = action.mask | (1 << (signum - 1));
                if action.flags & SA_NODEFER != 0 {
                    mask_to_add &= !(1u64 << (signum - 1));
                }
                return Some(SignalDelivery::Handler {
                    signum,
                    handler: handler_addr,
                    restorer: action.restorer,
                    mask_to_add,
                    flags: action.flags,
                    siginfo,
                });
            }
        }
    }
}

/// Snapshots `saved` (the frame the interrupted syscall was about to resume into) and grows
/// `blocked_signals` by `mask_to_add` for the handler's own duration — called by
/// `deliver_pending_signal` right before it redirects the live frame into the handler itself.
/// Pushes onto `Process::signal_stack` rather than overwriting a single slot, so a second (or
/// third, ...) delivery while one or more handlers are already running (chained, still before
/// userspace ever resumes) stacks cleanly instead of clobbering an in-progress one — see that
/// field's own doc comment. `take_signal_saved_frame` (below) is this operation's inverse (one pop
/// per `sigreturn`). Returns the *pre*-mutation `blocked_signals` (the mask the interrupted
/// program was actually running under) — `deliver_pending_signal`'s own `SA_SIGINFO` path uses
/// this for the constructed `ucontext_t`'s `uc_sigmask`, matching real Linux's own "the mask in
/// effect just before the handler was entered" semantics.
pub(crate) fn stash_signal_context(pid: Pid, saved: SyscallFrame, mask_to_add: u64) -> u64 {
    let mut table = PROCESS_TABLE.lock();
    let Some(proc) = table.get_mut(&pid) else {
        return 0;
    };
    let old_mask = proc.blocked_signals;
    proc.blocked_signals |= mask_to_add;
    proc.signal_stack.push(SignalStackFrame {
        saved,
        blocked_before: old_mask,
    });
    old_mask
}

/// `sigreturn`'s real logic (`src/syscall.rs`'s `do_sigreturn` — kept here, not there, since it
/// only needs `Process`/table access, not `SyscallFrame` field access). Pops the top entry
/// `stash_signal_context` pushed and restores `blocked_signals` to what it was before that entry's
/// own handler was entered. `None` if the stack is empty (a spurious call). `do_sigreturn` itself
/// re-checks for a further deliverable signal right after this returns — see that function's own
/// doc comment — which is what turns a chain of `stash_signal_context` pushes into a real,
/// POSIX-shaped succession of handler invocations before userspace ever actually resumes.
pub(crate) fn take_signal_saved_frame(pid: Pid) -> Option<SyscallFrame> {
    let mut table = PROCESS_TABLE.lock();
    let proc = table.get_mut(&pid)?;
    let entry = proc.signal_stack.pop()?;
    proc.blocked_signals = entry.blocked_before;
    Some(entry.saved)
}

/// Real relative-timeout conversion for `sigtimedwait` -- the same whole-second/sub-second tick
/// rounding `crate::fs::sysv_sem`'s own `resolve_relative_deadline`/`do_nanosleep` already
/// establish (duplicated locally, not shared -- same "small per-module copy over cross-module
/// abstraction" precedent that module's own doc comment already sets for this exact shape).
/// `ts_ptr == 0` blocks indefinitely -- `sigwaitinfo`/`sigwait`'s own null-timeout case, matching
/// real POSIX (`sigwaitinfo(mask, si)` is exactly `sigtimedwait(mask, si, NULL)`).
fn resolve_relative_deadline(ts_ptr: u64) -> Result<u64, u64> {
    if ts_ptr == 0 {
        return Ok(u64::MAX);
    }
    // SAFETY: same known pointer-validation gap every other user-memory read in this codebase
    // already has.
    let (sec, nsec) = unsafe {
        let ts = ts_ptr as *const i64;
        (*ts, *ts.add(1))
    };
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        return Err(EINVAL);
    }
    let hz = crate::cpu::pit::TIMER_HZ as u64;
    let whole_second_ticks = sec as u64 * hz;
    let sub_second_ticks = (nsec as u64 * hz).div_ceil(1_000_000_000);
    Ok(crate::cpu::interrupts::ticks() + whole_second_ticks + sub_second_ticks)
}

/// `SYS_SIGTIMEDWAIT = 495` -- backs real `sigtimedwait(2)`/`sigwaitinfo(2)`/`sigwait(3)`, all
/// three musl library entry points route through this one real syscall (`sigwaitinfo` passes a
/// null `ts`; `sigwait` passes its own stack `siginfo_t` and discards everything but `si_signo`) --
/// one of the two real, confirmed-live-caller gaps `docs/MISSING_POSIX_SYSCALLS.md`'s own "Missing,
/// live caller confirmed" table tracked (`src/signal/sigtimedwait.c:19,22`). Real `(mask_ptr,
/// info_ptr, ts_ptr, sigsetsize)` wire format already fits this ABI's 4 real register args --
/// confirmed directly against that file's own call site: no `SYS_rt_sigtimedwait_time64` sibling is
/// ever defined for this arch (`bits/syscall.h.in` never declares one), so only the plain
/// `__syscall_cp(SYS_rt_sigtimedwait, mask, si, ts, _NSIG/8)` branch ever compiles in -- zero musl
/// call-site patches needed, the same "already fits" story `semget`'s own sub-batch established.
///
/// **Real semantics, deliberately distinct from `do_pause`/`do_sigsuspend`**: this directly
/// *consumes* a pending signal matching `wait_set` from `pending_signals` and returns its number --
/// no handler is ever invoked, even if one is installed for it (real POSIX: `sigwaitinfo` bypasses
/// the normal disposition/handler machinery entirely), so this never goes through
/// `take_deliverable_signal`/`deliver_pending_signal` at all. Checks `pending_signals & wait_set`
/// directly, **not** `& !blocked_signals` -- a signal in `wait_set` doesn't need to already be
/// blocked via `sigprocmask` for this to see it, though real usage always blocks it first anyway
/// (the standard idiom this call exists for), and this kernel already lets a blocked signal
/// accumulate in `pending_signals` regardless (see `do_kill`'s own doc comment).
///
/// `info_ptr`, if non-null, is filled with a real (not fabricated) `siginfo_t` built from
/// `Process::pending_siginfo` -- real `si_code` (`SI_USER` for a plain `kill`, `SI_QUEUE` for
/// `sigqueue`), real sender `si_pid`/`si_uid`, and the real `sigqueue`-supplied `si_value` if any.
///
/// Real `EAGAIN` on timeout -- `sigtimedwait(2)`'s own documented errno for this case, distinct
/// from `semtimedop`'s `ETIMEDOUT`. Checks the real wake condition before ever blocking and loops/
/// re-checks after every wake, same "avoid a lost wakeup, never trust the wake reason alone"
/// discipline every blocking primitive in this codebase already follows.
pub fn do_sigtimedwait(pid: Pid, mask_ptr: u64, info_ptr: u64, ts_ptr: u64) -> Result<u64, u64> {
    // SAFETY: same known pointer-validation gap every other user-memory read in this codebase
    // already has.
    let wait_set = unsafe { (mask_ptr as *const u64).read() };
    let deadline = resolve_relative_deadline(ts_ptr)?;

    loop {
        {
            let mut table = PROCESS_TABLE.lock();
            let proc = table
                .get_mut(&pid)
                .expect("sigtimedwait: current process missing from table");
            let ready = proc.pending_signals & wait_set;
            if ready != 0 {
                let signum = ready.trailing_zeros() as u64 + 1;
                // Same real-time-vs-standard split as take_deliverable_signal above.
                let info = if signum >= SIGRTMIN {
                    let q = &mut proc.rt_queue[(signum - SIGRTMIN) as usize];
                    let info = q.remove(0);
                    if q.is_empty() {
                        proc.pending_signals &= !(1 << (signum - 1));
                    }
                    info
                } else {
                    proc.pending_signals &= !(1 << (signum - 1));
                    proc.pending_siginfo[signum as usize]
                };
                drop(table);
                if info_ptr != 0 {
                    let raw = RawSiginfo {
                        si_signo: signum as i32,
                        si_code: info.code,
                        si_errno: 0,
                        _pad0: 0,
                        si_pid: info.pid as i32,
                        si_uid: info.uid as i32,
                        si_value: info.value,
                        _tail: [0; 128 - 4 * 4 - 2 * 4 - 8],
                    };
                    // SAFETY: same known pointer-validation gap every other user-memory write in
                    // this codebase already has.
                    unsafe { (info_ptr as *mut RawSiginfo).write_unaligned(raw) };
                }
                return Ok(signum);
            }
            if crate::cpu::interrupts::ticks() >= deadline {
                return Err(EAGAIN);
            }
            proc.state = ProcState::Blocked(BlockReason::WaitingForSpecificSignal(wait_set, deadline));
        }
        scheduler::schedule();
    }
}

/// `SYS_SIGQUEUE = 496` -- backs real `sigqueue(3)`, the other of the two real, confirmed-live-
/// caller gaps `docs/MISSING_POSIX_SYSCALLS.md`'s own "Missing, live caller confirmed" table
/// tracked (`src/signal/sigqueue.c:19`). Real `(pid, sig, siginfo_ptr)` wire format
/// (`third_party/musl/src/signal/sigqueue.c`'s own `syscall(SYS_rt_sigqueueinfo, pid, sig, &si)`),
/// no musl call-site patch needed. Unlike `kill(2)`, real `sigqueue(2)` only ever targets a single
/// specific pid -- no `0`/negative process-group broadcast shape exists for it, so `target_pid <=
/// 0` is a real `EINVAL` here (real Linux's kernel does technically permit a couple of edge-case
/// pid values through `rt_sigqueueinfo` -- no live caller to exercise the difference, and musl's own
/// `sigqueue(3)` wrapper only ever takes a plain `pid_t` from its own caller unmodified).
///
/// **Real sender-identity/payload delivery**: unlike `do_kill` (which never recorded who sent a
/// signal until this session -- see `QueuedSigInfo`'s own doc comment), this reads the real
/// `si_value` the caller built at `siginfo_ptr + 24` (musl's own `siginfo_t` layout, matching
/// `RawSiginfo`'s field order exactly: `si_signo`/`si_code`/`si_errno`/pad/`si_pid`/`si_uid` = 24
/// bytes, then `si_value`) and records it alongside the *kernel's own* authoritative `caller_pid`/
/// its real uid -- not whatever `si_pid`/`si_uid` musl happened to put in the caller's own buffer,
/// matching real Linux's kernel-side behavior of always overwriting those two fields with the true
/// sender identity regardless of what userspace supplied. Lands in `Process::pending_siginfo`, the
/// same store `sigtimedwait`/`sigwaitinfo` read back from and the `SA_SIGINFO` handler-invocation
/// path (`src/syscall/mod.rs`'s `deliver_pending_signal`) now populates its own `si_pid`/`si_uid`/
/// `si_value` from.
///
/// Reuses the same Discard/Terminate/Stop/SetPending disposition resolution `do_kill`'s own
/// single-target tail already establishes (duplicated rather than factored out -- same "small
/// per-caller copy over cross-function abstraction" precedent `signal_foreground_group` already set
/// for this exact enum/match shape). `SI_QUEUE`/the real value only ever matters for the
/// `SetPending` arm -- the only shape a caught handler or a blocked `sigtimedwait` could ever
/// actually observe it through.
///
/// **Real `sig == 0`**: same real POSIX existence-and-permission-only convention `do_kill`'s own
/// `kill(pid, 0)` already establishes (musl's `sigqueue(3)` wrapper places no extra restriction on
/// `sig` beyond what a real signal number check would already reject) -- no signal is actually
/// queued, `Err(EPERM)`/`Err(ESRCH)` via `has_signal_permission`/table lookup exactly mirrors
/// `do_kill`'s own `sig == 0` branch.
pub fn do_sigqueue(caller_pid: Pid, target_pid: i64, sig: i64, siginfo_ptr: u64) -> Result<u64, u64> {
    if sig != 0 && !((1..=31).contains(&sig) || (SIGRTMIN as i64..=SIGRTMAX as i64).contains(&sig)) {
        return Err(EINVAL);
    }
    if target_pid <= 0 {
        return Err(EINVAL);
    }
    let target = target_pid as u64;
    let sig = sig as u64;
    let caller_uid = oxidebsd_current_uid() as u32;

    if sig == 0 {
        if target == caller_pid {
            return Ok(0);
        }
        let table = PROCESS_TABLE.lock();
        return match table.get(&target) {
            Some(proc) if has_signal_permission(caller_uid, proc.shared.lock().uid) => Ok(0),
            Some(_) => Err(EPERM),
            None => Err(ESRCH),
        };
    }
    // SAFETY: same known pointer-validation gap every other user-memory read in this codebase
    // already has; si_value sits at byte offset 24 in musl's own siginfo_t, matching RawSiginfo's
    // field layout exactly (see this function's own doc comment).
    let value = unsafe { *((siginfo_ptr + 24) as *const u64) };

    if target == caller_pid {
        let mut table = PROCESS_TABLE.lock();
        let me = table
            .get_mut(&caller_pid)
            .expect("sigqueue: current process missing from table");
        // Real sigqueue(2) does document EAGAIN for a full queue -- propagate it, unlike do_kill's
        // own self-target arm.
        record_pending(me, sig, SI_QUEUE, caller_pid, caller_uid, value)?;
        return Ok(0);
    }

    enum Action {
        Discard,
        Terminate,
        Stop,
        SetPending,
    }

    let action = {
        let mut table = PROCESS_TABLE.lock();
        let proc = table.get(&target).ok_or(ESRCH)?;
        if !has_signal_permission(caller_uid, proc.shared.lock().uid) {
            return Err(EPERM);
        }
        if matches!(proc.state, ProcState::Zombie(_)) {
            return Ok(0);
        }
        // Real SIGCONT semantics -- same pre-dispatch step do_kill's own cross-process branch
        // already establishes.
        if sig == SIGCONT && matches!(proc.state, ProcState::Stopped(_)) {
            let proc = table.get_mut(&target).unwrap();
            proc.state = ProcState::Ready;
            proc.cont_notify_pending = true;
            proc.stop_notify_pending = false;
            proc.pending_signals &= !((1 << (SIGSTOP - 1)) | (1 << (SIGTSTP - 1)));
            scheduler::enqueue_ready(target);
        }
        let proc = table.get(&target).unwrap();
        match proc.sigactions[sig as usize].handler {
            1 => Action::Discard, // SIG_IGN
            0 => match default_disposition(sig) {
                DefaultDisposition::Ignore => Action::Discard,
                DefaultDisposition::Terminate => Action::Terminate,
                DefaultDisposition::Stop => Action::Stop,
            },
            _ => Action::SetPending,
        }
    };

    match action {
        Action::Discard => {}
        Action::Terminate => terminate_process(target, 128 + sig as i32),
        Action::Stop => {
            let mut table = PROCESS_TABLE.lock();
            if let Some(proc) = table.get_mut(&target) {
                if proc.state == ProcState::Ready {
                    scheduler::remove_ready(target);
                }
                proc.state = ProcState::Stopped(sig);
                proc.stop_notify_pending = true;
                proc.pending_signals &= !(1 << (SIGCONT - 1));
                wake_parent_if_waiting(&mut table, target);
            }
        }
        Action::SetPending => {
            let mut table = PROCESS_TABLE.lock();
            if let Some(proc) = table.get_mut(&target) {
                record_pending(proc, sig, SI_QUEUE, caller_pid, caller_uid, value)?;
                wake_if_paused(target, proc, sig);
                wake_if_sigwaiting(target, proc, sig);
                wake_if_sleeping(target, proc, sig);
                wake_if_mq_waiting(target, proc, sig);
                wake_if_futex_waiting(target, proc, sig);
            }
        }
    }
    Ok(0)
}
