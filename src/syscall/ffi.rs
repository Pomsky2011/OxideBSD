//! Every syscall handler this kernel implements directly in the `syscall` module (rather than
//! delegating straight to a `process::do_*` function) — real `sys_*` logic, plus the thin
//! `oxidebsd_sys_*` `extern "C"` FFI adapters `modules/native_abi`/`modules/posix_compat`/etc.
//! actually call through (see `super`'s own module doc comment for why the real behavior stays
//! kernel-resident rather than moving into those modules). Split out from `syscall.rs`'s original
//! single file purely for size — this is still conceptually the same "syscall ABI" module, just
//! its handler-implementation half rather than its dispatch-mechanism half (`super`/`mod.rs`).

use x86_64::VirtAddr;

use crate::serial_println;

use super::{EBADF, EINVAL, ENOTTY, EPERM, EPROTONOSUPPORT, ffi_result_to_result};

/// Reads up to `len` bytes into `ptr` from `fd` — a pure lookup into `crate::fs::fd`'s registry now,
/// for *every* fd including 0/1/2 (see `src/fs/fd.rs`'s module doc comment for why stdin/stdout/
/// stderr moved from being special-cased here into being ordinary, `dup2`-able registry entries:
/// stdin's own non-blocking-ring-buffer behavior lives in that file's `stdin_read` now, not here).
/// `EBADF` if `fd` isn't registered at all.
pub(crate) fn sys_read(fd: u64, ptr: u64, len: u64) -> Result<u64, u64> {
    match crate::fs::fd::read(fd, ptr, len) {
        Some(raw) => ffi_result_to_result(raw),
        None => Err(EBADF),
    }
}

/// Writes `len` bytes at `ptr` to `fd` — a pure lookup into `crate::fs::fd`'s registry now, for
/// *every* fd including 0/1/2 (see `src/fs/fd.rs`'s module doc comment; stdout/stderr's own
/// UTF-8-checked `serial_print!` path lives in that file's `stdout_write` now, not here). `EBADF`
/// if `fd` isn't registered at all.
pub(crate) fn sys_write(fd: u64, ptr: u64, len: u64) -> Result<u64, u64> {
    match crate::fs::fd::write(fd, ptr, len) {
        Some(raw) => ffi_result_to_result(raw),
        None => Err(EBADF),
    }
}

/// `SYS_WRITEV = 104` — OxideBSD's own invention, added specifically because musl's *entire*
/// stdio write path goes through `writev`, never plain `write` (see `third_party/musl`'s
/// `src/stdio/__stdio_write.c`) — without this, `printf` et al. silently produce no output at all.
/// `(fd, iov_ptr, iovcnt)` matches real `writev`'s own argument positions exactly (unlike
/// `SYS_MMAP`, nothing here needs to be dropped to fit into this ABI's argument registers). Reads
/// `iovcnt` real C `struct iovec { void *iov_base; size_t iov_len; }` entries (16 bytes each,
/// standard layout) from `iov_ptr`, and calls `sys_write` once per entry, accumulating the total.
/// Matches real `writev`'s partial-write semantics: if an entry fails after at least one earlier
/// entry already succeeded, returns `Ok(total so far)` rather than propagating the failure (a
/// later `write` call surfaces it instead); only propagates `Err` if the very first entry fails.
pub(crate) fn sys_writev(fd: u64, iov_ptr: u64, iovcnt: u64) -> Result<u64, u64> {
    #[repr(C)]
    struct IoVec {
        base: u64,
        len: u64,
    }

    let mut total: u64 = 0;
    for i in 0..iovcnt {
        // SAFETY: same known pointer-validation gap sys_read/sys_write already document -- iov_ptr
        // isn't checked against the caller's actual mappings before it's dereferenced.
        let iov = unsafe { &*(iov_ptr as *const IoVec).add(i as usize) };
        match sys_write(fd, iov.base, iov.len) {
            Ok(n) => total += n,
            Err(errno) => return if total > 0 { Ok(total) } else { Err(errno) },
        }
    }
    Ok(total)
}

/// `SYS_READV = 153` — OxideBSD's own invention, continuing the sequence past `SYS_SHUTDOWN =
/// 152`. Added specifically because musl's stdio read path goes through `readv`, not plain
/// `read`, whenever a `FILE*` has real internal buffering enabled (`third_party/musl`'s
/// `src/stdio/__stdio_read.c`: a 2-iovec scatter-read, the caller's own buffer plus musl's own
/// internal `FILE` buffer) — the exact same "musl doesn't call the simpler syscall you'd expect"
/// story `SYS_WRITEV` already told for the write side, just found much later because nothing had
/// exercised a *buffered* `fread()`/`fgets()` call against a real, slow-arriving data source until
/// BusyBox's `wget` actually downloaded a real file over HTTPS (confirmed live: the TLS/TCP fix
/// chain in this file's own known-gaps entry all worked — real response bytes came through —
/// then this surfaced on the very next buffered read). `(fd, iov_ptr, iovcnt)` matches real
/// `readv`'s own argument positions exactly. Calls `sys_read` once per `iovec` entry, stopping at
/// the first short read (an entry only partially filled) — matches real `readv`'s own contract: a
/// read returning less than requested ends the whole call there, it doesn't move on to the next
/// iovec expecting more to somehow still be available. Same partial-success semantics as
/// `sys_writev`: only propagates `Err` if the very first entry fails with nothing read yet.
pub(crate) fn sys_readv(fd: u64, iov_ptr: u64, iovcnt: u64) -> Result<u64, u64> {
    #[repr(C)]
    struct IoVec {
        base: u64,
        len: u64,
    }

    let mut total: u64 = 0;
    for i in 0..iovcnt {
        // SAFETY: same known pointer-validation gap sys_read/sys_write already document -- iov_ptr
        // isn't checked against the caller's actual mappings before it's dereferenced.
        let iov = unsafe { &*(iov_ptr as *const IoVec).add(i as usize) };
        match sys_read(fd, iov.base, iov.len) {
            Ok(n) => {
                total += n;
                if n < iov.len {
                    break; // short read -- real readv stops here, not the next iovec
                }
            }
            Err(errno) => return if total > 0 { Ok(total) } else { Err(errno) },
        }
    }
    Ok(total)
}

/// `SYS_PIPE` (`105`) — unlike most of this ABI's own inventions, matches real `pipe(2)`'s wire
/// format exactly (a single pointer to a `[i32; 2]` the kernel fills in): there's no
/// argument-convention reason to invent anything different the way `open`/`execve` needed to (see
/// "musl port"/"BusyBox port" in CLAUDE.md). Delegates to `crate::fs::pipe` for the real logic — a
/// genuinely new subsystem, needed once `sh` (BusyBox's `hush`) required real pipeline support;
/// see that module's own doc comment for why a pipe read needs to actually block (not just return
/// `Ok(0)`/`EAGAIN` the way `sys_read`'s stdin case does) for a pipeline to work at all on this
/// single-core, cooperatively-scheduled kernel.
pub(crate) fn sys_pipe(fds_ptr: u64) -> Result<u64, u64> {
    crate::fs::pipe::do_pipe(fds_ptr)
}

/// `SYS_DUP2` (`106`) — matches real `dup2(2)`'s exact `(oldfd, newfd)` signature (no
/// argument-convention mismatch here either). Delegates to `crate::fs::fd::dup2` — see that
/// function's own doc comment, and `src/fs/fd.rs`'s module doc comment, for the refcount-aware
/// fd-aliasing this needs to actually work (not just copy function pointers around).
pub(crate) fn sys_dup2(oldfd: u64, newfd: u64) -> Result<u64, u64> {
    crate::fs::fd::dup2(oldfd, newfd).map_err(|_| EBADF)
}

/// `SYS_DUP` (`125`) — matches real `dup(2)`'s exact single-argument `(oldfd)` signature.
/// Delegates to `crate::fs::fd::dup` — see that function's own doc comment for why this exists at
/// all (BusyBox's `hush`, with `CONFIG_HUSH_JOB` on, needs it to set up `G_interactive_fd`).
pub(crate) fn sys_dup(oldfd: u64) -> Result<u64, u64> {
    crate::fs::fd::dup(oldfd).map_err(|_| EBADF)
}

/// `SYS_SOCKETPAIR` (`149`) — real `socketpair(2)`'s exact `(domain, type, protocol, sv_ptr)`
/// wire format, already only 4 arguments so (unlike `open`/`execve`) nothing needed inventing
/// (see `third_party/musl`'s own `arch/x86_64/bits/syscall.h.in` comment on `__NR_socketpair`).
/// `AF_UNIX`/`SOCK_STREAM` only — the one shape BusyBox's `wget` needs (see CLAUDE.md's "Real
/// networking" known-gaps entry on `spawn_ssl_client`); anything else is `EPROTONOSUPPORT`, same
/// masking convention `net::udp::oxidebsd_sys_socket` already uses for `SOCK_CLOEXEC`/
/// `SOCK_NONBLOCK`. Delegates to `crate::fs::pipe::do_socketpair` for the real logic — not a real
/// `AF_UNIX` abstraction, just the same blocking pipe-buffer machinery `sys_pipe` already uses,
/// cross-wired into a full-duplex pair (see that module's own doc comment).
const AF_UNIX: u64 = 1;
const SOCK_STREAM: u64 = 1;
const SOCK_CLOEXEC: u64 = 0o2000000;
const SOCK_NONBLOCK: u64 = 0o4000;

pub(crate) fn sys_socketpair(
    domain: u64,
    ty: u64,
    _protocol: u64,
    fds_ptr: u64,
) -> Result<u64, u64> {
    let base_ty = ty & !(SOCK_CLOEXEC | SOCK_NONBLOCK);
    if domain != AF_UNIX || base_ty != SOCK_STREAM {
        return Err(EPROTONOSUPPORT);
    }
    crate::fs::pipe::do_socketpair(fds_ptr)
}

/// `SYS_SET_TID_ADDRESS` (`150`) — real `set_tid_address(2)`'s exact single-pointer wire format.
/// Called unconditionally by every musl-linked program at startup (`third_party/musl/src/env/
/// __init_tls.c`, storing the result as the main thread's own `tid`) and again after every real
/// `fork()` (`third_party/musl/src/process/_Fork.c`) -- previously entirely unregistered, so every
/// process on this kernel silently ran with `tid = -ENOSYS` the whole time (harmless *so far*,
/// since nothing here reads `pthread_self()->tid` for anything correctness-critical, but a real,
/// previously undiscovered gap all the same, found while tracing an unrelated `wget` HTTPS
/// failure). No real threading exists on this kernel (see CLAUDE.md) -- `tid` and `pid` are the
/// same concept here, so this just echoes `scheduler::current_pid()` back, ignoring `_tidptr`
/// entirely (no `clear_child_tid`-on-exit futex wake to honor without real `pthread_create`-spawned
/// threads).
pub(crate) fn sys_set_tid_address(_tidptr: u64) -> Result<u64, u64> {
    Ok(crate::process::scheduler::current_pid())
}

/// `SYS_FCNTL` (`151`) — real `fcntl(2)`'s `(fd, cmd, arg)` shape, already only 3 arguments (musl's
/// own wrapper, `third_party/musl/src/fcntl/fcntl.c`, always calls it this way for every command
/// this kernel implements). Only the commands BusyBox's own `libbb/xfuncs.c` (`ndelay_on`/
/// `ndelay_off`/`close_on_exec_on`) and musl's `F_DUPFD_CLOEXEC` fallback dance actually reach are
/// implemented -- everything else is `EINVAL`, matching real `fcntl`'s own behavior for a command
/// it doesn't recognize.
///
/// `F_GETFL`/`F_SETFL` only ever track/report `O_NONBLOCK` (`crate::fs::fd::is_nonblocking`/
/// `set_nonblocking`) -- real `F_GETFL` also reports the access-mode bits (`O_RDONLY`/`O_WRONLY`/
/// `O_RDWR`), not tracked here at all, a real simplification nothing in this port's roster needs
/// yet. `crate::fs::pipe::blocking_read` is the *only* reader that currently consults this flag
/// (see that module's own doc comment) -- a TCP/UDP socket or oxfs file's own read path already
/// returns promptly on "no data yet" by a different, pre-existing convention (see
/// `src/net/tcp.rs`'s `tcp_read`), so `O_NONBLOCK` on one of those is accepted and tracked but
/// doesn't change behavior.
///
/// `F_SETFD` only recognizes `FD_CLOEXEC` and accepts it as a pure no-op -- this kernel has no
/// close-on-exec enforcement in `process::do_execve` at all yet, so tracking the bit would be a
/// write nobody ever reads. `F_DUPFD`/`F_DUPFD_CLOEXEC` delegate to `crate::fs::fd::dup` (ignoring
/// the real "minimum fd number" hint in `arg` -- this kernel's bump allocator has no notion of it)
/// -- added mainly so musl's own `F_DUPFD_CLOEXEC` fallback in `fcntl.c` (which always tries that
/// command first, then falls back through `F_DUPFD`) resolves cleanly instead of chasing `EINVAL`
/// down every branch.
const F_DUPFD: u64 = 0;
const F_GETFD: u64 = 1;
const F_SETFD: u64 = 2;
const F_GETFL: u64 = 3;
const F_SETFL: u64 = 4;
const F_DUPFD_CLOEXEC: u64 = 1030;
const O_NONBLOCK: u64 = 0o4000;

pub(crate) fn sys_fcntl(fd: u64, cmd: u64, arg: u64) -> Result<u64, u64> {
    let Some(real_fd) = crate::fs::fd::real_fd_of(fd) else {
        return Err(EBADF);
    };
    match cmd {
        F_GETFL => Ok(if crate::fs::fd::is_nonblocking(real_fd) {
            O_NONBLOCK
        } else {
            0
        }),
        F_SETFL => {
            crate::fs::fd::set_nonblocking(real_fd, arg & O_NONBLOCK != 0);
            Ok(0)
        }
        F_GETFD => Ok(0), // FD_CLOEXEC never actually tracked -- see this function's doc comment
        F_SETFD => Ok(0),
        F_DUPFD | F_DUPFD_CLOEXEC => crate::fs::fd::dup(fd).map_err(|_| EBADF),
        _ => Err(EINVAL),
    }
}

/// `SYS_SHUTDOWN` (`152`) — real `shutdown(2)`'s exact `(fd, how)` shape. Resolves the caller's
/// `fd` to its own `real_fd` first (same pattern `oxfs_fstat` already established for a handler
/// that isn't routed through `crate::fs::fd::read`/`write`'s own automatic resolution) and
/// delegates to `crate::fs::pipe::do_shutdown` — real half-close semantics for an `AF_UNIX`/
/// `SOCK_STREAM` socketpair endpoint only (`ENOTSOCK` for anything else); see that function's own
/// doc comment.
pub(crate) fn sys_shutdown(fd: u64, how: u64) -> Result<u64, u64> {
    let Some(real_fd) = crate::fs::fd::real_fd_of(fd) else {
        return Err(EBADF);
    };
    crate::fs::pipe::do_shutdown(real_fd, how)
}

/// `SYS_SET_FS_BASE` (`103`) — OxideBSD's own invention, not modeled on any real OS's syscall (see
/// `modules/native_abi/`'s doc comment for why new syscalls this ABI adds don't chase FreeBSD
/// authenticity the way the pre-existing ones do). musl's x86_64 port needs a way to point `FS`
/// at a thread's TLS block during startup — real Linux uses `arch_prctl(ARCH_SET_FS, addr)`, real
/// BSD uses `sysarch(AMD64_SET_FSBASE, &addr)`; this just takes the base address directly, no
/// subcommand or indirection needed since it's the only operation this call will ever perform.
/// Always succeeds: writing `IA32_FS_BASE` has no failure mode on this kernel (no permission check,
/// no address validation — same known gap `sys_write`/`sys_read` already have for user pointers).
///
/// **Also records `base` into the calling process's own `Process::fs_base`**, not just the live
/// MSR — `IA32_FS_BASE` is a single global register, not saved/restored per-process by
/// `context_switch::switch_context` the way `RSP`/callee-saved GPRs are, so without this every
/// *other* process's `%fs`-relative TLS access (including the stack-protector canary check every
/// musl-linked binary emits) would silently break the instant a second musl-linked process ever
/// ran. `scheduler`'s own `activate_and_prepare` restores this stored value into the MSR on every
/// switch into a process — see `Process::fs_base`'s own doc comment for the real crash this fixed.
pub(crate) fn sys_set_fs_base(base: u64) -> Result<u64, u64> {
    x86_64::registers::model_specific::FsBase::write(VirtAddr::new(base));
    if let Some(me) = crate::process::table()
        .lock()
        .get_mut(&crate::process::scheduler::current_pid())
    {
        me.fs_base = base;
    }
    Ok(0)
}

/// `SYS_KILL` (`116`) — matches real `kill(2)`'s exact `(pid, sig)` wire format, same
/// "no argument-convention patch needed" story `sys_pipe`/`sys_dup2` already established.
/// Delegates to `process::do_kill` — see that function's own doc comment for what's and isn't
/// supported (no process-group/broadcast targeting, signals 0-31 -- `0` is the real POSIX
/// existence-check convention, no signal actually sent -- no `EINTR` for a signal that arrives
/// while the target is already blocked on something else).
pub(crate) fn sys_kill(pid: u64, sig: u64) -> Result<u64, u64> {
    crate::process::do_kill(crate::process::scheduler::current_pid(), pid as i64, sig as i64)
}

/// `SYS_SIGACTION` (`117`) — matches real `rt_sigaction(2)`'s exact
/// `(sig, act_ptr, oldact_ptr, sigsetsize)` wire format (`sigsetsize` is read but not otherwise
/// validated — this ABI always treats a signal set as a single `u64`, matching what musl's own
/// `_NSIG/8` happens to already be on this ABI). `SIGKILL`/`SIGSTOP` can never be caught, matching
/// real `sigaction()`'s own `EINVAL` for them.
pub(crate) fn sys_sigaction(
    sig: u64,
    act_ptr: u64,
    oldact_ptr: u64,
    sigsetsize: u64,
) -> Result<u64, u64> {
    let _ = sigsetsize;
    if !(1..=31).contains(&sig) || sig == crate::process::SIGKILL || sig == crate::process::SIGSTOP
    {
        return Err(EINVAL);
    }
    crate::process::do_sigaction(crate::process::scheduler::current_pid(), sig, act_ptr, oldact_ptr)
}

/// `SYS_SIGPROCMASK` (`118`) — matches real `rt_sigprocmask(2)`'s exact
/// `(how, set_ptr, oldset_ptr, sigsetsize)` wire format, same story as `sys_sigaction` above.
pub(crate) fn sys_sigprocmask(
    how: u64,
    set_ptr: u64,
    oldset_ptr: u64,
    sigsetsize: u64,
) -> Result<u64, u64> {
    let _ = sigsetsize;
    crate::process::do_sigprocmask(crate::process::scheduler::current_pid(), how, set_ptr, oldset_ptr)
}

/// `SYS_SIGPENDING` (real `rt_sigpending`'s own wire slot, `494` after the collision sweep in
/// `docs/MISSING_POSIX_SYSCALLS.md` redirected it off its previous accidental home at `SYS_STAT =
/// 127`) — matches real `sigpending(2)`'s exact `(set_ptr, sigsetsize)` wire format, same
/// "sigsetsize read but not otherwise validated" story `sys_sigaction`/`sys_sigprocmask` already
/// have. Delegates to `process::do_sigpending`.
pub(crate) fn sys_sigpending(set_ptr: u64, sigsetsize: u64) -> Result<u64, u64> {
    let _ = sigsetsize;
    crate::process::do_sigpending(crate::process::scheduler::current_pid(), set_ptr)
}

/// `SYS_TKILL` (real, unclaimed Linux number `200` — used directly, no invented number or musl-
/// side remap needed, same "still completely unassigned in this ABI's own registry" story
/// `SYS_FCHMOD`/`SYS_FCHDIR` already have). Matches real `tkill(2)`'s exact `(tid, sig)` wire
/// format. `raise()`/`abort()`/`pthread_kill()`/`pthread_cancel()`/`timer_delete()` all call this
/// directly (`third_party/musl/src/signal/raise.c`, `src/exit/abort.c`), never through `kill()` —
/// previously a flat `ENOSYS`, so `abort()`/`assert()` fell through to a raw trap instead of real
/// `SIGABRT` delivery. Since `SYS_SET_TID_ADDRESS` already returns the real pid as `tid` on this
/// single-threaded kernel, `tkill(tid, sig)` is exactly `kill(tid, sig)` — a thin wrapper over the
/// existing `do_kill`, not a new primitive.
pub(crate) fn sys_tkill(tid: u64, sig: u64) -> Result<u64, u64> {
    crate::process::do_kill(crate::process::scheduler::current_pid(), tid as i64, sig as i64)
}

/// `SYS_SETPGID` (`120`) — matches real `setpgid(2)`'s exact `(pid, pgid)` wire format, same
/// "no argument-convention patch needed" story `sys_pipe`/`sys_dup2`/`sys_kill` already
/// established. Delegates to `process::do_setpgid` — see that function's own doc comment for the
/// real, documented simplification (no permission/session checks — this kernel has no uid model at
/// all yet).
pub(crate) fn sys_setpgid(pid: u64, pgid: u64) -> Result<u64, u64> {
    crate::process::do_setpgid(crate::process::scheduler::current_pid(), pid as i64, pgid as i64)
}

/// `SYS_GETPGID` (`121`) — matches real `getpgid(2)`'s exact `(pid)` wire format.
pub(crate) fn sys_getpgid(pid: u64) -> Result<u64, u64> {
    crate::process::do_getpgid(crate::process::scheduler::current_pid(), pid as i64)
}

/// `SYS_SETSID` — real x86_64 Linux's own `__NR_setsid` value (`112`, confirmed against
/// `third_party/musl/arch/x86_64/bits/syscall.h.in` directly, not assumed from a generic/other-arch
/// table — the exact class of mismatch CLAUDE.md's syscall-ABI section warns about elsewhere).
/// `third_party/musl/src/unistd/setsid.c` is a bare `syscall(SYS_setsid)` with no arguments and no
/// call-site patch needed at all — registering a handler at `112` is the complete fix, unlike
/// `open`/`execve`/`chown`/... which also needed musl-side argument-shape patches. Delegates to
/// `process::do_setsid` — see that function's own doc comment.
pub(crate) fn sys_setsid() -> Result<u64, u64> {
    crate::process::do_setsid(crate::process::scheduler::current_pid())
}

/// `SYS_GETSID` (`177` — an invented number, unlike `SYS_SETSID`: real x86_64 Linux's own
/// `__NR_getsid` is `124`, which already means `SYS_IOCTL` in this ABI, so it needed the usual
/// remap-in-musl treatment `open`/`chown`/... get, not a free ride). Matches real `getsid(2)`'s
/// exact `(pid)` wire format. Delegates to `process::do_getsid` — see that function's own doc
/// comment for why this exists (`getty`'s own real fallback path).
pub(crate) fn sys_getsid(pid: u64) -> Result<u64, u64> {
    crate::process::do_getsid(crate::process::scheduler::current_pid(), pid as i64)
}

/// `SYS_GETUID`/`SYS_GETEUID`/`SYS_GETGID`/`SYS_GETEGID` (`158`-`161`, registered by
/// `modules/posix_compat`, continuing on from `SYS_SETITIMER`/`SYS_GETITIMER = 156`/`157`) — all
/// four are real zero-argument `getuid(2)`-family calls, so only the number needed remapping on
/// the musl side. Delegate to `process::do_getuid`/`do_getgid` — see `Process::uid`'s own doc
/// comment for why there's no distinct effective value to compute.
pub(crate) fn sys_getuid() -> u64 {
    crate::process::do_getuid(crate::process::scheduler::current_pid())
}

pub(crate) fn sys_geteuid() -> u64 {
    crate::process::do_getuid(crate::process::scheduler::current_pid())
}

pub(crate) fn sys_getgid() -> u64 {
    crate::process::do_getgid(crate::process::scheduler::current_pid())
}

pub(crate) fn sys_getegid() -> u64 {
    crate::process::do_getgid(crate::process::scheduler::current_pid())
}

/// `SYS_SETUID`/`SYS_SETGID` (`162`/`163`) — real single-argument `setuid(2)`/`setgid(2)` wire
/// format. Delegates to `process::do_setuid`/`do_setgid` for the real POSIX permission rule (root
/// may become any uid/gid, anything else may only "become" its own current one).
pub(crate) fn sys_setuid(uid: u64) -> Result<u64, u64> {
    crate::process::do_setuid(crate::process::scheduler::current_pid(), uid as u32)
}

pub(crate) fn sys_setgid(gid: u64) -> Result<u64, u64> {
    crate::process::do_setgid(crate::process::scheduler::current_pid(), gid as u32)
}

/// `SYS_GETGROUPS` (`164`) — real `getgroups(int size, gid_t list[])` wire format, no
/// argument-convention patch needed (a plain `(size, ptr)` pair, no string argument to mismatch).
/// See `process::do_getgroups`'s own doc comment for why the caller's own `gid` is the complete,
/// correct answer on a kernel with no supplementary-group concept.
pub(crate) fn sys_getgroups(size: u64, list_ptr: u64) -> Result<u64, u64> {
    crate::process::do_getgroups(crate::process::scheduler::current_pid(), size as i64, list_ptr)
}

/// `SYS_SETGROUPS` (`178` — an *invented* number, not real Linux's own `__NR_setgroups` (`116`):
/// that value was already independently claimed by this ABI's own `SYS_KILL`, a real collision
/// found live — see `third_party/musl/arch/x86_64/bits/syscall.h.in`'s own comment on this line
/// for the full story). Real `(size, gid_list_ptr)` wire format, no argument-convention patch
/// needed. See `process::do_setgroups`'s own doc comment for why this is a real, permission-
/// checked no-op rather than either an unconditional success or a plain `ENOSYS`.
pub(crate) fn sys_setgroups(count: u64, list_ptr: u64) -> Result<u64, u64> {
    crate::process::do_setgroups(crate::process::scheduler::current_pid(), count, list_ptr)
}

/// `SYS_PRLIMIT64` (`478`) — real `prlimit64(2)`'s exact `(pid, resource, new_limit, old_limit)`
/// wire format. See `process::do_prlimit64`'s own doc comment for the real logic and why nothing
/// it stores is actually enforced.
pub(crate) fn sys_prlimit64(
    pid: u64,
    resource: u64,
    new_ptr: u64,
    old_ptr: u64,
) -> Result<u64, u64> {
    crate::process::do_prlimit64(
        crate::process::scheduler::current_pid(),
        pid as i64,
        resource,
        new_ptr,
        old_ptr,
    )
}

/// `SYS_SETPRIORITY` (`479`) — real `setpriority(2)`'s exact `(which, who, prio)` wire format.
pub(crate) fn sys_setpriority(which: u64, who: u64, prio: u64) -> Result<u64, u64> {
    crate::process::do_setpriority(
        crate::process::scheduler::current_pid(),
        which,
        who as i64,
        prio as i32,
    )
}

/// `SYS_GETPRIORITY` (`480`) — real `getpriority(2)`'s exact `(which, who)` wire format. See
/// `process::do_getpriority`'s own doc comment for the real `20 - nice` return-value convention.
pub(crate) fn sys_getpriority(which: u64, who: u64) -> Result<u64, u64> {
    crate::process::do_getpriority(crate::process::scheduler::current_pid(), which, who as i64)
}

/// `SYS_UMASK` (`487`) — real `umask(2)`'s exact single-`mask`-argument wire format. See
/// `process::do_umask`'s own doc comment for the real always-succeeds/returns-previous-mask
/// semantics this backs.
pub(crate) fn sys_umask(new_mask: u64) -> Result<u64, u64> {
    crate::process::do_umask(crate::process::scheduler::current_pid(), new_mask as u32)
}

/// `SYS_SCHED_SETSCHEDULER` (`481`) — real `sched_setscheduler(2)`'s exact
/// `(pid, policy, param_ptr)` wire format.
pub(crate) fn sys_sched_setscheduler(pid: u64, policy: u64, param_ptr: u64) -> Result<u64, u64> {
    crate::process::do_sched_setscheduler(
        crate::process::scheduler::current_pid(),
        pid as i64,
        policy as i32,
        param_ptr,
    )
}

/// `SYS_SCHED_GETSCHEDULER` (`482`) — real `sched_getscheduler(2)`'s exact `(pid)` wire format.
pub(crate) fn sys_sched_getscheduler(pid: u64) -> Result<u64, u64> {
    crate::process::do_sched_getscheduler(crate::process::scheduler::current_pid(), pid as i64)
}

/// `SYS_SCHED_GETPARAM` (`483`) — real `sched_getparam(2)`'s exact `(pid, param_ptr)` wire format.
pub(crate) fn sys_sched_getparam(pid: u64, param_ptr: u64) -> Result<u64, u64> {
    crate::process::do_sched_getparam(crate::process::scheduler::current_pid(), pid as i64, param_ptr)
}

/// `SYS_SCHED_GETAFFINITY` — real Linux's own `__NR_sched_getaffinity = 204`, used directly (see
/// `process::do_sched_getaffinity`'s own doc comment for why no invented number/musl remap was
/// needed) — real `sched_getaffinity(2)`'s exact `(pid, cpusetsize, mask_ptr)` wire format.
pub(crate) fn sys_sched_getaffinity(pid: u64, cpusetsize: u64, mask_ptr: u64) -> Result<u64, u64> {
    crate::process::do_sched_getaffinity(
        crate::process::scheduler::current_pid(),
        pid as i64,
        cpusetsize,
        mask_ptr,
    )
}

/// `SYS_SCHED_GET_PRIORITY_MAX`/`SYS_SCHED_GET_PRIORITY_MIN` (`484`/`485`) — real
/// `sched_get_priority_max/min(2)`'s exact single-`policy`-argument wire format. Pure functions of
/// `policy` alone — no current-process state involved.
pub(crate) fn sys_sched_get_priority_max(policy: u64) -> Result<u64, u64> {
    crate::process::do_sched_get_priority_max(policy as i32)
}

pub(crate) fn sys_sched_get_priority_min(policy: u64) -> Result<u64, u64> {
    crate::process::do_sched_get_priority_min(policy as i32)
}

/// `SYS_REBOOT` (`486`) — real `reboot(2)`'s exact single-`cmd`-argument wire format (musl's own
/// `reboot.c` passes the two real magic numbers as the first two syscall args and `cmd` as the
/// third — this ABI's 4-register width holds all three whole, no call-site patch needed). See
/// `process::do_reboot`'s own doc comment: every success path diverges.
pub(crate) fn sys_reboot(cmd: u64) -> Result<u64, u64> {
    crate::process::do_reboot(cmd)
}

/// Real Linux/generic `ioctl` request codes (`third_party/musl`'s `arch/generic/bits/ioctl.h`) --
/// this ABI's `SYS_IOCTL` reuses these verbatim as its own `request` argument values (they're
/// already architecture-generic constants, not syscall numbers, so there's nothing to remap the
/// way `open`/`execve` needed -- see `sys_ioctl`'s own doc comment).
const TCGETS: u64 = 0x5401;
const TCSETS: u64 = 0x5402;
const TCSETSW: u64 = 0x5403;
const TCSETSF: u64 = 0x5404;
const TIOCGWINSZ: u64 = 0x5413;
const TIOCSWINSZ: u64 = 0x5414;
/// Session/controlling-tty requests (see CLAUDE.md's session/controlling-tty notes, and
/// `process::Process::sid`/`stdin::CONTROLLING_SESSION`/`stdin::FOREGROUND_PGID`) — added
/// specifically to get `sulogin`/`getty` (both call `setsid()` then one of these) past their own
/// startup sequence, since this kernel had no session/foreground-process-group concept at all
/// before.
const TIOCSCTTY: u64 = 0x540E;
const TIOCGPGRP: u64 = 0x540F;
const TIOCSPGRP: u64 = 0x5410;
const TIOCNOTTY: u64 = 0x5422;

/// A fixed, plausible `struct winsize` (`third_party/musl`'s `include/alltypes.h.in`: four `u16`s,
/// `ws_row`/`ws_col`/`ws_xpixel`/`ws_ypixel`, no padding) -- this kernel has no real display-size
/// concept to report (VGA text mode is a fixed 80x25, but nothing downstream actually depends on
/// the exact number, same "value now, precision later" reasoning `AT_RANDOM`'s own placeholder
/// bytes already use), so `80x24` (leaving one row of headroom, the traditional default terminal
/// size real `stty size`/`resize`-less setups already assume) is picked purely to look sane, not
/// measured from anything.
#[repr(C)]
struct RawWinsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}
const FIXED_WINSIZE: RawWinsize = RawWinsize {
    ws_row: 24,
    ws_col: 80,
    ws_xpixel: 0,
    ws_ypixel: 0,
};

/// `SYS_IOCTL` (`124`) — real request codes (see above), but **not** real `ioctl(2)`'s full
/// surface: only the handful of tty-specific requests this kernel's own console can plausibly
/// answer (`TCGETS`/`TCSETS*`/`TIOCGWINSZ`/`TIOCSWINSZ`/`TIOCSCTTY`/`TIOCNOTTY`/`TIOCGPGRP`/
/// `TIOCSPGRP`) are handled; anything else is `ENOTTY`, logged the same way an unregistered
/// syscall number already is, so a future need is discoverable the same "boot it and read the log"
/// way every other gap in this codebase was found.
///
/// **Only ever succeeds against the console** (`crate::fs::fd::real_fd_of(fd)` resolving to
/// stdin's or stdout's own `real_fd`, `0`/`1` -- see `src/fs/fd.rs`'s module doc comment for why
/// checking `fd` itself, rather than what it currently resolves to, would be wrong after a
/// `dup2`), `ENOTTY` otherwise. This is load-bearing, not incidental: musl's own `isatty(fd)`
/// (`third_party/musl`'s `src/unistd/isatty.c`) is implemented as "does `ioctl(fd, TIOCGWINSZ,
/// ...)` succeed" -- if this answered every fd successfully, every regular `oxfs` file and every
/// pipe end would suddenly report itself as a tty too, which would be a real regression: BusyBox's
/// own graceful "not a tty" degradation (see CLAUDE.md's musl-port/BusyBox-port sections) is what
/// currently keeps e.g. a redirected/piped `cat`/`more` behaving like a real Unix pipeline.
///
/// **`TCSETS`/`TCSETSW`/`TCSETSF` are all treated identically** — real Unix distinguishes them by
/// *when* the change takes effect relative to already-queued output/input (immediately, after
/// output drains, or after input is also flushed), a distinction this kernel has no queued-output
/// concept to make meaningful at all, so applying the new settings immediately, unconditionally,
/// is already the correct behavior for the two "drain first" variants and a harmless
/// oversimplification for the third.
pub(crate) fn sys_ioctl(fd: u64, request: u64, argp: u64) -> Result<u64, u64> {
    match crate::fs::fd::real_fd_of(fd) {
        Some(0) | Some(1) => {}
        _ => return Err(ENOTTY),
    }

    match request {
        TCGETS => {
            let termios = crate::console::stdin::get_termios();
            // SAFETY: same known pointer-validation gap every other user-memory write in this
            // file already has -- argp isn't checked against the caller's actual mappings first.
            unsafe { *(argp as *mut crate::console::stdin::RawTermios) = termios };
            Ok(0)
        }
        TCSETS | TCSETSW | TCSETSF => {
            // SAFETY: same known pointer-validation gap as above, for a read this time.
            let termios = unsafe { *(argp as *const crate::console::stdin::RawTermios) };
            crate::console::stdin::set_termios(termios);
            Ok(0)
        }
        TIOCGWINSZ => {
            // SAFETY: same known pointer-validation gap as above.
            unsafe { *(argp as *mut RawWinsize) = FIXED_WINSIZE };
            Ok(0)
        }
        TIOCSWINSZ => Ok(0), // accepted, silently discarded -- nothing reads window size back out
        TIOCSCTTY => {
            let caller_pid = crate::process::scheduler::current_pid();
            let table = crate::process::table().lock();
            let Some(proc) = table.get(&caller_pid) else {
                return Err(ENOTTY);
            };
            // Real Linux requires the caller be a session leader unless `force` (the raw `argp`
            // value here, not a pointer -- real `ioctl(fd, TIOCSCTTY, arg)` passes `arg` by value
            // for this request) is set; this kernel has no permission model gating `force` itself
            // (only root has ever existed as a concept predating this pass), so any caller may
            // force-steal the controlling tty, matching real Linux's own "force requires
            // CAP_SYS_ADMIN" collapsing to "always allowed" on a kernel with no capability model.
            if proc.sid != caller_pid && argp == 0 {
                return Err(EPERM);
            }
            let sid = proc.sid;
            drop(table);
            crate::console::stdin::set_controlling_session(sid);
            Ok(0)
        }
        TIOCNOTTY => {
            let caller_pid = crate::process::scheduler::current_pid();
            let sid = crate::process::table()
                .lock()
                .get(&caller_pid)
                .map(|p| p.sid)
                .ok_or(ENOTTY)?;
            crate::console::stdin::clear_controlling_session_if(sid);
            Ok(0)
        }
        TIOCGPGRP => {
            let caller_pid = crate::process::scheduler::current_pid();
            let sid = crate::process::table()
                .lock()
                .get(&caller_pid)
                .map(|p| p.sid)
                .ok_or(ENOTTY)?;
            if crate::console::stdin::controlling_session() != Some(sid) {
                return Err(ENOTTY);
            }
            let pgid = crate::console::stdin::foreground_pgid().unwrap_or(sid) as i32;
            // SAFETY: same known pointer-validation gap every other user-memory write in this file
            // already has.
            unsafe { *(argp as *mut i32) = pgid };
            Ok(0)
        }
        TIOCSPGRP => {
            let caller_pid = crate::process::scheduler::current_pid();
            let sid = crate::process::table()
                .lock()
                .get(&caller_pid)
                .map(|p| p.sid)
                .ok_or(ENOTTY)?;
            if crate::console::stdin::controlling_session() != Some(sid) {
                return Err(ENOTTY);
            }
            // SAFETY: same known pointer-validation gap as above, for a read this time.
            let pgid = unsafe { *(argp as *const i32) };
            if pgid <= 0 {
                return Err(EINVAL);
            }
            crate::console::stdin::set_foreground_pgid(pgid as u64);
            Ok(0)
        }
        _ => {
            serial_println!("[boot] unrecognized ioctl request 0x{:x}", request);
            Err(ENOTTY)
        }
    }
}

/// musl's own `struct utsname` (`third_party/musl/include/sys/utsname.h`): six fixed 65-byte
/// NUL-padded fields, no padding between them -- same "byte-exact against the real musl layout"
/// discipline `modules/oxfs`'s `MuslStat` already follows for `stat(2)`.
#[repr(C)]
struct RawUtsname {
    sysname: [u8; 65],
    nodename: [u8; 65],
    release: [u8; 65],
    version: [u8; 65],
    machine: [u8; 65],
    domainname: [u8; 65],
}

fn utsname_field(s: &str) -> [u8; 65] {
    let mut field = [0u8; 65];
    let bytes = s.as_bytes();
    let len = bytes.len().min(64);
    field[..len].copy_from_slice(&bytes[..len]);
    field
}

/// `SYS_UNAME` (registered as `137` by `modules/posix_compat`, continuing on from `SYS_MKDIR =
/// 136`) — happens to match real `uname(2)`'s exact single-pointer wire format (musl's own
/// `uname()`, `third_party/musl/src/misc/uname.c`, is just `syscall(SYS_uname, uts)` — no
/// derived-argument computation from the pointer the way `open`'s `strlen` needed, so no
/// argument-convention patch was needed on the musl side beyond the usual number remap).
///
/// Every field is a fixed placeholder — this kernel has no real hostname-configuration mechanism
/// (`nodename` is a constant, not settable via a `sethostname(2)` this ABI doesn't have) and no
/// real build-timestamp source (`version` is a plausible-looking, hand-picked string, not derived
/// from anything). `release` is the one field that isn't fully static: it's this crate's own
/// `CARGO_PKG_VERSION`, so bumping `Cargo.toml`'s `version` moves what `uname -a`/`uname -r`
/// reports without touching this function again.
pub(crate) fn sys_uname(uts_ptr: u64) -> Result<u64, u64> {
    let uts = RawUtsname {
        sysname: utsname_field("OxideBSD"),
        nodename: utsname_field("oxidebsd"),
        release: utsname_field(env!("CARGO_PKG_VERSION")),
        version: utsname_field("#1 SMP PREEMPT"),
        machine: utsname_field("x86_64"),
        domainname: utsname_field("(none)"),
    };
    // SAFETY: same known pointer-validation gap every other user-memory write in this file
    // already has -- uts_ptr isn't checked against the caller's actual mappings first.
    unsafe { *(uts_ptr as *mut RawUtsname) = uts };
    Ok(0)
}

/// musl's own `struct timeval` on x86_64 (`third_party/musl/include/alltypes.h.in`'s `STRUCT
/// timeval` template): a `time_t`/`suseconds_t` pair, both 8 bytes on this arch.
#[repr(C)]
#[derive(Default)]
struct RawTimeval {
    tv_sec: i64,
    tv_usec: i64,
}

/// musl's own `struct rusage` on x86_64 (`third_party/musl/include/sys/resource.h`): two
/// `timeval`s, then 14 `long` fields, then a 16-`long` reserved tail -- 272 bytes total. Backs both
/// `SYS_GETRUSAGE` and `SYS_WAIT4`'s own optional 4th `rusage_ptr` argument (see
/// `write_zeroed_rusage`, below). This kernel tracks no per-process CPU time/memory-usage
/// accounting at all (`/proc/stat`'s own `cpu` line is already an honest all-zero placeholder for
/// the identical reason) -- every field here is a real, correctly-shaped, honestly-zeroed
/// placeholder rather than an invented number, same tier as `/proc/meminfo`'s `MemFree ==
/// MemTotal`.
#[repr(C)]
#[derive(Default)]
struct RawRusage {
    ru_utime: RawTimeval,
    ru_stime: RawTimeval,
    fields: [i64; 14],
    reserved: [i64; 16],
}

const _: () = assert!(core::mem::size_of::<RawRusage>() == 272);

/// Writes an all-zero `RawRusage` to `ptr` if it's non-null -- shared by `sys_getrusage` and
/// `do_wait4`'s own optional `rusage_ptr` argument (`src/process.rs`), the same "one real helper,
/// two call sites" shape `write_stat`-style functions elsewhere in this codebase already use.
pub(crate) fn write_zeroed_rusage(ptr: u64) {
    if ptr == 0 {
        return;
    }
    // SAFETY: same known pointer-validation gap every other user-memory write in this file already
    // has -- an arbitrary caller-supplied pointer isn't checked against the caller's actual
    // mappings first. `write_unaligned` since a real `struct rusage*` has no alignment guarantee
    // this kernel can rely on (unlike `RawUtsname` above, which is only ever reached from
    // `sys_uname`'s own single, always-aligned-in-practice call site).
    unsafe { (ptr as *mut RawRusage).write_unaligned(RawRusage::default()) };
}

/// `SYS_GETRUSAGE` (registered by `modules/posix_compat`, continuing on from `SYS_UMASK = 487`) --
/// real `getrusage(2)`'s exact `(who, rusage_ptr)` wire format (musl's own `getrusage()`,
/// `third_party/musl/src/misc/getrusage.c`, already issues a plain 2-argument raw syscall with a
/// bare pointer, no length-prefixing involved, so no call-site patch was needed beyond the usual
/// number remap). `who` (`RUSAGE_SELF`/`RUSAGE_CHILDREN`) makes no difference to the answer -- see
/// `RawRusage`'s own doc comment for why.
pub(crate) fn sys_getrusage(who: u64, rusage_ptr: u64) -> Result<u64, u64> {
    let _ = who;
    write_zeroed_rusage(rusage_ptr);
    Ok(0)
}

/// musl's own `struct tms` on x86_64 (`third_party/musl/include/sys/times.h`): four `clock_t`
/// (`long`, 8 bytes on this arch) fields, no padding -- 32 bytes total.
#[repr(C)]
#[derive(Default)]
struct RawTms {
    tms_utime: i64,
    tms_stime: i64,
    tms_cutime: i64,
    tms_cstime: i64,
}

const _: () = assert!(core::mem::size_of::<RawTms>() == 32);

/// `SYS_TIMES` (real `times`'s own wire slot, `493` after the collision sweep in
/// `docs/MISSING_POSIX_SYSCALLS.md` redirected it off its previous accidental home at
/// `SYS_MMAP = 100`) -- matches real `times(2)`'s exact `(tms_ptr)` wire format
/// (`third_party/musl/src/time/times.c` is a bare `__syscall(SYS_times, tms)`, no call-site patch
/// needed). The `tms` fields are an honest all-zero placeholder, same tier and same reasoning as
/// `RawRusage` above -- this kernel tracks no per-process CPU time at all, so a real per-field
/// breakdown would just be a fabricated number. The return value (real `times(2)`'s "clock ticks
/// since an arbitrary point in the past") is `crate::cpu::interrupts::ticks()` itself, the same
/// real `TIMER_HZ`-cadence counter `sys_clock_gettime`'s own `CLOCK_MONOTONIC` arm already uses --
/// an honest, non-fabricated value, just not tied to any particular epoch (matching the standard's
/// own "arbitrary point" wording).
pub(crate) fn sys_times(tms_ptr: u64) -> Result<u64, u64> {
    if tms_ptr != 0 {
        // SAFETY: same known pointer-validation gap every other user-memory write in this file
        // already has.
        unsafe { (tms_ptr as *mut RawTms).write_unaligned(RawTms::default()) };
    }
    Ok(crate::cpu::interrupts::ticks())
}

/// musl's own `struct timespec` on x86_64 (`third_party/musl/include/alltypes.h.in`'s `STRUCT
/// timespec` template, `time_t`/`long` both 8 bytes on this arch): two `i64`s, no padding.
#[repr(C)]
struct RawTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

/// Real, architecture-generic `clockid_t` values (`third_party/musl/include/time.h`) -- not
/// syscall numbers, so no remapping needed, unlike `SYS_clock_gettime` itself below.
const CLOCK_REALTIME: u64 = 0;
const CLOCK_MONOTONIC: u64 = 1;

/// `SYS_CLOCK_GETTIME` (registered as `138` by `modules/clock`, continuing on from `SYS_UNAME =
/// 137`) — matches real `clock_gettime(2)`'s exact `(clockid, timespec_ptr)` wire format, so only
/// the number needed remapping. musl's own `time()`/`gettimeofday()` (`third_party/musl/src/time/
/// time.c`/`gettimeofday.c`) are both plain wrappers around `clock_gettime(CLOCK_REALTIME, ...)`
/// at the C level, not separate syscalls, so this one remap is enough to unlock all three.
///
/// `CLOCK_REALTIME` reads `src/cpu/rtc.rs`'s CMOS RTC live on every call (see that module's own
/// doc comment for why that's simpler and more honest than caching a boot-time baseline).
/// `CLOCK_MONOTONIC` converts `src/cpu/interrupts.rs`'s `ticks()` against `src/cpu/pit.rs`'s
/// now-known `TIMER_HZ`, seconds since boot -- not wall-clock time, matching real `CLOCK_MONOTONIC`
/// semantics (unspecified epoch, only meaningful as a delta between two readings). Any other
/// `clockid` (`CLOCK_PROCESS_CPUTIME_ID`, `CLOCK_THREAD_CPUTIME_ID`, ...) is `EINVAL` -- this
/// kernel tracks neither per-process nor per-thread CPU time.
pub(crate) fn sys_clock_gettime(clockid: u64, ts_ptr: u64) -> Result<u64, u64> {
    let ts = match clockid {
        CLOCK_REALTIME => RawTimespec {
            tv_sec: crate::cpu::rtc::unix_epoch_seconds(),
            tv_nsec: 0,
        },
        CLOCK_MONOTONIC => {
            let ticks = crate::cpu::interrupts::ticks();
            let hz = crate::cpu::pit::TIMER_HZ as u64;
            RawTimespec {
                tv_sec: (ticks / hz) as i64,
                tv_nsec: ((ticks % hz) * 1_000_000_000 / hz) as i64,
            }
        }
        _ => return Err(EINVAL),
    };
    // SAFETY: same known pointer-validation gap every other user-memory write in this file
    // already has -- ts_ptr isn't checked against the caller's actual mappings first.
    unsafe { *(ts_ptr as *mut RawTimespec) = ts };
    Ok(0)
}

/// Thin FFI adapters over `sys_read`/`sys_write` for `modules/native_abi/` to call — see `super`'s
/// own module doc comment for why the underlying behavior stays here rather than being duplicated
/// into that module. Converts each function's `Result<u64, u64>` into `SyscallHandler`'s plain
/// `i64` FFI convention.
///
/// `oxidebsd_sys_exit` goes through `process::do_exit` — real, per-process termination that hands
/// control to whatever the scheduler picks next, only falling back to a full `hlt_loop()` when
/// nothing else is runnable.
///
/// **The exit code is shifted into bits 8-15 before reaching `do_exit`, matching real
/// `wait(2)`'s status-word encoding** (`WIFEXITED(status)` is `(status & 0x7f) == 0`;
/// `WEXITSTATUS(status)` is `(status >> 8) & 0xff`) — found live, the hard way: an earlier
/// version stored the caller's raw exit code unshifted, which happened to round-trip fine
/// against this kernel's own test suite (every test that checks `wait4`'s reported status against
/// a raw exit code agreed with itself, since both the write side and the read side used the same
/// wrong convention) but is real POSIX-incompatible ABI breakage for a real caller checking the
/// status the standard way. Confirmed live: BusyBox's `hush`, driving real applets through this
/// exact wait4/status path, uses the real `WIFSIGNALED`/`WTERMSIG` macros
/// (`third_party/busybox/shell/hush.c`'s `checkjobs`) to decide whether to print a
/// `strsignal()`-derived death message -- with the unshifted encoding, *any* applet exiting
/// normally with a nonzero code (extremely common: `touch`/`tee`/`rm`/... all do this on their own
/// ordinary internal errors) had its raw low byte misread as "terminated by signal N", e.g. exit
/// code `1` decoded as `WTERMSIG == 1 == SIGHUP` -- printing a spurious "Hangup" after essentially
/// any failing command and (per `checkjobs`' own default-signal-death handling) corrupting later
/// commands in the same interactive session. Signal-based termination (`process::do_kill`'s
/// `Terminate` branch, and `SignalDelivery::Terminate`'s own self-delivery path in this same file)
/// already passes `terminate_process`/`do_exit` a pre-encoded `128 + sig` value directly -- *not*
/// shifted here, and must stay that way: its low 7 bits already equal the real signal number
/// (`(128 + sig) & 0x7f == sig` for `sig < 128`), which is everything `WIFSIGNALED`/`WTERMSIG`
/// actually look at, so it was already real-wait-status-compatible before this fix and shifting it
/// again here would corrupt it. This function is the *only* place a genuine user-supplied
/// `exit(code)` value becomes a `Zombie` status, which is why the shift belongs here and not
/// inside `do_exit`/`terminate_process` themselves (shared by both conventions).
pub(crate) extern "C" fn oxidebsd_sys_exit(code: u64) -> ! {
    let status = ((code as i32) & 0xff) << 8;
    crate::process::do_exit(crate::process::scheduler::current_pid(), status)
}

// `pub`, not `pub(crate)` -- same "kept public for test use" precedent `oxidebsd_register_syscall`
// already has (see `tests/fork_wait.rs`). `tests/tcp_smoke.rs` needs the real SYS_READ/SYS_WRITE
// entry point (not a lower-level shortcut) to exercise an accepted TCP connection's fd-ops
// callbacks the exact way a real process's read()/write() would reach them.
pub extern "C" fn oxidebsd_sys_read(fd: u64, ptr: u64, len: u64) -> i64 {
    result_to_ffi(sys_read(fd, ptr, len))
}

pub extern "C" fn oxidebsd_sys_write(fd: u64, ptr: u64, len: u64) -> i64 {
    result_to_ffi(sys_write(fd, ptr, len))
}

pub(crate) extern "C" fn oxidebsd_sys_writev(fd: u64, iov_ptr: u64, iovcnt: u64) -> i64 {
    result_to_ffi(sys_writev(fd, iov_ptr, iovcnt))
}

// `pub`, not `pub(crate)` -- same "kept public for test use" precedent above; `tests/
// readv_smoke.rs` calls this directly.
pub extern "C" fn oxidebsd_sys_readv(fd: u64, iov_ptr: u64, iovcnt: u64) -> i64 {
    result_to_ffi(sys_readv(fd, iov_ptr, iovcnt))
}

pub(crate) extern "C" fn oxidebsd_sys_pipe(fds_ptr: u64) -> i64 {
    result_to_ffi(sys_pipe(fds_ptr))
}

pub(crate) extern "C" fn oxidebsd_sys_dup2(oldfd: u64, newfd: u64) -> i64 {
    result_to_ffi(sys_dup2(oldfd, newfd))
}

// `pub`, not `pub(crate)` -- same "kept public for test use" precedent `oxidebsd_sys_read`/
// `oxidebsd_sys_write` already have; `tests/socketpair_smoke.rs` calls this directly.
pub extern "C" fn oxidebsd_sys_socketpair(
    domain: u64,
    ty: u64,
    protocol: u64,
    fds_ptr: u64,
) -> i64 {
    result_to_ffi(sys_socketpair(domain, ty, protocol, fds_ptr))
}

// `pub`, not `pub(crate)` -- same "kept public for test use" precedent above.
pub extern "C" fn oxidebsd_sys_set_tid_address(tidptr: u64) -> i64 {
    result_to_ffi(sys_set_tid_address(tidptr))
}

// `pub`, not `pub(crate)` -- same "kept public for test use" precedent above; a future smoke test
// for real O_NONBLOCK behavior would call this directly, the same way tests already do for
// read/write/socketpair.
pub extern "C" fn oxidebsd_sys_fcntl(fd: u64, cmd: u64, arg: u64) -> i64 {
    result_to_ffi(sys_fcntl(fd, cmd, arg))
}

pub extern "C" fn oxidebsd_sys_shutdown(fd: u64, how: u64) -> i64 {
    result_to_ffi(sys_shutdown(fd, how))
}

pub(crate) extern "C" fn oxidebsd_sys_dup(oldfd: u64) -> i64 {
    result_to_ffi(sys_dup(oldfd))
}

pub(crate) extern "C" fn oxidebsd_sys_set_fs_base(base: u64) -> i64 {
    result_to_ffi(sys_set_fs_base(base))
}

pub(crate) extern "C" fn oxidebsd_sys_kill(pid: u64, sig: u64) -> i64 {
    result_to_ffi(sys_kill(pid, sig))
}

pub(crate) extern "C" fn oxidebsd_sys_sigaction(
    sig: u64,
    act_ptr: u64,
    oldact_ptr: u64,
    sigsetsize: u64,
) -> i64 {
    result_to_ffi(sys_sigaction(sig, act_ptr, oldact_ptr, sigsetsize))
}

pub(crate) extern "C" fn oxidebsd_sys_sigprocmask(
    how: u64,
    set_ptr: u64,
    oldset_ptr: u64,
    sigsetsize: u64,
) -> i64 {
    result_to_ffi(sys_sigprocmask(how, set_ptr, oldset_ptr, sigsetsize))
}

pub(crate) extern "C" fn oxidebsd_sys_sigpending(set_ptr: u64, sigsetsize: u64) -> i64 {
    result_to_ffi(sys_sigpending(set_ptr, sigsetsize))
}

pub(crate) extern "C" fn oxidebsd_sys_tkill(tid: u64, sig: u64) -> i64 {
    result_to_ffi(sys_tkill(tid, sig))
}

pub(crate) extern "C" fn oxidebsd_sys_setpgid(pid: u64, pgid: u64) -> i64 {
    result_to_ffi(sys_setpgid(pid, pgid))
}

pub(crate) extern "C" fn oxidebsd_sys_getpgid(pid: u64) -> i64 {
    result_to_ffi(sys_getpgid(pid))
}

pub(crate) extern "C" fn oxidebsd_sys_setsid() -> i64 {
    result_to_ffi(sys_setsid())
}

pub(crate) extern "C" fn oxidebsd_sys_getsid(pid: u64) -> i64 {
    result_to_ffi(sys_getsid(pid))
}

pub(crate) extern "C" fn oxidebsd_sys_getuid() -> i64 {
    sys_getuid() as i64
}

pub(crate) extern "C" fn oxidebsd_sys_geteuid() -> i64 {
    sys_geteuid() as i64
}

pub(crate) extern "C" fn oxidebsd_sys_getgid() -> i64 {
    sys_getgid() as i64
}

pub(crate) extern "C" fn oxidebsd_sys_getegid() -> i64 {
    sys_getegid() as i64
}

pub(crate) extern "C" fn oxidebsd_sys_setuid(uid: u64) -> i64 {
    result_to_ffi(sys_setuid(uid))
}

pub(crate) extern "C" fn oxidebsd_sys_setgid(gid: u64) -> i64 {
    result_to_ffi(sys_setgid(gid))
}

pub(crate) extern "C" fn oxidebsd_sys_getgroups(size: u64, list_ptr: u64) -> i64 {
    result_to_ffi(sys_getgroups(size, list_ptr))
}

pub(crate) extern "C" fn oxidebsd_sys_setgroups(count: u64, list_ptr: u64) -> i64 {
    result_to_ffi(sys_setgroups(count, list_ptr))
}

pub(crate) extern "C" fn oxidebsd_sys_prlimit64(
    pid: u64,
    resource: u64,
    new_ptr: u64,
    old_ptr: u64,
) -> i64 {
    result_to_ffi(sys_prlimit64(pid, resource, new_ptr, old_ptr))
}

pub(crate) extern "C" fn oxidebsd_sys_setpriority(which: u64, who: u64, prio: u64) -> i64 {
    result_to_ffi(sys_setpriority(which, who, prio))
}

pub(crate) extern "C" fn oxidebsd_sys_getpriority(which: u64, who: u64) -> i64 {
    result_to_ffi(sys_getpriority(which, who))
}

pub(crate) extern "C" fn oxidebsd_sys_umask(new_mask: u64) -> i64 {
    result_to_ffi(sys_umask(new_mask))
}

pub(crate) extern "C" fn oxidebsd_sys_sched_setscheduler(
    pid: u64,
    policy: u64,
    param_ptr: u64,
) -> i64 {
    result_to_ffi(sys_sched_setscheduler(pid, policy, param_ptr))
}

pub(crate) extern "C" fn oxidebsd_sys_sched_getscheduler(pid: u64) -> i64 {
    result_to_ffi(sys_sched_getscheduler(pid))
}

pub(crate) extern "C" fn oxidebsd_sys_sched_getparam(pid: u64, param_ptr: u64) -> i64 {
    result_to_ffi(sys_sched_getparam(pid, param_ptr))
}

pub(crate) extern "C" fn oxidebsd_sys_sched_getaffinity(
    pid: u64,
    cpusetsize: u64,
    mask_ptr: u64,
) -> i64 {
    result_to_ffi(sys_sched_getaffinity(pid, cpusetsize, mask_ptr))
}

pub(crate) extern "C" fn oxidebsd_sys_sched_get_priority_max(policy: u64) -> i64 {
    result_to_ffi(sys_sched_get_priority_max(policy))
}

pub(crate) extern "C" fn oxidebsd_sys_sched_get_priority_min(policy: u64) -> i64 {
    result_to_ffi(sys_sched_get_priority_min(policy))
}

pub(crate) extern "C" fn oxidebsd_sys_reboot(cmd: u64) -> i64 {
    result_to_ffi(sys_reboot(cmd))
}

pub(crate) extern "C" fn oxidebsd_sys_ioctl(fd: u64, request: u64, argp: u64) -> i64 {
    result_to_ffi(sys_ioctl(fd, request, argp))
}

pub(crate) extern "C" fn oxidebsd_sys_uname(uts_ptr: u64) -> i64 {
    result_to_ffi(sys_uname(uts_ptr))
}

pub(crate) extern "C" fn oxidebsd_sys_clock_gettime(clockid: u64, ts_ptr: u64) -> i64 {
    result_to_ffi(sys_clock_gettime(clockid, ts_ptr))
}

pub(crate) extern "C" fn oxidebsd_sys_nanosleep(req_ptr: u64, rem_ptr: u64) -> i64 {
    result_to_ffi(crate::process::do_nanosleep(
        crate::process::scheduler::current_pid(),
        req_ptr,
        rem_ptr,
    ))
}

/// `SYS_SETITIMER = 156`/`SYS_GETITIMER = 157` (registered by `modules/clock`, continuing on from
/// `SYS_SYMLINK = 155`) — see `process::do_setitimer`'s own doc comment for the real logic and why
/// this one syscall is enough to back both `setitimer(2)` and real `alarm(2)` (a thin musl-side
/// wrapper around it).
pub(crate) extern "C" fn oxidebsd_sys_setitimer(which: u64, new_ptr: u64, old_ptr: u64) -> i64 {
    result_to_ffi(crate::process::do_setitimer(
        crate::process::scheduler::current_pid(),
        which,
        new_ptr,
        old_ptr,
    ))
}

pub(crate) extern "C" fn oxidebsd_sys_getitimer(which: u64, old_ptr: u64) -> i64 {
    result_to_ffi(crate::process::do_getitimer(
        crate::process::scheduler::current_pid(),
        which,
        old_ptr,
    ))
}

/// Thin FFI adapters over `src/process.rs`'s `do_fork_from_current`/`do_wait4`/`do_execve`/
/// `do_getpid`/`do_mmap`/`do_munmap`/`do_brk` for `modules/native_abi/` to call — same pattern as
/// the exit/read/write adapters above, real logic kept kernel-side since module code can't use
/// `alloc`.
pub(crate) extern "C" fn oxidebsd_sys_fork() -> i64 {
    result_to_ffi(crate::process::do_fork_from_current())
}

pub(crate) extern "C" fn oxidebsd_sys_wait4(
    pid: u64,
    status_ptr: u64,
    options: u64,
    rusage_ptr: u64,
) -> i64 {
    result_to_ffi(crate::process::do_wait4(
        crate::process::scheduler::current_pid(),
        pid as i64,
        options,
        status_ptr,
        rusage_ptr,
    ))
}

pub(crate) extern "C" fn oxidebsd_sys_getrusage(who: u64, rusage_ptr: u64) -> i64 {
    result_to_ffi(sys_getrusage(who, rusage_ptr))
}

pub(crate) extern "C" fn oxidebsd_sys_times(tms_ptr: u64) -> i64 {
    result_to_ffi(sys_times(tms_ptr))
}

pub(crate) extern "C" fn oxidebsd_sys_execve(
    path_ptr: u64,
    path_len: u64,
    argv_ptr: u64,
    envp_ptr: u64,
) -> i64 {
    result_to_ffi(crate::process::do_execve(
        crate::process::scheduler::current_pid(),
        path_ptr,
        path_len,
        argv_ptr,
        envp_ptr,
    ))
}

pub(crate) extern "C" fn oxidebsd_sys_mmap(addr_hint: u64, len: u64, prot: u64) -> i64 {
    result_to_ffi(crate::process::do_mmap(
        crate::process::scheduler::current_pid(),
        addr_hint,
        len,
        prot,
    ))
}

pub(crate) extern "C" fn oxidebsd_sys_munmap(addr: u64, len: u64) -> i64 {
    result_to_ffi(crate::process::do_munmap(addr, len))
}

pub(crate) extern "C" fn oxidebsd_sys_brk(addr: u64) -> i64 {
    result_to_ffi(crate::process::do_brk(
        crate::process::scheduler::current_pid(),
        addr,
    ))
}

pub(crate) extern "C" fn oxidebsd_sys_mprotect(addr: u64, len: u64, prot: u64) -> i64 {
    result_to_ffi(crate::process::do_mprotect(addr, len, prot))
}

pub(crate) extern "C" fn oxidebsd_sys_getpid() -> i64 {
    crate::process::do_getpid() as i64
}

pub(crate) extern "C" fn oxidebsd_sys_getppid() -> i64 {
    crate::process::do_getppid() as i64
}

fn result_to_ffi(result: Result<u64, u64>) -> i64 {
    match result {
        Ok(value) => value as i64,
        Err(errno) => -(errno as i64),
    }
}
