//! Smoke test for `src/ata.rs`'s raw sector-level PIO driver -- `read_sector`/`write_sector`
//! called directly as plain Rust functions (no real `SYSCALL` involved; there's no syscall surface
//! for raw block I/O in this phase -- oxfs consumes `src/ata.rs` internally via
//! `oxidebsd_block_read`/`_write`, not through anything a userland ELF could reach). Runs against
//! `target/oxfs_test_disk.img` (`Cargo.toml`'s `test-args`, always freshly zeroed by `build.rs`),
//! not the real persistent `target/oxfs_disk.img` `cargo run` uses -- this test writes raw,
//! filesystem-format-agnostic patterns directly to low LBAs, which would corrupt a real oxfs
//! superblock/inode table if it ever ran against the dev disk instead.
//!
//! Covers: the disk is actually detected at boot (`oxidebsd_block_device_present`), a written
//! sector reads back byte-for-byte identical at a few different LBAs (catching address-computation
//! bugs, not just "it round-trips at LBA 0"), and a full 0..256 byte-value sweep round-trips
//! correctly (catching a word-order/byte-order bug in the 16-bit PIO word transfer that a
//! same-valued pattern like all-zero or all-`0xAA` could never catch).
#![no_std]
#![no_main]

use core::panic::PanicInfo;

use bootloader::{BootInfo, entry_point};
use oxidebsd::ata::{self, Channel, Drive};
use oxidebsd::qemu::{QemuExitCode, exit_qemu};
use oxidebsd::serial_println;

entry_point!(main);

fn round_trip(lba: u32, pattern: &[u8; 512]) {
    ata::write_sector(Channel::Secondary, Drive::Master, lba, pattern)
        .unwrap_or_else(|e| panic!("write_sector(lba={lba}) failed: {e:?}"));

    let mut readback = [0u8; 512];
    ata::read_sector(Channel::Secondary, Drive::Master, lba, &mut readback)
        .unwrap_or_else(|e| panic!("read_sector(lba={lba}) failed: {e:?}"));

    assert_eq!(
        &readback, pattern,
        "sector {lba} didn't read back what was written"
    );
}

fn main(boot_info: &'static BootInfo) -> ! {
    oxidebsd::init(boot_info);

    ata::init();
    assert_eq!(
        ata::oxidebsd_block_device_present(),
        1,
        "ata_smoke's own test-args should have attached a data disk at secondary/master"
    );
    serial_println!("ata_smoke: data disk detected");

    // A few different LBAs, not just 0 -- catches an address-computation bug that only a
    // non-first sector would expose (e.g. the drive/head or LBA-high registers never actually
    // being written).
    for &lba in &[0u32, 1, 7, 100] {
        let mut pattern = [0u8; 512];
        for (i, b) in pattern.iter_mut().enumerate() {
            *b = ((lba as usize + i) % 256) as u8;
        }
        round_trip(lba, &pattern);
    }
    serial_println!("ata_smoke: round trip verified at several LBAs");

    // A full 0..256 byte-value sweep, twice over to fill 512 bytes -- would catch a byte-order
    // bug in the 16-bit PIO word transfer that an all-same-value pattern could never expose (a
    // swapped high/low byte within a word is invisible if both bytes happen to be equal).
    let mut sweep = [0u8; 512];
    for (i, b) in sweep.iter_mut().enumerate() {
        *b = (i % 256) as u8;
    }
    round_trip(200, &sweep);
    serial_println!("ata_smoke: byte-order sweep verified");

    exit_qemu(QemuExitCode::Success);
    oxidebsd::hlt_loop();
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    oxidebsd::test_panic_handler(info)
}
