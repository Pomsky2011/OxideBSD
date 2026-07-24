//! The home for whatever POSIX/libc-surface syscalls a real C program's musl-linked startup path
//! needs beyond what `modules/native_abi/` (the small, BSD-authentic core: `exit`/`read`/`write`/
//! `fork`/`wait4`/`execve`/`getpid`, plus the musl-port-driven `mmap`/`munmap`/`brk`/
//! `set_fs_base`/`writev`) and `modules/fat32/` (`open`/`close`/`chdir`/`mkdir`) already register —
//! deliberately kept a separate module rather than folded into `native_abi` further, so that one
//! stays "the authentic core" while this one carries whatever extra function real userland C
//! programs turn out to need. `true`/`echo`/`cat` didn't need anything from here; `sh` (BusyBox's
//! `hush`) is what actually filled it in, once real pipeline support (`cmd1 | cmd2`) turned out to
//! need `pipe(2)`/`dup2(2)` — see CLAUDE.md's BusyBox section.
//!
//! Same "module registers, kernel implements" split every other syscall module in this codebase
//! already uses: this crate only ever gains thin `extern "C" fn handle_x` wrappers over
//! kernel-resident `oxidebsd_sys_x` functions, never the actual behavior itself (this module can't
//! use `alloc` — see CLAUDE.md's module-loading section; the real pipe-buffer/fd-aliasing logic
//! lives in `src/pipe.rs`/`src/fd.rs`, ordinary kernel code that can).
//!
//! `SYS_SETPGID = 120`/`SYS_GETPGID = 121` (see CLAUDE.md's "BusyBox gap analysis" — the "process
//! groups" gap) continue the invented-number sequence right past `SYS_SIGRETURN = 119`, and — like
//! `SYS_PIPE`/`SYS_DUP2` above — happen to match real `setpgid(2)`/`getpgid(2)`'s own wire formats
//! exactly, no argument-convention patch needed on the musl side beyond the usual number remap.
//! Real logic (`process::do_setpgid`/`do_getpgid`, a new `Process::pgid` field) is kernel-resident,
//! same reasoning as everything else this module only ever calls through to.
//!
//! `SYS_IOCTL = 124` (see CLAUDE.md's "BusyBox gap analysis" — the "termios/ioctl + pty" gap)
//! continues the sequence right past `getpgid` (`122`/`123` reserved for a future clock/
//! `nanosleep` pass). `(fd, request, argp)` matches real `ioctl(2)`'s own argument positions, and
//! the request codes (`TCGETS`/`TCSETS*`/`TIOCGWINSZ`/`TIOCSWINSZ`) this ABI actually recognizes
//! are real Linux/generic values too — see `src/syscall.rs`'s `sys_ioctl` for what's actually
//! implemented (a real, kernel-resident termios-echo toggle plus a fixed winsize; not real
//! `ioctl`'s full surface) and why only the console fd (never a pipe/regular file) is ever allowed
//! to answer at all (`isatty()`'s own correctness depends on it).
//!
//! `SYS_DUP = 125` — real `dup(2)`'s single-argument form, needed once `CONFIG_HUSH_JOB` (see
//! CLAUDE.md's "Interactive shell" section) turned out to reach for it: `hush`'s own
//! `dup_CLOEXEC` helper tries `fcntl(fd, F_DUPFD_CLOEXEC, ...)` first, which this kernel doesn't
//! implement at all (harmlessly `ENOSYS`s), then falls back to plain `dup(fd)` — without this,
//! that fallback fails too and `hush` silently gives up on interactive mode entirely. See
//! `src/fd.rs`'s `dup` for the real aliasing logic.
#![no_std]

unsafe extern "C" {
    fn oxidebsd_log(ptr: *const u8, len: u64);
    fn oxidebsd_register_syscall(
        number: u64,
        handler: extern "C" fn(u64, u64, u64, u64) -> i64,
    ) -> i32;
    fn oxidebsd_sys_pipe(fds_ptr: u64) -> i64;
    fn oxidebsd_sys_dup2(oldfd: u64, newfd: u64) -> i64;
    fn oxidebsd_sys_setpgid(pid: u64, pgid: u64) -> i64;
    fn oxidebsd_sys_getpgid(pid: u64) -> i64;
    fn oxidebsd_sys_ioctl(fd: u64, request: u64, argp: u64) -> i64;
    fn oxidebsd_sys_dup(oldfd: u64) -> i64;
}

fn log(message: &str) {
    unsafe { oxidebsd_log(message.as_ptr(), message.len() as u64) };
}

/// `SYS_PIPE = 105`/`SYS_DUP2 = 106` — OxideBSD's own invention, numbered continuing on from the
/// musl-port-driven `mmap`/`munmap`/`brk`/`set_fs_base`/`writev` (`100`-`104`) `native_abi`
/// already registers, though both happen to match real `pipe(2)`/`dup2(2)`'s own argument shapes
/// exactly (see `src/syscall.rs`'s own doc comments on `sys_pipe`/`sys_dup2` for why neither needed
/// an invented wire format the way `open`/`execve` did).
const SYS_PIPE: u64 = 105;
const SYS_DUP2: u64 = 106;
const SYS_SETPGID: u64 = 120;
const SYS_GETPGID: u64 = 121;
const SYS_IOCTL: u64 = 124;
const SYS_DUP: u64 = 125;

extern "C" fn handle_pipe(fds_ptr: u64, _arg1: u64, _arg2: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_pipe(fds_ptr) }
}

extern "C" fn handle_dup2(oldfd: u64, newfd: u64, _arg2: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_dup2(oldfd, newfd) }
}

extern "C" fn handle_setpgid(pid: u64, pgid: u64, _arg2: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_setpgid(pid, pgid) }
}

extern "C" fn handle_getpgid(pid: u64, _arg1: u64, _arg2: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_getpgid(pid) }
}

extern "C" fn handle_ioctl(fd: u64, request: u64, argp: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_ioctl(fd, request, argp) }
}

extern "C" fn handle_dup(oldfd: u64, _arg1: u64, _arg2: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_dup(oldfd) }
}

#[unsafe(no_mangle)]
pub extern "C" fn module_init() -> i32 {
    unsafe {
        oxidebsd_register_syscall(SYS_PIPE, handle_pipe);
        oxidebsd_register_syscall(SYS_DUP2, handle_dup2);
        oxidebsd_register_syscall(SYS_SETPGID, handle_setpgid);
        oxidebsd_register_syscall(SYS_GETPGID, handle_getpgid);
        oxidebsd_register_syscall(SYS_IOCTL, handle_ioctl);
        oxidebsd_register_syscall(SYS_DUP, handle_dup);
    }
    log("[module] posix_compat: module_init running (registered SYS_PIPE/SYS_DUP2/SYS_SETPGID/SYS_GETPGID/SYS_IOCTL/SYS_DUP)\n");
    0
}
