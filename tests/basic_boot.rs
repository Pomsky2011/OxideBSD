#![no_std]
#![no_main]

use core::panic::PanicInfo;

use bootloader::{entry_point, BootInfo};
use oxidebsd::qemu::{exit_qemu, QemuExitCode};
use oxidebsd::{serial_print, serial_println};

entry_point!(main);

fn main(_boot_info: &'static BootInfo) -> ! {
    serial_print!("basic_boot::kernel_boots...\t");
    assert_eq!(1, 1);
    serial_println!("[ok]");

    exit_qemu(QemuExitCode::Success);
    oxidebsd::hlt_loop();
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    oxidebsd::test_panic_handler(info)
}
