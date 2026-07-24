//! A small, fixed-capacity keyboard input buffer feeding `syscall::sys_read`.
//!
//! `keyboard_interrupt_handler` (`src/interrupts.rs`) pushes decoded ASCII bytes here as they
//! arrive; `read` below drains them, blocking for real if none are buffered yet. This is
//! deliberately a plain array-backed ring buffer, not `alloc::collections::VecDeque` — that would
//! need to allocate to grow, and while doing so from an interrupt handler is not actually unsound
//! here (see below), avoiding the question entirely is simpler than reasoning about it.
//!
//! **`read` genuinely blocks now — the same `Blocked` + `scheduler::schedule()` pattern
//! `crate::pipe`'s own blocking read already established — rather than returning `0` immediately
//! on an empty buffer.** This is exactly what makes `sh.elf` (BusyBox's `hush`), run with no `-c`
//! argument, able to actually read a line from the keyboard instead of seeing an instant EOF and
//! exiting: a real blocking `read()` on stdin, from `hush`'s own perspective, is indistinguishable
//! from a real OS's. `push_byte` wakes any process blocked on `BlockReason::WaitingForStdin` the
//! moment a byte arrives, the same way `crate::pipe::pipe_write` wakes a blocked pipe reader.
//! `stsh`'s own `read_byte` (a busy-poll loop around a call that used to always return `0`
//! immediately when empty) still works completely unmodified against a blocking `read` — each
//! call simply returns exactly when a byte becomes available instead of needing several
//! immediately-`0` polls first, so the busy-poll wrapper's own `spin_loop()` fallback just never
//! actually triggers in practice anymore.
//!
//! **Why this can safely block on a single core, when nothing but a *hardware* keyboard interrupt
//! can ever wake it:** it can't, unless something also makes sure interrupts actually get
//! re-enabled while this is waiting — see `scheduler::schedule()`'s own `wait_for_ready`, which is
//! the piece that had to change to make this sound at all (previously, "nothing runnable" meant
//! spinning forever with interrupts left masked, which would make a stdin-blocked process
//! unwakeable).
//!
//! **The shared `spin::Mutex` around the ring buffer itself still can't deadlock, but the reasoning
//! is narrower than it used to be.** `IA32_SFMASK` (see `src/syscall.rs::init`) clears `IF` on
//! every `SYSCALL` entry, so interrupts stay disabled for nearly the entire duration of any
//! syscall — including every spot in `read` below that actually touches `BUFFER`, which always
//! drops that lock before ever calling `scheduler::schedule()`. The one deliberate exception is
//! `wait_for_ready`'s own brief `sti; hlt` window, entered only once every lock this module (and
//! everything else on the blocking path) might hold has already been dropped — see that
//! function's own doc comment. The keyboard IRQ can still never preempt code that's actually
//! holding `BUFFER`'s lock; it's just no longer true that it can never preempt *any* syscall in
//! progress at all.

use spin::Mutex;

use crate::process::{self, BlockReason, ProcState};
use crate::scheduler;

const CAPACITY: usize = 256;

struct RingBuffer {
    data: [u8; CAPACITY],
    head: usize,
    len: usize,
}

impl RingBuffer {
    const fn new() -> Self {
        RingBuffer {
            data: [0; CAPACITY],
            head: 0,
            len: 0,
        }
    }

    fn push(&mut self, byte: u8) {
        if self.len == CAPACITY {
            // Simplest possible backpressure: drop the newest byte rather than grow or overwrite
            // unread data.
            return;
        }
        let tail = (self.head + self.len) % CAPACITY;
        self.data[tail] = byte;
        self.len += 1;
    }

    fn pop(&mut self) -> Option<u8> {
        if self.len == 0 {
            return None;
        }
        let byte = self.data[self.head];
        self.head = (self.head + 1) % CAPACITY;
        self.len -= 1;
        Some(byte)
    }
}

static BUFFER: Mutex<RingBuffer> = Mutex::new(RingBuffer::new());

/// Real musl x86_64 `struct termios` layout (`third_party/musl`'s `arch/generic/bits/termios.h`) —
/// `c_iflag`/`c_oflag`/`c_cflag`/`c_lflag` as `u32`s, `c_line` as one byte, `c_cc[32]`, then
/// `c_ispeed`/`c_ospeed` as `u32`s. `repr(C)` alone gives the right layout (60 bytes total,
/// `c_ispeed` naturally padded up to offset 52 after `c_line`+`c_cc`'s 33 bytes) — no explicit
/// padding field needed, this is exactly what a real C compiler already does for this field order.
/// `syscall::sys_ioctl` reads/writes this directly at a caller's `TCGETS`/`TCSETS*` pointer, the
/// same "cast a raw pointer straight to a `#[repr(C)]` struct" pattern `RawSigAction`/
/// `RawArgvEntry` already use elsewhere in this codebase.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct RawTermios {
    pub c_iflag: u32,
    pub c_oflag: u32,
    pub c_cflag: u32,
    pub c_lflag: u32,
    pub c_line: u8,
    pub c_cc: [u8; 32],
    pub c_ispeed: u32,
    pub c_ospeed: u32,
}

/// `c_lflag` bits this kernel actually consults — see `arch/generic/bits/termios.h` for the
/// complete real set. `ICANON` is stored (so `TCGETS` round-trips whatever was last set) but not
/// otherwise consulted: this kernel's own ring buffer has never done real canonical-mode
/// line-holdback (backspace/erase has always been a userland concern — see `userland/stsh/`'s own
/// `read_line`), so there's no existing "canonical" behavior for turning `ICANON` off to actually
/// change; it's already effectively raw-mode-shaped regardless of this bit. `ECHO` is the one bit
/// with a real, live effect — see `echo_enabled` below.
pub(crate) const ICANON: u32 = 0o000002;
pub(crate) const ECHO: u32 = 0o000010;
const ISIG: u32 = 0o000001;

/// A plausible "cooked mode" default (`ISIG | ICANON | ECHO`, real Unix convention for a freshly
/// opened terminal) — matters only in that it must be *something* self-consistent: nothing in this
/// kernel or the musl fork ever depends on exact default `c_cc`/speed values (every real caller
/// reads the current settings via `TCGETS` before modifying and restoring them via `TCSETS`, never
/// assumes a specific starting value beyond "cooked mode looks cooked").
const DEFAULT_TERMIOS: RawTermios = RawTermios {
    c_iflag: 0,
    c_oflag: 0,
    c_cflag: 0,
    c_lflag: ISIG | ICANON | ECHO,
    c_line: 0,
    c_cc: [0; 32],
    c_ispeed: 0,
    c_ospeed: 0,
};

/// The kernel-resident "current termios mode" the BusyBox gap analysis (see CLAUDE.md) calls for —
/// deliberately a single global, not per-process/per-session: this kernel has exactly one console
/// (no pty concept, no multiple sessions), the same simplification `CURRENT_DIR_CLUSTER` used to be
/// before `oxfs` made cwd per-process (see "oxfs filesystem module" in CLAUDE.md) — except there's
/// no equivalent fix available here yet, since there's nothing to scope a second console to.
static TERMIOS: Mutex<RawTermios> = Mutex::new(DEFAULT_TERMIOS);

/// `keyboard_interrupt_handler`'s own auto-echo (see `src/interrupts.rs`) consults this before
/// printing a typed character back to the console — real terminal convention: a program that
/// switches to raw mode with `ECHO` cleared (readline-style line editors, like `hush`'s own
/// `CONFIG_FEATURE_EDITING` would if enabled — see CLAUDE.md's BusyBox gap analysis) does its own
/// echoing, and a kernel that kept echoing on top of it would double every keystroke.
pub(crate) fn echo_enabled() -> bool {
    TERMIOS.lock().c_lflag & ECHO != 0
}

pub(crate) fn get_termios() -> RawTermios {
    *TERMIOS.lock()
}

pub(crate) fn set_termios(new: RawTermios) {
    *TERMIOS.lock() = new;
}

/// Called from `keyboard_interrupt_handler` for each decoded ASCII character. Also wakes any
/// process blocked in `read` below waiting for exactly this — see this module's own doc comment.
pub fn push_byte(byte: u8) {
    BUFFER.lock().push(byte);
    wake_blocked_readers();
}

fn wake_blocked_readers() {
    let mut table = process::table().lock();
    for (&pid, proc) in table.iter_mut() {
        if proc.state == ProcState::Blocked(BlockReason::WaitingForStdin) {
            proc.state = ProcState::Ready;
            scheduler::enqueue_ready(pid);
        }
    }
}

/// Drains up to `buf.len()` buffered bytes into `buf`, returning how many were actually
/// available. Blocks (see this module's own doc comment) if nothing is buffered yet, rather than
/// returning `0` immediately — returns as soon as at least one byte is available, possibly fewer
/// than `buf.len()` if that's all that's arrived so far (a real blocking `read`'s own "at least
/// one byte, don't wait to fill the whole request" semantics).
pub fn read(buf: &mut [u8]) -> usize {
    loop {
        {
            let mut buffer = BUFFER.lock();
            let mut n = 0;
            while n < buf.len() {
                match buffer.pop() {
                    Some(byte) => {
                        buf[n] = byte;
                        n += 1;
                    }
                    None => break,
                }
            }
            if n > 0 {
                return n;
            }
        } // BUFFER's lock dropped before touching the process table/scheduler below

        let caller = scheduler::current_pid();
        let mut table = process::table().lock();
        table.get_mut(&caller).unwrap().state = ProcState::Blocked(BlockReason::WaitingForStdin);
        drop(table); // every lock dropped before schedule() -- see process::table()'s own doc comment
        scheduler::schedule();
        // Woken by push_byte's wake_blocked_readers once a keystroke arrives -- loop back and
        // re-check from the top.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_case]
    fn test_ring_buffer_fifo_order() {
        let mut buffer = RingBuffer::new();
        buffer.push(b'a');
        buffer.push(b'b');
        buffer.push(b'c');
        assert_eq!(buffer.pop(), Some(b'a'));
        assert_eq!(buffer.pop(), Some(b'b'));
        assert_eq!(buffer.pop(), Some(b'c'));
        assert_eq!(buffer.pop(), None);
    }

    #[test_case]
    fn test_ring_buffer_drops_when_full() {
        let mut buffer = RingBuffer::new();
        for i in 0..CAPACITY {
            buffer.push(i as u8);
        }
        // One more push while already at capacity should be silently dropped, not overwrite the
        // oldest unread byte.
        buffer.push(0xff);
        assert_eq!(buffer.pop(), Some(0));
    }
}
