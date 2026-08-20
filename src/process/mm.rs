//! mmap/munmap/mprotect/brk syscalls -- split out of the original process.rs.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use spin::Mutex;
use x86_64::VirtAddr;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, Page, PageTableFlags, PhysFrame, Size4KiB, Translate,
};
use x86_64::structures::paging::mapper::TranslateResult;

use crate::memory::{self, with_frame_allocator};
use crate::syscall::{EINVAL, ENODEV, ENOMEM};
use super::*;

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

/// Real `PROT_WRITE` (matches every real Unix's value) — the only `prot` bit `do_mmap` currently
/// consults, to decide whether a real fd-backed mapping's frames get write-back on `munmap`/exit.
/// Every mapped page is still unconditionally `WRITABLE` at the page-table level regardless
/// (`do_mprotect`'s own doc comment already documents this kernel's total lack of real page
/// protection enforcement) — this only gates the *software* write-back decision, not hardware
/// access.
const PROT_WRITE: u64 = 0x2;

/// Real, cross-open shared physical-frame cache for fd-backed `MAP_SHARED` mappings — keyed by
/// `crate::fs::fd::content_id_of`'s real inode identity, not by `real_fd`/pid. **This is what
/// makes two *separate* `open()` calls on the same underlying file (`shm_open`'s own documented
/// re-open-by-name pattern, or any two processes independently opening the same path) actually
/// share live writes with no `msync` needed** — real POSIX `MAP_SHARED` guarantees exactly this,
/// and there is no page-fault/dirty-tracking machinery in this kernel to synthesize it any other
/// way; sharing the *same physical frames* directly, the same trick `crate::fs::sysv_shm` already
/// uses for real SysV shared memory, sidesteps needing one. Grows in place while any mapping is
/// still live (never shrinks/discards a shorter existing entry — see `do_mmap_file_backed`'s own
/// doc comment) — but **is evicted, not leaked, once `MMAP_FILE_REFCOUNT` for a `content_id` drops
/// to zero** (every live mapping of it torn down). Found live, not designed in up front: a
/// never-evicted cache made an in-memory-only modification to the partial page beyond a mapped
/// object's own real length outlive every mapping of it, including across a full `munmap`+
/// process-exit+fresh-`open`+fresh-`mmap` cycle — `mmap/11-4.c`/`11-5.c` write such a byte and
/// require a *later, independent* mapping to never see it (real POSIX: that partial-page content
/// was never part of the object to begin with, only ever visible through the specific mapping that
/// wrote it). Eviction doesn't free the frames themselves (this kernel has no frame-deallocation
/// path anywhere) — a later mmap of the same file just allocates a fresh set and re-populates from
/// the real (correctly `writeback_region`-clamped) file content, exactly as if it were the first
/// mmap of that file ever.
static MMAP_FILE_CACHE: Mutex<BTreeMap<u64, Vec<PhysFrame<Size4KiB>>>> =
    Mutex::new(BTreeMap::new());

/// How many currently-live `MmapFileRegion`s (across every process) reference each `content_id` —
/// see `MMAP_FILE_CACHE`'s own doc comment for why this drives real cache eviction rather than
/// being purely informational.
static MMAP_FILE_REFCOUNT: Mutex<BTreeMap<u64, u64>> = Mutex::new(BTreeMap::new());

/// Releases one reference to `content_id`'s cache entry — call after `writeback_region` has
/// already run for the region being torn down. Evicts `MMAP_FILE_CACHE`'s own entry once the
/// count reaches zero (see that static's own doc comment).
fn release_mmap_file_ref(content_id: u64) {
    let mut refs = MMAP_FILE_REFCOUNT.lock();
    let Some(count) = refs.get_mut(&content_id) else {
        return;
    };
    *count -= 1;
    if *count == 0 {
        refs.remove(&content_id);
        drop(refs);
        MMAP_FILE_CACHE.lock().remove(&content_id);
    }
}

/// One real fd-backed `MAP_SHARED` mapping still live in a process's own address space — enough
/// state for `do_munmap` (or exit/`execve` cleanup) to write real content back to the underlying
/// file (via `crate::fs::fd::content_write`, keyed by `content_id` alone — no fd/`real_fd`
/// involved at all, see that function's own doc comment for why). Not inherited by `fork`
/// (`Process::mmap_file_regions` starts empty in a forked child) — same documented simplification
/// `Process::sysv_shm_attach` already established, for the identical underlying reason: this
/// kernel's `fork` is a full eager address-space copy, not real copy-on-write, so a child's own
/// page-table entries at these VAs already point at freshly-copied *private* frames regardless of
/// what this list remembers.
pub struct MmapFileRegion {
    va_start: u64,
    npages: u64,
    /// How many pages starting at `va_start` are actually backed by real frames -- always
    /// `<= npages`, computed once at `mmap()` time from the object's own real length rounded up to
    /// a whole page (`do_mmap_file_backed`'s own doc comment). Frozen at creation, not re-checked
    /// against the object's current length on every fault -- a documented simplification (real
    /// Linux dynamically re-checks the current inode size on each fault, letting a later
    /// `ftruncate()` grow retroactively into what SIGBUSed before; no test in this kernel's own
    /// conformance pilot exercises that). Pages `[mapped_pages, npages)` are deliberately left
    /// *unmapped* in the caller's page table -- real POSIX MPR: a reference anywhere in that range
    /// must raise `SIGBUS`, not silently succeed against a zero-filled page -- see
    /// `signal_for_user_fault` below, which is what actually turns "unmapped" into a real signal
    /// instead of the reboot every other unmapped ring-3 reference still gets.
    mapped_pages: u64,
    content_id: u64,
    writable: bool,
}

/// `SYS_MMAP`'s real logic — OxideBSD's own invention, not modeled on any real OS's `mmap` (see
/// `src/syscall.rs`'s module doc comment). `addr_hint` is still ignored (OxideBSD always chooses
/// the address itself, matching `src/module.rs`'s own loader). `fd`/`off` are real now — decoded
/// by the caller (`src/syscall/ffi.rs`'s `oxidebsd_sys_mmap`) from the one spare ABI register this
/// syscall has room for (see that function's own doc comment for the packed wire format and why
/// `flags` itself was dropped rather than threaded through: every real caller in this kernel's own
/// call graph distinguishes anonymous vs. file-backed purely by `fd == -1` vs. `fd >= 0`, matching
/// musl's own convention, so there's nothing real `flags` bits would add here). `fd == -1`: the
/// original anonymous+private, always-zero-filled, bump-allocated behavior, unchanged. `fd >= 0`:
/// real fd-backed `MAP_SHARED` — see `do_mmap_file_backed`.
pub fn do_mmap(caller_pid: Pid, addr_hint: u64, len: u64, prot: u64, fd: i32, off: u32) -> Result<u64, u64> {
    let _ = addr_hint;
    if len == 0 {
        return Err(EINVAL);
    }
    let page_count = len.div_ceil(4096);
    let region_len = page_count * 4096;

    if fd >= 0 {
        return do_mmap_file_backed(caller_pid, fd as u64, off, page_count, prot);
    }

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

/// Real fd-backed `MAP_SHARED` — `do_mmap`'s `fd >= 0` case. Scoped deliberately: `off != 0` is an
/// honest `EINVAL` rather than silently mapping from offset `0` instead — none of this kernel's
/// own real callers (BusyBox, TinyCC-compiled programs, the POSIX conformance pilot) ever request
/// a nonzero offset. Always behaves as `MAP_SHARED` regardless of what the caller actually
/// requested (`MAP_PRIVATE`'s copy-on-write semantics would need real per-page fault tracking this
/// kernel has none of) — a deliberate, documented simplification, not yet hit by any real caller
/// requesting `MAP_PRIVATE` against a real fd.
///
/// **Content population/writeback goes through `crate::fs::fd::content_read`/`content_write`
/// (keyed by `content_id`, a real inode number), not any fd's own read/write callbacks** — found
/// live, not designed in from the start: `modules/oxfs`'s `oxfs_read` unconditionally fails
/// (`-EBADF`) for a fd still in `OpenFile::Write` state, which is exactly the state every real
/// caller's fd is still in at `mmap()` time (`open(O_CREAT|O_RDWR) -> ftruncate()/fstat() ->
/// mmap()`, never `close()`d first) — a population path built on the generic per-fd `read`
/// callback silently populated nothing for every real test that exercises this. Content-by-
/// identity sidesteps that, and (unlike a fd's own stream position, which `write()`/`fstat()`
/// calls before `mmap()` already moved forward) always reads from real offset `0`.
fn do_mmap_file_backed(
    caller_pid: Pid,
    fd: u64,
    off: u32,
    page_count: u64,
    prot: u64,
) -> Result<u64, u64> {
    if off != 0 {
        return Err(EINVAL);
    }
    let content_id = crate::fs::fd::content_id_of(fd).ok_or(ENODEV)?;

    let phys_offset = memory::phys_mem_offset();
    let real_size = crate::fs::fd::content_size(content_id).max(0) as u64;
    // Real POSIX MPR (`mmap/11-2.c`/`11-3.c` in the conformance pilot): only pages actually
    // overlapping the object's own real content -- including its final, real-content-plus-
    // zero-padding partial page -- ever get backed by a real frame. Anything past that, up to the
    // caller's own requested `page_count`, is deliberately left unmapped so a reference there
    // real-faults instead of silently succeeding against a zero-filled page; `signal_for_user_fault`
    // below is what turns that fault into a real `SIGBUS` rather than a reboot.
    let covered_pages = real_size.div_ceil(4096).min(page_count);

    let frames: Vec<PhysFrame<Size4KiB>> = {
        let mut cache = MMAP_FILE_CACHE.lock();
        let existing_len = cache.get(&content_id).map(Vec::len).unwrap_or(0) as u64;
        if existing_len < covered_pages {
            // First mmap of this file, or a later mmap asking for more real-content pages than any
            // earlier one did -- allocate+zero exactly the *new* frames needed and append them,
            // never discarding whatever's already cached (those frames may already be live in
            // another process's, or this same process's, own page table -- see MMAP_FILE_CACHE's
            // own doc comment for why replacing them outright would be a real correctness bug, not
            // just a missed optimization).
            let new_frames: Vec<PhysFrame<Size4KiB>> = with_frame_allocator(|fa| {
                let mut new_frames = Vec::with_capacity((covered_pages - existing_len) as usize);
                for _ in existing_len..covered_pages {
                    let frame = fa.allocate_frame().ok_or(ENOMEM)?;
                    let frame_ptr =
                        (phys_offset + frame.start_address().as_u64()).as_mut_ptr::<u8>();
                    unsafe { core::ptr::write_bytes(frame_ptr, 0, 4096) };
                    new_frames.push(frame);
                }
                Ok::<_, u64>(new_frames)
            })?;
            // Populate only the genuinely new region from real file content, via one staging
            // buffer read (read_inode_at already walks the real block chain internally, so this
            // doesn't need its own frame-by-frame loop the way population once did) then copied
            // out page by page (frames from a bump allocator aren't guaranteed physically
            // contiguous, so a single multi-page write through the phys-mem-offset window isn't
            // safe). Clamped to the real object's own current length -- a file shorter than the
            // requested mapping leaves the remainder zero (already true: `new_frames` start
            // zeroed), matching POSIX's own "bytes past EOF within the mapped region read as
            // zero".
            let new_region_start = existing_len * 4096;
            let covered_len = covered_pages * 4096;
            let read_len = real_size
                .saturating_sub(new_region_start)
                .min(covered_len - new_region_start);
            if read_len > 0 {
                let mut staging = alloc::vec![0u8; read_len as usize];
                let n = crate::fs::fd::content_read(
                    content_id,
                    new_region_start,
                    staging.as_mut_ptr() as u64,
                    read_len,
                );
                if n > 0 {
                    staging.truncate(n as usize);
                    for (i, chunk) in staging.chunks(4096).enumerate() {
                        let frame_ptr =
                            (phys_offset + new_frames[i].start_address().as_u64()).as_mut_ptr::<u8>();
                        unsafe {
                            core::ptr::copy_nonoverlapping(chunk.as_ptr(), frame_ptr, chunk.len())
                        };
                    }
                }
            }
            cache.entry(content_id).or_default().extend(new_frames);
        }
        // `.entry(...).or_default()`, not `.get(...).expect(...)`: the growth block above is
        // skipped entirely when `covered_pages == 0` (an object with no real content yet, e.g. a
        // freshly `open(O_CREAT)`'d file `mmap()`'d before any `write()`/`ftruncate()`) *and*
        // nothing was cached for this content_id before (`existing_len == 0`, so `0 < 0` is
        // false) -- the very first such mmap of a file had no entry to `.get()`, and the `.expect`
        // here panicked instead of just producing the correct empty `frames` list. Found live, not
        // caught by this session's own smoke test (every scenario there `ftruncate`s first).
        cache
            .entry(content_id)
            .or_default()
            .iter()
            .take(covered_pages as usize)
            .copied()
            .collect()
    };

    let region_len = page_count * 4096;
    let base = {
        let mut next = NEXT_MMAP_PAGE.lock();
        let base = *next;
        let end = next
            .checked_add(region_len)
            .filter(|&e| e <= MMAP_REGION_CEILING)
            .ok_or(ENOMEM)?;
        *next = end;
        base
    };

    let writable = prot & PROT_WRITE != 0;
    // SHARED_LEAF: these frames are owned by MMAP_FILE_CACHE (keyed by content_id, real cross-open
    // sharing -- see that cache's own doc comment), not by this one mapping -- marks every leaf so
    // a real address-space teardown (`memory::address_space::AddressSpace::teardown`) never frees
    // one still cached/still live in another process's or another mapping's own page table. See
    // `SHARED_LEAF`'s own doc comment.
    let mut flags = PageTableFlags::PRESENT
        | PageTableFlags::USER_ACCESSIBLE
        | memory::address_space::SHARED_LEAF;
    if writable {
        flags |= PageTableFlags::WRITABLE;
    }

    let mut table = PROCESS_TABLE.lock();
    let me = table
        .get_mut(&caller_pid)
        .expect("mmap: current process missing from table");
    // SAFETY: see do_mmap's identical reasoning -- me.address_space is the currently active
    // address space.
    let mut mapper = unsafe { me.address_space.mapper(phys_offset) };
    // Only `[base, base + covered_pages*4096)` is actually mapped -- `[covered_pages, page_count)`
    // is deliberately left with no page-table entry at all, real POSIX MPR (see MmapFileRegion's
    // own `mapped_pages` field doc comment); `covered_pages == 0` (the whole reservation is beyond
    // the object's own real content) skips this loop entirely.
    if covered_pages > 0 {
        let start_page = Page::<Size4KiB>::containing_address(VirtAddr::new(base));
        let end_page =
            Page::<Size4KiB>::containing_address(VirtAddr::new(base + covered_pages * 4096 - 1));
        with_frame_allocator(|fa| -> Result<(), u64> {
            for (page, frame) in Page::range_inclusive(start_page, end_page).zip(frames.iter()) {
                // SAFETY: `frame` is one of MMAP_FILE_CACHE's own real backing frames (never reused
                // for anything else -- this kernel has no frame-deallocation path), and `page` falls
                // in this process's own, freshly bump-allocated mmap region.
                unsafe {
                    mapper
                        .map_to(page, *frame, flags, fa)
                        .map_err(|_| ENOMEM)?
                        .flush();
                }
            }
            Ok(())
        })?;
    }

    *MMAP_FILE_REFCOUNT.lock().entry(content_id).or_insert(0) += 1;
    me.mmap_file_regions.push(MmapFileRegion {
        va_start: base,
        npages: page_count,
        mapped_pages: covered_pages,
        content_id,
        writable,
    });

    Ok(base)
}

/// Real POSIX MPR (`mmap/11-2.c`/`11-3.c`): resolves what signal a ring-3 reference to `addr`
/// should raise, for `interrupts::page_fault_handler`'s own real-fault-to-signal delivery.
/// `SIGBUS` when `addr` falls inside a live fd-backed mapping's own reserved-but-unbacked tail
/// (`[mapped_pages, npages)` of one of `caller_pid`'s own `mmap_file_regions` -- see
/// `MmapFileRegion::mapped_pages`'s own doc comment), `SIGSEGV` for every other unmapped ring-3
/// reference (a real general fix in its own right -- this kernel used to reboot on any such
/// reference at all, ring-3 or not, rather than terminate just the one offending process).
pub fn signal_for_user_fault(caller_pid: Pid, addr: u64) -> u64 {
    let table = PROCESS_TABLE.lock();
    let Some(me) = table.get(&caller_pid) else {
        return crate::process::SIGSEGV;
    };
    let in_reserved_tail = me.mmap_file_regions.iter().any(|r| {
        let backed_end = r.va_start + r.mapped_pages * 4096;
        let reserved_end = r.va_start + r.npages * 4096;
        addr >= backed_end && addr < reserved_end
    });
    if in_reserved_tail {
        crate::process::SIGBUS
    } else {
        crate::process::SIGSEGV
    }
}

/// Writes one region's current frame content back to its real underlying file via
/// `crate::fs::fd::content_write`, keyed by `content_id` alone — real POSIX `MAP_SHARED`
/// correctness (a plain `read()` of the file after `munmap`, through a *fresh* `open()`, should
/// see what was written through the mapping), on top of what `MMAP_FILE_CACHE`'s own frame-sharing
/// already gives other *live* mappings for free. A no-op for a read-only mapping. **Clamped to the
/// real object's own current length, queried fresh (not cached from population time)** — real
/// POSIX: "the system shall never write out any modified portions of the last page of an object
/// which are beyond its end" (found live: `mmap/11-4.c`/`11-5.c` write a byte into the partial
/// page past a half-page-sized object's own end and require it to *never* persist — an earlier
/// version of this function wrote back the whole, page-rounded region unconditionally, silently
/// failing exactly this check). **Known, accepted simplification**: if the real object has grown
/// past what *this* region's own `npages` covers (a different fd resized it after this mapping was
/// created), writeback only touches the first `npages * 4096` bytes and leaves the rest of the
/// file's real content untouched by skipping entirely rather than risking a destructive partial
/// overwrite — no real caller in this kernel's own call graph hits this. Idempotent (safe to call
/// more than once, or on more than one still-live mapping of the same file) — oxfs's own write
/// model always replaces a file's whole content in one shot (see `modules/oxfs`'s `OpenFile::Write`
/// doc comment), so a redundant write-back just re-commits the same bytes.
fn writeback_region(region: &MmapFileRegion, phys_offset: VirtAddr) {
    if !region.writable {
        return;
    }
    let current_size = crate::fs::fd::content_size(region.content_id).max(0) as u64;
    let region_len = region.npages * 4096;
    if current_size > region_len {
        // See this function's own doc comment -- a partial-coverage writeback here would
        // truncate/destroy real content past what this one mapping ever saw.
        return;
    }
    let write_len = current_size;
    if write_len == 0 {
        return;
    }
    // Stages a contiguous buffer from MMAP_FILE_CACHE's own frame list (indexed `0..npages`, not
    // re-derived from `va_start`) -- this runs from `do_munmap`/exit cleanup, potentially after
    // the page is already unmapped from the caller's own table, so the content has to come from
    // the frame itself, not a fresh page-table walk; frames aren't guaranteed physically
    // contiguous, so `content_write` needs one assembled buffer, not a per-frame call (which would
    // also incorrectly re-replace the whole object's content on every single page).
    let mut staging = alloc::vec![0u8; write_len as usize];
    {
        let cache = MMAP_FILE_CACHE.lock();
        let Some(frames) = cache.get(&region.content_id) else {
            return;
        };
        for (i, chunk) in staging.chunks_mut(4096).enumerate() {
            let Some(frame) = frames.get(i) else { break };
            let frame_ptr = (phys_offset + frame.start_address().as_u64()).as_ptr::<u8>();
            unsafe { core::ptr::copy_nonoverlapping(frame_ptr, chunk.as_mut_ptr(), chunk.len()) };
        }
    }
    crate::fs::fd::content_write(region.content_id, staging.as_ptr() as u64, write_len);
}

/// `SYS_MUNMAP`'s real logic. Unmaps every page actually present in `[addr, addr + len)` from the
/// caller's own table (sound without a `FrameDeallocator` -- only *freeing* the backing frames
/// would need one, and this kernel has no frame-deallocation path anywhere; frames leak, same
/// policy as `MMAP_REGION_BASE`/`module::NEXT_MODULE_PAGE`). Doesn't error on a page that was
/// never mapped (matches real `munmap(2)`, which only fails for a misaligned `addr`, never for
/// "nothing was mapped there"). Separately, if `addr` exactly matches a live fd-backed region's own
/// `va_start` (the shape every real caller here always uses -- `munmap` exactly what `mmap`
/// returned), writes its content back and releases the extra reference `do_mmap_file_backed` took
/// out (see `crate::fs::fd::capture_for_mmap`'s own doc comment for why that reference exists at
/// all) -- real POSIX: the file itself is never actually removed by this kernel (no deallocation
/// anywhere), so "removed when there are no more mappings" is honestly satisfied by "no longer
/// artificially kept open past the caller's own `close()`".
pub fn do_munmap(caller_pid: Pid, addr: u64, len: u64) -> Result<u64, u64> {
    if len == 0 {
        return Err(EINVAL);
    }
    let page_count = len.div_ceil(4096);
    // Real, checked arithmetic, not `page_count * 4096` / `addr + region_len - 1` -- found live by
    // the POSIX conformance pilot's own `mmap/31-1.c`: it deliberately calls `munmap((void*)-1,
    // ULONG_MAX)` (an *unconditional* cleanup call after an expected-to-fail `mmap()`, not even
    // gated on that mmap having actually succeeded), and the old unchecked arithmetic panicked
    // ("attempt to add with overflow") on exactly that input, rebooting the whole VM instead of a
    // clean per-syscall error return -- a real hardening gap independent of the pilot itself, since
    // any real userland program passing similarly extreme values would hit the same panic. `addr`
    // and the computed end address are also rejected outright unless both are canonical
    // (`VirtAddr::try_new`, not `new`, which panics the same way) -- a non-canonical `addr` is
    // never a real prior `mmap()` result, so `EINVAL` is the correct response, not a second panic.
    let Some(region_len) = page_count.checked_mul(4096) else {
        return Err(EINVAL);
    };
    let Some(end_addr_inclusive) = addr.checked_add(region_len).and_then(|e| e.checked_sub(1))
    else {
        return Err(EINVAL);
    };
    if VirtAddr::try_new(addr).is_err() || VirtAddr::try_new(end_addr_inclusive).is_err() {
        return Err(EINVAL);
    }
    let phys_offset = memory::phys_mem_offset();

    let mut table = PROCESS_TABLE.lock();
    let me = table
        .get_mut(&caller_pid)
        .expect("munmap: current process missing from table");

    let file_region_idx = me
        .mmap_file_regions
        .iter()
        .position(|r| r.va_start == addr);

    // SAFETY: me.address_space is the currently active address space -- same reasoning do_mmap's
    // own identical comment already establishes.
    let mut mapper = unsafe { me.address_space.mapper(phys_offset) };
    let start_page = Page::<Size4KiB>::containing_address(VirtAddr::new(addr));
    let end_page = Page::<Size4KiB>::containing_address(VirtAddr::new(end_addr_inclusive));

    // Real x86 hardware DIRTY-bit check, read *before* unmapping (Mapper::unmap removes the PTE
    // entirely, so this has to happen first) -- lets writeback_region below skip real work
    // entirely for a mapping that was never actually written through, not just a writable one.
    // Found live, not designed in up front: `mmap/10-1.c` (the POSIX conformance pilot)
    // mmap()+munmap()s the same never-written-to file 100,000 times in a loop -- harmless with no
    // real disk attached (every automated `cargo test` target boots that way, see CLAUDE.md's
    // disk-persistence section), but with a real persistent disk attached (an ordinary interactive
    // `cargo run`), unconditional writeback meant 100,000 real, individually-polled-PIO ATA writes
    // -- easily enough to blow past `t0`'s own 40s timeout on real hardware/QEMU-TCG timing, even
    // though not one byte was ever actually modified.
    let any_dirty = file_region_idx.is_some()
        && Page::range_inclusive(start_page, end_page).any(|page| {
            matches!(
                mapper.translate(page.start_address()),
                TranslateResult::Mapped { flags, .. } if flags.contains(PageTableFlags::DIRTY)
            )
        });

    for page in Page::range_inclusive(start_page, end_page) {
        // Real hardening, not just tidiness: a kernel-only (non-USER_ACCESSIBLE) mapping is always
        // a *shared*, kernel-half PML4/PDPT/PD entry aliased into every address space by reference
        // (see `memory::address_space::copy_table_level`'s own doc comment) -- `Mapper::unmap`
        // would still happily remove such an entry if asked, corrupting that shared mapping for
        // every other process, not just this one. `addr`/`len` are entirely caller-controlled with
        // no prior validation that they ever came from a real `mmap()` return value, so this check
        // is the only thing standing between a wild `munmap()` call and exactly that.
        let is_user_page = matches!(
            mapper.translate(page.start_address()),
            TranslateResult::Mapped { flags, .. } if flags.contains(PageTableFlags::USER_ACCESSIBLE)
        );
        if !is_user_page {
            continue;
        }
        if let Ok((_, flush)) = mapper.unmap(page) {
            flush.flush();
        }
    }

    if let Some(idx) = file_region_idx {
        let region = me.mmap_file_regions.remove(idx);
        drop(table);
        if any_dirty {
            writeback_region(&region, phys_offset);
        }
        release_mmap_file_ref(region.content_id);
    }

    Ok(0)
}

/// Real cleanup for every fd-backed mapping a process still had live at exit/`execve` — writes
/// back and releases each one's `MMAP_FILE_CACHE` reference, the same way an explicit `munmap`
/// would, since neither event ever calls `do_munmap` itself. Called from
/// `process::lifecycle::terminate_process` (before `PROCESS_TABLE` is locked, same placement
/// `crate::fs::sysv_shm::detach_all_for_exit`'s own call site already establishes) and from
/// `do_execve` (after the new `AddressSpace` is committed -- the old one, and every page mapped
/// into it, just became unreachable, matching real Linux's own implicit-unmap-on-execve).
pub fn cleanup_mmap_file_regions_for_exit(pid: Pid) {
    let regions = {
        let mut table = PROCESS_TABLE.lock();
        let Some(me) = table.get_mut(&pid) else {
            return;
        };
        core::mem::take(&mut me.mmap_file_regions)
    };
    if regions.is_empty() {
        return;
    }
    let phys_offset = memory::phys_mem_offset();
    for region in &regions {
        writeback_region(region, phys_offset);
        release_mmap_file_ref(region.content_id);
    }
}

/// `SYS_MPROTECT`'s real logic — a permissive no-op success stub, exactly like `do_munmap` above.
/// This kernel doesn't enforce page protection anywhere yet (`do_mmap` already ignores its own
/// `prot` argument and unconditionally grants `WRITABLE`; `NO_EXECUTE`/`EFER.NXE` isn't plumbed at
/// all) — so a caller that successfully changed nothing is an honest, not a regressive, answer,
/// same tier as `getrusage`'s all-zero-but-correctly-shaped struct. Registered mainly so a real
/// dynamic linker's own RELRO-protection step (`mprotect`ing its relocated `PT_GNU_RELRO` segment
/// read-only after relocation) doesn't surface as an unrecognized-syscall `ENOSYS` — real
/// enforcement is future work, once something actually depends on W^X being real.
pub fn do_mprotect(addr: u64, len: u64, prot: u64) -> Result<u64, u64> {
    let _ = (addr, len, prot);
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
