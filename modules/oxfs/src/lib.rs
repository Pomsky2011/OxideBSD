//! `oxfs`: a small, real Unix-shaped filesystem (inodes with direct + single-indirect block
//! pointers, directories as ordinary inodes holding fixed-size records, real multi-component path
//! resolution, real per-process current-working-directory) -- replacing `modules/fat32` as the
//! filesystem `stsh`/BusyBox actually run on. See `CLAUDE.md`'s oxfs section for the full design
//! rationale; `modules/fat32` is kept in the workspace, still building and self-checking on every
//! `cargo build`, but is no longer loaded at boot.
//!
//! Like `modules/fat32`, this is in-memory only -- no real block device exists yet, so nothing
//! persists across reboot. Unlike FAT32, there's no on-disk *format* to invent or generate at
//! build time at all: `module_init` below populates the inode table directly via ordinary function
//! calls, using content `build.rs` hands this crate's own `include_bytes!(env!(...))` calls (each
//! already-built userland/BusyBox ELF gets its own env var, the same `extra_env` mechanism
//! `FAT32_IMAGE_PATH` already used) or, for the two small text files, a literal and the same
//! `b'A' + i % 26` formula `modules/fat32`'s own self-check already used.
//!
//! **What this fixes relative to FAT32** (see CLAUDE.md's FAT32 section for the full list of
//! limitations this replaces): 8.3 short names -> real names up to `NAME_MAX` bytes; one path
//! component per syscall call -> real multi-component `a/b/c`/`../x`/`/a/b` resolution in one call
//! (`resolve_path`/`resolve_parent` below); a directory that can never grow past its first
//! cluster -> a directory's own inode grows additional blocks like any other file; a fixed
//! per-open-file read cap (`MAX_FILE_BUFFER`, raised three times) -> real files stream straight
//! from their own block chain on each `read()`, capped only by the block pool itself; one
//! kernel-wide current directory shared by every process -> real per-process cwd
//! (`Process::cwd` in `src/process.rs`, via `oxidebsd_get_cwd`/`oxidebsd_set_cwd`); no
//! `unlink`/`rmdir`/`rename` at all -> all three now exist.
//!
//! **Storage**, all fixed-size `static mut` arrays (modules can't use `alloc`/`Vec`/`BTreeMap` --
//! see CLAUDE.md's module-loading section): a flat pool of `NUM_BLOCKS` `BLOCK_SIZE`-byte blocks
//! (`BLOCKS`/`BLOCK_USED`), and a flat table of `MAX_INODES` inodes (`INODES`). An inode holds up
//! to `DIRECT_BLOCKS` block numbers directly plus one single-indirect block (another
//! `BLOCK_SIZE / 4` pointers) -- max file size is bounded only by the block pool, not by an
//! arbitrary per-file cap.
//!
//! **Directories are ordinary inodes** whose data blocks hold fixed 32-byte records
//! (`{ used: u8, name_len: u8, inode: u32, name: [u8; NAME_MAX] }`, `NAME_MAX = 26`) --
//! `RECORDS_PER_BLOCK = BLOCK_SIZE / 32 = 128` entries per block. A directory that fills its
//! current blocks grows another one via the same `inode_ensure_block_at` every other file write
//! uses, rather than failing outright the way FAT32's own `DirectoryFull` did. `unlink`/`rmdir`
//! just clear a record's `used` byte -- the underlying inode/blocks are never freed, matching this
//! codebase's blanket "no deallocation anywhere" policy (`do_munmap`, module unload, etc.).
//!
//! **Root is a fixed inode number (`ROOT_INODE = 0`)**, self-referencing `.`/`..` (root's `..`
//! points at itself) -- no FAT32-style "`0` means root" special-casing needed, since there's no
//! on-disk format to stay compatible with. `ROOT_INODE`'s value (`0`) deliberately coincides with
//! `Process::cwd`'s own default (`0`, "unset") -- a freshly spawned process's cwd is root with no
//! translation needed.
//!
//! **Syscalls registered at the exact numbers `modules/fat32` used** (so nothing else in the ABI
//! changes): `SYS_OPEN = 5`, `SYS_CLOSE = 6`, `SYS_CHDIR = 12`, `SYS_MKDIR = 136`,
//! `SYS_GETCWD = 108`. Plus three new ones, OxideBSD-own-invented numbers continuing from `108`
//! (per this project's own established convention -- syscalls added after the musl/BusyBox port
//! invent their own numbers rather than copying FreeBSD's, see `SYS_GETPPID`/`SYS_GETCWD`/
//! `SYS_PIPE`/`SYS_DUP2`): `SYS_UNLINK = 109`, `SYS_RMDIR = 110`,
//! `SYS_RENAME = 111` (`(old_ptr, old_len, new_ptr, new_len)` -- uses all four of this ABI's
//! argument registers, the same precedent `execve`'s `envp_ptr` set for needing `R10`). Plus
//! `SYS_FSTAT = 126`, `SYS_STAT = 127`, `SYS_LSTAT = 128` (continuing past `SYS_DUP = 125`, the
//! highest number any module had claimed) -- see `write_stat`'s own doc comment for the wire
//! format and what's synthesized vs. real. Plus `SYS_GETDENTS = 129` -- real `readdir()`'s own
//! syscall, see `oxfs_getdents`'s own doc comment for the wire format.
#![no_std]

unsafe extern "C" {
    fn oxidebsd_log(ptr: *const u8, len: u64);
    fn oxidebsd_register_syscall(
        number: u64,
        handler: extern "C" fn(u64, u64, u64, u64) -> i64,
    ) -> i32;
    fn oxidebsd_alloc_fd() -> u64;
    fn oxidebsd_register_fd_ops(
        fd: u64,
        read: extern "C" fn(u64, u64, u64) -> i64,
        write: extern "C" fn(u64, u64, u64) -> i64,
        close: extern "C" fn(u64) -> i64,
    ) -> i32;
    fn oxidebsd_close_fd(fd: u64) -> i32;
    fn oxidebsd_get_cwd() -> u64;
    fn oxidebsd_set_cwd(inode: u64);
    fn oxidebsd_real_fd_of(fd: u64) -> i64;
}

const SYS_OPEN: u64 = 5;
const SYS_CLOSE: u64 = 6;
const SYS_CHDIR: u64 = 12;
const SYS_MKDIR: u64 = 136;
const SYS_GETCWD: u64 = 108;
const SYS_UNLINK: u64 = 109;
const SYS_RMDIR: u64 = 110;
const SYS_RENAME: u64 = 111;
const SYS_FSTAT: u64 = 126;
const SYS_STAT: u64 = 127;
const SYS_LSTAT: u64 = 128;
const SYS_GETDENTS: u64 = 129;

/// Same real POSIX value FAT32's own `O_CREAT` already uses (`0o100`, not an arbitrary bit) --
/// see `modules/fat32`'s own doc comment for why matching the real bit matters (musl's real
/// `open()` passes real POSIX flag values).
const O_CREAT: u64 = 0o100;

/// Real POSIX `st_mode` file-type bits (`S_IFREG`/`S_IFDIR`), matched with `FIXED_PERM` below.
/// There's no permission model in this kernel at all yet (see `CLAUDE.md`'s "uid/permissions
/// stub" gap) -- every inode reports the same fixed `0755`, real enough to let `test -x`/`ls -l`
/// work without pretending this filesystem tracks anything it doesn't.
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const FIXED_PERM: u32 = 0o755;

/// Real `d_type` values (`include/dirent.h` on the `oxidebsd` musl branch) for `SYS_GETDENTS`'s
/// own wire format -- see `write_dirent_record`'s own doc comment.
const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;

const EBADF: i64 = 9;
const ENOENT: i64 = 2;
const EEXIST: i64 = 17;
const ENOTDIR: i64 = 20;
const EISDIR: i64 = 21;
const EMFILE: i64 = 24;
const ENOSPC: i64 = 28;
const EIO: i64 = 5;
const EINVAL: i64 = 22;
const ERANGE: i64 = 34;
/// FreeBSD's value (`66`), not Linux's (`39`) -- matching this codebase's established convention
/// of using FreeBSD errno values where they diverge (see `src/syscall.rs`'s own `ENOSYS`).
const ENOTEMPTY: i64 = 66;

const BLOCK_SIZE: usize = 4096;
/// 32 MiB pool (raised from 4 MiB once the BusyBox roster grew from 24 applets to ~300 -- see
/// CLAUDE.md's BusyBox section -- whose combined embedded ELF bytes alone run to ~18 MiB), with
/// real headroom left over for runtime-created files (`stsh`'s `write` built-in, BusyBox's own
/// file creation). `src/memory.rs`'s frame allocator and this module's own eager, non-paged
/// mapping mean this whole pool becomes a real physical-memory commitment the moment the module
/// loads (see `Cargo.toml`'s `[package.metadata.bootimage]` `-m` bump, made at the same time as
/// this).
const NUM_BLOCKS: usize = 8192;
/// Raised from 64 alongside `NUM_BLOCKS` above, same reason -- ~300 applets plus root/`hello.txt`/
/// `big.txt`/the self-check's own `/gdtest` fixtures need comfortably more than 64 inode slots.
const MAX_INODES: usize = 512;
const DIRECT_BLOCKS: usize = 12;
const PTRS_PER_INDIRECT: usize = BLOCK_SIZE / 4;
/// Sentinel for "no block"/"no indirect block" -- block numbers are plain indices into `BLOCKS`
/// starting at `0` (unlike FAT32's cluster numbering, which reserves `0`/`1`), so `0` itself can't
/// double as the sentinel the way it does there.
const NO_BLOCK: u32 = u32::MAX;

const ROOT_INODE: u32 = 0;

const DIR_RECORD_SIZE: usize = 32;
const NAME_MAX: usize = 26;
const RECORDS_PER_BLOCK: usize = BLOCK_SIZE / DIR_RECORD_SIZE;

const MAX_OPEN_FILES: usize = 8;
/// Write-side accumulator cap (see `OpenFile::Write`'s own doc comment) -- comfortably past
/// today's largest embedded binary (`sh.elf`, ~102 KB). Matches `modules/fat32`'s own final,
/// proven-sufficient `MAX_FILE_BUFFER` value exactly (rather than something bigger): `OpenFile`'s
/// `Write` variant is the largest in the enum, so every `OPEN_FILES` slot reserves this much
/// space regardless of what it actually holds -- no reason to size it past what's actually needed.
const MAX_WRITE_BUFFER: usize = 131072;
const DIR_LISTING_BUFFER: usize = 4096;

const MAX_CWD_PATH: usize = 256;
const MAX_CWD_DEPTH: usize = 32;

const BIG_FILE_LEN: usize = 5000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum InodeKind {
    Free,
    File,
    Dir,
}

#[derive(Clone, Copy)]
struct Inode {
    kind: InodeKind,
    size: u32,
    direct: [u32; DIRECT_BLOCKS],
    indirect: u32,
}

impl Inode {
    const FREE: Inode = Inode {
        kind: InodeKind::Free,
        size: 0,
        direct: [NO_BLOCK; DIRECT_BLOCKS],
        indirect: NO_BLOCK,
    };

    fn new(kind: InodeKind) -> Inode {
        Inode {
            kind,
            size: 0,
            direct: [NO_BLOCK; DIRECT_BLOCKS],
            indirect: NO_BLOCK,
        }
    }
}

/// `static mut`, not `static` -- same requirement `modules/fat32`'s own `DISK`/`OPEN_FILES` have
/// (see that module's doc comment): every read happens from within this module's own exported,
/// syscall-reachable functions, whose results feed observably into `oxidebsd_log`/syscall return
/// values, so the optimizer can't treat any write as an unobservable dead store. All-zero initial
/// values place these in `.bss` (not baked into the merged object's own size).
static mut BLOCKS: [[u8; BLOCK_SIZE]; NUM_BLOCKS] = [[0; BLOCK_SIZE]; NUM_BLOCKS];
static mut BLOCK_USED: [bool; NUM_BLOCKS] = [false; NUM_BLOCKS];
static mut INODES: [Inode; MAX_INODES] = [Inode::FREE; MAX_INODES];
static mut OPEN_FILES: [Option<(u64, OpenFile)>; MAX_OPEN_FILES] = [None; MAX_OPEN_FILES];

fn read_block(n: u32) -> [u8; BLOCK_SIZE] {
    // SAFETY: see BLOCKS's own doc comment -- single-core, syscall-serialized access only. Copies
    // the whole block out by value rather than returning a reference, so no borrow of the static
    // ever outlives this call -- deliberately simple over clever, see the module doc comment.
    unsafe { (*core::ptr::addr_of!(BLOCKS))[n as usize] }
}

fn write_block(n: u32, data: &[u8; BLOCK_SIZE]) {
    unsafe { (*core::ptr::addr_of_mut!(BLOCKS))[n as usize] = *data };
}

fn block_used(n: u32) -> bool {
    unsafe { (*core::ptr::addr_of!(BLOCK_USED))[n as usize] }
}

fn set_block_used(n: u32, used: bool) {
    unsafe { (*core::ptr::addr_of_mut!(BLOCK_USED))[n as usize] = used };
}

fn read_inode(n: u32) -> Inode {
    unsafe { (*core::ptr::addr_of!(INODES))[n as usize] }
}

fn write_inode(n: u32, inode: Inode) {
    unsafe { (*core::ptr::addr_of_mut!(INODES))[n as usize] = inode };
}

/// Linear scan for the first free block -- fine at this module's scale (`NUM_BLOCKS = 1024`), same
/// "simplicity over a free list" choice `modules/fat32`'s own `allocate_cluster` already makes.
fn alloc_block() -> Option<u32> {
    for i in 0..NUM_BLOCKS as u32 {
        if !block_used(i) {
            set_block_used(i, true);
            write_block(i, &[0u8; BLOCK_SIZE]);
            return Some(i);
        }
    }
    None
}

/// Like `alloc_block`, but fills the fresh block with `0xFF` bytes, not zero -- required for an
/// indirect block specifically: each 4-byte slot is a block-number pointer, and a plain
/// zero-filled block would decode every slot as block `0` (a real, valid block), not `NO_BLOCK`
/// ("no pointer here yet").
fn alloc_indirect_block() -> Option<u32> {
    let n = alloc_block()?;
    write_block(n, &[0xFFu8; BLOCK_SIZE]);
    Some(n)
}

fn alloc_inode() -> Option<u32> {
    (0..MAX_INODES as u32).find(|&i| read_inode(i).kind == InodeKind::Free)
}

/// Reads the block number backing `inode`'s logical block `index` (direct or, past
/// `DIRECT_BLOCKS`, via the single-indirect block), or `None` if that block was never allocated.
fn inode_block_at(inode: &Inode, index: usize) -> Option<u32> {
    if index < DIRECT_BLOCKS {
        let b = inode.direct[index];
        (b != NO_BLOCK).then_some(b)
    } else {
        let indirect_index = index - DIRECT_BLOCKS;
        if inode.indirect == NO_BLOCK || indirect_index >= PTRS_PER_INDIRECT {
            return None;
        }
        let ib = read_block(inode.indirect);
        let off = indirect_index * 4;
        let b = u32::from_le_bytes([ib[off], ib[off + 1], ib[off + 2], ib[off + 3]]);
        (b != NO_BLOCK).then_some(b)
    }
}

/// Like `inode_block_at`, but allocates a fresh block (and, if needed, a fresh indirect block)
/// when `index` isn't backed by one yet -- used by both real file writes and directory growth.
/// Takes an inode *number*, not `&mut Inode`: every access to `INODES`/`BLOCKS` in this module
/// goes through the copy-in/copy-out helpers above, so no reference to either static is ever held
/// across a nested call (`alloc_block` here) that itself touches them.
fn inode_ensure_block_at(inode_num: u32, index: usize) -> Option<u32> {
    let mut inode = read_inode(inode_num);
    let result = if index < DIRECT_BLOCKS {
        if inode.direct[index] == NO_BLOCK {
            inode.direct[index] = alloc_block()?;
        }
        Some(inode.direct[index])
    } else {
        let indirect_index = index - DIRECT_BLOCKS;
        if indirect_index >= PTRS_PER_INDIRECT {
            return None;
        }
        if inode.indirect == NO_BLOCK {
            inode.indirect = alloc_indirect_block()?;
        }
        let mut ib = read_block(inode.indirect);
        let off = indirect_index * 4;
        let existing = u32::from_le_bytes([ib[off], ib[off + 1], ib[off + 2], ib[off + 3]]);
        if existing == NO_BLOCK {
            let nb = alloc_block()?;
            ib[off..off + 4].copy_from_slice(&nb.to_le_bytes());
            write_block(inode.indirect, &ib);
            Some(nb)
        } else {
            Some(existing)
        }
    };
    write_inode(inode_num, inode);
    result
}

/// Reads up to `out.len()` bytes starting at `position` within `inode_num`'s data, honoring its
/// stored `size` (real files only -- directories never call this, they walk raw records instead).
/// Returns the number of bytes actually read (`0` at or past EOF).
fn read_inode_at(inode_num: u32, position: usize, out: &mut [u8]) -> usize {
    let inode = read_inode(inode_num);
    let size = inode.size as usize;
    if position >= size {
        return 0;
    }
    let n = out.len().min(size - position);
    let mut written = 0;
    while written < n {
        let file_off = position + written;
        let block_index = file_off / BLOCK_SIZE;
        let in_block_off = file_off % BLOCK_SIZE;
        let Some(blk) = inode_block_at(&inode, block_index) else {
            break;
        };
        let block = read_block(blk);
        let chunk = (n - written).min(BLOCK_SIZE - in_block_off);
        out[written..written + chunk].copy_from_slice(&block[in_block_off..in_block_off + chunk]);
        written += chunk;
    }
    written
}

/// Writes `content` as `inode_num`'s complete contents, allocating whatever blocks are needed and
/// setting `size` -- the only write primitive this module has (matching `modules/fat32`'s own
/// "writes only ever create/replace a file's complete contents in one operation" simplification).
fn write_inode_data(inode_num: u32, content: &[u8]) -> bool {
    let block_count = content.len().div_ceil(BLOCK_SIZE);
    for i in 0..block_count {
        let Some(blk) = inode_ensure_block_at(inode_num, i) else {
            return false;
        };
        let start = i * BLOCK_SIZE;
        let end = (start + BLOCK_SIZE).min(content.len());
        let mut buf = [0u8; BLOCK_SIZE];
        buf[..end - start].copy_from_slice(&content[start..end]);
        write_block(blk, &buf);
    }
    let mut inode = read_inode(inode_num);
    inode.size = content.len() as u32;
    write_inode(inode_num, inode);
    true
}

/// Byte-exact mirror of musl's `struct stat` for x86_64 (`arch/x86_64/bits/stat.h` in
/// `third_party/musl`) -- `dev_t`/`ino_t`/`nlink_t`/`off_t`/`blksize_t`/`blkcnt_t` are all 64-bit
/// on this target, and `struct timespec`'s `{tv_sec, tv_nsec}` is bit-identical to two raw `i64`s
/// here, so this `repr(C)` struct's natural layout already matches the real one field-for-field --
/// no manual padding needed beyond `__pad0` (which upstream also has explicitly, between the
/// `u32` id fields and the next `u64`). `src/stat/{stat,fstat,lstat}.c` on the `oxidebsd` musl
/// branch write straight into this shape, bypassing musl's usual `fstatat`/`kstat` indirection
/// entirely (same "patch the entry point, not the generic multiplexer" pattern `open()`/`chdir()`/
/// `mkdir()` already established -- see `CLAUDE.md`'s musl section).
#[repr(C)]
struct MuslStat {
    st_dev: u64,
    st_ino: u64,
    st_nlink: u64,
    st_mode: u32,
    st_uid: u32,
    st_gid: u32,
    __pad0: u32,
    st_rdev: u64,
    st_size: i64,
    st_blksize: i64,
    st_blocks: i64,
    st_atime_sec: i64,
    st_atime_nsec: i64,
    st_mtime_sec: i64,
    st_mtime_nsec: i64,
    st_ctime_sec: i64,
    st_ctime_nsec: i64,
    __unused: [i64; 3],
}

const _: () = assert!(core::mem::size_of::<MuslStat>() == 144);

/// Builds a `MuslStat` for `inode_num` and writes it into the caller's buffer at `buf_ptr` --
/// shared by `oxfs_stat`/`oxfs_lstat` (path-based) and `oxfs_fstat` (fd-based). Everything this
/// filesystem doesn't actually model is a fixed, honestly-fake value rather than a plausible-
/// looking guess: `st_uid`/`st_gid` are `0` (no uid/permission model exists at all -- see
/// `CLAUDE.md`'s gap table), `st_mode`'s permission bits are always `FIXED_PERM`, timestamps are
/// all `0` (no clock/RTC source exists yet -- see the same gap table's "clock + nanosleep" row),
/// and `st_dev` is a fixed `1` (there's only ever one filesystem). `st_nlink` is `2` for a
/// directory (`.` plus its parent's entry for it) and `1` for a file -- this filesystem doesn't
/// track hard links, so a directory's real subdirectory count (which would also bump its parent's
/// linked-from count) isn't reflected either. `st_ino`/`st_size`/`st_blocks` are the only fields
/// backed by something real. `write_unaligned` since a userland `struct stat*` has no alignment
/// guarantee this kernel can rely on (same trust boundary as every other raw user pointer here --
/// see the module doc comment).
fn write_stat(inode_num: u32, buf_ptr: u64) -> i64 {
    let inode = read_inode(inode_num);
    let (mode, nlink) = match inode.kind {
        InodeKind::Dir => (S_IFDIR | FIXED_PERM, 2u64),
        _ => (S_IFREG | FIXED_PERM, 1u64),
    };
    let size = inode.size as i64;
    let stat = MuslStat {
        st_dev: 1,
        st_ino: inode_num as u64,
        st_nlink: nlink,
        st_mode: mode,
        st_uid: 0,
        st_gid: 0,
        __pad0: 0,
        st_rdev: 0,
        st_size: size,
        st_blksize: BLOCK_SIZE as i64,
        st_blocks: (size + 511) / 512,
        st_atime_sec: 0,
        st_atime_nsec: 0,
        st_mtime_sec: 0,
        st_mtime_nsec: 0,
        st_ctime_sec: 0,
        st_ctime_nsec: 0,
        __unused: [0; 3],
    };
    // SAFETY: same trust boundary as elsewhere -- caller-owned pointer, sized by the caller's own
    // `sizeof(struct stat)` (144 bytes, matching `MuslStat` exactly, checked above).
    unsafe { (buf_ptr as *mut MuslStat).write_unaligned(stat) };
    0
}

/// Looks up the inode number backing an already-open real fd -- `oxfs_fstat`'s own lookup, since
/// `OPEN_FILES` is keyed by `real_fd` (see `oxidebsd_real_fd_of`'s own doc comment for why a
/// syscall-number-registered handler has to resolve that itself rather than getting it for free
/// the way `SYS_READ`/`SYS_WRITE` do). `None` for a `Write`-in-progress fd (`open(O_CREAT)` before
/// `close`) -- this filesystem doesn't allocate a real inode until close (see `OpenFile::Write`'s
/// own doc comment), so there's genuinely nothing to report yet.
fn inode_of_open_file(real_fd: u64) -> Option<u32> {
    match find_open_file(real_fd)? {
        OpenFile::FileRead { inode, .. } => Some(*inode),
        OpenFile::DirListing { inode, .. } => Some(*inode),
        OpenFile::Write { .. } => None,
    }
}

/// The on-wire byte size of one `SYS_GETDENTS` record for a name of `name_len` bytes -- real
/// Linux `dirent64` layout (`d_ino: u64, d_off: i64, d_reclen: u16, d_type: u8, d_name: [u8; N]`,
/// `N` bytes wide including a NUL terminator), padded up to the next 8-byte boundary the same way
/// real Linux does (musl's `struct dirent` -- `arch/generic/bits/dirent.h` on the `oxidebsd` musl
/// branch, since `x86_64` doesn't override it -- assumes 8-byte-aligned records when it casts a
/// raw syscall buffer straight into `struct dirent*`).
fn dirent_record_len(name_len: usize) -> usize {
    let unpadded = 8 + 8 + 2 + 1 + name_len + 1;
    (unpadded + 7) & !7
}

/// Writes one `SYS_GETDENTS` record into `out`, whose length must already be exactly
/// `dirent_record_len(name.len())` (`oxfs_getdents` slices its output buffer to that size before
/// calling this). `off_cookie` becomes `d_off` -- real Linux uses this as an opaque seek cookie
/// for `telldir`/`seekdir`; nothing in this port's ported applets calls either, so a monotonic
/// counter (`oxfs_getdents`'s own `dirent_pos`, one-past the record just written) is honest enough
/// without pretending to support real seeking. Padding bytes past the NUL terminator are zeroed,
/// not left as whatever `out` already held -- `out` is caller-owned userland memory, reused across
/// `SYS_GETDENTS` calls at the same address in `hush`/coreutils' own DIR buffer.
fn write_dirent_record(out: &mut [u8], ino: u64, off_cookie: i64, dtype: u8, name: &[u8]) {
    let reclen = out.len();
    out[0..8].copy_from_slice(&ino.to_le_bytes());
    out[8..16].copy_from_slice(&off_cookie.to_le_bytes());
    out[16..18].copy_from_slice(&(reclen as u16).to_le_bytes());
    out[18] = dtype;
    let name_start = 19;
    out[name_start..name_start + name.len()].copy_from_slice(name);
    for b in &mut out[name_start + name.len()..reclen] {
        *b = 0;
    }
}

fn dir_record_used(block: &[u8; BLOCK_SIZE], idx: usize) -> bool {
    block[idx * DIR_RECORD_SIZE] != 0
}

fn dir_record_name_len(block: &[u8; BLOCK_SIZE], idx: usize) -> usize {
    block[idx * DIR_RECORD_SIZE + 1] as usize
}

fn dir_record_name(block: &[u8; BLOCK_SIZE], idx: usize) -> &[u8] {
    let len = dir_record_name_len(block, idx);
    let start = idx * DIR_RECORD_SIZE + 6;
    &block[start..start + len]
}

fn dir_record_inode(block: &[u8; BLOCK_SIZE], idx: usize) -> u32 {
    let off = idx * DIR_RECORD_SIZE + 2;
    u32::from_le_bytes([block[off], block[off + 1], block[off + 2], block[off + 3]])
}

fn write_dir_record(block: &mut [u8; BLOCK_SIZE], idx: usize, name: &[u8], inode: u32) {
    let off = idx * DIR_RECORD_SIZE;
    block[off] = 1;
    block[off + 1] = name.len() as u8;
    block[off + 2..off + 6].copy_from_slice(&inode.to_le_bytes());
    block[off + 6..off + 6 + name.len()].copy_from_slice(name);
}

fn clear_dir_record(block: &mut [u8; BLOCK_SIZE], idx: usize) {
    block[idx * DIR_RECORD_SIZE] = 0;
}

/// Looks `name` up directly inside `dir_inode` (no path walking -- see `resolve_path`/
/// `resolve_parent` for that). `name` may be `.`/`..`, both stored as real records like any other
/// (seeded by `oxfs_mkdir`/`module_init`, self-referencing for root).
fn dir_lookup(dir_inode: u32, name: &[u8]) -> Option<u32> {
    let inode = read_inode(dir_inode);
    let mut i = 0;
    while let Some(blk) = inode_block_at(&inode, i) {
        let block = read_block(blk);
        for r in 0..RECORDS_PER_BLOCK {
            if dir_record_used(&block, r) && dir_record_name(&block, r) == name {
                return Some(dir_record_inode(&block, r));
            }
        }
        i += 1;
    }
    None
}

#[derive(Debug, Clone, Copy)]
enum OxfsError {
    NotFound,
    NotADirectory,
    InvalidPath,
    DiskFull,
}

fn errno_for(e: OxfsError) -> i64 {
    -match e {
        OxfsError::NotFound => ENOENT,
        OxfsError::NotADirectory => ENOTDIR,
        OxfsError::InvalidPath => EINVAL,
        OxfsError::DiskFull => ENOSPC,
    }
}

/// Inserts a new `(name, target_inode)` record into `dir_inode`, reusing the first free (cleared
/// by a previous `dir_remove`) or never-yet-used record slot -- growing `dir_inode` with a fresh
/// block via `inode_ensure_block_at` if every existing block is full. This is the "a directory can
/// grow past its first cluster" fix over `modules/fat32`'s own `DirectoryFull`/`ENOSPC` dead end.
fn dir_insert(dir_inode: u32, name: &[u8], target_inode: u32) -> Result<(), OxfsError> {
    if name.len() > NAME_MAX {
        return Err(OxfsError::InvalidPath);
    }
    let inode = read_inode(dir_inode);
    let mut i = 0;
    loop {
        match inode_block_at(&inode, i) {
            Some(blk) => {
                let mut block = read_block(blk);
                for r in 0..RECORDS_PER_BLOCK {
                    if !dir_record_used(&block, r) {
                        write_dir_record(&mut block, r, name, target_inode);
                        write_block(blk, &block);
                        return Ok(());
                    }
                }
                i += 1;
            }
            None => {
                let Some(blk) = inode_ensure_block_at(dir_inode, i) else {
                    return Err(OxfsError::DiskFull);
                };
                // inode_ensure_block_at hands back a freshly zeroed block (record 0 is free) --
                // no need to scan it first.
                let mut block = [0u8; BLOCK_SIZE];
                write_dir_record(&mut block, 0, name, target_inode);
                write_block(blk, &block);
                return Ok(());
            }
        }
    }
}

/// Clears the record named `name` inside `dir_inode` -- the underlying inode/blocks are *not*
/// freed (see the module doc comment). `Err(NotFound)` if no such record exists (callers normally
/// check via `dir_lookup` first, so this mostly can't fail in practice).
fn dir_remove(dir_inode: u32, name: &[u8]) -> Result<(), OxfsError> {
    let inode = read_inode(dir_inode);
    let mut i = 0;
    while let Some(blk) = inode_block_at(&inode, i) {
        let mut block = read_block(blk);
        for r in 0..RECORDS_PER_BLOCK {
            if dir_record_used(&block, r) && dir_record_name(&block, r) == name {
                clear_dir_record(&mut block, r);
                write_block(blk, &block);
                return Ok(());
            }
        }
        i += 1;
    }
    Err(OxfsError::NotFound)
}

/// Counts live records in `dir_inode`, `.`/`..` included -- an otherwise-empty directory always
/// has exactly `2` (used by `oxfs_rmdir`).
fn dir_entry_count(dir_inode: u32) -> usize {
    let inode = read_inode(dir_inode);
    let mut count = 0;
    let mut i = 0;
    while let Some(blk) = inode_block_at(&inode, i) {
        let block = read_block(blk);
        for r in 0..RECORDS_PER_BLOCK {
            if dir_record_used(&block, r) {
                count += 1;
            }
        }
        i += 1;
    }
    count
}

/// Returns the `n`th (0-indexed) used record inside `dir_inode`, walking blocks in the same
/// order `dir_lookup`/`dir_entry_count` do -- `.`/`..` included, unlike `open_dir_listing`'s own
/// pretty-printed summary, since `SYS_GETDENTS`'s real callers (`opendir`/`readdir`) expect every
/// real record. `None` once `n` reaches the record count -- `oxfs_getdents`'s own EOF signal.
fn dir_nth_used_record(dir_inode: u32, n: usize) -> Option<(u32, [u8; NAME_MAX], u8)> {
    let inode = read_inode(dir_inode);
    let mut seen = 0usize;
    let mut i = 0;
    while let Some(blk) = inode_block_at(&inode, i) {
        let block = read_block(blk);
        for r in 0..RECORDS_PER_BLOCK {
            if dir_record_used(&block, r) {
                if seen == n {
                    let name = dir_record_name(&block, r);
                    let mut buf = [0u8; NAME_MAX];
                    buf[..name.len()].copy_from_slice(name);
                    return Some((dir_record_inode(&block, r), buf, name.len() as u8));
                }
                seen += 1;
            }
        }
        i += 1;
    }
    None
}

/// Resolves `path` to a single inode number, starting from `cwd_inode` (or root, if `path` starts
/// with `/`) and walking every `/`-separated component (`.`/`..`/empty components handled along
/// the way) -- real multi-component resolution, replacing `modules/fat32`'s single-component-only
/// `to_short_name`. Every component but the last must itself be a directory; the last may be
/// anything (a file, a directory, or simply not exist, which callers interested in creating
/// something use `resolve_parent` to detect instead).
fn resolve_path(cwd_inode: u32, path: &[u8]) -> Result<u32, OxfsError> {
    let mut current = if path.first() == Some(&b'/') {
        ROOT_INODE
    } else {
        cwd_inode
    };
    let mut iter = path
        .split(|&b| b == b'/')
        .filter(|c| !c.is_empty())
        .peekable();
    while let Some(component) = iter.next() {
        let is_last = iter.peek().is_none();
        if component == b"." {
            continue;
        }
        let next = dir_lookup(current, component).ok_or(OxfsError::NotFound)?;
        if !is_last && read_inode(next).kind != InodeKind::Dir {
            return Err(OxfsError::NotADirectory);
        }
        current = next;
    }
    Ok(current)
}

/// Resolves `path` to its *parent* directory's inode number plus the final path component's raw
/// name bytes (still borrowed from `path`) -- used by every operation that creates, removes, or
/// renames a name (`open` with `O_CREAT`, `mkdir`, `unlink`, `rmdir`, `rename`), since those need
/// to mutate the parent's own directory records rather than just look the target up.
fn resolve_parent(cwd_inode: u32, path: &[u8]) -> Result<(u32, &[u8]), OxfsError> {
    let mut end = path.len();
    while end > 0 && path[end - 1] == b'/' {
        end -= 1;
    }
    if end == 0 {
        // "" / "/" / "///" -- no leaf component to create, remove, or rename.
        return Err(OxfsError::InvalidPath);
    }
    let head = &path[..end];
    let leaf_start = head.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1);
    let leaf = &head[leaf_start..];
    if leaf == b"." || leaf == b".." || leaf.len() > NAME_MAX {
        return Err(OxfsError::InvalidPath);
    }
    // Includes the trailing '/' when leaf_start > 0 (e.g. head = "/foo" -> parent_path = "/",
    // head = "sub/foo" -> parent_path = "sub/") -- harmless, resolve_path treats a trailing
    // separator as no extra component. When leaf_start == 0 (a bare name, no directory prefix)
    // this is "", which resolve_path already resolves to cwd_inode directly.
    let parent_path = &head[..leaf_start];
    let parent_inode = resolve_path(cwd_inode, parent_path)?;
    if read_inode(parent_inode).kind != InodeKind::Dir {
        return Err(OxfsError::NotADirectory);
    }
    Ok((parent_inode, leaf))
}

fn current_cwd() -> u32 {
    // SAFETY: FFI call to a kernel-exported function, matching its declared signature exactly.
    unsafe { oxidebsd_get_cwd() as u32 }
}

fn set_current_cwd(inode: u32) {
    unsafe { oxidebsd_set_cwd(inode as u64) };
}

/// Finds `target`'s own name as recorded in `parent`'s listing (`.`/`..` excluded) -- a directory
/// never stores its own name, only its parent's records do, so recovering one always means
/// searching the parent. Used by `build_cwd_path`.
fn find_name_of_inode_in_dir(parent: u32, target: u32) -> Option<([u8; NAME_MAX], u8)> {
    let inode = read_inode(parent);
    let mut i = 0;
    while let Some(blk) = inode_block_at(&inode, i) {
        let block = read_block(blk);
        for r in 0..RECORDS_PER_BLOCK {
            if dir_record_used(&block, r) {
                let name = dir_record_name(&block, r);
                if name != b"." && name != b".." && dir_record_inode(&block, r) == target {
                    let mut buf = [0u8; NAME_MAX];
                    buf[..name.len()].copy_from_slice(name);
                    return Some((buf, name.len() as u8));
                }
            }
        }
        i += 1;
    }
    None
}

/// Reconstructs an absolute path for `inode_num` by walking `..` links up to root and, at each
/// level, recovering that level's own name from its parent's listing -- there's no stored path
/// anywhere, only inode numbers, so every call re-derives it from scratch (same approach
/// `modules/fat32`'s own `build_cwd_path` already used for cluster numbers). Root itself is `"/"`.
fn build_cwd_path(inode_num: u32, out: &mut [u8; MAX_CWD_PATH]) -> usize {
    let mut chain = [0u32; MAX_CWD_DEPTH];
    let mut depth = 0;
    let mut cur = inode_num;
    while cur != ROOT_INODE && depth < MAX_CWD_DEPTH {
        chain[depth] = cur;
        depth += 1;
        cur = dir_lookup(cur, b"..").unwrap_or(ROOT_INODE);
    }

    if depth == 0 {
        out[0] = b'/';
        return 1;
    }

    let mut len = 0;
    for i in (0..depth).rev() {
        let child = chain[i];
        let parent = if i + 1 < depth {
            chain[i + 1]
        } else {
            ROOT_INODE
        };
        let Some((name, name_len)) = find_name_of_inode_in_dir(parent, child) else {
            break;
        };
        out[len] = b'/';
        len += 1;
        let name_len = name_len as usize;
        out[len..len + name_len].copy_from_slice(&name[..name_len]);
        len += name_len;
    }
    len
}

/// An open file's own state, keyed by fd in `OPEN_FILES`. `Write`'s buffer dwarfs the other two
/// variants -- deliberate, not overlooked: modules can't use `alloc`/`Box`, so every `OPEN_FILES`
/// slot has to be sized for the worst case regardless (the same "no allocator, so every slot pays
/// the largest variant's cost" shape `modules/fat32`'s own `OpenFile` already has, just with more
/// size variance here since `FileRead` no longer carries a buffer at all).
#[derive(Clone, Copy)]
#[allow(clippy::large_enum_variant)]
enum OpenFile {
    /// A real file, opened for reading -- streams straight from `inode`'s own block chain on each
    /// `read()` call via `read_inode_at` rather than caching the whole file at `open` time (unlike
    /// `modules/fat32`'s own `OpenFile::Read`), so file size is bounded only by the block pool,
    /// not by a fixed per-fd buffer.
    FileRead { inode: u32, position: usize },
    /// A directory listing, formatted into a fixed buffer at `open` time -- listings stay small,
    /// so caching one is simpler than streaming it record-by-record, and this mirrors
    /// `modules/fat32`'s existing "open a directory, read back a formatted listing" trick for
    /// `ls`. `inode` is the directory's own inode number -- used by `inode_of_open_file`
    /// (`oxfs_fstat` on a directory fd) and by `oxfs_getdents`, which walks `inode`'s *live*
    /// records directly rather than this variant's own pre-formatted `content` (real
    /// `readdir()`/`getdents()` must see every record, `.`/`..` included, not the human-readable
    /// summary `content` holds). `dirent_pos` is `oxfs_getdents`'s own resume cursor -- see that
    /// function's own doc comment.
    DirListing {
        inode: u32,
        content: [u8; DIR_LISTING_BUFFER],
        len: usize,
        position: usize,
        dirent_pos: usize,
    },
    /// A file opened for writing -- accumulates across possibly-multiple `write` calls, committed
    /// to a real inode only at `close` time (same all-at-once-on-close model
    /// `modules/fat32` already uses).
    Write {
        parent_inode: u32,
        name: [u8; NAME_MAX],
        name_len: u8,
        buffer: [u8; MAX_WRITE_BUFFER],
        len: usize,
    },
}

fn register_open_file(open_file: OpenFile) -> i64 {
    let slots = unsafe { &mut *core::ptr::addr_of_mut!(OPEN_FILES) };
    let Some(slot) = slots.iter_mut().find(|s| s.is_none()) else {
        return -EMFILE;
    };
    // SAFETY: FFI call to a kernel-exported function, matching its declared signature exactly.
    let fd = unsafe { oxidebsd_alloc_fd() };
    *slot = Some((fd, open_file));
    // SAFETY: oxfs_read/oxfs_write/oxfs_close are this module's own functions, already relocated
    // by the time module_init (which makes this function reachable) runs.
    unsafe { oxidebsd_register_fd_ops(fd, oxfs_read, oxfs_write, oxfs_close) };
    fd as i64
}

fn find_open_file(fd: u64) -> Option<&'static mut OpenFile> {
    let slots = unsafe { &mut *core::ptr::addr_of_mut!(OPEN_FILES) };
    for (slot_fd, file) in slots.iter_mut().flatten() {
        if *slot_fd == fd {
            return Some(file);
        }
    }
    None
}

/// Formats `dir_inode`'s listing (one name per line, `<DIR>` or a byte count) into a fresh
/// `OpenFile::DirListing` and registers a fd for it -- see the module doc comment's note on `ls`.
/// `.`/`..` are hidden, matching plain `ls`'s default.
fn open_dir_listing(dir_inode: u32) -> i64 {
    let mut content = [0u8; DIR_LISTING_BUFFER];
    let len = {
        let mut out = ByteBuf {
            buf: &mut content,
            len: 0,
        };
        let inode = read_inode(dir_inode);
        let mut i = 0;
        while let Some(blk) = inode_block_at(&inode, i) {
            let block = read_block(blk);
            for r in 0..RECORDS_PER_BLOCK {
                if !dir_record_used(&block, r) {
                    continue;
                }
                let name = dir_record_name(&block, r);
                if name == b"." || name == b".." {
                    continue;
                }
                let child_inode = read_inode(dir_record_inode(&block, r));
                out.push_bytes(name);
                if child_inode.kind == InodeKind::Dir {
                    out.push_bytes(b"  <DIR>\n");
                } else {
                    out.push_bytes(b"  ");
                    out.push_decimal(child_inode.size);
                    out.push_bytes(b"\n");
                }
            }
            i += 1;
        }
        out.len
    };
    register_open_file(OpenFile::DirListing {
        inode: dir_inode,
        content,
        len,
        position: 0,
        dirent_pos: 0,
    })
}

/// Registered for `SYS_OPEN`. `""`/`"."`/`".."`/`"/"` are special-cased (mirroring
/// `modules/fat32`'s own handling of them) before falling into `resolve_parent`, which -- unlike
/// FAT32's single-component `to_short_name` -- handles an arbitrarily deep path
/// (`sub/inner/file.txt`) in this one call.
extern "C" fn oxfs_open(path_ptr: u64, path_len: u64, flags: u64, _r10: u64) -> i64 {
    // SAFETY: same trust boundary as sys_write's own documented pointer-validation gap in
    // src/syscall.rs -- the caller (ultimately userland, via SYS_OPEN) owns this pointer/length.
    let path = unsafe { core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize) };
    let create = flags & O_CREAT != 0;
    let cwd = current_cwd();

    if path.is_empty() || path == b"." {
        return open_dir_listing(cwd);
    }
    if path == b"/" {
        return open_dir_listing(ROOT_INODE);
    }
    if path == b".." {
        return match dir_lookup(cwd, b"..") {
            Some(parent) => open_dir_listing(parent),
            None => -ENOENT,
        };
    }

    let (parent, leaf) = match resolve_parent(cwd, path) {
        Ok(v) => v,
        Err(e) => return errno_for(e),
    };

    match dir_lookup(parent, leaf) {
        Some(inode_num) => match read_inode(inode_num).kind {
            InodeKind::Dir => open_dir_listing(inode_num),
            _ => register_open_file(OpenFile::FileRead {
                inode: inode_num,
                position: 0,
            }),
        },
        None if create => {
            let mut name = [0u8; NAME_MAX];
            name[..leaf.len()].copy_from_slice(leaf);
            register_open_file(OpenFile::Write {
                parent_inode: parent,
                name,
                name_len: leaf.len() as u8,
                buffer: [0; MAX_WRITE_BUFFER],
                len: 0,
            })
        }
        None => -ENOENT,
    }
}

extern "C" fn oxfs_read(fd: u64, ptr: u64, len: u64) -> i64 {

    let Some(file) = find_open_file(fd) else {
        return -EBADF;
    };
    match file {
        OpenFile::FileRead { inode, position } => {
            // SAFETY: same trust boundary as elsewhere -- caller-owned pointer/length.
            let out = unsafe { core::slice::from_raw_parts_mut(ptr as *mut u8, len as usize) };
            let n = read_inode_at(*inode, *position, out);
            *position += n;
            n as i64
        }
        OpenFile::DirListing {
            content,
            len: total,
            position,
            ..
        } => {
            let remaining = *total - *position;
            let n = remaining.min(len as usize);
            let out = unsafe { core::slice::from_raw_parts_mut(ptr as *mut u8, n) };
            out.copy_from_slice(&content[*position..*position + n]);
            *position += n;
            n as i64
        }
        OpenFile::Write { .. } => -EBADF,
    }
}

extern "C" fn oxfs_write(fd: u64, ptr: u64, len: u64) -> i64 {

    let Some(file) = find_open_file(fd) else {
        return -EBADF;
    };
    match file {
        OpenFile::Write {
            buffer,
            len: buf_len,
            ..
        } => {
            let available = MAX_WRITE_BUFFER - *buf_len;
            let n = available.min(len as usize);
            if n == 0 && len > 0 {
                return -ENOSPC;
            }
            // SAFETY: same trust boundary as elsewhere -- caller-owned pointer/length.
            let src = unsafe { core::slice::from_raw_parts(ptr as *const u8, n) };
            buffer[*buf_len..*buf_len + n].copy_from_slice(src);
            *buf_len += n;
            n as i64
        }
        _ => -EBADF,
    }
}

/// Registered as `fd`'s close callback via `oxidebsd_register_fd_ops`. For a file opened for
/// writing, this is the only point its accumulated buffer is actually committed to a real inode
/// (same all-at-once-on-close model `modules/fat32` already uses).
extern "C" fn oxfs_close(fd: u64) -> i64 {

    let slots = unsafe { &mut *core::ptr::addr_of_mut!(OPEN_FILES) };
    let Some(slot) = slots
        .iter_mut()
        .find(|s| matches!(s, Some((slot_fd, _)) if *slot_fd == fd))
    else {
        return -EBADF;
    };
    let (_, file) = slot.take().expect("just matched Some above");

    if let OpenFile::Write {
        parent_inode,
        name,
        name_len,
        buffer,
        len,
    } = file
    {
        let Some(new_inode) = alloc_inode() else {
            return -ENOSPC;
        };
        write_inode(new_inode, Inode::new(InodeKind::File));
        if !write_inode_data(new_inode, &buffer[..len]) {
            return -EIO;
        }
        return match dir_insert(parent_inode, &name[..name_len as usize], new_inode) {
            Ok(()) => 0,
            Err(e) => errno_for(e),
        };
    }
    0
}

/// Registered for `SYS_CLOSE`. Delegates to the kernel's own `oxidebsd_close_fd`, which removes
/// `fd` from its registry and invokes `oxfs_close` above -- not a direct call, so a closed fd is
/// also no longer reachable via `SYS_READ`/`SYS_WRITE` afterward.
extern "C" fn sys_close(fd: u64, _a1: u64, _a2: u64, _a3: u64) -> i64 {
    // SAFETY: FFI call to a kernel-exported function, matching its declared signature exactly.
    unsafe { oxidebsd_close_fd(fd) as i64 }
}

/// Registered for `SYS_CHDIR`. `resolve_path` already handles every case `chdir` needs
/// (`""`/`"."`/`".."`/`"/"`/a multi-component path) uniformly -- no separate resolver needed the
/// way `modules/fat32`'s own single-component-only grammar required.
extern "C" fn oxfs_chdir(path_ptr: u64, path_len: u64, _a2: u64, _a3: u64) -> i64 {

    // SAFETY: same trust boundary as elsewhere -- caller-owned pointer/length.
    let path = unsafe { core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize) };
    let cwd = current_cwd();
    match resolve_path(cwd, path) {
        Ok(inode_num) if read_inode(inode_num).kind == InodeKind::Dir => {
            set_current_cwd(inode_num);
            0
        }
        Ok(_) => -ENOTDIR,
        Err(e) => errno_for(e),
    }
}

/// Registered for `SYS_GETCWD`. Same wire format as `modules/fat32`'s own `sys_getcwd` (a
/// NUL-terminated string written into `buf`, byte count including the NUL on success, `-ERANGE`
/// if `buf_len` is too small).
extern "C" fn oxfs_getcwd(buf_ptr: u64, buf_len: u64, _a2: u64, _a3: u64) -> i64 {
    let mut path = [0u8; MAX_CWD_PATH];
    let len = build_cwd_path(current_cwd(), &mut path);

    if buf_len == 0 || (len as u64) + 1 > buf_len {
        return -ERANGE;
    }

    // SAFETY: same trust boundary as elsewhere -- caller-owned pointer/length.
    let out = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, buf_len as usize) };
    out[..len].copy_from_slice(&path[..len]);
    out[len] = 0;
    (len + 1) as i64
}

/// Registered for `SYS_MKDIR`. `path` may now be multi-component (`sub/nested`, as long as `sub`
/// already exists) -- `resolve_parent` handles that the same way it does for `open`'s `O_CREAT`
/// case.
extern "C" fn oxfs_mkdir(path_ptr: u64, path_len: u64, _a2: u64, _a3: u64) -> i64 {

    // SAFETY: same trust boundary as elsewhere -- caller-owned pointer/length.
    let path = unsafe { core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize) };
    let cwd = current_cwd();

    let (parent, leaf) = match resolve_parent(cwd, path) {
        Ok(v) => v,
        Err(e) => return errno_for(e),
    };
    if dir_lookup(parent, leaf).is_some() {
        return -EEXIST;
    }
    let Some(new_inode) = alloc_inode() else {
        return -ENOSPC;
    };
    write_inode(new_inode, Inode::new(InodeKind::Dir));
    if dir_insert(new_inode, b".", new_inode).is_err()
        || dir_insert(new_inode, b"..", parent).is_err()
    {
        return -ENOSPC;
    }
    match dir_insert(parent, leaf, new_inode) {
        Ok(()) => 0,
        Err(e) => errno_for(e),
    }
}

/// Registered for `SYS_UNLINK`. Refuses to unlink a directory (`EISDIR` -- use `SYS_RMDIR`
/// instead, matching real Unix convention). The removed record's inode/blocks are not freed (see
/// the module doc comment).
extern "C" fn oxfs_unlink(path_ptr: u64, path_len: u64, _a2: u64, _a3: u64) -> i64 {

    // SAFETY: same trust boundary as elsewhere -- caller-owned pointer/length.
    let path = unsafe { core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize) };
    let cwd = current_cwd();

    let (parent, leaf) = match resolve_parent(cwd, path) {
        Ok(v) => v,
        Err(e) => return errno_for(e),
    };
    let Some(target) = dir_lookup(parent, leaf) else {
        return -ENOENT;
    };
    if read_inode(target).kind == InodeKind::Dir {
        return -EISDIR;
    }
    match dir_remove(parent, leaf) {
        Ok(()) => 0,
        Err(e) => errno_for(e),
    }
}

/// Registered for `SYS_RMDIR`. Only succeeds on an empty directory (`.`/`..` excepted, via
/// `dir_entry_count`).
extern "C" fn oxfs_rmdir(path_ptr: u64, path_len: u64, _a2: u64, _a3: u64) -> i64 {

    // SAFETY: same trust boundary as elsewhere -- caller-owned pointer/length.
    let path = unsafe { core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize) };
    let cwd = current_cwd();

    let (parent, leaf) = match resolve_parent(cwd, path) {
        Ok(v) => v,
        Err(e) => return errno_for(e),
    };
    let Some(target) = dir_lookup(parent, leaf) else {
        return -ENOENT;
    };
    if read_inode(target).kind != InodeKind::Dir {
        return -ENOTDIR;
    }
    if dir_entry_count(target) > 2 {
        return -ENOTEMPTY;
    }
    match dir_remove(parent, leaf) {
        Ok(()) => 0,
        Err(e) => errno_for(e),
    }
}

/// Registered for `SYS_RENAME`. `(old_ptr, old_len, new_ptr, new_len)` -- uses all four of this
/// ABI's argument registers (see the module doc comment). Overwriting an existing plain file at
/// `new` is allowed (its old record is cleared first, its inode leaked like every other removal
/// here); overwriting an existing directory is refused (`EISDIR`, kept simple rather than
/// implementing real directory-replace semantics).
extern "C" fn oxfs_rename(old_ptr: u64, old_len: u64, new_ptr: u64, new_len: u64) -> i64 {

    // SAFETY: same trust boundary as elsewhere -- caller-owned pointer/length.
    let old_path = unsafe { core::slice::from_raw_parts(old_ptr as *const u8, old_len as usize) };
    let new_path = unsafe { core::slice::from_raw_parts(new_ptr as *const u8, new_len as usize) };
    let cwd = current_cwd();

    let (old_parent, old_leaf) = match resolve_parent(cwd, old_path) {
        Ok(v) => v,
        Err(e) => return errno_for(e),
    };
    let Some(target) = dir_lookup(old_parent, old_leaf) else {
        return -ENOENT;
    };
    let (new_parent, new_leaf) = match resolve_parent(cwd, new_path) {
        Ok(v) => v,
        Err(e) => return errno_for(e),
    };
    if let Some(existing) = dir_lookup(new_parent, new_leaf) {
        if read_inode(existing).kind == InodeKind::Dir {
            return -EISDIR;
        }
        let _ = dir_remove(new_parent, new_leaf);
    }
    if dir_remove(old_parent, old_leaf).is_err() {
        return -EIO;
    }
    match dir_insert(new_parent, new_leaf, target) {
        Ok(()) => 0,
        Err(e) => {
            // Best-effort rollback so a failed rename doesn't just lose the entry outright.
            let _ = dir_insert(old_parent, old_leaf, target);
            errno_for(e)
        }
    }
}

/// Registered for `SYS_STAT`. No symlinks exist in this filesystem at all, so unlike real
/// `stat`/`lstat`, there's no "follow the final component" distinction to make -- `oxfs_lstat`
/// below just calls this same resolver.
extern "C" fn oxfs_stat(path_ptr: u64, path_len: u64, buf_ptr: u64, _r10: u64) -> i64 {
    // SAFETY: same trust boundary as elsewhere -- caller-owned pointer/length.
    let path = unsafe { core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize) };
    let cwd = current_cwd();
    match resolve_path(cwd, path) {
        Ok(inode_num) => write_stat(inode_num, buf_ptr),
        Err(e) => errno_for(e),
    }
}

/// Registered for `SYS_LSTAT` -- see `oxfs_stat`'s own doc comment for why this is just an alias
/// rather than a separate implementation.
extern "C" fn oxfs_lstat(path_ptr: u64, path_len: u64, buf_ptr: u64, r10: u64) -> i64 {
    oxfs_stat(path_ptr, path_len, buf_ptr, r10)
}

/// Registered for `SYS_FSTAT`. `fd` here is the calling *process's* own fd number, not this
/// module's `real_fd` -- `oxidebsd_real_fd_of` (see its own doc comment in `src/fd.rs`) resolves
/// that first, the same way `SYS_READ`/`SYS_WRITE` get it resolved for them automatically by
/// `crate::fd::read`/`write` before ever reaching a registered callback.
extern "C" fn oxfs_fstat(fd: u64, buf_ptr: u64, _a2: u64, _a3: u64) -> i64 {
    // SAFETY: FFI call to a kernel-exported function, matching its declared signature exactly.
    let real_fd = unsafe { oxidebsd_real_fd_of(fd) };
    if real_fd < 0 {
        return -EBADF;
    }
    match inode_of_open_file(real_fd as u64) {
        Some(inode_num) => write_stat(inode_num, buf_ptr),
        None => -EBADF,
    }
}

/// Registered for `SYS_GETDENTS`. `fd` is the calling process's own fd number, resolved to this
/// module's `real_fd` the same way `oxfs_fstat` does (see its own doc comment). Fills as many
/// whole records as fit in `buf_len` starting from the open directory's own resume cursor
/// (`OpenFile::DirListing::dirent_pos`), returns the byte count actually written (`0` once every
/// record has already been emitted -- real `getdents(2)`'s own EOF convention, which `readdir()`
/// relies on to stop looping). A record that doesn't fully fit is left for the next call rather
/// than truncated -- matching real Linux, which never splits a record across two `getdents` calls.
extern "C" fn oxfs_getdents(fd: u64, buf_ptr: u64, buf_len: u64, _a3: u64) -> i64 {
    // SAFETY: FFI call to a kernel-exported function, matching its declared signature exactly.
    let real_fd = unsafe { oxidebsd_real_fd_of(fd) };
    if real_fd < 0 {
        return -EBADF;
    }
    let Some(file) = find_open_file(real_fd as u64) else {
        return -EBADF;
    };
    let OpenFile::DirListing {
        inode: dir_inode,
        dirent_pos,
        ..
    } = file
    else {
        return -ENOTDIR;
    };
    let dir_inode = *dir_inode;

    // SAFETY: same trust boundary as elsewhere -- caller-owned pointer/length.
    let out = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, buf_len as usize) };
    let mut written = 0usize;
    while let Some((child_inode, name, name_len)) = dir_nth_used_record(dir_inode, *dirent_pos) {
        let name = &name[..name_len as usize];
        let reclen = dirent_record_len(name.len());
        if written + reclen > out.len() {
            break;
        }
        let dtype = if read_inode(child_inode).kind == InodeKind::Dir {
            DT_DIR
        } else {
            DT_REG
        };
        write_dirent_record(
            &mut out[written..written + reclen],
            child_inode as u64,
            (*dirent_pos + 1) as i64,
            dtype,
            name,
        );
        written += reclen;
        *dirent_pos += 1;
    }
    written as i64
}

fn log_bytes(bytes: &[u8]) {
    unsafe { oxidebsd_log(bytes.as_ptr(), bytes.len() as u64) };
}

fn log(message: &str) {
    log_bytes(message.as_bytes());
}

/// A minimal, `core::fmt`-free byte-buffer builder -- see `modules/fat32`'s own doc comment for
/// why module code avoids `core::fmt::Write`/`write!` entirely (it reintroduces `GOTPCREL`
/// relocations and pulls in a large fraction of `core::fmt`'s tables).
struct ByteBuf<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl ByteBuf<'_> {
    fn push_bytes(&mut self, bytes: &[u8]) {
        let available = self.buf.len() - self.len;
        let n = bytes.len().min(available);
        self.buf[self.len..self.len + n].copy_from_slice(&bytes[..n]);
        self.len += n;
    }

    fn push_decimal(&mut self, value: u32) {
        if value == 0 {
            self.push_bytes(b"0");
            return;
        }
        let mut digits = [0u8; 10];
        let mut count = 0;
        let mut remaining = value;
        while remaining > 0 {
            digits[count] = b'0' + (remaining % 10) as u8;
            remaining /= 10;
            count += 1;
        }
        digits[..count].reverse();
        self.push_bytes(&digits[..count]);
    }
}

/// Allocates a fresh file inode under `parent` named `name` with `content` as its complete
/// contents -- the `module_init`-time equivalent of `open(O_CREAT)` + `write` + `close`, used to
/// seed every embedded file without going through the fd/syscall machinery.
fn seed_file(parent: u32, name: &[u8], content: &[u8]) -> bool {
    let Some(inode) = alloc_inode() else {
        return false;
    };
    write_inode(inode, Inode::new(InodeKind::File));
    write_inode_data(inode, content) && dir_insert(parent, name, inode).is_ok()
}

#[unsafe(no_mangle)]
pub extern "C" fn module_init() -> i32 {
    let root = alloc_inode().expect("oxfs: failed to allocate root inode");
    debug_assert_eq!(
        root, ROOT_INODE,
        "oxfs: root must be the first inode allocated"
    );
    write_inode(root, Inode::new(InodeKind::Dir));
    dir_insert(root, b".", root).expect("oxfs: failed to seed root's . entry");
    dir_insert(root, b"..", root).expect("oxfs: failed to seed root's .. entry");

    let mut ok = true;

    ok &= seed_file(
        root,
        b"hello.txt",
        b"Hello from OxideBSD's own filesystem!\n",
    );

    // Formula-derived, not a literal -- so the self-check below can independently recompute the
    // expected bytes, same idiom modules/fat32's own self-check already established.
    let mut big = [0u8; BIG_FILE_LEN];
    for (i, b) in big.iter_mut().enumerate() {
        *b = b'A' + (i % 26) as u8;
    }
    ok &= seed_file(root, b"big.txt", &big);

    ok &= seed_file(
        root,
        b"smoke.elf",
        include_bytes!(env!("OXFS_SMOKE_ELF_PATH")),
    );
    ok &= seed_file(
        root,
        b"musl.elf",
        include_bytes!(env!("OXFS_MUSL_ELF_PATH")),
    );
    ok &= seed_file(
        root,
        b"true.elf",
        include_bytes!(env!("OXFS_TRUE_ELF_PATH")),
    );
    ok &= seed_file(
        root,
        b"echo.elf",
        include_bytes!(env!("OXFS_ECHO_ELF_PATH")),
    );
    ok &= seed_file(root, b"cat.elf", include_bytes!(env!("OXFS_CAT_ELF_PATH")));
    ok &= seed_file(root, b"sh.elf", include_bytes!(env!("OXFS_HUSH_ELF_PATH")));
    ok &= seed_file(
        root,
        b"false.elf",
        include_bytes!(env!("OXFS_FALSE_ELF_PATH")),
    );
    ok &= seed_file(root, b"yes.elf", include_bytes!(env!("OXFS_YES_ELF_PATH")));
    ok &= seed_file(
        root,
        b"more.elf",
        include_bytes!(env!("OXFS_MORE_ELF_PATH")),
    );
    ok &= seed_file(
        root,
        b"mkdir.elf",
        include_bytes!(env!("OXFS_MKDIR_ELF_PATH")),
    );
    ok &= seed_file(
        root,
        b"rmdir.elf",
        include_bytes!(env!("OXFS_RMDIR_ELF_PATH")),
    );
    ok &= seed_file(root, b"rm.elf", include_bytes!(env!("OXFS_RM_ELF_PATH")));
    ok &= seed_file(root, b"mv.elf", include_bytes!(env!("OXFS_MV_ELF_PATH")));
    ok &= seed_file(root, b"cp.elf", include_bytes!(env!("OXFS_CP_ELF_PATH")));
    ok &= seed_file(
        root,
        b"touch.elf",
        include_bytes!(env!("OXFS_TOUCH_ELF_PATH")),
    );
    ok &= seed_file(
        root,
        b"head.elf",
        include_bytes!(env!("OXFS_HEAD_ELF_PATH")),
    );
    ok &= seed_file(
        root,
        b"tail.elf",
        include_bytes!(env!("OXFS_TAIL_ELF_PATH")),
    );
    ok &= seed_file(root, b"wc.elf", include_bytes!(env!("OXFS_WC_ELF_PATH")));
    ok &= seed_file(
        root,
        b"basename.elf",
        include_bytes!(env!("OXFS_BASENAME_ELF_PATH")),
    );
    ok &= seed_file(
        root,
        b"dirname.elf",
        include_bytes!(env!("OXFS_DIRNAME_ELF_PATH")),
    );
    ok &= seed_file(
        root,
        b"printf.elf",
        include_bytes!(env!("OXFS_PRINTF_ELF_PATH")),
    );
    ok &= seed_file(root, b"seq.elf", include_bytes!(env!("OXFS_SEQ_ELF_PATH")));
    ok &= seed_file(root, b"cut.elf", include_bytes!(env!("OXFS_CUT_ELF_PATH")));
    ok &= seed_file(
        root,
        b"sort.elf",
        include_bytes!(env!("OXFS_SORT_ELF_PATH")),
    );
    ok &= seed_file(
        root,
        b"uniq.elf",
        include_bytes!(env!("OXFS_UNIQ_ELF_PATH")),
    );
    ok &= seed_file(
        root,
        b"kill.elf",
        include_bytes!(env!("OXFS_KILL_ELF_PATH")),
    );

    // Second pass: every applet build.rs's own second-pass probe found buildable against this
    // musl port (see build.rs's own BUSYBOX_APPLETS_PASS2 comment and docs/BUSYBOX_APPLETS.md for
    // what each one actually needs at runtime -- most need something OxideBSD doesn't implement
    // yet; "builds" was the bar this pass used, not "works"). One-liner form (not the multi-line
    // seed_file(...) call the first 24 applets above use) purely because there are ~300 of these --
    // no behavioral difference.
    ok &= seed_file(root, b"addgroup.elf", include_bytes!(env!("OXFS_ADDGROUP_ELF_PATH")));
    ok &= seed_file(root, b"adduser.elf", include_bytes!(env!("OXFS_ADDUSER_ELF_PATH")));
    ok &= seed_file(root, b"adjtimex.elf", include_bytes!(env!("OXFS_ADJTIMEX_ELF_PATH")));
    ok &= seed_file(root, b"ar.elf", include_bytes!(env!("OXFS_AR_ELF_PATH")));
    ok &= seed_file(root, b"arp.elf", include_bytes!(env!("OXFS_ARP_ELF_PATH")));
    ok &= seed_file(root, b"arping.elf", include_bytes!(env!("OXFS_ARPING_ELF_PATH")));
    ok &= seed_file(root, b"ascii.elf", include_bytes!(env!("OXFS_ASCII_ELF_PATH")));
    ok &= seed_file(root, b"ash.elf", include_bytes!(env!("OXFS_ASH_ELF_PATH")));
    ok &= seed_file(root, b"awk.elf", include_bytes!(env!("OXFS_AWK_ELF_PATH")));
    ok &= seed_file(root, b"base32.elf", include_bytes!(env!("OXFS_BASE32_ELF_PATH")));
    ok &= seed_file(root, b"base64.elf", include_bytes!(env!("OXFS_BASE64_ELF_PATH")));
    ok &= seed_file(root, b"bash_ash.elf", include_bytes!(env!("OXFS_BASH_ASH_ELF_PATH")));
    ok &= seed_file(root, b"bash.elf", include_bytes!(env!("OXFS_BASH_ELF_PATH")));
    ok &= seed_file(root, b"bbconfig.elf", include_bytes!(env!("OXFS_BBCONFIG_ELF_PATH")));
    ok &= seed_file(root, b"arch.elf", include_bytes!(env!("OXFS_ARCH_ELF_PATH")));
    ok &= seed_file(root, b"sysctl.elf", include_bytes!(env!("OXFS_SYSCTL_ELF_PATH")));
    ok &= seed_file(root, b"bc.elf", include_bytes!(env!("OXFS_BC_ELF_PATH")));
    ok &= seed_file(root, b"blkid.elf", include_bytes!(env!("OXFS_BLKID_ELF_PATH")));
    ok &= seed_file(root, b"bootchartd.elf", include_bytes!(env!("OXFS_BOOTCHARTD_ELF_PATH")));
    ok &= seed_file(root, b"bunzip2.elf", include_bytes!(env!("OXFS_BUNZIP2_ELF_PATH")));
    ok &= seed_file(root, b"bzcat.elf", include_bytes!(env!("OXFS_BZCAT_ELF_PATH")));
    ok &= seed_file(root, b"bzip2.elf", include_bytes!(env!("OXFS_BZIP2_ELF_PATH")));
    ok &= seed_file(root, b"cal.elf", include_bytes!(env!("OXFS_CAL_ELF_PATH")));
    ok &= seed_file(root, b"chat.elf", include_bytes!(env!("OXFS_CHAT_ELF_PATH")));
    ok &= seed_file(root, b"chattr.elf", include_bytes!(env!("OXFS_CHATTR_ELF_PATH")));
    ok &= seed_file(root, b"chgrp.elf", include_bytes!(env!("OXFS_CHGRP_ELF_PATH")));
    ok &= seed_file(root, b"chmod.elf", include_bytes!(env!("OXFS_CHMOD_ELF_PATH")));
    ok &= seed_file(root, b"chown.elf", include_bytes!(env!("OXFS_CHOWN_ELF_PATH")));
    ok &= seed_file(root, b"chpasswd.elf", include_bytes!(env!("OXFS_CHPASSWD_ELF_PATH")));
    ok &= seed_file(root, b"chroot.elf", include_bytes!(env!("OXFS_CHROOT_ELF_PATH")));
    ok &= seed_file(root, b"chrt.elf", include_bytes!(env!("OXFS_CHRT_ELF_PATH")));
    ok &= seed_file(root, b"chvt.elf", include_bytes!(env!("OXFS_CHVT_ELF_PATH")));
    ok &= seed_file(root, b"cksum.elf", include_bytes!(env!("OXFS_CKSUM_ELF_PATH")));
    ok &= seed_file(root, b"clear.elf", include_bytes!(env!("OXFS_CLEAR_ELF_PATH")));
    ok &= seed_file(root, b"cmp.elf", include_bytes!(env!("OXFS_CMP_ELF_PATH")));
    ok &= seed_file(root, b"comm.elf", include_bytes!(env!("OXFS_COMM_ELF_PATH")));
    ok &= seed_file(root, b"cpio.elf", include_bytes!(env!("OXFS_CPIO_ELF_PATH")));
    ok &= seed_file(root, b"crc32.elf", include_bytes!(env!("OXFS_CRC32_ELF_PATH")));
    ok &= seed_file(root, b"crond.elf", include_bytes!(env!("OXFS_CROND_ELF_PATH")));
    ok &= seed_file(root, b"crontab.elf", include_bytes!(env!("OXFS_CRONTAB_ELF_PATH")));
    ok &= seed_file(root, b"cttyhack.elf", include_bytes!(env!("OXFS_CTTYHACK_ELF_PATH")));
    ok &= seed_file(root, b"date.elf", include_bytes!(env!("OXFS_DATE_ELF_PATH")));
    ok &= seed_file(root, b"dc.elf", include_bytes!(env!("OXFS_DC_ELF_PATH")));
    ok &= seed_file(root, b"dd.elf", include_bytes!(env!("OXFS_DD_ELF_PATH")));
    ok &= seed_file(root, b"deallocvt.elf", include_bytes!(env!("OXFS_DEALLOCVT_ELF_PATH")));
    ok &= seed_file(root, b"delgroup.elf", include_bytes!(env!("OXFS_DELGROUP_ELF_PATH")));
    ok &= seed_file(root, b"devfsd.elf", include_bytes!(env!("OXFS_DEVFSD_ELF_PATH")));
    ok &= seed_file(root, b"devmem.elf", include_bytes!(env!("OXFS_DEVMEM_ELF_PATH")));
    ok &= seed_file(root, b"df.elf", include_bytes!(env!("OXFS_DF_ELF_PATH")));
    ok &= seed_file(root, b"dhcprelay.elf", include_bytes!(env!("OXFS_DHCPRELAY_ELF_PATH")));
    ok &= seed_file(root, b"diff.elf", include_bytes!(env!("OXFS_DIFF_ELF_PATH")));
    ok &= seed_file(root, b"dmesg.elf", include_bytes!(env!("OXFS_DMESG_ELF_PATH")));
    ok &= seed_file(root, b"dnsd.elf", include_bytes!(env!("OXFS_DNSD_ELF_PATH")));
    ok &= seed_file(root, b"dnsdomainname.elf", include_bytes!(env!("OXFS_DNSDOMAINNAME_ELF_PATH")));
    ok &= seed_file(root, b"dos2unix.elf", include_bytes!(env!("OXFS_DOS2UNIX_ELF_PATH")));
    ok &= seed_file(root, b"dpkg.elf", include_bytes!(env!("OXFS_DPKG_ELF_PATH")));
    ok &= seed_file(root, b"dpkg_deb.elf", include_bytes!(env!("OXFS_DPKG_DEB_ELF_PATH")));
    ok &= seed_file(root, b"du.elf", include_bytes!(env!("OXFS_DU_ELF_PATH")));
    ok &= seed_file(root, b"dumpkmap.elf", include_bytes!(env!("OXFS_DUMPKMAP_ELF_PATH")));
    ok &= seed_file(root, b"dumpleases.elf", include_bytes!(env!("OXFS_DUMPLEASES_ELF_PATH")));
    ok &= seed_file(root, b"ed.elf", include_bytes!(env!("OXFS_ED_ELF_PATH")));
    ok &= seed_file(root, b"egrep.elf", include_bytes!(env!("OXFS_EGREP_ELF_PATH")));
    ok &= seed_file(root, b"eject.elf", include_bytes!(env!("OXFS_EJECT_ELF_PATH")));
    ok &= seed_file(root, b"env.elf", include_bytes!(env!("OXFS_ENV_ELF_PATH")));
    ok &= seed_file(root, b"envuidgid.elf", include_bytes!(env!("OXFS_ENVUIDGID_ELF_PATH")));
    ok &= seed_file(root, b"expand.elf", include_bytes!(env!("OXFS_EXPAND_ELF_PATH")));
    ok &= seed_file(root, b"expr.elf", include_bytes!(env!("OXFS_EXPR_ELF_PATH")));
    ok &= seed_file(root, b"factor.elf", include_bytes!(env!("OXFS_FACTOR_ELF_PATH")));
    ok &= seed_file(root, b"fakeidentd.elf", include_bytes!(env!("OXFS_FAKEIDENTD_ELF_PATH")));
    ok &= seed_file(root, b"fallocate.elf", include_bytes!(env!("OXFS_FALLOCATE_ELF_PATH")));
    ok &= seed_file(root, b"fatattr.elf", include_bytes!(env!("OXFS_FATATTR_ELF_PATH")));
    ok &= seed_file(root, b"fbset.elf", include_bytes!(env!("OXFS_FBSET_ELF_PATH")));
    ok &= seed_file(root, b"fdformat.elf", include_bytes!(env!("OXFS_FDFORMAT_ELF_PATH")));
    ok &= seed_file(root, b"fdisk.elf", include_bytes!(env!("OXFS_FDISK_ELF_PATH")));
    ok &= seed_file(root, b"fgconsole.elf", include_bytes!(env!("OXFS_FGCONSOLE_ELF_PATH")));
    ok &= seed_file(root, b"fgrep.elf", include_bytes!(env!("OXFS_FGREP_ELF_PATH")));
    ok &= seed_file(root, b"find.elf", include_bytes!(env!("OXFS_FIND_ELF_PATH")));
    ok &= seed_file(root, b"findfs.elf", include_bytes!(env!("OXFS_FINDFS_ELF_PATH")));
    ok &= seed_file(root, b"flock.elf", include_bytes!(env!("OXFS_FLOCK_ELF_PATH")));
    ok &= seed_file(root, b"fold.elf", include_bytes!(env!("OXFS_FOLD_ELF_PATH")));
    ok &= seed_file(root, b"free.elf", include_bytes!(env!("OXFS_FREE_ELF_PATH")));
    ok &= seed_file(root, b"freeramdisk.elf", include_bytes!(env!("OXFS_FREERAMDISK_ELF_PATH")));
    ok &= seed_file(root, b"fsck.elf", include_bytes!(env!("OXFS_FSCK_ELF_PATH")));
    ok &= seed_file(root, b"fsck_minix.elf", include_bytes!(env!("OXFS_FSCK_MINIX_ELF_PATH")));
    ok &= seed_file(root, b"fsync.elf", include_bytes!(env!("OXFS_FSYNC_ELF_PATH")));
    ok &= seed_file(root, b"ftpd.elf", include_bytes!(env!("OXFS_FTPD_ELF_PATH")));
    ok &= seed_file(root, b"ftpget.elf", include_bytes!(env!("OXFS_FTPGET_ELF_PATH")));
    ok &= seed_file(root, b"ftpput.elf", include_bytes!(env!("OXFS_FTPPUT_ELF_PATH")));
    ok &= seed_file(root, b"fuser.elf", include_bytes!(env!("OXFS_FUSER_ELF_PATH")));
    ok &= seed_file(root, b"getopt.elf", include_bytes!(env!("OXFS_GETOPT_ELF_PATH")));
    ok &= seed_file(root, b"getty.elf", include_bytes!(env!("OXFS_GETTY_ELF_PATH")));
    ok &= seed_file(root, b"grep.elf", include_bytes!(env!("OXFS_GREP_ELF_PATH")));
    ok &= seed_file(root, b"groups.elf", include_bytes!(env!("OXFS_GROUPS_ELF_PATH")));
    ok &= seed_file(root, b"gunzip.elf", include_bytes!(env!("OXFS_GUNZIP_ELF_PATH")));
    ok &= seed_file(root, b"gzip.elf", include_bytes!(env!("OXFS_GZIP_ELF_PATH")));
    ok &= seed_file(root, b"halt.elf", include_bytes!(env!("OXFS_HALT_ELF_PATH")));
    ok &= seed_file(root, b"hd.elf", include_bytes!(env!("OXFS_HD_ELF_PATH")));
    ok &= seed_file(root, b"hexdump.elf", include_bytes!(env!("OXFS_HEXDUMP_ELF_PATH")));
    ok &= seed_file(root, b"hexedit.elf", include_bytes!(env!("OXFS_HEXEDIT_ELF_PATH")));
    ok &= seed_file(root, b"hostid.elf", include_bytes!(env!("OXFS_HOSTID_ELF_PATH")));
    ok &= seed_file(root, b"httpd.elf", include_bytes!(env!("OXFS_HTTPD_ELF_PATH")));
    ok &= seed_file(root, b"hwclock.elf", include_bytes!(env!("OXFS_HWCLOCK_ELF_PATH")));
    ok &= seed_file(root, b"ifconfig.elf", include_bytes!(env!("OXFS_IFCONFIG_ELF_PATH")));
    ok &= seed_file(root, b"ifdown.elf", include_bytes!(env!("OXFS_IFDOWN_ELF_PATH")));
    ok &= seed_file(root, b"inetd.elf", include_bytes!(env!("OXFS_INETD_ELF_PATH")));
    ok &= seed_file(root, b"inotifyd.elf", include_bytes!(env!("OXFS_INOTIFYD_ELF_PATH")));
    ok &= seed_file(root, b"install.elf", include_bytes!(env!("OXFS_INSTALL_ELF_PATH")));
    ok &= seed_file(root, b"iostat.elf", include_bytes!(env!("OXFS_IOSTAT_ELF_PATH")));
    ok &= seed_file(root, b"ipcalc.elf", include_bytes!(env!("OXFS_IPCALC_ELF_PATH")));
    ok &= seed_file(root, b"ipcrm.elf", include_bytes!(env!("OXFS_IPCRM_ELF_PATH")));
    ok &= seed_file(root, b"ipcs.elf", include_bytes!(env!("OXFS_IPCS_ELF_PATH")));
    ok &= seed_file(root, b"killall5.elf", include_bytes!(env!("OXFS_KILLALL5_ELF_PATH")));
    ok &= seed_file(root, b"klogd.elf", include_bytes!(env!("OXFS_KLOGD_ELF_PATH")));
    ok &= seed_file(root, b"less.elf", include_bytes!(env!("OXFS_LESS_ELF_PATH")));
    ok &= seed_file(root, b"link.elf", include_bytes!(env!("OXFS_LINK_ELF_PATH")));
    ok &= seed_file(root, b"linux32.elf", include_bytes!(env!("OXFS_LINUX32_ELF_PATH")));
    ok &= seed_file(root, b"linux64.elf", include_bytes!(env!("OXFS_LINUX64_ELF_PATH")));
    ok &= seed_file(root, b"ln.elf", include_bytes!(env!("OXFS_LN_ELF_PATH")));
    ok &= seed_file(root, b"loadkmap.elf", include_bytes!(env!("OXFS_LOADKMAP_ELF_PATH")));
    ok &= seed_file(root, b"logger.elf", include_bytes!(env!("OXFS_LOGGER_ELF_PATH")));
    ok &= seed_file(root, b"login.elf", include_bytes!(env!("OXFS_LOGIN_ELF_PATH")));
    ok &= seed_file(root, b"logname.elf", include_bytes!(env!("OXFS_LOGNAME_ELF_PATH")));
    ok &= seed_file(root, b"logread.elf", include_bytes!(env!("OXFS_LOGREAD_ELF_PATH")));
    ok &= seed_file(root, b"lpd.elf", include_bytes!(env!("OXFS_LPD_ELF_PATH")));
    ok &= seed_file(root, b"lpq.elf", include_bytes!(env!("OXFS_LPQ_ELF_PATH")));
    ok &= seed_file(root, b"lpr.elf", include_bytes!(env!("OXFS_LPR_ELF_PATH")));
    ok &= seed_file(root, b"ls.elf", include_bytes!(env!("OXFS_LS_ELF_PATH")));
    ok &= seed_file(root, b"lsattr.elf", include_bytes!(env!("OXFS_LSATTR_ELF_PATH")));
    ok &= seed_file(root, b"lsof.elf", include_bytes!(env!("OXFS_LSOF_ELF_PATH")));
    ok &= seed_file(root, b"lspci.elf", include_bytes!(env!("OXFS_LSPCI_ELF_PATH")));
    ok &= seed_file(root, b"lsscsi.elf", include_bytes!(env!("OXFS_LSSCSI_ELF_PATH")));
    ok &= seed_file(root, b"lsusb.elf", include_bytes!(env!("OXFS_LSUSB_ELF_PATH")));
    ok &= seed_file(root, b"lzcat.elf", include_bytes!(env!("OXFS_LZCAT_ELF_PATH")));
    ok &= seed_file(root, b"lzop.elf", include_bytes!(env!("OXFS_LZOP_ELF_PATH")));
    ok &= seed_file(root, b"makedevs.elf", include_bytes!(env!("OXFS_MAKEDEVS_ELF_PATH")));
    ok &= seed_file(root, b"makemime.elf", include_bytes!(env!("OXFS_MAKEMIME_ELF_PATH")));
    ok &= seed_file(root, b"man.elf", include_bytes!(env!("OXFS_MAN_ELF_PATH")));
    ok &= seed_file(root, b"md5sum.elf", include_bytes!(env!("OXFS_MD5SUM_ELF_PATH")));
    ok &= seed_file(root, b"mesg.elf", include_bytes!(env!("OXFS_MESG_ELF_PATH")));
    ok &= seed_file(root, b"microcom.elf", include_bytes!(env!("OXFS_MICROCOM_ELF_PATH")));
    ok &= seed_file(root, b"minips.elf", include_bytes!(env!("OXFS_MINIPS_ELF_PATH")));
    ok &= seed_file(root, b"mkfifo.elf", include_bytes!(env!("OXFS_MKFIFO_ELF_PATH")));
    ok &= seed_file(root, b"mkfs.elf", include_bytes!(env!("OXFS_MKFS_ELF_PATH")));
    ok &= seed_file(root, b"mknod.elf", include_bytes!(env!("OXFS_MKNOD_ELF_PATH")));
    ok &= seed_file(root, b"mkpasswd.elf", include_bytes!(env!("OXFS_MKPASSWD_ELF_PATH")));
    ok &= seed_file(root, b"mkswap.elf", include_bytes!(env!("OXFS_MKSWAP_ELF_PATH")));
    ok &= seed_file(root, b"mktemp.elf", include_bytes!(env!("OXFS_MKTEMP_ELF_PATH")));
    ok &= seed_file(root, b"modinfo.elf", include_bytes!(env!("OXFS_MODINFO_ELF_PATH")));
    ok &= seed_file(root, b"mount.elf", include_bytes!(env!("OXFS_MOUNT_ELF_PATH")));
    ok &= seed_file(root, b"mountpoint.elf", include_bytes!(env!("OXFS_MOUNTPOINT_ELF_PATH")));
    ok &= seed_file(root, b"mpstat.elf", include_bytes!(env!("OXFS_MPSTAT_ELF_PATH")));
    ok &= seed_file(root, b"mt.elf", include_bytes!(env!("OXFS_MT_ELF_PATH")));
    ok &= seed_file(root, b"nc.elf", include_bytes!(env!("OXFS_NC_ELF_PATH")));
    ok &= seed_file(root, b"netcat.elf", include_bytes!(env!("OXFS_NETCAT_ELF_PATH")));
    ok &= seed_file(root, b"netstat.elf", include_bytes!(env!("OXFS_NETSTAT_ELF_PATH")));
    ok &= seed_file(root, b"nice.elf", include_bytes!(env!("OXFS_NICE_ELF_PATH")));
    ok &= seed_file(root, b"nl.elf", include_bytes!(env!("OXFS_NL_ELF_PATH")));
    ok &= seed_file(root, b"nmeter.elf", include_bytes!(env!("OXFS_NMETER_ELF_PATH")));
    ok &= seed_file(root, b"nohup.elf", include_bytes!(env!("OXFS_NOHUP_ELF_PATH")));
    ok &= seed_file(root, b"nproc.elf", include_bytes!(env!("OXFS_NPROC_ELF_PATH")));
    ok &= seed_file(root, b"nsenter.elf", include_bytes!(env!("OXFS_NSENTER_ELF_PATH")));
    ok &= seed_file(root, b"nslookup.elf", include_bytes!(env!("OXFS_NSLOOKUP_ELF_PATH")));
    ok &= seed_file(root, b"ntpd.elf", include_bytes!(env!("OXFS_NTPD_ELF_PATH")));
    ok &= seed_file(root, b"nuke.elf", include_bytes!(env!("OXFS_NUKE_ELF_PATH")));
    ok &= seed_file(root, b"od.elf", include_bytes!(env!("OXFS_OD_ELF_PATH")));
    ok &= seed_file(root, b"passwd.elf", include_bytes!(env!("OXFS_PASSWD_ELF_PATH")));
    ok &= seed_file(root, b"paste.elf", include_bytes!(env!("OXFS_PASTE_ELF_PATH")));
    ok &= seed_file(root, b"patch.elf", include_bytes!(env!("OXFS_PATCH_ELF_PATH")));
    ok &= seed_file(root, b"pgrep.elf", include_bytes!(env!("OXFS_PGREP_ELF_PATH")));
    ok &= seed_file(root, b"pidof.elf", include_bytes!(env!("OXFS_PIDOF_ELF_PATH")));
    ok &= seed_file(root, b"ping.elf", include_bytes!(env!("OXFS_PING_ELF_PATH")));
    ok &= seed_file(root, b"pipe_progress.elf", include_bytes!(env!("OXFS_PIPE_PROGRESS_ELF_PATH")));
    ok &= seed_file(root, b"pivot_root.elf", include_bytes!(env!("OXFS_PIVOT_ROOT_ELF_PATH")));
    ok &= seed_file(root, b"pkill.elf", include_bytes!(env!("OXFS_PKILL_ELF_PATH")));
    ok &= seed_file(root, b"pmap.elf", include_bytes!(env!("OXFS_PMAP_ELF_PATH")));
    ok &= seed_file(root, b"popmaildir.elf", include_bytes!(env!("OXFS_POPMAILDIR_ELF_PATH")));
    ok &= seed_file(root, b"poweroff.elf", include_bytes!(env!("OXFS_POWEROFF_ELF_PATH")));
    ok &= seed_file(root, b"powertop.elf", include_bytes!(env!("OXFS_POWERTOP_ELF_PATH")));
    ok &= seed_file(root, b"printenv.elf", include_bytes!(env!("OXFS_PRINTENV_ELF_PATH")));
    ok &= seed_file(root, b"pscan.elf", include_bytes!(env!("OXFS_PSCAN_ELF_PATH")));
    ok &= seed_file(root, b"pstree.elf", include_bytes!(env!("OXFS_PSTREE_ELF_PATH")));
    ok &= seed_file(root, b"pwd.elf", include_bytes!(env!("OXFS_PWD_ELF_PATH")));
    ok &= seed_file(root, b"pwdx.elf", include_bytes!(env!("OXFS_PWDX_ELF_PATH")));
    ok &= seed_file(root, b"rdate.elf", include_bytes!(env!("OXFS_RDATE_ELF_PATH")));
    ok &= seed_file(root, b"rdev.elf", include_bytes!(env!("OXFS_RDEV_ELF_PATH")));
    ok &= seed_file(root, b"readlink.elf", include_bytes!(env!("OXFS_READLINK_ELF_PATH")));
    ok &= seed_file(root, b"readprofile.elf", include_bytes!(env!("OXFS_READPROFILE_ELF_PATH")));
    ok &= seed_file(root, b"realpath.elf", include_bytes!(env!("OXFS_REALPATH_ELF_PATH")));
    ok &= seed_file(root, b"reformime.elf", include_bytes!(env!("OXFS_REFORMIME_ELF_PATH")));
    ok &= seed_file(root, b"remove.elf", include_bytes!(env!("OXFS_REMOVE_ELF_PATH")));
    ok &= seed_file(root, b"renice.elf", include_bytes!(env!("OXFS_RENICE_ELF_PATH")));
    ok &= seed_file(root, b"reset.elf", include_bytes!(env!("OXFS_RESET_ELF_PATH")));
    ok &= seed_file(root, b"resize.elf", include_bytes!(env!("OXFS_RESIZE_ELF_PATH")));
    ok &= seed_file(root, b"resume.elf", include_bytes!(env!("OXFS_RESUME_ELF_PATH")));
    ok &= seed_file(root, b"rev.elf", include_bytes!(env!("OXFS_REV_ELF_PATH")));
    ok &= seed_file(root, b"route.elf", include_bytes!(env!("OXFS_ROUTE_ELF_PATH")));
    ok &= seed_file(root, b"rpm.elf", include_bytes!(env!("OXFS_RPM_ELF_PATH")));
    ok &= seed_file(root, b"rpm2cpio.elf", include_bytes!(env!("OXFS_RPM2CPIO_ELF_PATH")));
    ok &= seed_file(root, b"rtcwake.elf", include_bytes!(env!("OXFS_RTCWAKE_ELF_PATH")));
    ok &= seed_file(root, b"runsv.elf", include_bytes!(env!("OXFS_RUNSV_ELF_PATH")));
    ok &= seed_file(root, b"runsvdir.elf", include_bytes!(env!("OXFS_RUNSVDIR_ELF_PATH")));
    ok &= seed_file(root, b"run.elf", include_bytes!(env!("OXFS_RUN_ELF_PATH")));
    ok &= seed_file(root, b"rx.elf", include_bytes!(env!("OXFS_RX_ELF_PATH")));
    ok &= seed_file(root, b"script.elf", include_bytes!(env!("OXFS_SCRIPT_ELF_PATH")));
    ok &= seed_file(root, b"scriptreplay.elf", include_bytes!(env!("OXFS_SCRIPTREPLAY_ELF_PATH")));
    ok &= seed_file(root, b"sed.elf", include_bytes!(env!("OXFS_SED_ELF_PATH")));
    ok &= seed_file(root, b"sendmail.elf", include_bytes!(env!("OXFS_SENDMAIL_ELF_PATH")));
    ok &= seed_file(root, b"setarch.elf", include_bytes!(env!("OXFS_SETARCH_ELF_PATH")));
    ok &= seed_file(root, b"setconsole.elf", include_bytes!(env!("OXFS_SETCONSOLE_ELF_PATH")));
    ok &= seed_file(root, b"setfattr.elf", include_bytes!(env!("OXFS_SETFATTR_ELF_PATH")));
    ok &= seed_file(root, b"setkeycodes.elf", include_bytes!(env!("OXFS_SETKEYCODES_ELF_PATH")));
    ok &= seed_file(root, b"setlogcons.elf", include_bytes!(env!("OXFS_SETLOGCONS_ELF_PATH")));
    ok &= seed_file(root, b"setpriv.elf", include_bytes!(env!("OXFS_SETPRIV_ELF_PATH")));
    ok &= seed_file(root, b"setserial.elf", include_bytes!(env!("OXFS_SETSERIAL_ELF_PATH")));
    ok &= seed_file(root, b"setsid.elf", include_bytes!(env!("OXFS_SETSID_ELF_PATH")));
    ok &= seed_file(root, b"setuidgid.elf", include_bytes!(env!("OXFS_SETUIDGID_ELF_PATH")));
    ok &= seed_file(root, b"sha1sum.elf", include_bytes!(env!("OXFS_SHA1SUM_ELF_PATH")));
    ok &= seed_file(root, b"sha256sum.elf", include_bytes!(env!("OXFS_SHA256SUM_ELF_PATH")));
    ok &= seed_file(root, b"sha3sum.elf", include_bytes!(env!("OXFS_SHA3SUM_ELF_PATH")));
    ok &= seed_file(root, b"sha512sum.elf", include_bytes!(env!("OXFS_SHA512SUM_ELF_PATH")));
    ok &= seed_file(root, b"shred.elf", include_bytes!(env!("OXFS_SHRED_ELF_PATH")));
    ok &= seed_file(root, b"shuf.elf", include_bytes!(env!("OXFS_SHUF_ELF_PATH")));
    ok &= seed_file(root, b"sleep.elf", include_bytes!(env!("OXFS_SLEEP_ELF_PATH")));
    ok &= seed_file(root, b"smemcap.elf", include_bytes!(env!("OXFS_SMEMCAP_ELF_PATH")));
    ok &= seed_file(root, b"softlimit.elf", include_bytes!(env!("OXFS_SOFTLIMIT_ELF_PATH")));
    ok &= seed_file(root, b"split.elf", include_bytes!(env!("OXFS_SPLIT_ELF_PATH")));
    ok &= seed_file(root, b"ssl_client.elf", include_bytes!(env!("OXFS_SSL_CLIENT_ELF_PATH")));
    ok &= seed_file(root, b"start.elf", include_bytes!(env!("OXFS_START_ELF_PATH")));
    ok &= seed_file(root, b"stat.elf", include_bytes!(env!("OXFS_STAT_ELF_PATH")));
    ok &= seed_file(root, b"strings.elf", include_bytes!(env!("OXFS_STRINGS_ELF_PATH")));
    ok &= seed_file(root, b"stty.elf", include_bytes!(env!("OXFS_STTY_ELF_PATH")));
    ok &= seed_file(root, b"su.elf", include_bytes!(env!("OXFS_SU_ELF_PATH")));
    ok &= seed_file(root, b"sulogin.elf", include_bytes!(env!("OXFS_SULOGIN_ELF_PATH")));
    ok &= seed_file(root, b"sum.elf", include_bytes!(env!("OXFS_SUM_ELF_PATH")));
    ok &= seed_file(root, b"svlogd.elf", include_bytes!(env!("OXFS_SVLOGD_ELF_PATH")));
    ok &= seed_file(root, b"svok.elf", include_bytes!(env!("OXFS_SVOK_ELF_PATH")));
    ok &= seed_file(root, b"swapoff.elf", include_bytes!(env!("OXFS_SWAPOFF_ELF_PATH")));
    ok &= seed_file(root, b"switch_root.elf", include_bytes!(env!("OXFS_SWITCH_ROOT_ELF_PATH")));
    ok &= seed_file(root, b"sync.elf", include_bytes!(env!("OXFS_SYNC_ELF_PATH")));
    ok &= seed_file(root, b"syslogd.elf", include_bytes!(env!("OXFS_SYSLOGD_ELF_PATH")));
    ok &= seed_file(root, b"tac.elf", include_bytes!(env!("OXFS_TAC_ELF_PATH")));
    ok &= seed_file(root, b"tar.elf", include_bytes!(env!("OXFS_TAR_ELF_PATH")));
    ok &= seed_file(root, b"taskset.elf", include_bytes!(env!("OXFS_TASKSET_ELF_PATH")));
    ok &= seed_file(root, b"tcpsvd.elf", include_bytes!(env!("OXFS_TCPSVD_ELF_PATH")));
    ok &= seed_file(root, b"tee.elf", include_bytes!(env!("OXFS_TEE_ELF_PATH")));
    ok &= seed_file(root, b"telnet.elf", include_bytes!(env!("OXFS_TELNET_ELF_PATH")));
    ok &= seed_file(root, b"telnetd.elf", include_bytes!(env!("OXFS_TELNETD_ELF_PATH")));
    ok &= seed_file(root, b"test.elf", include_bytes!(env!("OXFS_TEST_ELF_PATH")));
    ok &= seed_file(root, b"time.elf", include_bytes!(env!("OXFS_TIME_ELF_PATH")));
    ok &= seed_file(root, b"timeout.elf", include_bytes!(env!("OXFS_TIMEOUT_ELF_PATH")));
    ok &= seed_file(root, b"top.elf", include_bytes!(env!("OXFS_TOP_ELF_PATH")));
    ok &= seed_file(root, b"tr.elf", include_bytes!(env!("OXFS_TR_ELF_PATH")));
    ok &= seed_file(root, b"traceroute.elf", include_bytes!(env!("OXFS_TRACEROUTE_ELF_PATH")));
    ok &= seed_file(root, b"tree.elf", include_bytes!(env!("OXFS_TREE_ELF_PATH")));
    ok &= seed_file(root, b"truncate.elf", include_bytes!(env!("OXFS_TRUNCATE_ELF_PATH")));
    ok &= seed_file(root, b"ts.elf", include_bytes!(env!("OXFS_TS_ELF_PATH")));
    ok &= seed_file(root, b"tsort.elf", include_bytes!(env!("OXFS_TSORT_ELF_PATH")));
    ok &= seed_file(root, b"tty.elf", include_bytes!(env!("OXFS_TTY_ELF_PATH")));
    ok &= seed_file(root, b"ttysize.elf", include_bytes!(env!("OXFS_TTYSIZE_ELF_PATH")));
    ok &= seed_file(root, b"udhcpd.elf", include_bytes!(env!("OXFS_UDHCPD_ELF_PATH")));
    ok &= seed_file(root, b"udpsvd.elf", include_bytes!(env!("OXFS_UDPSVD_ELF_PATH")));
    ok &= seed_file(root, b"umount.elf", include_bytes!(env!("OXFS_UMOUNT_ELF_PATH")));
    ok &= seed_file(root, b"uncompress.elf", include_bytes!(env!("OXFS_UNCOMPRESS_ELF_PATH")));
    ok &= seed_file(root, b"unexpand.elf", include_bytes!(env!("OXFS_UNEXPAND_ELF_PATH")));
    ok &= seed_file(root, b"unit.elf", include_bytes!(env!("OXFS_UNIT_ELF_PATH")));
    ok &= seed_file(root, b"unix2dos.elf", include_bytes!(env!("OXFS_UNIX2DOS_ELF_PATH")));
    ok &= seed_file(root, b"unlink.elf", include_bytes!(env!("OXFS_UNLINK_ELF_PATH")));
    ok &= seed_file(root, b"unlzma.elf", include_bytes!(env!("OXFS_UNLZMA_ELF_PATH")));
    ok &= seed_file(root, b"unshare.elf", include_bytes!(env!("OXFS_UNSHARE_ELF_PATH")));
    ok &= seed_file(root, b"unxz.elf", include_bytes!(env!("OXFS_UNXZ_ELF_PATH")));
    ok &= seed_file(root, b"unzip.elf", include_bytes!(env!("OXFS_UNZIP_ELF_PATH")));
    ok &= seed_file(root, b"uptime.elf", include_bytes!(env!("OXFS_UPTIME_ELF_PATH")));
    ok &= seed_file(root, b"usleep.elf", include_bytes!(env!("OXFS_USLEEP_ELF_PATH")));
    ok &= seed_file(root, b"uudecode.elf", include_bytes!(env!("OXFS_UUDECODE_ELF_PATH")));
    ok &= seed_file(root, b"uuencode.elf", include_bytes!(env!("OXFS_UUENCODE_ELF_PATH")));
    ok &= seed_file(root, b"vconfig.elf", include_bytes!(env!("OXFS_VCONFIG_ELF_PATH")));
    ok &= seed_file(root, b"vi.elf", include_bytes!(env!("OXFS_VI_ELF_PATH")));
    ok &= seed_file(root, b"volname.elf", include_bytes!(env!("OXFS_VOLNAME_ELF_PATH")));
    ok &= seed_file(root, b"watch.elf", include_bytes!(env!("OXFS_WATCH_ELF_PATH")));
    ok &= seed_file(root, b"wget.elf", include_bytes!(env!("OXFS_WGET_ELF_PATH")));
    ok &= seed_file(root, b"which.elf", include_bytes!(env!("OXFS_WHICH_ELF_PATH")));
    ok &= seed_file(root, b"whoami.elf", include_bytes!(env!("OXFS_WHOAMI_ELF_PATH")));
    ok &= seed_file(root, b"whois.elf", include_bytes!(env!("OXFS_WHOIS_ELF_PATH")));
    ok &= seed_file(root, b"xargs.elf", include_bytes!(env!("OXFS_XARGS_ELF_PATH")));
    ok &= seed_file(root, b"xxd.elf", include_bytes!(env!("OXFS_XXD_ELF_PATH")));
    ok &= seed_file(root, b"xzcat.elf", include_bytes!(env!("OXFS_XZCAT_ELF_PATH")));
    ok &= seed_file(root, b"zcat.elf", include_bytes!(env!("OXFS_ZCAT_ELF_PATH")));

    if !ok {
        log("[oxfs] self-check FAILED: seeding embedded files failed\n");
    }

    // --- Round-trip check: hello.txt/big.txt read back correctly. ---
    if let Some(hello) = dir_lookup(root, b"hello.txt") {
        let mut buf = [0u8; 64];
        let n = read_inode_at(hello, 0, &mut buf);
        if &buf[..n] != b"Hello from OxideBSD's own filesystem!\n" {
            ok = false;
            log("[oxfs] self-check FAILED: hello.txt contents mismatch\n");
        }
    } else {
        ok = false;
        log("[oxfs] self-check FAILED: hello.txt not found\n");
    }
    if let Some(big_inode) = dir_lookup(root, b"big.txt") {
        let mut buf = [0u8; BIG_FILE_LEN];
        let n = read_inode_at(big_inode, 0, &mut buf);
        let matches = n == BIG_FILE_LEN
            && buf[..n]
                .iter()
                .enumerate()
                .all(|(i, &b)| b == b'A' + (i % 26) as u8);
        if !matches {
            ok = false;
            log("[oxfs] self-check FAILED: big.txt contents mismatch (multi-block read)\n");
        }
    } else {
        ok = false;
        log("[oxfs] self-check FAILED: big.txt not found\n");
    }

    // --- stat/fstat/lstat round trip, through the real registered handlers. ---
    if let Some(hello) = dir_lookup(root, b"hello.txt") {
        let expected_size = read_inode(hello).size as i64;
        let path = b"hello.txt";
        let mut stat_buf = [0u8; 144];
        if oxfs_stat(
            path.as_ptr() as u64,
            path.len() as u64,
            stat_buf.as_mut_ptr() as u64,
            0,
        ) != 0
        {
            ok = false;
            log("[oxfs] self-check FAILED: stat hello.txt failed\n");
        } else {
            let st = unsafe { (stat_buf.as_ptr() as *const MuslStat).read_unaligned() };
            if st.st_ino != hello as u64 || st.st_size != expected_size || st.st_mode & S_IFREG == 0
            {
                ok = false;
                log("[oxfs] self-check FAILED: stat hello.txt field mismatch\n");
            }
        }

        let mut lstat_buf = [0u8; 144];
        if oxfs_lstat(
            path.as_ptr() as u64,
            path.len() as u64,
            lstat_buf.as_mut_ptr() as u64,
            0,
        ) != 0
            || lstat_buf != stat_buf
        {
            ok = false;
            log("[oxfs] self-check FAILED: lstat hello.txt disagreed with stat\n");
        }

        let fd = oxfs_open(path.as_ptr() as u64, path.len() as u64, 0, 0);
        if fd < 0 {
            ok = false;
            log("[oxfs] self-check FAILED: open hello.txt for fstat check failed\n");
        } else {
            let mut fstat_buf = [0u8; 144];
            if oxfs_fstat(fd as u64, fstat_buf.as_mut_ptr() as u64, 0, 0) != 0
                || fstat_buf != stat_buf
            {
                ok = false;
                log("[oxfs] self-check FAILED: fstat hello.txt disagreed with stat\n");
            }
            oxfs_close(fd as u64);
        }
    } else {
        ok = false;
        log("[oxfs] self-check FAILED: hello.txt not found for stat check\n");
    }

    // --- getdents round trip, through the real registered handler. ---
    let gdtest = b"/gdtest";
    if oxfs_mkdir(gdtest.as_ptr() as u64, gdtest.len() as u64, 0, 0) != 0 {
        ok = false;
        log("[oxfs] self-check FAILED: mkdir /gdtest failed\n");
    } else {
        let mut seeded = true;
        for name in [&b"/gdtest/a"[..], &b"/gdtest/b"[..]] {
            let fd = oxfs_open(name.as_ptr() as u64, name.len() as u64, O_CREAT, 0);
            if fd < 0 {
                seeded = false;
            } else {
                oxfs_close(fd as u64);
            }
        }
        if !seeded {
            ok = false;
            log("[oxfs] self-check FAILED: seeding /gdtest/{a,b} failed\n");
        }

        let dfd = oxfs_open(gdtest.as_ptr() as u64, gdtest.len() as u64, 0, 0);
        if dfd < 0 {
            ok = false;
            log("[oxfs] self-check FAILED: open /gdtest for getdents failed\n");
        } else {
            let dfd = dfd as u64;
            let mut buf = [0u8; 512];
            let n = oxfs_getdents(dfd, buf.as_mut_ptr() as u64, buf.len() as u64, 0);
            if n <= 0 {
                ok = false;
                log("[oxfs] self-check FAILED: getdents /gdtest returned nothing\n");
            } else {
                let (mut seen_dot, mut seen_dotdot, mut seen_a, mut seen_b) =
                    (false, false, false, false);
                let mut off = 0usize;
                let mut count = 0;
                while off < n as usize {
                    let reclen = u16::from_le_bytes([buf[off + 16], buf[off + 17]]) as usize;
                    if reclen == 0 || off + reclen > n as usize {
                        break;
                    }
                    let name_start = off + 19;
                    let name_end = buf[name_start..off + reclen]
                        .iter()
                        .position(|&b| b == 0)
                        .map_or(off + reclen, |p| name_start + p);
                    match &buf[name_start..name_end] {
                        b"." => seen_dot = true,
                        b".." => seen_dotdot = true,
                        b"a" => seen_a = true,
                        b"b" => seen_b = true,
                        _ => {}
                    }
                    count += 1;
                    off += reclen;
                }
                if count != 4 || !seen_dot || !seen_dotdot || !seen_a || !seen_b {
                    ok = false;
                    log("[oxfs] self-check FAILED: getdents /gdtest entries mismatch\n");
                }
                // Every record already consumed -- a second call must report EOF (0), the signal
                // readdir() relies on to stop looping.
                let n2 = oxfs_getdents(dfd, buf.as_mut_ptr() as u64, buf.len() as u64, 0);
                if n2 != 0 {
                    ok = false;
                    log("[oxfs] self-check FAILED: getdents /gdtest didn't reach EOF\n");
                }
            }
            oxfs_close(dfd);
        }
    }

    // --- mkdir/chdir/open(O_CREAT)/write/close/read, through the real registered handlers. ---
    if oxfs_mkdir(b"sub".as_ptr() as u64, 3, 0, 0) != 0 {
        ok = false;
        log("[oxfs] self-check FAILED: mkdir sub failed\n");
    } else if oxfs_chdir(b"sub".as_ptr() as u64, 3, 0, 0) != 0 {
        ok = false;
        log("[oxfs] self-check FAILED: chdir into sub failed\n");
    } else {
        let content = b"inside a subdirectory\n";
        let fd = oxfs_open(b"in.txt".as_ptr() as u64, 6, O_CREAT, 0);
        if fd < 0 {
            ok = false;
            log("[oxfs] self-check FAILED: open(O_CREAT) sub/in.txt failed\n");
        } else {
            let fd = fd as u64;
            if oxfs_write(fd, content.as_ptr() as u64, content.len() as u64) != content.len() as i64
            {
                ok = false;
                log("[oxfs] self-check FAILED: write sub/in.txt failed\n");
            }
            oxfs_close(fd);

            // getcwd inside sub -> "/sub".
            let mut cwd_buf = [0u8; 64];
            let n = oxfs_getcwd(cwd_buf.as_mut_ptr() as u64, cwd_buf.len() as u64, 0, 0);
            if n <= 0 || &cwd_buf[..(n as usize - 1)] != b"/sub" {
                ok = false;
                log("[oxfs] self-check FAILED: getcwd inside sub mismatch\n");
            }

            // Multi-component resolution: open "/sub/in.txt" in one call from root's own cwd.
            oxfs_chdir(b"/".as_ptr() as u64, 1, 0, 0);
            let path = b"/sub/in.txt";
            let fd = oxfs_open(path.as_ptr() as u64, path.len() as u64, 0, 0);
            if fd < 0 {
                ok = false;
                log("[oxfs] self-check FAILED: multi-component open /sub/in.txt failed\n");
            } else {
                let fd = fd as u64;
                let mut buf = [0u8; 64];
                let n = oxfs_read(fd, buf.as_mut_ptr() as u64, buf.len() as u64);
                oxfs_close(fd);
                if n < 0 || &buf[..n as usize] != content {
                    ok = false;
                    log("[oxfs] self-check FAILED: /sub/in.txt contents mismatch\n");
                }
            }

            // rename /sub/in.txt -> /sub/renamed.txt.
            let old = b"/sub/in.txt";
            let new = b"/sub/renamed.txt";
            if oxfs_rename(
                old.as_ptr() as u64,
                old.len() as u64,
                new.as_ptr() as u64,
                new.len() as u64,
            ) != 0
            {
                ok = false;
                log("[oxfs] self-check FAILED: rename /sub/in.txt failed\n");
            } else {
                let fd = oxfs_open(old.as_ptr() as u64, old.len() as u64, 0, 0);
                if fd >= 0 {
                    ok = false;
                    log("[oxfs] self-check FAILED: old name still openable after rename\n");
                }
                let fd = oxfs_open(new.as_ptr() as u64, new.len() as u64, 0, 0);
                if fd < 0 {
                    ok = false;
                    log("[oxfs] self-check FAILED: renamed.txt not openable after rename\n");
                } else {
                    oxfs_close(fd as u64);
                }
            }

            // unlink /sub/renamed.txt, mkdir /sub/nested (multi-component mkdir), rmdir checks.
            if oxfs_unlink(new.as_ptr() as u64, new.len() as u64, 0, 0) != 0 {
                ok = false;
                log("[oxfs] self-check FAILED: unlink /sub/renamed.txt failed\n");
            }
            let nested = b"/sub/nested";
            if oxfs_mkdir(nested.as_ptr() as u64, nested.len() as u64, 0, 0) != 0 {
                ok = false;
                log("[oxfs] self-check FAILED: mkdir /sub/nested failed\n");
            } else {
                let sub_path = b"/sub";
                if oxfs_rmdir(sub_path.as_ptr() as u64, sub_path.len() as u64, 0, 0) != -ENOTEMPTY {
                    ok = false;
                    log("[oxfs] self-check FAILED: rmdir /sub should have failed with ENOTEMPTY\n");
                }
                if oxfs_rmdir(nested.as_ptr() as u64, nested.len() as u64, 0, 0) != 0 {
                    ok = false;
                    log("[oxfs] self-check FAILED: rmdir /sub/nested failed\n");
                }
                if oxfs_rmdir(sub_path.as_ptr() as u64, sub_path.len() as u64, 0, 0) != 0 {
                    ok = false;
                    log("[oxfs] self-check FAILED: rmdir /sub failed\n");
                }
            }
        }
    }

    // Back to root, matching the state a booting kernel with no real process yet should leave
    // BOOT_CWD in (a real process's own cwd starts at Process::cwd's default, 0/root, regardless
    // of whatever this self-check did to BOOT_CWD -- see src/process.rs's own doc comment -- but
    // leaving this tidy avoids any confusion reading a boot log).
    set_current_cwd(ROOT_INODE);

    // SAFETY: FFI calls to kernel-exported functions, matching their declared signatures exactly.
    unsafe {
        oxidebsd_register_syscall(SYS_OPEN, oxfs_open);
        oxidebsd_register_syscall(SYS_CLOSE, sys_close);
        oxidebsd_register_syscall(SYS_CHDIR, oxfs_chdir);
        oxidebsd_register_syscall(SYS_MKDIR, oxfs_mkdir);
        oxidebsd_register_syscall(SYS_GETCWD, oxfs_getcwd);
        oxidebsd_register_syscall(SYS_UNLINK, oxfs_unlink);
        oxidebsd_register_syscall(SYS_RMDIR, oxfs_rmdir);
        oxidebsd_register_syscall(SYS_RENAME, oxfs_rename);
        oxidebsd_register_syscall(SYS_STAT, oxfs_stat);
        oxidebsd_register_syscall(SYS_LSTAT, oxfs_lstat);
        oxidebsd_register_syscall(SYS_FSTAT, oxfs_fstat);
        oxidebsd_register_syscall(SYS_GETDENTS, oxfs_getdents);
    }

    if ok {
        log("[oxfs] self-check passed\n");
        0
    } else {
        -1
    }
}
