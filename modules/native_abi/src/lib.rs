//! Converts OxideBSD's native `SYSCALL`/`SYSRETQ` syscall ABI's number → handler dispatch table
//! (in `src/syscall.rs`) from a hardcoded `match` into something a dynamically loaded module
//! populates — see `CLAUDE.md`'s module-loading section for the full design and, in particular,
//! why the underlying `sys_exit`/`sys_read`/`sys_write`/`sys_fork`/`sys_wait4`/`sys_execve`/
//! `sys_getpid` *behavior* stays kernel-resident rather than moving here too: this module can't use
//! `alloc` (see CLAUDE.md's module-loading section), and the process table/scheduler need
//! `Vec`/`BTreeMap` freely. What this module actually owns is just which syscall *numbers* route
//! to which handlers — registered here, once, at `module_init` time.
//!
//! Syscall numbers (`SYS_EXIT = 1`, `SYS_FORK = 2`, `SYS_READ = 3`, `SYS_WRITE = 4`,
//! `SYS_WAIT4 = 7`, `SYS_GETPID = 20`, `SYS_EXECVE = 59`) match real FreeBSD's long-stable values
//! — hand-duplicated here rather than shared via a crate with the kernel, the same "no shared
//! crate across this ABI boundary" convention already used for the raw syscall stub's own
//! constants inside `userland/*/src/main.rs`.
//!
//! `SYS_MMAP = 100`/`SYS_MUNMAP = 101`/`SYS_BRK = 102`/`SYS_SET_FS_BASE = 103` are different: they
//! don't chase FreeBSD authenticity the way the numbers above do. They're OxideBSD's own
//! invention — numbers and argument shapes chosen for what porting musl's userland actually needs
//! (see `src/process.rs`'s `do_mmap`/`do_munmap`/`do_brk` and `src/syscall.rs`'s
//! `sys_set_fs_base`), not copied from any real OS's syscall table.
#![no_std]

unsafe extern "C" {
    fn oxidebsd_register_syscall(
        number: u64,
        handler: extern "C" fn(u64, u64, u64, u64) -> i64,
    ) -> i32;
    fn oxidebsd_sys_exit(code: u64) -> !;
    fn oxidebsd_sys_read(fd: u64, ptr: u64, len: u64) -> i64;
    fn oxidebsd_sys_write(fd: u64, ptr: u64, len: u64) -> i64;
    fn oxidebsd_sys_fork() -> i64;
    fn oxidebsd_sys_wait4(pid: u64, status_ptr: u64, options: u64) -> i64;
    fn oxidebsd_sys_execve(path_ptr: u64, path_len: u64, argv_ptr: u64, envp_ptr: u64) -> i64;
    fn oxidebsd_sys_getpid() -> i64;
    fn oxidebsd_sys_mmap(addr_hint: u64, len: u64, prot: u64) -> i64;
    fn oxidebsd_sys_munmap(addr: u64, len: u64) -> i64;
    fn oxidebsd_sys_brk(addr: u64) -> i64;
    fn oxidebsd_sys_set_fs_base(base: u64) -> i64;
    fn oxidebsd_sys_writev(fd: u64, iov_ptr: u64, iovcnt: u64) -> i64;
}

const SYS_EXIT: u64 = 1;
const SYS_FORK: u64 = 2;
const SYS_READ: u64 = 3;
const SYS_WRITE: u64 = 4;
const SYS_WAIT4: u64 = 7;
const SYS_GETPID: u64 = 20;
const SYS_EXECVE: u64 = 59;
const SYS_MMAP: u64 = 100;
const SYS_MUNMAP: u64 = 101;
const SYS_BRK: u64 = 102;
const SYS_SET_FS_BASE: u64 = 103;
const SYS_WRITEV: u64 = 104;

extern "C" fn handle_exit(code: u64, _arg1: u64, _arg2: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_exit(code) }
}

extern "C" fn handle_read(fd: u64, ptr: u64, len: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_read(fd, ptr, len) }
}

extern "C" fn handle_write(fd: u64, ptr: u64, len: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_write(fd, ptr, len) }
}

extern "C" fn handle_fork(_arg0: u64, _arg1: u64, _arg2: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_fork() }
}

extern "C" fn handle_wait4(pid: u64, status_ptr: u64, options: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_wait4(pid, status_ptr, options) }
}

/// The one handler that actually reads its 4th argument (`envp_ptr`, via `R10`) -- see
/// `src/syscall.rs`'s module doc comment for why `R10` only became a real, read argument once
/// `execve` needed real `envp` passthrough.
extern "C" fn handle_execve(path_ptr: u64, path_len: u64, argv_ptr: u64, envp_ptr: u64) -> i64 {
    unsafe { oxidebsd_sys_execve(path_ptr, path_len, argv_ptr, envp_ptr) }
}

extern "C" fn handle_getpid(_arg0: u64, _arg1: u64, _arg2: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_getpid() }
}

extern "C" fn handle_mmap(addr_hint: u64, len: u64, prot: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_mmap(addr_hint, len, prot) }
}

extern "C" fn handle_munmap(addr: u64, len: u64, _arg2: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_munmap(addr, len) }
}

extern "C" fn handle_brk(addr: u64, _arg1: u64, _arg2: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_brk(addr) }
}

extern "C" fn handle_set_fs_base(base: u64, _arg1: u64, _arg2: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_set_fs_base(base) }
}

extern "C" fn handle_writev(fd: u64, iov_ptr: u64, iovcnt: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_writev(fd, iov_ptr, iovcnt) }
}

#[unsafe(no_mangle)]
pub extern "C" fn module_init() -> i32 {
    unsafe {
        oxidebsd_register_syscall(SYS_EXIT, handle_exit);
        oxidebsd_register_syscall(SYS_READ, handle_read);
        oxidebsd_register_syscall(SYS_WRITE, handle_write);
        oxidebsd_register_syscall(SYS_FORK, handle_fork);
        oxidebsd_register_syscall(SYS_WAIT4, handle_wait4);
        oxidebsd_register_syscall(SYS_EXECVE, handle_execve);
        oxidebsd_register_syscall(SYS_GETPID, handle_getpid);
        oxidebsd_register_syscall(SYS_MMAP, handle_mmap);
        oxidebsd_register_syscall(SYS_MUNMAP, handle_munmap);
        oxidebsd_register_syscall(SYS_BRK, handle_brk);
        oxidebsd_register_syscall(SYS_SET_FS_BASE, handle_set_fs_base);
        oxidebsd_register_syscall(SYS_WRITEV, handle_writev);
    }
    0
}
