/* Load base 0xbd00000 -- next in the stepped-by-0x100000 sequence the real-SYSCALL smoke tests
 * established (... needs-syscall-smoke 0xbb00000, needs-syscall2-smoke 0xbc00000 -- see
 * userland/socketpair-syscall-smoke/linker.ld's own comment for the full "why this address"
 * story). Re-derive via `readelf -l target/x86_64-oxidebsd/debug/oxidebsd | grep -A1 LOAD` before
 * trusting this if the kernel image has grown meaningfully since this was picked. Distinct from
 * `tcc`'s own load base (0xa280000, see build.rs's `build_tinycc`) and `hello.elf`'s (compiled by
 * `tcc` itself at test run time, wherever `tcc`'s own default link base lands it -- not this
 * crate's concern, since it runs in its own separate address space once `execve`'d). */
ENTRY(_start)

SECTIONS
{
    . = 0xbd00000;

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
