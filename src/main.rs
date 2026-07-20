#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(oxidebsd::test_runner)]
#![reexport_test_harness_main = "test_main"]

use core::panic::PanicInfo;

use bootloader::{BootInfo, entry_point};
use oxidebsd::serial_println;

entry_point!(kernel_main);

#[cfg(test)]
fn kernel_main(boot_info: &'static BootInfo) -> ! {
    serial_println!("OxideBSD kernel booting...");

    oxidebsd::init(boot_info);
    test_main();

    serial_println!("OxideBSD kernel is up, entering idle loop");

    oxidebsd::hlt_loop();
}

/// Non-test builds boot, then demonstrate paging/address-space separation/ELF loading by loading
/// and jumping into the `ring3-smoke` userland binary (see `userland/ring3-smoke/`) — see its
/// module doc comment and `usermode::jump_to_usermode` for why this never returns here.
#[cfg(not(test))]
fn kernel_main(boot_info: &'static BootInfo) -> ! {
    serial_println!("OxideBSD kernel booting...");

    let (_mapper, mut frame_allocator) = oxidebsd::init(boot_info);

    run_ring3_smoke_demo(boot_info, &mut frame_allocator)
}

#[cfg(not(test))]
fn run_ring3_smoke_demo(
    boot_info: &'static BootInfo,
    frame_allocator: &mut oxidebsd::memory::BootInfoFrameAllocator,
) -> ! {
    use oxidebsd::address_space::AddressSpace;
    use oxidebsd::elf::{self, Elf};
    use oxidebsd::usermode::jump_to_usermode;
    use x86_64::VirtAddr;
    use x86_64::structures::paging::{FrameAllocator, Mapper, Page, PageTableFlags};

    const RING3_SMOKE_ELF: &[u8] = include_bytes!(env!("RING3_SMOKE_ELF_PATH"));
    // Arbitrary, just clear of the kernel image, heap, and phys-memory-offset window.
    const USER_STACK_TOP: u64 = 0x_5000_0000_0000;
    const USER_STACK_PAGES: u64 = 4;

    let physical_memory_offset = VirtAddr::new(boot_info.physical_memory_offset);

    serial_println!(
        "[boot] ring3-smoke: building a demo address space ({} byte ELF)",
        RING3_SMOKE_ELF.len()
    );
    let address_space = AddressSpace::new(physical_memory_offset, frame_allocator);
    // SAFETY: physical_memory_offset is the bootloader's phys-memory mapping, and this is the
    // only live view of address_space's level 4 table right now.
    let mut demo_mapper = unsafe { address_space.mapper(physical_memory_offset) };

    let elf = Elf::parse(RING3_SMOKE_ELF).expect("ring3-smoke: failed to parse ELF");
    let entry = elf::load(
        &elf,
        &mut demo_mapper,
        frame_allocator,
        physical_memory_offset,
    )
    .expect("ring3-smoke: failed to load segments");

    let stack_top = VirtAddr::new(USER_STACK_TOP);
    let stack_bottom_page = Page::containing_address(stack_top - USER_STACK_PAGES * 4096);
    let stack_top_page = Page::containing_address(stack_top - 1u64);
    for page in Page::range_inclusive(stack_bottom_page, stack_top_page) {
        let frame = frame_allocator
            .allocate_frame()
            .expect("ring3-smoke: out of memory mapping the user stack");
        // SAFETY: frame was just allocated (unused, per BootInfoFrameAllocator's contract), and
        // page falls in address_space's own, not-yet-active range.
        unsafe {
            demo_mapper
                .map_to(
                    page,
                    frame,
                    PageTableFlags::PRESENT
                        | PageTableFlags::WRITABLE
                        | PageTableFlags::USER_ACCESSIBLE,
                    frame_allocator,
                )
                .expect("ring3-smoke: failed to map the user stack")
                .flush();
        }
    }

    serial_println!(
        "[boot] ring3-smoke: activating demo address space, jumping to entry {:?}",
        entry
    );
    // SAFETY: address_space has the kernel's own mappings (shallow-copied at creation) plus the
    // ELF's segments and the user stack just mapped above, so activating it and jumping to the
    // ELF's own entry point on that stack satisfies jump_to_usermode's requirements.
    unsafe {
        address_space.activate();
        jump_to_usermode(entry, stack_top)
    }
}

#[cfg(not(test))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    serial_println!("{}", info);
    oxidebsd::hlt_loop();
}

#[cfg(test)]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    oxidebsd::test_panic_handler(info)
}
