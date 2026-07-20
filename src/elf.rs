//! A hand-rolled, minimal ELF64 parser and loader.
//!
//! Deliberately not a dependency (see `CLAUDE.md`'s dependency notes): this only needs to handle
//! the narrow slice of the format our own toolchain produces — a static, non-PIE, non-relocatable
//! `ET_EXEC` binary with a handful of `PT_LOAD` segments and nothing else (no dynamic linking, no
//! relocations, no interpreter) — which is small and mechanical enough to own outright.
//!
//! Multi-byte fields are read via explicit `from_le_bytes` on byte slices rather than casting the
//! input to a `#[repr(C)]` struct: `include_bytes!` output has no alignment guarantee, and an
//! unaligned struct cast would be undefined behavior.

use x86_64::VirtAddr;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, Page, PageTableFlags, Size4KiB, mapper::MapToError,
};

const MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const CLASS_64: u8 = 2;
const DATA_LITTLE_ENDIAN: u8 = 1;
const TYPE_EXEC: u16 = 2;
const MACHINE_X86_64: u16 = 62;

const PT_LOAD: u32 = 1;
const PF_WRITE: u32 = 1 << 1;

const EHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;
const PAGE_SIZE: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    TooShort,
    BadMagic,
    UnsupportedClass,
    UnsupportedEndianness,
    UnsupportedType,
    UnsupportedMachine,
    ProgramHeaderOutOfBounds,
    SegmentOutOfBounds,
    OutOfMemory,
    MappingFailed,
}

/// A parsed view over an in-memory ELF64 executable's header and program headers.
pub struct Elf<'a> {
    bytes: &'a [u8],
    entry: u64,
    phoff: usize,
    phnum: usize,
    phentsize: usize,
}

struct ProgramHeader {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_filesz: u64,
    p_memsz: u64,
}

impl<'a> Elf<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, ElfError> {
        if bytes.len() < EHDR_SIZE {
            return Err(ElfError::TooShort);
        }
        if bytes[0..4] != MAGIC {
            return Err(ElfError::BadMagic);
        }
        if bytes[4] != CLASS_64 {
            return Err(ElfError::UnsupportedClass);
        }
        if bytes[5] != DATA_LITTLE_ENDIAN {
            return Err(ElfError::UnsupportedEndianness);
        }
        if read_u16(bytes, 16) != TYPE_EXEC {
            return Err(ElfError::UnsupportedType);
        }
        if read_u16(bytes, 18) != MACHINE_X86_64 {
            return Err(ElfError::UnsupportedMachine);
        }

        let e_entry = read_u64(bytes, 24);
        let e_phoff = read_u64(bytes, 32) as usize;
        let e_phentsize = read_u16(bytes, 54) as usize;
        let e_phnum = read_u16(bytes, 56) as usize;

        let phdrs_size = e_phentsize
            .checked_mul(e_phnum)
            .ok_or(ElfError::ProgramHeaderOutOfBounds)?;
        let phdrs_end = e_phoff
            .checked_add(phdrs_size)
            .ok_or(ElfError::ProgramHeaderOutOfBounds)?;
        if e_phentsize < PHDR_SIZE || phdrs_end > bytes.len() {
            return Err(ElfError::ProgramHeaderOutOfBounds);
        }

        Ok(Elf {
            bytes,
            entry: e_entry,
            phoff: e_phoff,
            phnum: e_phnum,
            phentsize: e_phentsize,
        })
    }

    pub fn entry_point(&self) -> VirtAddr {
        VirtAddr::new(self.entry)
    }

    fn program_headers(&self) -> impl Iterator<Item = ProgramHeader> + '_ {
        (0..self.phnum).map(move |i| {
            let offset = self.phoff + i * self.phentsize;
            let raw = &self.bytes[offset..offset + PHDR_SIZE];
            ProgramHeader {
                p_type: read_u32(raw, 0),
                p_flags: read_u32(raw, 4),
                p_offset: read_u64(raw, 8),
                p_vaddr: read_u64(raw, 16),
                p_filesz: read_u64(raw, 32),
                p_memsz: read_u64(raw, 40),
            }
        })
    }
}

/// Maps and copies every `PT_LOAD` segment of `elf` into `mapper`'s address space, returning the
/// entry point. `physical_memory_offset` is used to write segment bytes into freshly allocated
/// frames directly (rather than through `mapper`'s own mapping, which may be read-only, and which
/// may belong to an address space that isn't active yet) — the same technique used throughout
/// `src/memory.rs` and `src/address_space.rs`.
pub fn load(
    elf: &Elf,
    mapper: &mut impl Mapper<Size4KiB>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    physical_memory_offset: VirtAddr,
) -> Result<VirtAddr, ElfError> {
    for header in elf.program_headers() {
        if header.p_type != PT_LOAD {
            continue;
        }
        if header.p_filesz > header.p_memsz {
            return Err(ElfError::SegmentOutOfBounds);
        }
        let file_end = header
            .p_offset
            .checked_add(header.p_filesz)
            .ok_or(ElfError::SegmentOutOfBounds)?;
        if file_end as usize > elf.bytes.len() {
            return Err(ElfError::SegmentOutOfBounds);
        }

        let mem_start = header.p_vaddr;
        let mem_end = header
            .p_vaddr
            .checked_add(header.p_memsz)
            .ok_or(ElfError::SegmentOutOfBounds)?;
        // The file-backed portion of the segment; anything in [file_backed_end, mem_end) is BSS.
        let file_backed_end = mem_start + header.p_filesz;

        let start_page = Page::<Size4KiB>::containing_address(VirtAddr::new(mem_start));
        let end_page = Page::<Size4KiB>::containing_address(VirtAddr::new(
            mem_end.saturating_sub(1).max(mem_start),
        ));

        let flags = PageTableFlags::PRESENT
            | PageTableFlags::USER_ACCESSIBLE
            | if header.p_flags & PF_WRITE != 0 {
                PageTableFlags::WRITABLE
            } else {
                PageTableFlags::empty()
            };

        for page in Page::range_inclusive(start_page, end_page) {
            let frame = frame_allocator
                .allocate_frame()
                .ok_or(ElfError::OutOfMemory)?;
            unsafe {
                mapper
                    .map_to(page, frame, flags, frame_allocator)
                    .map_err(|_: MapToError<Size4KiB>| ElfError::MappingFailed)?
                    .flush();
            }

            // Zero the whole frame first (covers BSS and any partial-page padding), then copy in
            // whatever file bytes actually land in this page.
            let frame_ptr =
                (physical_memory_offset + frame.start_address().as_u64()).as_mut_ptr::<u8>();
            unsafe { core::ptr::write_bytes(frame_ptr, 0, PAGE_SIZE as usize) };

            let page_start = page.start_address().as_u64();
            let page_end = page_start + PAGE_SIZE;
            let copy_start = mem_start.max(page_start);
            let copy_end = file_backed_end.min(page_end);
            if copy_start < copy_end {
                let file_offset = (header.p_offset + (copy_start - mem_start)) as usize;
                let len = (copy_end - copy_start) as usize;
                let dst = unsafe { frame_ptr.add((copy_start - page_start) as usize) };
                let src = &elf.bytes[file_offset..file_offset + len];
                unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), dst, len) };
            }
        }
    }

    Ok(elf.entry_point())
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}
