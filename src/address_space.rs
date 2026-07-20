use x86_64::VirtAddr;
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::structures::paging::{FrameAllocator, OffsetPageTable, PhysFrame, Size4KiB};

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
