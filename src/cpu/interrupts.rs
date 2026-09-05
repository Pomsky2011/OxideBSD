use core::sync::atomic::{AtomicU64, Ordering};

use pc_keyboard::layouts::Us104Key;
use pc_keyboard::{DecodedKey, HandleControl, PS2Keyboard, ScancodeSet1};
use spin::{Lazy, Mutex};
use x86_64::VirtAddr;
use x86_64::instructions::port::Port;
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use crate::cpu::gdt::DOUBLE_FAULT_IST_INDEX;
use crate::cpu::pic::{self, PIC_1_OFFSET, PIC_2_OFFSET};
use crate::reboot::reboot;
use crate::{serial_print, serial_println};

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
enum InterruptIndex {
    Timer = PIC_1_OFFSET,
    Keyboard,
}

impl InterruptIndex {
    fn as_u8(self) -> u8 {
        self as u8
    }
}

static TICKS: AtomicU64 = AtomicU64::new(0);

/// Number of timer interrupts handled since boot.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

type IrqHandlerSlot = Mutex<Option<fn()>>;

/// One slot per possible IRQ line (0-15); only 2-15 are ever populated -- 0/1 are permanently
/// owned by the timer/keyboard's own dedicated handlers below, never routed through this table.
/// Lets a driver whose IRQ line isn't known until runtime (e.g. read from a PCI device's
/// interrupt-line register during `net::rtl8139::probe_and_init`) claim a vector without the
/// static `IDT` needing to change shape.
static IRQ_HANDLERS: [IrqHandlerSlot; 16] = [const { Mutex::new(None) }; 16];

/// Registers `handler` to be called whenever `irq` fires. Must be paired with a subsequent
/// `pic::unmask_irq(irq)` -- until that call, the line stays masked at the controller and
/// `handler` is simply never invoked. Both calls should happen inside
/// `x86_64::instructions::interrupts::without_interrupts` to close the race where the line fires
/// between registration and unmasking.
pub fn register_irq_handler(irq: u8, handler: fn()) {
    assert!(
        (2..16).contains(&irq),
        "IRQ 0/1 are reserved for the timer/keyboard"
    );
    *IRQ_HANDLERS[irq as usize].lock() = Some(handler);
}

/// Defines one `extern "x86-interrupt"` trampoline per listed IRQ line (each must be a distinct
/// function item -- a handler installed into the IDT can't be parameterized by IRQ number at
/// runtime) plus `install_irq_trampolines`, which wires all of them into a `Lazy<IDT>` under
/// construction. Each trampoline dispatches through `IRQ_HANDLERS`, doing nothing but the EOI if
/// that line has no registered handler (a spurious or not-yet-claimed IRQ) -- and always sends
/// the EOI regardless, since skipping it leaves the line masked at the controller forever, not
/// just for this one interrupt.
macro_rules! define_irq_trampolines {
    ($( $name:ident => $irq:literal ),+ $(,)?) => {
        $(
            extern "x86-interrupt" fn $name(_stack_frame: InterruptStackFrame) {
                let handler = *IRQ_HANDLERS[$irq].lock();
                if let Some(handler) = handler {
                    handler();
                }
                unsafe {
                    pic::notify_end_of_interrupt(PIC_1_OFFSET + $irq);
                }
            }
        )+

        fn install_irq_trampolines(idt: &mut InterruptDescriptorTable) {
            $(
                idt[PIC_1_OFFSET + $irq].set_handler_fn($name);
            )+
        }
    };
}

define_irq_trampolines! {
    irq2_trampoline => 2,
    irq3_trampoline => 3,
    irq4_trampoline => 4,
    irq5_trampoline => 5,
    irq6_trampoline => 6,
    irq7_trampoline => 7,
    irq8_trampoline => 8,
    irq9_trampoline => 9,
    irq10_trampoline => 10,
    irq11_trampoline => 11,
    irq12_trampoline => 12,
    irq13_trampoline => 13,
    irq14_trampoline => 14,
    irq15_trampoline => 15,
}

// MapLettersToUnicode (not Ignore) so Ctrl+<letter> decodes to the corresponding C0 control code
// (Ctrl+C => 0x03, Ctrl+D => 0x04, etc.) instead of being silently dropped to the plain letter --
// stsh's read_line (see `userland/stsh/`) relies on those bytes reaching stdin to implement
// abort-line/EOF handling.
static KEYBOARD: Mutex<PS2Keyboard<Us104Key, ScancodeSet1>> = Mutex::new(PS2Keyboard::new(
    ScancodeSet1::new(),
    Us104Key,
    HandleControl::MapLettersToUnicode,
));

static IDT: Lazy<InterruptDescriptorTable> = Lazy::new(|| {
    let mut idt = InterruptDescriptorTable::new();

    // DPL 3 so ring-3 code can hit this via `int3` directly: interrupt gates default to DPL 0,
    // and a *software*-invoked interrupt (unlike a hardware exception) additionally requires
    // CPL <= gate DPL, so leaving this at the default causes int3-from-ring-3 to fault with a
    // #GP on the gate itself instead of ever reaching this handler.
    idt.breakpoint
        .set_handler_fn(breakpoint_handler)
        .set_privilege_level(x86_64::PrivilegeLevel::Ring3);
    idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);
    idt.general_protection_fault
        .set_handler_fn(general_protection_fault_handler);
    idt.page_fault.set_handler_fn(page_fault_handler);
    unsafe {
        idt.double_fault
            .set_handler_fn(double_fault_handler)
            .set_stack_index(DOUBLE_FAULT_IST_INDEX);
    }
    idt[InterruptIndex::Timer.as_u8()].set_handler_fn(timer_interrupt_handler);
    idt[InterruptIndex::Keyboard.as_u8()].set_handler_fn(keyboard_interrupt_handler);
    install_irq_trampolines(&mut idt);

    idt
});

pub fn init_idt() {
    serial_println!(
        "[boot] loading IDT: breakpoint, invalid_opcode, general_protection_fault, page_fault, \
         double_fault, timer (vector {:#x}), keyboard (vector {:#x}), IRQ2-15 trampolines \
         (vectors {:#x}-{:#x}, unclaimed until a driver calls register_irq_handler)",
        InterruptIndex::Timer.as_u8(),
        InterruptIndex::Keyboard.as_u8(),
        PIC_1_OFFSET + 2,
        PIC_1_OFFSET + 15,
    );
    IDT.load();
    serial_println!("[boot] IDT loaded");
}

/// Remaps the PIC pair's interrupt vectors and unmasks them. Must run after `init_idt` and
/// before interrupts are enabled, so every unmasked IRQ already has a handler installed.
pub fn init_pics() {
    serial_println!(
        "[boot] remapping PIC1/PIC2 to vectors {:#x}/{:#x}",
        PIC_1_OFFSET,
        PIC_2_OFFSET
    );
    unsafe {
        pic::init();
    }
    serial_println!("[boot] PICs initialized and unmasked");
}

extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    serial_println!("EXCEPTION: BREAKPOINT\n{:#?}", stack_frame);
}

extern "x86-interrupt" fn invalid_opcode_handler(stack_frame: InterruptStackFrame) {
    serial_println!("EXCEPTION: INVALID OPCODE\n{:#?}", stack_frame);
    reboot();
}

/// Ring-3 `#GP`s get the same fault-to-signal treatment `page_fault_handler` below already
/// documents in full (see that function's own doc comment for the trampoline mechanism and why a
/// direct handler call isn't possible from an `extern "x86-interrupt" fn`) -- found live, not
/// preemptively: the expanded POSIX conformance pilot's own `strftime/2-1.c` has a real stack-
/// buffer overflow (an upstream test bug -- it declares `char text[20]` but passes `256` as
/// `strftime`'s own max-length argument), correctly caught by musl's stack-protector, whose
/// `__stack_chk_fail` on this target is a bare `hlt; ret` (`hlt` is a privileged, ring-0-only
/// instruction) -- executing it from ring 3 raises exactly this fault. Before this fix, *any*
/// ring-3 `#GP` (a real stack-smashing catch, a bad segment reference, any other privileged-
/// instruction misuse) rebooted the whole VM instead of just terminating the one offending
/// process, a real robustness gap independent of this one test file. No fault-specific signal
/// distinction is needed the way `page_fault_handler`'s `SIGBUS`-vs-`SIGSEGV` split is: real Linux
/// maps every userland `#GP` to `SIGSEGV` uniformly, so this does too.
extern "x86-interrupt" fn general_protection_fault_handler(
    mut stack_frame: InterruptStackFrame,
    error_code: u64,
) {
    let interrupted_ring3 = stack_frame.code_segment.0 & 0x3 == 3;
    if interrupted_ring3 {
        let pid = crate::process::scheduler::current_pid();
        if pid != 0 {
            // Self-signal: always just records the pending bit -- see page_fault_handler's own
            // identical call for why this is sound from interrupt context.
            let _ = crate::process::do_kill(pid, pid as i64, crate::process::SIGSEGV as i64);
            // SAFETY: see page_fault_handler's own identical redirect.
            unsafe {
                stack_frame.as_mut().update(|f| {
                    f.instruction_pointer =
                        VirtAddr::new(crate::process::fault_trampoline::FAULT_TRAMPOLINE_VA);
                });
            }
            return;
        }
    }
    serial_println!(
        "EXCEPTION: GENERAL PROTECTION FAULT (error code: {:#x})\n{:#?}",
        error_code,
        stack_frame
    );
    reboot();
}

/// Ring-3 faults no longer reboot the whole kernel — they terminate (or, if a real handler is
/// installed, signal) just the one offending process. A kernel-mode fault (a real bug in this
/// kernel itself) is unchanged: still an unconditional reboot, the same safety net as before.
///
/// **Why this can't just invoke the process's own handler directly, right here**: unlike
/// `syscall::SyscallFrame` (built by hand, every GPR an explicit mutable field),
/// `extern "x86-interrupt"`'s compiler-generated entry/exit leaves the interrupted GPRs
/// (`RDI`/`RSI`/`RDX`/...) completely inaccessible to this function — only
/// `InterruptStackFrame`'s handful of fields (`instruction_pointer`, `stack_pointer`, `cpu_flags`,
/// the segment selectors) can be read or changed via its own `as_mut()`. A real, argument-correct
/// handler call needs to set `RDI = signum` (and `RSI`/`RDX` for `SA_SIGINFO`) directly, which this
/// function has no way to do. See `process::fault_trampoline`'s own module doc comment for the real
/// fix: record the signal as pending, then redirect `instruction_pointer` to a tiny kernel-authored
/// trampoline page that forces the process straight back through a real `SYSCALL` instruction,
/// landing in the already-correct, GPR-complete `syscall::deliver_pending_signal` machinery
/// instead.
///
/// `signal_for_user_fault` decides `SIGBUS` (a reference into a real fd-backed mapping's own
/// reserved-but-unbacked tail — `mmap/11-2.c`/`11-3.c` in the conformance pilot) vs. `SIGSEGV`
/// (every other unmapped/invalid ring-3 reference) — either way, `do_kill`'s own self-signal path
/// (just sets the pending bit, doesn't act on it) is reused rather than hand-rolling a second
/// "record this signal" path; `deliver_pending_signal`, reached via the trampoline, resolves the
/// real disposition (terminate, by default — a real fix in its own right, replacing a full reboot
/// — or a genuine handler invocation).
extern "x86-interrupt" fn page_fault_handler(
    mut stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    let interrupted_ring3 = stack_frame.code_segment.0 & 0x3 == 3;
    if interrupted_ring3
        && let Ok(fault_addr) = Cr2::read()
    {
        let pid = crate::process::scheduler::current_pid();
        if pid != 0 {
            let sig = crate::process::signal_for_user_fault(pid, fault_addr.as_u64());
            // Self-signal: always just records the pending bit (see do_kill's own doc comment),
            // safe to call from here for the identical reason timer_interrupt_handler's own calls
            // into process::table()/scheduler already are (this interrupt gate runs with IF
            // cleared, same single-core non-reentrancy this whole file already relies on).
            let _ = crate::process::do_kill(pid, pid as i64, sig as i64);
            // SAFETY: only the resume RIP is changed -- redirecting straight into a real,
            // kernel-mapped, user-executable trampoline page (see its own module doc comment).
            // RSP/RFLAGS/the segment selectors are left exactly as they were; this process has
            // exactly one ring-3 code/data selector pair, already correct.
            unsafe {
                stack_frame.as_mut().update(|f| {
                    f.instruction_pointer =
                        VirtAddr::new(crate::process::fault_trampoline::FAULT_TRAMPOLINE_VA);
                });
            }
            return;
        }
    }
    serial_println!(
        "EXCEPTION: PAGE FAULT\naccessed address: {:?}\nerror code: {:?}\n{:#?}",
        Cr2::read(),
        error_code,
        stack_frame
    );
    reboot();
}

extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    serial_println!("EXCEPTION: DOUBLE FAULT\n{:#?}", stack_frame);
    reboot();
}

/// How many timer ticks (`TIMER_HZ = 100`, see `cpu::pit.rs`) a process gets before it's
/// preempted — `4` ticks = 40ms, a fairly conventional interactive quantum (in the same ballpark
/// as classic Linux's old `HZ=100` default). Only ever consulted for a process actually caught
/// running ring-3 (user-mode) code — see `timer_interrupt_handler`'s own doc comment.
pub(crate) const PREEMPT_QUANTUM_TICKS: u64 = 4;

/// Real preemption lives here: on top of the tick/sleeper-wake bookkeeping this handler has always
/// done, it now also decides whether to force a reschedule.
///
/// **Deliberately scoped to ring-3 only** — checked via `stack_frame.code_segment`'s RPL bits (the
/// CPU's own record of what was interrupted, not anything this handler has to track itself): a
/// hardware interrupt gate automatically switches onto `TSS.RSP0` (this process's own kernel
/// stack, kept in sync by `gdt::set_kernel_stack` on every context switch) whenever it catches
/// ring-3 code, so by the time this function runs, we're already on the right stack to suspend
/// this exact process via an ordinary `scheduler::schedule()` call. Ring-0 code (a syscall handler,
/// another IRQ handler, this very function's own housekeeping) is never preempted — deliberately,
/// not just "not implemented yet": user-mode code never holds a kernel `spin::Mutex` or touches
/// module static state, so scoping preemption to ring-3 sidesteps auditing every critical section
/// in this codebase for preemption-safety. See `scheduler.rs`'s own module doc comment for why
/// calling `schedule()` directly from this `extern "x86-interrupt" fn` is sound even though it may
/// not return for an arbitrary stretch of wall-clock time.
///
/// **EOI is sent before the possible `schedule()` call, not after** — load-bearing, not stylistic.
/// `schedule()` can switch away to a completely different process for an arbitrary amount of time
/// before this exact call returns (i.e. before this exact process is picked again); until the EOI
/// is sent, the PIC still considers IRQ0 "in service" and won't deliver *any* further timer
/// interrupt to anyone — which would freeze not just future preemption but every other
/// `ticks()`-gated wakeup in this file (sleepers, POSIX timers, `SIGALRM`, ...) for good.
extern "x86-interrupt" fn timer_interrupt_handler(mut stack_frame: InterruptStackFrame) {
    let now = TICKS.fetch_add(1, Ordering::Relaxed) + 1;

    // Wake any process blocked in `process::do_nanosleep` (`BlockReason::Sleeping`) whose deadline
    // has now passed -- same "IRQ handler reaches directly into `process::table()`" shape
    // `crate::console::stdin::push_byte`'s own `wake_blocked_readers` already established for
    // `WaitingForStdin`, just driven by this timer IRQ instead of the keyboard one. Safe for the
    // same reason that one is: every other place `process::table()` is locked either runs inside a
    // `SYSCALL` (where `SFMASK` clears `IF` for the syscall's entire duration) or inside
    // `scheduler::schedule()`'s own `without_interrupts` section -- this lock can never be held by
    // code this interrupt could actually preempt.
    {
        let mut table = crate::process::table().lock();
        // TEMPORARY diagnostic for the POSIX pilot full-corpus run: printed every ~10 real
        // seconds to check whether the process table grows unboundedly over a long boot with
        // hundreds of fork/exec/exit cycles (suspected live while chasing a `fork/8-1.c` busy-loop
        // that took 9+ minutes instead of ~1s under KVM) -- remove once that's resolved one way or
        // the other.
        if now % 1000 == 0 {
            crate::serial_println!("[diag] tick={} table_len={}", now, table.len());
            // TEMPORARY diagnostic for the pthread/aio massfix investigation: per-process state
            // dump to see exactly what a hung sigwait/6-1.c-shaped test is actually blocked on
            // (or spinning on) without needing live GDB against the QEMU stub -- narrows whether
            // `pending_signals`/`blocked_signals` ever reach the "deliverable" state this session's
            // timer-redirect mechanism checks for, and whether `preempted_resume` ever gets set at
            // all. Remove once the underlying hang is understood one way or the other.
            for (&diag_pid, diag_proc) in table.iter() {
                crate::serial_println!(
                    "[diag-thread] pid={} tgid={} state={:?} pending={:#x} blocked={:#x} preempted_resume={} has_addr_space={}",
                    diag_pid,
                    diag_proc.tgid,
                    diag_proc.state,
                    diag_proc.pending_signals,
                    diag_proc.blocked_signals,
                    diag_proc.preempted_resume.is_some(),
                    diag_proc.address_space.is_some()
                );
            }
        }
        // Real per-process CPU-time accounting (`Process::cpu_ticks`, see its own doc comment) --
        // the process this tick actually interrupted is the one that was consuming the CPU for it.
        // `current_pid() == 0` only at boot, before any real process exists.
        let current = crate::process::scheduler::current_pid();
        if current != 0
            && let Some(proc) = table.get_mut(&current)
        {
            proc.cpu_ticks += 1;
        }
        for (&pid, proc) in table.iter_mut() {
            if let crate::process::ProcState::Blocked(crate::process::BlockReason::Sleeping(
                deadline,
            )) = proc.state
                && now >= deadline
            {
                proc.state = crate::process::ProcState::Ready;
                crate::process::scheduler::enqueue_ready(pid);
            }

            // Real `mq_timedsend`/`mq_timedreceive` deadline expiry (`crate::fs::mqueue`) -- same
            // "IRQ handler reaches directly into `process::table()`" shape `Sleeping` just above
            // already established; `u64::MAX` (the plain `mq_send`/`mq_receive` wrapper's "no
            // timeout" case) never realistically matches `now` within this kernel's lifetime, so
            // no separate `Option`-typed deadline is needed. `do_mq_timedsend`/`do_mq_timedreceive`
            // re-check the real condition after waking, same discipline `Sleeping` establishes.
            if let crate::process::ProcState::Blocked(
                crate::process::BlockReason::WaitingForMqData(_, deadline)
                | crate::process::BlockReason::WaitingForMqSpace(_, deadline),
            ) = proc.state
                && now >= deadline
            {
                proc.state = crate::process::ProcState::Ready;
                crate::process::scheduler::enqueue_ready(pid);
            }

            // Real `semtimedop` deadline expiry (`crate::fs::sysv_sem`) -- same dual-wake shape
            // `WaitingForMqData`/`WaitingForMqSpace` just above already establish; `u64::MAX` (the
            // plain, non-timed `semop` wrapper's case) never realistically matches `now`.
            // `do_semop`/`do_semtimedop` re-check the whole op array from scratch after waking,
            // same discipline `Sleeping` establishes.
            if let crate::process::ProcState::Blocked(crate::process::BlockReason::WaitingForSemOp(
                _,
                _,
                _,
                deadline,
            )) = proc.state
                && now >= deadline
            {
                proc.state = crate::process::ProcState::Ready;
                crate::process::scheduler::enqueue_ready(pid);
            }

            // Real `sigtimedwait` deadline expiry (`process::signals::do_sigtimedwait`) -- same
            // dual-wake shape `WaitingForSemOp`/`WaitingForMqData` just above already establish;
            // `u64::MAX` (the plain `sigwaitinfo`/`sigwait` case) never realistically matches
            // `now`. `do_sigtimedwait`'s own loop re-checks `pending_signals & wait_set` fresh
            // after waking, same discipline `Sleeping` establishes.
            if let crate::process::ProcState::Blocked(
                crate::process::BlockReason::WaitingForSpecificSignal(_, deadline),
            ) = proc.state
                && now >= deadline
            {
                proc.state = crate::process::ProcState::Ready;
                crate::process::scheduler::enqueue_ready(pid);
            }

            // Real `FUTEX_WAIT` deadline expiry (`process::do_futex`) -- same dual-wake shape
            // `WaitingForSpecificSignal`/`WaitingForSemOp` just above already establish; `u64::MAX`
            // (a null `to`, the plain "wait forever" case) never realistically matches `now`.
            // `do_futex`'s own post-wake check re-verifies the real deadline fresh, same
            // discipline `Sleeping` establishes -- this scan only needs to flip `state` back to
            // `Ready` so that check gets a chance to run at all.
            if let crate::process::ProcState::Blocked(crate::process::BlockReason::WaitingForFutex(
                _,
                _,
                deadline,
            )) = proc.state
                && now >= deadline
            {
                proc.state = crate::process::ProcState::Ready;
                crate::process::scheduler::enqueue_ready(pid);
            }

            // `SYS_SETITIMER`'s `ITIMER_REAL` expiry (also backs real `alarm()`, a thin musl-side
            // wrapper around it -- see `process::do_setitimer`'s own doc comment). Just sets the
            // pending bit, the same simple, already-established pattern `process::do_kill`'s own
            // self-targeting case uses (`me.pending_signals |= ...`) -- real delivery/default
            // termination happens naturally at this exact process's own next syscall-dispatch tail
            // (`src/syscall.rs`'s `deliver_pending_signal`), not from here. Deliberately *not* the
            // stronger immediate-termination path `do_kill`'s cross-process branch uses for a
            // no-handler target: doing that here would mean re-locking `process::table()` (or
            // calling `terminate_process`, which does its own locking) while this exact lock is
            // still held for the surrounding scan -- a real deadlock against `spin::Mutex`'s
            // non-reentrant guarantee. Sufficient for this kernel's actual use case (`ping`'s own
            // real usage pattern: a tight loop of individually non-blocking `recvfrom` calls, each
            // its own syscall) -- a process genuinely blocked elsewhere (`BlockReason::Sleeping`/
            // `WaitingForPipeData`/...) won't see this promptly, the same documented, accepted gap
            // `do_kill`'s own doc comment already calls out for a handler-installed cross-process
            // signal.
            if let Some(deadline) = proc.real_timer_deadline
                && now >= deadline
            {
                proc.pending_signals |= 1 << (crate::process::SIGALRM - 1);
                proc.real_timer_deadline = if proc.real_timer_interval_ticks > 0 {
                    Some(now + proc.real_timer_interval_ticks)
                } else {
                    None
                };
                // A process genuinely `Blocked` (`pause`/`nanosleep`/`sigwait`/a plain `mq_receive`)
                // waiting specifically on this signal must be woken here, or it hangs forever --
                // this expiry only sets the pending bit, unlike `do_kill`'s own `Action::SetPending`
                // arm, whose callers already wire these same four hooks in. Safe to call
                // unconditionally: each hook only fires if `proc`'s own state actually matches its
                // one relevant `BlockReason`, a no-op otherwise. Found live via
                // `clock_settime/4-1.c` (a real `alarm()`-shaped `SIGALRM` expiry while the target
                // sits in `sigwait()`) permanently hanging the POSIX conformance pilot.
                crate::process::signals::wake_if_paused(pid, proc, crate::process::SIGALRM);
                crate::process::signals::wake_if_sigwaiting(pid, proc, crate::process::SIGALRM);
                crate::process::signals::wake_if_sleeping(pid, proc, crate::process::SIGALRM);
                crate::process::signals::wake_if_mq_waiting(pid, proc, crate::process::SIGALRM);
                crate::process::signals::wake_if_futex_waiting(pid, proc, crate::process::SIGALRM);
            }

            // `SYS_TIMER_CREATE`'s own per-timer expiry (`Process::posix_timers`, batch items
            // 6-10 of `docs/MISSING_POSIX_SYSCALLS.md`) -- same simple pending-bit-only delivery
            // as `real_timer_deadline` just above (no forced cross-process wake here either, same
            // reasoning), plus a real `timer_getoverrun` count: an expiry whose signal is *still*
            // pending from a previous, undelivered expiry increments `overrun` instead of getting
            // lost silently.
            for i in 0..proc.posix_timers.len() {
                // `newly_signaled` carries a just-set signal number past the end of `slot`'s own
                // borrow (of `proc.posix_timers[i]`, not all of `proc`) -- calling the wake hooks
                // below needs `&mut Process` (the whole struct), which can't coexist with a live
                // sub-borrow from array indexing, even though the two touch disjoint fields.
                let mut newly_signaled = None;
                let Some(slot) = proc.posix_timers[i].as_mut() else {
                    continue;
                };
                // A live wall-clock comparison, not the (possibly stale) tick-domain `deadline`,
                // for a timer armed via `TIMER_ABSTIME` against `CLOCK_REALTIME` -- see
                // `PosixTimer::realtime_target`'s own doc comment for why this is what lets a
                // later `clock_settime(CLOCK_REALTIME, ...)` correctly retarget an already-armed
                // timer with zero extra bookkeeping on the `clock_settime` side.
                // `CLOCK_PROCESS_CPUTIME_ID`/`CLOCK_THREAD_CPUTIME_ID` timers compare against
                // `proc.cpu_ticks` (real per-process CPU time), not wall-clock `now` -- a direct
                // field read, not a call to `timers::timer_now(proc, ...)`, since that takes
                // `&Process` and can't coexist with `slot`'s own live sub-borrow of
                // `proc.posix_timers` (same disjoint-field-access pattern this loop's own
                // `proc.pending_signals` accesses below already rely on).
                let timer_now = if crate::process::timers::is_cputime_clock(slot.clockid) {
                    proc.cpu_ticks
                } else {
                    now
                };
                let fired = if let Some(target) = slot.realtime_target {
                    crate::cpu::rtc::unix_epoch_now_precise() >= target
                } else if let Some(deadline) = slot.deadline {
                    timer_now >= deadline
                } else {
                    false
                };
                if fired {
                    if slot.signo != 0 {
                        let bit = 1 << (slot.signo - 1);
                        if proc.pending_signals & bit != 0 {
                            slot.overrun = slot.overrun.saturating_add(1);
                        } else {
                            slot.overrun = 0;
                            proc.pending_signals |= bit;
                            newly_signaled = Some(slot.signo);
                        }
                    }
                    // Only the *first* expiry of an abstime-armed `CLOCK_REALTIME` timer needs
                    // real wall-clock precision -- a periodic reload falls back to plain
                    // tick-domain `interval_ticks`, same simplification `PosixTimer::
                    // realtime_target`'s own doc comment already flags.
                    slot.realtime_target = None;
                    slot.deadline = if slot.interval_ticks > 0 {
                        Some(timer_now + slot.interval_ticks)
                    } else {
                        None
                    };
                }
                // `slot`'s own borrow of `proc.posix_timers[i]` ends here -- only now can `proc`
                // be reborrowed whole for the wake hooks below. Same hang this section's own
                // `real_timer_deadline` sibling above just got fixed for, via a real POSIX timer
                // instead of `alarm()`/`setitimer()` -- a process sitting in `pause`/`nanosleep`/
                // `sigwait`/a plain `mq_receive` waiting on a `timer_create`-armed signal (e.g.
                // `clock_settime/4-1.c`'s own `sigwait()` for a `TIMER_ABSTIME` timer) would
                // otherwise never wake once this loop merely set the pending bit.
                if let Some(signo) = newly_signaled {
                    crate::process::signals::wake_if_paused(pid, proc, signo);
                    crate::process::signals::wake_if_sigwaiting(pid, proc, signo);
                    crate::process::signals::wake_if_sleeping(pid, proc, signo);
                    crate::process::signals::wake_if_mq_waiting(pid, proc, signo);
                    crate::process::signals::wake_if_futex_waiting(pid, proc, signo);
                }
            }
        }
    }

    // Must happen before the preemption check below can call scheduler::schedule() -- see this
    // function's own doc comment.
    unsafe {
        pic::notify_end_of_interrupt(InterruptIndex::Timer.as_u8());
    }

    // Ring check via the CPU's own saved CS RPL bits (`& 0x3`), not e.g. `scheduler::current_pid()`
    // state -- see this function's own doc comment for why ring-3-only is the deliberate scope.
    let interrupted_ring3 = stack_frame.code_segment.0 & 0x3 == 3;
    if interrupted_ring3 {
        // Real `SCHED_FIFO`/`SCHED_RR` priority preemption: checked on *every* tick, not gated on
        // the quantum below -- a higher-`sched_priority` process becoming Ready must preempt within
        // about one tick, not wait up to a full `PREEMPT_QUANTUM_TICKS` quantum. Every `SCHED_OTHER`
        // process (this kernel's spawn-time default, see `Process::sched_policy`'s own doc comment)
        // shares the one priority valid for that policy (`0`), so `higher_priority_ready` is always
        // `false` for the pre-existing plain-round-robin case -- this whole branch is inert unless
        // something has actually called `sched_setscheduler`/`sched_setparam` into a real-time
        // policy. See `scheduler::ready_queue_has_higher_priority_than`'s own doc comment for why a
        // process-table lookup here is safe (no lock this handler already holds could conflict).
        let current_priority = crate::process::table()
            .lock()
            .get(&crate::process::scheduler::current_pid())
            .map(|p| p.sched_priority)
            .unwrap_or(0);
        let higher_priority_ready =
            crate::process::scheduler::ready_queue_has_higher_priority_than(current_priority);
        if higher_priority_ready || now.is_multiple_of(PREEMPT_QUANTUM_TICKS) {
            crate::process::scheduler::schedule();
        }

        // Real signal delivery for a ring-3 process making no syscalls of its own -- a tight,
        // purely userspace-computational loop (e.g. `pthread_atfork/3-3.c`'s own worker thread,
        // whose `while(do_it) pthread_atfork(...)` loop never traps into the kernel at all).
        // `syscall::deliver_pending_signal` only ever runs from a real syscall's own dispatch tail
        // or `sigreturn` -- a signal set pending via `do_kill`/an itimer/POSIX-timer expiry on a
        // process that never makes another syscall would otherwise sit pending forever, hanging
        // any caller (or `sigwait`-ing sibling thread) that depends on it. Checked on every tick,
        // independent of the preemption decision above (real signal delivery isn't gated by
        // scheduling quantum) -- by the time execution reaches here, whether or not `schedule()`
        // was just called, `current_pid()` always names the exact process `stack_frame` belongs to
        // (a preempted process's own call into `schedule()` above only "returns" once *that same*
        // process is re-selected -- see `scheduler.rs`'s own module doc comment).
        //
        // `pending & !blocked != 0` is a safe, sufficient proxy for "something is really
        // deliverable": `record_pending`/`do_kill`'s own `Action::Discard` arm never sets the
        // pending bit at all for a `SIG_IGN` disposition, so a set-and-unblocked bit here always
        // means a real handler invocation or a real default-disposition resolution (including
        // `Terminate`) is waiting. Reuses the exact same real, kernel-authored trampoline page
        // `page_fault_handler`'s own fault-to-signal redirect already established (see
        // `fault_trampoline`'s own module doc comment for why a raw `extern "x86-interrupt" fn`
        // can't build a real handler-invocation frame itself) -- but stashes the real, true
        // interrupted `(rip, rsp, rflags)` first (`Process::preempted_resume`), unlike the
        // page-fault case: a page fault's own "resume" point is the faulting instruction itself
        // (real POSIX doesn't define resuming past an unhandled fault as portable anyway), but
        // this is *ordinary*, otherwise-uninterrupted ring-3 code -- resuming it after a handler
        // returns (or immediately, if no handler fires) must land back on the real original
        // instruction, not the trampoline's own internal `syscall`.
        let pid = crate::process::scheduler::current_pid();
        // Only redirect `instruction_pointer` the *first* tick a deliverable signal is found --
        // NOT unconditionally on every tick this remains true. Found live, the hard way: an
        // unconditional per-tick redirect actively prevents forward progress rather than being
        // the harmless idempotent no-op it looks like -- once redirected, the process needs a few
        // real cycles to run the trampoline's own `mov`+`syscall` and reach `syscall_dispatch`
        // (which is what actually clears the pending bit); slamming `instruction_pointer` back to
        // the trampoline's *start* on every subsequent tick, before that ever completes, resets
        // that progress every ~10ms forever -- a real, self-inflicted livelock, not a race. Once
        // `preempted_resume` is `Some`, the process is trusted to already be correctly on its way
        // through the trampoline on its own; this only fires again for a *genuinely new* signal
        // becoming deliverable after the previous one was actually consumed (`preempted_resume`
        // cleared by `syscall_dispatch`'s own `SYS_FAULT_PUMP` handling).
        // Real, live-found correctness gap: skip entirely while a signal handler is already
        // running (`!signal_stack.is_empty()`) -- this kernel's own `deliver_pending_signal`
        // chaining (`do_sigreturn` re-checking for a further deliverable signal, see that
        // function's own doc comment) already guarantees strict lowest-signal-number-first
        // delivery order across a chain of several deliverable signals, entirely via real
        // syscalls (the handler's own eventual `sigreturn`) -- no timer help is ever needed
        // there, and a currently-running handler is *always* going to make that real syscall
        // shortly (when it returns through its restorer), so this mechanism's own reason for
        // existing (a thread making *no* syscalls of its own at all) doesn't apply mid-handler.
        // Found live: a real RT-signal ordering regression (`rt_signal_syscall_smoke`'s own part
        // 3, `sigqueue/7-1.c`'s scenario) -- a timer tick landing at the *exact* instant a lower-
        // numbered signal's handler was about to execute its first instruction (a real, valid,
        // narrow window: `mask_to_add` only blocks the signal *being delivered*, not siblings)
        // found the higher-numbered sibling already pending+unblocked too and redirected *that*
        // one in first, delivering strictly out of the kernel's own documented order -- a real
        // regression this mechanism must never cause, since the ordinary chaining path already
        // had this covered without it.
        let should_redirect = {
            let mut table = crate::process::table().lock();
            table.get_mut(&pid).is_some_and(|p| {
                let deliverable = p.pending_signals & !p.blocked_signals != 0
                    && p.signal_stack.is_empty();
                if deliverable && p.preempted_resume.is_none() {
                    p.preempted_resume = Some((
                        stack_frame.instruction_pointer.as_u64(),
                        stack_frame.stack_pointer.as_u64(),
                        stack_frame.cpu_flags.bits(),
                    ));
                    true
                } else {
                    false
                }
            })
        };
        if should_redirect {
            // SAFETY: only the resume RIP is changed -- redirecting straight into a real,
            // kernel-mapped, user-executable trampoline page. RSP/RFLAGS/the segment selectors are
            // left exactly as they were; this process has exactly one ring-3 code/data selector
            // pair, already correct. Same technique `page_fault_handler` already uses.
            unsafe {
                stack_frame.as_mut().update(|f| {
                    f.instruction_pointer =
                        VirtAddr::new(crate::process::fault_trampoline::FAULT_TRAMPOLINE_VA);
                });
            }
        }
    }
}

extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    let mut port: Port<u8> = Port::new(0x60);
    // SAFETY: 0x60 is the PS/2 controller's data port; reading it is how a keyboard IRQ is
    // acknowledged at the hardware level, and it's only ever read here.
    let scancode: u8 = unsafe { port.read() };

    // Real, externally-triggered timing jitter (a human keystroke, at whatever exact cycle count
    // it happened to land) -- feeds `src/random.rs`'s persistent entropy pool. Unconditional, not
    // gated on how the scancode later decodes, so every keyboard IRQ contributes.
    crate::random::mix_entropy(scancode as u64);

    let mut keyboard = KEYBOARD.lock();
    if let Ok(Some(key_event)) = keyboard.add_byte(scancode)
        && let Some(key) = keyboard.process_keyevent(key_event)
    {
        match key {
            DecodedKey::Unicode(character) => {
                // Non-ASCII is silently dropped here -- a US keyboard layout won't produce it,
                // and it keeps sys_read's contract (raw bytes, not full UTF-8) simple.
                if character.is_ascii() {
                    let byte = character as u8;
                    // Only echo printable characters and newline directly here. Control bytes
                    // (backspace, delete, Ctrl+C, Ctrl+D, ...) are still pushed to stdin below,
                    // but *how* they should look on screen (erasing a character, printing "^C",
                    // etc.) is a userland concern -- see `userland/stsh/`'s `read_line` -- and
                    // echoing them raw here just produces VGA's placeholder glyph for anything
                    // outside 0x20..=0x7e, which isn't useful for any of them.
                    //
                    // Gated on the console's own current termios ECHO bit (see `src/stdin.rs`) --
                    // a program that's switched to raw mode with ECHO cleared (e.g. a real
                    // line-editing shell) does its own echoing; echoing here on top of that would
                    // double every keystroke. Defaults to on, matching this kernel's original,
                    // always-echo behavior before real termios existed.
                    // Real tty-driver INTR behavior: once a real session has actually claimed the
                    // controlling terminal and set a foreground process group (`TIOCSCTTY`/
                    // `TIOCSPGRP` -- see CLAUDE.md's session/controlling-tty notes), Ctrl+C (ASCII
                    // ETX, `0x03`) is intercepted here and turned into a real `SIGINT` delivered to
                    // that whole group, exactly like a real terminal driver consuming INTR before
                    // it ever reaches a reading process's buffer -- it is deliberately *not* also
                    // pushed to stdin in this case. Gated on the console's own `ISIG` bit (real
                    // convention: a program that's cleared it, same as `ECHO` above, wants raw
                    // bytes instead, e.g. a line editor that means to handle Ctrl+C itself). Until
                    // some session actually does this (the common case today -- nothing calls
                    // `setsid`/`TIOCSCTTY` yet outside `sulogin`/`getty`), `foreground_pgid()` stays
                    // `None` and this falls through to the original behavior below: the raw byte is
                    // pushed to stdin and a userland reader (`stsh`'s own `read_line`, BusyBox
                    // `hush`'s line editor) handles it itself, unchanged from before this existed.
                    if byte == 0x03
                        && crate::console::stdin::get_termios().c_lflag & crate::console::stdin::ISIG != 0
                        && let Some(pgid) = crate::console::stdin::foreground_pgid()
                    {
                        serial_print!("^C\n");
                        crate::process::signal_foreground_group(pgid, crate::process::SIGINT);
                        unsafe {
                            pic::notify_end_of_interrupt(InterruptIndex::Keyboard.as_u8());
                        }
                        return;
                    }
                    // Real tty-driver SUSP behavior, same shape as the Ctrl+C/SIGINT interception
                    // directly above (ASCII SUB, `0x1a`, is Ctrl+Z's real terminal-driver INTR-
                    // family byte) -- delivers a real SIGTSTP to the foreground group instead of
                    // SIGINT (see `ProcState::Stopped`/`process::signals`'s `Action::Stop` for what
                    // happens next: the target genuinely stops, observable via `wait4(WUNTRACED)`,
                    // resumable via a later `SIGCONT` -- `hush`'s own `fg`/`bg`/`jobs` builtins
                    // already send/observe that real machinery unmodified). Same `ISIG`/
                    // `foreground_pgid()` gating and not-also-pushed-to-stdin behavior.
                    if byte == 0x1a
                        && crate::console::stdin::get_termios().c_lflag & crate::console::stdin::ISIG != 0
                        && let Some(pgid) = crate::console::stdin::foreground_pgid()
                    {
                        serial_print!("^Z\n");
                        crate::process::signal_foreground_group(pgid, crate::process::SIGTSTP);
                        unsafe {
                            pic::notify_end_of_interrupt(InterruptIndex::Keyboard.as_u8());
                        }
                        return;
                    }
                    if crate::console::stdin::echo_enabled()
                        && (byte == b'\n' || byte == b'\r' || (0x20..=0x7e).contains(&byte))
                    {
                        serial_print!("{character}");
                    }
                    crate::console::stdin::push_byte(byte);
                }
            }
            // Modifier/lock keys (Shift, Ctrl, CapsLock, ...) and any other non-Unicode key --
            // nothing to echo or push to stdin. These used to be logged via `{key:?}` for
            // debugging during early keyboard-decode bring-up, but that printed raw debug names
            // like "LControl" inline with real typed text (e.g. right before a Ctrl+C's "^C"),
            // which is exactly the kind of noise a real shell shouldn't produce.
            DecodedKey::RawKey(_) => {}
        }
    }

    unsafe {
        pic::notify_end_of_interrupt(InterruptIndex::Keyboard.as_u8());
    }
}
