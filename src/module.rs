//! A dynamic kernel-module loader: takes a relocatable (`ET_REL`) object file -- produced by
//! `build.rs`'s `build_module_crate` from a plain `#![no_std]` `lib` crate under `modules/`,
//! already merged against the exact `core`/`alloc`/`compiler_builtins` the kernel's own
//! `-Z build-std` produced -- maps its `SHF_ALLOC` sections into the kernel's own, currently
//! active address space, applies its relocations, resolves its few remaining undefined symbols
//! against a small hand-curated kernel API, and calls its `module_init` entry point.
//!
//! This is a genuinely different job from `src/elf.rs`: that file loads a handful of `PT_LOAD`
//! segments from a statically-linked, non-relocatable `ET_EXEC` userland binary with zero
//! relocations. A relocatable object has no program headers at all (`ET_REL`'s `e_phnum` is
//! always 0) -- what it has instead is potentially hundreds of small linker sections (one per
//! function/global, before any `--gc-sections` pruning -- not attempted here, see `CLAUDE.md`),
//! a symbol table, and relocation entries that must be resolved and applied by hand. Only the
//! low-level "read an ELF64 field" helpers are shared with `elf.rs` (`crate::elf::read_u{16,32,
//! 64}`); the loading logic below is independent.
//!
//! Module code never runs in ring 3 and is mapped without `USER_ACCESSIBLE` -- it's invoked only
//! from kernel context (`module_init` at load time, and, once the native ABI becomes a module
//! itself, via syscall-registry callbacks the kernel calls into directly). See `CLAUDE.md`'s
//! module-loading section for the full design, including the empirical findings (relocation
//! types actually observed, why a build-time partial relink is necessary, the panic-entry-point
//! answer, a `static mut`-dead-store gotcha distinct from `gdt.rs`'s) that shaped this file.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use spin::Mutex;
use x86_64::VirtAddr;
use x86_64::structures::paging::{FrameAllocator, Mapper, Page, PageTableFlags, Size4KiB};

use crate::elf::{read_u16, read_u32, read_u64};
use crate::{serial_print, serial_println};

const MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const CLASS_64: u8 = 2;
const DATA_LITTLE_ENDIAN: u8 = 1;
const TYPE_REL: u16 = 1;
const MACHINE_X86_64: u16 = 62;

const SHT_SYMTAB: u32 = 2;
const SHT_RELA: u32 = 4;
const SHT_NOBITS: u32 = 8;

const SHF_ALLOC: u64 = 0x2;

const SHN_UNDEF: u16 = 0;

const R_X86_64_64: u32 = 1;
const R_X86_64_PC32: u32 = 2;
const R_X86_64_PLT32: u32 = 4;
const R_X86_64_GOTPCREL: u32 = 9;
const R_X86_64_32: u32 = 10;
const R_X86_64_32S: u32 = 11;

const EHDR_SIZE: usize = 64;
const SHDR_SIZE: usize = 64;
const SYM_SIZE: usize = 24;
const RELA_SIZE: usize = 24;

const PAGE_SIZE: u64 = 4096;

/// Fixed base of the kernel-module virtual address region, and its ceiling. Modules are mapped
/// into the kernel's own, currently active address space (not a separate one -- unlike userland
/// demos, module code always runs in kernel context), at addresses well clear of the kernel
/// image, heap (`0x_4444_4444_0000`), and userland demo load addresses (`0x400000`-`0x600000`).
///
/// The low-2 GiB ceiling is load-bearing, not arbitrary: `build.rs` compiles every module with
/// `-C relocation-model=static`, which -- confirmed empirically -- eliminates GOT-indirected
/// relocations entirely (a real GOT would need lazy-vs-eager-binding decisions and its own
/// alignment bookkeeping this loader doesn't implement), in exchange for every relocation being a
/// simple absolute or PC-relative 32-bit write. Those don't just prefer small addresses, they
/// silently corrupt if a resolved address doesn't actually fit -- `apply_relocation` below
/// validates every truncating write and errors loudly rather than trust the range implicitly.
const MODULE_VA_BASE: u64 = 0x_1000_0000;
const MODULE_REGION_CEILING: u64 = 0x_8000_0000;

static NEXT_MODULE_PAGE: Mutex<u64> = Mutex::new(MODULE_VA_BASE);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleError {
    TooShort,
    BadMagic,
    UnsupportedClass,
    UnsupportedEndianness,
    UnsupportedType,
    UnsupportedMachine,
    SectionHeaderOutOfBounds,
    MissingSymbolOrStringTable,
    OutOfMemory,
    MappingFailed,
    RegionExhausted,
    UnresolvedSymbol,
    UnsupportedRelocation(u32),
    RelocationOverflow,
    MissingEntryPoint,
}

struct Section {
    sh_type: u32,
    flags: u64,
    offset: u64,
    size: u64,
    link: u32,
    info: u32,
    addralign: u64,
}

struct Symbol {
    name_off: u32,
    shndx: u16,
    value: u64,
}

struct RelaSectionMeta {
    offset: u64,
    size: u64,
    target_section: usize,
}

struct Relocation {
    offset: u64,
    symbol: u32,
    reloc_type: u32,
    addend: i64,
}

/// A parsed view over an in-memory `ET_REL` object file's sections, symbol table, and relocation
/// sections.
struct Object<'a> {
    bytes: &'a [u8],
    sections: Vec<Section>,
    symbols: Vec<Symbol>,
    strtab: &'a [u8],
    rela_sections: Vec<RelaSectionMeta>,
}

impl<'a> Object<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self, ModuleError> {
        if bytes.len() < EHDR_SIZE {
            return Err(ModuleError::TooShort);
        }
        if bytes[0..4] != MAGIC {
            return Err(ModuleError::BadMagic);
        }
        if bytes[4] != CLASS_64 {
            return Err(ModuleError::UnsupportedClass);
        }
        if bytes[5] != DATA_LITTLE_ENDIAN {
            return Err(ModuleError::UnsupportedEndianness);
        }
        if read_u16(bytes, 16) != TYPE_REL {
            return Err(ModuleError::UnsupportedType);
        }
        if read_u16(bytes, 18) != MACHINE_X86_64 {
            return Err(ModuleError::UnsupportedMachine);
        }

        let e_shoff = read_u64(bytes, 40) as usize;
        let e_shentsize = read_u16(bytes, 58) as usize;
        let e_shnum = read_u16(bytes, 60) as usize;

        if e_shentsize < SHDR_SIZE {
            return Err(ModuleError::SectionHeaderOutOfBounds);
        }
        let shdrs_size = e_shentsize
            .checked_mul(e_shnum)
            .ok_or(ModuleError::SectionHeaderOutOfBounds)?;
        let shdrs_end = e_shoff
            .checked_add(shdrs_size)
            .ok_or(ModuleError::SectionHeaderOutOfBounds)?;
        if shdrs_end > bytes.len() {
            return Err(ModuleError::SectionHeaderOutOfBounds);
        }

        let mut sections = Vec::with_capacity(e_shnum);
        for i in 0..e_shnum {
            let off = e_shoff + i * e_shentsize;
            let raw = &bytes[off..off + SHDR_SIZE];
            sections.push(Section {
                sh_type: read_u32(raw, 4),
                flags: read_u64(raw, 8),
                offset: read_u64(raw, 24),
                size: read_u64(raw, 32),
                link: read_u32(raw, 40),
                info: read_u32(raw, 44),
                addralign: read_u64(raw, 48),
            });
        }

        // The (first) SHT_SYMTAB section names its own string table via sh_link -- no separate
        // section-header string table lookup is needed anywhere in this loader, since nothing
        // here needs a *section's* name, only symbol names (via the symtab's own strtab).
        let symtab_index = sections
            .iter()
            .position(|s| s.sh_type == SHT_SYMTAB)
            .ok_or(ModuleError::MissingSymbolOrStringTable)?;
        let symtab = &sections[symtab_index];
        let strtab_section = sections
            .get(symtab.link as usize)
            .ok_or(ModuleError::MissingSymbolOrStringTable)?;
        let strtab_start = strtab_section.offset as usize;
        let strtab_end = strtab_start
            .checked_add(strtab_section.size as usize)
            .ok_or(ModuleError::SectionHeaderOutOfBounds)?;
        let strtab = bytes
            .get(strtab_start..strtab_end)
            .ok_or(ModuleError::SectionHeaderOutOfBounds)?;

        let sym_start = symtab.offset as usize;
        let sym_end = sym_start
            .checked_add(symtab.size as usize)
            .ok_or(ModuleError::SectionHeaderOutOfBounds)?;
        let sym_bytes = bytes
            .get(sym_start..sym_end)
            .ok_or(ModuleError::SectionHeaderOutOfBounds)?;
        let sym_count = sym_bytes.len() / SYM_SIZE;
        let mut symbols = Vec::with_capacity(sym_count);
        for i in 0..sym_count {
            let raw = &sym_bytes[i * SYM_SIZE..i * SYM_SIZE + SYM_SIZE];
            symbols.push(Symbol {
                name_off: read_u32(raw, 0),
                shndx: read_u16(raw, 6),
                value: read_u64(raw, 8),
            });
        }

        let rela_sections = sections
            .iter()
            .filter(|s| s.sh_type == SHT_RELA)
            .map(|s| RelaSectionMeta {
                offset: s.offset,
                size: s.size,
                target_section: s.info as usize,
            })
            .collect();

        Ok(Object {
            bytes,
            sections,
            symbols,
            strtab,
            rela_sections,
        })
    }

    fn section_bytes(&self, section: &Section) -> Result<&'a [u8], ModuleError> {
        let start = section.offset as usize;
        let end = start
            .checked_add(section.size as usize)
            .ok_or(ModuleError::SectionHeaderOutOfBounds)?;
        self.bytes
            .get(start..end)
            .ok_or(ModuleError::SectionHeaderOutOfBounds)
    }

    fn relocations(
        &self,
        meta: &RelaSectionMeta,
    ) -> Result<impl Iterator<Item = Relocation> + '_, ModuleError> {
        let start = meta.offset as usize;
        let end = start
            .checked_add(meta.size as usize)
            .ok_or(ModuleError::SectionHeaderOutOfBounds)?;
        let raw = self
            .bytes
            .get(start..end)
            .ok_or(ModuleError::SectionHeaderOutOfBounds)?;
        let count = raw.len() / RELA_SIZE;
        Ok((0..count).map(move |i| {
            let entry = &raw[i * RELA_SIZE..i * RELA_SIZE + RELA_SIZE];
            let info = read_u64(entry, 8);
            Relocation {
                offset: read_u64(entry, 0),
                symbol: (info >> 32) as u32,
                reloc_type: info as u32,
                addend: read_u64(entry, 16) as i64,
            }
        }))
    }

    fn symbol_name(&self, index: usize) -> Result<&'a str, ModuleError> {
        let symbol = self
            .symbols
            .get(index)
            .ok_or(ModuleError::UnresolvedSymbol)?;
        read_str(self.strtab, symbol.name_off as usize).ok_or(ModuleError::UnresolvedSymbol)
    }

    fn find_defined_symbol(&self, name: &str) -> Option<usize> {
        self.symbols.iter().position(|s| {
            s.shndx != SHN_UNDEF && read_str(self.strtab, s.name_off as usize) == Some(name)
        })
    }
}

fn read_str(strtab: &[u8], offset: usize) -> Option<&str> {
    let bytes = strtab.get(offset..)?;
    let end = bytes.iter().position(|&b| b == 0)?;
    core::str::from_utf8(&bytes[..end]).ok()
}

fn align_up(value: u64, align: u64) -> u64 {
    if align <= 1 {
        return value;
    }
    (value + align - 1) & !(align - 1)
}

/// Loads `object_bytes` (a merged, relocatable module object -- see `build.rs`'s
/// `build_module_crate`), maps it into the kernel's own address space via `mapper`, resolves and
/// applies its relocations, then calls its `module_init` entry point and returns whatever it
/// returned. `panic_symbol` is the exact mangled name `build.rs` discovered for this specific
/// module's merged panic-entry reference (empty string if the module's code never references
/// one) -- see `resolve_external_symbol`.
pub fn load(
    name: &str,
    object_bytes: &[u8],
    panic_symbol: &str,
    mapper: &mut impl Mapper<Size4KiB>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<i32, ModuleError> {
    serial_println!(
        "[module] {}: loading ({} byte object)",
        name,
        object_bytes.len()
    );

    let object = Object::parse(object_bytes)?;

    // Pass 1: decide where each SHF_ALLOC section (the sections that actually consume runtime
    // memory -- .text/.rodata/.data/.bss equivalents, as opposed to e.g. relocation or symbol
    // sections themselves) lands within this module's own region, respecting each section's own
    // alignment. Section count is unpredictable and can be large (hundreds, pre-optimization) --
    // nothing here assumes a small, fixed number the way elf.rs's PT_LOAD handling can.
    let mut placements: BTreeMap<usize, u64> = BTreeMap::new();
    let mut cursor: u64 = 0;
    for (index, section) in object.sections.iter().enumerate() {
        if section.flags & SHF_ALLOC == 0 || section.size == 0 {
            continue;
        }
        cursor = align_up(cursor, section.addralign);
        placements.insert(index, cursor);
        cursor += section.size;
    }

    // A minimal GOT, appended after every placed section: one 8-byte slot per R_X86_64_GOTPCREL
    // relocation (no dedup -- a module has at most a handful, not worth the bookkeeping),
    // eagerly populated during relocation application below rather than lazily bound, since every
    // symbol is already fully resolved at load time and there's no dynamic linker-style deferral
    // to gain from doing otherwise. See CLAUDE.md's module-loading section for why this turned
    // out to be a real requirement rather than the optional complexity earlier drafts of this
    // design deliberately avoided: `core::panicking::panic_bounds_check`'s own internal message
    // formatting references a numeric `Display::fmt` impl via GOTPCREL, unavoidably, in any module
    // whose code does ordinary slice indexing -- essentially all of them.
    let mut got_slots_needed: u64 = 0;
    for rela_section in &object.rela_sections {
        for rela in object.relocations(rela_section)? {
            if rela.reloc_type == R_X86_64_GOTPCREL {
                got_slots_needed += 1;
            }
        }
    }
    let got_base = align_up(cursor, 8);
    cursor = got_base + got_slots_needed * 8;

    let region_size = align_up(cursor, PAGE_SIZE);

    let base = allocate_region(region_size)?;
    map_region(base, region_size, mapper, frame_allocator)?;

    // map_region zeroes every page it maps, which already satisfies SHT_NOBITS (.bss-equivalent)
    // sections; copy real (SHT_PROGBITS) section bytes in on top.
    for (&index, &offset) in &placements {
        let section = &object.sections[index];
        if section.sh_type == SHT_NOBITS {
            continue;
        }
        let src = object.section_bytes(section)?;
        let dst = (base + offset) as *mut u8;
        // SAFETY: dst falls within the region map_region just mapped PRESENT | WRITABLE, and
        // src/dst don't overlap (src is a view into the immutable object_bytes input).
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len()) };
    }

    // Every defined symbol's now-known absolute address (section-relative offset + this module's
    // base), used both to resolve internal relocations and to find `module_init` below.
    let mut symbol_addrs: BTreeMap<usize, u64> = BTreeMap::new();
    for (index, symbol) in object.symbols.iter().enumerate() {
        if symbol.shndx == SHN_UNDEF {
            continue;
        }
        let Some(&section_offset) = placements.get(&(symbol.shndx as usize)) else {
            // Defined in a section this loader didn't place (not SHF_ALLOC -- e.g. leftover
            // debug info) -- irrelevant, no code will ever reference it via a placed relocation.
            continue;
        };
        symbol_addrs.insert(index, base + section_offset + symbol.value);
    }

    let mut next_got_slot = base + got_base;
    for rela_section in &object.rela_sections {
        let Some(&target_offset) = placements.get(&rela_section.target_section) else {
            continue;
        };
        for rela in object.relocations(rela_section)? {
            let symbol_index = rela.symbol as usize;
            let resolved = match symbol_addrs.get(&symbol_index) {
                Some(&addr) => addr,
                None => {
                    let sym_name = object.symbol_name(symbol_index)?;
                    resolve_external_symbol(sym_name, panic_symbol)
                        .ok_or(ModuleError::UnresolvedSymbol)?
                }
            };
            let site = base + target_offset + rela.offset;
            if rela.reloc_type == R_X86_64_GOTPCREL {
                // SAFETY: slot falls within the GOT region reserved above, inside this module's
                // own just-mapped-writable pages.
                let slot = next_got_slot;
                next_got_slot += 8;
                unsafe { core::ptr::write_unaligned(slot as *mut u64, resolved) };
                // R_X86_64_GOTPCREL's formula (G + GOT + A - P) is exactly R_X86_64_PC32's own
                // formula with the GOT slot's address standing in for the symbol's address --
                // reuse apply_relocation's PC32 branch rather than duplicating the arithmetic.
                apply_relocation(site, R_X86_64_PC32, slot, rela.addend)?;
            } else {
                apply_relocation(site, rela.reloc_type, resolved, rela.addend)?;
            }
        }
    }

    let init_symbol = object
        .find_defined_symbol("module_init")
        .ok_or(ModuleError::MissingEntryPoint)?;
    let init_addr = *symbol_addrs
        .get(&init_symbol)
        .ok_or(ModuleError::MissingEntryPoint)?;

    serial_println!(
        "[module] {}: relocated at {:#x}, calling module_init",
        name,
        base
    );
    // SAFETY: init_addr was computed above from module_init's own symbol table entry plus this
    // module's now-fully-relocated base -- every relocation touching its code has already been
    // applied, and module_init's real signature (established by every module crate's own
    // #[unsafe(no_mangle)] pub extern "C" fn module_init() -> i32) matches this transmute.
    let module_init: extern "C" fn() -> i32 = unsafe { core::mem::transmute(init_addr) };
    let result = module_init();
    serial_println!("[module] {}: module_init returned {}", name, result);

    Ok(result)
}

/// Claims `size` bytes of the kernel-module virtual address region, bump-allocator style --
/// mirrors `BootInfoFrameAllocator`'s own "hand out forward, never reuse" philosophy (no module
/// unload/reload exists yet, so there's nothing to reclaim). Errors loudly rather than silently
/// wrapping past `MODULE_REGION_CEILING`, since addresses past it would violate the
/// relocation-model-static assumptions relocations below rely on.
fn allocate_region(size: u64) -> Result<u64, ModuleError> {
    let mut next = NEXT_MODULE_PAGE.lock();
    let base = *next;
    let end = base.checked_add(size).ok_or(ModuleError::RegionExhausted)?;
    if end > MODULE_REGION_CEILING {
        return Err(ModuleError::RegionExhausted);
    }
    *next = end;
    Ok(base)
}

fn map_region(
    base: u64,
    size: u64,
    mapper: &mut impl Mapper<Size4KiB>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<(), ModuleError> {
    if size == 0 {
        return Ok(());
    }
    let start_page = Page::<Size4KiB>::containing_address(VirtAddr::new(base));
    let end_page = Page::<Size4KiB>::containing_address(VirtAddr::new(base + size - 1));
    // No USER_ACCESSIBLE: module code runs only in kernel context, never executed directly by
    // ring-3 code. Every page gets WRITABLE, including ones backing .text-equivalent sections --
    // relocation application below must patch bytes inside them, and this kernel doesn't
    // implement NO_EXECUTE/W^X anywhere yet (the same simplification elf.rs's own doc comment
    // already calls out), so there's no protection benefit to a stricter per-section split today.
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
    for page in Page::range_inclusive(start_page, end_page) {
        let frame = frame_allocator
            .allocate_frame()
            .ok_or(ModuleError::OutOfMemory)?;
        // SAFETY: frame was just allocated (unused, per BootInfoFrameAllocator's contract), and
        // page falls within this module's freshly bump-allocated, previously-unmapped region.
        unsafe {
            mapper
                .map_to(page, frame, flags, frame_allocator)
                .map_err(|_| ModuleError::MappingFailed)?
                .flush();
        }
        // Freshly allocated physical frames aren't guaranteed zeroed. Written through the page's
        // own, now-mapped virtual address directly: unlike elf.rs (which maps into a *different*,
        // not-yet-active address space and must therefore write through the physical-memory-
        // offset window instead), module pages are mapped into the *currently active* kernel
        // table, so this pointer is immediately valid to dereference.
        let page_ptr = page.start_address().as_mut_ptr::<u8>();
        unsafe { core::ptr::write_bytes(page_ptr, 0, PAGE_SIZE as usize) };
    }
    Ok(())
}

/// Applies one relocation at `site` (an absolute virtual address within this module's now-mapped
/// region) for `reloc_type`, given the already-resolved address (for `R_X86_64_GOTPCREL`, the
/// address of the GOT slot the caller already populated, not the symbol itself -- see `load`'s
/// GOTPCREL handling) and the relocation's addend. These are the complete set of types observed
/// empirically across every module build tried so far (plain calls/data references,
/// `core::fmt`-heavy code including what `core::panicking::panic_bounds_check` itself references,
/// large static-buffer fills/copies) -- see `CLAUDE.md`'s module-loading section. An unrecognized
/// type is reported, not silently ignored: a module built with different codegen (a different
/// optimization level, say) could plausibly need one this loader doesn't handle yet.
fn apply_relocation(
    site: u64,
    reloc_type: u32,
    symbol_addr: u64,
    addend: i64,
) -> Result<(), ModuleError> {
    let value = (symbol_addr as i64).wrapping_add(addend);
    match reloc_type {
        R_X86_64_64 => {
            // SAFETY: site was computed from a placed section's offset within this module's own,
            // just-mapped-writable region (see map_region) plus an in-bounds relocation offset.
            unsafe { core::ptr::write_unaligned(site as *mut u64, value as u64) };
        }
        R_X86_64_32 => {
            let unsigned = value as u64;
            if unsigned > u32::MAX as u64 {
                return Err(ModuleError::RelocationOverflow);
            }
            unsafe { core::ptr::write_unaligned(site as *mut u32, unsigned as u32) };
        }
        R_X86_64_32S => {
            let signed = i32::try_from(value).map_err(|_| ModuleError::RelocationOverflow)?;
            unsafe { core::ptr::write_unaligned(site as *mut u32, signed as u32) };
        }
        R_X86_64_PC32 | R_X86_64_PLT32 => {
            // No real PLT/lazy binding exists here, or is needed: PLT32 is resolved exactly like
            // PC32, direct-referencing the real target -- correct whenever there's no lazy
            // binding to preserve, true for every eagerly-relocated module this loader handles.
            let pc_relative = value.wrapping_sub(site as i64);
            let signed = i32::try_from(pc_relative).map_err(|_| ModuleError::RelocationOverflow)?;
            unsafe { core::ptr::write_unaligned(site as *mut u32, signed as u32) };
        }
        other => return Err(ModuleError::UnsupportedRelocation(other)),
    }
    Ok(())
}

/// The kernel's hand-curated API surface for module code to call into, resolved by name against
/// each module's undefined symbols. Deliberately small and explicit, not an automatically
/// enumerated kernel symbol table -- see `CLAUDE.md`'s module-loading section for why modules
/// avoid `alloc`/`Vec`/`BTreeMap` (so this table doesn't need to expose the internal, unstable
/// `__rust_alloc`-family ABI `#[global_allocator]` wires up) and instead get an explicit
/// `oxidebsd_*` C-ABI surface plus the fixed panic-entry trampoline every module needs whether it
/// calls anything else here or not.
fn resolve_external_symbol(name: &str, panic_symbol: &str) -> Option<u64> {
    if !panic_symbol.is_empty() && name == panic_symbol {
        return Some(module_panic_trampoline as *const () as u64);
    }
    match name {
        "oxidebsd_log" => Some(oxidebsd_log as *const () as u64),
        "oxidebsd_register_syscall" => {
            Some(crate::syscall::oxidebsd_register_syscall as *const () as u64)
        }
        "oxidebsd_sys_exit" => Some(crate::syscall::oxidebsd_sys_exit as *const () as u64),
        "oxidebsd_sys_read" => Some(crate::syscall::oxidebsd_sys_read as *const () as u64),
        "oxidebsd_sys_write" => Some(crate::syscall::oxidebsd_sys_write as *const () as u64),
        "oxidebsd_alloc_fd" => Some(crate::fd::oxidebsd_alloc_fd as *const () as u64),
        "oxidebsd_register_fd_ops" => Some(crate::fd::oxidebsd_register_fd_ops as *const () as u64),
        "oxidebsd_close_fd" => Some(crate::fd::oxidebsd_close_fd as *const () as u64),
        _ => None,
    }
}

/// Writes `len` bytes at `ptr` to the kernel's serial/VGA console. Modules don't use `alloc`, so
/// there's no `&str`/`String` to pass across the module boundary directly -- a raw pointer and
/// length is the simplest shape that survives relocation without a shared ABI crate, matching how
/// the userland ELF boundary already hand-duplicates syscall constants rather than sharing one.
///
/// `extern "C"`, not `#[unsafe(no_mangle)]`: no external object ever needs to find this by
/// symbol name through the system linker (modules resolve against it purely via
/// `resolve_external_symbol`'s own name match, at module-load time, in Rust code) -- only the
/// calling *convention* needs to match what a module's `unsafe extern "C" { fn oxidebsd_log(...);
/// }` declaration expects.
extern "C" fn oxidebsd_log(ptr: *const u8, len: u64) {
    // SAFETY: modules only reach this via a relocated `call`, always passing a pointer/length
    // pair the module itself owns (e.g. a `&str`'s raw parts) -- same trust boundary as
    // sys_write's existing, documented pointer-validation gap in src/syscall.rs.
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len as usize) };
    if let Ok(s) = core::str::from_utf8(bytes) {
        serial_print!("{s}");
    }
}

/// The kernel-side replacement for every loaded module's merged `rust_begin_unwind` reference:
/// every panicking-path function in a module's `core`/`alloc` code ultimately calls this (wired
/// up by `resolve_external_symbol`, keyed off the exact mangled name `build.rs` discovers per
/// module). A module can't define its own `#[panic_handler]` -- only `bin` crates may, and this
/// target's `panic-strategy = "abort"` means there's no unwinding to reason about regardless -- so
/// a panic inside module code is exactly as fatal as a kernel panic, just logged with a different
/// prefix.
///
/// `extern "Rust"` (not `"C"`) to match how `core::panicking` itself declares this symbol --
/// relying on both sides being compiled by the very same rustc invocation's ABI for a plain
/// single-reference-argument function, which isn't an officially stable guarantee but holds in
/// practice within one compiler version, same toolchain on both sides.
extern "Rust" fn module_panic_trampoline(info: &core::panic::PanicInfo<'_>) -> ! {
    serial_println!("[module] panic: {}", info);
    crate::hlt_loop();
}
