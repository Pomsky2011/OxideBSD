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

/// Floor: the original fixed heap size this kernel always used, raised from 100 KiB once
/// `src/process.rs`'s `KernelStack` started allocating each process's own kernel stack from this
/// same heap (several are live at once under `fork`), on top of the process table itself and
/// whatever `execve`'s internal `Vec<u8>` needs to hold a loaded ELF's bytes. Proven sufficient for
/// today's workload -- never shrink below this just because a boot reports little RAM.
const HEAP_SIZE_FLOOR: usize = 4 * 1024 * 1024; // 4 MiB
/// Ceiling: bounds one-time boot cost (mapping + zeroing heap pages) on a RAM-rich machine.
/// Nothing in this kernel today needs anywhere near this much heap; it exists purely so
/// `compute_heap_size` doesn't hand back an unreasonably large region on a multi-GiB host.
const HEAP_SIZE_CEILING: usize = 128 * 1024 * 1024; // 128 MiB
/// What fraction of total usable RAM the heap gets, before clamping to the floor/ceiling above.
const HEAP_SIZE_DIVISOR: u64 = 8; // 1/8th of usable RAM

/// Picks a heap size scaled to `usable_ram_bytes` (as reported by
/// `memory::usable_ram_bytes`, itself populated by `memory::BootInfoFrameAllocator::init`),
/// clamped to `[HEAP_SIZE_FLOOR, HEAP_SIZE_CEILING]`. Called once, at boot, before `init_heap`.
pub fn compute_heap_size(usable_ram_bytes: u64) -> usize {
    let scaled = (usable_ram_bytes / HEAP_SIZE_DIVISOR) as usize;
    scaled.clamp(HEAP_SIZE_FLOOR, HEAP_SIZE_CEILING)
}

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
    heap_size: usize,
) -> Result<(), MapToError<Size4KiB>> {
    serial_println!(
        "[boot] mapping heap: {:#x}..{:#x} ({} KiB)",
        HEAP_START,
        HEAP_START + heap_size,
        heap_size / 1024
    );

    let page_range = {
        let heap_start = VirtAddr::new(HEAP_START as u64);
        let heap_end = heap_start + (heap_size - 1) as u64;
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
            .init(HEAP_START as *mut u8, heap_size);
    }
    serial_println!("[boot] heap ready");

    Ok(())
}
