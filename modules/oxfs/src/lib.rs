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
//! argument registers, the same precedent `execve`'s `envp_ptr` set for needing `R10`).
//! `stat`/`fstat` is deliberately not attempted here -- it needs a byte-exact musl `struct stat`
//! layout, separate follow-up work.
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
}

const SYS_OPEN: u64 = 5;
const SYS_CLOSE: u64 = 6;
const SYS_CHDIR: u64 = 12;
const SYS_MKDIR: u64 = 136;
const SYS_GETCWD: u64 = 108;
const SYS_UNLINK: u64 = 109;
const SYS_RMDIR: u64 = 110;
const SYS_RENAME: u64 = 111;

/// Same real POSIX value FAT32's own `O_CREAT` already uses (`0o100`, not an arbitrary bit) --
/// see `modules/fat32`'s own doc comment for why matching the real bit matters (musl's real
/// `open()` passes real POSIX flag values).
const O_CREAT: u64 = 0o100;

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
/// 4 MiB pool -- comfortably more than every embedded binary combined today (~250 KB), with real
/// headroom for runtime-created files (`stsh`'s `write` built-in, BusyBox's own file creation).
const NUM_BLOCKS: usize = 1024;
const MAX_INODES: usize = 64;
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
    /// `ls`.
    DirListing {
        content: [u8; DIR_LISTING_BUFFER],
        len: usize,
        position: usize,
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
        content,
        len,
        position: 0,
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
    }

    if ok {
        log("[oxfs] self-check passed\n");
        0
    } else {
        -1
    }
}
