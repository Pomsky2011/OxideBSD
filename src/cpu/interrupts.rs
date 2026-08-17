use core::sync::atomic::{AtomicU64, Ordering};

use pc_keyboard::layouts::Us104Key;
use pc_keyboard::{DecodedKey, HandleControl, PS2Keyboard, ScancodeSet1};
use spin::{Lazy, Mutex};
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

extern "x86-interrupt" fn general_protection_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: u64,
) {
    serial_println!(
        "EXCEPTION: GENERAL PROTECTION FAULT (error code: {:#x})\n{:#?}",
        error_code,
        stack_frame
    );
    reboot();
}

extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
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

extern "x86-interrupt" fn timer_interrupt_handler(_stack_frame: InterruptStackFrame) {
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
        for (&pid, proc) in table.iter_mut() {
            if let crate::process::ProcState::Blocked(crate::process::BlockReason::Sleeping(
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
            }

            // `SYS_TIMER_CREATE`'s own per-timer expiry (`Process::posix_timers`, batch items
            // 6-10 of `docs/MISSING_POSIX_SYSCALLS.md`) -- same simple pending-bit-only delivery
            // as `real_timer_deadline` just above (no forced cross-process wake here either, same
            // reasoning), plus a real `timer_getoverrun` count: an expiry whose signal is *still*
            // pending from a previous, undelivered expiry increments `overrun` instead of getting
            // lost silently.
            for slot in proc.posix_timers.iter_mut().flatten() {
                if let Some(deadline) = slot.deadline
                    && now >= deadline
                {
                    if slot.signo != 0 {
                        let bit = 1 << (slot.signo - 1);
                        if proc.pending_signals & bit != 0 {
                            slot.overrun = slot.overrun.saturating_add(1);
                        } else {
                            slot.overrun = 0;
                            proc.pending_signals |= bit;
                        }
                    }
                    slot.deadline = if slot.interval_ticks > 0 {
                        Some(now + slot.interval_ticks)
                    } else {
                        None
                    };
                }
            }
        }
    }

    unsafe {
        pic::notify_end_of_interrupt(InterruptIndex::Timer.as_u8());
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
