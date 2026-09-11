/* Kernel linker script for the Limine boot protocol (see CLAUDE.md's boot section). Wired in via
 * `.cargo/config.toml`'s `[target.x86_64-oxidebsd] rustflags` (`-C link-arg=-T...`), applying
 * uniformly to the main kernel binary and every `tests/*.rs` binary, since they all share that
 * one target+rustflags block.
 *
 * There was no explicit kernel linker script before this (the retired `bootimage` tool handled
 * final linking implicitly) -- the exact section list below (`.got`/`.data.rel.ro`/
 * `.relro_padding` alongside plain `.text`/`.rodata`/`.data`/`.bss`) was re-derived by running
 * `readelf -S` against the pre-migration kernel binary rather than assumed from a reference
 * template: rust-lld's default (non-PIE, non-relocation-model=static) link for this crate already
 * emits real `.got`/RELRO-shaped sections even though nothing here ever runs a dynamic linker
 * against them (every GOT entry is fully resolved at static-link time) -- folded into the same
 * `:data` segment as `.data` itself, same treatment the Limine reference template gives `.got`/
 * `.got.plt`, rather than left for the linker's own orphan-section placement to decide.
 */
OUTPUT_FORMAT(elf64-x86-64)

ENTRY(kmain)

PHDRS
{
    text   PT_LOAD FLAGS(5); /* R+X */
    rodata PT_LOAD FLAGS(4); /* R   */
    data   PT_LOAD FLAGS(6); /* R+W */
}

SECTIONS
{
    /* Topmost 2 GiB, as the Limine protocol mandates for a higher-half x86_64 kernel. */
    . = 0xffffffff80000000;

    .text : {
        *(.text .text.*)
    } :text

    . = ALIGN(CONSTANT(MAXPAGESIZE));

    .rodata : {
        *(.rodata .rodata.*)
    } :rodata

    . = ALIGN(CONSTANT(MAXPAGESIZE));

    .data : {
        *(.data .data.*)
        *(.data.rel.ro .data.rel.ro.*)
        *(.got)
        *(.relro_padding)

        /* Limine requests -- see src/boot.rs. Must be `KEEP()`'d explicitly: nothing in this
         * kernel's own code ever references these statics by name after startup, so an
         * unadorned output-section rule would let the linker consider them unreferenced and
         * drop them, and Limine can only find them by scanning this section's raw bytes. */
        KEEP(*(.requests_start))
        KEEP(*(.requests))
        KEEP(*(.requests_end))
    } :data

    /* .bss last, and mapped to :data rather than getting its own PHDR entry, so the file on disk
     * doesn't have to carry real zero bytes for it. */
    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
        *(.comment)
    }
}
