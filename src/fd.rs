//! A minimal kernel-owned file-descriptor registry — the only channel two independently loaded
//! modules have to coordinate with each other, since modules can only ever call *into* the
//! kernel, never directly into another module (see `src/module.rs`'s doc comment). Concretely:
//! `modules/fat32/`'s open files register their read/write/close callbacks here; `src/syscall.rs`'s
//! `sys_read`/`sys_write`, for any fd that isn't stdin/stdout, look it up here and delegate.
//!
//! Known, deliberate limitation: fd numbers are handed out by a simple bump counter and never
//! reused, even after `close` — consistent with this kernel's broader "no deallocation" pattern
//! elsewhere (e.g. `memory::BootInfoFrameAllocator`, `module::allocate_region`).

use alloc::collections::BTreeMap;

use spin::Mutex;

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
}

/// 0/1/2 reserved for stdin/stdout/(a currently-unused stderr slot) — never handed out here.
static NEXT_FD: Mutex<u64> = Mutex::new(3);
static TABLE: Mutex<BTreeMap<u64, FdOps>> = Mutex::new(BTreeMap::new());

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
    TABLE.lock().insert(fd, FdOps { read, write, close });
    0
}

/// Removes `fd` from the registry and invokes its registered `close` callback (letting whichever
/// module owns it do its own cleanup, e.g. flushing a pending write) — `0` on success, `-1` if
/// `fd` wasn't registered here at all.
pub(crate) extern "C" fn oxidebsd_close_fd(fd: u64) -> i32 {
    match TABLE.lock().remove(&fd) {
        Some(ops) => {
            (ops.close)(fd);
            0
        }
        None => -1,
    }
}

/// Looks `fd` up and, if registered, calls its read callback. `None` (not any particular error
/// value) means `fd` isn't registered here at all — `syscall::sys_read` treats that as `EBADF`.
pub(crate) fn read(fd: u64, ptr: u64, len: u64) -> Option<i64> {
    let ops = *TABLE.lock().get(&fd)?;
    Some((ops.read)(fd, ptr, len))
}

pub(crate) fn write(fd: u64, ptr: u64, len: u64) -> Option<i64> {
    let ops = *TABLE.lock().get(&fd)?;
    Some((ops.write)(fd, ptr, len))
}
