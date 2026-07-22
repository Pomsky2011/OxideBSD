use x86_64::VirtAddr;
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::structures::paging::{
    FrameAllocator, OffsetPageTable, PageTable, PageTableFlags, PhysFrame, Size4KiB,
};

use crate::memory::frame_to_page_table;

/// A separate top-level page table — a distinct virtual address space from the kernel's own.
///
/// New address spaces start as a **shallow** copy of the kernel's own level 4 table: the copy is
/// just 512 raw entries (pointers to lower-level tables), so it shares every one of the kernel's
/// existing mappings (code, heap, stacks, the physical-memory-offset window) with the original,
/// rather than duplicating them. That's deliberate — the kernel must stay identically mapped and
/// reachable no matter which address space is active, since interrupt/exception handlers run in
/// the *kernel's* context regardless of what was running when they fired. Only entries this
/// address space adds later (e.g. a loaded ELF's segments) are actually new and private to it.
pub struct AddressSpace {
    level_4_frame: PhysFrame,
}

impl AddressSpace {
    /// Creates a new address space seeded with the kernel's current mappings.
    ///
    /// **Only safe to call when the currently active table's low half (user-space entries) is
    /// already empty** -- this clones *all* 512 entries unconditionally, not just the kernel
    /// half, so if the active table already has real user mappings (i.e. this is called from
    /// inside an already-running process, not at boot against the kernel's own address space),
    /// the result silently *aliases* those mappings into the new table instead of leaving room
    /// for fresh ones. True for `process::spawn` (only ever called while the *kernel's own*
    /// address space -- no user mappings at all -- is active) but **not** for `AddressSpace::fork`
    /// or `process::do_execve`'s replacement address space, both of which run with the calling
    /// process's own, already-populated address space active and need `fork`/
    /// `new_excluding_user` below instead.
    pub fn new(
        physical_memory_offset: VirtAddr,
        frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    ) -> Self {
        let new_frame = frame_allocator
            .allocate_frame()
            .expect("out of memory allocating a new address space's level 4 table");
        let (active_frame, _flags) = Cr3::read();

        // SAFETY: physical_memory_offset is the bootloader's phys-memory mapping (same
        // requirement as memory::init); active_frame is CR3's own live level 4 table, read fresh
        // above, and new_frame was just allocated so nothing else can be viewing it yet.
        let (active_table, new_table) = unsafe {
            (
                frame_to_page_table(active_frame, physical_memory_offset),
                frame_to_page_table(new_frame, physical_memory_offset),
            )
        };
        *new_table = active_table.clone();

        AddressSpace {
            level_4_frame: new_frame,
        }
    }

    /// Builds a fresh address space that shares every one of the currently active table's
    /// kernel-only mappings but excludes its user-accessible ones entirely (used by
    /// `process::do_execve`, which needs a totally clean user address space, not the calling
    /// process's inherited one). Thin wrapper over the same recursive walk `fork` uses, just
    /// without copying user leaves at all -- see `copy_table_level`'s own doc comment for why a
    /// naive "clone everything, then zero out low addresses" approach (an earlier, broken version
    /// of this code) doesn't work: this kernel has no clean higher-half split. Kernel code, the
    /// heap, the phys-mem-offset window, and every user ELF's load address all coexist in the low
    /// canonical range at different indices -- the only thing that reliably distinguishes "safe to
    /// share" from "must be fresh" at *any* level is the `USER_ACCESSIBLE` flag itself, which the
    /// MMU's own hierarchical walk requires to be set at *every* level down to a user page (so a
    /// clear `USER_ACCESSIBLE` bit anywhere guarantees nothing user-facing exists beneath it, safe
    /// to alias as-is).
    pub(crate) fn new_excluding_user(
        physical_memory_offset: VirtAddr,
        frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    ) -> Self {
        Self::build_from_active(physical_memory_offset, frame_allocator, false)
    }

    /// Builds a **deep** copy of the currently active address space's user-space mappings into a
    /// fresh child -- the `fork()` half of `fork()`/`execve()`/`wait()` (`src/process.rs`'s
    /// `do_fork_from_current`). Kernel-only subtrees are shared (aliased) with the parent, exactly
    /// like `new_excluding_user`; every subtree that leads to at least one user-accessible leaf
    /// page is instead freshly allocated and recursed into, and each user leaf itself is copied
    /// byte-for-byte (via the phys-mem-offset window, the same technique `elf::load` already uses
    /// to write segment bytes) into a freshly allocated frame. No copy-on-write -- a full eager
    /// copy, matching this codebase's existing correctness-over-cleverness bias (see e.g.
    /// `BootInfoFrameAllocator`'s own no-reuse policy).
    ///
    /// # Panics
    ///
    /// Panics (via `expect`) on frame exhaustion or an unexpected huge page -- this codebase has
    /// no established error-propagation convention for OOM during address-space setup yet (`new`
    /// and every `elf::load` caller panic the same way today), and nothing here creates a huge
    /// page, so encountering one means a future change violated that assumption.
    ///
    /// # Safety requirement, not enforced by the type system
    ///
    /// Must only be called on the **currently active** address space (`self`'s level 4 frame must
    /// be what `CR3` currently points at) -- real `fork()` always forks the calling process, which
    /// is necessarily active mid-syscall (`do_fork_from_current` runs synchronously on the
    /// caller's own kernel stack with its own CR3 still loaded), so this holds for every call site
    /// this codebase has -- but a hypothetical future caller trying to fork some other, non-running
    /// process would silently copy the *wrong* table.
    pub fn fork(
        &self,
        physical_memory_offset: VirtAddr,
        frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    ) -> AddressSpace {
        Self::build_from_active(physical_memory_offset, frame_allocator, true)
    }

    /// Shared implementation behind `new_excluding_user`/`fork`: allocates a fresh level 4 table
    /// and recursively walks it against the currently active one via `copy_table_level`.
    fn build_from_active(
        physical_memory_offset: VirtAddr,
        frame_allocator: &mut impl FrameAllocator<Size4KiB>,
        copy_user_leaves: bool,
    ) -> AddressSpace {
        let new_frame = frame_allocator
            .allocate_frame()
            .expect("out of memory allocating a new address space's level 4 table");
        let (active_frame, _flags) = Cr3::read();

        // SAFETY: physical_memory_offset is the bootloader's phys-memory mapping (same
        // requirement as memory::init); active_frame is CR3's own live level 4 table; new_frame
        // was just allocated so nothing else can be viewing it yet.
        let (active_table, new_table) = unsafe {
            (
                frame_to_page_table(active_frame, physical_memory_offset),
                frame_to_page_table(new_frame, physical_memory_offset),
            )
        };
        new_table.zero();
        copy_table_level(
            active_table,
            new_table,
            4,
            physical_memory_offset,
            frame_allocator,
            copy_user_leaves,
        );

        AddressSpace {
            level_4_frame: new_frame,
        }
    }

    /// Builds a mapper over this address space's own level 4 table, independent of whichever
    /// address space `CR3` currently points at — lets a loader map pages into a not-yet-active
    /// address space.
    ///
    /// # Safety
    ///
    /// `physical_memory_offset` must be where the bootloader mapped all of physical memory (same
    /// requirement as `memory::init`), and there must be no other live `&mut` view of this
    /// address space's level 4 table.
    pub unsafe fn mapper(&self, physical_memory_offset: VirtAddr) -> OffsetPageTable<'static> {
        let level_4_table =
            unsafe { frame_to_page_table(self.level_4_frame, physical_memory_offset) };
        unsafe { OffsetPageTable::new(level_4_table, physical_memory_offset) }
    }

    /// Switches to this address space by writing `CR3`.
    ///
    /// # Safety
    ///
    /// Every mapping the currently-running code needs (its own instructions, stack, and anything
    /// an interrupt handler might touch) must already be present in this address space — true by
    /// construction for a freshly-`new`'d one, but code that later removes kernel entries from a
    /// process's address space must not activate it from a context that depends on what it removed.
    pub unsafe fn activate(&self) {
        unsafe { Cr3::write(self.level_4_frame, Cr3Flags::empty()) };
    }
}

/// Recursively walks `parent`'s `level` (`4` = PML4, `3` = PDPT, `2` = PD, `1` = PT -- the leaf
/// level, where entries point at actual 4 KiB data frames rather than further tables) alongside
/// `child` (already zeroed by the caller), populating `child` entry-by-entry:
///
/// - A present entry that is **not** `USER_ACCESSIBLE` is guaranteed, by the MMU's own
///   hierarchical walk requirements, to lead to nothing but kernel-only content -- safe to alias
///   directly (`child[i] = parent[i].clone()`) without recursing any further.
/// - A present, `USER_ACCESSIBLE` entry at the **leaf** level (`level == 1`) is an actual user
///   page. If `copy_user_leaves` is `false` (building an `execve` target's fresh address space),
///   it's simply skipped, leaving `child`'s slot unused. If `true` (forking), a fresh frame is
///   allocated and the parent's bytes are copied into it (via the phys-mem-offset window), and
///   `child[i]` is pointed at the copy with the same flags.
/// - A present, `USER_ACCESSIBLE` entry above the leaf level leads to *at least one* user page
///   somewhere beneath it (possibly alongside purely-kernel siblings under the same entry -- e.g.
///   this kernel's own PML4 index 0 hosts both its own code and every userland ELF's load
///   address, since it has no higher-half split). It can't be aliased *or* skipped outright:
///   `child` gets its own fresh, zeroed next-level table, and this function recurses into it.
fn copy_table_level(
    parent: &PageTable,
    child: &mut PageTable,
    level: u8,
    physical_memory_offset: VirtAddr,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    copy_user_leaves: bool,
) {
    for i in 0..512usize {
        let entry = &parent[i];
        if !entry.flags().contains(PageTableFlags::PRESENT) {
            continue;
        }
        if !entry.flags().contains(PageTableFlags::USER_ACCESSIBLE) {
            // Kernel-only beneath this entry, guaranteed by the MMU's hierarchy rules -- share it
            // as-is, whether it's itself a leaf or points to a further table.
            child[i] = entry.clone();
            continue;
        }

        assert!(
            !entry.flags().contains(PageTableFlags::HUGE_PAGE),
            "address space copy: unexpected huge page at level {level} -- nothing in this \
             codebase creates one"
        );

        if level == 1 {
            // A real user leaf page.
            if !copy_user_leaves {
                continue;
            }
            let src_frame = entry.frame().expect("present leaf entry must have a frame");
            let new_frame = frame_allocator
                .allocate_frame()
                .expect("out of memory copying an address space");
            let src = (physical_memory_offset + src_frame.start_address().as_u64()).as_ptr::<u8>();
            let dst =
                (physical_memory_offset + new_frame.start_address().as_u64()).as_mut_ptr::<u8>();
            // SAFETY: src is a live, present physical frame (just read through the parent's own
            // page table); dst is a frame allocate_frame just handed out, unused by construction
            // -- both viewed through the same phys-mem-offset window elf::load already relies on
            // for the same kind of copy.
            unsafe { core::ptr::copy_nonoverlapping(src, dst, 4096) };
            child[i].set_frame(new_frame, entry.flags());
            continue;
        }

        // Mixed (or purely user) subtree above the leaf level: child needs its own private,
        // fresh table here, then recurse.
        let parent_next = entry
            .frame()
            .expect("present non-leaf entry must have a frame");
        let child_next_frame = frame_allocator
            .allocate_frame()
            .expect("out of memory copying an address space");
        // SAFETY: physical_memory_offset is the bootloader's phys-memory mapping; parent_next is a
        // real, live next-level table (entry is PRESENT and not a leaf at this level);
        // child_next_frame was just allocated, so nothing else can be viewing it yet.
        let (parent_next_table, child_next_table) = unsafe {
            (
                frame_to_page_table(parent_next, physical_memory_offset),
                frame_to_page_table(child_next_frame, physical_memory_offset),
            )
        };
        child_next_table.zero();
        child[i].set_frame(child_next_frame, entry.flags());
        copy_table_level(
            parent_next_table,
            child_next_table,
            level - 1,
            physical_memory_offset,
            frame_allocator,
            copy_user_leaves,
        );
    }
}
