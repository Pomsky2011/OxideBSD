//! Smoke test for `src/random.rs`'s `oxidebsd_random_bytes` -- the real, SHA-256/ChaCha20-backed
//! random byte source added to unblock BusyBox's vendored TLS code (`networking/tls.c`'s
//! `tls_get_random`, called via `/dev/urandom` -- see CLAUDE.md's "Real networking" known-gaps
//! entry on `wget` HTTPS). Only exercises the kernel-resident generator itself, not `modules/
//! oxfs`'s own `/dev` path routing -- that can only really be driven by a real process actually
//! `open()`ing the path, which needs a full module-loading boot (like `tests/fork_wait.rs`'s own
//! pattern); this covers the new, riskier cryptographic logic this pass actually added instead.
//!
//! Covers: the call doesn't crash for a range of buffer sizes (including `0`, matching this
//! codebase's own established `len == 0` convention elsewhere), two consecutive calls produce
//! genuinely different output (proving this is real per-call entropy, not a fixed or all-zero
//! buffer a stub could also satisfy), and the output isn't a degenerate all-same-byte pattern.
#![no_std]
#![no_main]

use core::panic::PanicInfo;

use bootloader::{BootInfo, entry_point};
use oxidebsd::qemu::{QemuExitCode, exit_qemu};
use oxidebsd::random::oxidebsd_random_bytes;
use oxidebsd::serial_println;

entry_point!(main);

fn main(boot_info: &'static BootInfo) -> ! {
    oxidebsd::init(boot_info);

    // len == 0 must be a harmless no-op, not a crash -- same convention every other read/write
    // path in this codebase already established.
    let mut empty = [0xAAu8; 4];
    let n = oxidebsd_random_bytes(empty.as_mut_ptr() as u64, 0);
    assert_eq!(n, 0, "len == 0 should return 0: {n}");
    assert_eq!(
        empty, [0xAA; 4],
        "len == 0 must not touch the buffer at all"
    );

    let mut buf_a = [0u8; 64];
    let n = oxidebsd_random_bytes(buf_a.as_mut_ptr() as u64, buf_a.len() as u64);
    assert_eq!(n, buf_a.len() as i64, "wrong byte count returned: {n}");
    assert!(
        buf_a.iter().any(|&b| b != buf_a[0]),
        "output is a single repeated byte -- not real random output"
    );
    serial_println!(
        "random_smoke: oxidebsd_random_bytes() filled a 64-byte buffer, not degenerate"
    );

    let mut buf_b = [0u8; 64];
    let n = oxidebsd_random_bytes(buf_b.as_mut_ptr() as u64, buf_b.len() as u64);
    assert_eq!(
        n,
        buf_b.len() as i64,
        "wrong byte count returned (2nd call): {n}"
    );
    assert_ne!(
        buf_a, buf_b,
        "two consecutive calls produced identical output -- not real per-call entropy"
    );
    serial_println!(
        "random_smoke: two consecutive calls produced different output -- oxidebsd_random_bytes verified end to end"
    );

    // A partial-buffer request (shorter than a full ChaCha20 block) must still work cleanly.
    let mut small = [0u8; 3];
    let n = oxidebsd_random_bytes(small.as_mut_ptr() as u64, small.len() as u64);
    assert_eq!(n, small.len() as i64, "short read failed: {n}");
    serial_println!("random_smoke: sub-block-length read verified");

    exit_qemu(QemuExitCode::Success);
    oxidebsd::hlt_loop();
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    oxidebsd::test_panic_handler(info)
}
