/* Forces a load base clear of the kernel's own image (~0x200000-0x237000), the kernel heap
 * (0x444444440000), and the physical-memory-offset window (~0x18000000000) -- see
 * src/address_space.rs for why those regions matter: a new address space starts as a shallow
 * copy of the kernel's own page tables, so this binary's segments must not collide with them.
 *
 * Also clear of the bootloader's own identity-mapped low-memory region: the `bootloader` crate
 * (v0.9) identity-maps the first ~6 MiB of physical memory (kernel-only, not USER_ACCESSIBLE) as
 * part of its own bootstrap, independent of and larger than the kernel's own image -- confirmed
 * empirically (PD entries 0-2 present at boot, entry 3+ not). Since every fresh address space
 * shallow-copies/aliases whatever's kernel-only in the currently active table (see
 * `AddressSpace::fork`/`new_excluding_user` in `src/address_space.rs`), a user ELF loaded inside
 * that identity-mapped range collides with it (`MapToError::PageAlreadyMapped`) the moment
 * anything tries to `execve` it into a freshly built address space -- this used to be
 * 0x400000, which sat squarely inside that range and only broke once `process::do_execve`
 * started actually exercising this path via `stsh`'s `execve` support. 0x800000 is clear of it
 * (confirmed the same way `stsh`'s own 0x600000 and `fork-exec-smoke`'s 0x700000 already are). */
ENTRY(_start)

SECTIONS
{
    . = 0x800000;

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
