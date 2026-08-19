//! A tiny, real, kernel-authored piece of user-executable code, mapped at a fixed VA in every
//! process's own address space, purely to make real signal-handler invocation from a hardware
//! page fault possible.
//!
//! **Why this exists at all**: `interrupts::page_fault_handler` runs as `extern "x86-interrupt"`,
//! whose compiler-generated entry/exit saves and restores every clobbered register itself, with no
//! Rust-visible field for them -- unlike `syscall::SyscallFrame`, which the `SYSCALL` entry stub
//! (`syscall_entry`) builds by hand, with every GPR as an explicit, mutable field.
//! `syscall::deliver_pending_signal` already knows how to redirect a live `SyscallFrame` into a
//! real, argument-correct handler invocation (`rdi`/`rsi`/`rdx`/`rcx`/`user_rsp` all set directly)
//! -- but a page fault has no `SyscallFrame` to redirect, only an `InterruptStackFrame` whose only
//! mutable fields are `instruction_pointer`/`stack_pointer`/`cpu_flags`/the segment selectors, not
//! GPRs.
//!
//! The fix: don't try to invoke the handler directly from the fault. Instead, `page_fault_handler`
//! records the real signal as pending on the faulting process (`process::do_kill`'s own
//! self-signal path -- just sets the bit, doesn't act on it yet) and redirects the interrupted
//! context's `instruction_pointer` to land *here* instead of resuming the faulting instruction.
//! These few bytes (`mov eax, SYS_FAULT_PUMP` / `syscall`) do nothing but force the process
//! straight back through a real `SYSCALL` instruction -- landing in `syscall_entry`, which captures
//! every GPR into a genuine `SyscallFrame`, and `syscall_dispatch`, which (for this specific,
//! never-issued-by-real-userland number) skips the normal dispatch table and calls
//! `deliver_pending_signal` directly. That function then finds the signal this same fault just
//! queued and redirects the *real* `SyscallFrame` -- the exact same machinery already proven
//! correct for `kill()`-shaped signal delivery, reused verbatim rather than duplicated.
//! `stack_pointer` is left untouched (the process's own real user stack, still valid -- the
//! trampoline itself never touches it, and `SYSCALL` doesn't require any particular stack
//! contents), and `cpu_flags`/the segment selectors don't need to change either (this kernel has
//! exactly one ring-3 code/data selector pair, already correct).
//!
//! A `ud2` tail is a real safety net, not decoration: it should never execute (a fault always
//! queues a real pending signal before redirecting here, and `deliver_pending_signal` always finds
//! *something* deliverable -- worst case, default-`Terminate` disposition, which calls
//! `process::do_exit` and never returns to `syscall_return_tail` at all), so actually reaching it
//! is a real bug worth a hard stop, not a silent spin.

use x86_64::VirtAddr;
use x86_64::structures::paging::{FrameAllocator, Mapper, Page, PageTableFlags, Size4KiB};

use crate::memory::with_frame_allocator;

/// One page, fixed, identical in every process's own address space -- directly below
/// `mm::MMAP_REGION_BASE` (`0x_2000_0000_0000`), a huge, otherwise entirely unused canonical gap
/// far from every other fixed region this codebase hands out (`module::MODULE_VA_BASE`/
/// `_REGION_CEILING`, userland ELF load bases, `mm::BRK_REGION_CEILING`, `USER_STACK_TOP`,
/// `lifecycle::INTERP_LOAD_BASE`, `fs::sysv_shm::SHM_REGION_BASE`).
pub const FAULT_TRAMPOLINE_VA: u64 = 0x_1FFF_FFFF_F000;

/// Maps `FAULT_TRAMPOLINE_VA` into `mapper`'s own (not-yet-active) address space with real
/// `mov eax, SYS_FAULT_PUMP; syscall; ud2` bytes -- called once per fresh address space
/// (`process::spawn` at boot, `do_execve` on every exec; a forked child gets its own copy for free,
/// same as every other user page, via `AddressSpace::fork`'s existing full eager copy of
/// `USER_ACCESSIBLE` content). Not `WRITABLE` -- this kernel has no W^X enforcement anywhere (see
/// CLAUDE.md's own note on `elf::load`), but there's no reason for this one page to need it
/// regardless.
pub fn map(mapper: &mut impl Mapper<Size4KiB>, phys_offset: VirtAddr) {
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(FAULT_TRAMPOLINE_VA));
    with_frame_allocator(|fa| {
        let frame = fa
            .allocate_frame()
            .expect("out of memory mapping the fault trampoline page");
        // SAFETY: frame was just allocated (unused, per BootInfoFrameAllocator's contract), and
        // page falls in this address space's own, not-yet-active, otherwise-unused VA range.
        unsafe {
            mapper
                .map_to(
                    page,
                    frame,
                    PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE,
                    fa,
                )
                .expect("failed to map the fault trampoline page")
                .flush();
        }
        let frame_ptr = (phys_offset + frame.start_address().as_u64()).as_mut_ptr::<u8>();
        let imm = (crate::syscall::SYS_FAULT_PUMP as u32).to_le_bytes();
        #[rustfmt::skip]
        let code: [u8; 9] = [
            0xB8, imm[0], imm[1], imm[2], imm[3], // mov eax, imm32
            0x0F, 0x05,                           // syscall
            0x0F, 0x0B,                           // ud2 -- see this module's own doc comment
        ];
        // SAFETY: frame_ptr points at the whole, just-allocated, not-yet-active 4096-byte frame.
        unsafe {
            core::ptr::write_bytes(frame_ptr, 0, 4096);
            core::ptr::copy_nonoverlapping(code.as_ptr(), frame_ptr, code.len());
        }
    });
}
