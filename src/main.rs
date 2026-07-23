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

    // The home for whatever POSIX/libc-surface syscalls BusyBox's applets need beyond what
    // native_abi/fat32 already provide -- see CLAUDE.md's BusyBox section and
    // modules/posix_compat/src/lib.rs's own doc comment. Empty (registers nothing) at this point;
    // must still load before stsh is spawned, same as native_abi, since anything it later
    // registers needs to be in place before a program calling it can run.
    const POSIX_COMPAT_MOD: &[u8] = include_bytes!(env!("POSIX_COMPAT_MOD_PATH"));
    const POSIX_COMPAT_PANIC_SYMBOL: &str = env!("POSIX_COMPAT_MOD_PANIC_SYMBOL");
    oxidebsd::module::load(
        "posix_compat",
        POSIX_COMPAT_MOD,
        POSIX_COMPAT_PANIC_SYMBOL,
        &mut mapper,
        &mut frame_allocator,
    )
    .unwrap_or_else(|e| panic!("failed to load the posix_compat module: {e:?}"));

    // The live filesystem (see CLAUDE.md's oxfs section) -- modules/fat32 is kept in the workspace
    // (still built and self-checked by build.rs on every `cargo build`) but deliberately not
    // loaded here anymore.
    const OXFS_MOD: &[u8] = include_bytes!(env!("OXFS_MOD_PATH"));
    const OXFS_PANIC_SYMBOL: &str = env!("OXFS_MOD_PANIC_SYMBOL");
    oxidebsd::module::load(
        "oxfs",
        OXFS_MOD,
        OXFS_PANIC_SYMBOL,
        &mut mapper,
        &mut frame_allocator,
    )
    .unwrap_or_else(|e| panic!("failed to load the oxfs module: {e:?}"));

    // Modules are loaded; nothing else needs `frame_allocator`/`physical_memory_offset` as local
    // values from here on -- hand them over to memory's global state (moving frame_allocator by
    // value, not cloning it: BootInfoFrameAllocator's own bump-allocation state must stay singular,
    // never tracked in two places at once) so process::spawn/do_fork_from_current/do_execve can
    // reach them from arbitrary syscall contexts via memory::with_frame_allocator/phys_mem_offset.
    oxidebsd::memory::install_global_memory_state(frame_allocator, physical_memory_offset);

    // Registers fd 0/1/2 as real crate::fd entries (see that module's own doc comment for why
    // stdin/stdout/stderr moved out of being special-cased directly in sys_read/sys_write) --
    // must happen before any process (starting with pid 1 below) can issue its first read/write.
    oxidebsd::fd::init();

    // BusyBox's `hush` (see CLAUDE.md's BusyBox/oxfs sections) replaces `stsh` as pid 1 -- a real
    // shell over a real filesystem, not a purpose-built demo. `stsh` (`userland/stsh/`) stays in
    // the workspace, still built by build.rs, but is no longer spawned here; unlike
    // `modules/fat32`, it isn't even embedded into oxfs's filesystem (nothing execve's it, so
    // there's no reason to). `hush` prints no prompt of its own (`CONFIG_HUSH_INTERACTIVE` is off
    // -- see CLAUDE.md's BusyBox section) -- it silently blocks reading the first line, which is
    // correct, not stuck (confirmed via QEMU + injected keystrokes: ordinary commands, `cd`/`pwd`,
    // and piping all work).
    const HUSH_ELF: &[u8] = include_bytes!(env!("HUSH_ELF_PATH"));
    serial_println!(
        "[boot] spawning hush (BusyBox sh) as pid 1 ({} byte ELF)",
        HUSH_ELF.len()
    );
    let pid1 = oxidebsd::process::spawn(HUSH_ELF, None)
        .unwrap_or_else(|e| panic!("failed to spawn hush: {e:?}"));

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
