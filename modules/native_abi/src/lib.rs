//! Converts OxideBSD's native `int 0x80` syscall ABI's number → handler dispatch table (in
//! `src/syscall.rs`) from a hardcoded `match` into something a dynamically loaded module
//! populates — see `CLAUDE.md`'s module-loading section for the full design and, in particular,
//! why the underlying `sys_exit`/`sys_read`/`sys_write` *behavior* stays kernel-resident rather
//! than moving here too: `src/linux_syscall.rs` (a completely separate `SYSCALL`/`SYSRET`
//! mechanism, out of scope for modularization) calls those same three functions directly, so
//! duplicating their logic into this module would either break those calls or require converting
//! `linux_syscall.rs` as well. What this module actually owns is just which syscall *numbers*
//! route to which handlers — registered here, once, at `module_init` time.
//!
//! Syscall numbers (`SYS_EXIT = 1`, `SYS_READ = 3`, `SYS_WRITE = 4`) match real FreeBSD's
//! long-stable values — hand-duplicated here rather than shared via a crate with the kernel, the
//! same "no shared crate across this ABI boundary" convention already used for the raw `int 0x80`
//! stub's own constants inside `userland/*/src/main.rs`.
#![no_std]

unsafe extern "C" {
    fn oxidebsd_register_syscall(number: u64, handler: extern "C" fn(u64, u64, u64) -> i64) -> i32;
    fn oxidebsd_sys_exit(code: u64) -> !;
    fn oxidebsd_sys_read(fd: u64, ptr: u64, len: u64) -> i64;
    fn oxidebsd_sys_write(fd: u64, ptr: u64, len: u64) -> i64;
}

const SYS_EXIT: u64 = 1;
const SYS_READ: u64 = 3;
const SYS_WRITE: u64 = 4;

extern "C" fn handle_exit(code: u64, _arg1: u64, _arg2: u64) -> i64 {
    unsafe { oxidebsd_sys_exit(code) }
}

extern "C" fn handle_read(fd: u64, ptr: u64, len: u64) -> i64 {
    unsafe { oxidebsd_sys_read(fd, ptr, len) }
}

extern "C" fn handle_write(fd: u64, ptr: u64, len: u64) -> i64 {
    unsafe { oxidebsd_sys_write(fd, ptr, len) }
}

#[unsafe(no_mangle)]
pub extern "C" fn module_init() -> i32 {
    unsafe {
        oxidebsd_register_syscall(SYS_EXIT, handle_exit);
        oxidebsd_register_syscall(SYS_READ, handle_read);
        oxidebsd_register_syscall(SYS_WRITE, handle_write);
    }
    0
}
