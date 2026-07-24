/* A distinct load base from userland/ring3-smoke/ (0x4080000) and userland/fork-exec-smoke/
 * (0x4040000) -- not currently required since only one userland demo is ever loaded at a time, but
 * keeps them from colliding if that changes.
 *
 * 0x4000000 (64 MiB), not the original 0x600000 (6 MiB): the kernel's own image was ~0x237000
 * (~2.2 MiB) when 0x600000 was first chosen (see ring3-smoke/fork-exec-smoke's own linker.ld
 * comments) but crossed ~0x162c530 (~22.7 MiB) once `modules/oxfs` started embedding every
 * BusyBox applet's own ELF bytes (~300 of them -- see CLAUDE.md's BusyBox section) via
 * `include_bytes!`, which itself gets embedded into the *kernel's own* binary the same way --
 * silently swallowing 0x600000 along with several BusyBox applet load addresses too (`hush`'s
 * 0xe00000 among them, the first place this actually surfaced: `Elf(MappingFailed)` spawning pid
 * 1). 64 MiB leaves close to 3x headroom over the kernel's current size, not just barely enough --
 * this exact fragility has already bitten once from "barely enough". Re-derive this by hand
 * (`readelf -l target/x86_64-oxidebsd/debug/oxidebsd | grep -A1 LOAD`, highest `VirtAddr + MemSiz`)
 * before adding anything with a fixed load address below this floor. */
ENTRY(_start)

SECTIONS
{
    . = 0x4000000;

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
