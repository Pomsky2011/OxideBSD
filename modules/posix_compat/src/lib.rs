//! The home for whatever POSIX/libc-surface syscalls a real C program's musl-linked startup path
//! needs beyond what `modules/native_abi/` (the small, BSD-authentic core: `exit`/`read`/`write`/
//! `fork`/`wait4`/`execve`/`getpid`, plus the musl-port-driven `mmap`/`munmap`/`brk`/
//! `set_fs_base`/`writev`) and `modules/fat32/` (`open`/`close`/`chdir`/`mkdir`) already register —
//! deliberately kept a separate module rather than folded into `native_abi` further, so that one
//! stays "the authentic core" while this one carries whatever extra function real userland C
//! programs turn out to need (BusyBox's `true`/`echo` applets first — see CLAUDE.md's BusyBox
//! section — likely followed by more as later applets are ported).
//!
//! Empty for now: `true`/`echo` haven't needed anything beyond what `native_abi`/`fat32` already
//! provide yet — this module exists so there's a place to register the next one the moment
//! booting in QEMU turns up a `[boot] unrecognized syscall number N` line that calls for it,
//! without having to grow `native_abi` further to do so. Same "module registers, kernel
//! implements" split every other syscall module in this codebase already uses: this crate would
//! only ever gain thin `extern "C" fn handle_x` wrappers over kernel-resident `oxidebsd_sys_x`
//! functions, never the actual behavior itself (this module can't use `alloc` — see CLAUDE.md's
//! module-loading section).
#![no_std]

unsafe extern "C" {
    fn oxidebsd_log(ptr: *const u8, len: u64);
}

fn log(message: &str) {
    unsafe { oxidebsd_log(message.as_ptr(), message.len() as u64) };
}

#[unsafe(no_mangle)]
pub extern "C" fn module_init() -> i32 {
    log("[module] posix_compat: module_init running (no syscalls registered yet)\n");
    0
}
