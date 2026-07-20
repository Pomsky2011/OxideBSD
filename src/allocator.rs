use core::alloc::{GlobalAlloc, Layout};
use core::ptr::NonNull;

use linked_list_allocator::Heap;
use spin::Mutex;
use x86_64::VirtAddr;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, Page, PageTableFlags, Size4KiB, mapper::MapToError,
};

use crate::serial_println;

pub const HEAP_START: usize = 0x_4444_4444_0000;
pub const HEAP_SIZE: usize = 100 * 1024; // 100 KiB

/// Wraps a type behind a `spin::Mutex`, reusing the project's existing spinlock rather than
/// pulling in `linked_list_allocator`'s own `spinning_top` dependency just for `LockedHeap`.
struct Locked<A> {
    inner: Mutex<A>,
}

impl<A> Locked<A> {
    const fn new(inner: A) -> Self {
        Locked {
            inner: Mutex::new(inner),
        }
    }
}

#[global_allocator]
static ALLOCATOR: Locked<Heap> = Locked::new(Heap::empty());

unsafe impl GlobalAlloc for Locked<Heap> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.inner
            .lock()
            .allocate_first_fit(layout)
            .map_or(core::ptr::null_mut(), NonNull::as_ptr)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe {
            self.inner
                .lock()
                .deallocate(NonNull::new_unchecked(ptr), layout);
        }
    }
}

/// Maps the kernel heap's virtual page range to physical frames and hands the resulting region
/// to the global allocator.
pub fn init_heap(
    mapper: &mut impl Mapper<Size4KiB>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<(), MapToError<Size4KiB>> {
    serial_println!(
        "[boot] mapping heap: {:#x}..{:#x} ({} KiB)",
        HEAP_START,
        HEAP_START + HEAP_SIZE,
        HEAP_SIZE / 1024
    );

    let page_range = {
        let heap_start = VirtAddr::new(HEAP_START as u64);
        let heap_end = heap_start + (HEAP_SIZE - 1) as u64;
        let heap_start_page = Page::containing_address(heap_start);
        let heap_end_page = Page::containing_address(heap_end);
        Page::range_inclusive(heap_start_page, heap_end_page)
    };

    for page in page_range {
        let frame = frame_allocator
            .allocate_frame()
            .ok_or(MapToError::FrameAllocationFailed)?;
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        unsafe {
            mapper.map_to(page, frame, flags, frame_allocator)?.flush();
        }
    }

    unsafe {
        ALLOCATOR
            .inner
            .lock()
            .init(HEAP_START as *mut u8, HEAP_SIZE);
    }
    serial_println!("[boot] heap ready");

    Ok(())
}
