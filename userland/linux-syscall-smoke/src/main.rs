//! A minimal, raw-assembly test of the kernel's `SYSCALL`/`SYSRET` mechanism
//! (`src/linux_syscall.rs`) — deliberately *not* musl, deliberately not even using libc-style
//! helpers. Issues Linux's real `write`(1) and `exit`(60) syscalls directly, with Linux's real
//! calling convention (number in `RAX`, args in `RDI`/`RSI`/`RDX`, `syscall` instruction), to
//! prove the mechanism itself works — GDT layout, `IA32_STAR`/`LSTAR`/`SFMASK`, the entry stub's
//! stack switch and register save/restore, `SYSRETQ` — in isolation, before musl's own startup
//! complexity (TLS setup, its allocator, the auxiliary vector, ...) gets layered on top.
#![no_std]
#![no_main]

use core::arch::asm;
use core::hint::spin_loop;
use core::panic::PanicInfo;

const SYS_WRITE: u64 = 1;
const SYS_EXIT: u64 = 60;

/// Issues a real Linux syscall: number in `rax`, up to three arguments in `rdi`/`rsi`/`rdx`,
/// return value in `rax`. `rcx`/`r11` are clobbered by the `syscall` instruction itself (it uses
/// them to save `rip`/`rflags`) — that's Linux's real convention, not a choice made here.
#[inline(always)]
unsafe fn syscall(number: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number => ret,
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    ret
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    let message = b"linux-syscall-smoke: hello via real SYSCALL/SYSRET\n";
    unsafe {
        syscall(
            SYS_WRITE,
            1, // fd 1, stdout
            message.as_ptr() as u64,
            message.len() as u64,
        );
        syscall(SYS_EXIT, 42, 0, 0);
    }

    // SYS_EXIT never returns, but that's not visible to the type system all the way out here.
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
