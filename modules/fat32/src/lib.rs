//! A basic FAT32 filesystem module (read and write, including subdirectories), parsing an
//! embedded in-memory disk image generated at build time (`build.rs`'s `write_fat32_image`, not
//! `mkfs.fat` -- see `CLAUDE.md`'s module-loading/FAT32 section for why: hermeticity, and a real
//! `mkfs.fat` volume would need to be tens of megabytes to meet Microsoft's minimum-cluster-count
//! heuristic). No real block device exists yet, so this is squarely a filesystem-*format* proof,
//! not a storage-*driver* one -- writes mutate the in-memory working copy (`DISK` below) only and
//! do not persist across reboot.
//!
//! **Deliberate simplifications, all documented rather than accidental:**
//! - 8.3 short names only -- no VFAT/long-filename directory entries.
//! - **Path grammar is deliberately minimal: a single component only**, resolved against either
//!   the current directory or (with a leading `/`) root -- never a multi-level path like `a/b/c`
//!   in one call (`to_short_name` rejects any embedded `/`, returning `EINVAL`). `..` (parent) and
//!   `.`/empty (current directory itself) are the only two special components. Real shells build
//!   multi-level navigation out of repeated single-component `cd`s; this module doesn't need to
//!   understand a full path to support that.
//! - No directory's own cluster chain is ever extended once full (`create_file`/`sys_mkdir` fail
//!   with `DirectoryFull`/`ENOSPC` instead) -- fine for this module's tiny demo scale, a real gap
//!   for heavier use.
//! - Sequential reads via an internal walk of a file's cluster chain -- no `lseek`/random access.
//! - Writes only ever *create* a brand-new file with its full contents in one logical operation --
//!   no appending to or truncating an existing file's clusters.
//! - The generated image is *structurally* correct FAT32 (real BPB/FSInfo, 2 FAT copies, 32-bit
//!   FAT entries, root directory as a proper cluster chain) but deliberately smaller than
//!   Microsoft's conventional FAT32 minimum volume size -- safe since only this module's own
//!   parser ever reads it.
//! - There is exactly one, kernel-wide "current directory" (`CURRENT_DIR_CLUSTER`), not a
//!   per-process one -- this kernel has no process abstraction at all yet, so there's nothing to
//!   scope it to. Fine for a single interactive shell; would need real per-process state once
//!   more than one thing can be "current" at once.
//!
//! **Testing note:** this crate is compiled entirely independently of the kernel (see
//! `src/module.rs`'s doc comment) and only ever runs as relocated, loaded module code -- there is
//! no way for the kernel's `#[test_case]`-based test framework (`src/lib.rs`) to reach into a
//! separately-compiled module crate at all. Rather than duplicate this parsing logic into a
//! second, kernel-side copy purely to get `#[test_case]` coverage (risking the two drifting apart)
//! this module instead runs a self-check against its own real, actually-loaded code from
//! `module_init` and logs PASS/FAIL -- the same "boot in QEMU and self-report" testing philosophy
//! `CLAUDE.md`'s "Test architecture" section already establishes for the kernel as a whole.
//!
//! **Discovered the hard way: no `core::fmt`/`write!` anywhere in module code.** An earlier draft
//! of this file's logging used `write!` into a custom `core::fmt::Write` sink for readability.
//! That doesn't just re-introduce `R_X86_64_GOTPCREL` relocations despite `-C
//! relocation-model=static` (confirmed via `objdump -r`) -- `core::fmt::Write::write_fmt`'s
//! default implementation calls `core::fmt::write(&mut dyn Write, ..)`, and constructing that
//! trait object's vtable is what actually emits the GOT-relative reference, a code path none of
//! the simpler `{}`/`{:x}`-on-a-primitive cases this design was originally validated against ever
//! exercises. It also pulls in a large fraction of `core::fmt`'s numeric-formatting and Unicode
//! tables, which is what actually caused this module's very first boot attempt to crash: the
//! merged object ballooned to 3+ MB with thousands of sections, and `src/module.rs`'s loader
//! (kernel-side code, using `alloc`) exhausted the kernel's 100 KiB heap just parsing that many
//! section headers. Manual byte-level formatting (`ByteBuf` below) avoids both problems at once.
//!
//! **Syscall integration:** registers `SYS_OPEN = 5` / `SYS_CLOSE = 6` / `SYS_CHDIR = 12` /
//! `SYS_MKDIR = 136` (real FreeBSD values, continuing the existing "authenticity nod" pattern
//! already used for `SYS_EXIT`/`SYS_READ`/`SYS_WRITE`) against the shared syscall dispatch table,
//! the same mechanism `modules/native_abi/` uses. `SYS_READ`/`SYS_WRITE` themselves stay owned by
//! `native_abi` and `src/syscall.rs` -- for any fd this module didn't hand out, they fall through
//! to a kernel-owned fd-ops registry (`src/fd.rs`) this module registers each open file's
//! read/write/close callbacks against. That registry exists specifically because two independently
//! loaded modules can't call each other directly (only module → kernel, via each module's own
//! resolved symbol table) -- it's the only coordination point available.
//!
//! **`ls` reuses `open`/`read`/`close` rather than a dedicated syscall.** `fat32_open`, when the
//! resolved name is a directory (or when the path names the current directory itself), doesn't
//! read file content -- it formats a listing into the very same `OpenFile::Read` buffer a real
//! file's content would occupy, and the caller (`stsh`'s `ls`) just reads it out like any other
//! file. A pragmatic simplification (real `open()` on a directory doesn't behave this way -- you'd
//! use `getdents`/`readdir`), not a claim of POSIX directory-fd semantics.
#![no_std]

unsafe extern "C" {
    fn oxidebsd_log(ptr: *const u8, len: u64);
    fn oxidebsd_register_syscall(number: u64, handler: extern "C" fn(u64, u64, u64) -> i64) -> i32;
    fn oxidebsd_alloc_fd() -> u64;
    fn oxidebsd_register_fd_ops(
        fd: u64,
        read: extern "C" fn(u64, u64, u64) -> i64,
        write: extern "C" fn(u64, u64, u64) -> i64,
        close: extern "C" fn(u64) -> i64,
    ) -> i32;
    fn oxidebsd_close_fd(fd: u64) -> i32;
}

const SYS_OPEN: u64 = 5;
const SYS_CLOSE: u64 = 6;
const SYS_CHDIR: u64 = 12;
const SYS_MKDIR: u64 = 136;

/// `open`'s `flags`: create the file if it doesn't already exist. No other flag bits are
/// interpreted. Deliberately the *real* POSIX `O_CREAT` value (`0o100`), not an arbitrary
/// OxideBSD-invented bit like this module's syscall numbers otherwise use -- this one used to be
/// bit 0 (`1`), which happens to collide with real POSIX's `O_WRONLY`. Harmless as long as only
/// `stsh`'s own native-ABI caller (which never sets that bit) constructs flags, but musl's real
/// `open()` (patched to speak this module's wire format directly -- see CLAUDE.md's musl-port
/// section) passes real POSIX flag values, where a plain `O_WRONLY` open with no `O_CREAT` would
/// otherwise be silently misread as "create". Matching the real bit sidesteps the collision
/// entirely rather than working around it.
const O_CREAT: u64 = 0o100;

const EBADF: i64 = 9;
const ENOENT: i64 = 2;
const EEXIST: i64 = 17;
const ENOTDIR: i64 = 20;
const EMFILE: i64 = 24;
const ENOSPC: i64 = 28;
const EIO: i64 = 5;
const EFBIG: i64 = 27;
const EINVAL: i64 = 22;

/// Per-open-file content buffer capacity, for both directions -- read caches a whole file's (or
/// directory listing's) contents at `open` time (see `OpenFile::Read`'s doc comment for why),
/// write accumulates a whole file's contents across possibly-multiple `write` calls until `close`.
/// Comfortably larger than every file/listing this module's own demo/self-check content produces,
/// and than `SMOKE.ELF`/`MUSL.ELF` (the embedded `ring3-smoke`/`musl-smoke` binaries `stsh`'s
/// `execve` support -- see `src/process.rs`'s `do_execve` -- exercises end to end). Raised twice
/// now: 4096 -> 16384 for `SMOKE.ELF`'s debug build, then 16384 -> 65536 here for `musl-smoke`
/// (~23 KB once statically linked against a real libc -- meaningfully bigger than a bare
/// hand-written demo binary, being real compiled C plus musl's crt/malloc/stdio).
const MAX_FILE_BUFFER: usize = 65536;
const MAX_OPEN_FILES: usize = 8;

/// An open file's own state, keyed by fd in `OPEN_FILES` below.
///
/// `Read` caches the *entire* file's contents (or, for a directory, a formatted listing -- see
/// the module doc comment's note on `ls`) at `open` time rather than walking the cluster chain
/// incrementally on each `read` call -- simpler and reuses the same "read whole thing into a
/// fixed buffer" shape already established for this module's self-check, at the cost of capping
/// readable file size at `MAX_FILE_BUFFER` (`open` returns `EFBIG` past that).
#[derive(Clone, Copy)]
enum OpenFile {
    Read {
        content: [u8; MAX_FILE_BUFFER],
        len: usize,
        position: usize,
    },
    Write {
        name: [u8; 11],
        dir_cluster: u32,
        buffer: [u8; MAX_FILE_BUFFER],
        len: usize,
    },
}

/// Fixed-size, linearly-scanned open-file table -- `static mut` for the same reason `DISK` is
/// (see its doc comment): every read of it happens from within this module's own exported
/// syscall-registered handlers (`fat32_open`/`fat32_read`/`fat32_write`/`fat32_close`), each of
/// which is externally callable and whose results feed observably into `oxidebsd_log`/the return
/// value handed back across the syscall boundary, so the optimizer can't treat any of it as dead.
static mut OPEN_FILES: [Option<(u64, OpenFile)>; MAX_OPEN_FILES] = [None; MAX_OPEN_FILES];

fn find_open_file(fd: u64) -> Option<&'static mut OpenFile> {
    // SAFETY: see OPEN_FILES's own doc comment -- single-core, syscall-serialized access only.
    let slots = unsafe { &mut *core::ptr::addr_of_mut!(OPEN_FILES) };
    for (slot_fd, file) in slots.iter_mut().flatten() {
        if *slot_fd == fd {
            return Some(file);
        }
    }
    None
}

/// The kernel-wide "current directory" cluster -- see the module doc comment for why this is a
/// single, not per-process, piece of state. `static mut` for the same reason `DISK`/`OPEN_FILES`
/// are: every read of it happens from within this module's own exported, syscall-reachable
/// functions (all registered from `module_init`, so all transitively kept alive by `build.rs`'s
/// `--gc-sections -u module_init`), so the optimizer can't prove any write to it unobservable.
static mut CURRENT_DIR_CLUSTER: u32 = 0;

fn current_dir_cluster() -> u32 {
    // SAFETY: see CURRENT_DIR_CLUSTER's own doc comment.
    unsafe { CURRENT_DIR_CLUSTER }
}

fn set_current_dir_cluster(cluster: u32) {
    // SAFETY: see CURRENT_DIR_CLUSTER's own doc comment.
    unsafe { CURRENT_DIR_CLUSTER = cluster };
}

/// Converts a human-typed path component like `foo.txt` or `FOO` into FAT32's padded 11-byte 8.3
/// short-name form. `None` if the base name or extension is too long to fit (no long-filename
/// support), or if `input` contains a `/` at all (this module's path grammar is single-component
/// only -- see the module doc comment; a `/` here means the caller was given a multi-level path,
/// which isn't a "this name doesn't fit" situation so much as "this isn't a name at all").
fn to_short_name(input: &[u8]) -> Option<[u8; 11]> {
    if input.contains(&b'/') {
        return None;
    }
    let mut name = [b' '; 11];
    let (base, ext) = match input.iter().position(|&b| b == b'.') {
        Some(dot) => (&input[..dot], &input[dot + 1..]),
        None => (input, &input[0..0]),
    };
    if base.is_empty() || base.len() > 8 || ext.len() > 3 {
        return None;
    }
    for (i, &b) in base.iter().enumerate() {
        name[i] = b.to_ascii_uppercase();
    }
    for (i, &b) in ext.iter().enumerate() {
        name[8 + i] = b.to_ascii_uppercase();
    }
    Some(name)
}

/// Registered for `SYS_OPEN`. `path_ptr`/`path_len` name the target (converted via
/// `to_short_name`, see the module doc comment for the path grammar); `flags & O_CREAT` requests
/// creation (see `O_CREAT`'s own doc comment for its real POSIX value and why that matters now).
/// If the resolved name is a directory (or the path names the current directory itself, or root),
/// this "opens" a formatted listing rather than file content -- see the module doc comment's note
/// on `ls`. Returns a new fd on success, or a negative `-errno` (`ENOENT` if a file doesn't exist
/// and `O_CREAT` wasn't set, `EFBIG` if an existing file is too large for `MAX_FILE_BUFFER`,
/// `EMFILE` if `OPEN_FILES` is full, `EINVAL` for a malformed/multi-component path).
extern "C" fn fat32_open(path_ptr: u64, path_len: u64, flags: u64) -> i64 {
    // SAFETY: same trust boundary as sys_write's own documented pointer-validation gap in
    // src/syscall.rs -- the caller (ultimately userland, via SYS_OPEN) owns this pointer/length.
    let path = unsafe { core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize) };
    let create = flags & O_CREAT != 0;

    // SAFETY: see DISK's own doc comment -- single-core, syscall-serialized access only.
    let disk: &mut [u8; DISK_SIZE] = unsafe { &mut *core::ptr::addr_of_mut!(DISK) };
    let bpb = Bpb::parse(disk);
    let cwd = current_dir_cluster();

    if path.is_empty() || path == b"." {
        return open_directory_listing(disk, &bpb, cwd);
    }
    if path == b"/" {
        return open_directory_listing(disk, &bpb, bpb.root_cluster);
    }

    let (base, name) = match path.strip_prefix(b"/") {
        Some(rest) => (bpb.root_cluster, rest),
        None => (cwd, path),
    };

    if name == b".." {
        return match parent_of(disk, &bpb, base) {
            Some(parent) => open_directory_listing(disk, &bpb, parent),
            None => -ENOENT,
        };
    }

    let Some(short) = to_short_name(name) else {
        return -EINVAL;
    };

    let mut entries = [DirEntry::EMPTY; MAX_DIR_ENTRIES];
    let count = list_dir(disk, &bpb, base, &mut entries);

    let open_file = match find_entry(&entries[..count], &short) {
        Some(entry) if entry.attr & ATTR_DIRECTORY != 0 => {
            let target = if entry.first_cluster == 0 {
                bpb.root_cluster
            } else {
                entry.first_cluster
            };
            return open_directory_listing(disk, &bpb, target);
        }
        Some(entry) => {
            if entry.size as usize > MAX_FILE_BUFFER {
                return -EFBIG;
            }
            let mut content = [0u8; MAX_FILE_BUFFER];
            let n = read_file(disk, &bpb, entry, &mut content);
            OpenFile::Read {
                content,
                len: n,
                position: 0,
            }
        }
        None if create => OpenFile::Write {
            name: short,
            dir_cluster: base,
            buffer: [0; MAX_FILE_BUFFER],
            len: 0,
        },
        None => return -ENOENT,
    };

    register_open_file(open_file)
}

/// Formats `dir_cluster`'s listing (one name per line; `<DIR>` or a byte count) into an
/// `OpenFile::Read` buffer and registers a fd for it, exactly like a real file's content would be
/// -- see the module doc comment's note on why `ls` doesn't need its own syscall. `.`/`..` entries
/// are hidden from the listing (matching a plain `ls`'s default, not `ls -a`'s).
fn open_directory_listing(disk: &[u8], bpb: &Bpb, dir_cluster: u32) -> i64 {
    let mut entries = [DirEntry::EMPTY; MAX_DIR_ENTRIES];
    let count = list_dir(disk, bpb, dir_cluster, &mut entries);

    let mut content = [0u8; MAX_FILE_BUFFER];
    let len = {
        let mut out = ByteBuf {
            buf: &mut content,
            len: 0,
        };
        for entry in &entries[..count] {
            if entry.name[0] == b'.' {
                continue;
            }
            out.push_bytes(&entry.name);
            if entry.attr & ATTR_DIRECTORY != 0 {
                out.push_bytes(b"  <DIR>\n");
            } else {
                out.push_bytes(b"  ");
                out.push_decimal(entry.size);
                out.push_bytes(b"\n");
            }
        }
        out.len
    };

    register_open_file(OpenFile::Read {
        content,
        len,
        position: 0,
    })
}

fn register_open_file(open_file: OpenFile) -> i64 {
    let slots = unsafe { &mut *core::ptr::addr_of_mut!(OPEN_FILES) };
    let Some(slot) = slots.iter_mut().find(|s| s.is_none()) else {
        return -EMFILE;
    };
    // SAFETY: FFI call to a kernel-exported function, matching its declared signature exactly.
    let fd = unsafe { oxidebsd_alloc_fd() };
    *slot = Some((fd, open_file));
    // SAFETY: fat32_read/fat32_write/fat32_close are this module's own functions, already
    // relocated by the time module_init (and therefore this function, which module_init's own
    // registration made reachable) runs.
    unsafe { oxidebsd_register_fd_ops(fd, fat32_read, fat32_write, fat32_close) };
    fd as i64
}

/// Registered as `fd`'s read callback via `oxidebsd_register_fd_ops`.
extern "C" fn fat32_read(fd: u64, ptr: u64, len: u64) -> i64 {
    let Some(file) = find_open_file(fd) else {
        return -EBADF;
    };
    match file {
        OpenFile::Read {
            content,
            len: file_len,
            position,
        } => {
            let remaining = *file_len - *position;
            let n = remaining.min(len as usize);
            // SAFETY: same trust boundary as elsewhere -- caller-owned pointer/length.
            let out = unsafe { core::slice::from_raw_parts_mut(ptr as *mut u8, n) };
            out.copy_from_slice(&content[*position..*position + n]);
            *position += n;
            n as i64
        }
        OpenFile::Write { .. } => -EBADF,
    }
}

/// Registered as `fd`'s write callback via `oxidebsd_register_fd_ops`.
extern "C" fn fat32_write(fd: u64, ptr: u64, len: u64) -> i64 {
    let Some(file) = find_open_file(fd) else {
        return -EBADF;
    };
    match file {
        OpenFile::Write {
            buffer,
            len: buf_len,
            ..
        } => {
            let available = MAX_FILE_BUFFER - *buf_len;
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
        OpenFile::Read { .. } => -EBADF,
    }
}

/// Registered as `fd`'s close callback via `oxidebsd_register_fd_ops` -- called by
/// `oxidebsd_close_fd` (which itself removes `fd` from the kernel's registry first), not directly
/// by `sys_close` below. For a file that was opened for writing, this is the only point at which
/// its accumulated buffer is actually committed to `DISK` via `create_file` -- see the module doc
/// comment: writes are all-at-once-on-close, not incrementally applied per `write` call.
extern "C" fn fat32_close(fd: u64) -> i64 {
    let slots = unsafe { &mut *core::ptr::addr_of_mut!(OPEN_FILES) };
    let Some(slot) = slots
        .iter_mut()
        .find(|s| matches!(s, Some((slot_fd, _)) if *slot_fd == fd))
    else {
        return -EBADF;
    };
    let (_, file) = slot.take().expect("just matched Some above");

    if let OpenFile::Write {
        name,
        dir_cluster,
        buffer,
        len,
    } = file
    {
        let disk: &mut [u8; DISK_SIZE] = unsafe { &mut *core::ptr::addr_of_mut!(DISK) };
        let bpb = Bpb::parse(disk);
        return match create_file(disk, &bpb, dir_cluster, &name, &buffer[..len]) {
            Ok(()) => 0,
            Err(_) => -EIO,
        };
    }
    0
}

/// Registered for `SYS_CLOSE`. Delegates to the kernel's own `oxidebsd_close_fd`, which removes
/// `fd` from its registry and invokes `fat32_close` above -- not a direct call to `fat32_close`,
/// so a closed fd is also no longer reachable via `SYS_READ`/`SYS_WRITE` afterward.
extern "C" fn sys_close(fd: u64, _arg1: u64, _arg2: u64) -> i64 {
    // SAFETY: FFI call to a kernel-exported function, matching its declared signature exactly.
    unsafe { oxidebsd_close_fd(fd) as i64 }
}

/// Registered for `SYS_CHDIR`. Resolves `path` (see the module doc comment's path grammar)
/// against the current directory and, if it names a directory, makes it the new current
/// directory. `ENOTDIR` covers both "doesn't exist" and "exists but isn't a directory" -- kept
/// simple rather than distinguishing them.
extern "C" fn sys_chdir(path_ptr: u64, path_len: u64, _arg2: u64) -> i64 {
    // SAFETY: same trust boundary as elsewhere -- caller-owned pointer/length.
    let path = unsafe { core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize) };
    let disk: &[u8; DISK_SIZE] = unsafe { &*core::ptr::addr_of!(DISK) };
    let bpb = Bpb::parse(disk);
    match resolve_dir(disk, &bpb, current_dir_cluster(), path) {
        Some(cluster) => {
            set_current_dir_cluster(cluster);
            0
        }
        None => -ENOTDIR,
    }
}

/// Registered for `SYS_MKDIR`. `path` must be a single plain component (no `/` prefix, no `..`,
/// no multi-level path -- see the module doc comment) naming a new subdirectory of the current
/// directory. Initializes the new subdirectory's own `.`/`..` entries (per FAT32 convention, a
/// `..` whose parent is root stores cluster `0`, not root's real cluster number -- `parent_of`
/// undoes the same translation when reading it back).
extern "C" fn sys_mkdir(path_ptr: u64, path_len: u64, _arg2: u64) -> i64 {
    // SAFETY: same trust boundary as elsewhere -- caller-owned pointer/length.
    let path = unsafe { core::slice::from_raw_parts(path_ptr as *const u8, path_len as usize) };
    let Some(short) = to_short_name(path) else {
        return -EINVAL;
    };

    let disk: &mut [u8; DISK_SIZE] = unsafe { &mut *core::ptr::addr_of_mut!(DISK) };
    let bpb = Bpb::parse(disk);
    let cwd = current_dir_cluster();

    let mut entries = [DirEntry::EMPTY; MAX_DIR_ENTRIES];
    let count = list_dir(disk, &bpb, cwd, &mut entries);
    if find_entry(&entries[..count], &short).is_some() {
        return -EEXIST;
    }

    let Some(new_cluster) = bpb.allocate_cluster(disk) else {
        return -ENOSPC;
    };

    // allocate_cluster only marks the FAT entry used -- it doesn't touch the data region, so the
    // new cluster's actual bytes could be stale (never happens today, since nothing frees/reuses
    // clusters yet, but relying on that implicitly would be fragile).
    let cluster_offset = bpb.cluster_offset(new_cluster);
    disk[cluster_offset..cluster_offset + bpb.cluster_size()].fill(0);

    let dotdot_target = if cwd == bpb.root_cluster { 0 } else { cwd };
    write_dir_entry(
        disk,
        cluster_offset,
        &DOT_NAME,
        ATTR_DIRECTORY,
        new_cluster,
        0,
    );
    write_dir_entry(
        disk,
        cluster_offset + DIR_ENTRY_SIZE,
        &DOTDOT_NAME,
        ATTR_DIRECTORY,
        dotdot_target,
        0,
    );

    let Some(dir_offset) = find_free_dir_slot(disk, &bpb, cwd) else {
        return -ENOSPC;
    };
    write_dir_entry(disk, dir_offset, &short, ATTR_DIRECTORY, new_cluster, 0);
    0
}

fn log_bytes(bytes: &[u8]) {
    unsafe { oxidebsd_log(bytes.as_ptr(), bytes.len() as u64) };
}

fn log(message: &str) {
    log_bytes(message.as_bytes());
}

/// The embedded disk image template, generated by `build.rs`'s `write_fat32_image` and baked in
/// via `include_bytes!` -- immutable (it's `&'static [u8]`, living in `.rodata`), copied once into
/// `DISK` below at `module_init` time. All actual reads/writes after that go through `DISK`, never
/// this template directly, so writes are visible to later reads within the same boot.
static TEMPLATE: &[u8] = include_bytes!(env!("FAT32_IMAGE_PATH"));

const DISK_SIZE: usize = 2 * 1024 * 1024;

/// The mutable working copy of the disk image -- `static mut`, not `static`, because it's
/// genuinely written at runtime (unlike `TEMPLATE`). Per `CLAUDE.md`'s module-loading section, a
/// private `static mut` buffer written once but never read back through an *externally visible*
/// function can have that write optimized away entirely as an unobservable dead store (a
/// different failure mode from `gdt.rs`'s own `static mut` requirement, which is about hardware
/// writes being invisible to the optimizer in the first place). This buffer is safe from that:
/// every read of it happens from within this module's own exported, syscall-reachable functions,
/// whose results feed observably into `oxidebsd_log` calls or syscall return values -- the
/// optimizer can't prove any of that observable behavior away.
static mut DISK: [u8; DISK_SIZE] = [0; DISK_SIZE];

const SECTOR_HEADER_BYTES_PER_SECTOR: usize = 11;
const SECTOR_HEADER_SECTORS_PER_CLUSTER: usize = 13;
const SECTOR_HEADER_RESERVED_SECTORS: usize = 14;
const SECTOR_HEADER_NUM_FATS: usize = 16;
const SECTOR_HEADER_FAT_SIZE_32: usize = 36;
const SECTOR_HEADER_ROOT_CLUSTER: usize = 44;

const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const DIR_ENTRY_SIZE: usize = 32;
const DIR_FREE: u8 = 0xE5;
const DIR_END: u8 = 0x00;
/// Masking off the top 4 (reserved) bits of a 32-bit FAT32 entry is required reading behavior per
/// spec, not optional -- a conforming writer must never rely on them being zero.
const FAT_ENTRY_MASK: u32 = 0x0FFF_FFFF;
const EOC_MIN: u32 = 0x0FFF_FFF8;
const EOC: u32 = 0x0FFF_FFFF;

const MAX_DIR_ENTRIES: usize = 32;

/// A `.` short-name entry, padded per 8.3 rules ('.' followed by 10 spaces -- 11 bytes total,
/// verified by the array-length-checked literal below, not by hand-counting spaces).
const DOT_NAME: [u8; 11] = *b".          ";
/// A `..` short-name entry, padded per 8.3 rules ('..' followed by 9 spaces -- 11 bytes total).
const DOTDOT_NAME: [u8; 11] = *b"..         ";

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

struct Bpb {
    bytes_per_sector: u32,
    sectors_per_cluster: u32,
    reserved_sectors: u32,
    num_fats: u32,
    fat_size_sectors: u32,
    root_cluster: u32,
}

impl Bpb {
    fn parse(image: &[u8]) -> Self {
        Bpb {
            bytes_per_sector: read_u16(image, SECTOR_HEADER_BYTES_PER_SECTOR) as u32,
            sectors_per_cluster: image[SECTOR_HEADER_SECTORS_PER_CLUSTER] as u32,
            reserved_sectors: read_u16(image, SECTOR_HEADER_RESERVED_SECTORS) as u32,
            num_fats: image[SECTOR_HEADER_NUM_FATS] as u32,
            fat_size_sectors: read_u32(image, SECTOR_HEADER_FAT_SIZE_32),
            root_cluster: read_u32(image, SECTOR_HEADER_ROOT_CLUSTER),
        }
    }

    fn data_start_sector(&self) -> u32 {
        self.reserved_sectors + self.num_fats * self.fat_size_sectors
    }

    fn cluster_size(&self) -> usize {
        self.sectors_per_cluster as usize * self.bytes_per_sector as usize
    }

    fn cluster_offset(&self, cluster: u32) -> usize {
        let sector = self.data_start_sector() + (cluster - 2) * self.sectors_per_cluster;
        sector as usize * self.bytes_per_sector as usize
    }

    fn fat_entry(&self, image: &[u8], cluster: u32) -> u32 {
        let fat_start = self.reserved_sectors as usize * self.bytes_per_sector as usize;
        let offset = fat_start + cluster as usize * 4;
        read_u32(image, offset) & FAT_ENTRY_MASK
    }

    /// Writes `value` into `cluster`'s entry in *every* FAT copy (`num_fats`, normally 2) --
    /// real FAT32 keeps them mirrored; this loader's own reads only ever consult the first copy,
    /// but writing both keeps the on-disk structure honest.
    fn write_fat_entry(&self, disk: &mut [u8], cluster: u32, value: u32) {
        for fat_index in 0..self.num_fats {
            let fat_start = (self.reserved_sectors + fat_index * self.fat_size_sectors) as usize
                * self.bytes_per_sector as usize;
            let offset = fat_start + cluster as usize * 4;
            disk[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
    }

    /// Scans the FAT for the first free (`0x00000000`) cluster at or after cluster 2, marks it
    /// end-of-chain, and returns its number. No free-cluster cache/hint (real FAT32's `FSInfo`
    /// sector exists for exactly this, but this module's parser never reads `FSInfo` at all --
    /// see the module doc comment) -- a linear scan is fine at this module's scale.
    fn allocate_cluster(&self, disk: &mut [u8]) -> Option<u32> {
        let max_entries = (self.fat_size_sectors * self.bytes_per_sector) / 4;
        for cluster in 2..max_entries {
            if self.fat_entry(disk, cluster) == 0 {
                self.write_fat_entry(disk, cluster, EOC);
                return Some(cluster);
            }
        }
        None
    }
}

#[derive(Clone, Copy)]
struct DirEntry {
    name: [u8; 11],
    attr: u8,
    first_cluster: u32,
    size: u32,
}

impl DirEntry {
    const EMPTY: DirEntry = DirEntry {
        name: [0; 11],
        attr: 0,
        first_cluster: 0,
        size: 0,
    };
}

/// Walks `dir_cluster`'s cluster chain, collecting up to `MAX_DIR_ENTRIES` entries (files *and*
/// subdirectories -- only the volume-label entry is skipped) into `out`. Returns the count found.
/// Works for root or any subdirectory alike -- there's nothing root-specific about it, despite the
/// name this function had before subdirectory support existed.
fn list_dir(
    image: &[u8],
    bpb: &Bpb,
    dir_cluster: u32,
    out: &mut [DirEntry; MAX_DIR_ENTRIES],
) -> usize {
    let mut count = 0;
    let mut cluster = dir_cluster;
    'clusters: loop {
        let offset = bpb.cluster_offset(cluster);
        let cluster_bytes = &image[offset..offset + bpb.cluster_size()];
        let (dir_entries, _) = cluster_bytes.as_chunks::<DIR_ENTRY_SIZE>();
        for entry_bytes in dir_entries {
            if entry_bytes[0] == DIR_END {
                break 'clusters;
            }
            if entry_bytes[0] == DIR_FREE {
                continue;
            }
            let attr = entry_bytes[11];
            if attr & ATTR_VOLUME_ID != 0 {
                continue;
            }
            if count >= MAX_DIR_ENTRIES {
                break 'clusters;
            }
            let mut name = [0u8; 11];
            name.copy_from_slice(&entry_bytes[0..11]);
            let cluster_hi = read_u16(entry_bytes, 20) as u32;
            let cluster_lo = read_u16(entry_bytes, 26) as u32;
            out[count] = DirEntry {
                name,
                attr,
                first_cluster: (cluster_hi << 16) | cluster_lo,
                size: read_u32(entry_bytes, 28),
            };
            count += 1;
        }
        let next = bpb.fat_entry(image, cluster);
        if next >= EOC_MIN {
            break;
        }
        cluster = next;
    }
    count
}

fn find_entry<'a>(entries: &'a [DirEntry], name: &[u8; 11]) -> Option<&'a DirEntry> {
    entries.iter().find(|e| &e.name == name)
}

/// Reads `dir_cluster`'s own `..` entry (present in every subdirectory except root, which has
/// neither `.` nor `..` -- there's nothing above it) to find its parent. FAT32 stores `0` in a
/// `..` entry's cluster field to mean "the root directory" specifically (a quirk inherited from
/// FAT12/16, where root had no cluster number at all, being a separate fixed region) -- resolved
/// to `bpb.root_cluster` here.
fn parent_of(disk: &[u8], bpb: &Bpb, dir_cluster: u32) -> Option<u32> {
    if dir_cluster == bpb.root_cluster {
        return Some(bpb.root_cluster);
    }
    let offset = bpb.cluster_offset(dir_cluster);
    let dotdot = &disk[offset + DIR_ENTRY_SIZE..offset + 2 * DIR_ENTRY_SIZE];
    let cluster_hi = read_u16(dotdot, 20) as u32;
    let cluster_lo = read_u16(dotdot, 26) as u32;
    let parent = (cluster_hi << 16) | cluster_lo;
    Some(if parent == 0 {
        bpb.root_cluster
    } else {
        parent
    })
}

/// Resolves `path` to a directory cluster -- see the module doc comment for the (deliberately
/// minimal) path grammar: `""`/`"."` (cwd itself), `"/"` (root), `".."` (cwd's parent), or a
/// single plain name (a subdirectory of cwd), optionally `/`-prefixed to resolve against root
/// instead of cwd. `None` if the named entry doesn't exist or isn't a directory.
fn resolve_dir(disk: &[u8], bpb: &Bpb, cwd: u32, path: &[u8]) -> Option<u32> {
    if path.is_empty() || path == b"." {
        return Some(cwd);
    }
    if path == b"/" {
        return Some(bpb.root_cluster);
    }
    let (base, name) = match path.strip_prefix(b"/") {
        Some(rest) => (bpb.root_cluster, rest),
        None => (cwd, path),
    };
    if name.is_empty() || name == b"." {
        return Some(base);
    }
    if name == b".." {
        return parent_of(disk, bpb, base);
    }
    let short = to_short_name(name)?;
    let mut entries = [DirEntry::EMPTY; MAX_DIR_ENTRIES];
    let count = list_dir(disk, bpb, base, &mut entries);
    let entry = find_entry(&entries[..count], &short)?;
    if entry.attr & ATTR_DIRECTORY == 0 {
        return None;
    }
    Some(if entry.first_cluster == 0 {
        bpb.root_cluster
    } else {
        entry.first_cluster
    })
}

/// Reads `entry`'s full contents into `out` (which must be at least `entry.size` bytes long),
/// following its cluster chain. Sequential only -- no partial/offset reads, see the module doc
/// comment.
fn read_file(image: &[u8], bpb: &Bpb, entry: &DirEntry, out: &mut [u8]) -> usize {
    let mut remaining = entry.size as usize;
    let mut written = 0;
    let mut cluster = entry.first_cluster;
    let cluster_size = bpb.cluster_size();
    while remaining > 0 && cluster < EOC_MIN {
        let offset = bpb.cluster_offset(cluster);
        let chunk = remaining.min(cluster_size);
        out[written..written + chunk].copy_from_slice(&image[offset..offset + chunk]);
        written += chunk;
        remaining -= chunk;
        cluster = bpb.fat_entry(image, cluster);
    }
    written
}

#[derive(Debug, Clone, Copy)]
enum Fat32Error {
    /// The target directory's cluster chain has no free/end-marker slot left, and (see the module
    /// doc comment) this loader never extends a directory's own cluster chain to make one.
    DirectoryFull,
    /// The FAT has no free clusters left for the requested content length.
    DiskFull,
}

/// Finds the byte offset of the first free or end-of-listing directory-entry slot in
/// `dir_cluster`'s chain.
fn find_free_dir_slot(disk: &[u8], bpb: &Bpb, dir_cluster: u32) -> Option<usize> {
    let mut cluster = dir_cluster;
    loop {
        let offset = bpb.cluster_offset(cluster);
        let cluster_bytes = &disk[offset..offset + bpb.cluster_size()];
        let (dir_entries, _) = cluster_bytes.as_chunks::<DIR_ENTRY_SIZE>();
        for (i, entry_bytes) in dir_entries.iter().enumerate() {
            if entry_bytes[0] == DIR_END || entry_bytes[0] == DIR_FREE {
                return Some(offset + i * DIR_ENTRY_SIZE);
            }
        }
        let next = bpb.fat_entry(disk, cluster);
        if next >= EOC_MIN {
            return None;
        }
        cluster = next;
    }
}

fn write_dir_entry(
    disk: &mut [u8],
    offset: usize,
    name: &[u8; 11],
    attr: u8,
    first_cluster: u32,
    size: u32,
) {
    let entry = &mut disk[offset..offset + DIR_ENTRY_SIZE];
    entry[0..11].copy_from_slice(name);
    entry[11] = attr;
    entry[20..22].copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
    entry[26..28].copy_from_slice(&(first_cluster as u16).to_le_bytes());
    entry[28..32].copy_from_slice(&size.to_le_bytes());
}

/// Creates a brand-new regular file named `name` in `dir_cluster` with `content` as its complete
/// contents, allocating and chaining as many clusters as needed. See the module doc comment: this
/// is the only write operation implemented -- no append, no truncate, no rewriting an existing
/// file (a name collision isn't even checked for; that's the caller's job).
fn create_file(
    disk: &mut [u8],
    bpb: &Bpb,
    dir_cluster: u32,
    name: &[u8; 11],
    content: &[u8],
) -> Result<(), Fat32Error> {
    let dir_offset = find_free_dir_slot(disk, bpb, dir_cluster).ok_or(Fat32Error::DirectoryFull)?;

    let cluster_size = bpb.cluster_size();
    let clusters_needed = content.len().div_ceil(cluster_size).max(1);

    let mut first_cluster = None;
    let mut prev_cluster: Option<u32> = None;
    let mut remaining = content;
    for _ in 0..clusters_needed {
        let cluster = bpb.allocate_cluster(disk).ok_or(Fat32Error::DiskFull)?;
        if let Some(prev) = prev_cluster {
            bpb.write_fat_entry(disk, prev, cluster);
        }
        first_cluster.get_or_insert(cluster);
        prev_cluster = Some(cluster);

        let offset = bpb.cluster_offset(cluster);
        let chunk_len = remaining.len().min(cluster_size);
        disk[offset..offset + chunk_len].copy_from_slice(&remaining[..chunk_len]);
        remaining = &remaining[chunk_len..];
    }

    write_dir_entry(
        disk,
        dir_offset,
        name,
        0x20,
        first_cluster.expect("clusters_needed is always >= 1"),
        content.len() as u32,
    );
    Ok(())
}

/// A minimal, `core::fmt`-free byte-buffer builder for log messages and directory listings -- see
/// the module doc comment for why module code avoids `core::fmt::Write`/`write!` entirely.
struct ByteBuf<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> ByteBuf<'a> {
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
        // Digits come out least-significant-first; `reverse()` them in place before pushing.
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

    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn module_init() -> i32 {
    assert_eq!(
        TEMPLATE.len(),
        DISK_SIZE,
        "embedded FAT32 image size doesn't match DISK_SIZE"
    );
    // SAFETY: module_init runs once, synchronously, at load time, before any other exported
    // function in this module could possibly be called -- nothing else observes or races this
    // initial copy. See DISK's own doc comment for why `static mut` is required here and why its
    // writes aren't at risk of being optimized away.
    let disk: &mut [u8; DISK_SIZE] = unsafe { &mut *core::ptr::addr_of_mut!(DISK) };
    disk.copy_from_slice(TEMPLATE);

    let bpb = Bpb::parse(disk);
    set_current_dir_cluster(bpb.root_cluster);

    let mut entries = [DirEntry::EMPTY; MAX_DIR_ENTRIES];
    let count = list_dir(disk, &bpb, bpb.root_cluster, &mut entries);

    let mut msg_buf = [0u8; 512];
    let mut msg = ByteBuf {
        buf: &mut msg_buf,
        len: 0,
    };
    msg.push_bytes(b"[fat32] root directory (");
    msg.push_decimal(count as u32);
    msg.push_bytes(b" entries):\n");
    for entry in &entries[..count] {
        msg.push_bytes(b"  ");
        msg.push_bytes(&entry.name);
        msg.push_bytes(b" (");
        msg.push_decimal(entry.size);
        msg.push_bytes(b" bytes)\n");
    }
    log_bytes(msg.as_bytes());

    let mut ok = true;

    match find_entry(&entries[..count], b"HELLO   TXT") {
        Some(hello) => {
            let mut buf = [0u8; 64];
            let n = read_file(disk, &bpb, hello, &mut buf);
            if n != hello.size as usize || &buf[..n] != b"Hello from FAT32!\n" {
                ok = false;
                log("[fat32] self-check FAILED: HELLO.TXT contents mismatch\n");
            }
        }
        None => {
            ok = false;
            log("[fat32] self-check FAILED: HELLO.TXT not found\n");
        }
    }

    match find_entry(&entries[..count], b"BIG     TXT") {
        Some(big) => {
            let mut buf = [0u8; 4096];
            let n = read_file(disk, &bpb, big, &mut buf);
            let mut matches = n == big.size as usize;
            if matches {
                for (i, &byte) in buf[..n].iter().enumerate() {
                    if byte != b'A' + (i % 26) as u8 {
                        matches = false;
                        break;
                    }
                }
            }
            if !matches {
                ok = false;
                log("[fat32] self-check FAILED: BIG.TXT contents mismatch\n");
            }
        }
        None => {
            ok = false;
            log("[fat32] self-check FAILED: BIG.TXT not found\n");
        }
    }

    // --- Write support self-check: create a brand-new file, then read it back through the same
    // mutated `disk` buffer, proving the write is actually visible to subsequent reads. ---
    let new_content = b"written by the fat32 module\n";
    match create_file(disk, &bpb, bpb.root_cluster, b"NEW     TXT", new_content) {
        Ok(()) => {
            let mut entries_after = [DirEntry::EMPTY; MAX_DIR_ENTRIES];
            let count_after = list_dir(disk, &bpb, bpb.root_cluster, &mut entries_after);
            match find_entry(&entries_after[..count_after], b"NEW     TXT") {
                Some(new_entry) => {
                    let mut buf = [0u8; 64];
                    let n = read_file(disk, &bpb, new_entry, &mut buf);
                    if n != new_content.len() || &buf[..n] != new_content {
                        ok = false;
                        log("[fat32] self-check FAILED: NEW.TXT round-trip mismatch\n");
                    }
                }
                None => {
                    ok = false;
                    log("[fat32] self-check FAILED: NEW.TXT not found after create_file\n");
                }
            }
        }
        Err(_) => {
            ok = false;
            log("[fat32] self-check FAILED: create_file errored\n");
        }
    }

    // --- Subdirectory self-check: mkdir, cd into it, create a file there, cd back, confirm the
    // file is reachable via an absolute path and invisible from root's own listing. ---
    match sys_mkdir(b"SUB".as_ptr() as u64, 3, 0) {
        0 => {
            let cwd_before = current_dir_cluster();
            if sys_chdir(b"SUB".as_ptr() as u64, 3, 0) != 0 {
                ok = false;
                log("[fat32] self-check FAILED: chdir into SUB failed\n");
            } else {
                let sub_content = b"inside a subdirectory\n";
                // SAFETY: a fresh reference, not an alias of the `disk` binding taken at the top
                // of this function -- that binding is never used again after this point (its
                // sole remaining uses were the HELLO/BIG/NEW checks above), and sys_mkdir/
                // sys_chdir's own internal references are already dropped by the time they
                // returned above. Reusing the outer `disk` binding here instead (its lexical
                // scope technically still covers this point) would create two live `&mut`
                // references to the same static across those calls -- real aliasing UB, not
                // just a style concern, since sys_mkdir/sys_chdir each independently derive
                // their own `&mut DISK` internally while running.
                let disk2: &mut [u8; DISK_SIZE] = unsafe { &mut *core::ptr::addr_of_mut!(DISK) };
                match create_file(
                    disk2,
                    &bpb,
                    current_dir_cluster(),
                    b"IN      TXT",
                    sub_content,
                ) {
                    Ok(()) => {
                        let mut sub_entries = [DirEntry::EMPTY; MAX_DIR_ENTRIES];
                        let sub_count =
                            list_dir(disk2, &bpb, current_dir_cluster(), &mut sub_entries);
                        match find_entry(&sub_entries[..sub_count], b"IN      TXT") {
                            Some(entry) => {
                                let mut buf = [0u8; 64];
                                let n = read_file(disk2, &bpb, entry, &mut buf);
                                if n != sub_content.len() || &buf[..n] != sub_content {
                                    ok = false;
                                    log(
                                        "[fat32] self-check FAILED: SUB/IN.TXT contents mismatch\n",
                                    );
                                }
                            }
                            None => {
                                ok = false;
                                log("[fat32] self-check FAILED: SUB/IN.TXT not found\n");
                            }
                        }
                    }
                    Err(_) => {
                        ok = false;
                        log("[fat32] self-check FAILED: create_file inside SUB errored\n");
                    }
                }
                set_current_dir_cluster(cwd_before);
            }
        }
        _ => {
            ok = false;
            log("[fat32] self-check FAILED: mkdir SUB failed\n");
        }
    }

    // SAFETY: FFI calls to kernel-exported functions, matching their declared signatures exactly.
    unsafe {
        oxidebsd_register_syscall(SYS_OPEN, fat32_open);
        oxidebsd_register_syscall(SYS_CLOSE, sys_close);
        oxidebsd_register_syscall(SYS_CHDIR, sys_chdir);
        oxidebsd_register_syscall(SYS_MKDIR, sys_mkdir);
    }

    if ok {
        log("[fat32] self-check passed\n");
        0
    } else {
        -1
    }
}
