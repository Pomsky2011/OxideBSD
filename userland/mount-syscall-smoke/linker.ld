/* Load base 0xb900000 -- next in the stepped-by-0x100000 sequence the real-SYSCALL smoke tests
 * established (socketpair 0xb000000, ping 0xb100000, udp 0xb200000, poll 0xb300000, tcp
 * 0xb400000, proc 0xb500000, itimer 0xb600000, uid 0xb700000, oxfs-persistence 0xb800000 -- see
 * userland/socketpair-syscall-smoke/linker.ld's own comment for the full "why this address"
 * story). Re-derive via `readelf -l target/x86_64-oxidebsd/debug/oxidebsd | grep -A1 LOAD` before
 * trusting this if the kernel image has grown meaningfully since this was picked. */
ENTRY(_start)

SECTIONS
{
    . = 0xb900000;

    .text : {
        *(.text*)
    }

    .rodata : {
        *(.rodata*)
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
