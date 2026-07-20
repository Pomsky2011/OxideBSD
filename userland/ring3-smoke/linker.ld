/* Forces a load base clear of the kernel's own image (~0x200000-0x237000), the kernel heap
 * (0x444444440000), and the physical-memory-offset window (~0x18000000000) -- see
 * src/address_space.rs for why those regions matter: a new address space starts as a shallow
 * copy of the kernel's own page tables, so this binary's segments must not collide with them. */
ENTRY(_start)

SECTIONS
{
    . = 0x400000;

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
