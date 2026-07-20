//! A minimal freestanding ring-3 smoke test.
//!
//! Not part of the kernel — this is a standalone ELF binary, built by the kernel's `build.rs` and
//! embedded via `include_bytes!`, that the kernel loads and jumps into to prove paging, address
//! space separation, and ELF loading actually work. It does a bit of arithmetic (so there's real
//! work for the CPU to have done), then executes `int3`. The kernel's breakpoint handler logs the
//! exception it causes, including the CPU's privilege level at the time — `Ring3` there is the
//! proof. There is no syscall ABI yet, so this program has no way back into the kernel: after the
//! breakpoint returns, it just spins forever.
#![no_std]
#![no_main]

use core::arch::asm;
use core::hint::spin_loop;
use core::panic::PanicInfo;

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    let mut checksum: u64 = 0;
    for i in 0..1000u64 {
        checksum = checksum.wrapping_add(i);
    }

    // SAFETY: int3 is always valid to execute; it just traps to the kernel's breakpoint handler,
    // which resumes execution here afterward. `checksum` is passed in a register purely so it's
    // observable (e.g. under a debugger) rather than optimized away as dead computation.
    unsafe {
        asm!("int3", in("rax") checksum);
    }

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
