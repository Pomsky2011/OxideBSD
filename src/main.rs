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

/// Non-test builds boot, then demonstrate paging/address-space separation/ELF loading/the syscall
/// ABI by loading and jumping into a userland demo binary — see `run_userland_demo` and
/// `usermode::jump_to_usermode` for why this never returns here.
///
/// Currently runs `stsh` (see `userland/stsh/`), a genuinely interactive shell over OxideBSD's own
/// native, BSD-style `int 0x80` ABI (`src/syscall.rs`) — unlike every earlier demo, this one loops
/// forever reading keyboard input instead of printing a message and exiting. `ring3-smoke` and
/// `linux-syscall-smoke` (`userland/ring3-smoke/`, `userland/linux-syscall-smoke/`) still work —
/// verified separately by booting with one of them loaded instead — they're just not what's wired
/// up here at the moment, since only one demo can run per boot.
///
/// Before that, loads the `hello` kernel module (`modules/hello/`) via `oxidebsd::module::load`
/// — see `CLAUDE.md`'s module-loading section. This is the first, deliberately minimal proof that
/// dynamic module loading works end to end; later modules (the native syscall ABI, FAT32) load
/// the same way.
#[cfg(not(test))]
fn kernel_main(boot_info: &'static BootInfo) -> ! {
    serial_println!("OxideBSD kernel booting...");

    let (mut mapper, mut frame_allocator) = oxidebsd::init(boot_info);

    const HELLO_MOD: &[u8] = include_bytes!(env!("HELLO_MOD_PATH"));
    const HELLO_PANIC_SYMBOL: &str = env!("HELLO_MOD_PANIC_SYMBOL");
    oxidebsd::module::load(
        "hello",
        HELLO_MOD,
        HELLO_PANIC_SYMBOL,
        &mut mapper,
        &mut frame_allocator,
    )
    .unwrap_or_else(|e| panic!("failed to load the hello module: {e:?}"));

    // Populates src/syscall.rs's dispatch table (SYS_EXIT/SYS_READ/SYS_WRITE) -- must load before
    // stsh, below, is jumped into, since stsh's syscalls resolve through that table.
    const NATIVE_ABI_MOD: &[u8] = include_bytes!(env!("NATIVE_ABI_MOD_PATH"));
    const NATIVE_ABI_PANIC_SYMBOL: &str = env!("NATIVE_ABI_MOD_PANIC_SYMBOL");
    oxidebsd::module::load(
        "native_abi",
        NATIVE_ABI_MOD,
        NATIVE_ABI_PANIC_SYMBOL,
        &mut mapper,
        &mut frame_allocator,
    )
    .unwrap_or_else(|e| panic!("failed to load the native_abi module: {e:?}"));

    // Parses its embedded FAT32 image and runs its own self-check (logged over serial) --
    // read-only for now, see modules/fat32's doc comment. Not yet wired into any syscall (that's
    // later work), so this only proves the filesystem-format parsing itself for the moment.
    const FAT32_MOD: &[u8] = include_bytes!(env!("FAT32_MOD_PATH"));
    const FAT32_PANIC_SYMBOL: &str = env!("FAT32_MOD_PANIC_SYMBOL");
    oxidebsd::module::load(
        "fat32",
        FAT32_MOD,
        FAT32_PANIC_SYMBOL,
        &mut mapper,
        &mut frame_allocator,
    )
    .unwrap_or_else(|e| panic!("failed to load the fat32 module: {e:?}"));

    const STSH_ELF: &[u8] = include_bytes!(env!("STSH_ELF_PATH"));
    // Arbitrary, just clear of the kernel image, heap, and phys-memory-offset window.
    const USER_STACK_TOP: u64 = 0x_5000_0000_0000;

    run_userland_demo(
        boot_info,
        &mut frame_allocator,
        "stsh",
        STSH_ELF,
        USER_STACK_TOP,
    )
}

/// Builds a fresh demo address space, loads `elf_bytes` into it, maps a small user stack ending at
/// `user_stack_top`, and jumps into ring 3 at the ELF's entry point. `name` is only used for
/// `[boot]`-tagged logging. Never returns: see `usermode::jump_to_usermode`.
#[cfg(not(test))]
fn run_userland_demo(
    boot_info: &'static BootInfo,
    frame_allocator: &mut oxidebsd::memory::BootInfoFrameAllocator,
    name: &str,
    elf_bytes: &[u8],
    user_stack_top: u64,
) -> ! {
    use oxidebsd::address_space::AddressSpace;
    use oxidebsd::elf::{self, Elf};
    use oxidebsd::usermode::jump_to_usermode;
    use x86_64::VirtAddr;
    use x86_64::structures::paging::{FrameAllocator, Mapper, Page, PageTableFlags};

    const USER_STACK_PAGES: u64 = 4;

    let physical_memory_offset = VirtAddr::new(boot_info.physical_memory_offset);

    serial_println!(
        "[boot] {}: building a demo address space ({} byte ELF)",
        name,
        elf_bytes.len()
    );
    let address_space = AddressSpace::new(physical_memory_offset, frame_allocator);
    // SAFETY: physical_memory_offset is the bootloader's phys-memory mapping, and this is the
    // only live view of address_space's level 4 table right now.
    let mut demo_mapper = unsafe { address_space.mapper(physical_memory_offset) };

    let elf =
        Elf::parse(elf_bytes).unwrap_or_else(|e| panic!("{name}: failed to parse ELF: {e:?}"));
    let entry = elf::load(
        &elf,
        &mut demo_mapper,
        frame_allocator,
        physical_memory_offset,
    )
    .unwrap_or_else(|e| panic!("{name}: failed to load segments: {e:?}"));

    let stack_top = VirtAddr::new(user_stack_top);
    let stack_bottom_page = Page::containing_address(stack_top - USER_STACK_PAGES * 4096);
    let stack_top_page = Page::containing_address(stack_top - 1u64);
    for page in Page::range_inclusive(stack_bottom_page, stack_top_page) {
        let frame = frame_allocator
            .allocate_frame()
            .unwrap_or_else(|| panic!("{name}: out of memory mapping the user stack"));
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
                .unwrap_or_else(|_| panic!("{name}: failed to map the user stack"))
                .flush();
        }
    }

    serial_println!(
        "[boot] {}: activating demo address space, jumping to entry {:?}",
        name,
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
