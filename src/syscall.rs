//! OxideBSD's native syscall ABI: `SYSCALL`/`SYSRETQ`, syscall number in `RAX`, up to four
//! arguments in `RDI`/`RSI`/`RDX`/`R10`, success/failure signaled via the **carry flag** — the
//! traditional BSD (and general historical x86 Unix) convention, layered on top of the modern
//! fast-syscall instruction pair instead of the legacy `int 0x80` software-interrupt gate this
//! kernel used up through its first process/scheduler milestone. On success, `CF = 0` and `RAX`
//! holds the return value; on failure, `CF = 1` and `RAX` holds the *positive* `errno`. Register
//! placement (`RDI`/`RSI`/`RDX`/`R10`, avoiding `RCX`/`R11` since `SYSCALL` itself clobbers them
//! to save `RIP`/`RFLAGS`) mirrors real BSD's own `SYSCALL`-based convention.
//!
//! **`R10` used to be reserved but unread** — the entry stub always pushed it (uniform GPR
//! save/restore, see `SyscallFrame`'s own doc comment), but `syscall_dispatch`/`dispatch` only
//! ever forwarded `RDI`/`RSI`/`RDX` to a handler. Wired up for real once `SYS_EXECVE` needed a 4th
//! argument (`envp_ptr`, alongside the existing `path_ptr`/`path_len`/`argv_ptr`) to support real
//! `envp` passthrough — see `CLAUDE.md`'s BusyBox section. `SyscallHandler` is now a 4-argument
//! function pointer; every registered handler across every module gained a 4th parameter (ignored
//! by every syscall except `execve`), not just `execve`'s own — a real 4th argument is now a
//! permanent part of this ABI, available to any future syscall that needs one, not a one-off
//! special case threaded through only where `execve` needed it.
//!
//! **This mechanism used to be split across two files**: this module's own `int 0x80` gate, and a
//! separate `src/linux_syscall.rs` that proved the `SYSCALL`/`SYSRETQ` mechanism in isolation
//! (Linux's numbering, negative-`RAX` error convention — deliberately different from this ABI, as
//! a stepping stone toward eventually running unmodified Linux binaries). That plan changed: this
//! kernel is instead porting musl to speak *this* ABI directly (see `CLAUDE.md`'s "musl" section),
//! so there was no longer a reason to keep two different syscall-numbering/error conventions each
//! tied to a different trap instruction. `IA32_LSTAR` can only point at one entry stub, so this
//! ABI now **owns** the `SYSCALL`/`SYSRETQ` mechanism outright — `src/linux_syscall.rs` and its
//! dedicated `userland/linux-syscall-smoke/` test are gone, having already served their purpose of
//! proving the mechanism (`IA32_STAR`/`LSTAR`/`SFMASK` setup, the GDT segment-ordering
//! requirement, the stack-switch-on-entry problem) works at all.
//!
//! Syscall numbers match real FreeBSD's long-stable values for the calls implemented so far, as a
//! deliberate nod to authenticity — not a claim of binary compatibility with real BSD userland
//! (newer syscalls this ABI invents for itself, e.g. `mmap`/`brk`/TLS-base-set, don't extend that
//! convention — see `modules/native_abi/`). errno values are *mostly* shared across Linux and the
//! BSDs (`EBADF`, `EINVAL` are identical), but not universally — `ENOSYS` is 38 on Linux, 78 on
//! FreeBSD; this module uses the FreeBSD value.
//!
//! **The number → handler mapping (`dispatch`'s table) is populated by a dynamically loaded
//! kernel module, not hardcoded here.** `modules/native_abi/` registers `SYS_EXIT`/`SYS_READ`/
//! `SYS_WRITE`/etc. via `oxidebsd_register_syscall` from its own `module_init` — see `CLAUDE.md`'s
//! module-loading section. What stays kernel-resident, deliberately *not* moved into that module,
//! is the actual `sys_exit`/`sys_read`/`sys_write` *behavior* below. `oxidebsd_sys_exit`/
//! `oxidebsd_sys_read`/`oxidebsd_sys_write` near the bottom of this file are the thin FFI adapters
//! the module actually calls through.

use alloc::collections::{BTreeMap, BTreeSet};
use core::arch::global_asm;
use core::sync::atomic::{AtomicPtr, Ordering};

use spin::Mutex;
use x86_64::VirtAddr;
use x86_64::registers::model_specific::{Efer, EferFlags, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;

use crate::gdt;
use crate::serial_println;

/// Standard, POSIX-heritage errno values. `EBADF`/`EINVAL`/`ECHILD`/`ENOEXEC`/`EPIPE` happen to be
/// identical on Linux and the BSDs; `ENOSYS` is not (see module doc comment) — this is FreeBSD's
/// real value.
pub(crate) const EBADF: u64 = 9;
pub(crate) const EINVAL: u64 = 22;
pub(crate) const ENOSYS: u64 = 78;
pub(crate) const ECHILD: u64 = 10;
pub(crate) const ENOEXEC: u64 = 8;
pub(crate) const ENOMEM: u64 = 12;
/// Returned by `crate::pipe`'s `pipe_write` once a pipe's read end has been closed.
pub(crate) const EPIPE: u64 = 32;
/// Returned by `sys_kill` when the target pid doesn't exist -- "no such process," identical on
/// Linux and the BSDs.
pub(crate) const ESRCH: u64 = 3;
/// Returned by `sys_ioctl` for a tty-specific request issued against a non-console fd -- identical
/// on Linux and the BSDs, same as most of this group.
pub(crate) const ENOTTY: u64 = 25;

/// A registered syscall handler's own FFI return convention: negative is `-errno`, non-negative
/// is the success value. Deliberately distinct from the public syscall ABI's own carry-flag
/// convention (see this file's module doc comment) — it's purely this internal module↔kernel
/// registration boundary's own shape, chosen because it's representable in a plain scalar FFI
/// return without a `#[repr(C)]` result struct.
pub type SyscallHandler = extern "C" fn(u64, u64, u64, u64) -> i64;

static SYSCALL_TABLE: Mutex<BTreeMap<u64, SyscallHandler>> = Mutex::new(BTreeMap::new());

/// Registers `handler` for `number` in the table `dispatch` consults. `pub`, not `pub(crate)`:
/// the primary caller is a loaded module's `module_init` (currently `modules/native_abi/`,
/// crossing the module-relocation FFI boundary) populating what used to be `dispatch`'s own
/// hardcoded `match`, but integration tests under `tests/` (a separate crate linking against this
/// one) also need to register test-only syscall numbers directly — see `tests/fork_wait.rs`'s
/// `SYS_TEST_EXIT` handler, which sidesteps the fact that `scheduler::start`/`process::do_exit`
/// never return control to a test's own `main` the way a normal QEMU-exit-based test does.
/// Returns `0` on success, `-1` if `number` is already registered: nothing registers the same
/// number twice today, but silently overwriting a handler would be a far more confusing failure
/// mode than refusing outright.
pub extern "C" fn oxidebsd_register_syscall(number: u64, handler: SyscallHandler) -> i32 {
    let mut table = SYSCALL_TABLE.lock();
    if table.contains_key(&number) {
        return -1;
    }
    table.insert(number, handler);
    0
}

/// RFLAGS bit 0.
const CARRY_FLAG: u64 = 1;

/// Configures `SYSCALL`/`SYSRETQ`: `IA32_STAR` (from the real GDT selectors — `src/gdt.rs`'s
/// segment ordering exists specifically to satisfy `SYSRETQ`'s fixed-offset selector-reconstruction
/// scheme; `Star::write` validates this and fails loudly rather than silently misprogramming it),
/// `IA32_LSTAR` (this file's own `syscall_entry`), `IA32_SFMASK` (clears `RFLAGS::INTERRUPT_FLAG`
/// on entry, same as the old `int 0x80` gate did), and `EFER.SCE` — without which `SYSCALL` raises
/// `#UD` (handled, fatally, by `invalid_opcode_handler`), so forgetting this step fails loudly.
pub fn init() {
    serial_println!("[boot] configuring SYSCALL/SYSRETQ (native ABI)");

    // SAFETY: kernel_code_selector/kernel_data_selector are DPL 0, user_code_selector/
    // user_data_selector are DPL 3, and src/gdt.rs lays the GDT out specifically so their offsets
    // satisfy Star::write's own validation -- an error here means the GDT layout regressed.
    Star::write(
        gdt::user_code_selector(),
        gdt::user_data_selector(),
        gdt::kernel_code_selector(),
        gdt::kernel_data_selector(),
    )
    .expect("IA32_STAR: GDT layout doesn't satisfy SYSCALL/SYSRETQ's fixed offset scheme");

    let entry_addr = VirtAddr::new(syscall_entry as *const () as u64);
    LStar::write(entry_addr);
    SFMask::write(RFlags::INTERRUPT_FLAG);

    // SAFETY: STAR/LSTAR/SFMASK are all configured above; enabling SCE now is what actually makes
    // SYSCALL start dispatching to syscall_entry instead of raising #UD.
    unsafe { Efer::update(|flags| flags.insert(EferFlags::SYSTEM_CALL_EXTENSIONS)) };

    serial_println!("[boot] SYSCALL/SYSRETQ ready");
}

unsafe extern "C" {
    /// Defined in the `global_asm!` block below: switches onto the current process's own kernel
    /// stack (`SYSCALL` doesn't do this automatically the way an interrupt gate + TSS `RSP0`
    /// does), saves every general-purpose register plus the user's own `RSP`, calls
    /// `syscall_dispatch` with a pointer to them, restores everything, and `SYSRETQ`s back to
    /// whatever issued `SYSCALL`.
    pub fn syscall_entry();
}

/// The saved register state `syscall_entry` hands to `syscall_dispatch`, as a single pointer
/// (`RDI`, System V's first argument register) rather than loose arguments — `RAX`-for-number and
/// `RDI`/`RSI`/`RDX`-for-args don't line up with System V's own `call` convention closely enough
/// to just pass straight through, and this shape doubles as the mechanism for the carry-flag
/// trick below. Field order matches the entry stub's push order exactly (last pushed = lowest
/// address = first field). Two fields do double duty, both forced by `SYSCALL`'s own hardware
/// contract rather than a choice made here: `rcx` holds the user `RIP` to resume at (`SYSCALL`
/// clobbers real `RCX` with it on entry, so whatever the user program's actual `RCX` held before
/// the call is unrecoverable — the same reason every userland syscall stub declares `RCX`
/// clobbered), and `r11` holds the user `RFLAGS` (same story) — `SYSRETQ` reads both back
/// directly from registers, not from memory, so `syscall_dispatch` flips bit 0 of the saved `r11`
/// value to signal `CF` exactly the way it used to flip a dedicated `rflags` field for `iretq`.
/// `user_rsp` is the one genuinely new field: `SYSCALL` doesn't switch stacks the way an
/// interrupt gate does, so the entry stub has to save/restore the user's `RSP` itself (see
/// `gdt::CURRENT_RSP0`), and unlike `RCX`/`R11` there's no GPR slot already carrying it.
// pub(crate), not private: src/context_switch.rs needs to name this type (to size a fork child's
// seeded stack region and to type its `parent_frame`/`dst` pointers) without being able to touch
// its fields directly -- field access stays private to this module; only `copy_frame_for_fork`/
// `redirect_frame`/`current_frame` below cross that boundary, deliberately narrow.
//
// `Clone`/`Copy`: lets `process::Process` hold a `signal_saved_frame: Option<SyscallFrame>`
// snapshot (moved/copied by value, never needing field access outside this module) for signal
// delivery/`sigreturn` -- see `deliver_pending_signal`/`do_sigreturn` below.
#[derive(Clone, Copy)]
#[repr(C)]
pub(crate) struct SyscallFrame {
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
    user_rsp: u64,
}

/// The in-flight syscall's own `SyscallFrame`, valid only between `syscall_dispatch` storing it
/// and returning. `SyscallHandler`'s `(u64, u64, u64) -> i64` signature can't carry a frame
/// pointer, but `sys_fork`/`sys_execve` (`src/process.rs`) both need raw access to the live frame
/// (fork copies it into the child's own kernel stack; execve overwrites `rip`/`rsp` in place) —
/// this is a narrow, explicit exception for those two, not a signature change for every syscall.
/// Plain `AtomicPtr`, not a lock: this is single-core and `SYSFMASK` clears `RFLAGS::INTERRUPT_FLAG`
/// on every `SYSCALL` entry, so nothing can preempt a syscall in progress to observe a
/// half-updated value (same reasoning `src/stdin.rs`'s ring buffer doc comment already relies on).
static CURRENT_FRAME: AtomicPtr<SyscallFrame> = AtomicPtr::new(core::ptr::null_mut());

/// The currently in-flight syscall's frame — only valid to call from within a syscall handler
/// (i.e. from code reachable through `dispatch`), and only for as long as that handler is still
/// running.
pub(crate) fn current_frame() -> *mut SyscallFrame {
    CURRENT_FRAME.load(Ordering::Relaxed)
}

/// Copies `*src` into `*dst` (byte-for-byte, all 16 fields), then forces the copy's `rax` to `0`
/// *and* clears its `CARRY_FLAG` bit (in the copy's `r11` field, which doubles as the saved
/// `RFLAGS` — see `SyscallFrame`'s own doc comment) — used by `sys_fork` to seed a forked child's
/// kernel stack so its first-ever "return" looks exactly like returning from the same `fork()`
/// call the parent made, but with a clean success return (child pid `0`) of its own. Clearing
/// `CARRY_FLAG` explicitly matters, not just zeroing `rax`: at the moment this runs, `*src`'s
/// `r11` still holds whatever `CF` happened to be *before* the parent ever executed `SYSCALL` for
/// this `fork()` call (ordinary instructions like `mov` don't touch `EFLAGS`, so that bit is
/// really just leftover state from earlier in the parent's execution, not anything this syscall
/// itself set yet — `syscall_dispatch`'s own CF-clearing/setting for the *parent's* return happens
/// later, after `dispatch()`/`do_fork_from_current` returns, and only touches the parent's live
/// frame, never this copy). Without this, the child could spuriously see `Err` from a stale `CF`
/// bit that predates the call entirely. `dst` is raw, uninitialized stack memory (not yet a live
/// `SyscallFrame` reference), so this writes through pointers rather than going through `&mut`.
///
/// # Safety
///
/// `dst` must point at `size_of::<SyscallFrame>()` writable bytes; `src` must point at a valid,
/// fully-initialized `SyscallFrame`.
pub(crate) unsafe fn copy_frame_for_fork(dst: *mut SyscallFrame, src: *const SyscallFrame) {
    unsafe {
        core::ptr::copy_nonoverlapping(src, dst, 1);
        (*dst).rax = 0;
        (*dst).r11 &= !CARRY_FLAG;
    }
}

/// Redirects the live syscall frame at `frame` to resume execution at `rip` on `rsp` instead of
/// returning normally to the caller — used by `sys_execve` on success to hand the calling process
/// a whole new program image. Resets every GPR to `0` and the saved `RFLAGS` (`r11`) to
/// `usermode::USER_RFLAGS` (hygiene: a freshly exec'd program shouldn't see the old program's
/// register/flag state); doesn't touch `CS`/`SS` at all (there's no per-frame field for them any
/// more — `SYSRETQ` always reconstructs both from `IA32_STAR`), which is fine since `execve`
/// doesn't change privilege level anyway.
///
/// # Safety
///
/// `frame` must point at the currently in-flight syscall's live frame (i.e. `current_frame()`'s
/// return value, called from within that same syscall's handling).
pub(crate) unsafe fn redirect_frame(frame: *mut SyscallFrame, rip: VirtAddr, rsp: VirtAddr) {
    unsafe {
        let frame = &mut *frame;
        frame.r15 = 0;
        frame.r14 = 0;
        frame.r13 = 0;
        frame.r12 = 0;
        frame.r11 = 0;
        frame.r10 = 0;
        frame.r9 = 0;
        frame.r8 = 0;
        frame.rbp = 0;
        frame.rdi = 0;
        frame.rsi = 0;
        frame.rdx = 0;
        frame.rcx = 0;
        frame.rbx = 0;
        frame.rax = 0;
        frame.rcx = rip.as_u64();
        frame.r11 = crate::usermode::USER_RFLAGS;
        frame.user_rsp = rsp.as_u64();
    }
}

/// OxideBSD's own invention -- see `bits/syscall.h.in`'s own comment on the musl fork for why
/// this bypasses `SYSCALL_TABLE`/`dispatch` entirely (`do_sigreturn` below) rather than being
/// registered like every other syscall.
const SYS_SIGRETURN: u64 = 119;

#[unsafe(no_mangle)]
extern "C" fn syscall_dispatch(frame: *mut SyscallFrame) {
    // SAFETY: frame points at syscall_entry's just-pushed register block, on the current
    // process's own kernel stack, valid and exclusively ours for the duration of this call.
    let frame = unsafe { &mut *frame };

    // A real, narrow exception, not routed through the normal table/Ok-Err machinery below:
    // sigreturn must restore the interrupted context's own saved carry flag (in `r11`) bit for
    // bit, but the normal `Ok(value)`/`Err(errno)` convention can only ever force that bit to one
    // fixed polarity (clear on `Ok`, set on `Err`) -- neither can reproduce an arbitrary restored
    // value. See `do_sigreturn`'s own doc comment.
    if frame.rax == SYS_SIGRETURN {
        do_sigreturn(frame);
        return;
    }

    CURRENT_FRAME.store(frame as *mut SyscallFrame, Ordering::Relaxed);
    let result = dispatch(frame.rax, frame.rdi, frame.rsi, frame.rdx, frame.r10);
    match result {
        Ok(value) => {
            frame.rax = value;
            frame.r11 &= !CARRY_FLAG;
        }
        Err(errno) => {
            frame.rax = errno;
            frame.r11 |= CARRY_FLAG;
        }
    }
    CURRENT_FRAME.store(core::ptr::null_mut(), Ordering::Relaxed);

    // Every path back to userspace this kernel has funnels through here (a blocked-then-woken
    // process resumes by finishing the very syscall it blocked inside, same as any other) except
    // a never-run process's very first launch (`spawn_trampoline_inner`, which can't have a signal
    // pending before it's even executed once) -- so checking here, once, covers every real case.
    // See `deliver_pending_signal`'s own doc comment.
    deliver_pending_signal(frame);
}

/// Restores `*frame` from this process's own `Process::signal_saved_frame` (see
/// `deliver_pending_signal` below), byte for byte -- including `rax`/`r11`'s carry-flag bit, which
/// is exactly why this bypasses `syscall_dispatch`'s normal `Ok`/`Err` rewrite instead of being a
/// registered handler returning a plain `Result` like every other syscall. If nothing is actually
/// stashed (a spurious/duplicate call -- the trampoline this codebase installs only ever calls
/// this once, right after a real handler returns, so this should never happen in practice, but
/// nothing about a syscall number is trustworthy input), fails the call directly here (the normal
/// error-signaling convention, just applied by hand since the usual `dispatch()`/`Result` path
/// isn't reached for this number at all).
fn do_sigreturn(frame: &mut SyscallFrame) {
    let pid = crate::scheduler::current_pid();
    match crate::process::take_signal_saved_frame(pid) {
        Some(saved) => *frame = saved,
        None => {
            frame.rax = EINVAL;
            frame.r11 |= CARRY_FLAG;
        }
    }
}

/// Checked once, at the tail of every completed syscall (see `syscall_dispatch` above): if the
/// current process has a pending, unblocked signal, act on it now, right before control actually
/// returns to userspace. Real Unix semantics for the same reason: a signal only ever "arrives"
/// (steps a handler, or ends the process) between one instruction and the next of the process's
/// own userspace execution, never mid-syscall.
///
/// `SigDisposition::Terminate` calls `process::do_exit`, which never returns -- this function
/// itself can too, for that one case (it just never reaches its own tail).
/// `SigDisposition::Handler` rewrites `*frame` in place so the *next* thing this process's own
/// `sysretq` resumes into is the handler, not whatever userspace code the syscall originally
/// interrupted -- the interrupted state is snapshotted into `Process::signal_saved_frame` first
/// (see `process::stash_signal_context`), restored later by `do_sigreturn` once the handler
/// itself returns (via the trampoline `sa_restorer` names -- musl's own `__restore_rt`, patched to
/// call `SYS_SIGRETURN` -- see `bits/syscall.h.in`'s own comment on the musl fork).
///
/// Two known, deliberate simplifications:
/// - **No `SA_SIGINFO` support.** A handler is always invoked as `void (*)(int)` (`rdi = signum`
///   only, `rsi`/`rdx` zeroed) even if `SA_SIGINFO` was set and a real 3-argument
///   `void (*)(int, siginfo_t *, void *)` handler was installed -- there's no `siginfo_t`/
///   `ucontext_t` construction anywhere in this file. A real `SA_SIGINFO` handler that
///   dereferences its `info`/`ucontext` arguments would fault on the `NULL` this hands it.
/// - **`Process::signal_saved_frame` holds exactly one snapshot, not a real signal stack.** If a
///   *second*, different (unblocked) signal becomes deliverable while already inside a handler --
///   e.g. the handler itself issues a syscall, and that syscall's own tail finds another pending
///   signal -- this overwrites the first snapshot rather than nesting it, so the eventual
///   `sigreturn` from the *inner* handler restores into the *outer* handler's own interrupted
///   state, not back to the original pre-signal program -- a real correctness gap for nested
///   delivery specifically (single, non-nested signal handling was the only case exercised so
///   far: `kill.elf $$`-style default-terminate delivery, not a live handler-invocation round
///   trip -- see CLAUDE.md's own note on what was and wasn't boot-verified for this feature).
fn deliver_pending_signal(frame: &mut SyscallFrame) {
    let pid = crate::scheduler::current_pid();
    if pid == 0 {
        // Boot time (module_init self-checks, etc.) -- no real Process to carry signal state.
        return;
    }
    let Some(delivery) = crate::process::take_deliverable_signal(pid) else {
        return;
    };
    match delivery {
        crate::process::SignalDelivery::Terminate(code) => {
            crate::process::do_exit(pid, code);
        }
        crate::process::SignalDelivery::Handler {
            signum,
            handler,
            restorer,
            mask_to_add,
        } => {
            // Snapshotted *before* frame is mutated below -- this is the exact state the
            // interrupted syscall was about to resume into.
            crate::process::stash_signal_context(pid, *frame, mask_to_add);

            // 128 bytes of red-zone headroom (the interrupted code may have live data there,
            // System V's own red-zone convention this ABI otherwise never has to think about),
            // then 16-byte-align down, then back off 8 more bytes so the slot this writes to
            // lands exactly where an ordinary `call`'s own implicit return-address push would --
            // i.e. RSP%16==8 at the handler's own entry, matching System V's calling convention.
            let mut sp = frame.user_rsp.wrapping_sub(128);
            sp &= !0xF;
            sp = sp.wrapping_sub(8);
            // SAFETY: same known pointer-validation gap every other user-memory write in this
            // file already has -- sp is derived from this process's own live user_rsp, and this
            // process's own address space is the one currently active (signals are only ever
            // delivered to the process that's actually running right now).
            unsafe { (sp as *mut u64).write(restorer) };

            frame.rdi = signum;
            frame.rsi = 0;
            frame.rdx = 0;
            frame.rcx = handler; // resume RIP
            frame.r11 = crate::usermode::USER_RFLAGS; // resume RFLAGS
            frame.user_rsp = sp;
        }
    }
}

/// Numbers `dispatch` has already logged an "unrecognized syscall" line for — see `dispatch`'s own
/// doc comment for why this exists: a real interactive session (`sh.elf` with no `-c`, run for
/// real rather than as a single smoke-tested command) calls the *same* missing syscall repeatedly
/// (concretely, `hush` re-issues `rt_sigaction`/`rt_sigprocmask` around every command it runs), and
/// logging it every single time drowns out the actual command output on the same serial console —
/// discovered by actually using this interactively, not by inspection.
static LOGGED_UNRECOGNIZED: Mutex<BTreeSet<u64>> = Mutex::new(BTreeSet::new());

/// The actual dispatch logic, kept separate from `syscall_dispatch`'s raw pointer/frame handling
/// so it's directly unit-testable (see the `test_syscall_dispatch_*` tests in `src/lib.rs`). A
/// pure lookup into `SYSCALL_TABLE` — no number is special-cased here anymore, they're all
/// registered externally by whatever module chose to claim them.
pub(crate) fn dispatch(
    number: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
) -> Result<u64, u64> {
    let handler = SYSCALL_TABLE.lock().get(&number).copied();
    match handler {
        Some(handler) => ffi_result_to_result(handler(arg0, arg1, arg2, arg3)),
        None => {
            // Only the first occurrence of a given number is logged -- see LOGGED_UNRECOGNIZED's
            // own doc comment. Still the intended tool for discovering what a program's startup
            // needs (every *distinct* unimplemented number still gets one line), just no longer at
            // the cost of spamming every repeat once real interactive use started producing many.
            if LOGGED_UNRECOGNIZED.lock().insert(number) {
                serial_println!("[boot] unrecognized syscall number {}", number);
            }
            Err(ENOSYS)
        }
    }
}

fn ffi_result_to_result(raw: i64) -> Result<u64, u64> {
    if raw < 0 {
        Err((-raw) as u64)
    } else {
        Ok(raw as u64)
    }
}

/// Reads up to `len` bytes into `ptr` from `fd` — a pure lookup into `crate::fd`'s registry now,
/// for *every* fd including 0/1/2 (see `src/fd.rs`'s module doc comment for why stdin/stdout/
/// stderr moved from being special-cased here into being ordinary, `dup2`-able registry entries:
/// stdin's own non-blocking-ring-buffer behavior lives in that file's `stdin_read` now, not here).
/// `EBADF` if `fd` isn't registered at all.
pub(crate) fn sys_read(fd: u64, ptr: u64, len: u64) -> Result<u64, u64> {
    match crate::fd::read(fd, ptr, len) {
        Some(raw) => ffi_result_to_result(raw),
        None => Err(EBADF),
    }
}

/// Writes `len` bytes at `ptr` to `fd` — a pure lookup into `crate::fd`'s registry now, for *every*
/// fd including 0/1/2 (see `src/fd.rs`'s module doc comment; stdout/stderr's own UTF-8-checked
/// `serial_print!` path lives in that file's `stdout_write` now, not here). `EBADF` if `fd` isn't
/// registered at all.
pub(crate) fn sys_write(fd: u64, ptr: u64, len: u64) -> Result<u64, u64> {
    match crate::fd::write(fd, ptr, len) {
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

/// `SYS_PIPE` (`105`) — unlike most of this ABI's own inventions, matches real `pipe(2)`'s wire
/// format exactly (a single pointer to a `[i32; 2]` the kernel fills in): there's no
/// argument-convention reason to invent anything different the way `open`/`execve` needed to (see
/// "musl port"/"BusyBox port" in CLAUDE.md). Delegates to `crate::pipe` for the real logic — a
/// genuinely new subsystem, needed once `sh` (BusyBox's `hush`) required real pipeline support;
/// see that module's own doc comment for why a pipe read needs to actually block (not just return
/// `Ok(0)`/`EAGAIN` the way `sys_read`'s stdin case does) for a pipeline to work at all on this
/// single-core, cooperatively-scheduled kernel.
pub(crate) fn sys_pipe(fds_ptr: u64) -> Result<u64, u64> {
    crate::pipe::do_pipe(fds_ptr)
}

/// `SYS_DUP2` (`106`) — matches real `dup2(2)`'s exact `(oldfd, newfd)` signature (no
/// argument-convention mismatch here either). Delegates to `crate::fd::dup2` — see that function's
/// own doc comment, and `src/fd.rs`'s module doc comment, for the refcount-aware fd-aliasing this
/// needs to actually work (not just copy function pointers around).
pub(crate) fn sys_dup2(oldfd: u64, newfd: u64) -> Result<u64, u64> {
    crate::fd::dup2(oldfd, newfd).map_err(|_| EBADF)
}

/// `SYS_DUP` (`125`) — matches real `dup(2)`'s exact single-argument `(oldfd)` signature.
/// Delegates to `crate::fd::dup` — see that function's own doc comment for why this exists at all
/// (BusyBox's `hush`, with `CONFIG_HUSH_JOB` on, needs it to set up `G_interactive_fd`).
pub(crate) fn sys_dup(oldfd: u64) -> Result<u64, u64> {
    crate::fd::dup(oldfd).map_err(|_| EBADF)
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
        .get_mut(&crate::scheduler::current_pid())
    {
        me.fs_base = base;
    }
    Ok(0)
}

/// `SYS_KILL` (`116`) — matches real `kill(2)`'s exact `(pid, sig)` wire format, same
/// "no argument-convention patch needed" story `sys_pipe`/`sys_dup2` already established.
/// Delegates to `process::do_kill` — see that function's own doc comment for what's and isn't
/// supported (no process-group/broadcast targeting, signals 1-31 only, no `EINTR` for a signal
/// that arrives while the target is already blocked on something else).
pub(crate) fn sys_kill(pid: u64, sig: u64) -> Result<u64, u64> {
    crate::process::do_kill(crate::scheduler::current_pid(), pid as i64, sig as i64)
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
    crate::process::do_sigaction(crate::scheduler::current_pid(), sig, act_ptr, oldact_ptr)
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
    crate::process::do_sigprocmask(crate::scheduler::current_pid(), how, set_ptr, oldset_ptr)
}

/// `SYS_SETPGID` (`120`) — matches real `setpgid(2)`'s exact `(pid, pgid)` wire format, same
/// "no argument-convention patch needed" story `sys_pipe`/`sys_dup2`/`sys_kill` already
/// established. Delegates to `process::do_setpgid` — see that function's own doc comment for the
/// real, documented simplification (no permission/session checks — this kernel has no uid model at
/// all yet).
pub(crate) fn sys_setpgid(pid: u64, pgid: u64) -> Result<u64, u64> {
    crate::process::do_setpgid(crate::scheduler::current_pid(), pid as i64, pgid as i64)
}

/// `SYS_GETPGID` (`121`) — matches real `getpgid(2)`'s exact `(pid)` wire format.
pub(crate) fn sys_getpgid(pid: u64) -> Result<u64, u64> {
    crate::process::do_getpgid(crate::scheduler::current_pid(), pid as i64)
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
/// answer (`TCGETS`/`TCSETS*`/`TIOCGWINSZ`/`TIOCSWINSZ`) are handled; anything else is `ENOTTY`,
/// logged the same way an unregistered syscall number already is, so a future need is discoverable
/// the same "boot it and read the log" way every other gap in this codebase was found.
///
/// **Only ever succeeds against the console** (`crate::fd::real_fd_of(fd)` resolving to stdin's or
/// stdout's own `real_fd`, `0`/`1` -- see `src/fd.rs`'s module doc comment for why checking `fd`
/// itself, rather than what it currently resolves to, would be wrong after a `dup2`), `ENOTTY`
/// otherwise. This is load-bearing, not incidental: musl's own `isatty(fd)`
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
    match crate::fd::real_fd_of(fd) {
        Some(0) | Some(1) => {}
        _ => return Err(ENOTTY),
    }

    match request {
        TCGETS => {
            let termios = crate::stdin::get_termios();
            // SAFETY: same known pointer-validation gap every other user-memory write in this
            // file already has -- argp isn't checked against the caller's actual mappings first.
            unsafe { *(argp as *mut crate::stdin::RawTermios) = termios };
            Ok(0)
        }
        TCSETS | TCSETSW | TCSETSF => {
            // SAFETY: same known pointer-validation gap as above, for a read this time.
            let termios = unsafe { *(argp as *const crate::stdin::RawTermios) };
            crate::stdin::set_termios(termios);
            Ok(0)
        }
        TIOCGWINSZ => {
            // SAFETY: same known pointer-validation gap as above.
            unsafe { *(argp as *mut RawWinsize) = FIXED_WINSIZE };
            Ok(0)
        }
        TIOCSWINSZ => Ok(0), // accepted, silently discarded -- nothing reads window size back out
        _ => {
            serial_println!("[boot] unrecognized ioctl request 0x{:x}", request);
            Err(ENOTTY)
        }
    }
}

/// Thin FFI adapters over `sys_read`/`sys_write` for `modules/native_abi/` to call — see this
/// file's module doc comment for why the underlying behavior stays here rather than being
/// duplicated into that module. Converts each function's `Result<u64, u64>` into `SyscallHandler`'s
/// plain `i64` FFI convention.
///
/// `oxidebsd_sys_exit` goes through `process::do_exit` — real, per-process termination that hands
/// control to whatever the scheduler picks next, only falling back to a full `hlt_loop()` when
/// nothing else is runnable.
pub(crate) extern "C" fn oxidebsd_sys_exit(code: u64) -> ! {
    crate::process::do_exit(crate::scheduler::current_pid(), code as i32)
}

pub(crate) extern "C" fn oxidebsd_sys_read(fd: u64, ptr: u64, len: u64) -> i64 {
    result_to_ffi(sys_read(fd, ptr, len))
}

pub(crate) extern "C" fn oxidebsd_sys_write(fd: u64, ptr: u64, len: u64) -> i64 {
    result_to_ffi(sys_write(fd, ptr, len))
}

pub(crate) extern "C" fn oxidebsd_sys_writev(fd: u64, iov_ptr: u64, iovcnt: u64) -> i64 {
    result_to_ffi(sys_writev(fd, iov_ptr, iovcnt))
}

pub(crate) extern "C" fn oxidebsd_sys_pipe(fds_ptr: u64) -> i64 {
    result_to_ffi(sys_pipe(fds_ptr))
}

pub(crate) extern "C" fn oxidebsd_sys_dup2(oldfd: u64, newfd: u64) -> i64 {
    result_to_ffi(sys_dup2(oldfd, newfd))
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

pub(crate) extern "C" fn oxidebsd_sys_setpgid(pid: u64, pgid: u64) -> i64 {
    result_to_ffi(sys_setpgid(pid, pgid))
}

pub(crate) extern "C" fn oxidebsd_sys_getpgid(pid: u64) -> i64 {
    result_to_ffi(sys_getpgid(pid))
}

pub(crate) extern "C" fn oxidebsd_sys_ioctl(fd: u64, request: u64, argp: u64) -> i64 {
    result_to_ffi(sys_ioctl(fd, request, argp))
}

/// Thin FFI adapters over `src/process.rs`'s `do_fork_from_current`/`do_wait4`/`do_execve`/
/// `do_getpid`/`do_mmap`/`do_munmap`/`do_brk` for `modules/native_abi/` to call — same pattern as
/// the exit/read/write adapters above, real logic kept kernel-side since module code can't use
/// `alloc`.
pub(crate) extern "C" fn oxidebsd_sys_fork() -> i64 {
    result_to_ffi(crate::process::do_fork_from_current())
}

pub(crate) extern "C" fn oxidebsd_sys_wait4(pid: u64, status_ptr: u64, options: u64) -> i64 {
    let _ = options; // no WNOHANG/etc support this pass -- always blocks until a match exists
    result_to_ffi(crate::process::do_wait4(
        crate::scheduler::current_pid(),
        pid as i64,
        status_ptr,
    ))
}

pub(crate) extern "C" fn oxidebsd_sys_execve(
    path_ptr: u64,
    path_len: u64,
    argv_ptr: u64,
    envp_ptr: u64,
) -> i64 {
    result_to_ffi(crate::process::do_execve(
        crate::scheduler::current_pid(),
        path_ptr,
        path_len,
        argv_ptr,
        envp_ptr,
    ))
}

pub(crate) extern "C" fn oxidebsd_sys_mmap(addr_hint: u64, len: u64, prot: u64) -> i64 {
    result_to_ffi(crate::process::do_mmap(
        crate::scheduler::current_pid(),
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
        crate::scheduler::current_pid(),
        addr,
    ))
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

// `static mut`, not `static`: only ever written/read by this file's own raw asm (`mov [X], rsp`,
// never through a Rust `&`/`&mut`), invisible to the optimizer either way -- same defensive
// treatment as `gdt::CURRENT_RSP0`. Transiently holds the user's `RSP` for the handful of
// instructions between "SYSCALL just landed, RSP is still the user's" and "pushed as the first
// field of this process's own SyscallFrame, on this process's own kernel stack" -- genuinely a
// single global for that brief window (this kernel is single-core and SFMASK keeps interrupts off
// for the whole entry sequence, so at most one syscall can be *entering* at once), but safe past
// that window regardless, since by the time any Rust code able to call scheduler::schedule() runs,
// the saved RSP already lives in the (per-process) SyscallFrame instead of here. A single global
// scratch slot living for the *entire* syscall the way src/linux_syscall.rs's old
// USER_RSP_SCRATCH did would not have been safe here: do_wait4 already blocks and reschedules
// mid-syscall, so a second process could enter its own syscall before the first one returns.
#[unsafe(no_mangle)]
static mut SYSCALL_RSP_SCRATCH: u64 = 0;

global_asm!(
    ".global syscall_entry",
    "syscall_entry:",
    // SYSCALL leaves RSP on the user's own stack (unlike an interrupt gate + TSS RSP0, there's no
    // automatic switch) -- stash it, then move onto *this process's own* kernel stack (mirrored
    // into CURRENT_RSP0 by gdt::set_kernel_stack on every context switch) before pushing anything.
    "mov [SYSCALL_RSP_SCRATCH], rsp",
    "mov rsp, [CURRENT_RSP0]",
    "push qword ptr [SYSCALL_RSP_SCRATCH]", // user_rsp -- see SyscallFrame's doc comment
    "push rax",
    "push rbx",
    "push rcx", // = user RIP (SYSCALL saved it here)
    "push rdx",
    "push rsi",
    "push rdi",
    "push rbp",
    "push r8",
    "push r9",
    "push r10",
    "push r11", // = user RFLAGS (SYSCALL saved it here)
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "mov rdi, rsp", // &mut SyscallFrame, System V's first argument register
    "call syscall_dispatch",
    // Labeled separately (not just a fallthrough) so src/context_switch.rs's fork trampoline can
    // jump straight here: a freshly forked child's kernel stack is seeded with a copy of its
    // parent's SyscallFrame (rax forced to 0), placed at exactly the stack offset this tail
    // expects, so "return from fork() with 0" and "return from any other syscall" are the same
    // code path. See CLAUDE.md's process/scheduler section.
    ".global syscall_return_tail",
    "syscall_return_tail:",
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
    "pop rsp", // user_rsp -- must be the very last pop, right before sysretq
    "sysretq",
);
