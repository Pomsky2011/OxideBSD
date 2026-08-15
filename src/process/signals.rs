//! Signal syscalls (kill/sigaction/sigprocmask) and delivery bookkeeping -- split out of the original process.rs.

use alloc::vec::Vec;


use crate::syscall::{EINVAL, ESRCH, SyscallFrame};
use super::*;

/// `SYS_KILL`'s real logic. Signals `1..=31` only; anything else is `EINVAL`, matching real
/// `kill()`'s own validation. `target_pid == 0`/`< 0` are real POSIX process-group broadcasts
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
    if !(0..=31).contains(&sig) {
        return Err(EINVAL);
    }

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

    // Real kill(pid, 0): no signal is actually sent -- only existence (real kill(2) also checks
    // permission, which this kernel's own do_kill doesn't check for any signal, see this
    // function's own doc comment) is checked. The standard POSIX idiom for "is this pid still
    // alive" (`kill -0 $pid`) depends on this, and would otherwise always fail EINVAL since 0
    // isn't a real signal number.
    if sig == 0 {
        if target == caller_pid {
            return Ok(0);
        }
        let table = PROCESS_TABLE.lock();
        return match table.get(&target) {
            Some(_) => Ok(0), // zombie counts too -- still "exists" until reaped
            None => Err(ESRCH),
        };
    }
    let sig = sig as u64;

    if target == caller_pid {
        let mut table = PROCESS_TABLE.lock();
        let me = table
            .get_mut(&caller_pid)
            .expect("kill: current process missing from table");
        me.pending_signals |= 1 << (sig - 1);
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
                proc.pending_signals |= 1 << (sig - 1);
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
                    proc.pending_signals |= 1 << (sig - 1);
                }
            }
        }
    }
}

/// `SYS_SIGACTION`'s real logic (`sig` already validated — `1..=31`, not `SIGKILL`/`SIGSTOP` — by
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
        proc.pending_signals &= !(1 << (signum - 1));

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
                });
            }
        }
    }
}

/// Snapshots `saved` (the frame the interrupted syscall was about to resume into) and grows
/// `blocked_signals` by `mask_to_add` for the handler's own duration — called by
/// `deliver_pending_signal` right before it redirects the live frame into the handler itself.
/// `take_signal_saved_frame` (below) is this operation's inverse, run by `sigreturn`. Returns the
/// *pre*-mutation `blocked_signals` (the mask the interrupted program was actually running under)
/// — `deliver_pending_signal`'s own `SA_SIGINFO` path uses this for the constructed `ucontext_t`'s
/// `uc_sigmask`, matching real Linux's own "the mask in effect just before the handler was
/// entered" semantics.
pub(crate) fn stash_signal_context(pid: Pid, saved: SyscallFrame, mask_to_add: u64) -> u64 {
    let mut table = PROCESS_TABLE.lock();
    let Some(proc) = table.get_mut(&pid) else {
        return 0;
    };
    let old_mask = proc.blocked_signals;
    proc.signal_saved_blocked = old_mask;
    proc.blocked_signals |= mask_to_add;
    proc.signal_saved_frame = Some(saved);
    old_mask
}

/// `sigreturn`'s real logic (`src/syscall.rs`'s `do_sigreturn` — kept here, not there, since it
/// only needs `Process`/table access, not `SyscallFrame` field access). Takes (removes) the
/// snapshot `stash_signal_context` stored and restores `blocked_signals` to what it was before the
/// handler was entered. `None` if nothing was actually stashed (a spurious call).
pub(crate) fn take_signal_saved_frame(pid: Pid) -> Option<SyscallFrame> {
    let mut table = PROCESS_TABLE.lock();
    let proc = table.get_mut(&pid)?;
    let saved = proc.signal_saved_frame.take()?;
    proc.blocked_signals = proc.signal_saved_blocked;
    Some(saved)
}
