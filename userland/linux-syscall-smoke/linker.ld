/* A distinct load base from userland/ring3-smoke/ (0x400000) -- not currently required since only
 * one userland demo is ever loaded at a time, but keeps the two from colliding if that changes. */
ENTRY(_start)

SECTIONS
{
    . = 0x500000;

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
