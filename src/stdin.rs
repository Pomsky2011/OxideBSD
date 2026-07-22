//! A small, fixed-capacity keyboard input buffer feeding `syscall::sys_read`.
//!
//! `keyboard_interrupt_handler` (`src/interrupts.rs`) pushes decoded ASCII bytes here as they
//! arrive; `sys_read` drains them. This is deliberately a plain array-backed ring buffer, not
//! `alloc::collections::VecDeque` — that would need to allocate to grow, and while doing so from
//! an interrupt handler is not actually unsound here (see below), avoiding the question entirely
//! is simpler than reasoning about it.
//!
//! **Why the shared `spin::Mutex` can't deadlock, even though it's touched from both the keyboard
//! IRQ handler and syscall code:** `IA32_SFMASK` (see `src/syscall.rs::init`) clears `IF` on every
//! `SYSCALL` entry, so interrupts are already disabled for the entire duration of any syscall. The
//! keyboard IRQ can never preempt a syscall in progress on this single core — the two sides of
//! this buffer are mutually exclusive by construction, not by anything the lock itself provides.

use spin::Mutex;

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

/// Called from `keyboard_interrupt_handler` for each decoded ASCII character.
pub fn push_byte(byte: u8) {
    BUFFER.lock().push(byte);
}

/// Drains up to `buf.len()` buffered bytes into `buf`, returning how many were actually
/// available. Non-blocking: returns `0` immediately if nothing is buffered yet.
pub fn read(buf: &mut [u8]) -> usize {
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
    n
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
