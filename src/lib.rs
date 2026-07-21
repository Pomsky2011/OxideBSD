#![no_std]
#![cfg_attr(test, no_main)]
#![feature(custom_test_frameworks)]
#![feature(abi_x86_interrupt)]
#![test_runner(crate::test_runner)]
#![reexport_test_harness_main = "test_main"]

extern crate alloc;

pub mod address_space;
pub mod allocator;
pub mod elf;
pub mod gdt;
pub mod interrupts;
pub mod memory;
pub mod pic;
pub mod qemu;
pub mod reboot;
pub mod serial;
pub mod syscall;
pub mod usermode;
pub mod vga;

use core::panic::PanicInfo;

use bootloader::BootInfo;
#[cfg(test)]
use bootloader::entry_point;
use qemu::{QemuExitCode, exit_qemu};

/// Brings up the kernel: GDT/TSS, IDT, PIC + hardware interrupts, paging, and the heap.
///
/// Returns the kernel's own page-table mapper and physical frame allocator so callers can keep
/// using the *same* frame allocator afterward (e.g. to build further address spaces) — a second,
/// separately-`init`'d `BootInfoFrameAllocator` would restart handing out frames from the start of
/// the usable memory map, re-allocating ones the heap has already claimed.
pub fn init(
    boot_info: &'static BootInfo,
) -> (
    x86_64::structures::paging::OffsetPageTable<'static>,
    memory::BootInfoFrameAllocator,
) {
    serial_println!("[boot] kernel initialization starting");

    gdt::init();
    interrupts::init_idt();
    interrupts::init_pics();

    serial_println!("[boot] enabling interrupts");
    x86_64::instructions::interrupts::enable();

    let phys_mem_offset = x86_64::VirtAddr::new(boot_info.physical_memory_offset);
    let mut mapper = unsafe { memory::init(phys_mem_offset) };
    let mut frame_allocator =
        unsafe { memory::BootInfoFrameAllocator::init(&boot_info.memory_map) };

    allocator::init_heap(&mut mapper, &mut frame_allocator).expect("heap initialization failed");

    serial_println!("[boot] kernel initialization complete");

    (mapper, frame_allocator)
}

pub trait Testable {
    fn run(&self);
}

impl<T: Fn()> Testable for T {
    fn run(&self) {
        serial_print!("{}...\t", core::any::type_name::<T>());
        self();
        serial_println!("[ok]");
    }
}

pub fn test_runner(tests: &[&dyn Testable]) {
    serial_println!("running {} tests", tests.len());
    for test in tests {
        test.run();
    }
    exit_qemu(QemuExitCode::Success);
}

pub fn test_panic_handler(info: &PanicInfo) -> ! {
    serial_println!("[failed]\n");
    serial_println!("error: {}\n", info);
    exit_qemu(QemuExitCode::Failed);
    hlt_loop();
}

/// Spins forever and never returns.
///
/// This deliberately does not use `hlt`: `hlt` only resumes on the next interrupt, so if it's
/// ever reached with interrupts disabled (e.g. a panic during a `without_interrupts` critical
/// section) the CPU parks on that single instruction forever — indistinguishable from a genuine
/// halt/crash from outside the VM. A plain spin loop keeps the CPU visibly executing regardless
/// of interrupt state, at the cost of burning a full core doing nothing.
pub fn hlt_loop() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[cfg(test)]
entry_point!(test_kernel_main);

#[cfg(test)]
fn test_kernel_main(boot_info: &'static BootInfo) -> ! {
    init(boot_info);
    test_main();
    hlt_loop();
}

#[test_case]
fn test_breakpoint_exception() {
    x86_64::instructions::interrupts::int3();
}

#[test_case]
fn test_timer_interrupt_fires() {
    let ticks_before = interrupts::ticks();
    while interrupts::ticks() == ticks_before {
        x86_64::instructions::hlt();
    }
    assert!(interrupts::ticks() > ticks_before);
}

#[test_case]
fn test_syscall_dispatch_rejects_unknown_number() {
    let unknown = syscall::SYS_WRITE + 1000;
    assert_eq!(syscall::syscall_dispatch(unknown, 0, 0, 0), u64::MAX);
}

#[test_case]
fn test_heap_allocation() {
    use alloc::boxed::Box;
    use alloc::vec::Vec;

    let heap_value = Box::new(41);
    assert_eq!(*heap_value, 41);

    let mut vec = Vec::new();
    for i in 0..500 {
        vec.push(i);
    }
    assert_eq!(vec.iter().sum::<u64>(), (0..500).sum());
}

#[cfg(test)]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    test_panic_handler(info)
}
