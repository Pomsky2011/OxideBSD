//! The home for whatever POSIX/libc-surface syscalls a real C program's musl-linked startup path
//! needs beyond what `modules/native_abi/` (the small, BSD-authentic core: `exit`/`read`/`write`/
//! `fork`/`wait4`/`execve`/`getpid`, plus the musl-port-driven `mmap`/`munmap`/`brk`/
//! `set_fs_base`/`writev`) and `modules/fat32/` (`open`/`close`/`chdir`/`mkdir`) already register —
//! deliberately kept a separate module rather than folded into `native_abi` further, so that one
//! stays "the authentic core" while this one carries whatever extra function real userland C
//! programs turn out to need. `true`/`echo`/`cat` didn't need anything from here; `sh` (BusyBox's
//! `hush`) is what actually filled it in, once real pipeline support (`cmd1 | cmd2`) turned out to
//! need `pipe(2)`/`dup2(2)` — see CLAUDE.md's BusyBox section.
//!
//! Same "module registers, kernel implements" split every other syscall module in this codebase
//! already uses: this crate only ever gains thin `extern "C" fn handle_x` wrappers over
//! kernel-resident `oxidebsd_sys_x` functions, never the actual behavior itself (this module can't
//! use `alloc` — see CLAUDE.md's module-loading section; the real pipe-buffer/fd-aliasing logic
//! lives in `src/pipe.rs`/`src/fd.rs`, ordinary kernel code that can).
#![no_std]

unsafe extern "C" {
    fn oxidebsd_log(ptr: *const u8, len: u64);
    fn oxidebsd_register_syscall(
        number: u64,
        handler: extern "C" fn(u64, u64, u64, u64) -> i64,
    ) -> i32;
    fn oxidebsd_sys_pipe(fds_ptr: u64) -> i64;
    fn oxidebsd_sys_dup2(oldfd: u64, newfd: u64) -> i64;
}

fn log(message: &str) {
    unsafe { oxidebsd_log(message.as_ptr(), message.len() as u64) };
}

/// `SYS_PIPE = 105`/`SYS_DUP2 = 106` — OxideBSD's own invention, numbered continuing on from the
/// musl-port-driven `mmap`/`munmap`/`brk`/`set_fs_base`/`writev` (`100`-`104`) `native_abi`
/// already registers, though both happen to match real `pipe(2)`/`dup2(2)`'s own argument shapes
/// exactly (see `src/syscall.rs`'s own doc comments on `sys_pipe`/`sys_dup2` for why neither needed
/// an invented wire format the way `open`/`execve` did).
const SYS_PIPE: u64 = 105;
const SYS_DUP2: u64 = 106;

extern "C" fn handle_pipe(fds_ptr: u64, _arg1: u64, _arg2: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_pipe(fds_ptr) }
}

extern "C" fn handle_dup2(oldfd: u64, newfd: u64, _arg2: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_dup2(oldfd, newfd) }
}

#[unsafe(no_mangle)]
pub extern "C" fn module_init() -> i32 {
    unsafe {
        oxidebsd_register_syscall(SYS_PIPE, handle_pipe);
        oxidebsd_register_syscall(SYS_DUP2, handle_dup2);
    }
    log("[module] posix_compat: module_init running (registered SYS_PIPE/SYS_DUP2)\n");
    0
}
