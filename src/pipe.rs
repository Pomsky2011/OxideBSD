//! Real pipes — `src/fd.rs`'s registry gains two new kinds of registered fd (a pipe's read end and
//! write end), backed by an actual, blocking, in-kernel buffer. Added specifically because `sh`
//! (BusyBox's `hush` — see CLAUDE.md's BusyBox section) needs real `cmd1 | cmd2` pipeline support,
//! which `pipe(2)`/`dup2(2)` alone can't provide without something to actually hold the bytes in
//! flight between the two processes.
//!
//! **A pipe read has to genuinely block, not just return `Ok(0)`/`EAGAIN` the way stdin's own
//! non-blocking `sys_read` does.** This kernel is single-core and purely cooperatively scheduled
//! (see `src/scheduler.rs`'s own doc comment) — nothing preempts a running process. If a
//! pipeline's reader (say `cmd2` in `cmd1 | cmd2`) polled an empty pipe and got `Ok(0)`/`EAGAIN`
//! back immediately instead of actually yielding the CPU, it would spin forever on its own kernel
//! stack: `cmd1` (the writer) would never get a chance to run and produce the data `cmd2` is
//! waiting for, since nothing else can interrupt `cmd2`'s busy loop. `pipe_read` below instead
//! blocks for real — `process::BlockReason::WaitingForPipeData`, the exact same
//! block-then-`scheduler::schedule()` pattern `process::do_wait4` already established for blocking
//! on a child process — and `pipe_write`/`pipe_close` wake any process blocked on the pipe they
//! just touched, the same way `process::do_exit` wakes a parent blocked in `wait4`.
//!
//! **Deliberately unbounded, so only the read side ever blocks.** A real pipe has a fixed capacity
//! and blocks a writer once it's full; this one's buffer is a plain growable `VecDeque<u8>`, so
//! `pipe_write` always succeeds immediately and completely. Simpler, and safe for this kernel's
//! actual use case (a shell's own pipeline commands, not an adversarial or high-throughput
//! producer) — a real bound (and the write-side blocking that would come with it) is a follow-up,
//! not attempted here.

use alloc::collections::{BTreeMap, VecDeque};

use spin::Mutex;

use crate::process::{self, BlockReason, ProcState};
use crate::scheduler;
use crate::syscall::EPIPE;

struct PipeBuffer {
    data: VecDeque<u8>,
    read_closed: bool,
    write_closed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum End {
    Read,
    Write,
}

static NEXT_PIPE_ID: Mutex<u64> = Mutex::new(1);
static PIPES: Mutex<BTreeMap<u64, PipeBuffer>> = Mutex::new(BTreeMap::new());
/// Keyed by a pipe end's own `real_fd` (see `src/fd.rs`'s module doc comment) — stable across any
/// `dup2` alias of that end, since `crate::fd::read`/`write`/`close` always invoke a registered
/// callback with `real_fd`, never whichever fd was actually looked up.
static PIPE_ENDS: Mutex<BTreeMap<u64, (u64, End)>> = Mutex::new(BTreeMap::new());

const EBADF: i64 = 9;

/// `SYS_PIPE`'s real logic. Allocates a fresh pipe id and buffer, allocates two fds
/// (`crate::fd::oxidebsd_alloc_fd`) and registers each end's own callbacks against them, then
/// writes `[read_fd, write_fd]` at `fds_ptr` as two `i32`s — matching real `pipe(2)`'s exact wire
/// format (a pointer to `int pipefd[2]`), since nothing about this call's shape needed inventing
/// the way `open`/`execve` did (see `src/syscall.rs`'s own doc comment on `sys_pipe`).
pub(crate) fn do_pipe(fds_ptr: u64) -> Result<u64, u64> {
    let pipe_id = {
        let mut next = NEXT_PIPE_ID.lock();
        let id = *next;
        *next += 1;
        id
    };
    PIPES.lock().insert(
        pipe_id,
        PipeBuffer {
            data: VecDeque::new(),
            read_closed: false,
            write_closed: false,
        },
    );

    let read_fd = crate::fd::oxidebsd_alloc_fd();
    let write_fd = crate::fd::oxidebsd_alloc_fd();
    PIPE_ENDS.lock().insert(read_fd, (pipe_id, End::Read));
    PIPE_ENDS.lock().insert(write_fd, (pipe_id, End::Write));
    crate::fd::oxidebsd_register_fd_ops(read_fd, pipe_read, write_denied, pipe_close);
    crate::fd::oxidebsd_register_fd_ops(write_fd, read_denied, pipe_write, pipe_close);

    // SAFETY: same known pointer-validation gap every other user-memory write in this codebase
    // already has -- fds_ptr isn't checked against the caller's actual mappings first.
    unsafe {
        (fds_ptr as *mut i32).write(read_fd as i32);
        (fds_ptr as *mut i32).add(1).write(write_fd as i32);
    }
    Ok(0)
}

extern "C" fn write_denied(_real_fd: u64, _ptr: u64, _len: u64) -> i64 {
    -EBADF
}

extern "C" fn read_denied(_real_fd: u64, _ptr: u64, _len: u64) -> i64 {
    -EBADF
}

extern "C" fn pipe_read(real_fd: u64, ptr: u64, len: u64) -> i64 {
    let Some(&(pipe_id, end)) = PIPE_ENDS.lock().get(&real_fd) else {
        return -EBADF;
    };
    debug_assert_eq!(
        end,
        End::Read,
        "pipe_read called against a pipe's write end"
    );

    loop {
        {
            let mut pipes = PIPES.lock();
            let pipe = pipes
                .get_mut(&pipe_id)
                .expect("pipe_read: pipe id missing from PIPES");
            if !pipe.data.is_empty() {
                let n = (len as usize).min(pipe.data.len());
                // SAFETY: same known pointer-validation gap sys_read/sys_write already document.
                let buf = unsafe { core::slice::from_raw_parts_mut(ptr as *mut u8, n) };
                for slot in buf.iter_mut() {
                    *slot = pipe.data.pop_front().unwrap();
                }
                return n as i64;
            }
            if pipe.write_closed {
                return 0; // EOF: no data, and nothing left to ever write more
            }
            // Empty, write end still open -- block and let something else run (see this module's
            // own doc comment for why this can't just return Ok(0)/EAGAIN instead).
            let caller = scheduler::current_pid();
            let mut table = process::table().lock();
            table.get_mut(&caller).unwrap().state =
                ProcState::Blocked(BlockReason::WaitingForPipeData(pipe_id));
        } // every lock dropped before schedule() -- see process::table()'s own doc comment
        scheduler::schedule();
        // Woken by pipe_write/pipe_close (write end closing counts too -- see below) -- loop back
        // and re-check from the top.
    }
}

extern "C" fn pipe_write(real_fd: u64, ptr: u64, len: u64) -> i64 {
    let Some(&(pipe_id, end)) = PIPE_ENDS.lock().get(&real_fd) else {
        return -EBADF;
    };
    debug_assert_eq!(
        end,
        End::Write,
        "pipe_write called against a pipe's read end"
    );

    {
        let mut pipes = PIPES.lock();
        let pipe = pipes
            .get_mut(&pipe_id)
            .expect("pipe_write: pipe id missing from PIPES");
        if pipe.read_closed {
            return -(EPIPE as i64);
        }
        // SAFETY: same known pointer-validation gap sys_read/sys_write already document.
        let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
        pipe.data.extend(bytes.iter().copied());
    }
    wake_blocked_readers(pipe_id);
    len as i64
}

extern "C" fn pipe_close(real_fd: u64) -> i64 {
    let Some((pipe_id, end)) = PIPE_ENDS.lock().remove(&real_fd) else {
        return -EBADF;
    };
    let both_closed = {
        let mut pipes = PIPES.lock();
        let pipe = pipes
            .get_mut(&pipe_id)
            .expect("pipe_close: pipe id missing from PIPES");
        match end {
            End::Read => pipe.read_closed = true,
            End::Write => pipe.write_closed = true,
        }
        pipe.read_closed && pipe.write_closed
    };
    if end == End::Write {
        // A blocked reader needs to wake up and re-check even though no new data arrived -- it's
        // waiting to see write_closed flip to true (EOF), not just for data.
        wake_blocked_readers(pipe_id);
    }
    if both_closed {
        PIPES.lock().remove(&pipe_id);
    }
    0
}

fn wake_blocked_readers(pipe_id: u64) {
    let mut table = process::table().lock();
    for (&pid, proc) in table.iter_mut() {
        if proc.state == ProcState::Blocked(BlockReason::WaitingForPipeData(pipe_id)) {
            proc.state = ProcState::Ready;
            scheduler::enqueue_ready(pid);
        }
    }
}
