use spin::Lazy;
use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{CS, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;

use crate::serial_println;

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

const STACK_SIZE: usize = 4096 * 5;

/// Index into `TaskStateSegment::privilege_stack_table` for ring 0 (RSP0): the stack the CPU
/// switches to automatically on any interrupt/exception that fires while running in ring 3.
/// Without this set, such a transition uses `RSP0 = 0` — a crash the instant any user-mode code
/// takes an interrupt.
const KERNEL_STACK_INDEX: usize = 0;

static TSS: Lazy<TaskStateSegment> = Lazy::new(|| {
    let mut tss = TaskStateSegment::new();
    tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = {
        // `static mut` (not `static`) is load-bearing here: the only pointer ever taken to this
        // is a *const one, so as far as the Rust/LLVM optimizer can see, this data is never
        // written and is free to be interned into read-only .rodata. It's actually written by
        // the CPU pushing an interrupt frame onto it in hardware, invisible to that analysis —
        // `static mut` (mutable-by-definition) forces genuinely writable .bss placement instead.
        static mut STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];

        let stack_start = VirtAddr::from_ptr(&raw mut STACK);
        stack_start + STACK_SIZE as u64
    };
    tss.privilege_stack_table[KERNEL_STACK_INDEX] = {
        // See the comment on the IST stack above — same reasoning applies here.
        static mut STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];

        let stack_start = VirtAddr::from_ptr(&raw mut STACK);
        stack_start + STACK_SIZE as u64
    };
    tss
});

struct Selectors {
    kernel_code_selector: SegmentSelector,
    kernel_data_selector: SegmentSelector,
    user_data_selector: SegmentSelector,
    user_code_selector: SegmentSelector,
    tss_selector: SegmentSelector,
}

static GDT: Lazy<(GlobalDescriptorTable, Selectors)> = Lazy::new(|| {
    let mut gdt = GlobalDescriptorTable::new();
    // Order matters: SYSCALL/SYSRETQ (see src/linux_syscall.rs) reconstruct target segment
    // selectors from IA32_STAR using fixed offsets from two base values, which only works if
    // kernel_code/kernel_data/[placeholder]/user_data/user_code stay in exactly this order and
    // adjacency. `Descriptor::user_*_segment` are already tagged Ring 3 (DPL 3), and
    // `GlobalDescriptorTable::append` bakes the descriptor's DPL into the returned selector's RPL
    // bits.
    let kernel_code_selector = gdt.append(Descriptor::kernel_code_segment());
    let kernel_data_selector = gdt.append(Descriptor::kernel_data_segment());
    // Reserves the GDT slot SYSRETQ's fixed offset scheme needs 8 bytes before
    // user_data_selector — historically a 32-bit-compat user code segment, but since this kernel
    // only ever uses SYSRETQ (64-bit), that slot's *contents* are never actually loaded by
    // hardware. Its selector value is never used past this point; only its position matters.
    let _syscall_compat_placeholder = gdt.append(Descriptor::user_code_segment());
    let user_data_selector = gdt.append(Descriptor::user_data_segment());
    let user_code_selector = gdt.append(Descriptor::user_code_segment());
    let tss_selector = gdt.append(Descriptor::tss_segment(&TSS));
    (
        gdt,
        Selectors {
            kernel_code_selector,
            kernel_data_selector,
            user_data_selector,
            user_code_selector,
            tss_selector,
        },
    )
});

pub fn init() {
    serial_println!(
        "[boot] loading GDT/TSS (double-fault stack at IST[{}], ring-0 stack at RSP0, {} bytes \
         each)",
        DOUBLE_FAULT_IST_INDEX,
        STACK_SIZE
    );
    GDT.0.load();
    unsafe {
        CS::set_reg(GDT.1.kernel_code_selector);
        SS::set_reg(GDT.1.kernel_data_selector);
        load_tss(GDT.1.tss_selector);
    }
    serial_println!("[boot] GDT/TSS loaded");
}

/// The ring 3 code segment selector, for building an `iretq` frame into user mode.
pub fn user_code_selector() -> SegmentSelector {
    GDT.1.user_code_selector
}

/// The ring 3 data segment selector, for building an `iretq` frame into user mode.
pub fn user_data_selector() -> SegmentSelector {
    GDT.1.user_data_selector
}

/// The ring 0 code segment selector — needed by `src/linux_syscall.rs` to program `IA32_STAR`.
pub fn kernel_code_selector() -> SegmentSelector {
    GDT.1.kernel_code_selector
}

/// The ring 0 data segment selector — needed by `src/linux_syscall.rs` to program `IA32_STAR`.
pub fn kernel_data_selector() -> SegmentSelector {
    GDT.1.kernel_data_selector
}
