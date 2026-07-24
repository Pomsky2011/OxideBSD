use core::sync::atomic::{AtomicU64, Ordering};

use pc_keyboard::layouts::Us104Key;
use pc_keyboard::{DecodedKey, HandleControl, PS2Keyboard, ScancodeSet1};
use spin::{Lazy, Mutex};
use x86_64::instructions::port::Port;
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use crate::gdt::DOUBLE_FAULT_IST_INDEX;
use crate::pic::{self, PIC_1_OFFSET, PIC_2_OFFSET};
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

    idt
});

pub fn init_idt() {
    serial_println!(
        "[boot] loading IDT: breakpoint, invalid_opcode, general_protection_fault, page_fault, \
         double_fault, timer (vector {:#x}), keyboard (vector {:#x})",
        InterruptIndex::Timer.as_u8(),
        InterruptIndex::Keyboard.as_u8(),
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
    TICKS.fetch_add(1, Ordering::Relaxed);

    unsafe {
        pic::notify_end_of_interrupt(InterruptIndex::Timer.as_u8());
    }
}

extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    let mut port: Port<u8> = Port::new(0x60);
    // SAFETY: 0x60 is the PS/2 controller's data port; reading it is how a keyboard IRQ is
    // acknowledged at the hardware level, and it's only ever read here.
    let scancode: u8 = unsafe { port.read() };

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
                    if crate::stdin::echo_enabled()
                        && (byte == b'\n' || byte == b'\r' || (0x20..=0x7e).contains(&byte))
                    {
                        serial_print!("{character}");
                    }
                    crate::stdin::push_byte(byte);
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
