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
//!
//! `SYS_UNAME = 137` (see CLAUDE.md's "BusyBox gap analysis" — the "uname/gethostname" gap)
//! continues the sequence past `modules/oxfs`'s `SYS_MKDIR = 136`. Real logic (`src/syscall.rs`'s
//! `sys_uname`, filling in a fixed `struct utsname`) is kernel-resident, same reasoning as
//! everything else this module only ever calls through to.
//!
//! `SYS_SOCKETPAIR = 149` (see CLAUDE.md's "Real networking" known-gaps entry) continues the
//! sequence right past `modules/net`'s own `SYS_POLL = 148`. Registered here, not in
//! `modules/net/`, because it never touches the actual network stack at all -- real
//! `socketpair(2)`'s `(domain, type, protocol, sv_ptr)` shape matches this ABI's 4-register width
//! whole, so no argument-convention patch was needed on the musl side beyond the usual `__NR_*`
//! remap. Real logic (`src/syscall.rs`'s `sys_socketpair`, delegating to `crate::pipe::
//! do_socketpair`) is kernel-resident and pipe-shaped, not socket-shaped -- see that module's own
//! doc comment for why an `AF_UNIX`/`SOCK_STREAM` pair is just two cross-wired pipe buffers here,
//! same reasoning as everything else this module only ever calls through to.
//!
//! `SYS_FCNTL = 151`/`SYS_SHUTDOWN = 152` continue the sequence past `modules/native_abi`'s own
//! `SYS_SET_TID_ADDRESS = 150` (itself right past this module's `SYS_SOCKETPAIR = 149`) -- found
//! missing while tracing BusyBox's `wget` HTTPS path (see CLAUDE.md's "Real networking" known-gaps
//! entry): `libbb/xfuncs.c`'s `ndelay_on`/`ndelay_off`/`close_on_exec_on` call `fcntl`, and
//! `wget.c` itself calls `shutdown(fd, SHUT_WR)` on the same kind of socketpair endpoint
//! `SYS_SOCKETPAIR` already provides. Both `(fd, cmd, arg)`/`(fd, how)` already fit this ABI's
//! register width whole, no argument-convention patch needed. `SYS_SHUTDOWN` lives here rather
//! than `modules/net/`, same reasoning as `SYS_SOCKETPAIR` above -- it only implements real
//! half-close semantics for a `crate::pipe`-backed socketpair endpoint, not a real TCP/UDP socket.
//! Real logic (`src/syscall.rs`'s `sys_fcntl`/`sys_shutdown`) is kernel-resident, same reasoning as
//! everything else this module only ever calls through to.
//!
//! `SYS_GETUID = 158`/`SYS_GETEUID = 159`/`SYS_GETGID = 160`/`SYS_GETEGID = 161`/
//! `SYS_SETUID = 162`/`SYS_SETGID = 163`/`SYS_GETGROUPS = 164` (see CLAUDE.md's "BusyBox gap
//! analysis" -- the "uid/passwd-db model" gap) continue the sequence past `modules/clock`'s own
//! `SYS_GETITIMER = 157`. All seven are real Linux/generic `getuid(2)`-family wire formats
//! (zero/one/two plain-integer arguments, no string argument to mismatch), so only the usual
//! `__NR_*` number remap was needed on the musl side. Real logic (`process::do_getuid`/`do_getgid`/
//! `do_setuid`/`do_setgid`/`do_getgroups`, a new `Process::uid`/`gid` pair) is kernel-resident,
//! same reasoning as everything else this module only ever calls through to -- see those
//! functions' own doc comments for the real POSIX permission rule `setuid`/`setgid` enforce (root
//! may become any uid/gid, anything else may only "become" what it already is) and why
//! `getgroups` reporting a single-element list (the caller's own `gid`) is the complete, correct
//! answer on a kernel with no supplementary-group concept.
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
    fn oxidebsd_sys_uname(uts_ptr: u64) -> i64;
    fn oxidebsd_sys_socketpair(domain: u64, ty: u64, protocol: u64, fds_ptr: u64) -> i64;
    fn oxidebsd_sys_fcntl(fd: u64, cmd: u64, arg: u64) -> i64;
    fn oxidebsd_sys_shutdown(fd: u64, how: u64) -> i64;
    fn oxidebsd_sys_getuid() -> i64;
    fn oxidebsd_sys_geteuid() -> i64;
    fn oxidebsd_sys_getgid() -> i64;
    fn oxidebsd_sys_getegid() -> i64;
    fn oxidebsd_sys_setuid(uid: u64) -> i64;
    fn oxidebsd_sys_setgid(gid: u64) -> i64;
    fn oxidebsd_sys_getgroups(size: u64, list_ptr: u64) -> i64;
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
const SYS_UNAME: u64 = 137;
const SYS_SOCKETPAIR: u64 = 149;
const SYS_FCNTL: u64 = 151;
const SYS_SHUTDOWN: u64 = 152;
const SYS_GETUID: u64 = 158;
const SYS_GETEUID: u64 = 159;
const SYS_GETGID: u64 = 160;
const SYS_GETEGID: u64 = 161;
const SYS_SETUID: u64 = 162;
const SYS_SETGID: u64 = 163;
const SYS_GETGROUPS: u64 = 164;

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

extern "C" fn handle_uname(uts_ptr: u64, _arg1: u64, _arg2: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_uname(uts_ptr) }
}

extern "C" fn handle_socketpair(domain: u64, ty: u64, protocol: u64, fds_ptr: u64) -> i64 {
    unsafe { oxidebsd_sys_socketpair(domain, ty, protocol, fds_ptr) }
}

extern "C" fn handle_fcntl(fd: u64, cmd: u64, arg: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_fcntl(fd, cmd, arg) }
}

extern "C" fn handle_shutdown(fd: u64, how: u64, _arg2: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_shutdown(fd, how) }
}

extern "C" fn handle_getuid(_a0: u64, _a1: u64, _a2: u64, _a3: u64) -> i64 {
    unsafe { oxidebsd_sys_getuid() }
}

extern "C" fn handle_geteuid(_a0: u64, _a1: u64, _a2: u64, _a3: u64) -> i64 {
    unsafe { oxidebsd_sys_geteuid() }
}

extern "C" fn handle_getgid(_a0: u64, _a1: u64, _a2: u64, _a3: u64) -> i64 {
    unsafe { oxidebsd_sys_getgid() }
}

extern "C" fn handle_getegid(_a0: u64, _a1: u64, _a2: u64, _a3: u64) -> i64 {
    unsafe { oxidebsd_sys_getegid() }
}

extern "C" fn handle_setuid(uid: u64, _a1: u64, _a2: u64, _a3: u64) -> i64 {
    unsafe { oxidebsd_sys_setuid(uid) }
}

extern "C" fn handle_setgid(gid: u64, _a1: u64, _a2: u64, _a3: u64) -> i64 {
    unsafe { oxidebsd_sys_setgid(gid) }
}

extern "C" fn handle_getgroups(size: u64, list_ptr: u64, _a2: u64, _a3: u64) -> i64 {
    unsafe { oxidebsd_sys_getgroups(size, list_ptr) }
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
        oxidebsd_register_syscall(SYS_UNAME, handle_uname);
        oxidebsd_register_syscall(SYS_SOCKETPAIR, handle_socketpair);
        oxidebsd_register_syscall(SYS_FCNTL, handle_fcntl);
        oxidebsd_register_syscall(SYS_SHUTDOWN, handle_shutdown);
        oxidebsd_register_syscall(SYS_GETUID, handle_getuid);
        oxidebsd_register_syscall(SYS_GETEUID, handle_geteuid);
        oxidebsd_register_syscall(SYS_GETGID, handle_getgid);
        oxidebsd_register_syscall(SYS_GETEGID, handle_getegid);
        oxidebsd_register_syscall(SYS_SETUID, handle_setuid);
        oxidebsd_register_syscall(SYS_SETGID, handle_setgid);
        oxidebsd_register_syscall(SYS_GETGROUPS, handle_getgroups);
    }
    log(
        "[module] posix_compat: module_init running (registered SYS_PIPE/SYS_DUP2/SYS_SETPGID/SYS_GETPGID/SYS_IOCTL/SYS_DUP/SYS_UNAME/SYS_SOCKETPAIR/SYS_FCNTL/SYS_SHUTDOWN/SYS_GETUID/SYS_GETEUID/SYS_GETGID/SYS_GETEGID/SYS_SETUID/SYS_SETGID/SYS_GETGROUPS)\n",
    );
    0
}
