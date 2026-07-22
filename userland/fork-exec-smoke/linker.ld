/* Forces a load base clear of the kernel's own image (~0x200000-0x237000), the kernel heap
 * (0x444444440000), the physical-memory-offset window (~0x18000000000), the bootloader's own
 * ~6 MiB identity-mapped low-memory region (see ring3-smoke/linker.ld's comment), and every other
 * userland crate's own fixed base (ring3-smoke 0x800000, stsh 0x600000) -- see
 * src/address_space.rs for why those regions matter: a new address space starts
 * as a shallow copy of the kernel's own page tables, so this binary's segments must not collide
 * with them. */
ENTRY(_start)

SECTIONS
{
    . = 0x700000;

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
