//! Process lifecycle: spawn/fork/execve/wait4/exit/reboot -- split out of the original process.rs.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::VirtAddr;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, Page, PageTableFlags, PhysFrame, Size4KiB,
};

use crate::memory::address_space::AddressSpace;
use crate::process::elf::{self, Elf};
use crate::memory::{self, with_frame_allocator};
use crate::process::scheduler;
use crate::syscall::{self, ECHILD, EINVAL, ELOOP, ENOEXEC, SyscallFrame};
use super::*;

// Real FreeBSD syscall numbers, duplicated here rather than imported — same "no shared crate
// across this internal ABI boundary" convention `modules/fat32`/`modules/native_abi` already use
// for their own copies of these same constants. `do_execve` uses these to drive its own internal
// open/read-loop/close against the exact same fd/fat32 machinery `stsh`'s `cat` already exercises
// via `syscall::dispatch` directly (`dispatch` is `pub(crate)`, callable from arbitrary kernel
// code, not just from the `SYSCALL` entry stub).
const SYS_OPEN: u64 = 5;
const SYS_READ: u64 = 3;
const SYS_CLOSE: u64 = 6;
/// Builds a brand-new process from `elf_bytes`: a fresh `AddressSpace` (`AddressSpace::new`, same
/// as the old one-shot demo path), the ELF loaded into it (`elf::load`), a mapped user stack, and
/// a fresh kernel stack seeded (`context_switch::seed_spawn_frame`) so its first-ever run lands in
/// `spawn_trampoline_asm` rather than resuming mid-syscall like every other switch does. Inserts
/// into the process table in `Ready` state and enqueues it — does not itself switch to it (the
/// caller decides when, via `scheduler::schedule`/`start`).
pub fn spawn(elf_bytes: &[u8], parent: Option<Pid>) -> Result<Pid, SpawnError> {
    let phys_offset = memory::phys_mem_offset();

    let address_space = with_frame_allocator(|fa| AddressSpace::new(phys_offset, fa));
    // SAFETY: phys_offset is the bootloader's phys-memory mapping; this is the only live view of
    // address_space's own (not-yet-active) level 4 table right now.
    let mut mapper = unsafe { address_space.mapper(phys_offset) };

    let elf = Elf::parse(elf_bytes).map_err(SpawnError::Elf)?;
    let entry = with_frame_allocator(|fa| elf::load(&elf, &mut mapper, fa, phys_offset, 0))
        .map_err(SpawnError::Elf)?;

    let stack_top = VirtAddr::new(USER_STACK_TOP);
    let mapped_pages = map_user_stack(&mut mapper, stack_top);
    crate::process::fault_trampoline::map(&mut mapper, phys_offset);
    // spawn() has no real invocation path to use as argv[0] (unlike do_execve, which knows exactly
    // what path it opened) -- this is only ever pid 1, built directly from an embedded ELF at
    // boot, so a fixed placeholder is all there is to give. pid 1 is a real musl-linked binary
    // (BusyBox's `hush`) now -- `envp` carries a single-entry `PATH=/bin`: musl's `__execvpe`
    // builds one candidate per colon-separated component as `<component>/<name>`, so this always
    // searches oxfs's `/bin` directory (where every applet is seeded, under its bare name --
    // `ls`, `cat`, ... -- not `.elf`-suffixed) as an *absolute* path, regardless of hush's current
    // cwd. (An earlier version relied on an empty `PATH=` component, which POSIX defines as
    // "search cwd" -- only worked by coincidence while both applets and hush's cwd sat at root;
    // see CLAUDE.md's BusyBox section.) `PATH=/bin` beats musl's own hardcoded
    // `/usr/local/bin:/bin:/usr/bin` fallback (used only when `$PATH` is unset entirely), since
    // none of *those* directories exist in oxfs.
    // `TERM=linux` matches this console's real nature (a VGA text-mode VT, see
    // `src/console/vga.rs`'s own SGR/CSI parser) -- most BusyBox tools treat unset `TERM` as
    // non-dumb already (`is_TERM_dumb()` only fires on an exact "dumb" match), but ncurses-shaped
    // tools (`vi`, `clear`, `reset`) key off a real value. `PS1` uses `hush`'s already-compiled
    // `CONFIG_FEATURE_EDITING_FANCY_PROMPT` escapes (`build.rs`'s HUSH-specific Kconfig flip) --
    // these are literal two-byte `\e`/`\[`/`\]`/`\u`/`\h`/`\w`/`\$` sequences that `lineedit.c`'s
    // own `parse_prompt` expands at print time (NOT a raw ESC byte here -- that's what `\e` itself
    // expands to). `\[`/`\]` mark non-printing spans so line-editing cursor math ignores the color
    // codes, `\u`/`\h` resolve via /etc/passwd + uname()'s nodename (both already real), `\w` is
    // cwd, `\$` is euid-sensitive ('#' for root).
    let initial_rsp = crate::process::user_stack::build(
        &elf,
        &[b"(init)"],
        &[
            b"PATH=/bin",
            b"TERM=linux",
            b"PS1=\\[\\e[1;32m\\]\\u@\\h\\[\\e[0m\\]:\\[\\e[1;34m\\]\\w\\[\\e[0m\\]\\$ ",
        ],
        stack_top,
        user_stack_bottom(stack_top),
        &mapped_pages,
        phys_offset,
        None,
    );

    let pid = alloc_pid();
    // No parent to inherit a process group from (spawn doesn't inherit anything else from parent
    // either -- cwd/fs_base/brk all start fresh too) -- becomes its own group leader, same
    // convention as a real init/session leader.
    let pgid = pid;
    // pid 1 is BusyBox's `hush` today (see the `argv[0]` placeholder note above) -- more accurate
    // for `/proc/1/stat`'s `(comm)` field than reusing that same "(init)" placeholder verbatim.
    let comm = b"hush".to_vec();
    let cmdline = build_cmdline(&[b"(init)"]);
    let kernel_stack = KernelStack::new();
    let kernel_stack_top = kernel_stack.top();
    let rsp = crate::process::context_switch::seed_spawn_frame(kernel_stack_top);

    let process = Process {
        pid,
        // pid 1's own thread group of one -- see Process::tgid's own doc comment.
        tgid: pid,
        parent,
        children: Vec::new(),
        state: ProcState::Ready,
        address_space,
        kernel_stack,
        kernel_stack_top,
        rsp,
        entry_point: entry,
        user_stack_top: initial_rsp,
        brk: VirtAddr::new(elf.highest_loaded_address()),
        fs_base: 0,
        cwd: 0,
        root_inode: 0,
        pending_signals: 0,
        blocked_signals: 0,
        sigactions: [SigAction::DEFAULT; (SIGRTMAX + 1) as usize],
        signal_stack: Vec::new(),
        altstack: AltStack::default(),
        priority: 0,
        pgid,
        sid: pid,
        comm,
        cmdline,
        real_timer_deadline: None,
        real_timer_interval_ticks: 0,
        posix_timers: [None; MAX_POSIX_TIMERS],
        // No login mechanism exists -- pid 1 (and every process descending from it) starts as
        // root, same reasoning as Process::uid's own doc comment.
        uid: 0,
        gid: 0,
        rlimits: [(u64::MAX, u64::MAX); 16],
        nice: 0,
        sched_policy: SCHED_RR_DEFAULT,
        sched_priority: 0,
        umask: 0o022,
        stop_notify_pending: false,
        cont_notify_pending: false,
        sigsuspend_restore_mask: None,
        sysv_sem_undo: Vec::new(),
        sysv_shm_attach: Vec::new(),
        mmap_file_regions: Vec::new(),
        pending_siginfo: [QueuedSigInfo::default(); 32],
        rt_queue: core::array::from_fn(|_| Vec::new()),
        fpu_state: crate::cpu::fpu::clean_state(),
        cpu_ticks: 0,
    };

    {
        let mut table = PROCESS_TABLE.lock();
        if let Some(parent_pid) = parent
            && let Some(p) = table.get_mut(&parent_pid)
        {
            p.children.push(pid);
        }
        table.insert(pid, Box::new(process));
    }
    // spawn() is only ever called once, for pid 1, with stdin/stdout/stderr already wired
    // directly to the real console (never through a real `open()` syscall -- see the envp comment
    // above). On a real kernel, a session leader's first real `open()` of a tty auto-associates it
    // as that session's controlling terminal; since pid 1 never takes that path here, nothing ever
    // would otherwise. Granting it directly mirrors that real behavior and is what lets `hush`'s
    // own already-compiled job-control startup (`tcgetpgrp`/`bb_setpgrp`/`tcsetpgrp`, gated on
    // `isatty()` succeeding via a real controlling session -- see `src/console/stdin.rs`'s
    // `TIOCGPGRP`/`TIOCSPGRP` handling) actually activate instead of sitting permanently dormant --
    // this is what makes Ctrl+C interrupt a running foreground job for real.
    crate::console::stdin::set_controlling_session(pid);
    // Bootstraps this process's own stdin/stdout/stderr from crate::fs::fd::init's own pseudo-pid
    // registration -- the same fork_inherit path a real fork() uses, see that function's own doc
    // comment.
    crate::fs::fd::fork_inherit(0, pid);
    scheduler::enqueue_ready(pid);
    Ok(pid)
}

/// Maps a fresh user stack ending at `stack_top`, returning the `(Page, PhysFrame)` map of every
/// page it just mapped — `user_stack::build` needs this to write the argv/envp/auxv image into the
/// right physical frames afterward, the same way `elf::load` already tracks its own mapped pages
/// for BSS zeroing.
fn map_user_stack(
    mapper: &mut impl Mapper<Size4KiB>,
    stack_top: VirtAddr,
) -> BTreeMap<Page<Size4KiB>, PhysFrame<Size4KiB>> {
    let stack_bottom_page = Page::containing_address(stack_top - user_stack_pages() * 4096);
    let stack_top_page = Page::containing_address(stack_top - 1u64);
    let mut mapped_pages = BTreeMap::new();
    with_frame_allocator(|fa| {
        for page in Page::range_inclusive(stack_bottom_page, stack_top_page) {
            let frame = fa
                .allocate_frame()
                .expect("out of memory mapping a user stack");
            // SAFETY: frame was just allocated (unused, per BootInfoFrameAllocator's contract),
            // and page falls in this address space's own, not-yet-active range.
            unsafe {
                mapper
                    .map_to(
                        page,
                        frame,
                        PageTableFlags::PRESENT
                            | PageTableFlags::WRITABLE
                            | PageTableFlags::USER_ACCESSIBLE,
                        fa,
                    )
                    .expect("failed to map a user stack page")
                    .flush();
            }
            mapped_pages.insert(page, frame);
        }
    });
    mapped_pages
}

/// `stack_top` minus enough room for `user_stack::build`'s image to always fit, regardless of
/// argv/envp length -- `map_user_stack` itself already computed this same bound (`user_stack_pages()`
/// below `stack_top`); re-derived here rather than threading an extra return value through, since
/// both callers already have `stack_top` in scope.
fn user_stack_bottom(stack_top: VirtAddr) -> VirtAddr {
    stack_top - user_stack_pages() * 4096
}

/// `sys_fork`'s real logic: deep-copies the calling process's address space
/// (`AddressSpace::fork`), builds a fresh kernel stack seeded so the child's first switch-in
/// resumes it as if returning from this very same syscall with `0`
/// (`context_switch::seed_fork_frame`), and enqueues it `Ready`. The parent's own return value
/// (the child's pid) flows back through the completely ordinary `Ok(child_pid)` -> `frame.rax`
/// path — no special-casing needed on the parent side.
pub fn do_fork_from_current() -> Result<u64, u64> {
    let caller_pid = scheduler::current_pid();
    let parent_frame = syscall::current_frame() as *const SyscallFrame;
    let phys_offset = memory::phys_mem_offset();

    let child_pid = alloc_pid();
    let (
        child_address_space,
        parent_brk,
        parent_fs_base,
        parent_cwd,
        parent_root_inode,
        parent_pgid,
        parent_sid,
        parent_blocked_signals,
        parent_sigactions,
        parent_signal_stack,
        parent_altstack,
        parent_comm,
        parent_cmdline,
        parent_uid,
        parent_gid,
        parent_rlimits,
        parent_nice,
        parent_sched_policy,
        parent_sched_priority,
        parent_umask,
        parent_fpu_state,
    ) = {
        let mut table = PROCESS_TABLE.lock();
        let parent = table
            .get_mut(&caller_pid)
            .expect("fork: current process missing from table");
        // SAFETY: AddressSpace::fork requires self to be the currently active address space --
        // true here, since sys_fork runs synchronously on the calling process's own kernel stack
        // with its own CR3 still live.
        let child_address_space =
            with_frame_allocator(|fa| parent.address_space.fork(phys_offset, fa));
        (
            child_address_space,
            parent.brk,
            parent.fs_base,
            parent.cwd,
            parent.root_inode,
            parent.pgid,
            parent.sid,
            // Real fork() semantics: signal disposition and the blocked-signal mask are
            // inherited; pending signals are not (the child starts with an empty pending set —
            // see the child's own construction below). signal_stack is inherited too, in case the
            // parent forked from inside one or more handlers — the child gets its own independent
            // copy of that same in-progress-handler bookkeeping (each entry is plain data, no
            // pointers into the parent's own address space beyond what `saved` already carries).
            parent.blocked_signals,
            parent.sigactions,
            parent.signal_stack.clone(),
            // Real fork() semantics: the alt stack's own address stays valid in the child (the
            // whole address space is duplicated) -- see AltStack's own doc comment.
            parent.altstack,
            // Real fork() semantics: the child keeps the parent's comm/cmdline until it execs its
            // own -- same reasoning as brk/fs_base/cwd above.
            parent.comm.clone(),
            parent.cmdline.clone(),
            // Real fork() semantics: uid/gid are copied, same as cwd/pgid/brk -- see
            // Process::uid's own doc comment.
            parent.uid,
            parent.gid,
            // Real POSIX rlimit/nice/scheduling-attribute/fork semantics: all four are copied,
            // same as uid/gid/pgid -- see Process::rlimits's own doc comment.
            parent.rlimits,
            parent.nice,
            parent.sched_policy,
            parent.sched_priority,
            parent.umask,
            // Real fork() semantics: the child's own FPU/SSE register state starts as an exact
            // copy of the parent's live state at the moment of the fork() call (the parent's own
            // in-flight computation, if any, is a real thing the child should see too) -- same
            // "copied, not reset" treatment as uid/gid/rlimits above.
            parent.fpu_state,
        )
    };

    let kernel_stack = KernelStack::new();
    let kernel_stack_top = kernel_stack.top();
    // SAFETY: parent_frame is the caller's own live SyscallFrame, valid for the duration of this
    // call (we're still inside sys_fork's own handling of it).
    let rsp = unsafe { crate::process::context_switch::seed_fork_frame(kernel_stack_top, parent_frame) };

    let child = Process {
        pid: child_pid,
        // A forked child is a real POSIX process, not a thread -- its own tgid, never the
        // parent's. See Process::tgid's own doc comment.
        tgid: child_pid,
        parent: Some(caller_pid),
        children: Vec::new(),
        state: ProcState::Ready,
        address_space: child_address_space,
        kernel_stack,
        kernel_stack_top,
        rsp,
        entry_point: VirtAddr::zero(),
        user_stack_top: VirtAddr::zero(),
        brk: parent_brk,
        fs_base: parent_fs_base,
        cwd: parent_cwd,
        root_inode: parent_root_inode,
        pending_signals: 0,
        blocked_signals: parent_blocked_signals,
        sigactions: parent_sigactions,
        signal_stack: parent_signal_stack,
        altstack: parent_altstack,
        priority: 0,
        pgid: parent_pgid,
        sid: parent_sid,
        comm: parent_comm,
        cmdline: parent_cmdline,
        // Not inherited -- see `real_timer_deadline`'s own doc comment (real POSIX itimer/fork
        // semantics: a forked child starts with its own disarmed timer).
        real_timer_deadline: None,
        real_timer_interval_ticks: 0,
        // Not inherited either -- see `Process::posix_timers`'s own doc comment (real
        // `timer_create(2)` semantics: not inherited across `fork`).
        posix_timers: [None; MAX_POSIX_TIMERS],
        uid: parent_uid,
        gid: parent_gid,
        rlimits: parent_rlimits,
        nice: parent_nice,
        sched_policy: parent_sched_policy,
        sched_priority: parent_sched_priority,
        umask: parent_umask,
        // A forked child is never born stopped, and hasn't just resumed from anything -- real
        // POSIX fork() semantics, same "starts fresh" story as real_timer_deadline above.
        stop_notify_pending: false,
        cont_notify_pending: false,
        // Never straddles a fork boundary -- see this field's own doc comment on Process.
        sigsuspend_restore_mask: None,
        // Not inherited -- see this field's own doc comment (real SysV semadj/fork semantics).
        sysv_sem_undo: Vec::new(),
        // Not inherited either -- see this field's own doc comment (tied to this kernel's own
        // "no copy-on-write fork" limitation, not an independent design choice).
        sysv_shm_attach: Vec::new(),
        // Not inherited -- same reasoning, see this field's own doc comment on Process.
        mmap_file_regions: Vec::new(),
        // Not inherited -- see this field's own doc comment (pending_signals itself starts at 0
        // for a forked child too, so there's nothing meaningful to carry over).
        pending_siginfo: [QueuedSigInfo::default(); 32],
        // Not inherited -- same reasoning as pending_siginfo above.
        rt_queue: core::array::from_fn(|_| Vec::new()),
        fpu_state: parent_fpu_state,
        // Not inherited -- real POSIX: a forked child's own CPU time starts at 0, it hasn't run
        // yet (see this field's own doc comment on Process).
        cpu_ticks: 0,
    };

    {
        let mut table = PROCESS_TABLE.lock();
        table.get_mut(&caller_pid).unwrap().children.push(child_pid);
        table.insert(child_pid, Box::new(child));
    }
    // Real fork() semantics: the child gets its own independently-closable copy of every fd the
    // parent has open, not a shared view of the parent's own table entries -- see
    // crate::fs::fd::fork_inherit's own doc comment for why this specifically matters for pipes.
    crate::fs::fd::fork_inherit(caller_pid, child_pid);
    scheduler::enqueue_ready(child_pid);
    Ok(child_pid)
}
/// `sys_execve`'s real logic. Reuses `syscall::dispatch` directly to drive an internal
/// open/read-loop/close against whatever `path_ptr`/`path_len` (the caller's own user-space
/// pointer, still valid since the caller's address space is what's currently active) names —
/// exactly the same fd/fat32 machinery `stsh`'s `cat` already exercises through the public
/// syscall path. Every fallible step (open, each read, close, `Elf::parse`, the new
/// `AddressSpace`, `elf::load`, mapping the user stack) completes *before* any mutation of the
/// live syscall frame, `CR3`, or the process's stored `AddressSpace` — real `execve(2)` semantics:
/// a failure at any point must leave the calling program completely untouched.
/// Wire format for `SYS_EXECVE`'s optional third and fourth arguments (`argv_ptr`/`envp_ptr`) --
/// OxideBSD's own invention, not modeled on real `execve`'s NUL-terminated `char **argv`/`char
/// **envp` (this ABI's syscalls are length-prefixed throughout instead -- see CLAUDE.md's syscall
/// ABI section). A sequence of these structs, terminated by a `ptr == 0` entry, describes either
/// argv[] or envp[] (same shape, read the same way -- see `read_ptr_len_array` below).
/// `argv_ptr == 0` (or a non-null `argv_ptr` whose very first entry is already the `{0, 0}`
/// terminator) falls back to a synthesized one-element `argv = [path_bytes]`, matching real
/// `execve`'s convention that `argv[0]` always exists even when the caller passes an empty/absent
/// array -- every caller before `stsh`'s own execve wrapper grew argument support relied on
/// exactly this fallback, and keeps doing so unaffected. **A non-empty `argv_ptr` supplies the
/// *complete* array, including `argv[0]` -- it is no longer implicitly `path_bytes` glued onto
/// `argv_ptr`'s own contents.** This matches real `execve(2)` semantics (the caller chooses
/// `argv[0]`, which need not equal the path used to find the file at all -- e.g. a login shell's
/// `argv[0]` of `-bash`) and is what makes real multi-call-binary dispatch (a `busybox`-style
/// binary picking an applet by `argv[0]`/basename) possible at all; it used to be silently
/// unreachable, since `do_execve` always overwrote `argv[0]` with the exec path itself regardless
/// of what a real caller supplied. `envp_ptr` is `R10`, the ABI's 4th argument -- see
/// `src/syscall.rs`'s module doc comment for why that register only became a real, read argument
/// once this needed it.
#[repr(C)]
struct RawArgvEntry {
    ptr: u64,
    len: u64,
}

/// Bounded as a sanity cap against a runaway/garbage `argv_ptr`/`envp_ptr`, not a deliberate
/// argument/environment-count limit -- `stsh`'s own 128-byte line buffer can't produce anywhere
/// near this many words anyway, and no `envp` this codebase builds today comes close either.
const MAX_PTR_LEN_ENTRIES: usize = 32;

/// Reads the `RawArgvEntry` array `ptr` describes, if any -- shared by `argv_ptr` (argv[1..]) and
/// `envp_ptr` (envp[]), which use the exact same wire format (see `RawArgvEntry`'s own doc
/// comment).
fn read_ptr_len_array(ptr: u64) -> Vec<Vec<u8>> {
    let mut entries_out = Vec::new();
    if ptr == 0 {
        return entries_out;
    }
    for i in 0..MAX_PTR_LEN_ENTRIES {
        // SAFETY: same known pointer-validation gap every other user-memory read in this file
        // already has -- ptr isn't checked against the caller's actual mappings before use.
        let entry = unsafe { &*(ptr as *const RawArgvEntry).add(i) };
        if entry.ptr == 0 {
            break;
        }
        let bytes =
            unsafe { core::slice::from_raw_parts(entry.ptr as *const u8, entry.len as usize) };
        entries_out.push(bytes.to_vec());
    }
    entries_out
}

/// Tail of `path` after its last `/` (the whole slice if there's none) -- used to derive
/// `Process::comm` from the path `execve`/`spawn` actually loaded, matching real Linux's own
/// "comm comes from the executable's filename, not argv[0]" rule.
fn basename(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

/// Real `/proc/[pid]/cmdline` wire format: each `argv` entry followed by one NUL byte, no
/// trailing separator beyond that.
fn build_cmdline(argv: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for arg in argv {
        out.extend_from_slice(arg);
        out.push(0);
    }
    out
}

/// Reads a whole file's contents via the real `SYS_OPEN`/`SYS_READ`/`SYS_CLOSE` syscall path,
/// given a raw `(ptr, len)` pointing at a NUL-free path string. `ptr` may point into the
/// *caller's* own user address space (a top-level `execve` target) or into this kernel's own heap
/// (a `#!`-line interpreter path parsed out of a script's own content, see `do_execve`'s
/// shebang-following loop below) -- both are valid dereference targets regardless of which
/// process's `CR3` happens to be active: kernel-heap virtual addresses are mapped identically in
/// every address space's page table (see CLAUDE.md's own address-space section on why
/// `AddressSpace::fork`/`new_excluding_user` still shallow-copy the kernel's own high entries).
fn read_file_via_syscall(path_ptr: u64, path_len: u64) -> Result<Vec<u8>, u64> {
    let fd = syscall::dispatch(SYS_OPEN, path_ptr, path_len, 0, 0)?;
    let mut bytes: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        match syscall::dispatch(
            SYS_READ,
            fd,
            chunk.as_mut_ptr() as u64,
            chunk.len() as u64,
            0,
        ) {
            Ok(0) => break,
            Ok(n) => bytes.extend_from_slice(&chunk[..n as usize]),
            Err(errno) => {
                let _ = syscall::dispatch(SYS_CLOSE, fd, 0, 0, 0);
                return Err(errno);
            }
        }
    }
    let _ = syscall::dispatch(SYS_CLOSE, fd, 0, 0, 0);
    Ok(bytes)
}

/// Leading/trailing ASCII-whitespace trim -- `core::slice` has no built-in for `&[u8]` the way
/// `str::trim` does.
fn trim_ascii_whitespace(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |i| i + 1);
    &bytes[start..end]
}

/// Parses a `#!interpreter [optional-single-arg]` first line out of `bytes` (already confirmed to
/// start with `#!`) -- real Linux `binfmt_script` semantics: the interpreter path runs up to the
/// first whitespace, then *at most one* further whitespace-trimmed argument runs to the end of
/// the line (never further word-split, unlike a normal shell command line). An empty interpreter
/// (a bare `#!` with nothing else on the line) is reported as `None` -- `do_execve`'s own caller
/// turns that into `ENOEXEC`, matching real Linux's own refusal to exec a scriptless shebang.
fn parse_shebang_line(bytes: &[u8]) -> Option<(Vec<u8>, Option<Vec<u8>>)> {
    let line_end = bytes
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(bytes.len());
    let line = trim_ascii_whitespace(&bytes[2..line_end]);
    let interpreter_end = line
        .iter()
        .position(|b| b.is_ascii_whitespace())
        .unwrap_or(line.len());
    let interpreter = &line[..interpreter_end];
    if interpreter.is_empty() {
        return None;
    }
    let rest = trim_ascii_whitespace(&line[interpreter_end..]);
    let optional_arg = if rest.is_empty() {
        None
    } else {
        Some(rest.to_vec())
    };
    Some((interpreter.to_vec(), optional_arg))
}

/// Real Linux caps `#!`-chain recursion (a script whose own interpreter is itself another `#!`
/// script) rather than following it forever -- this kernel has no equivalent existing constant to
/// reuse (`modules/oxfs`'s own `MAX_SYMLINK_DEPTH` lives in a separate, unlinkable module), so a
/// small, independently-chosen bound of the same shape.
const MAX_SHEBANG_DEPTH: u32 = 4;

/// Real, kernel-chosen runtime base for a `PT_INTERP` interpreter, added as `elf::load`'s `bias`
/// parameter (see that function's own doc comment for why this must be a real additive bias, not
/// a link-time-fixed address the interpreter file itself assumes). Clear of every existing fixed
/// userland load base (`0x4000000`-`0xbf00000`, see every `userland/*/linker.ld`) and of
/// `module::MODULE_VA_BASE`/`BRK_REGION_CEILING` (`0x10000000`) -- picked once, reused for every
/// `PT_INTERP` load rather than per-binary, since nothing here needs more than one interpreter
/// image loaded at a time.
const INTERP_LOAD_BASE: u64 = 0xc000000;

pub fn do_execve(
    caller_pid: Pid,
    path_ptr: u64,
    path_len: u64,
    argv_ptr: u64,
    envp_ptr: u64,
) -> Result<u64, u64> {
    // Copied out now, while the caller's own address space (where path_ptr/argv_ptr/envp_ptr are
    // valid) is still active -- used for the new program's initial stack, built further down, by
    // which point a fresh (as-yet-unactivated) address space is what's live instead. Same known
    // pointer-validation gap sys_write/sys_read already have for user pointers.
    let path_bytes: Vec<u8> =
        unsafe { core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize) }.to_vec();
    let raw_argv = read_ptr_len_array(argv_ptr);
    let envp = read_ptr_len_array(envp_ptr);

    // Real Unix `execve()` on a `#!`-prefixed file re-execs the named interpreter instead of
    // trying to load the script itself as an ELF (this kernel's own `elf::load` has no shebang
    // awareness at all -- a script handed to it directly fails `Elf::parse` with `ENOEXEC`, "Exec
    // format error", the same error a real kernel gives for any genuinely non-ELF file). Found
    // live: hush has no userspace `ENOEXEC`-fallback-to-script-interpretation logic of its own
    // (real Linux shells generally don't need one, since the kernel already handles `#!`) -- a
    // real script's own `+x` bit alone was never going to be enough to run it directly without
    // this. `effective_path` starts as the caller's own target and is replaced by each
    // interpreter in turn; `shebang_argv_prefix`, once any `#!` is followed, becomes the real
    // `[interpreter, optional-arg?, script-path]` prefix `argv` is built from below instead of the
    // caller's original `argv[0]`.
    let mut effective_path: Vec<u8> = path_bytes.clone();
    let mut shebang_argv_prefix: Option<Vec<Vec<u8>>> = None;
    let mut shebang_depth: u32 = 0;
    let elf_bytes: Vec<u8> = loop {
        let bytes =
            read_file_via_syscall(effective_path.as_ptr() as u64, effective_path.len() as u64)?;
        if bytes.len() < 2 || &bytes[0..2] != b"#!" {
            break bytes;
        }
        shebang_depth += 1;
        if shebang_depth > MAX_SHEBANG_DEPTH {
            return Err(ELOOP);
        }
        let (interpreter, optional_arg) = parse_shebang_line(&bytes).ok_or(ENOEXEC)?;
        let mut prefix = alloc::vec![interpreter.clone()];
        if let Some(arg) = optional_arg {
            prefix.push(arg);
        }
        prefix.push(effective_path);
        shebang_argv_prefix = Some(prefix);
        effective_path = interpreter;
    };

    let elf = Elf::parse(&elf_bytes).map_err(|_| ENOEXEC)?;

    let phys_offset = memory::phys_mem_offset();
    // new_excluding_user, not AddressSpace::new: the currently active address space here is the
    // calling process's own, already-populated one (execve runs mid-syscall, on the caller's own
    // kernel stack, with its own CR3 still live) -- AddressSpace::new would shallow-copy that
    // process's *user* mappings too, aliasing them into what's supposed to be a fresh image.
    let new_address_space =
        with_frame_allocator(|fa| AddressSpace::new_excluding_user(phys_offset, fa));
    // SAFETY: phys_offset is the bootloader's phys-memory mapping; this is the only live view of
    // new_address_space's own (not-yet-active) level 4 table right now.
    let mut mapper = unsafe { new_address_space.mapper(phys_offset) };
    let entry = with_frame_allocator(|fa| elf::load(&elf, &mut mapper, fa, phys_offset, 0))
        .map_err(|_| ENOEXEC)?;

    // A real PT_INTERP dynamic linker, if the binary carries one, gets loaded into the same
    // not-yet-active address space right here, immediately after the main binary's own segments --
    // `SYSRETQ` needs to land in the interpreter's own entry, not the main binary's (the
    // interpreter relocates/resolves itself, then jumps to the main binary's real entry on its
    // own, via AT_ENTRY -- see `user_stack.rs`). `jump_entry`/`interp_base` stay `entry`/`None`
    // (today's only case, unchanged) when there's no interpreter. `INTERP_LOAD_BASE` is a real,
    // kernel-chosen runtime bias applied by `elf::load` itself, not a link-time address the
    // interpreter file assumes -- see that function's own doc comment for the real bug (a wild
    // pointer inside the interpreter's own relocation self-processing) a fixed-link-time-base
    // interpreter caused, and why this bias has to be applied here instead.
    let mut jump_entry = entry;
    let mut interp_base: Option<u64> = None;
    if let Some(interp_path) = elf.interpreter().map_err(|_| ENOEXEC)? {
        let interp_bytes =
            read_file_via_syscall(interp_path.as_ptr() as u64, interp_path.len() as u64)?;
        let interp_elf = Elf::parse(&interp_bytes).map_err(|_| ENOEXEC)?;
        let interp_entry = with_frame_allocator(|fa| {
            elf::load(&interp_elf, &mut mapper, fa, phys_offset, INTERP_LOAD_BASE)
        })
        .map_err(|_| ENOEXEC)?;
        jump_entry = interp_entry;
        interp_base = Some(INTERP_LOAD_BASE);
    }

    let stack_top = VirtAddr::new(USER_STACK_TOP);
    let mapped_pages = map_user_stack(&mut mapper, stack_top);
    crate::process::fault_trampoline::map(&mut mapper, phys_offset);
    // raw_argv (read above, while the caller's own address space was still active) is the caller's
    // complete, real argv[] -- including a real, caller-chosen argv[0], which need not equal
    // path_bytes (see RawArgvEntry's own doc comment). An empty raw_argv (argv_ptr == 0, or a
    // present-but-immediately-terminated array) falls back to a synthesized single-element
    // argv = [path_bytes], the same fallback every pre-existing caller already relies on. envp is
    // real too -- whatever the caller's own envp_ptr described, or empty if it passed 0.
    //
    // When a `#!` shebang was followed above (`shebang_argv_prefix` is `Some`), real
    // `binfmt_script` semantics replace the whole thing instead: `[interpreter, optional-arg?,
    // script-path]` followed by the caller's own *original* `argv[1..]` -- `argv[0]` is always
    // discarded, matching real Linux (the script's own path, not the caller's `argv[0]`, is what
    // ends up in the new argv, since they need not be equal -- see `RawArgvEntry`'s own doc
    // comment referenced above).
    let argv_owned: Vec<Vec<u8>> = if let Some(mut prefix) = shebang_argv_prefix {
        if !raw_argv.is_empty() {
            prefix.extend(raw_argv[1..].iter().cloned());
        }
        prefix
    } else if raw_argv.is_empty() {
        alloc::vec![path_bytes.clone()]
    } else {
        raw_argv.clone()
    };
    let argv: Vec<&[u8]> = argv_owned.iter().map(Vec::as_slice).collect();
    let envp_refs: Vec<&[u8]> = envp.iter().map(Vec::as_slice).collect();
    let initial_rsp = crate::process::user_stack::build(
        &elf,
        &argv,
        &envp_refs,
        stack_top,
        user_stack_bottom(stack_top),
        &mapped_pages,
        phys_offset,
        interp_base,
    );

    // ---- commit point: nothing above may fail past here ----
    // SAFETY: new_address_space carries the kernel's own mappings (shallow-copied at construction,
    // same as every address space) plus the just-loaded ELF and user stack -- the same guarantee
    // AddressSpace::activate's own contract requires. Activating it mid-syscall, still running on
    // the caller's own kernel stack, is safe: the kernel half is identical no matter which address
    // space is live.
    unsafe { new_address_space.activate() };
    let old_address_space = {
        let mut table = PROCESS_TABLE.lock();
        let me = table
            .get_mut(&caller_pid)
            .expect("execve: current process missing from table");
        // Old AddressSpace captured here, torn down for real below (once this lock is dropped) --
        // see AddressSpace::teardown's own doc comment for why this exact call site is safe:
        // new_address_space.activate() above already switched CR3 away from it.
        let old_address_space = core::mem::replace(&mut me.address_space, new_address_space);
        me.user_stack_top = initial_rsp;
        // The real jump target (interpreter's own entry when PT_INTERP loaded one, else the main
        // binary's) -- not read again for this exact pid (only a never-run process's first switch,
        // via spawn_trampoline_inner, ever consults entry_point; an execve'd process is already
        // running and jumps immediately via redirect_frame below), kept in sync anyway since it's
        // the honest answer to "where does this process's AddressSpace currently expect to run".
        me.entry_point = jump_entry;
        me.brk = VirtAddr::new(elf.highest_loaded_address());
        // effective_path is whichever path actually got loaded as the real ELF above -- the
        // caller's own original path_bytes when there was no `#!` to follow, or the final
        // interpreter in a shebang chain otherwise. Matches real Linux: `/proc/[pid]/comm` names
        // the actual binary execve() loaded (the interpreter), not the script argument handed to
        // it, since only the interpreter is ever really "exec'd" at the kernel level.
        me.comm = basename(&effective_path).to_vec();
        me.cmdline = build_cmdline(&argv);
        // The old program's TLS base doesn't mean anything to the new one -- reset the stored value
        // (restored on every future context switch, see Process::fs_base's own doc comment) *and*
        // the live MSR right now, since execve keeps running as this exact process/kernel stack with
        // no context switch in between; leaving the stale value live until the new program's own
        // crt1 gets around to calling SYS_SET_FS_BASE would be a real (if narrow) window for it to
        // read garbage through %fs before then.
        me.fs_base = 0;
        x86_64::registers::model_specific::FsBase::write(VirtAddr::new(0));
        // Real execve() semantics: a caught signal's handler address means nothing in the new
        // program image, so it resets to SIG_DFL; SIG_IGN and already-SIG_DFL entries are left
        // alone (both are position-independent of any particular program's own code), and so is
        // pending_signals/blocked_signals -- both persist across execve on a real system too.
        for action in me.sigactions.iter_mut() {
            if action.handler > 1 {
                *action = SigAction::DEFAULT;
            }
        }
        // Real execve() semantics: an established alt stack's address belongs to the old program
        // image and means nothing in the new one -- reset to disabled, same reasoning fs_base
        // above already uses.
        me.altstack = AltStack::default();
        // Real `timer_create(2)` semantics: POSIX per-process timers are disarmed and deleted
        // across execve -- see `Process::posix_timers`'s own doc comment.
        me.posix_timers = [None; MAX_POSIX_TIMERS];
        old_address_space
    };
    // Real reclaim: the old address space is truly unreachable now (CR3 already moved, and
    // `me.address_space` already holds the new one) -- see AddressSpace::teardown's own doc
    // comment for why this exact call site is safe. Real fd-backed mmap/SysV shm content the old
    // image had live is protected from this by SHARED_LEAF (see that constant's own doc comment)
    // regardless of whether the cleanup calls below have run yet. `phys_offset` here is the same
    // one this function established above, still valid (it never changes at runtime).
    with_frame_allocator(|fa| unsafe { old_address_space.teardown(phys_offset, fa) });
    // Real SysV shm semantics: the old address space (just torn down above) is what every prior
    // shmat's own mapping actually lived in -- the new image can't see any of it, so this is a
    // real implicit detach of everything, exactly like a real process exit's own
    // detach_all_for_exit call (see that function's own doc comment) except the process survives.
    // Must run after the PROCESS_TABLE-locked block above ends, not inside it -- this function
    // takes that same lock itself, briefly, to drain the attachment list.
    crate::fs::sysv_shm::detach_all_for_exit(caller_pid);
    // Same story for any real fd-backed mmap the old image had live -- see `mm::
    // cleanup_mmap_file_regions_for_exit`'s own doc comment.
    mm::cleanup_mmap_file_regions_for_exit(caller_pid);
    // Real close-on-exec: any fd this process marked FD_CLOEXEC (fcntl(F_SETFD)/open(O_CLOEXEC),
    // see `crate::fs::fd::CLOEXEC`'s own doc comment) doesn't survive into the new program image.
    crate::fs::fd::close_cloexec(caller_pid);

    let frame = syscall::current_frame();
    // SAFETY: frame is this exact syscall's own live frame -- do_execve is only ever reached via
    // dispatch(SYS_EXECVE, ...), called from within syscall_dispatch, which set CURRENT_FRAME to
    // it just before.
    unsafe { syscall::redirect_frame(frame, jump_entry, initial_rsp) };
    Ok(0)
}
/// Real `wait4(2)` `options` bits (`third_party/musl/include/sys/wait.h`) -- musl's own wrapper
/// passes these straight through as the real 3rd syscall argument, no call-site patch needed.
pub const WNOHANG: u64 = 1;
pub const WUNTRACED: u64 = 2;
pub const WCONTINUED: u64 = 8;

/// What `do_wait4` found to report back, before it's translated into the real wire-format
/// `status` value below -- kept distinct from that encoding so the three real, mutually exclusive
/// shapes (exited/stopped/continued) can't be confused with each other while being decided.
enum Reported {
    Exited(Pid, i32),
    Stopped(Pid, u64),
    Continued(Pid),
}

/// `sys_wait4`'s real logic. If the caller already has a `Zombie` child matching `target_pid`
/// (`-1` = any), reaps it immediately (removes it from the table, writes its exit code through the
/// optional `status_ptr`, returns its pid). Real `WUNTRACED`/`WCONTINUED` support: if `options`
/// requests them and a matching child has `stop_notify_pending`/`cont_notify_pending` set (see
/// `ProcState::Stopped`'s own doc comment for why a plain state flip is enough to make this safe),
/// reports that instead -- without reaping, since the child is still alive. If the caller has no
/// child matching `target_pid` at all, `ECHILD`. If nothing reportable exists yet and `options`
/// includes real `WNOHANG`, returns `0` immediately rather than blocking -- load-bearing, not just
/// a nice-to-have: `hush`'s own `checkjobs()` (background job-status polling, run throughout its
/// interactive main loop) always passes `WUNTRACED | WNOHANG`, and this kernel never delivers a
/// real `SIGCHLD` to a parent at all (nothing `do_kill`'s it anywhere), so `hush`'s own
/// `CONFIG_HUSH_FAST` short-circuit (skip the real syscall unless a `SIGCHLD` counter changed) is
/// silently permanently active for that call today -- without a real `WNOHANG`, any call that ever
/// did reach the real syscall with a still-running child would block the entire interactive shell.
/// Otherwise blocks (`ProcState::Blocked`) and calls `scheduler::schedule()`, which only returns
/// once something (`do_exit`/the real stop/continue transitions in `process::signals`/
/// `do_stop_self`) wakes the parent — at which point the loop re-checks from the top, since a
/// wakeup never hands the child's info across directly (and, per `ProcState::Stopped`'s own doc
/// comment, a wakeup that turns out not to match what this exact call actually asked for is safe
/// to just re-block on).
pub fn do_wait4(
    caller_pid: Pid,
    target_pid: i64,
    options: u64,
    status_ptr: u64,
    rusage_ptr: u64,
) -> Result<u64, u64> {
    let matches = |pid: Pid| target_pid == -1 || target_pid as u64 == pid;

    loop {
        // Set only by the `Reported::Exited` arm below, to the just-reaped child's own address
        // space -- real reclaim happens after this loop iteration's own `table` lock is dropped
        // (see the real teardown call further below, and AddressSpace::teardown's own doc comment
        // for why this exact call site -- a reaped process, necessarily not the active `CR3` since
        // it already stopped running back when it originally called `do_exit` -- is safe).
        let mut reaped_address_space: Option<AddressSpace> = None;
        let reported = {
            let mut table = PROCESS_TABLE.lock();

            let children: Vec<Pid> = table
                .get(&caller_pid)
                .expect("wait4: current process missing from table")
                .children
                .iter()
                .copied()
                .filter(|&c| matches(c))
                .collect();
            if children.is_empty() {
                return Err(ECHILD);
            }

            let zombie = children
                .iter()
                .copied()
                .find_map(|c| match table.get(&c).map(|p| p.state) {
                    Some(ProcState::Zombie(code)) => Some((c, code)),
                    _ => None,
                });

            if let Some((child_pid, code)) = zombie {
                let removed = table
                    .remove(&child_pid)
                    .expect("wait4: zombie child vanished under the same lock that found it");
                reaped_address_space = Some(removed.address_space);
                table
                    .get_mut(&caller_pid)
                    .unwrap()
                    .children
                    .retain(|&c| c != child_pid);
                Some(Reported::Exited(child_pid, code))
            } else if options & WUNTRACED != 0
                && let Some(&child_pid) = children
                    .iter()
                    .find(|&&c| table.get(&c).is_some_and(|p| p.stop_notify_pending))
            {
                let ProcState::Stopped(stopsig) = table.get(&child_pid).unwrap().state else {
                    unreachable!(
                        "stop_notify_pending is always cleared in the same lock as the state \
                         transition back out of Stopped -- see Process::stop_notify_pending's own \
                         doc comment"
                    )
                };
                table.get_mut(&child_pid).unwrap().stop_notify_pending = false;
                Some(Reported::Stopped(child_pid, stopsig))
            } else if options & WCONTINUED != 0
                && let Some(&child_pid) = children
                    .iter()
                    .find(|&&c| table.get(&c).is_some_and(|p| p.cont_notify_pending))
            {
                table.get_mut(&child_pid).unwrap().cont_notify_pending = false;
                Some(Reported::Continued(child_pid))
            } else if options & WNOHANG != 0 {
                return Ok(0);
            } else {
                let target = if target_pid == -1 {
                    None
                } else {
                    Some(target_pid as u64)
                };
                table.get_mut(&caller_pid).unwrap().state =
                    ProcState::Blocked(BlockReason::WaitingForChild(target));
                None
            }
        }; // table lock dropped here, before schedule() -- see table()'s own doc comment

        // Real reclaim: only set by the Exited arm above, always safe here (see
        // reaped_address_space's own doc comment).
        if let Some(address_space) = reaped_address_space {
            let phys_offset = memory::phys_mem_offset();
            with_frame_allocator(|fa| unsafe { address_space.teardown(phys_offset, fa) });
        }

        if let Some(reported) = reported {
            let (child_pid, status) = match reported {
                // `code` is already the real, fully wait(2)-encoded status by the time it reaches
                // Zombie(code) -- either WEXITSTATUS-shifted at oxidebsd_sys_exit (normal exit) or
                // the pre-encoded 128+sig value terminate_process's own Terminate path passes
                // directly (signal termination) -- see CLAUDE.md's own note on this. Written
                // through verbatim, not re-shifted.
                Reported::Exited(child_pid, code) => (child_pid, code),
                // 0x7f | (stopsig << 8) -- real WIFSTOPPED wire shape (confirmed disjoint from
                // both the exit encoding above, low byte always 0x00, and terminate_process's own
                // 128+sig signal-termination encoding, low byte always 0x81..0x9f).
                Reported::Stopped(child_pid, stopsig) => {
                    (child_pid, 0x7f | ((stopsig as i32) << 8))
                }
                // The real, exact WIFCONTINUED wire value -- no other bits allowed.
                Reported::Continued(child_pid) => (child_pid, 0xffff),
            };
            if status_ptr != 0 {
                // SAFETY: same known pointer-validation gap src/syscall.rs's sys_read/sys_write
                // already document -- status_ptr isn't checked against the caller's actual
                // mappings first. The caller's own address space is active right now (we're still
                // running on its behalf), so a genuinely valid pointer here is really writable; an
                // invalid one page-faults, handled safely elsewhere (log + reboot).
                unsafe { (status_ptr as *mut i32).write(status) };
            }
            // No per-process CPU-time/memory-usage accounting exists anywhere in this kernel --
            // see `RawRusage`'s own doc comment (`src/syscall.rs`) for why an honest all-zero
            // placeholder is written here rather than an invented number.
            crate::syscall::write_zeroed_rusage(rusage_ptr);
            return Ok(child_pid);
        }

        scheduler::schedule();
    }
}
/// `sys_exit`'s real, per-process logic (reached only through `syscall::oxidebsd_sys_exit`, the
/// native ABI's own exit handler). Marks the caller `Zombie(code)`; if its parent is blocked waiting on it
/// (or on any child), wakes the parent; then yields to the scheduler, which is guaranteed to
/// either switch to something else or `hlt_loop()` if nothing else is runnable — a `Zombie` is
/// never re-enqueued, so this call never returns.
///
/// Orphaned grandchildren are *not* reparented to a pid-1 "init" this pass — an accepted
/// simplification (see CLAUDE.md), not required for fork/exec/wait correctness.
pub fn do_exit(caller_pid: Pid, code: i32) -> ! {
    terminate_process(caller_pid, code);
    scheduler::schedule();
    unreachable!("do_exit: schedule() returned control to a Zombie process");
}

/// The actual state transition `do_exit` (the caller terminating itself) and `do_kill` (one
/// process terminating a *different* one, for a default-disposition signal — see that function's
/// own doc comment) both need: closes every fd `pid` still has open, marks it `Zombie(code)`, and
/// wakes its parent if it's blocked waiting on it (or on any child). Deliberately does *not* call
/// `scheduler::schedule()` — `do_exit` (terminating the *currently running* process) needs to
/// yield afterward; `do_kill` (terminating some other, not-currently-running process — only one
/// process is ever `Running` at a time on this cooperatively-scheduled kernel, so `target !=
/// caller` always means "not the one currently executing") must not, since the calling process
/// itself is still running and hasn't blocked or exited.
pub(crate) fn terminate_process(pid: Pid, code: i32) {
    // Real exit() semantics: every fd this process still has open gets closed automatically.
    // Genuinely load-bearing, not just tidiness -- see crate::fs::fd::close_all's own doc comment for
    // why a leaked fd here can leave a pipe's reader blocked forever.
    crate::fs::fd::close_all(pid);
    // Real SysV SEM_UNDO semantics: every accumulated adjustment this process ever recorded gets
    // applied (added back) on termination -- see `Process::sysv_sem_undo`'s own doc comment. Must
    // run *before* `PROCESS_TABLE` is locked below: `apply_undo_for_exit` takes that same lock
    // itself (briefly, to drain the list) and then wakes any blocked `semop` waiters via a second,
    // separate `process::table()` lock -- both would deadlock against `spin::Mutex`'s non-reentrant
    // guarantee if nested inside the lock this function is about to take.
    crate::fs::sysv_sem::apply_undo_for_exit(pid);
    // Real SysV shm semantics: every segment this process still had attached gets a real implicit
    // detach (nattch decrement, possible IPC_RMID finalization) -- see
    // `crate::fs::sysv_shm::detach_all_for_exit`'s own doc comment for why this must run before
    // `PROCESS_TABLE` is locked below, same reasoning as the `apply_undo_for_exit` call above.
    crate::fs::sysv_shm::detach_all_for_exit(pid);
    // Real fd-backed mmap semantics: every mapping this process still had live gets its content
    // written back and its extra reference released -- see `mm::
    // cleanup_mmap_file_regions_for_exit`'s own doc comment. Must run after the `close_all` call
    // above (which releases each mapped fd's *own* reference first, leaving only the mmap-held one
    // for this call to release -- matching real POSIX's "mmap() keeps the file open past close()")
    // and, for the same reentrant-`Mutex` reason as the two calls above, before `PROCESS_TABLE` is
    // locked below.
    mm::cleanup_mmap_file_regions_for_exit(pid);
    let mut table = PROCESS_TABLE.lock();
    match table.get_mut(&pid) {
        Some(me) if matches!(me.state, ProcState::Zombie(_)) => return, // already dead
        Some(me) => me.state = ProcState::Zombie(code),
        None => return,
    }
    // Real hardening, found live: `do_kill`'s cross-process `Action::Terminate` branch (a default-
    // disposition signal killing a target this process didn't just switch away from) can target a
    // process that's currently `Ready` and still sitting in `scheduler::READY_QUEUE` -- unlike the
    // adjacent `Action::Stop` branch, which already dequeues first (see that branch's own comment),
    // this path used to leave the queue entry dangling. If the caller (`do_wait4`) then reaps this
    // exact pid before the scheduler ever pops that stale entry, `PROCESS_TABLE` no longer has it
    // at all by the time `scheduler::activate_and_prepare` tries to switch to it -- a real panic
    // ("pid missing from table"), not just a logic bug. Found by the expanded POSIX conformance
    // pilot's own `sigaction/9-1.c`: its final `kill(pid, SIGHUP)` racing a child that (thanks to
    // `select()` immediately `ENOSYS`-ing rather than truly blocking, see `process::do_futex`'s own
    // doc comment for the identical-in-spirit `sem_wait` finding) cycles through `Ready` far more
    // often than a real system's `select()` ever would, made this race easy to hit. Safe (and cheap)
    // to call unconditionally here rather than gating on `state == Ready` first, unlike `Action::
    // Stop`'s own check -- `do_exit`'s self-termination path (this function's *other* caller) is
    // never `Ready` when it calls this (it's the one process currently `Running`), so this is a
    // harmless no-op there; `remove_ready` itself is already a no-op for a pid that isn't queued.
    scheduler::remove_ready(pid);
    wake_parent_if_waiting(&mut table, pid);
}

/// Wakes `child_pid`'s parent if it's blocked in `wait4` waiting on this child (or on any child) --
/// shared by `terminate_process` (a real exit) and the real stop/continue transitions in
/// `process::signals`/`do_stop_self` below (a `WUNTRACED`/`WCONTINUED`-observable state change,
/// not just exit). Spurious wakes are safe: `do_wait4`'s own loop always re-checks its real
/// condition fresh after `scheduler::schedule()` returns rather than trusting "state==Ready now"
/// to mean "my specific event happened" (see `ProcState::Stopped`'s own doc comment) -- so waking a
/// parent whose particular `wait4` call didn't actually request `WUNTRACED`/`WCONTINUED` just costs
/// it one harmless extra reschedule before it re-blocks.
pub(crate) fn wake_parent_if_waiting(table: &mut BTreeMap<Pid, Box<Process>>, child_pid: Pid) {
    let Some(parent_pid) = table.get(&child_pid).and_then(|p| p.parent) else {
        return;
    };
    let Some(parent) = table.get_mut(&parent_pid) else {
        return;
    };
    let should_wake = matches!(
        parent.state,
        ProcState::Blocked(BlockReason::WaitingForChild(target))
            if target.is_none() || target == Some(child_pid)
    );
    if should_wake {
        parent.state = ProcState::Ready;
        scheduler::enqueue_ready(parent_pid);
    }
}

/// Real self-targeted `SIGSTOP`/`SIGTSTP` (default disposition) -- reached from
/// `deliver_pending_signal`'s new `SignalDelivery::Stop` arm, the self-signal counterpart to
/// `process::signals`'s cross-process `Action::Stop` handling (`do_kill`/
/// `signal_foreground_group` -- a self-stop can only happen via an explicit
/// `kill(getpid(), SIGSTOP/SIGTSTP)`, since the interactive Ctrl+Z path always targets a
/// *different*, not-currently-running process). Sets `Stopped(signum)` + `stop_notify_pending`,
/// wakes a waiting parent, then blocks via the same `scheduler::schedule()` shape `do_nanosleep`
/// already uses -- resumes normally (does not diverge) once a later `SIGCONT` flips `state` back
/// to `Ready`.
pub(crate) fn do_stop_self(pid: Pid, signum: u64) {
    {
        let mut table = PROCESS_TABLE.lock();
        let me = table
            .get_mut(&pid)
            .expect("do_stop_self: current process missing from table");
        me.state = ProcState::Stopped(signum);
        me.stop_notify_pending = true;
        wake_parent_if_waiting(&mut table, pid);
    } // table lock dropped before schedule() -- see table()'s own doc comment
    scheduler::schedule();
}
/// Real Linux `reboot(2)` magic values (`third_party/musl/include/sys/reboot.h`) -- `reboot.c`'s
/// own musl wrapper passes these straight through as the real 3rd syscall argument, no call-site
/// patch needed (see this ABI's own remap comment in `bits/syscall.h.in`).
const RB_AUTOBOOT: u64 = 0x01234567;
const RB_HALT_SYSTEM: u64 = 0xcdef0123;
const RB_POWER_OFF: u64 = 0x4321fedc;

/// `SYS_REBOOT`'s real logic -- matches real Linux's own magic `cmd` values against this kernel's
/// three real actions (`src/reboot.rs`). No permission check: this kernel has no capability model
/// to gate real Linux's own `CAP_SYS_BOOT` requirement against, the same "collapses to
/// always-allowed" reasoning `do_setpgid`/`TIOCSCTTY`'s own `force` flag already use. Every
/// success arm diverges (`-> !`) -- the `Result` return type exists only for the `EINVAL` case.
pub fn do_reboot(cmd: u64) -> Result<u64, u64> {
    match cmd {
        RB_AUTOBOOT => crate::reboot::reboot(),
        RB_HALT_SYSTEM => crate::reboot::halt(),
        RB_POWER_OFF => crate::reboot::poweroff(),
        _ => Err(EINVAL),
    }
}
/// Backing store for `oxidebsd_get_cwd`/`oxidebsd_set_cwd` while `scheduler::current_pid() == 0`
/// -- i.e. only during boot, before `scheduler::start` ever runs a real process (pid 1 onward).
/// `modules/oxfs`'s own `module_init` self-check calls `chdir`/`mkdir`/etc. directly at exactly
/// this point, with no `Process` yet in `PROCESS_TABLE` for pid 0 to store a cwd in -- mirrors
/// `src/fd.rs`'s own `BOOTSTRAP_PID` idiom for the identical "boot-time, no real process exists
/// yet" problem. Never touched again once a real process is running, since `current_pid()` is
/// never `0` again after that.
static BOOT_CWD: AtomicU64 = AtomicU64::new(0);

/// Exposed to modules (see `src/module.rs`'s `resolve_external_symbol`) so a filesystem module can
/// track cwd per-process without the kernel needing to interpret what the value means -- see
/// `Process::cwd`'s own doc comment. No pid crosses the module boundary; the kernel resolves
/// `scheduler::current_pid()` itself, the same way `src/fd.rs` already does for the fd table.
pub(crate) extern "C" fn oxidebsd_get_cwd() -> u64 {
    let pid = scheduler::current_pid();
    if pid == 0 {
        return BOOT_CWD.load(Ordering::Relaxed);
    }
    table().lock().get(&pid).map(|p| p.cwd).unwrap_or(0)
}

pub(crate) extern "C" fn oxidebsd_set_cwd(inode: u64) {
    let pid = scheduler::current_pid();
    if pid == 0 {
        BOOT_CWD.store(inode, Ordering::Relaxed);
        return;
    }
    if let Some(p) = table().lock().get_mut(&pid) {
        p.cwd = inode;
    }
}

/// `BOOT_CWD`'s own counterpart for `Process::root_inode` -- same "no real process exists yet
/// during `modules/oxfs`'s own boot self-check" reasoning, and the same `0`-doubles-as-"real root"
/// default.
static BOOT_ROOT: AtomicU64 = AtomicU64::new(0);

/// Exposed to modules (see `src/module.rs`'s `resolve_external_symbol`) for `SYS_CHROOT` --
/// byte-for-byte mirrors `oxidebsd_get_cwd`/`oxidebsd_set_cwd` immediately above, right down to the
/// `pid == 0` boot-time fallback.
pub(crate) extern "C" fn oxidebsd_get_root() -> u64 {
    let pid = scheduler::current_pid();
    if pid == 0 {
        return BOOT_ROOT.load(Ordering::Relaxed);
    }
    table().lock().get(&pid).map(|p| p.root_inode).unwrap_or(0)
}

pub(crate) extern "C" fn oxidebsd_set_root(inode: u64) {
    let pid = scheduler::current_pid();
    if pid == 0 {
        BOOT_ROOT.store(inode, Ordering::Relaxed);
        return;
    }
    if let Some(p) = table().lock().get_mut(&pid) {
        p.root_inode = inode;
    }
}
/// The landing point for a never-run process's very first switch-in
/// (`context_switch::spawn_trampoline_asm` `call`s straight into this). Reads the current
/// process's stored entry point/user stack top and jumps into ring 3 exactly like the old one-shot
/// demo path did — `usermode::jump_to_usermode` itself is unchanged, just reached through a
/// different route now.
#[unsafe(no_mangle)]
extern "C" fn spawn_trampoline_inner() -> ! {
    let pid = scheduler::current_pid();
    let (entry, stack_top) = {
        let table = PROCESS_TABLE.lock();
        let p = table
            .get(&pid)
            .expect("spawn_trampoline_inner: current pid not in table");
        (p.entry_point, p.user_stack_top)
    };
    // SAFETY: this process's AddressSpace was activated (CR3) and its RSP0 repointed by
    // scheduler::start/schedule immediately before switching to it; its ELF segments and user
    // stack were mapped by spawn() when the process was created -- the same preconditions the old
    // run_userland_demo satisfied directly.
    unsafe { crate::process::usermode::jump_to_usermode(entry, stack_top) }
}
