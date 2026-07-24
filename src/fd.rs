//! A minimal kernel-owned file-descriptor registry — the only channel two independently loaded
//! modules have to coordinate with each other, since modules can only ever call *into* the
//! kernel, never directly into another module (see `src/module.rs`'s doc comment). Concretely:
//! `modules/fat32/`'s open files register their read/write/close callbacks here; `src/syscall.rs`'s
//! `sys_read`/`sys_write` delegate to this registry unconditionally, for *every* fd, including
//! 0/1/2 (see below) — no fd is special-cased in `sys_read`/`sys_write` itself.
//!
//! **Scoped per-process (`(Pid, fd)`), not a single flat table keyed by fd alone** — the table
//! used to be exactly that (a real, deliberate simplification, documented as a known limitation:
//! "two unrelated process trees allocating fds independently would collide"), but that turned out
//! to be more than a hypothetical collision risk once `sh` (BusyBox's `hush`) actually built a real
//! pipeline (`cmd1 | cmd2`): `pipe(2)` creates two fds in the shell process, which then forks twice
//! and each child `dup2`s one end onto its own stdin/stdout and closes its own copy of the
//! originals. With a single flat table, the *parent* closing its own copy of a pipe fd (completely
//! ordinary, expected shell behavior — the parent doesn't need the pipe once both children have it)
//! tore the entry down globally, out from under children that still needed it via `dup2`, and a
//! child's own `dup2` couldn't even find an fd the *parent* had allocated if the parent hadn't
//! forked it into being yet. Real `fork()` duplicates the whole fd table into the child (each
//! process gets its own independently-closable entry referring to the same underlying resource) —
//! `fork_inherit` below is what actually does that now.
//!
//! Known, deliberate limitation kept from before: fd numbers (and the `real_fd` identity described
//! below) are handed out by a simple global bump counter and never reused, even after every
//! reference to one is closed — consistent with this kernel's broader "no deallocation" pattern
//! elsewhere (e.g. `memory::BootInfoFrameAllocator`, `module::allocate_region`).
//!
//! **stdin/stdout/stderr are registered entries here, not special cases in `sys_read`/`sys_write`**
//! — forced by `dup2`: a shell redirecting a pipeline's stdout (`dup2(pipe_write_fd, 1)`) needs fd 1
//! itself to become an ordinary, overwritable registry entry. Bootstrapped once at boot (`init`,
//! before any real process exists) under a reserved pseudo-pid (`0`, never a real running process)
//! and inherited into every subsequently spawned process the same way a forked child inherits its
//! parent's fds (`process::spawn` calls `fork_inherit(0, new_pid)`) — see `init`'s own doc comment.
//!
//! **`real_fd` + refcounting is what makes `dup2`/`fork_inherit` actually alias shared state, not
//! just copy function pointers.** A naive alias that copied `FdOps` verbatim into a new `(pid, fd)`
//! slot would call the *same* callback functions but pass that new fd number as their first
//! argument — wrong, since e.g. `modules/fat32/`'s open-file table (or `src/pipe.rs`'s pipe-buffer
//! table) is keyed by whatever fd the resource was originally opened/created under, not by every
//! later alias. `FdOps::real_fd` (a globally unique identity, independent of which process(es) can
//! currently see it) is threaded through every alias: `read`/`write`/`close` below always invoke
//! the registered callback with `ops.real_fd`, never the `(pid, fd)` that was actually looked up.
//! `REFCOUNTS` (keyed by `real_fd`, shared across every process that has an alias of it, not per
//! `(pid, fd)`) ensures the underlying resource's own `close` callback only actually fires once
//! every alias — across every process that ever had one — has been closed.

use alloc::collections::BTreeMap;

use spin::Mutex;

use crate::scheduler;
use crate::syscall::EBADF;

/// Matches `syscall::SyscallHandler`'s own FFI convention (negative = `-errno`, non-negative =
/// success value) for the same reason — see that type's doc comment. Kept as a separate type
/// here (rather than reusing `SyscallHandler` directly) since the two represent conceptually
/// different things — syscall-number dispatch vs. per-fd operations — even though their shapes
/// happen to coincide.
pub(crate) type FdReadWrite = extern "C" fn(u64, u64, u64) -> i64;
pub(crate) type FdClose = extern "C" fn(u64) -> i64;

#[derive(Clone, Copy)]
struct FdOps {
    read: FdReadWrite,
    write: FdReadWrite,
    close: FdClose,
    /// The fd this entry's callbacks are actually invoked with — itself for a fresh registration,
    /// or another entry's own `real_fd` for a `dup2`/`fork_inherit`-created alias (see this file's
    /// module doc comment). Chains never nest more than one level deep in practice, but every alias
    /// is built by copying the *source* entry's already-resolved `real_fd`, not the source's own
    /// `(pid, fd)`, so an alias-of-an-alias still resolves directly rather than accumulating a
    /// chain.
    real_fd: u64,
}

/// A pseudo-pid, never a real running process (`process::alloc_pid` starts at `1`) — the identity
/// `init` registers fd 0/1/2 under, so `process::spawn` can bootstrap every real process's own
/// stdin/stdout/stderr via the exact same `fork_inherit` path a real `fork()` uses, rather than a
/// separate one-off "seed the new process's fds" routine.
const BOOTSTRAP_PID: u64 = 0;

/// 0/1/2 reserved for stdin/stdout/stderr, registered by `init` below — never handed out by
/// `oxidebsd_alloc_fd`.
static NEXT_FD: Mutex<u64> = Mutex::new(3);
static TABLE: Mutex<BTreeMap<(u64, u64), FdOps>> = Mutex::new(BTreeMap::new());
/// Keyed by `real_fd` (a global identity, not a `(pid, fd)` pair) — how many live `TABLE` entries,
/// across *every* process that has one, currently resolve to this underlying resource. See this
/// file's module doc comment.
static REFCOUNTS: Mutex<BTreeMap<u64, u32>> = Mutex::new(BTreeMap::new());

pub(crate) extern "C" fn oxidebsd_alloc_fd() -> u64 {
    let mut next = NEXT_FD.lock();
    let fd = *next;
    *next += 1;
    fd
}

pub(crate) extern "C" fn oxidebsd_register_fd_ops(
    fd: u64,
    read: FdReadWrite,
    write: FdReadWrite,
    close: FdClose,
) -> i32 {
    register(scheduler::current_pid(), fd, read, write, close);
    0
}

/// The non-`extern "C"` body `oxidebsd_register_fd_ops` and `init` (stdin/stdout/stderr) both share.
fn register(pid: u64, fd: u64, read: FdReadWrite, write: FdReadWrite, close: FdClose) {
    TABLE.lock().insert(
        (pid, fd),
        FdOps {
            read,
            write,
            close,
            real_fd: fd,
        },
    );
    *REFCOUNTS.lock().entry(fd).or_insert(0) += 1;
}

/// Removes the calling process's own `fd` from the registry; only actually invokes the underlying
/// resource's `close` callback once its refcount (shared across every alias, in every process,
/// created by `dup2`/`fork_inherit`) reaches zero — see this file's module doc comment. `0` on
/// success, `-1` if the calling process has no such `fd` registered at all.
pub(crate) extern "C" fn oxidebsd_close_fd(fd: u64) -> i32 {
    if close_one(scheduler::current_pid(), fd) {
        0
    } else {
        -1
    }
}

/// Shared by `oxidebsd_close_fd` and `close_all` — closes exactly one `(pid, fd)` entry. Returns
/// `false` if there was no such entry (not registered, or already closed).
fn close_one(pid: u64, fd: u64) -> bool {
    let Some(ops) = TABLE.lock().remove(&(pid, fd)) else {
        return false;
    };
    let mut refcounts = REFCOUNTS.lock();
    let count = refcounts.get_mut(&ops.real_fd).expect(
        "fd registry: real_fd missing from REFCOUNTS -- every TABLE entry must have one, \
         maintained by register/dup2/fork_inherit",
    );
    *count -= 1;
    if *count == 0 {
        refcounts.remove(&ops.real_fd);
        drop(refcounts); // don't hold the lock across the callback
        (ops.close)(ops.real_fd);
    }
    true
}

/// Closes every fd the given process still has open — real `exit()` semantics ("all of the
/// descriptors open in the calling process are closed"), not previously implemented at all (the
/// old flat, unscoped table had no notion of "this process's own fds" to close in the first
/// place). Genuinely load-bearing now, not just tidiness: a pipe's reader blocks until it sees the
/// write end's refcount reach zero (`src/pipe.rs`) — if an exiting writer process leaked its own
/// copy of that fd instead of it being closed automatically, the reader would block forever, since
/// nothing else would ever bring the refcount down. Called from `process::do_exit`.
pub(crate) fn close_all(pid: u64) {
    let fds: alloc::vec::Vec<u64> = TABLE
        .lock()
        .keys()
        .filter(|&&(p, _)| p == pid)
        .map(|&(_, fd)| fd)
        .collect();
    for fd in fds {
        close_one(pid, fd);
    }
}

/// Copies every fd the given `parent` pid currently has open into `child`'s own, freshly-forking
/// process — real `fork()` semantics (the child gets its own independently-closable reference to
/// each of the parent's open files, not a shared view of the parent's own table slots). Also used
/// by `process::spawn` to bootstrap a brand-new process's stdin/stdout/stderr from the pseudo-pid
/// `init` registered them under (see this file's module doc comment) — spawning is "inheriting from
/// the boot-time bootstrap identity" in exactly the same sense forking is "inheriting from a real
/// parent," so this one function serves both.
pub(crate) fn fork_inherit(parent: u64, child: u64) {
    let parent_entries: alloc::vec::Vec<(u64, FdOps)> = TABLE
        .lock()
        .iter()
        .filter(|&(&(p, _), _)| p == parent)
        .map(|(&(_, fd), &ops)| (fd, ops))
        .collect();
    let mut table = TABLE.lock();
    let mut refcounts = REFCOUNTS.lock();
    for (fd, ops) in parent_entries {
        table.insert((child, fd), ops);
        *refcounts.entry(ops.real_fd).or_insert(0) += 1;
    }
}

/// `SYS_DUP2`'s real logic (`src/syscall.rs`'s `sys_dup2` is a thin wrapper over this), scoped to
/// the calling process's own fds — real `dup2` is always within one process. `newfd == oldfd` is a
/// no-op *if* `oldfd` is actually open (matching real `dup2`'s own documented special case);
/// otherwise whatever `newfd` previously referred to is closed first (through the same
/// refcount-aware path `oxidebsd_close_fd` uses, so an in-use underlying resource survives if
/// another alias — in this process or any other — still references it), then `newfd` becomes a
/// fresh alias of `oldfd`'s own `real_fd`.
pub(crate) fn dup2(oldfd: u64, newfd: u64) -> Result<u64, ()> {
    let pid = scheduler::current_pid();
    if oldfd == newfd {
        return if TABLE.lock().contains_key(&(pid, oldfd)) {
            Ok(newfd)
        } else {
            Err(())
        };
    }
    let old_ops = *TABLE.lock().get(&(pid, oldfd)).ok_or(())?;
    if TABLE.lock().contains_key(&(pid, newfd)) {
        close_one(pid, newfd);
    }
    TABLE.lock().insert(
        (pid, newfd),
        FdOps {
            real_fd: old_ops.real_fd,
            ..old_ops
        },
    );
    *REFCOUNTS.lock().entry(old_ops.real_fd).or_insert(0) += 1;
    Ok(newfd)
}

/// `SYS_DUP`'s real logic (`src/syscall.rs`'s `sys_dup` is a thin wrapper over this) — real
/// `dup(2)`'s single-argument form: allocate a fresh fd (via the same bump counter
/// `oxidebsd_alloc_fd` uses) and alias it to `oldfd`'s own `real_fd`, same aliasing mechanics as
/// `dup2` above minus the caller-chosen-target-fd/close-first cases `dup2` has to handle. Added
/// specifically because BusyBox's `hush` (`CONFIG_HUSH_JOB`) calls `dup_CLOEXEC`, which tries
/// `fcntl(fd, F_DUPFD_CLOEXEC, ...)` first — this kernel has no `fcntl` at all, so that call
/// harmlessly `ENOSYS`s — and only then falls back to plain `dup(fd)`; without this, hush's own
/// `G_interactive_fd` setup gives up entirely and silently treats itself as non-interactive, the
/// exact thing turning on real interactive mode was for. See CLAUDE.md's "Interactive shell"
/// section for the full trace of why `fcntl` itself doesn't need implementing for this to work.
pub(crate) fn dup(oldfd: u64) -> Result<u64, ()> {
    let pid = scheduler::current_pid();
    let old_ops = *TABLE.lock().get(&(pid, oldfd)).ok_or(())?;
    let newfd = oxidebsd_alloc_fd();
    TABLE.lock().insert(
        (pid, newfd),
        FdOps {
            real_fd: old_ops.real_fd,
            ..old_ops
        },
    );
    *REFCOUNTS.lock().entry(old_ops.real_fd).or_insert(0) += 1;
    Ok(newfd)
}

/// Looks the calling process's own `fd` up and, if registered, calls its read callback (with
/// `real_fd`, not `fd` — see this file's module doc comment). `None` (not any particular error
/// value) means the calling process has no such `fd` registered — `syscall::sys_read` treats that
/// as `EBADF`.
/// `len == 0` short-circuits *before* reaching any registered callback, for every fd alike — a
/// real, previously-latent bug, found only by running BusyBox's `hush` as pid 1 long enough for
/// its own stdio layer to flush an empty buffer: real `write(fd, buf, 0)`/`read(fd, buf, 0)` is
/// POSIX-guaranteed not to touch `buf` at all and to return `0` immediately, regardless of whether
/// `buf` is even a valid pointer — musl's own stdio does call `write()` this way (an `fflush()` on
/// an empty buffer, seen in practice as `write(1, NULL, 0)`). Every registered callback
/// (`stdin_read`, `stdout_write`, `modules/oxfs`'s file read/write, `src/pipe.rs`'s pipe ends) used
/// to construct a slice via `core::slice::from_raw_parts(_mut)` unconditionally, which Rust's own
/// safety contract requires a non-null, aligned pointer for even at length `0` — a real, not just
/// theoretical, panic once a null pointer actually reached one. Guarding once here, centrally,
/// covers every one of them without touching each callback individually, since `sys_read`/
/// `sys_write` (`src/syscall.rs`) route every fd through these two functions unconditionally.
pub(crate) fn read(fd: u64, ptr: u64, len: u64) -> Option<i64> {
    let ops = *TABLE.lock().get(&(scheduler::current_pid(), fd))?;
    if len == 0 {
        return Some(0);
    }
    Some((ops.read)(ops.real_fd, ptr, len))
}

pub(crate) fn write(fd: u64, ptr: u64, len: u64) -> Option<i64> {
    let ops = *TABLE.lock().get(&(scheduler::current_pid(), fd))?;
    if len == 0 {
        return Some(0);
    }
    Some((ops.write)(ops.real_fd, ptr, len))
}

/// Looks up the calling process's own `fd` and returns its `real_fd` — the underlying resource
/// identity a `dup2`/`fork_inherit` alias ultimately resolves to (see this file's module doc
/// comment), not necessarily `fd` itself. `None` if the calling process has no such `fd`
/// registered. Used by `syscall::sys_ioctl` to answer "is this fd actually the console" correctly
/// even after `dup2` — checking `fd == 0/1/2` directly would be wrong, since a shell can `dup2` a
/// pipe end onto fd 0/1 (real pipelines already do exactly this), and a program can just as
/// legitimately `dup2` its real stdout onto some other fd number.
pub(crate) fn real_fd_of(fd: u64) -> Option<u64> {
    TABLE
        .lock()
        .get(&(scheduler::current_pid(), fd))
        .map(|ops| ops.real_fd)
}

/// FFI wrapper over `real_fd_of` for modules that own their own fd-keyed state (`modules/oxfs`'s
/// `OPEN_FILES`, keyed by `real_fd` like every `FdOps` callback already is) but can't call
/// `crate::fd` directly -- modules only ever call *into* the kernel, through their own hand-curated
/// symbol table (see `src/module.rs`). `SYS_FSTAT`'s handler is the first case that needs this:
/// unlike `SYS_READ`/`SYS_WRITE`, which this file's own `read`/`write` already resolve fd -> real_fd
/// for before invoking a callback, a syscall-number-registered handler (`oxfs_fstat`) receives the
/// caller's raw `fd` argument directly and has to do that resolution itself. Returns `-1` (never a
/// valid `real_fd`, which starts at `3`) for an fd the calling process doesn't have open, rather than
/// `Option`, to stay in this codebase's plain-`i64`-FFI-boundary convention (see `SyscallHandler`'s
/// own doc comment in `src/syscall.rs`).
pub(crate) extern "C" fn oxidebsd_real_fd_of(fd: u64) -> i64 {
    match real_fd_of(fd) {
        Some(real_fd) => real_fd as i64,
        None => -1,
    }
}

extern "C" fn stdin_read(_real_fd: u64, ptr: u64, len: u64) -> i64 {
    // SAFETY: same known pointer-validation gap every other user-memory read in this codebase
    // already has -- [ptr, ptr+len) isn't checked against the caller's actual mappings first.
    // len == 0 is already handled by this file's own read() above, never reaching here.
    let buf = unsafe { core::slice::from_raw_parts_mut(ptr as *mut u8, len as usize) };
    crate::stdin::read(buf) as i64
}

extern "C" fn write_not_permitted(_real_fd: u64, _ptr: u64, _len: u64) -> i64 {
    -(EBADF as i64)
}

extern "C" fn read_not_permitted(_real_fd: u64, _ptr: u64, _len: u64) -> i64 {
    -(EBADF as i64)
}

/// Shared by fd 1 (stdout) and, via `dup2`-style aliasing, fd 2 (stderr) — see this file's module
/// doc comment for why stderr isn't a genuinely separate destination.
extern "C" fn stdout_write(_real_fd: u64, ptr: u64, len: u64) -> i64 {
    // SAFETY: same known pointer-validation gap sys_read/sys_write already document.
    // len == 0 is already handled by this file's own write() above, never reaching here.
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    match core::str::from_utf8(bytes) {
        Ok(s) => {
            crate::serial_print!("{s}");
            len as i64
        }
        Err(_) => -(crate::syscall::EINVAL as i64),
    }
}

extern "C" fn stdio_close(_real_fd: u64) -> i64 {
    0
}

/// Registers fd 0/1/2 under `BOOTSTRAP_PID` — called once at boot (`src/main.rs`, before any
/// process is spawned). `process::spawn` (used both for `stsh`, pid 1, and any later
/// non-`fork`-based process creation) calls `fork_inherit(BOOTSTRAP_PID, new_pid)` right after
/// creating each new process, giving it its own independent stdin/stdout/stderr the exact same way
/// a real `fork()`ed child inherits its parent's — see this file's module doc comment. fd 2 is a
/// `dup2`-style alias of fd 1 from the moment it's created, not a second independent registration —
/// matching real shell convention (`2>&1` is normally already true by default in practice) and
/// this kernel's own total lack of a second output destination.
pub fn init() {
    register(
        BOOTSTRAP_PID,
        0,
        stdin_read,
        write_not_permitted,
        stdio_close,
    );
    register(
        BOOTSTRAP_PID,
        1,
        read_not_permitted,
        stdout_write,
        stdio_close,
    );
    TABLE.lock().insert(
        (BOOTSTRAP_PID, 2),
        FdOps {
            read: read_not_permitted,
            write: stdout_write,
            close: stdio_close,
            real_fd: 1,
        },
    );
    *REFCOUNTS.lock().entry(1).or_insert(0) += 1;
}
