//! A minimal syscall ABI: `int 0x80`, syscall number in `RDI`, up to three arguments in
//! `RSI`/`RDX`/`RCX`, return value in `RAX`.
//!
//! This convention (rather than the more traditional "number in `RAX`") is chosen specifically so
//! `syscall_entry` never has to shuffle registers before calling into Rust: the System V ABI
//! already passes a function's first four integer arguments in `RDI`, `RSI`, `RDX`, `RCX`, so
//! `syscall_dispatch(number, arg0, arg1, arg2)` receives them in exactly the registers a caller
//! already left them in.
//!
//! There is still no process abstraction or scheduler, so `sys_exit` doesn't "return" control to
//! anything — it logs the exit code and idles the whole system. See `CLAUDE.md`'s syscall ABI
//! section for the full picture, including the deliberately-unvalidated `sys_write` pointer.

use core::arch::global_asm;

use crate::{hlt_loop, serial_print, serial_println};

/// The IDT vector `int 0x80` traps to.
pub const SYSCALL_VECTOR: u8 = 0x80;

pub const SYS_EXIT: u64 = 0;
pub const SYS_WRITE: u64 = 1;

/// Returned by `syscall_dispatch` for an unrecognized syscall number, or by a syscall that itself
/// failed (e.g. `sys_write` with a bad file descriptor or non-UTF-8 bytes).
const SYSCALL_ERROR: u64 = u64::MAX;

unsafe extern "C" {
    /// Defined in the `global_asm!` block below: saves every general-purpose register, calls
    /// `syscall_dispatch`, restores everything except `RAX` (left holding the dispatcher's return
    /// value), and `iretq`s back to whatever issued `int 0x80`.
    pub fn syscall_entry();
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn syscall_dispatch(number: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    match number {
        SYS_EXIT => sys_exit(arg0),
        SYS_WRITE => sys_write(arg0, arg1, arg2),
        _ => SYSCALL_ERROR,
    }
}

/// Terminates the calling process. Since there's no scheduler to switch to something else, this
/// is the *whole system's* clean stopping point, not a per-process one: log the code and idle.
fn sys_exit(code: u64) -> ! {
    serial_println!("[boot] process exited with code {}", code as i64);
    hlt_loop();
}

/// Writes `len` bytes at `ptr` to `fd` (only `fd == 1`, "stdout", is supported — routed through
/// `serial_print!`, which already mirrors to VGA). Returns the byte count on success, or
/// `SYSCALL_ERROR`.
fn sys_write(fd: u64, ptr: u64, len: u64) -> u64 {
    const STDOUT: u64 = 1;
    if fd != STDOUT {
        return SYSCALL_ERROR;
    }

    // SAFETY: this does not validate that [ptr, ptr+len) is actually mapped and user-accessible
    // before dereferencing it -- see CLAUDE.md's syscall ABI section. A bad pointer page-faults,
    // which the existing page_fault_handler already handles safely (log + reboot).
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    match core::str::from_utf8(bytes) {
        Ok(s) => {
            serial_print!("{s}");
            len
        }
        Err(_) => SYSCALL_ERROR,
    }
}

global_asm!(
    ".global syscall_entry",
    "syscall_entry:",
    "push rax",
    "push rbx",
    "push rcx",
    "push rdx",
    "push rsi",
    "push rdi",
    "push rbp",
    "push r8",
    "push r9",
    "push r10",
    "push r11",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "call syscall_dispatch",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop r11",
    "pop r10",
    "pop r9",
    "pop r8",
    "pop rbp",
    "pop rdi",
    "pop rsi",
    "pop rdx",
    "pop rcx",
    "pop rbx",
    "add rsp, 8", // discard the stale saved rax -- the real return value is already in rax
    "iretq",
);
