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

/// Non-test builds boot, load the kernel modules that populate the native syscall ABI's dispatch
/// table and filesystem support, spawn the first real process (`stsh`, pid 1), and hand off to the
/// scheduler — see `oxidebsd::process::spawn` and `oxidebsd::scheduler::start` for why this never
/// returns here (the same one-way shape `usermode::jump_to_usermode` always had, just reached
/// through the scheduler's own first-run trampoline now instead of a direct call).
///
/// `stsh` (see `userland/stsh/`) is a genuinely interactive shell over OxideBSD's own native,
/// BSD-style `SYSCALL`/`SYSRETQ` ABI (`src/syscall.rs`), and — now that `fork`/`execve`/`wait4` are
/// real — can `fork`+`execve`+`wait` other programs instead of only running shell built-ins.
/// `ring3-smoke` (`userland/ring3-smoke/`) isn't spawned directly at boot; it's instead embedded
/// into the FAT32 image (see `build.rs`) so `stsh` can `execve` it as a real file.
///
/// Before that, loads the `hello` kernel module (`modules/hello/`) via `oxidebsd::module::load`
/// — see `CLAUDE.md`'s module-loading section. This is the first, deliberately minimal proof that
/// dynamic module loading works end to end; later modules (the native syscall ABI, FAT32) load
/// the same way.
#[cfg(not(test))]
fn kernel_main(boot_info: &'static BootInfo) -> ! {
    serial_println!("OxideBSD kernel booting...");

    let (mut mapper, mut frame_allocator) = oxidebsd::init(boot_info);
    let physical_memory_offset = x86_64::VirtAddr::new(boot_info.physical_memory_offset);

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

    // Populates src/syscall.rs's dispatch table (SYS_EXIT/SYS_READ/SYS_WRITE/SYS_FORK/SYS_WAIT4/
    // SYS_EXECVE/SYS_GETPID) -- must load before stsh, below, is spawned, since stsh's syscalls
    // resolve through that table.
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

    // Modules are loaded; nothing else needs `frame_allocator`/`physical_memory_offset` as local
    // values from here on -- hand them over to memory's global state (moving frame_allocator by
    // value, not cloning it: BootInfoFrameAllocator's own bump-allocation state must stay singular,
    // never tracked in two places at once) so process::spawn/do_fork_from_current/do_execve can
    // reach them from arbitrary syscall contexts via memory::with_frame_allocator/phys_mem_offset.
    oxidebsd::memory::install_global_memory_state(frame_allocator, physical_memory_offset);

    const STSH_ELF: &[u8] = include_bytes!(env!("STSH_ELF_PATH"));
    serial_println!(
        "[boot] spawning stsh as pid 1 ({} byte ELF)",
        STSH_ELF.len()
    );
    let pid1 = oxidebsd::process::spawn(STSH_ELF, None)
        .unwrap_or_else(|e| panic!("failed to spawn stsh: {e:?}"));

    oxidebsd::scheduler::start(pid1)
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
