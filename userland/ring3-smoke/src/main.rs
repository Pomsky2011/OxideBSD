//! A minimal freestanding ring-3 smoke test.
//!
//! Not part of the kernel — this is a standalone ELF binary, built by the kernel's `build.rs` and
//! embedded via `include_bytes!`, that the kernel loads and jumps into to prove paging, address
//! space separation, ELF loading, and the syscall ABI all actually work. It does a bit of
//! arithmetic (so there's real work for the CPU to have done), writes a message via the `SYS_WRITE`
//! syscall, then terminates via `SYS_EXIT` with the computed checksum as its exit code.
//!
//! The syscall numbers/vector/register convention here must match `src/syscall.rs` in the kernel
//! exactly — there's no shared crate between the two, this is the ABI boundary itself.
#![no_std]
#![no_main]

use core::arch::asm;
use core::hint::spin_loop;
use core::panic::PanicInfo;

const SYSCALL_VECTOR: u32 = 0x80;
const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;
const STDOUT: u64 = 1;

/// Issues a syscall: number in `rdi`, up to three arguments in `rsi`/`rdx`/`rcx`, return value in
/// `rax`. The kernel's `syscall_entry` preserves every other register, so nothing here needs to be
/// marked clobbered.
#[inline(always)]
unsafe fn syscall(number: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "int {vector}",
            vector = const SYSCALL_VECTOR,
            in("rdi") number,
            in("rsi") arg0,
            in("rdx") arg1,
            in("rcx") arg2,
            out("rax") ret,
        );
    }
    ret
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    let mut checksum: u64 = 0;
    for i in 0..1000u64 {
        checksum = checksum.wrapping_add(i);
    }

    let message = b"ring3-smoke: hello from ring 3, via SYS_WRITE\n";
    unsafe {
        syscall(
            SYS_WRITE,
            STDOUT,
            message.as_ptr() as u64,
            message.len() as u64,
        );
        syscall(SYS_EXIT, checksum, 0, 0);
    }

    // SYS_EXIT never returns, but the kernel's dispatcher is a normal Rust match, not something
    // the type system knows diverges from all the way out here through the asm! call above.
    loop {
        spin_loop();
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        spin_loop();
    }
}
