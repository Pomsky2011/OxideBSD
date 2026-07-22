/* A distinct load base from userland/ring3-smoke/ (0x800000) and userland/fork-exec-smoke/
 * (0x700000) -- not currently required since only one userland demo is ever loaded at a time, but
 * keeps them from colliding if that changes. */
ENTRY(_start)

SECTIONS
{
    . = 0x600000;

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
