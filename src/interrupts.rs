use core::sync::atomic::{AtomicU64, Ordering};

use pic8259::ChainedPics;
use spin::{Lazy, Mutex};
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use crate::gdt::DOUBLE_FAULT_IST_INDEX;
use crate::reboot::reboot;
use crate::serial_println;

/// The 8259 PIC pair defaults to mapping IRQ0-15 onto interrupt vectors 0x8-0xF, which collides
/// with CPU exception vectors (0x0-0x1F) — so both PICs are remapped to start right after them.
const PIC_1_OFFSET: u8 = 32;
const PIC_2_OFFSET: u8 = PIC_1_OFFSET + 8;

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
enum InterruptIndex {
    Timer = PIC_1_OFFSET,
}

impl InterruptIndex {
    fn as_u8(self) -> u8 {
        self as u8
    }
}

static PICS: Mutex<ChainedPics> =
    // SAFETY: PIC_1_OFFSET/PIC_2_OFFSET place both PICs' vectors outside the CPU exception range.
    Mutex::new(unsafe { ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) });

static TICKS: AtomicU64 = AtomicU64::new(0);

/// Number of timer interrupts handled since boot.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

static IDT: Lazy<InterruptDescriptorTable> = Lazy::new(|| {
    let mut idt = InterruptDescriptorTable::new();

    idt.breakpoint.set_handler_fn(breakpoint_handler);
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

    idt
});

pub fn init_idt() {
    serial_println!(
        "[boot] loading IDT: breakpoint, invalid_opcode, general_protection_fault, page_fault, \
         double_fault, timer (vector {:#x})",
        InterruptIndex::Timer.as_u8()
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
        PICS.lock().initialize();
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
        PICS.lock()
            .notify_end_of_interrupt(InterruptIndex::Timer.as_u8());
    }
}
