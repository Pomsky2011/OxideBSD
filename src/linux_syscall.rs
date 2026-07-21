//! A Linux-compatible `SYSCALL`/`SYSRET` entry point — separate from, and unrelated to, this
//! kernel's own `int 0x80`-based ABI in `src/syscall.rs`. Real x86_64 Linux binaries (which is
//! what a musl/BusyBox userland actually is) never use `int 0x80` — that's the 32-bit legacy
//! path, and musl's x86_64 target has no fallback to it. They use the dedicated `SYSCALL`
//! instruction: number in `RAX`, up to six arguments in `RDI`/`RSI`/`RDX`/`R10`/`R8`/`R9` (`R10`,
//! not `RCX`, for the 4th argument — `SYSCALL` itself clobbers `RCX`/`R11` to save `RIP`/`RFLAGS`,
//! which is exactly why Linux's convention avoids them), return value in `RAX`, and Linux's own
//! syscall numbering (`write` = 1, `exit` = 60, `exit_group` = 231, ...).
//!
//! This is deliberately scoped to the *mechanism* — see `CLAUDE.md`'s "Linux-compatible syscall
//! mechanism" section for what's intentionally not here yet (musl's startup needs many more
//! syscalls than the two implemented below; that's real follow-up work, not guessed at here).

use x86_64::VirtAddr;
use x86_64::registers::model_specific::{Efer, EferFlags, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;

use crate::gdt;
use crate::serial_println;

/// Linux's real x86_64 syscall numbers for the two calls implemented so far.
const SYS_WRITE: u64 = 1;
const SYS_EXIT: u64 = 60;
const SYS_EXIT_GROUP: u64 = 231;

/// Linux's real `ENOSYS` ("no such syscall") value, positive per the shared `Result<u64, u64>`
/// convention `src/syscall.rs`'s `sys_write`/`sys_exit` use — negated into `RAX` (Linux's actual
/// wire convention) down in `linux_syscall_dispatch`, same as any other error these return.
const ENOSYS: u64 = 38;

const SCRATCH_STACK_SIZE: usize = 4096 * 5;

/// Wraps the scratch stack to guarantee 16-byte alignment of its start (and therefore its
/// computed top) — `linux_syscall_entry` needs this to satisfy System V's "RSP is 16-aligned
/// immediately before `call`" requirement when it calls into `linux_syscall_dispatch`, and a bare
/// `[u8; N]` only guarantees 1-byte alignment.
#[repr(align(16))]
struct ScratchStack(
    // Never read through Rust: this is stack memory the CPU writes via push/pop in
    // linux_syscall_entry, invisible to dead-code analysis. Only its address matters.
    #[allow(dead_code)] [u8; SCRATCH_STACK_SIZE],
);

// `static mut`, not `static`: see the identical reasoning on gdt.rs's RSP0/IST stacks — this is
// only ever written through by the CPU executing `push`/`pop` in linux_syscall_entry, invisible
// to Rust's own aliasing analysis, so a plain `static` would be free to land in read-only .rodata.
static mut SCRATCH_STACK: ScratchStack = ScratchStack([0; SCRATCH_STACK_SIZE]);

/// Referenced by name (via `#[unsafe(no_mangle)]`) from the `global_asm!` entry stub below.
/// `SYSCALL` doesn't switch stacks the way an interrupt gate + TSS RSP0 does — control arrives at
/// `linux_syscall_entry` still on the *user's* stack, so the stub saves it here, switches to
/// `KERNEL_RSP_TOP`, and restores it before `sysretq`. This is a single global, not a per-CPU
/// slot (real kernels use `swapgs` + `IA32_KERNEL_GS_BASE` for that) — a legitimate simplification
/// as long as this kernel stays single-core; revisit if SMP ever happens.
#[unsafe(no_mangle)]
static mut USER_RSP_SCRATCH: u64 = 0;

/// The (fixed, computed once in `init`) top of `SCRATCH_STACK`. Also referenced by name from the
/// entry stub.
#[unsafe(no_mangle)]
static mut KERNEL_RSP_TOP: u64 = 0;

unsafe extern "C" {
    /// Defined in the `global_asm!` block below.
    fn linux_syscall_entry();
}

/// The saved register state `linux_syscall_entry` hands to `linux_syscall_dispatch`, as a single
/// pointer (System V's first integer argument, `RDI`) rather than trying to force `RAX` plus six
/// argument registers into one `extern "C"` call — Linux's argument registers
/// (`RDI`/`RSI`/`RDX`/`R10`/`R8`/`R9`) don't line up with System V's own parameter registers
/// (`RDI`/`RSI`/`RDX`/`RCX`/`R8`/`R9`, note `RCX` vs `R10`), so there's no clean way to just "call
/// straight through" the way `src/syscall.rs`'s `int 0x80` stub does. Field order matches the
/// entry stub's push order exactly (last pushed = lowest address = first field).
#[repr(C)]
struct SyscallFrame {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    r10: u64,
    r9: u64,
    r8: u64,
    rbp: u64,
    rdi: u64,
    rsi: u64,
    rdx: u64,
    rbx: u64,
    rax: u64,
    _r11: u64, // saved user RFLAGS, for sysretq -- not a dispatch argument
    _rcx: u64, // saved user RIP, for sysretq -- not a dispatch argument
}

/// Configures `SYSCALL`/`SYSRET`: `IA32_STAR` (from the real GDT selectors, validated by
/// `Star::write` itself against the fixed-offset scheme `src/gdt.rs` lays the GDT out for),
/// `IA32_LSTAR` (this module's entry stub), `IA32_SFMASK` (clears `RFLAGS::INTERRUPT_FLAG` on
/// entry, same as our interrupt gates already do), and `EFER.SCE` — without which `SYSCALL` raises
/// `#UD` (already handled, fatally, by `invalid_opcode_handler`), so forgetting this step fails
/// loudly rather than silently.
pub fn init() {
    serial_println!("[boot] configuring SYSCALL/SYSRET (Linux-compatible)");

    // SAFETY: kernel_code_selector/kernel_data_selector are DPL 0, user_code_selector/
    // user_data_selector are DPL 3, and src/gdt.rs lays the GDT out specifically so their offsets
    // satisfy Star::write's own validation -- an error here means the GDT layout regressed.
    Star::write(
        gdt::user_code_selector(),
        gdt::user_data_selector(),
        gdt::kernel_code_selector(),
        gdt::kernel_data_selector(),
    )
    .expect("IA32_STAR: GDT layout doesn't satisfy SYSCALL/SYSRET's fixed offset scheme");

    let entry_addr = VirtAddr::new(linux_syscall_entry as *const () as u64);
    LStar::write(entry_addr);
    SFMask::write(RFlags::INTERRUPT_FLAG);

    unsafe {
        let stack_top = VirtAddr::from_ptr(&raw mut SCRATCH_STACK) + SCRATCH_STACK_SIZE as u64;
        KERNEL_RSP_TOP = stack_top.as_u64();

        // SAFETY: STAR/LSTAR/SFMASK are all configured above; enabling SCE now is what actually
        // makes SYSCALL start dispatching to linux_syscall_entry instead of raising #UD.
        Efer::update(|flags| flags.insert(EferFlags::SYSTEM_CALL_EXTENSIONS));
    }

    serial_println!("[boot] SYSCALL/SYSRET ready");
}

#[unsafe(no_mangle)]
extern "C" fn linux_syscall_dispatch(frame: *mut SyscallFrame) {
    // SAFETY: frame points at linux_syscall_entry's just-pushed register block on
    // KERNEL_RSP_TOP-derived stack space, valid and exclusively ours for the duration of this
    // call.
    let frame = unsafe { &mut *frame };

    // src/syscall.rs's sys_write/sys_exit return Result<u64, u64> (Ok(value) / Err(positive
    // errno)) -- that's the shared, canonical representation; adapt it to Linux's own convention
    // here (negative value in RAX) rather than the BSD carry-flag convention src/syscall.rs's own
    // dispatcher uses for the exact same underlying functions.
    let result = match frame.rax {
        SYS_WRITE => crate::syscall::sys_write(frame.rdi, frame.rsi, frame.rdx),
        SYS_EXIT | SYS_EXIT_GROUP => crate::syscall::sys_exit(frame.rdi),
        other => {
            serial_println!("[boot] unrecognized Linux syscall number {}", other);
            Err(ENOSYS)
        }
    };
    frame.rax = match result {
        Ok(value) => value,
        Err(errno) => (-(errno as i64)) as u64,
    };
}

core::arch::global_asm!(
    ".global linux_syscall_entry",
    "linux_syscall_entry:",
    // SYSCALL leaves RSP on the user's own stack (unlike an interrupt gate + TSS RSP0, there's no
    // automatic switch) -- save it and move to our own, before touching anything else.
    "mov [USER_RSP_SCRATCH], rsp",
    "mov rsp, [KERNEL_RSP_TOP]",
    "sub rsp, 8", // padding: 15 pushes below is not itself a multiple of 16 bytes
    "push rcx",   // user RIP (SYSCALL saved it here)
    "push r11",   // user RFLAGS (SYSCALL saved it here)
    "push rax",
    "push rbx",
    "push rdx",
    "push rsi",
    "push rdi",
    "push rbp",
    "push r8",
    "push r9",
    "push r10",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "mov rdi, rsp", // &mut SyscallFrame, System V's first argument register
    "call linux_syscall_dispatch",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop r10",
    "pop r9",
    "pop r8",
    "pop rbp",
    "pop rdi",
    "pop rsi",
    "pop rdx",
    "pop rbx",
    "pop rax",    // dispatcher's return value
    "pop r11",    // -> RFLAGS for sysretq
    "pop rcx",    // -> RIP for sysretq
    "add rsp, 8", // undo the padding
    "mov rsp, [USER_RSP_SCRATCH]",
    "sysretq",
);
