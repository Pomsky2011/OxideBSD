/* Load base 0xc600000 -- next in the stepped-by-0x100000 sequence the real-SYSCALL smoke tests
 * established (... sysinfo-syscall-smoke 0xc400000, sigaltstack-syscall-smoke 0xc500000 -- see
 * userland/socketpair-syscall-smoke/linker.ld's own comment for the full "why this address"
 * story). Re-derive via `readelf -l target/x86_64-oxidebsd/debug/oxidebsd | grep -A1 LOAD` before
 * trusting this if the kernel image has grown meaningfully since this was picked.
 *
 * The explicit `ALIGN(0x1000)` before `.got`/`.data`/`.bss` below is load-bearing, not cosmetic --
 * same fix `userland/sa-siginfo-syscall-smoke/linker.ld` already documents in full: this crate has
 * a real writable global (`HANDLER_RAN: AtomicBool`, set from inside the SIGUSR1 handler under
 * test), and without forcing it onto its own page, this tiny binary's RW `.got`/`.data`/`.bss`
 * `PT_LOAD` segment shares a physical page with the RX `.text`/`.rodata` segment --
 * `src/process/elf.rs`'s loader doesn't union permission flags across `PT_LOAD` segments sharing
 * one page (CLAUDE.md's own documented "known simplification"), so the shared page silently kept
 * only the first segment's R/E flags. Found live exactly the same way: the first static write
 * (`HANDLER_RAN.store(...)` from inside the child's own signal path) page-faulted
 * (`PROTECTION_VIOLATION | CAUSED_BY_WRITE | USER_MODE`) before this crate's real scenario ever
 * ran. */
ENTRY(_start)

SECTIONS
{
    . = 0xc600000;

    .text : {
        *(.text*)
    }

    .rodata : {
        *(.rodata*)
    }

    . = ALIGN(0x1000);

    .got : {
        *(.got*)
    }

    .data : {
        *(.data*)
    }

    .bss : {
        *(.bss*)
    }

    /DISCARD/ : {
        *(.comment)
        *(.eh_frame*)
        *(.note*)
    }
}
