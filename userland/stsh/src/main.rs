//! stsh ("stupidshell") — a genuinely minimal interactive shell for OxideBSD.
//!
//! Not part of the kernel — this is a standalone ELF binary, built by the kernel's `build.rs` and
//! embedded via `include_bytes!`, that the kernel loads and jumps into. Unlike the earlier
//! userland demo (`ring3-smoke`), this one doesn't print a message and
//! exit — it loops forever, reading a line at a time from the keyboard (via `SYS_READ`, busy-polled
//! since reads are non-blocking — see `CLAUDE.md`'s shell section for why) and dispatching a
//! small set of built-in commands, until `exit` is typed.
//!
//! **Basic line editing, still deliberately not full readline** — Backspace/Delete erase the
//! last typed byte, Ctrl+C aborts the in-progress line, and Ctrl+D on an empty line exits the
//! shell (see `read_line`'s own doc comment for the details), but there's no cursor movement
//! (arrow keys) and no history.
//!
//! `cat`/`write` exercise `modules/fat32/`'s `SYS_OPEN`/`SYS_CLOSE` (registered dynamically by
//! that module, not built into the kernel) end to end: `cat <name>` opens read-only and streams
//! bytes out via `SYS_READ` until it returns `0` (clean EOF, not "try again" — unlike stdin's own
//! non-blocking `SYS_READ`, a FAT32 file's read never needs busy-polling); `write <name> <text>`
//! opens with `O_CREAT`, writes `<text>` in one `SYS_WRITE` call, then closes (which is what
//! actually commits the file to the fat32 module's in-memory disk — see that module's own doc
//! comment on why writes are all-at-once-on-close, not incremental).
//!
//! `cd`/`ls`/`mkdir` are still just built-in commands here, dispatched the same way as `cat`/
//! `write` — not separate programs. `cd` has to be a shell built-in regardless of process support:
//! changing the *shell's own* notion of "current directory" from a separate child process is
//! impossible, since a child can't reach back into its parent's state. `ls` reuses the exact same
//! `SYS_OPEN`/`SYS_READ`/`SYS_CLOSE` sequence as `cat` — `modules/fat32/`'s `fat32_open` recognizes
//! a directory target and hands back a formatted listing instead of file content, so no separate
//! syscall was needed for it.
//!
//! Any other command name is treated as a real program to run: `run_command` `fork`s, the child
//! `execve`s the typed word as a path (against the same FAT32-backed filesystem `cat`/`write`
//! already use — see `build.rs`'s embedded `SMOKE.ELF`, a copy of `userland/ring3-smoke/`, for a
//! ready-made target), and the parent `wait4`s for it. If `execve` fails (e.g. the name doesn't
//! exist — `ENOENT`), the child itself prints "unknown command" and exits `127`, matching real
//! shell behavior; the parent's `wait4` always runs either way, so the prompt reliably comes back.
//!
//! The syscall numbers/register convention here must match `src/syscall.rs` in the kernel
//! exactly — there's no shared crate between the two, this is the ABI boundary itself.
#![no_std]
#![no_main]

use core::arch::asm;
use core::hint::spin_loop;
use core::panic::PanicInfo;

const SYS_EXIT: u64 = 1;
const SYS_FORK: u64 = 2;
const SYS_READ: u64 = 3;
const SYS_WRITE: u64 = 4;
const SYS_OPEN: u64 = 5;
const SYS_CLOSE: u64 = 6;
const SYS_WAIT4: u64 = 7;
const SYS_CHDIR: u64 = 12;
const SYS_MKDIR: u64 = 136;
const SYS_EXECVE: u64 = 59;
const STDIN: u64 = 0;
const STDOUT: u64 = 1;
/// Real POSIX `O_CREAT`'s value, not an arbitrary bit -- see `modules/fat32/src/lib.rs`'s own
/// `O_CREAT` doc comment for why this constant must match `fat32_open`'s exactly.
const O_CREAT: u64 = 0o100;

const LINE_CAPACITY: usize = 128;
/// Bounds how many words past the command name `execve`'s argv[1..] can carry -- a sanity cap,
/// not a deliberate limit; `LINE_CAPACITY`-worth of single-space-separated words can't come close
/// to this many anyway.
const MAX_ARGV: usize = 16;

/// Issues a syscall via `SYSCALL`: number in `rax`, up to three arguments in `rdi`/`rsi`/`rdx`.
/// Success/failure comes back via the carry flag (OxideBSD's native, BSD-style convention) —
/// `Ok(value)` if `CF` came back clear, `Err(errno)` if it came back set. `rcx`/`r11` must be
/// declared clobbered: `SYSCALL` itself overwrites them (to save `RIP`/`RFLAGS` on entry), so
/// whatever this program had in them beforehand doesn't survive the call — the kernel's
/// `syscall_entry` preserves every other register. Delegates to `syscall4` with a zeroed 4th
/// argument — every existing call site here predates the ABI's 4th argument (`r10`) becoming a
/// real, read one (see `src/syscall.rs`'s module doc comment in the kernel tree), and none of them
/// need it.
#[inline(always)]
unsafe fn syscall(number: u64, arg0: u64, arg1: u64, arg2: u64) -> Result<u64, u64> {
    unsafe { syscall4(number, arg0, arg1, arg2, 0) }
}

/// Like `syscall`, but with a real 4th argument in `r10` — needed by `execve`'s own wrapper below
/// to pass `envp_ptr` explicitly (leaving `r10` unset would leak whatever garbage happened to be
/// in it, which the kernel's `do_execve` would then misread as a bogus `envp_ptr` and dereference
/// — not hypothetical, since every handler that reads a 4th argument at all now expects one).
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

fn write_decimal(value: u64) {
    if value == 0 {
        write_bytes(b"0");
        return;
    }
    let mut digits = [0u8; 20];
    let mut count = 0;
    let mut remaining = value;
    while remaining > 0 {
        digits[count] = b'0' + (remaining % 10) as u8;
        remaining /= 10;
        count += 1;
    }
    digits[..count].reverse();
    write_bytes(&digits[..count]);
}

/// Blocks (by busy-polling `SYS_READ`, which is itself non-blocking) until one byte is available.
fn read_byte() -> u8 {
    let mut byte = [0u8; 1];
    loop {
        let n = unsafe { syscall(SYS_READ, STDIN, byte.as_mut_ptr() as u64, 1) };
        if n == Ok(1) {
            return byte[0];
        }
        spin_loop();
    }
}

const BACKSPACE: u8 = 0x08;
const DELETE: u8 = 0x7f;
const CTRL_C: u8 = 0x03;
const CTRL_D: u8 = 0x04;

/// Reads one line (up to `LINE_CAPACITY` bytes; anything past that is silently discarded, not
/// buffered) terminated by `\n` or `\r`.
///
/// Basic line editing, not full readline: Backspace and Delete both erase the most recently typed
/// byte (there's no cursor movement, so "forward delete" and "backspace" can't be told apart --
/// treating them the same is the pragmatic choice). Erasing on screen means writing a single raw
/// 0x08 byte, *not* the full "\x08 \x08" terminal idiom -- both `src/serial.rs`'s `SerialPort`
/// and `src/vga.rs`'s `Writer` already expand a lone backspace byte into that idiom themselves
/// (see their own doc comments), so writing the already-expanded sequence here would double it
/// up. Ctrl+C aborts the in-progress line (prints "^C" and returns as if an empty line had been
/// entered, so the caller's `run_command` no-ops and the main loop just reprompts). Ctrl+D on an
/// empty line signals EOF and exits the shell, matching real shell convention; on a non-empty
/// line it's ignored, since there's no cursor position to delete "under". Any other control byte
/// is dropped rather than inserted literally into the line.
fn read_line(line: &mut [u8; LINE_CAPACITY]) -> usize {
    let mut len = 0;
    loop {
        let byte = read_byte();
        match byte {
            b'\n' | b'\r' => {
                write_bytes(b"\n");
                return len;
            }
            BACKSPACE | DELETE => {
                if len > 0 {
                    len -= 1;
                    write_bytes(&[BACKSPACE]);
                }
            }
            CTRL_C => {
                write_bytes(b"^C\n");
                return 0;
            }
            CTRL_D => {
                if len == 0 {
                    unsafe {
                        let _ = syscall(SYS_EXIT, 0, 0, 0);
                    }
                }
            }
            // Printable characters are already echoed live by the kernel's keyboard IRQ handler
            // (see `src/interrupts.rs`) as they're typed, so nothing to write here -- just buffer
            // them.
            0x20..=0x7e => {
                if len < line.len() {
                    line[len] = byte;
                    len += 1;
                }
            }
            _ => {}
        }
    }
}

fn trim(mut s: &[u8]) -> &[u8] {
    while let [b' ', rest @ ..] = s {
        s = rest;
    }
    while let [rest @ .., b' '] = s {
        s = rest;
    }
    s
}

fn split_first_word(s: &[u8]) -> (&[u8], &[u8]) {
    match s.iter().position(|&b| b == b' ') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, &[]),
    }
}

/// Splits one word off the front of `s`: a leading `"` starts a quoted word running up to the next
/// `"` (quotes stripped from the result, not included as literal characters -- an unterminated
/// quote just takes the rest of `s` as the word rather than erroring); otherwise, splits on the
/// next space exactly like `split_first_word`. No escaping, no single-quote support, no nesting --
/// just enough to let `"two words"` become one `execve` argument instead of two.
fn split_word_maybe_quoted(s: &[u8]) -> (&[u8], &[u8]) {
    match s {
        [b'"', rest @ ..] => match rest.iter().position(|&b| b == b'"') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, &rest[rest.len()..]),
        },
        _ => split_first_word(s),
    }
}

/// Splits `s` into up to `MAX_ARGV` words (via `split_word_maybe_quoted`, so `"..."` groups count
/// as one word) -- these become `execve`'s argv[1..] in `run_program`/`execve` below. Words past
/// `MAX_ARGV` are silently dropped, not buffered or erred on, same "quietly bounded, not unbounded"
/// choice `read_line`'s own `LINE_CAPACITY` overflow handling already makes.
fn split_words<'a>(s: &'a [u8], out: &mut [&'a [u8]; MAX_ARGV]) -> usize {
    let mut count = 0;
    let mut rest = trim(s);
    while !rest.is_empty() && count < out.len() {
        let (word, tail) = split_word_maybe_quoted(rest);
        out[count] = word;
        count += 1;
        rest = trim(tail);
    }
    count
}

fn run_command(line: &[u8]) {
    let line = trim(line);
    if line.is_empty() {
        return;
    }

    let (command, rest) = split_first_word(line);
    match command {
        b"help" => write_bytes(
            b"commands: help, echo <text>, exit, cat <name>, write <name> <text>, cd [path], \
              ls [path], mkdir <name>; anything else is run as a program (e.g. smoke.elf)\n",
        ),
        b"echo" => {
            write_bytes(trim(rest));
            write_bytes(b"\n");
        }
        b"exit" => unsafe {
            let _ = syscall(SYS_EXIT, 0, 0, 0);
        },
        b"cat" => cat(trim(rest)),
        b"write" => run_write(rest),
        b"cd" => cd(trim(rest)),
        b"ls" => ls(trim(rest)),
        b"mkdir" => mkdir(trim(rest)),
        _ => run_program(command, rest),
    }
}

/// `fork`/`wait4`/`execve` wrappers -- same generic `syscall()` helper every other command here
/// already uses, no new inline asm needed.
fn fork() -> Result<u64, u64> {
    unsafe { syscall(SYS_FORK, 0, 0, 0) }
}

fn wait4(pid: u64, status: &mut i32, options: u64) -> Result<u64, u64> {
    unsafe { syscall(SYS_WAIT4, pid, status as *mut i32 as u64, options) }
}

/// Wire format for `SYS_EXECVE`'s optional third argument -- see `src/process.rs`'s
/// `RawArgvEntry` (the kernel-side counterpart this must match exactly: two `u64`s, `ptr` then
/// `len`). A sequence of these describes argv[1..] (argv[0] is always the `path` `execve` itself
/// was given), terminated by a `ptr == 0` entry.
#[repr(C)]
#[derive(Clone, Copy)]
struct RawArgvEntry {
    ptr: u64,
    len: u64,
}

/// `extra_args` becomes argv[1..] -- see `RawArgvEntry`. An empty `extra_args` passes `argv_ptr =
/// 0`, the same "no extra args" wire value every pre-existing `execve` call site here already
/// relied on before this parameter existed, so `cat`/`ls`/etc.'s internal machinery (none of which
/// calls `execve`) and every other caller stay unaffected.
fn execve(path: &[u8], extra_args: &[&[u8]]) -> Result<u64, u64> {
    let mut entries = [RawArgvEntry { ptr: 0, len: 0 }; MAX_ARGV + 1];
    for (i, arg) in extra_args.iter().enumerate() {
        entries[i] = RawArgvEntry {
            ptr: arg.as_ptr() as u64,
            len: arg.len() as u64,
        };
    }
    // entries[extra_args.len()] is still the {0, 0} terminator from initialization above.
    let argv_ptr = if extra_args.is_empty() {
        0
    } else {
        entries.as_ptr() as u64
    };
    // envp_ptr = 0: stsh has no real environment of its own to forward (it's the top of this
    // system's process tree, not spawned by any shell that gave it one) -- explicit 0, not a
    // stray/uninitialized r10, which do_execve would otherwise try to read as a real envp array.
    unsafe {
        syscall4(
            SYS_EXECVE,
            path.as_ptr() as u64,
            path.len() as u64,
            argv_ptr,
            0,
        )
    }
}

/// Anything that isn't a recognized built-in is treated as a real program to run: `fork`, `execve`
/// the typed word as a path (with `rest`, split on whitespace, as its arguments) in the child,
/// `wait4` in the parent. `execve` only ever "returns" here on failure -- on success the kernel
/// redirects the child's own execution directly, so the `is_err()` check below is the only path
/// that can run afterward.
fn run_program(name: &[u8], rest: &[u8]) {
    let mut argv_buf: [&[u8]; MAX_ARGV] = [&[]; MAX_ARGV];
    let argc = split_words(rest, &mut argv_buf);
    match fork() {
        Ok(0) => {
            if execve(name, &argv_buf[..argc]).is_err() {
                write_bytes(b"unknown command: ");
                write_bytes(name);
                write_bytes(b"\n");
            }
            unsafe {
                let _ = syscall(SYS_EXIT, 127, 0, 0);
            }
        }
        Ok(child_pid) => {
            // Parent: always wait, regardless of how the child fared, so the prompt reliably
            // comes back.
            let mut status: i32 = 0;
            let _ = wait4(child_pid, &mut status, 0);
        }
        Err(errno) => {
            write_bytes(b"fork failed, errno ");
            write_decimal(errno);
            write_bytes(b"\n");
        }
    }
}

fn cd(path: &[u8]) {
    if let Err(errno) = unsafe { syscall(SYS_CHDIR, path.as_ptr() as u64, path.len() as u64, 0) } {
        write_bytes(b"cd: failed, errno ");
        write_decimal(errno);
        write_bytes(b"\n");
    }
}

fn ls(path: &[u8]) {
    let fd = match unsafe { syscall(SYS_OPEN, path.as_ptr() as u64, path.len() as u64, 0) } {
        Ok(fd) => fd,
        Err(errno) => {
            write_bytes(b"ls: open failed, errno ");
            write_decimal(errno);
            write_bytes(b"\n");
            return;
        }
    };
    let mut byte = [0u8; 1];
    loop {
        match unsafe { syscall(SYS_READ, fd, byte.as_mut_ptr() as u64, 1) } {
            Ok(0) => break,
            Ok(_) => write_bytes(&byte),
            Err(_) => break,
        }
    }
    unsafe {
        let _ = syscall(SYS_CLOSE, fd, 0, 0);
    }
}

fn mkdir(name: &[u8]) {
    if name.is_empty() {
        write_bytes(b"usage: mkdir <name>\n");
        return;
    }
    match unsafe { syscall(SYS_MKDIR, name.as_ptr() as u64, name.len() as u64, 0) } {
        Ok(_) => write_bytes(b"created\n"),
        Err(errno) => {
            write_bytes(b"mkdir: failed, errno ");
            write_decimal(errno);
            write_bytes(b"\n");
        }
    }
}

fn cat(name: &[u8]) {
    if name.is_empty() {
        write_bytes(b"usage: cat <name>\n");
        return;
    }
    let fd = match unsafe { syscall(SYS_OPEN, name.as_ptr() as u64, name.len() as u64, 0) } {
        Ok(fd) => fd,
        Err(errno) => {
            write_bytes(b"cat: open failed, errno ");
            write_decimal(errno);
            write_bytes(b"\n");
            return;
        }
    };

    let mut byte = [0u8; 1];
    loop {
        match unsafe { syscall(SYS_READ, fd, byte.as_mut_ptr() as u64, 1) } {
            Ok(0) => break,
            Ok(_) => write_bytes(&byte),
            Err(_) => break,
        }
    }
    write_bytes(b"\n");
    unsafe {
        let _ = syscall(SYS_CLOSE, fd, 0, 0);
    }
}

fn run_write(rest: &[u8]) {
    let (name, content) = split_first_word(trim(rest));
    let content = trim(content);
    if name.is_empty() {
        write_bytes(b"usage: write <name> <text>\n");
        return;
    }
    let fd = match unsafe { syscall(SYS_OPEN, name.as_ptr() as u64, name.len() as u64, O_CREAT) } {
        Ok(fd) => fd,
        Err(errno) => {
            write_bytes(b"write: open failed, errno ");
            write_decimal(errno);
            write_bytes(b"\n");
            return;
        }
    };
    unsafe {
        let _ = syscall(SYS_WRITE, fd, content.as_ptr() as u64, content.len() as u64);
        let _ = syscall(SYS_CLOSE, fd, 0, 0);
    }
    write_bytes(b"wrote ");
    write_decimal(content.len() as u64);
    write_bytes(b" bytes\n");
}

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    write_bytes(b"stsh (stupidshell)\nType `help` for a list of commands.\n");

    let mut line = [0u8; LINE_CAPACITY];
    loop {
        write_bytes(b"> ");
        let len = read_line(&mut line);
        run_command(&line[..len]);
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        spin_loop();
    }
}
