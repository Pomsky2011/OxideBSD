#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(oxidebsd::test_runner)]
#![reexport_test_harness_main = "test_main"]

use core::panic::PanicInfo;

use bootloader::{BootInfo, entry_point};
use oxidebsd::serial_println;

entry_point!(kernel_main);

fn kernel_main(boot_info: &'static BootInfo) -> ! {
    serial_println!("OxideBSD kernel booting...");

    oxidebsd::init(boot_info);

    #[cfg(test)]
    test_main();

    serial_println!("OxideBSD kernel is up, entering idle loop");

    oxidebsd::hlt_loop();
}

#[cfg(not(test))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    serial_println!("{}", info);
    oxidebsd::hlt_loop();
}

#[cfg(test)]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    oxidebsd::test_panic_handler(info)
}
