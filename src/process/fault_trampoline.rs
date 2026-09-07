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
//!
//! **A real, second consumer since `interrupts::timer_interrupt_handler` gained its own redirect
//! into this same page** (see `Process::preempted_resume`'s own doc comment): that path, unlike
//! the page-fault one, genuinely needs the interrupted instruction to resume *transparently* --
//! but `mov eax, SYS_FAULT_PUMP` unavoidably clobbers the live, real `RAX` the interrupted code was
//! relying on (the `SYSCALL` ABI leaves no other register to carry the syscall number in, and
//! `timer_interrupt_handler`'s own `extern "x86-interrupt"` entry has no Rust-visible GPR fields to
//! save it from beforehand -- the exact same limitation this module's own doc comment above already
//! explains for why this trampoline exists at all). Found live: an earlier version of this redirect
//! didn't account for this, and a stray default-disposition signal (e.g. `SIGCHLD`) landing on
//! `hush` mid-instruction silently stomped a live computation's `RAX`, corrupting real control flow
//! with no crash to point at it. Fixed by having the trampoline itself stash the real `RAX` to
//! `RAX_SCRATCH_OFFSET` (via `MOV moffs64, RAX`, before it's clobbered) -- `syscall_dispatch`'s own
//! `SYS_FAULT_PUMP` handling reads it back and restores `frame.rax` right alongside `rcx`/`user_rsp`/
//! `r11`, but only when `Process::preempted_resume` was actually `Some` (the ordinary page-fault
//! case has no real prior `RAX` worth preserving and leaves the scratch slot unread). Runs
//! unconditionally regardless of which path redirected here -- one store is cheap, and branching
//! inside a 4-instruction trampoline to skip it buys nothing.

use x86_64::VirtAddr;
use x86_64::structures::paging::{FrameAllocator, Mapper, Page, PageTableFlags, Size4KiB};

use crate::memory::with_frame_allocator;

/// One page, fixed, identical in every process's own address space -- directly below
/// `mm::MMAP_REGION_BASE` (`0x_2000_0000_0000`), a huge, otherwise entirely unused canonical gap
/// far from every other fixed region this codebase hands out (`module::MODULE_VA_BASE`/
/// `_REGION_CEILING`, userland ELF load bases, `mm::BRK_REGION_CEILING`, `USER_STACK_TOP`,
/// `lifecycle::INTERP_LOAD_BASE`, `fs::sysv_shm::SHM_REGION_BASE`).
pub const FAULT_TRAMPOLINE_VA: u64 = 0x_1FFF_FFFF_F000;

/// Real length, in bytes, of the trampoline's own instruction sequence (`map`'s own `code` array
/// below) -- `interrupts::timer_interrupt_handler` uses this to recognize "currently mid-trampoline"
/// and defer preemption/redirect decisions until the thread has actually left this tiny window; see
/// `RAX_SCRATCH_OFFSET`'s own doc comment for why that matters now.
pub const CODE_LEN: u64 = 19;

/// Offset within the trampoline's own page where the real, pre-clobber `RAX` is stashed -- well
/// past the ~19 bytes of real code, arbitrary otherwise (nothing else ever lives on this page).
/// One frame per address space, so no cross-*process* race is possible.
///
/// **Was documented (wrongly, once real threading landed) as also race-free across threads** --
/// "single-core, so no cross-thread race is possible". True only while every `AddressSpace` was
/// exclusive to one schedulable entity; real `CLONE_THREAD` (see "Real threading") makes this page
/// the *same physical frame* for every thread sharing that address space (`AddressSpace::share`'s
/// `Arc::clone`), and single-core doesn't prevent two threads from *interleaving* through it via
/// preemption -- only from running it *simultaneously*. A thread preempted between its own
/// `mov [scratch],rax` and `syscall` leaves a live, unconsumed value sitting in this shared cell;
/// if a sibling thread of the same tgid enters the trampoline before the first one resumes and
/// reads it back, the sibling's own store clobbers it, and the first thread resumes with the
/// *sibling's* `rax` instead of its own. Found live via `pthread_mutex_trylock/4-3.c` (a real
/// `CRASH(139)` on OxideBSD that runs clean on real musl+Linux): three threads of one process
/// hammering `kill()`-driven `SIGUSR1`/`SIGUSR2` at high frequency for a full second gave enough
/// timer ticks landing mid-trampoline for this to actually happen -- a stray corrupted `rax`
/// resuming right where a compiled array-index computation (`count_ope % (NSCENAR+2)`) was about
/// to turn into a pointer, producing a wild `pthread_mutex_t*` and a real page fault one call
/// later. Confirmed by temporarily disabling `timer_interrupt_handler`'s whole async-redirect path:
/// the crash became a clean `TIMEOUT` instead (the signal simply never got delivered), ruling out
/// every other candidate.
///
/// **Mitigated, not fixed outright** -- not by giving each thread its own scratch cell, but by
/// never letting a thread be preempted while `rip` is inside `[FAULT_TRAMPOLINE_VA,
/// FAULT_TRAMPOLINE_VA + CODE_LEN)` in the first place, closing *this exact* interleaving (this
/// shared cell, this window) at its source -- see `timer_interrupt_handler`'s own guard. A 16-run
/// A/B batch of `pthread_mutex_trylock/4-3.c` in isolation confirms a real, measured improvement:
/// 10/16 (62.5%) `CRASH(139)` *without* this guard,
/// 6/16 (37.5%) *with* it -- real, but leaves a second, unisolated interleaving path into this
/// same bug class. A live-traced instance of the residual crash showed a plausible, non-garbage
/// restored `rax` from this exact redirect/restore mechanism, followed *much* later (after several
/// further, individually-correct redirect/handler/sigreturn cycles logged in between) by a fault
/// with a corrupted `rdi` holding what looks like a stray code address -- meaning the residual
/// corruption isn't this same mechanism recurring, and doesn't come from `do_clone` remapping this
/// page either (confirmed it doesn't -- `AddressSpace::share`'s `Arc::clone` is the only sharing
/// mechanism, exactly as assumed above). Every GPR round-trips correctly through `syscall_entry`'s
/// own push/pop sequence and `SyscallFrame`'s whole-struct copy in `stash_signal_context`/
/// `take_signal_saved_frame` (both independently verified by direct reading, not just inference) --
/// so the remaining corruption source is still open. Left for a follow-up investigation with more
/// live tracing (a full GPR dump spanning several complete `deliver_pending_signal`
/// handler-invoke-and-return cycles leading up to a caught crash would be the next step, ideally
/// with minimal added instrumentation -- extra `serial_println!` calls measurably perturb timing
/// enough to mask the race in smaller batches) rather than guessed at further here.
pub const RAX_SCRATCH_OFFSET: u64 = 0x100;

/// Maps `FAULT_TRAMPOLINE_VA` into `mapper`'s own (not-yet-active) address space with real
/// `mov [FAULT_TRAMPOLINE_VA + RAX_SCRATCH_OFFSET], rax; mov eax, SYS_FAULT_PUMP; syscall; ud2`
/// bytes -- called once per fresh address space (`process::spawn` at boot, `do_execve` on every
/// exec; a forked child gets its own copy for free, same as every other user page, via
/// `AddressSpace::fork`'s existing full eager copy of `USER_ACCESSIBLE` content). **Now real
/// `WRITABLE`** -- the leading `RAX`-stashing store needs it (this kernel has no W^X enforcement
/// anywhere regardless, see CLAUDE.md's own note on `elf::load`, so this costs nothing).
///
/// # Errors
///
/// `Err(())` on real frame exhaustion -- same "a real resource limit must fail one syscall, not
/// panic the kernel" motivation as `process::KernelStack::new`/`AddressSpace::build_from_active`.
/// `do_execve`'s own caller propagates this as a real `ENOMEM`; `spawn`'s boot-time call site
/// still panics (no syscall caller to report to that early, matching every other boot-time
/// allocation site).
#[allow(clippy::result_unit_err)] // see AddressSpace::new's own identical allow.
pub fn map(mapper: &mut impl Mapper<Size4KiB>, phys_offset: VirtAddr) -> Result<(), ()> {
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(FAULT_TRAMPOLINE_VA));
    with_frame_allocator(|fa| {
        let frame = fa.allocate_frame().ok_or(())?;
        // SAFETY: frame was just allocated (unused, per BootInfoFrameAllocator's contract), and
        // page falls in this address space's own, not-yet-active, otherwise-unused VA range.
        let flush = unsafe {
            mapper.map_to(
                page,
                frame,
                PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE,
                fa,
            )
        };
        match flush {
            // Real, resource-exhaustion-triggerable: map_to's own internal page-table-structure
            // allocation (for this page's not-yet-existing PT/PD/PDPT entries) can hit the exact
            // same frame exhaustion this whole function exists to report as a real ENOMEM, not a
            // kernel panic -- same reasoning as the leaf frame allocation just above.
            Err(x86_64::structures::paging::mapper::MapToError::FrameAllocationFailed) => {
                return Err(());
            }
            // Anything else (ParentEntryHugePage/PageAlreadyMapped) is a real logic-invariant
            // violation, not a resource limit -- this VA is always fresh in a brand-new address
            // space, so hitting either means a future change broke that assumption, worth a loud
            // panic rather than a silently-wrong ENOMEM.
            Err(e) => panic!("failed to map the fault trampoline page: {e:?}"),
            Ok(flush) => flush.flush(),
        }
        let frame_ptr = (phys_offset + frame.start_address().as_u64()).as_mut_ptr::<u8>();
        let imm = (crate::syscall::SYS_FAULT_PUMP as u32).to_le_bytes();
        let scratch_addr = (FAULT_TRAMPOLINE_VA + RAX_SCRATCH_OFFSET).to_le_bytes();
        #[rustfmt::skip]
        let code: [u8; 19] = [
            0x48, 0xA3, scratch_addr[0], scratch_addr[1], scratch_addr[2], scratch_addr[3],
                        scratch_addr[4], scratch_addr[5], scratch_addr[6], scratch_addr[7],
                                              // mov [RAX_SCRATCH_OFFSET], rax -- see this
                                              // module's own doc comment
            0xB8, imm[0], imm[1], imm[2], imm[3], // mov eax, imm32
            0x0F, 0x05,                           // syscall
            0x0F, 0x0B,                           // ud2 -- see this module's own doc comment
        ];
        assert_eq!(code.len() as u64, CODE_LEN, "CODE_LEN drifted from the real trampoline encoding");
        // SAFETY: frame_ptr points at the whole, just-allocated, not-yet-active 4096-byte frame.
        unsafe {
            core::ptr::write_bytes(frame_ptr, 0, 4096);
            core::ptr::copy_nonoverlapping(code.as_ptr(), frame_ptr, code.len());
        }
        Ok(())
    })
}
