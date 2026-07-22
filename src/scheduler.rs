//! Picks which `Ready` process runs next and performs the actual switch: repointing `CR3`
//! (address space) and `TSS.RSP0` (via `gdt::set_kernel_stack` — the stack the CPU auto-switches
//! to on the next ring-3→ring-0 transition) before handing off to
//! `context_switch::switch_context`.
//!
//! Cooperative round-robin only: a process only ever leaves `Running` by calling `schedule()`
//! itself (via `process::do_exit`/`do_wait4`), never because a timer interrupt preempted it. The
//! deliberate, documented seam for a later preemptive scheduler: `interrupts::timer_interrupt_handler`
//! would need converting from an `extern "x86-interrupt" fn` to a raw asm entry point (the same
//! way `syscall::syscall_entry` already is) so it can hand off full GPR state before deciding to
//! switch — not implemented here.

use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicU64, Ordering};

use spin::Mutex;
use x86_64::instructions::interrupts::without_interrupts;

use crate::context_switch::switch_context;
use crate::hlt_loop;
use crate::process::{self, Pid, ProcState};
use crate::{gdt, serial_println};

/// `0` is never a valid `Pid` (`process::alloc_pid` starts at 1) — used as "no current process,"
/// true only before the very first `scheduler::start`.
static CURRENT_PID: AtomicU64 = AtomicU64::new(0);
static READY_QUEUE: Mutex<VecDeque<Pid>> = Mutex::new(VecDeque::new());

/// Discard slot for `switch_context`'s `old_rsp_slot` on the very first switch (`start`), which
/// has no real "previous process" to save an `rsp` into. `static mut`, not `static`, for the same
/// reason `gdt.rs`'s RSP0/IST stacks are: it's written only by hardware-adjacent asm (the `mov
/// [rdi], rsp` inside `switch_context`), never through a Rust-visible write, so a plain `static`
/// risks being interned into `.rodata` by the optimizer.
static mut BOOT_SCRATCH_RSP: u64 = 0;

pub fn current_pid() -> Pid {
    CURRENT_PID.load(Ordering::Relaxed)
}

pub fn enqueue_ready(pid: Pid) {
    READY_QUEUE.lock().push_back(pid);
}

/// Voluntarily gives up the CPU. If the caller is still `Ready` or `Running` (i.e. it didn't just
/// block or exit), it's re-enqueued so it gets another turn later — a caller that transitioned to
/// `Blocked`/`Zombie` just before calling this is deliberately *not* re-enqueued, which is how
/// `process::do_wait4`/`do_exit` actually suspend/terminate. Picks the next `Ready` process and
/// switches to it; falls back to `hlt_loop()` only if nothing is runnable at all. Returns once
/// this exact call site is switched back into — for a caller that just blocked, that means "a
/// later event woke it back to `Ready` and the scheduler picked it again."
pub fn schedule() {
    without_interrupts(|| {
        let prev_pid = current_pid();
        let has_prev = prev_pid != 0;

        if has_prev {
            let table = process::table().lock();
            let prev_state = table
                .get(&prev_pid)
                .expect("schedule: current process missing from table")
                .state;
            if matches!(prev_state, ProcState::Ready | ProcState::Running) {
                drop(table);
                enqueue_ready(prev_pid);
            }
        }

        // Nothing runnable at all: if the caller itself were still Ready/Running it would have
        // just been popped back out above, so reaching None here means the system is genuinely
        // idle.
        let next_pid = READY_QUEUE.lock().pop_front().unwrap_or_else(|| hlt_loop());

        if has_prev && next_pid == prev_pid {
            process::table().lock().get_mut(&prev_pid).unwrap().state = ProcState::Running;
            return;
        }

        let prev_rsp_slot: *mut u64 = if has_prev {
            let mut table = process::table().lock();
            let prev = table.get_mut(&prev_pid).unwrap();
            &mut prev.rsp as *mut u64
        } else {
            &raw mut BOOT_SCRATCH_RSP
        };

        let next_rsp = activate_and_prepare(next_pid);
        CURRENT_PID.store(next_pid, Ordering::Relaxed);

        // SAFETY: prev_rsp_slot is either a live Process's own `rsp` field or the dedicated boot
        // scratch slot; next_rsp was seeded by process::spawn/do_fork_from_current (a never-run
        // process) or saved by this exact process's own previous call to schedule() (a resumed
        // one) — both satisfy switch_context's stack-shape requirement. The whole function runs
        // under without_interrupts, so no timer IRQ can land between repointing RSP0 in
        // activate_and_prepare and switch_context actually moving execution onto the new stack.
        unsafe { switch_context(prev_rsp_slot, next_rsp) };
    });
}

/// The very first switch: boots the scheduler by "switching away" from the current boot stack
/// into `first_pid`, which must already be `Ready` in the process table (i.e. already
/// `process::spawn`'d). Never returns — same one-way shape `usermode::jump_to_usermode` always
/// had (and which this function reaches indirectly, via `spawn_trampoline_inner`).
pub fn start(first_pid: Pid) -> ! {
    serial_println!("[boot] scheduler starting: switching to pid {}", first_pid);
    without_interrupts(|| {
        let next_rsp = activate_and_prepare(first_pid);
        CURRENT_PID.store(first_pid, Ordering::Relaxed);
        // SAFETY: see schedule()'s own safety comment; the boot stack this runs on is abandoned
        // for good, exactly like usermode::jump_to_usermode's own one-way transition.
        unsafe { switch_context(&raw mut BOOT_SCRATCH_RSP, next_rsp) };
    });
    unreachable!("scheduler::start's switch_context should never return to the boot stack");
}

/// Marks `pid` `Running`, activates its address space (`CR3`), repoints `TSS.RSP0` at its own
/// kernel stack, and returns its saved `rsp` — the common tail shared by `schedule()` and
/// `start()` just before the actual `switch_context` call.
fn activate_and_prepare(pid: Pid) -> u64 {
    let mut table = process::table().lock();
    let next = table
        .get_mut(&pid)
        .expect("activate_and_prepare: pid missing from table");
    next.state = ProcState::Running;
    // SAFETY: next's AddressSpace carries the kernel's own mappings (shared by every process, per
    // AddressSpace::new's shallow copy) plus its own user segments/stack, so activating it here —
    // still running on the outgoing stack, about to switch away — is safe, mirroring
    // AddressSpace::activate's own safety contract.
    unsafe { next.address_space.activate() };
    gdt::set_kernel_stack(next.kernel_stack_top);
    next.rsp
}
