//! OxideBSD's native syscall ABI: `int 0x80`, syscall number in `RAX`, up to three arguments in
//! `RDI`/`RSI`/`RDX`, success/failure signaled via the **carry flag** — the traditional BSD (and
//! general historical x86 Unix) convention, and deliberately different from
//! `src/linux_syscall.rs`'s Linux-compatible `SYSCALL`/`SYSRET` path, which returns a negative
//! value in `RAX` instead. On success, `CF = 0` and `RAX` holds the return value; on failure,
//! `CF = 1` and `RAX` holds the *positive* `errno`. Register placement
//! (`RDI`/`RSI`/`RDX`/reserving `R10` for a future 4th argument) mirrors how both Linux's and real
//! BSD's `SYSCALL`-based conventions place arguments — they independently converge on "mirror the
//! C calling convention, avoid `RCX`/`R11`" even though we reach this one via `int 0x80`, not
//! `SYSCALL`.
//!
//! Syscall numbers (`SYS_EXIT = 1`, `SYS_WRITE = 4`) match real FreeBSD's long-stable values for
//! these two calls, as a deliberate nod to authenticity — not a claim of binary compatibility with
//! real BSD userland. errno values are *mostly* shared across Linux and the BSDs (`EBADF`,
//! `EINVAL` are identical), but not universally — `ENOSYS` is 38 on Linux, 78 on FreeBSD; this
//! module uses the FreeBSD value, `src/linux_syscall.rs` uses the Linux one.
//!
//! There is still no process abstraction or scheduler, so `sys_exit` doesn't "return" control to
//! anything — it logs the exit code and idles the whole system.

use core::arch::global_asm;

use crate::{hlt_loop, serial_print, serial_println};

/// The IDT vector `int 0x80` traps to.
pub const SYSCALL_VECTOR: u8 = 0x80;

pub const SYS_EXIT: u64 = 1;
pub const SYS_WRITE: u64 = 4;

/// Standard, POSIX-heritage errno values. `EBADF`/`EINVAL` happen to be identical on Linux and the
/// BSDs; `ENOSYS` is not (see module doc comment) — this is FreeBSD's real value.
pub(crate) const EBADF: u64 = 9;
pub(crate) const EINVAL: u64 = 22;
pub(crate) const ENOSYS: u64 = 78;

/// RFLAGS bit 0.
const CARRY_FLAG: u64 = 1;

unsafe extern "C" {
    /// Defined in the `global_asm!` block below: saves every general-purpose register, calls
    /// `syscall_dispatch` with a pointer to them (plus the hardware-pushed interrupt frame that
    /// follows), restores everything, and `iretq`s back to whatever issued `int 0x80`.
    pub fn syscall_entry();
}

/// The saved register state `syscall_entry` hands to `syscall_dispatch`, as a single pointer
/// (`RDI`, System V's first argument register) rather than loose arguments — `RAX`-for-number and
/// `RDI`/`RSI`/`RDX`-for-args don't line up with System V's own `call` convention closely enough
/// to just pass straight through, and this shape doubles as the mechanism for the carry-flag
/// trick below. Field order matches the entry stub's push order exactly (last pushed = lowest
/// address = first field); the last five fields are the CPU's *own* automatic interrupt-frame
/// push (`InterruptStackFrame`'s fields, same order), which lands directly above our pushes in the
/// same contiguous stack region — no separate pointer needed to reach it.
#[repr(C)]
struct SyscallFrame {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    r11: u64,
    r10: u64,
    r9: u64,
    r8: u64,
    rbp: u64,
    rdi: u64,
    rsi: u64,
    rdx: u64,
    rcx: u64,
    rbx: u64,
    rax: u64,
    _rip: u64,
    _cs: u64,
    rflags: u64,
    _rsp: u64,
    _ss: u64,
}

#[unsafe(no_mangle)]
extern "C" fn syscall_dispatch(frame: *mut SyscallFrame) {
    // SAFETY: frame points at syscall_entry's just-pushed register block (plus the CPU's own
    // interrupt frame above it) on the current interrupt stack, valid and exclusively ours for
    // the duration of this call.
    let frame = unsafe { &mut *frame };

    let result = dispatch(frame.rax, frame.rdi, frame.rsi, frame.rdx);
    match result {
        Ok(value) => {
            frame.rax = value;
            frame.rflags &= !CARRY_FLAG;
        }
        Err(errno) => {
            frame.rax = errno;
            frame.rflags |= CARRY_FLAG;
        }
    }
}

/// The actual dispatch logic, kept separate from `syscall_dispatch`'s raw pointer/frame handling
/// so it's directly unit-testable (see `test_syscall_dispatch_rejects_unknown_number` in
/// `src/lib.rs`).
pub(crate) fn dispatch(number: u64, arg0: u64, arg1: u64, arg2: u64) -> Result<u64, u64> {
    match number {
        SYS_EXIT => sys_exit(arg0),
        SYS_WRITE => sys_write(arg0, arg1, arg2),
        _ => Err(ENOSYS),
    }
}

/// Terminates the calling process. Since there's no scheduler to switch to something else, this
/// is the *whole system's* clean stopping point, not a per-process one: log the code and idle.
/// Always "succeeds" in the sense that it never returns — not represented as `Result` since exit
/// can't meaningfully fail.
///
/// Shared with `src/linux_syscall.rs`'s `exit`/`exit_group` — the underlying "terminate" semantics
/// are identical regardless of which ABI a program called through.
pub(crate) fn sys_exit(code: u64) -> ! {
    serial_println!("[boot] process exited with code {}", code as i64);
    hlt_loop();
}

/// Writes `len` bytes at `ptr` to `fd` (only `fd == 1`, "stdout", is supported — routed through
/// `serial_print!`, which already mirrors to VGA). Returns the byte count on success, `EBADF` for
/// any other `fd`, or `EINVAL` if the bytes aren't valid UTF-8 (real `write` doesn't care about
/// UTF-8 at all; this is a self-imposed restriction of piping to a text-mode console).
///
/// Shared with `src/linux_syscall.rs`'s `write` — same known pointer-validation gap applies there
/// too.
pub(crate) fn sys_write(fd: u64, ptr: u64, len: u64) -> Result<u64, u64> {
    const STDOUT: u64 = 1;
    if fd != STDOUT {
        return Err(EBADF);
    }

    // SAFETY: this does not validate that [ptr, ptr+len) is actually mapped and user-accessible
    // before dereferencing it -- see CLAUDE.md's syscall ABI section. A bad pointer page-faults,
    // which the existing page_fault_handler already handles safely (log + reboot).
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    match core::str::from_utf8(bytes) {
        Ok(s) => {
            serial_print!("{s}");
            Ok(len)
        }
        Err(_) => Err(EINVAL),
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
    "mov rdi, rsp", // &mut SyscallFrame, System V's first argument register
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
    "pop rax", // dispatcher's return value or errno, written directly into this stack slot
    "iretq",
);
