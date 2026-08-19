//! Picks which `Ready` process runs next and performs the actual switch: repointing `CR3`
//! (address space) and `TSS.RSP0` (via `gdt::set_kernel_stack` — the stack the CPU auto-switches
//! to on the next ring-3→ring-0 transition) before handing off to
//! `context_switch::switch_context`.
//!
//! **Real preemption**: a process leaves `Running` either voluntarily (calling `schedule()` itself
//! via `process::do_exit`/`do_wait4`/every other blocking primitive — unchanged) *or* because
//! `interrupts::timer_interrupt_handler` caught it executing ring-3 (user-mode) code and called
//! this exact same `schedule()` directly, once a time quantum elapsed. No raw-asm timer-entry
//! conversion needed (an earlier version of this comment predicted one would be) — `schedule()`
//! itself is just a stack-pointer-swap primitive (`switch_context`) agnostic to *why* the caller
//! is yielding, so calling it from inside the existing `extern "x86-interrupt" fn` works
//! unmodified: the preempted process's own compiler-generated interrupt-return sequence sits
//! dormant on its own kernel stack (below wherever `switch_context`'s `ret` suspended it) until
//! this exact same pid is picked again, at which point unwinding back up through `schedule()` and
//! this file's own call site reaches that dormant `iretq` naturally. **Deliberately scoped to
//! ring-3 only** — kernel/syscall/module code is never preempted (`IA32_SFMASK` already clears
//! `IF` for a `SYSCALL`'s whole duration, and nothing else here runs with `IF` set for long), so no
//! kernel `spin::Mutex`/module-static-data critical section anywhere in this codebase needed
//! auditing for preemption-safety — user-mode code never holds one. See
//! `interrupts::timer_interrupt_handler`'s own doc comment for the ring check/quantum/EOI-ordering
//! details, and `cpu::fpu`'s module doc comment for the one real correctness gap preemption opened
//! up (unsaved SSE/x87 state) and how it's closed (`Process::fpu_state`).
//!
//! **"Nothing runnable" no longer means "spin forever with interrupts masked."** `schedule()`'s
//! own fallback used to be `crate::hlt_loop()` — a deliberate, permanent dead end, safe only
//! because until `sh` (BusyBox's `hush`) needed a real blocking stdin read (`crate::stdin`'s own
//! module doc comment, `process::BlockReason::WaitingForStdin`), *every* blocked process was woken
//! by another *schedulable* process's own syscall (`do_exit` waking a `wait4`er, `pipe_write`
//! waking a pipe reader) — something always eventually ran to do the waking, so an empty ready
//! queue genuinely meant "stuck forever" and burning a core spinning was an acceptable (if wasteful)
//! way to make that visible. A blocked stdin read breaks that assumption: the *only* thing that can
//! ever wake it is a hardware keyboard IRQ, which can't fire at all while `IA32_SFMASK`-cleared
//! `IF` (or a permanently spinning `hlt_loop` that never bothered to `sti` first) keeps interrupts
//! masked. `wait_for_ready` below replaces that fallback with a real idle wait: enable interrupts
//! just long enough to let a hardware interrupt land (the standard `sti; hlt` atomic idiom, so a
//! wakeup racing the check is never lost), then re-disable and re-check. Safe to do here
//! specifically because, by construction, nothing in `schedule()`'s own call path still holds
//! `process::table()`/`READY_QUEUE` at this point (every caller that blocks — `do_wait4`,
//! `pipe_read`, `stdin::read` — drops its locks before ever calling `schedule()`) — the keyboard
//! IRQ handler's own locking of those same structures (to wake a blocked reader) can't deadlock
//! against a lock this code is still holding, because it isn't holding any.

use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicU64, Ordering};

use spin::Mutex;
use x86_64::instructions::interrupts::without_interrupts;

use crate::process::context_switch::switch_context;
use crate::process::{self, Pid, ProcState};
use crate::cpu::gdt;
use crate::serial_println;

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

/// Removes `pid` from `READY_QUEUE` if it's sitting in it -- needed only for a cross-process
/// `SIGSTOP`/`SIGTSTP` (see `process::signals`'s `Action::Stop` handling) landing on a target
/// that's `Ready` but hasn't actually run yet. Without this, the scheduler would still pop and run
/// it on its next turn regardless of `state` having been flipped to `Stopped` — nothing else in
/// this cooperative scheduler ever needs to retract a pid it already enqueued, so this has no
/// other caller.
pub fn remove_ready(pid: Pid) {
    READY_QUEUE.lock().retain(|&p| p != pid);
}

/// Voluntarily gives up the CPU. If the caller is still `Ready` or `Running` (i.e. it didn't just
/// block or exit), it's re-enqueued so it gets another turn later — a caller that transitioned to
/// `Blocked`/`Zombie` just before calling this is deliberately *not* re-enqueued, which is how
/// `process::do_wait4`/`do_exit` actually suspend/terminate. Picks the next `Ready` process and
/// switches to it; falls back to `wait_for_ready`'s real idle wait only if nothing is runnable at
/// all. Returns once this exact call site is switched back into — for a caller that just blocked,
/// that means "a later event woke it back to `Ready` and the scheduler picked it again."
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

        // If the caller itself were still Ready/Running it would have just been popped back out
        // above, so an empty queue here means the system is genuinely idle right now -- wait_for_ready
        // blocks (with interrupts real-enabled, not spinning) until a hardware event makes that no
        // longer true.
        let next_pid = wait_for_ready();

        if has_prev && next_pid == prev_pid {
            process::table().lock().get_mut(&prev_pid).unwrap().state = ProcState::Running;
            return;
        }

        let prev_rsp_slot: *mut u64 = if has_prev {
            let mut table = process::table().lock();
            let prev = table.get_mut(&prev_pid).unwrap();
            // SAFETY: prev is the outgoing process, about to be switched away from on the very
            // next line below — its live hardware FPU/SSE state genuinely belongs to it (see
            // cpu::fpu's own module doc comment for why this capture is required now that
            // preemption exists) and fpu_state is a valid, 16-byte-aligned FxSaveArea field.
            unsafe { crate::cpu::fpu::save(&mut prev.fpu_state as *mut _) };
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

/// Blocks (genuinely — not spinning) until `READY_QUEUE` has something in it, and returns that
/// pid. See this module's own doc comment for why this exists at all (`hlt_loop()`'s old
/// permanent-dead-end fallback can't work once a process can be blocked waiting on nothing but a
/// hardware interrupt) and why it's safe to enable interrupts here specifically (every caller that
/// blocks drops its own locks before ever reaching `schedule()`, so nothing this code's caller
/// might be holding can be re-entered by the keyboard IRQ handler this is specifically waiting to
/// let fire).
///
/// Only ever called from inside `schedule()`'s own `without_interrupts` critical section, but
/// explicitly `disable()`s at the top of every loop iteration anyway (not just relying on that
/// outer wrapper) — cheap, and makes this function's own precondition self-contained rather than
/// relying on every future call site to remember it.
fn wait_for_ready() -> Pid {
    loop {
        x86_64::instructions::interrupts::disable();
        // Scoped so the READY_QUEUE guard is dropped before enable_and_hlt() below -- holding it
        // across that would let the keyboard IRQ handler's own wake-up (which needs this same
        // lock, see crate::console::stdin::push_byte) deadlock against itself on this single core.
        let popped = { READY_QUEUE.lock().pop_front() };
        if let Some(pid) = popped {
            return pid;
        }
        // Nothing runnable anywhere. The only thing that can still change that is a hardware
        // interrupt (concretely: the keyboard IRQ waking a process blocked on stdin) -- enable
        // interrupts and halt right up to the point one arrives, rather than either burning a full
        // core spinning forever or leaving interrupts masked forever (which would make that
        // wakeup impossible in the first place). `sti; hlt` back to back is the standard atomic
        // idiom for exactly this: x86 guarantees the very next instruction after `sti` (here,
        // `hlt`) executes before any interrupt just unmasked is actually taken, so a wakeup that
        // raced right up against the check above is never lost.
        x86_64::instructions::interrupts::enable_and_hlt();
        // Woken by *some* interrupt (timer or keyboard) -- loop back and re-check; only a keyboard
        // IRQ that actually queued a reader will make the next pop_front() succeed.
    }
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
/// kernel stack, restores its own `IA32_FS_BASE`, and returns its saved `rsp` — the common tail
/// shared by `schedule()` and `start()` just before the actual `switch_context` call.
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
    // IA32_FS_BASE is a single global MSR, not something switch_context itself saves/restores (it
    // only touches RSP/callee-saved GPRs) -- without this, one process's own %fs-relative TLS
    // (every musl-linked binary's stack-protector check among it) would silently see whichever
    // *other* process last called SYS_SET_FS_BASE. See Process::fs_base's own doc comment for the
    // real crash this fixed.
    x86_64::registers::model_specific::FsBase::write(x86_64::VirtAddr::new(next.fs_base));
    // Same "restore before the switch actually lands" reasoning as schedule()'s own fpu::save
    // call on the outgoing side — by the time switch_context's `ret` hands control to next, the
    // hardware FPU/SSE registers must already hold *its* state, not whichever process ran last.
    // SAFETY: next.fpu_state is a valid, 16-byte-aligned FxSaveArea — either a real prior
    // fpu::save() (a process that's run before) or cpu::fpu::clean_state()'s own output (a
    // never-run process, see process::lifecycle::spawn/do_fork_from_current).
    unsafe { crate::cpu::fpu::restore(&next.fpu_state as *const _) };
    next.rsp
}
