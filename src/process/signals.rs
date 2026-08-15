//! Signal syscalls (kill/sigaction/sigprocmask) and delivery bookkeeping -- split out of the original process.rs.

use alloc::vec::Vec;


use crate::syscall::{EINVAL, ESRCH, SyscallFrame};
use super::*;

/// `SYS_KILL`'s real logic. Only a positive `target_pid` (no process-group/broadcast targeting —
/// real `kill(2)`'s `pid <= 0` cases) and signals `1..=31` are supported; anything else is
/// `EINVAL`, matching real `kill()`'s own validation.
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
pub fn do_kill(caller_pid: Pid, target_pid: i64, sig: i64) -> Result<u64, u64> {
    if !(0..=31).contains(&sig) {
        return Err(EINVAL);
    }
    if target_pid <= 0 {
        return Err(EINVAL);
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
        SetPending,
    }

    let action = {
        // Scoped so this lock is dropped before terminate_process/the SetPending branch below
        // re-lock the table -- spin::Mutex isn't reentrant (same discipline table()'s own doc
        // comment already establishes for every other function here).
        let table = PROCESS_TABLE.lock();
        let proc = table.get(&target).ok_or(ESRCH)?;
        if matches!(proc.state, ProcState::Zombie(_)) {
            return Ok(0); // still "exists" until reaped, but there's nothing left to signal
        }
        match proc.sigactions[sig as usize].handler {
            1 => Action::Discard, // SIG_IGN
            0 => match default_disposition(sig) {
                DefaultDisposition::Ignore => Action::Discard,
                DefaultDisposition::Terminate => Action::Terminate,
            },
            _ => Action::SetPending,
        }
    };

    match action {
        Action::Discard => {}
        Action::Terminate => terminate_process(target, 128 + sig as i32),
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
/// "INTR character signals the whole foreground process group" contract, driven by
/// `interrupts::keyboard_interrupt_handler`'s own Ctrl+C handling once a real foreground group has
/// actually been claimed (`TIOCSCTTY`/`TIOCSPGRP` — see CLAUDE.md's session/controlling-tty
/// notes). Reuses the exact same Discard/Terminate/SetPending logic `do_kill`'s own cross-process
/// branch already established per target (including its own documented gap: an installed-handler
/// target only gets a deferred pending bit, not forced-immediate delivery) — just applied to every
/// matching pid instead of one. Two passes (collect targets+actions under one lock, then act) for
/// the same reason `do_kill` drops its own read lock before `terminate_process`/re-locking for
/// `SetPending`: `terminate_process` takes the table lock itself, and `spin::Mutex` isn't
/// reentrant.
pub fn signal_foreground_group(pgid: Pid, sig: u64) {
    enum Action {
        Discard,
        Terminate,
        SetPending,
    }

    let targets: Vec<(Pid, Action)> = {
        let table = PROCESS_TABLE.lock();
        table
            .iter()
            .filter(|(_, p)| p.pgid == pgid && !matches!(p.state, ProcState::Zombie(_)))
            .map(|(&pid, proc)| {
                let action = match proc.sigactions[sig as usize].handler {
                    1 => Action::Discard, // SIG_IGN
                    0 => match default_disposition(sig) {
                        DefaultDisposition::Ignore => Action::Discard,
                        DefaultDisposition::Terminate => Action::Terminate,
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
