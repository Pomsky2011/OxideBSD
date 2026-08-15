//! mmap/munmap/mprotect/brk syscalls -- split out of the original process.rs.


use spin::Mutex;
use x86_64::VirtAddr;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, Page, PageTableFlags, Size4KiB,
};

use crate::memory::{self, with_frame_allocator};
use crate::syscall::{EINVAL, ENOMEM};
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
