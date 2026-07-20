use bootloader::bootinfo::{MemoryMap, MemoryRegionType};
use x86_64::VirtAddr;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{FrameAllocator, OffsetPageTable, PageTable, PhysFrame, Size4KiB};

use crate::serial_println;

/// Builds a mapper over the bootloader's existing page tables.
///
/// # Safety
///
/// The complete physical memory must be mapped at `physical_memory_offset` (the bootloader does
/// this when built with the `map_physical_memory` feature), and this function must be called at
/// most once to avoid aliasing `&mut` references to the level 4 table.
pub unsafe fn init(physical_memory_offset: VirtAddr) -> OffsetPageTable<'static> {
    serial_println!(
        "[boot] mapping page tables (physical memory offset {:?})",
        physical_memory_offset
    );
    let level_4_table = unsafe { active_level_4_table(physical_memory_offset) };
    let mapper = unsafe { OffsetPageTable::new(level_4_table, physical_memory_offset) };
    serial_println!("[boot] page table mapper ready");
    mapper
}

/// # Safety
///
/// Same requirements as `init`.
unsafe fn active_level_4_table(physical_memory_offset: VirtAddr) -> &'static mut PageTable {
    let (level_4_table_frame, _flags) = Cr3::read();
    unsafe { frame_to_page_table(level_4_table_frame, physical_memory_offset) }
}

/// Views an arbitrary physical frame as a page table, through the physical-memory-offset window.
/// Used both for the currently-active level 4 table (`active_level_4_table`) and, by
/// `src/address_space.rs`, for a not-yet-active one.
///
/// # Safety
///
/// `physical_memory_offset` must be where the bootloader mapped all of physical memory (same
/// requirement as `init`), `frame` must actually contain a valid, live page table, and the
/// caller must ensure no other `&mut` view of the same frame exists concurrently.
pub unsafe fn frame_to_page_table(
    frame: PhysFrame,
    physical_memory_offset: VirtAddr,
) -> &'static mut PageTable {
    let virt = physical_memory_offset + frame.start_address().as_u64();
    let page_table_ptr: *mut PageTable = virt.as_mut_ptr();

    unsafe { &mut *page_table_ptr }
}

/// A `FrameAllocator` that hands out frames from the bootloader-reported usable regions of the
/// physical memory map, in order, never reusing a frame.
pub struct BootInfoFrameAllocator {
    memory_map: &'static MemoryMap,
    next: usize,
}

impl BootInfoFrameAllocator {
    /// # Safety
    ///
    /// The passed memory map must be valid; in particular, all frames it marks `Usable` must
    /// actually be unused.
    pub unsafe fn init(memory_map: &'static MemoryMap) -> Self {
        let usable_regions = memory_map
            .iter()
            .filter(|region| region.region_type == MemoryRegionType::Usable)
            .count();
        let usable_bytes: u64 = memory_map
            .iter()
            .filter(|region| region.region_type == MemoryRegionType::Usable)
            .map(|region| region.range.end_addr() - region.range.start_addr())
            .sum();
        serial_println!(
            "[boot] frame allocator ready: {} usable region(s), {} KiB total",
            usable_regions,
            usable_bytes / 1024
        );

        BootInfoFrameAllocator {
            memory_map,
            next: 0,
        }
    }

    fn usable_frames(&self) -> impl Iterator<Item = PhysFrame> {
        self.memory_map
            .iter()
            .filter(|region| region.region_type == MemoryRegionType::Usable)
            .map(|region| region.range.start_addr()..region.range.end_addr())
            .flat_map(|range| range.step_by(4096))
            .map(|addr| PhysFrame::containing_address(x86_64::PhysAddr::new(addr)))
    }
}

unsafe impl FrameAllocator<Size4KiB> for BootInfoFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        let frame = self.usable_frames().nth(self.next);
        self.next += 1;
        frame
    }
}
