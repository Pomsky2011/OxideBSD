/* Load base 0xc800000 -- next in the stepped-by-0x100000 sequence the real-SYSCALL smoke tests
 * established (... sigaltstack-syscall-smoke 0xc500000, pause-syscall-smoke 0xc600000,
 * sigsuspend-syscall-smoke 0xc700000 -- see userland/socketpair-syscall-smoke/linker.ld's own
 * comment for the full "why this address" story). Re-derive via `readelf -l
 * target/x86_64-oxidebsd/debug/oxidebsd | grep -A1 LOAD` before trusting this if the kernel image
 * has grown meaningfully since this was picked.
 *
 * The explicit `ALIGN(0x1000)` before `.got`/`.data`/`.bss` below is load-bearing, not cosmetic --
 * same fix `userland/sa-siginfo-syscall-smoke/linker.ld`/`userland/pause-syscall-smoke/linker.ld`
 * already document in full: this crate has real writable globals (`AtomicU32` handler-run
 * counters, set from inside the SIGALRM/SIGUSR1 handlers under test), and without forcing them
 * onto their own page, this tiny binary's RW `.got`/`.data`/`.bss` `PT_LOAD` segment shares a
 * physical page with the RX `.text`/`.rodata` segment -- `src/process/elf.rs`'s loader doesn't
 * union permission flags across `PT_LOAD` segments sharing one page (CLAUDE.md's own documented
 * "known simplification"), so the shared page silently keeps only the first segment's R/E flags,
 * and the first static write page-faults. */
ENTRY(_start)

SECTIONS
{
    . = 0xc800000;

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
