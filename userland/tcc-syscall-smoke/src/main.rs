//! Real-`SYSCALL` smoke test for TinyCC (`third_party/tinycc`, see CLAUDE.md's TinyCC section):
//! confirms `/bin/tcc`, seeded by `modules/oxfs`'s `format_fresh_filesystem` alongside every
//! BusyBox applet, can actually compile and link a real C file on target -- not just launch.
//!
//! Deliberately a real spawned ELF driven through genuine `SYSCALL`/`SYSRETQ`, not a plain Rust
//! function call from a test's own `main()` -- same reasoning every other real-`SYSCALL` smoke
//! test in this codebase documents: this is the first time this kernel has ever run a real
//! compiler+linker doing hundreds of small file opens (musl's header tree under `/usr/include`)
//! during a single syscall-driven process, exactly the class of thing worth exercising through the
//! genuine ring-3 path, not a shortcut.
//!
//! Three parts, all through `tests/tcc_syscall_smoke.rs` spawning this binary as pid 1:
//! 1. `fork` + `execve` `/bin/tcc -static -o /hello.elf /hello.c` (`/hello.c` seeded by oxfs's own
//!    `format_fresh_filesystem`, a real `printf`, not a bare `return`), `wait4` for a clean exit.
//! 2. `fork` + `execve` the just-produced `/hello.elf`, `wait4` and check its exit status --
//!    proves the *output* of a real on-target compile+link is itself a real, runnable ELF, not
//!    just that `tcc` itself exits `0`.
//! 3. The same round trip again, but via `/bin/tcc -o /hello2.elf /hello.c` -- no explicit
//!    `-static`. This kernel's `elf.rs` has no `PT_INTERP`/dynamic-relocation support at all, so a
//!    dynamically linked output (real upstream tcc's own default when neither `-static` nor
//!    `-shared` is given) would page-fault at address `0x0` the moment any indirect call went
//!    through an unresolved PLT/GOT slot -- confirmed live, the real crash this part guards
//!    against regressing. `third_party/tinycc`'s own `libtcc.c` (`tcc_new`) was patched to default
//!    `static_link = 1` unconditionally rather than relying on every caller to remember `-static`
//!    (see that file's own doc comment), so this part must now succeed exactly like part 1.
#![no_std]
#![no_main]

use core::arch::asm;
use core::hint::spin_loop;
use core::panic::PanicInfo;

const SYS_EXIT: u64 = 1;
const SYS_FORK: u64 = 2;
const SYS_WRITE: u64 = 4;
const SYS_WAIT4: u64 = 7;
const SYS_EXECVE: u64 = 59;
/// Not a real syscall number anything else in this codebase registers -- `tests/
/// tcc_syscall_smoke.rs` registers this one directly against a test-only handler, same convention
/// every other real-`SYSCALL` smoke test in this codebase uses.
const SYS_TEST_EXIT: u64 = 9999;

const STDOUT: u64 = 1;

#[inline(always)]
unsafe fn syscall(number: u64, arg0: u64, arg1: u64, arg2: u64) -> Result<u64, u64> {
    unsafe { syscall4(number, arg0, arg1, arg2, 0) }
}

#[inline(always)]
unsafe fn syscall4(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> Result<u64, u64> {
    let ret: u64;
    let failed: u8;
    unsafe {
        asm!(
            "syscall",
            "setc {failed}",
            inlateout("rax") number => ret,
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            in("r10") arg3,
            failed = out(reg_byte) failed,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    if failed != 0 { Err(ret) } else { Ok(ret) }
}

fn write_bytes(s: &[u8]) {
    unsafe {
        let _ = syscall(SYS_WRITE, STDOUT, s.as_ptr() as u64, s.len() as u64);
    }
}

fn test_exit(pass: bool) -> ! {
    unsafe {
        let _ = syscall(SYS_TEST_EXIT, if pass { 0 } else { 1 }, 0, 0);
    }
    loop {
        spin_loop();
    }
}

macro_rules! check {
    ($cond:expr, $msg:expr) => {
        if !$cond {
            write_bytes(b"tcc-syscall-smoke: FAIL: ");
            write_bytes($msg);
            write_bytes(b"\n");
            test_exit(false);
        }
    };
}

/// Wire format for `SYS_EXECVE`'s optional third argument -- see `src/process.rs`'s
/// `RawArgvEntry` (the kernel-side counterpart this must match exactly: two `u64`s, `ptr` then
/// `len`). A sequence of these describes the *complete* argv[] array, starting at argv[0],
/// terminated by a `ptr == 0` entry.
#[repr(C)]
#[derive(Clone, Copy)]
struct RawArgvEntry {
    ptr: u64,
    len: u64,
}

const MAX_ARGV: usize = 8;

/// `path` is the real fs path `execve` loads (`/bin/tcc`, `/hello.elf`); `argv` is the complete
/// argv[] including argv[0] (conventionally the program name, not necessarily equal to `path` --
/// see `RawArgvEntry`'s own doc comment). Envp is a fixed, minimal `PATH=` (empty value, present)
/// -- matches `userland/stsh/src/main.rs`'s own `ENVP` precedent; harmless here since neither `tcc`
/// nor `hello.elf` call `execvp`/care about `$PATH` at all.
fn execve(path: &[u8], argv: &[&[u8]]) -> Result<u64, u64> {
    let mut entries = [RawArgvEntry { ptr: 0, len: 0 }; MAX_ARGV + 1];
    for (i, arg) in argv.iter().enumerate() {
        entries[i] = RawArgvEntry {
            ptr: arg.as_ptr() as u64,
            len: arg.len() as u64,
        };
    }
    let argv_ptr = entries.as_ptr() as u64;
    const ENVP: &[u8] = b"PATH=";
    let envp_entries = [
        RawArgvEntry {
            ptr: ENVP.as_ptr() as u64,
            len: ENVP.len() as u64,
        },
        RawArgvEntry { ptr: 0, len: 0 },
    ];
    unsafe {
        syscall4(
            SYS_EXECVE,
            path.as_ptr() as u64,
            path.len() as u64,
            argv_ptr,
            envp_entries.as_ptr() as u64,
        )
    }
}

fn fork() -> Result<u64, u64> {
    unsafe { syscall(SYS_FORK, 0, 0, 0) }
}

fn wait4(pid: u64, status: &mut i32) -> Result<u64, u64> {
    unsafe { syscall(SYS_WAIT4, pid, status as *mut i32 as u64, 0) }
}

/// Forks, `execve`s `path`/`argv` in the child (exiting `127` on failure, matching the real shell
/// convention -- `stsh`/`hush` both use it too), `wait4`s in the parent. Returns whether the child
/// exited cleanly with status `0`.
fn run_and_wait(path: &[u8], argv: &[&[u8]]) -> bool {
    match fork() {
        Ok(0) => {
            let _ = execve(path, argv);
            unsafe {
                let _ = syscall(SYS_EXIT, 127, 0, 0);
            }
            loop {
                spin_loop();
            }
        }
        Ok(child_pid) => {
            let mut status: i32 = -1;
            wait4(child_pid, &mut status) == Ok(child_pid) && status == 0
        }
        Err(_) => {
            write_bytes(b"tcc-syscall-smoke: fork failed\n");
            false
        }
    }
}

/// Part 1 -- see this file's own module doc comment.
fn check_compile() -> bool {
    let ok = run_and_wait(
        b"/bin/tcc",
        &[b"tcc", b"-static", b"-o", b"/hello.elf", b"/hello.c"],
    );
    if ok {
        write_bytes(b"tcc-syscall-smoke: compile OK\n");
    } else {
        write_bytes(b"tcc-syscall-smoke: tcc -static -o /hello.elf /hello.c failed\n");
    }
    ok
}

/// Part 2 -- see this file's own module doc comment.
fn check_run_compiled_output() -> bool {
    let ok = run_and_wait(b"/hello.elf", &[b"hello.elf"]);
    if ok {
        write_bytes(b"tcc-syscall-smoke: compiled hello.elf ran and exited 0\n");
    } else {
        write_bytes(b"tcc-syscall-smoke: running /hello.elf failed\n");
    }
    ok
}

/// Part 3 -- see this file's own module doc comment.
fn check_compile_without_explicit_static() -> bool {
    let ok = run_and_wait(b"/bin/tcc", &[b"tcc", b"-o", b"/hello2.elf", b"/hello.c"]);
    if ok {
        write_bytes(b"tcc-syscall-smoke: compile (no -static) OK\n");
    } else {
        write_bytes(b"tcc-syscall-smoke: tcc -o /hello2.elf /hello.c failed\n");
    }
    ok && run_and_wait(b"/hello2.elf", &[b"hello2.elf"])
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    write_bytes(b"tcc-syscall-smoke: starting\n");

    check!(check_compile(), b"tcc compile+link round trip failed");
    check!(
        check_run_compiled_output(),
        b"running tcc's own compiled output failed"
    );
    check!(
        check_compile_without_explicit_static(),
        b"tcc compile+run without explicit -static failed"
    );

    write_bytes(b"tcc-syscall-smoke: PASS\n");
    test_exit(true);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        spin_loop();
    }
}
