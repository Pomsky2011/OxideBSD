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
//! Syscall numbers match real FreeBSD's long-stable values for the calls implemented so far, as a
//! deliberate nod to authenticity — not a claim of binary compatibility with real BSD userland.
//! errno values are *mostly* shared across Linux and the BSDs (`EBADF`, `EINVAL` are identical),
//! but not universally — `ENOSYS` is 38 on Linux, 78 on FreeBSD; this module uses the FreeBSD
//! value, `src/linux_syscall.rs` uses the Linux one.
//!
//! **The number → handler mapping (`dispatch`'s table) is populated by a dynamically loaded
//! kernel module, not hardcoded here.** `modules/native_abi/` registers `SYS_EXIT`/`SYS_READ`/
//! `SYS_WRITE` via `oxidebsd_register_syscall` from its own `module_init` — see `CLAUDE.md`'s
//! module-loading section. What stays kernel-resident, deliberately *not* moved into that module,
//! is the actual `sys_exit`/`sys_read`/`sys_write` *behavior* below: `src/linux_syscall.rs` calls
//! these same three functions directly (its own, separate `SYSCALL`/`SYSRET` mechanism, out of
//! scope for modularization here), so duplicating their logic into `native_abi` would either break
//! those direct calls or require converting `linux_syscall.rs` too. `oxidebsd_sys_exit`/
//! `oxidebsd_sys_read`/`oxidebsd_sys_write` near the bottom of this file are the thin FFI adapters
//! the module actually calls through.
//!
//! There is still no process abstraction or scheduler, so `sys_exit` doesn't "return" control to
//! anything — it logs the exit code and idles the whole system.

use alloc::collections::BTreeMap;
use core::arch::global_asm;

use spin::Mutex;

use crate::{hlt_loop, serial_print, serial_println};

/// The IDT vector `int 0x80` traps to.
pub const SYSCALL_VECTOR: u8 = 0x80;

/// Standard, POSIX-heritage errno values. `EBADF`/`EINVAL` happen to be identical on Linux and the
/// BSDs; `ENOSYS` is not (see module doc comment) — this is FreeBSD's real value.
pub(crate) const EBADF: u64 = 9;
pub(crate) const EINVAL: u64 = 22;
pub(crate) const ENOSYS: u64 = 78;

/// A registered syscall handler's own FFI return convention: negative is `-errno`, non-negative
/// is the success value. Deliberately distinct from *both* public syscall ABIs (`int 0x80`'s
/// carry-flag convention in this file, `SYSCALL`/`SYSRET`'s negative-`RAX` convention in
/// `src/linux_syscall.rs`) — it's purely this internal module↔kernel registration boundary's own
/// shape, chosen because it's representable in a plain scalar FFI return without a `#[repr(C)]`
/// result struct.
pub(crate) type SyscallHandler = extern "C" fn(u64, u64, u64) -> i64;

static SYSCALL_TABLE: Mutex<BTreeMap<u64, SyscallHandler>> = Mutex::new(BTreeMap::new());

/// Registers `handler` for `number` in the table `dispatch` consults — called by a loaded
/// module's `module_init` (currently just `modules/native_abi/`) to populate what used to be
/// `dispatch`'s own hardcoded `match`. Returns `0` on success, `-1` if `number` is already
/// registered: nothing registers the same number twice today, but silently overwriting a handler
/// would be a far more confusing failure mode than refusing outright.
pub(crate) extern "C" fn oxidebsd_register_syscall(number: u64, handler: SyscallHandler) -> i32 {
    let mut table = SYSCALL_TABLE.lock();
    if table.contains_key(&number) {
        return -1;
    }
    table.insert(number, handler);
    0
}

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
/// so it's directly unit-testable (see the `test_syscall_dispatch_*` tests in `src/lib.rs`). A
/// pure lookup into `SYSCALL_TABLE` — no number is special-cased here anymore, they're all
/// registered externally by whatever module chose to claim them.
pub(crate) fn dispatch(number: u64, arg0: u64, arg1: u64, arg2: u64) -> Result<u64, u64> {
    let handler = SYSCALL_TABLE.lock().get(&number).copied();
    match handler {
        Some(handler) => ffi_result_to_result(handler(arg0, arg1, arg2)),
        None => Err(ENOSYS),
    }
}

fn ffi_result_to_result(raw: i64) -> Result<u64, u64> {
    if raw < 0 {
        Err((-raw) as u64)
    } else {
        Ok(raw as u64)
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

/// Reads up to `len` bytes into `ptr` from `fd`. `fd == 0` ("stdin") is routed through
/// `crate::stdin`'s keyboard buffer, non-blocking (`Ok(0)` immediately if nothing is buffered yet
/// rather than waiting — see `CLAUDE.md`'s shell section for why). Any other `fd` is looked up in
/// `crate::fd`'s registry (populated by e.g. `modules/fat32/`'s open files) and delegated to
/// whichever module registered it; `EBADF` if nothing has.
///
/// Native-ABI only for now; `src/linux_syscall.rs` has no equivalent yet.
pub(crate) fn sys_read(fd: u64, ptr: u64, len: u64) -> Result<u64, u64> {
    const STDIN: u64 = 0;
    if fd == STDIN {
        // SAFETY: same known pointer-validation gap as sys_write -- [ptr, ptr+len) isn't checked
        // against the caller's actual mappings before we write through it.
        let buf = unsafe { core::slice::from_raw_parts_mut(ptr as *mut u8, len as usize) };
        return Ok(crate::stdin::read(buf) as u64);
    }

    match crate::fd::read(fd, ptr, len) {
        Some(raw) => ffi_result_to_result(raw),
        None => Err(EBADF),
    }
}

/// Writes `len` bytes at `ptr` to `fd`. `fd == 1` ("stdout") is routed through `serial_print!`
/// (which already mirrors to VGA), returning `EINVAL` if the bytes aren't valid UTF-8 (real
/// `write` doesn't care about UTF-8 at all; this is a self-imposed restriction of piping to a
/// text-mode console). Any other `fd` is looked up in `crate::fd`'s registry and delegated;
/// `EBADF` if nothing has registered it.
///
/// Shared with `src/linux_syscall.rs`'s `write` — same known pointer-validation gap applies there
/// too.
pub(crate) fn sys_write(fd: u64, ptr: u64, len: u64) -> Result<u64, u64> {
    const STDOUT: u64 = 1;
    if fd == STDOUT {
        // SAFETY: this does not validate that [ptr, ptr+len) is actually mapped and user-
        // accessible before dereferencing it -- see CLAUDE.md's syscall ABI section. A bad
        // pointer page-faults, which the existing page_fault_handler already handles safely (log
        // + reboot).
        let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
        return match core::str::from_utf8(bytes) {
            Ok(s) => {
                serial_print!("{s}");
                Ok(len)
            }
            Err(_) => Err(EINVAL),
        };
    }

    match crate::fd::write(fd, ptr, len) {
        Some(raw) => ffi_result_to_result(raw),
        None => Err(EBADF),
    }
}

/// Thin FFI adapters over `sys_exit`/`sys_read`/`sys_write` for `modules/native_abi/` to call —
/// see this file's module doc comment for why the underlying behavior stays here (shared with
/// `src/linux_syscall.rs`'s own direct calls to the very same three functions) rather than being
/// duplicated into that module. Converts each function's `Result<u64, u64>` (or, for `sys_exit`,
/// its unconditional divergence) into `SyscallHandler`'s plain `i64` FFI convention.
pub(crate) extern "C" fn oxidebsd_sys_exit(code: u64) -> ! {
    sys_exit(code)
}

pub(crate) extern "C" fn oxidebsd_sys_read(fd: u64, ptr: u64, len: u64) -> i64 {
    result_to_ffi(sys_read(fd, ptr, len))
}

pub(crate) extern "C" fn oxidebsd_sys_write(fd: u64, ptr: u64, len: u64) -> i64 {
    result_to_ffi(sys_write(fd, ptr, len))
}

fn result_to_ffi(result: Result<u64, u64>) -> i64 {
    match result {
        Ok(value) => value as i64,
        Err(errno) => -(errno as i64),
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
