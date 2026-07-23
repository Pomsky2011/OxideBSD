//! Process abstraction: PID allocation, the process control block (PCB), and the global process
//! table. Companion to `src/scheduler.rs` (which owns *when* a process runs) and
//! `src/context_switch.rs` (which owns *how* control moves from one kernel stack to another) —
//! this file owns process *state*: creating processes (`spawn`, `do_fork_from_current`), and the
//! syscalls that mutate that state (`do_execve`, `do_wait4`, `do_exit`, `do_getpid`).
//!
//! See CLAUDE.md's process/scheduler section for the full design rationale.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use spin::{Lazy, Mutex};
use x86_64::VirtAddr;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, Page, PageTableFlags, PhysFrame, Size4KiB,
};

use crate::address_space::AddressSpace;
use crate::elf::{self, Elf};
use crate::memory::{self, with_frame_allocator};
use crate::scheduler;
use crate::syscall::{self, ECHILD, EINVAL, ENOEXEC, ENOMEM, SyscallFrame};

pub type Pid = u64;

/// Floor: 128 KiB -- much larger than the 20 KiB the single, old, static RSP0 stack (`gdt.rs`'s
/// original fixed stack, before every process got its own) used to be. Found empirically, the hard
/// way: 16 KiB overflowed on `ls` (`SYS_OPEN` on a directory -> `modules/fat32`'s
/// `open_directory_listing`, deeper than plain `SYS_READ`/`SYS_WRITE`); 32 KiB then overflowed on
/// `fork()` (`do_fork_from_current` -> `AddressSpace::fork`'s 4-level page-table walk ->
/// `AddressSpace::new` -> `PageTable::clone()` -- in an unoptimized debug build, cloning a
/// 512-entry array through the generic `try_from_fn` machinery has a surprisingly large unoptimized
/// stack frame). There's no guard page (heap-allocated, not a dedicated mapped-with-a-gap region
/// like `gdt.rs`'s stacks), so a stack overflow here corrupts silently or double-faults rather than
/// failing cleanly -- this needs real margin for debug builds specifically, not just "enough for
/// the common case observed once," which is why RAM-constrained boots keep exactly this floor
/// rather than shrinking further (see `kernel_stack_size` below).
const KERNEL_STACK_SIZE_FLOOR: usize = 128 * 1024;
/// Ceiling: purely a bound on how much a RAM-rich boot hands each process for free (more headroom
/// against deeper call chains, at essentially no cost against a multi-GiB usable-RAM pool) -- not
/// something any code path has been observed to need.
const KERNEL_STACK_SIZE_CEILING: usize = 512 * 1024;

/// Scales the per-process kernel stack size to `memory::usable_ram_bytes()`, clamped to
/// `[KERNEL_STACK_SIZE_FLOOR, KERNEL_STACK_SIZE_CEILING]`. `spin::Lazy` (not a plain `const`)
/// because the real value depends on the memory map read at boot; safe to compute lazily since
/// every caller (`KernelStack::new`) only ever runs after `memory::BootInfoFrameAllocator::init`
/// has already populated `usable_ram_bytes` (process creation happens well after boot's memory
/// setup completes).
static KERNEL_STACK_SIZE: Lazy<usize> = Lazy::new(|| {
    let scaled = (memory::usable_ram_bytes() / 256) as usize; // 1/256th of usable RAM
    scaled.clamp(KERNEL_STACK_SIZE_FLOOR, KERNEL_STACK_SIZE_CEILING)
});

fn kernel_stack_size() -> usize {
    *KERNEL_STACK_SIZE
}

/// Fixed for every process, same constant `src/main.rs`'s old one-shot demo path used. `execve`
/// reuses it too — a fresh program image gets a fresh stack at the same fixed top — which is fine
/// since this only needs to be unique *within* one process's own address space, not across the
/// whole system; different address spaces don't share user-space mappings the way they share the
/// kernel half.
pub const USER_STACK_TOP: u64 = 0x_5000_0000_0000;

/// Floor: 4 pages (16 KiB) -- the fixed size this kernel always mapped, proven sufficient for
/// `stsh` and every program it execs so far.
const USER_STACK_PAGES_FLOOR: u64 = 4;
/// Ceiling: 64 pages (256 KiB) -- bounds how much a RAM-rich boot maps per process; nothing today
/// needs more.
const USER_STACK_PAGES_CEILING: u64 = 64;

/// Scales the per-process user stack size the same way `kernel_stack_size` scales the kernel-side
/// one, off the same `memory::usable_ram_bytes()` reading.
static USER_STACK_PAGES: Lazy<u64> = Lazy::new(|| {
    let scaled = memory::usable_ram_bytes() / (256 * 4096); // 1/256th of usable RAM, in pages
    scaled.clamp(USER_STACK_PAGES_FLOOR, USER_STACK_PAGES_CEILING)
});

fn user_stack_pages() -> u64 {
    *USER_STACK_PAGES
}

// Real FreeBSD syscall numbers, duplicated here rather than imported — same "no shared crate
// across this internal ABI boundary" convention `modules/fat32`/`modules/native_abi` already use
// for their own copies of these same constants. `do_execve` uses these to drive its own internal
// open/read-loop/close against the exact same fd/fat32 machinery `stsh`'s `cat` already exercises
// via `syscall::dispatch` directly (`dispatch` is `pub(crate)`, callable from arbitrary kernel
// code, not just from the `SYSCALL` entry stub).
const SYS_OPEN: u64 = 5;
const SYS_READ: u64 = 3;
const SYS_CLOSE: u64 = 6;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProcState {
    Ready,
    Running,
    Blocked(BlockReason),
    Zombie(i32),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BlockReason {
    /// `None` = waiting for any child (`wait4(-1, ...)`); `Some(pid)` = waiting for one specific
    /// child.
    WaitingForChild(Option<Pid>),
    /// Blocked in a `crate::pipe` read on an empty, still-open pipe (identified by that pipe's own
    /// id, not by fd — a pipe's read end can be `dup2`'d to other fds, but there's still only one
    /// underlying pipe). See `crate::pipe`'s module doc comment for why a pipe read has to
    /// genuinely block (not just return `Ok(0)`/`EAGAIN`) for a pipeline to work at all on this
    /// single-core, cooperatively-scheduled kernel.
    WaitingForPipeData(u64),
    /// Blocked in `crate::stdin::read` on an empty keyboard ring buffer. Unlike every other
    /// `BlockReason`, nothing *schedulable* ever wakes this one — the only thing that ever will is
    /// the keyboard IRQ handler itself, which is why `scheduler::schedule()`'s own "nothing
    /// runnable" fallback had to grow a real interrupts-enabled idle wait (`wait_for_ready`)
    /// instead of spinning forever with interrupts masked. See `crate::stdin`'s module doc comment
    /// for the full story — this is what makes `sh.elf` (BusyBox's `hush`), run with no `-c`
    /// argument, able to actually block reading a line from the keyboard instead of seeing an
    /// instant EOF.
    WaitingForStdin,
}

/// A process's own kernel stack: heap-allocated (not a fixed-size `static`/`static mut` array like
/// `gdt.rs`'s single RSP0 stack, since the number of processes isn't fixed) via a raw
/// `alloc`/`dealloc` pair rather than `Vec<u8>`/`Box<[u8]>`, neither of which guarantees the
/// 16-byte alignment `context_switch::SwitchFrame` needs.
struct KernelStack {
    base: *mut u8,
    layout: core::alloc::Layout,
}

impl KernelStack {
    fn new() -> Self {
        let stack_size = kernel_stack_size();
        let layout =
            core::alloc::Layout::from_size_align(stack_size, 16).expect("bad kernel stack layout");
        // SAFETY: layout has non-zero size (stack_size >= KERNEL_STACK_SIZE_FLOOR > 0).
        let base = unsafe { alloc::alloc::alloc_zeroed(layout) };
        assert!(!base.is_null(), "out of memory allocating a kernel stack");
        KernelStack { base, layout }
    }

    fn top(&self) -> VirtAddr {
        VirtAddr::from_ptr(self.base) + self.layout.size() as u64
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        // SAFETY: base/layout are exactly what alloc_zeroed returned in `new`; KernelStack is
        // never cloned or shared, so this is the sole owner.
        unsafe { alloc::alloc::dealloc(self.base, self.layout) };
    }
}

// SAFETY: KernelStack owns its allocation exclusively -- conceptually equivalent to `Box<[u8]>`'s
// own `Unique<u8>`, which is `Send` for the same reason. Needed because `PROCESS_TABLE` (below) is
// a `Mutex<BTreeMap<Pid, Box<Process>>>` static, which requires `Process` (and transitively this
// raw-pointer-holding field) to be `Send` for `Mutex<..>` to be `Sync`.
unsafe impl Send for KernelStack {}

pub struct Process {
    pub pid: Pid,
    pub parent: Option<Pid>,
    pub children: Vec<Pid>,
    pub state: ProcState,
    pub address_space: AddressSpace,
    #[allow(dead_code)] // kept alive for its Drop impl; never read after construction
    kernel_stack: KernelStack,
    pub kernel_stack_top: VirtAddr,
    /// Saved outgoing RSP for `context_switch::switch_context`, valid only while
    /// `state != Running` (i.e. only while this process isn't the one currently executing).
    pub rsp: u64,
    /// Consulted only by `spawn_trampoline_inner`, the first time a never-run process starts.
    pub entry_point: VirtAddr,
    /// Despite the name, this is the *initial `RSP`* the process starts running with — the
    /// `user_stack::build`-computed address of the argc/argv/envp/auxv image's own start, not the
    /// bare top of the mapped stack region (`process::USER_STACK_TOP`). Handed straight to
    /// `usermode::jump_to_usermode`/`syscall::redirect_frame` as the stack pointer.
    pub user_stack_top: VirtAddr,
    /// The current top of this process's `SYS_BRK`-managed heap region — see `do_brk`. Starts at
    /// the loaded ELF's `Elf::highest_loaded_address()` (page-aligned), grows/shrinks from there.
    pub brk: VirtAddr,
    /// This process's own `IA32_FS_BASE` value (see `SYS_SET_FS_BASE`), restored into the real MSR
    /// on every context switch into this process (`scheduler::activate_and_prepare`) — `IA32_FS_BASE`
    /// is a single global MSR, not something `context_switch::switch_context` saves/restores the
    /// way it does `RSP`/callee-saved GPRs, so without this every musl-linked process (which uses
    /// `%fs`-relative TLS, including the stack-protector canary check at `%fs:0x28`) would silently
    /// clobber every *other* process's TLS base the instant it set its own. A real, previously-
    /// latent bug: never surfaced by `true`/`echo`/`cat`/`musl-smoke` (each run directly by `stsh`,
    /// which isn't musl-linked and never touches `%fs` itself, and none of them survives long enough
    /// for a *different* still-running musl-linked process to resume and read a stale value) --
    /// only found once `sh` (BusyBox's `hush`, itself musl-linked) forked and `execve`'d another
    /// musl-linked child and then kept running *after* that child exited: `hush`'s own next
    /// stack-protector check read through the dead child's own leftover `FS_BASE`, page-faulting on
    /// whatever happened to be at that address in `hush`'s own (unrelated) address space. Starts at
    /// `0` for a freshly spawned/`execve`'d process (no TLS set up yet); a forked child inherits the
    /// parent's live value (real `fork()` semantics — TLS state is copied, not reset).
    pub fs_base: u64,
    /// Unused today; reserved so a future priority scheduler doesn't need a PCB layout change.
    #[allow(dead_code)]
    pub priority: u8,
}

static NEXT_PID: AtomicU64 = AtomicU64::new(1);
static PROCESS_TABLE: Mutex<BTreeMap<Pid, Box<Process>>> = Mutex::new(BTreeMap::new());

fn alloc_pid() -> Pid {
    NEXT_PID.fetch_add(1, Ordering::Relaxed)
}

/// `Box<Process>` (not `Process` by value) is load-bearing, not stylistic: it lets callers pull a
/// raw `*mut Process`/copy out needed fields from under a short-held lock and drop that lock
/// *before* doing anything that might call `scheduler::schedule()` — the `BTreeMap`'s internal
/// tree nodes can move on insert/remove, but a `Box`'s own heap allocation never does. Holding this
/// lock across a context switch would only be released whenever that exact stack next resumes —
/// a real deadlock risk if the switched-to process needs this same lock, which it always will.
pub(crate) fn table() -> &'static Mutex<BTreeMap<Pid, Box<Process>>> {
    &PROCESS_TABLE
}

#[derive(Debug)]
pub enum SpawnError {
    Elf(elf::ElfError),
}

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
    let entry = with_frame_allocator(|fa| elf::load(&elf, &mut mapper, fa, phys_offset))
        .map_err(SpawnError::Elf)?;

    let stack_top = VirtAddr::new(USER_STACK_TOP);
    let mapped_pages = map_user_stack(&mut mapper, stack_top);
    // spawn() has no real invocation path to use as argv[0] (unlike do_execve, which knows exactly
    // what path it opened) -- this is only ever pid 1, built directly from an embedded ELF at
    // boot, so a fixed placeholder is all there is to give. Neither of spawn()'s two current
    // callers (stsh, tests/fork_wait.rs's fork-exec-smoke) reads its own argv, so this is inert
    // until a real libc (musl) is what's spawned this way.
    let initial_rsp = crate::user_stack::build(
        &elf,
        &[b"(init)"],
        &[],
        stack_top,
        user_stack_bottom(stack_top),
        &mapped_pages,
        phys_offset,
    );

    let pid = alloc_pid();
    let kernel_stack = KernelStack::new();
    let kernel_stack_top = kernel_stack.top();
    let rsp = crate::context_switch::seed_spawn_frame(kernel_stack_top);

    let process = Process {
        pid,
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
        priority: 0,
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
    // Bootstraps this process's own stdin/stdout/stderr from crate::fd::init's own pseudo-pid
    // registration -- the same fork_inherit path a real fork() uses, see that function's own doc
    // comment.
    crate::fd::fork_inherit(0, pid);
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
    let (child_address_space, parent_brk, parent_fs_base) = {
        let mut table = PROCESS_TABLE.lock();
        let parent = table
            .get_mut(&caller_pid)
            .expect("fork: current process missing from table");
        // SAFETY: AddressSpace::fork requires self to be the currently active address space --
        // true here, since sys_fork runs synchronously on the calling process's own kernel stack
        // with its own CR3 still live.
        let child_address_space =
            with_frame_allocator(|fa| parent.address_space.fork(phys_offset, fa));
        (child_address_space, parent.brk, parent.fs_base)
    };

    let kernel_stack = KernelStack::new();
    let kernel_stack_top = kernel_stack.top();
    // SAFETY: parent_frame is the caller's own live SyscallFrame, valid for the duration of this
    // call (we're still inside sys_fork's own handling of it).
    let rsp = unsafe { crate::context_switch::seed_fork_frame(kernel_stack_top, parent_frame) };

    let child = Process {
        pid: child_pid,
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
        priority: 0,
    };

    {
        let mut table = PROCESS_TABLE.lock();
        table.get_mut(&caller_pid).unwrap().children.push(child_pid);
        table.insert(child_pid, Box::new(child));
    }
    // Real fork() semantics: the child gets its own independently-closable copy of every fd the
    // parent has open, not a shared view of the parent's own table entries -- see
    // crate::fd::fork_inherit's own doc comment for why this specifically matters for pipes.
    crate::fd::fork_inherit(caller_pid, child_pid);
    scheduler::enqueue_ready(child_pid);
    crate::serial_println!("[proc] pid {} forked pid {}", caller_pid, child_pid);
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
/// argv[1..] or envp[] (same shape, read the same way -- see `read_ptr_len_array` below); argv[0]
/// is always `path_bytes` itself (unchanged from before `argv_ptr` existed), so a caller that only
/// wants argv[0] and no environment just passes `argv_ptr == 0`/`envp_ptr == 0` -- every caller
/// before `stsh`'s own execve wrapper grew argument support did exactly that, and keeps doing so
/// unaffected. `envp_ptr` is `R10`, the ABI's 4th argument -- see `src/syscall.rs`'s module doc
/// comment for why that register only became a real, read argument once this needed it.
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
    let extra_argv = read_ptr_len_array(argv_ptr);
    let envp = read_ptr_len_array(envp_ptr);

    let fd = syscall::dispatch(SYS_OPEN, path_ptr, path_len, 0, 0)?;

    let mut elf_bytes: Vec<u8> = Vec::new();
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
            Ok(n) => elf_bytes.extend_from_slice(&chunk[..n as usize]),
            Err(errno) => {
                let _ = syscall::dispatch(SYS_CLOSE, fd, 0, 0, 0);
                return Err(errno);
            }
        }
    }
    let _ = syscall::dispatch(SYS_CLOSE, fd, 0, 0, 0);

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
    let entry = with_frame_allocator(|fa| elf::load(&elf, &mut mapper, fa, phys_offset))
        .map_err(|_| ENOEXEC)?;
    let stack_top = VirtAddr::new(USER_STACK_TOP);
    let mapped_pages = map_user_stack(&mut mapper, stack_top);
    // argv[0] is always path_bytes itself; argv[1..] is whatever extra_argv (read above, while the
    // caller's own address space was still active) supplied. envp is real now too -- whatever the
    // caller's own envp_ptr described, or empty if it passed 0 (every caller before this existed
    // did exactly that, and keeps doing so unaffected -- see RawArgvEntry's own doc comment).
    let mut argv: Vec<&[u8]> = Vec::with_capacity(1 + extra_argv.len());
    argv.push(&path_bytes);
    argv.extend(extra_argv.iter().map(Vec::as_slice));
    let envp_refs: Vec<&[u8]> = envp.iter().map(Vec::as_slice).collect();
    let initial_rsp = crate::user_stack::build(
        &elf,
        &argv,
        &envp_refs,
        stack_top,
        user_stack_bottom(stack_top),
        &mapped_pages,
        phys_offset,
    );

    // ---- commit point: nothing above may fail past here ----
    // SAFETY: new_address_space carries the kernel's own mappings (shallow-copied at construction,
    // same as every address space) plus the just-loaded ELF and user stack -- the same guarantee
    // AddressSpace::activate's own contract requires. Activating it mid-syscall, still running on
    // the caller's own kernel stack, is safe: the kernel half is identical no matter which address
    // space is live.
    unsafe { new_address_space.activate() };
    {
        let mut table = PROCESS_TABLE.lock();
        let me = table
            .get_mut(&caller_pid)
            .expect("execve: current process missing from table");
        // Old AddressSpace dropped here -- its frames leak (no FrameDeallocator exists anywhere in
        // this codebase yet, see CLAUDE.md's known-limitations list), consistent with `elf::load`/
        // `AddressSpace::new` never freeing anything either.
        me.address_space = new_address_space;
        me.user_stack_top = initial_rsp;
        me.entry_point = entry;
        me.brk = VirtAddr::new(elf.highest_loaded_address());
        // The old program's TLS base doesn't mean anything to the new one -- reset the stored value
        // (restored on every future context switch, see Process::fs_base's own doc comment) *and*
        // the live MSR right now, since execve keeps running as this exact process/kernel stack with
        // no context switch in between; leaving the stale value live until the new program's own
        // crt1 gets around to calling SYS_SET_FS_BASE would be a real (if narrow) window for it to
        // read garbage through %fs before then.
        me.fs_base = 0;
        x86_64::registers::model_specific::FsBase::write(VirtAddr::new(0));
    }
    crate::serial_println!("[proc] pid {} execve'd, entry={:?}", caller_pid, entry);

    let frame = syscall::current_frame();
    // SAFETY: frame is this exact syscall's own live frame -- do_execve is only ever reached via
    // dispatch(SYS_EXECVE, ...), called from within syscall_dispatch, which set CURRENT_FRAME to
    // it just before.
    unsafe { syscall::redirect_frame(frame, entry, initial_rsp) };
    Ok(0)
}

/// `sys_wait4`'s real logic. If the caller already has a `Zombie` child matching `target_pid`
/// (`-1` = any), reaps it immediately (removes it from the table, writes its exit code through the
/// optional `status_ptr`, returns its pid). If the caller has no child matching `target_pid` at
/// all, `ECHILD`. Otherwise blocks (`ProcState::Blocked`) and calls `scheduler::schedule()`, which
/// only returns once something (`do_exit`, on a matching child) wakes it back to `Ready` and the
/// scheduler picks it again — at which point the loop re-checks from the top, since `do_exit` only
/// wakes the parent, it doesn't hand the child's info across directly.
pub fn do_wait4(caller_pid: Pid, target_pid: i64, status_ptr: u64) -> Result<u64, u64> {
    let matches = |pid: Pid| target_pid == -1 || target_pid as u64 == pid;

    loop {
        let reaped = {
            let mut table = PROCESS_TABLE.lock();

            let has_matching_child = table
                .get(&caller_pid)
                .expect("wait4: current process missing from table")
                .children
                .iter()
                .any(|&c| matches(c));
            if !has_matching_child {
                return Err(ECHILD);
            }

            let zombie = table
                .get(&caller_pid)
                .unwrap()
                .children
                .iter()
                .copied()
                .filter(|&c| matches(c))
                .find_map(|c| match table.get(&c).map(|p| p.state) {
                    Some(ProcState::Zombie(code)) => Some((c, code)),
                    _ => None,
                });

            match zombie {
                Some((child_pid, code)) => {
                    table.remove(&child_pid);
                    table
                        .get_mut(&caller_pid)
                        .unwrap()
                        .children
                        .retain(|&c| c != child_pid);
                    Some((child_pid, code))
                }
                None => {
                    let target = if target_pid == -1 {
                        None
                    } else {
                        Some(target_pid as u64)
                    };
                    table.get_mut(&caller_pid).unwrap().state =
                        ProcState::Blocked(BlockReason::WaitingForChild(target));
                    None
                }
            }
        }; // table lock dropped here, before schedule() -- see table()'s own doc comment

        if let Some((child_pid, code)) = reaped {
            if status_ptr != 0 {
                // SAFETY: same known pointer-validation gap src/syscall.rs's sys_read/sys_write
                // already document -- status_ptr isn't checked against the caller's actual
                // mappings first. The caller's own address space is active right now (we're still
                // running on its behalf), so a genuinely valid pointer here is really writable; an
                // invalid one page-faults, handled safely elsewhere (log + reboot).
                unsafe { (status_ptr as *mut i32).write(code) };
            }
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
    crate::serial_println!("[proc] pid {} exited with code {}", caller_pid, code);
    // Real exit() semantics: every fd this process still has open gets closed automatically.
    // Genuinely load-bearing, not just tidiness -- see crate::fd::close_all's own doc comment for
    // why a leaked fd here can leave a pipe's reader blocked forever.
    crate::fd::close_all(caller_pid);
    {
        let mut table = PROCESS_TABLE.lock();
        if let Some(me) = table.get_mut(&caller_pid) {
            me.state = ProcState::Zombie(code);
        }
        let parent_pid = table.get(&caller_pid).and_then(|p| p.parent);
        if let Some(parent_pid) = parent_pid
            && let Some(parent) = table.get_mut(&parent_pid)
        {
            let should_wake = matches!(
                parent.state,
                ProcState::Blocked(BlockReason::WaitingForChild(target))
                    if target.is_none() || target == Some(caller_pid)
            );
            if should_wake {
                parent.state = ProcState::Ready;
                scheduler::enqueue_ready(parent_pid);
            }
        }
    }
    scheduler::schedule();
    unreachable!("do_exit: schedule() returned control to a Zombie process");
}

pub fn do_getpid() -> u64 {
    scheduler::current_pid()
}

/// `0` for a process with no parent (pid 1 itself), matching real `getppid()`'s convention for
/// the boot/init process — every other process always has one, set at `fork`/`spawn` time.
pub fn do_getppid() -> u64 {
    let table = table().lock();
    table
        .get(&scheduler::current_pid())
        .and_then(|p| p.parent)
        .unwrap_or(0)
}

/// Fixed VA window for anonymous `SYS_MMAP` allocations — a fresh region, not reused from
/// `module::MODULE_VA_BASE` (that one's kernel-mapped and shared across every address space; an
/// mmap region has to be per-process, mapped `USER_ACCESSIBLE` only in the calling process's own
/// table). Bump-allocated and never reclaimed, same "hand out forward, never reuse" policy as
/// `module::NEXT_MODULE_PAGE`/`BootInfoFrameAllocator` — consistent with this whole codebase
/// having no deallocation path anywhere yet. A single global counter is safe even across multiple
/// processes: this only hands out VA *values*, mapped separately into whichever process's own
/// address space asked for one — two different processes reusing the same numeric VA in their own
/// tables never interferes, no shared visibility (the same reasoning `USER_STACK_TOP` already
/// relies on being "fixed but per-address-space").
const MMAP_REGION_BASE: u64 = 0x_2000_0000_0000;
const MMAP_REGION_CEILING: u64 = 0x_3000_0000_0000;
static NEXT_MMAP_PAGE: Mutex<u64> = Mutex::new(MMAP_REGION_BASE);

/// `SYS_MMAP`'s real logic — OxideBSD's own invention, not modeled on any real OS's `mmap` (see
/// `src/syscall.rs`'s module doc comment). `addr_hint`/`prot` occupy real `mmap`'s first and third
/// argument positions (musl's libc wrapper always sends `addr, len, prot, flags, fd, off` in
/// `rdi, rsi, rdx, r10, r8, r9`, and this ABI only reads the first three registers), but are
/// ignored: OxideBSD always chooses the address itself, and every mapped page is unconditionally
/// `PRESENT | WRITABLE | USER_ACCESSIBLE` regardless of requested protection — the same
/// simplification `src/module.rs`'s own loader already applies. Always anonymous+private (the
/// only case musl's allocator needs); `flags`/`fd`/`offset` aren't even readable at this ABI's
/// 3-argument width, so there's no way to request anything else in the first place.
pub fn do_mmap(caller_pid: Pid, addr_hint: u64, len: u64, prot: u64) -> Result<u64, u64> {
    let _ = (addr_hint, prot);
    if len == 0 {
        return Err(EINVAL);
    }
    let page_count = len.div_ceil(4096);
    let region_len = page_count * 4096;

    let base = {
        let mut next = NEXT_MMAP_PAGE.lock();
        let base = *next;
        let end = base.checked_add(region_len).ok_or(ENOMEM)?;
        if end > MMAP_REGION_CEILING {
            return Err(ENOMEM);
        }
        *next = end;
        base
    };

    let phys_offset = memory::phys_mem_offset();
    let mut table = PROCESS_TABLE.lock();
    let me = table
        .get_mut(&caller_pid)
        .expect("mmap: current process missing from table");
    // SAFETY: me.address_space is the currently active address space -- mmap runs synchronously on
    // the caller's own kernel stack mid-syscall, with its own CR3 still live -- sound for the same
    // reason AddressSpace::fork's own doc comment already establishes for this "active table" case.
    let mut mapper = unsafe { me.address_space.mapper(phys_offset) };

    let start_page = Page::<Size4KiB>::containing_address(VirtAddr::new(base));
    let end_page = Page::<Size4KiB>::containing_address(VirtAddr::new(base + region_len - 1));
    with_frame_allocator(|fa| -> Result<(), u64> {
        for page in Page::range_inclusive(start_page, end_page) {
            let frame = fa.allocate_frame().ok_or(ENOMEM)?;
            // SAFETY: frame was just allocated (unused, per BootInfoFrameAllocator's contract),
            // and page falls in this process's own, freshly bump-allocated mmap region.
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
                    .map_err(|_| ENOMEM)?
                    .flush();
            }
            // Real anonymous mmap guarantees zero-filled pages; frames from BootInfoFrameAllocator
            // aren't pre-zeroed, so this has to happen explicitly (same technique elf::load uses
            // for BSS).
            let frame_ptr = (phys_offset + frame.start_address().as_u64()).as_mut_ptr::<u8>();
            unsafe { core::ptr::write_bytes(frame_ptr, 0, 4096) };
        }
        Ok(())
    })?;

    Ok(base)
}

/// `SYS_MUNMAP`'s real logic: a no-op success, consistent with this codebase having no
/// `FrameDeallocator` anywhere yet — matches `module.rs`/`BootInfoFrameAllocator`'s own "hand out
/// forward, never reclaim" policy. Doesn't validate `addr`/`len` against what `do_mmap` handed
/// out; nothing downstream depends on that. If this ever needs to become a real unmap, it should
/// call `Mapper::unmap` on the affected pages (removing translations is sound without a
/// `FrameDeallocator`; only *freeing* the backing frames needs one).
pub fn do_munmap(addr: u64, len: u64) -> Result<u64, u64> {
    let _ = (addr, len);
    Ok(0)
}

/// Ceiling for `SYS_BRK`-managed heap growth — matches `module::MODULE_VA_BASE` so a growing heap
/// can never collide with the kernel-mapped module region every address space shares.
const BRK_REGION_CEILING: u64 = 0x_1000_0000;

/// `SYS_BRK`'s real logic. `addr == 0` queries the current value without changing it (the
/// convention every real `sbrk(0)` already relies on). Shrinking just lowers the stored value —
/// no unmap, same no-reclaim simplification `do_munmap` above documents. Growing maps freshly
/// zeroed pages from the first not-yet-mapped page onward: `me.brk` isn't necessarily page-aligned
/// (a previous grow may have stopped mid-page), so the *page containing* the old value is already
/// mapped and must be skipped, not re-mapped (`Mapper::map_to` fails on an already-present page).
pub fn do_brk(caller_pid: Pid, addr: u64) -> Result<u64, u64> {
    let phys_offset = memory::phys_mem_offset();
    let mut table = PROCESS_TABLE.lock();
    let me = table
        .get_mut(&caller_pid)
        .expect("brk: current process missing from table");

    if addr == 0 {
        return Ok(me.brk.as_u64());
    }
    if addr <= me.brk.as_u64() {
        me.brk = VirtAddr::new(addr);
        return Ok(addr);
    }
    if addr > BRK_REGION_CEILING {
        return Err(ENOMEM);
    }

    let old_top = me.brk.as_u64();
    let new_top = addr;
    let map_start = old_top.div_ceil(4096) * 4096;
    if new_top > map_start {
        // SAFETY: see do_mmap's identical reasoning -- me.address_space is the currently active
        // address space.
        let mut mapper = unsafe { me.address_space.mapper(phys_offset) };
        let start_page = Page::<Size4KiB>::containing_address(VirtAddr::new(map_start));
        let end_page = Page::<Size4KiB>::containing_address(VirtAddr::new(new_top - 1));
        with_frame_allocator(|fa| -> Result<(), u64> {
            for page in Page::range_inclusive(start_page, end_page) {
                let frame = fa.allocate_frame().ok_or(ENOMEM)?;
                // SAFETY: frame was just allocated; page starts at the first not-yet-mapped page
                // past the current brk, so it isn't already present.
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
                        .map_err(|_| ENOMEM)?
                        .flush();
                }
                let frame_ptr = (phys_offset + frame.start_address().as_u64()).as_mut_ptr::<u8>();
                unsafe { core::ptr::write_bytes(frame_ptr, 0, 4096) };
            }
            Ok(())
        })?;
    }

    me.brk = VirtAddr::new(new_top);
    Ok(new_top)
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
    unsafe { crate::usermode::jump_to_usermode(entry, stack_top) }
}
