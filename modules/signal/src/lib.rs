//! Registers `SYS_KILL`/`SYS_SIGACTION`/`SYS_SIGPROCMASK` against `src/syscall.rs`'s dispatch
//! table — a dedicated module rather than folded into `native_abi`/`posix_compat` further, since
//! real signals are a big enough new subsystem of their own to deserve their own home (see
//! CLAUDE.md's BusyBox gap-analysis section: "kill/sigaction/sigprocmask ... either native_abi or
//! a dedicated new modules/signal, whichever the user prefers"). Same "module registers, kernel
//! implements" split every other syscall module in this codebase already uses: the real logic
//! (`do_kill`/`do_sigaction`/`do_sigprocmask` in `src/process.rs`, plus the pending-signal
//! delivery/`sigreturn` machinery in `src/syscall.rs`) all stays kernel-resident, since this
//! module can't use `alloc` and the process table needs `BTreeMap` freely — this crate only ever
//! gains thin `extern "C" fn handle_x` wrappers over kernel-resident `oxidebsd_sys_x` functions.
//!
//! `SYS_KILL = 116`/`SYS_SIGACTION = 117`/`SYS_SIGPROCMASK = 118` continue the existing
//! OxideBSD-own-invented-number sequence right past `SYS_RENAME = 111`/`SYS_GETPPID = 107`, per
//! this project's established convention that syscalls added after the musl/BusyBox port invent
//! their own numbers rather than copying FreeBSD/Linux — but all three happen to match real
//! `kill(2)`/`rt_sigaction(2)`/`rt_sigprocmask(2)`'s own wire formats exactly, the same
//! "no argument-convention patch needed" story `SYS_PIPE`/`SYS_DUP2` already had — see
//! `src/process.rs`'s `do_kill`/`do_sigaction`/`do_sigprocmask` and `bits/syscall.h.in`'s own
//! comment on the musl fork.
//!
//! `SYS_SIGRETURN = 119` (real `rt_sigreturn`'s own wire slot) is deliberately **not** registered
//! here at all — see `src/syscall.rs`'s `syscall_dispatch`, which intercepts that number directly,
//! before ever reaching this module's table.
#![no_std]

unsafe extern "C" {
    fn oxidebsd_register_syscall(
        number: u64,
        handler: extern "C" fn(u64, u64, u64, u64) -> i64,
    ) -> i32;
    fn oxidebsd_sys_kill(pid: u64, sig: u64) -> i64;
    fn oxidebsd_sys_sigaction(sig: u64, act_ptr: u64, oldact_ptr: u64, sigsetsize: u64) -> i64;
    fn oxidebsd_sys_sigprocmask(how: u64, set_ptr: u64, oldset_ptr: u64, sigsetsize: u64) -> i64;
}

const SYS_KILL: u64 = 116;
const SYS_SIGACTION: u64 = 117;
const SYS_SIGPROCMASK: u64 = 118;

extern "C" fn handle_kill(pid: u64, sig: u64, _arg2: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_kill(pid, sig) }
}

extern "C" fn handle_sigaction(sig: u64, act_ptr: u64, oldact_ptr: u64, sigsetsize: u64) -> i64 {
    unsafe { oxidebsd_sys_sigaction(sig, act_ptr, oldact_ptr, sigsetsize) }
}

extern "C" fn handle_sigprocmask(how: u64, set_ptr: u64, oldset_ptr: u64, sigsetsize: u64) -> i64 {
    unsafe { oxidebsd_sys_sigprocmask(how, set_ptr, oldset_ptr, sigsetsize) }
}

#[unsafe(no_mangle)]
pub extern "C" fn module_init() -> i32 {
    unsafe {
        oxidebsd_register_syscall(SYS_KILL, handle_kill);
        oxidebsd_register_syscall(SYS_SIGACTION, handle_sigaction);
        oxidebsd_register_syscall(SYS_SIGPROCMASK, handle_sigprocmask);
    }
    0
}
