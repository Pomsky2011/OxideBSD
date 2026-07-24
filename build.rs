//! Cross-builds the userland demo binaries under `userland/` and the kernel modules under
//! `modules/` so `src/main.rs` can embed them via `include_bytes!(env!(...))`. This keeps
//! `cargo build`/`cargo run`/`cargo test` working with no manual pre-step.
//!
//! Builds into target-dirs of their own (`target/userland`, `target/modules`), not the shared
//! workspace `target/` directory: cargo takes a lock on the target directory for the whole outer
//! build, including while this build script runs, so a nested `cargo build` sharing that same
//! target directory can deadlock waiting for a lock the outer, still-running build already holds.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let ring3_smoke_elf_path = build_userland_crate("ring3-smoke", "RING3_SMOKE_ELF_PATH");
    build_userland_crate("stsh", "STSH_ELF_PATH");
    build_userland_crate("fork-exec-smoke", "FORK_EXEC_SMOKE_ELF_PATH");

    build_module_crate("hello", "HELLO", &[]);
    build_module_crate("native_abi", "NATIVE_ABI", &[]);
    build_module_crate("posix_compat", "POSIX_COMPAT", &[]);
    build_module_crate("signal", "SIGNAL", &[]);

    // ring3-smoke is embedded into the FAT32 image below (as SMOKE.ELF) so stsh's fork+execve+wait
    // path has a real, already-working target it can run as an actual file, not just another
    // include_bytes!'d demo -- see CLAUDE.md's process/scheduler section.
    let ring3_smoke_elf = std::fs::read(&ring3_smoke_elf_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", ring3_smoke_elf_path.display()));

    // musl-smoke is a first real (patched) musl static binary -- see CLAUDE.md's musl section --
    // embedded into the FAT32 image below (as MUSL.ELF) the same way ring3-smoke is, so stsh's
    // existing fork+execve+wait path can run it as a real file with no separate boot-time wiring.
    let musl_sysroot = build_musl_sysroot();
    let musl_smoke_elf_path = build_musl_smoke(&musl_sysroot);
    let musl_smoke_elf = std::fs::read(&musl_smoke_elf_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", musl_smoke_elf_path.display()));

    // BusyBox applets ported to OxideBSD -- see CLAUDE.md's BusyBox section. Each is its own
    // genuinely standalone, single-applet static binary (not a multi-call `busybox` binary
    // dispatching on argv[0], which this codebase's execve doesn't support -- see
    // build_busybox_applet's own doc comment), embedded into the FAT32 image below as
    // <NAME>.ELF the same way SMOKE.ELF/MUSL.ELF already are. Data-driven (a plain list, not one
    // hand-duplicated block of variables per applet like the original TRUE/ECHO-only version of
    // this function) specifically so adding the next applet is a one-line addition here, not a
    // matching set of edits scattered across this function and generate_fat32_image below. Load
    // addresses continue the existing `0x<b|c|d...>00000` sequence every prior userland/BusyBox
    // binary in this codebase already claimed one of (see CLAUDE.md's "User-mode execution"
    // section) -- each must stay clear of every other one already in use.
    //
    // `cat` is the first applet added after `true`/`echo` that actually calls `open()` on a real
    // path -- see CLAUDE.md's musl-port section for the open()/SYS_OPEN argument-convention fix
    // (musl's `open()` is now patched to speak fat32_open's own (path_ptr, path_len, flags) wire
    // format directly) that had to land before this could work at all.
    //
    // "HUSH" (embedded as SH.ELF, not HUSH.ELF -- this codebase's own choice of filename, same as
    // every other applet here) is BusyBox's smaller/simpler shell, not "ASH" -- deliberately:
    // `CONFIG_HUSH_INTERACTIVE` is left off (`allnoconfig`'s own default), so hush just reads and
    // executes commands from stdin like a script, no prompt/readline/job-control machinery that
    // would need real termios/ioctl support this kernel doesn't have. See CLAUDE.md's BusyBox
    // section for what this needed: real pipe(2)/dup2(2) (modules/posix_compat, src/pipe.rs,
    // src/fd.rs), discovered the same iterative "boot and see what's unrecognized" way musl/cat's
    // own new syscalls were.
    // "FALSE"/"YES"/"MORE" continue the same load-address sequence right past HUSH's own
    // 0xe00000. `more`'s own isatty()/TIOCGWINSZ probe hits the same already-documented,
    // confirmed-harmless ioctl gap `cat`'s stdout write path already exercises (see the musl-port
    // section) -- without a real terminal, it just falls back to dumping the whole file, the same
    // shape `cat` already has.
    //
    // The next batch (`mkdir` through `uniq`) directly exercises the syscalls `modules/oxfs` added
    // over `modules/fat32` -- `mkdir`/`rmdir`/`rm`/`mv` map straight onto
    // `mkdir`/`rmdir`/`unlink`/`rename`, all real (`rm`'s directory-recursion mode, `-r`, isn't
    // exercised or expected to work -- it needs `lstat`/`readdir`, neither implemented). `cp`/
    // `touch` may call `fstat`/`utimensat`-family syscalls this kernel doesn't implement at all
    // (unmapped, so a real ENOSYS -- see CLAUDE.md's oxfs section on why `stat`/`fstat` was
    // deliberately skipped) and could misbehave or fail outright depending on how gracefully
    // BusyBox's own code tolerates that; not pre-verified line-by-line, same "boot it and see"
    // discovery process every applet before it went through. `head`/`tail`/`wc`/`cut`/`sort`/
    // `uniq` are plain stdin/stdout/file text tools needing nothing beyond `open`/`read`/`write`/
    // `close`; `basename`/`dirname`/`printf`/`seq` do no filesystem I/O at all beyond `write`ing
    // their result, the same shape `echo`/`true`/`false` already have. `kill` was included once
    // `modules/signal` (see CLAUDE.md) made real process signaling exist -- the gap that had
    // blocked it.
    //
    // Everything from `ADDGROUP` on is a later, much larger pass: once `SYS_STAT`/`SYS_FSTAT`/
    // `SYS_LSTAT`/`SYS_GETDENTS` existed (see CLAUDE.md's oxfs section), an exhaustive per-applet
    // build probe was run against every Kconfig applet symbol BusyBox's own `//applet:` source
    // markers define (393 candidates) using this exact recipe. **"Builds" is a much weaker bar
    // than "works"**: musl provides a fairly complete libc surface, so plenty of applets that make
    // no sense on this kernel (networking, mount, `/proc`-reading, uid/passwd-db tools) still
    // compile and link cleanly -- they just fail cleanly at runtime (usually `ENOSYS` from an
    // unregistered syscall). They're kept anyway (build success is the bar this pass used), but
    // every one is tagged with exactly what it's still missing in `docs/BUSYBOX_APPLETS.md`, along
    // with the full list -- and reasons -- for every candidate that didn't even build. Load
    // addresses use smaller (`0x40000`, not `0x100000`) steps than the original 24, purely to fit
    // this much larger roster comfortably below `module::MODULE_VA_BASE` (`0x10000000`) -- every
    // applet built so far stays well under 256 KiB. **All of these moved from their original,
    // smaller values** (`0xb00000`-`0x2200000` for the original 24) once the kernel's own image
    // grew past them -- see this array's own base (`0x4100000`) and `userland/ring3-smoke/
    // linker.ld`'s doc comment for the full story of why, and how to re-derive the safe floor
    // before trusting any of these numbers again.
    const BUSYBOX_APPLETS: &[(&str, &str, u64)] = &[
        ("TRUE", "true", 0x4100000),
        ("ECHO", "echo", 0x4200000),
        ("CAT", "cat", 0x4300000),
        ("HUSH", "sh", 0x4400000),
        ("FALSE", "false", 0x4500000),
        ("YES", "yes", 0x4600000),
        ("MORE", "more", 0x4700000),
        ("MKDIR", "mkdir", 0x4800000),
        ("RMDIR", "rmdir", 0x4900000),
        ("RM", "rm", 0x4a00000),
        ("MV", "mv", 0x4b00000),
        ("CP", "cp", 0x4c00000),
        ("TOUCH", "touch", 0x4d00000),
        ("HEAD", "head", 0x4e00000),
        ("TAIL", "tail", 0x4f00000),
        ("WC", "wc", 0x5000000),
        ("BASENAME", "basename", 0x5100000),
        ("DIRNAME", "dirname", 0x5200000),
        ("PRINTF", "printf", 0x5300000),
        ("SEQ", "seq", 0x5400000),
        ("CUT", "cut", 0x5500000),
        ("SORT", "sort", 0x5600000),
        ("UNIQ", "uniq", 0x5700000),
        ("KILL", "kill", 0x5800000),
    ];

    /// The second pass itself -- see `docs/BUSYBOX_APPLETS.md` for what every one of these
    /// actually needs at runtime (most need *something* OxideBSD doesn't implement yet; the
    /// doc tags each one) and for the full list of candidates that didn't even build.
    const BUSYBOX_APPLETS_PASS2: &[(&str, &str, u64)] = &[
        ("ADDGROUP", "addgroup", 0x5a00000),
        ("ADDUSER", "adduser", 0x5a40000),
        ("ADJTIMEX", "adjtimex", 0x5a80000),
        ("AR", "ar", 0x5ac0000),
        ("ARP", "arp", 0x5b00000),
        ("ARPING", "arping", 0x5b40000),
        ("ASCII", "ascii", 0x5b80000),
        ("ASH", "ash", 0x5bc0000),
        ("AWK", "awk", 0x5c00000),
        ("BASE32", "base32", 0x5c40000),
        ("BASE64", "base64", 0x5c80000),
        ("BASH_IS_ASH", "bash_ash", 0x5cc0000),
        ("BASH_IS_HUSH", "bash", 0x5d00000),
        ("BBCONFIG", "bbconfig", 0x5d40000),
        ("BB_ARCH", "arch", 0x5d80000),
        ("BB_SYSCTL", "sysctl", 0x5dc0000),
        ("BC", "bc", 0x5e00000),
        ("BLKID", "blkid", 0x5e40000),
        ("BOOTCHARTD", "bootchartd", 0x5e80000),
        ("BUNZIP2", "bunzip2", 0x5ec0000),
        ("BZCAT", "bzcat", 0x5f00000),
        ("BZIP2", "bzip2", 0x5f40000),
        ("CAL", "cal", 0x5f80000),
        ("CHAT", "chat", 0x5fc0000),
        ("CHATTR", "chattr", 0x6000000),
        ("CHGRP", "chgrp", 0x6040000),
        ("CHMOD", "chmod", 0x6080000),
        ("CHOWN", "chown", 0x60c0000),
        ("CHPASSWD", "chpasswd", 0x6100000),
        ("CHROOT", "chroot", 0x6140000),
        ("CHRT", "chrt", 0x6180000),
        ("CHVT", "chvt", 0x61c0000),
        ("CKSUM", "cksum", 0x6200000),
        ("CLEAR", "clear", 0x6240000),
        ("CMP", "cmp", 0x6280000),
        ("COMM", "comm", 0x62c0000),
        ("CPIO", "cpio", 0x6300000),
        ("CRC32", "crc32", 0x6340000),
        ("CROND", "crond", 0x6380000),
        ("CRONTAB", "crontab", 0x63c0000),
        ("CTTYHACK", "cttyhack", 0x6400000),
        ("DATE", "date", 0x6440000),
        ("DC", "dc", 0x6480000),
        ("DD", "dd", 0x64c0000),
        ("DEALLOCVT", "deallocvt", 0x6500000),
        ("DELGROUP", "delgroup", 0x6540000),
        ("DEVFSD", "devfsd", 0x6580000),
        ("DEVMEM", "devmem", 0x65c0000),
        ("DF", "df", 0x6600000),
        ("DHCPRELAY", "dhcprelay", 0x6640000),
        ("DIFF", "diff", 0x6680000),
        ("DMESG", "dmesg", 0x66c0000),
        ("DNSD", "dnsd", 0x6700000),
        ("DNSDOMAINNAME", "dnsdomainname", 0x6740000),
        ("DOS2UNIX", "dos2unix", 0x6780000),
        ("DPKG", "dpkg", 0x67c0000),
        ("DPKG_DEB", "dpkg_deb", 0x6800000),
        ("DU", "du", 0x6840000),
        ("DUMPKMAP", "dumpkmap", 0x6880000),
        ("DUMPLEASES", "dumpleases", 0x68c0000),
        ("ED", "ed", 0x6900000),
        ("EGREP", "egrep", 0x6940000),
        ("EJECT", "eject", 0x6980000),
        ("ENV", "env", 0x69c0000),
        ("ENVUIDGID", "envuidgid", 0x6a00000),
        ("EXPAND", "expand", 0x6a40000),
        ("EXPR", "expr", 0x6a80000),
        ("FACTOR", "factor", 0x6ac0000),
        ("FAKEIDENTD", "fakeidentd", 0x6b00000),
        ("FALLOCATE", "fallocate", 0x6b40000),
        ("FATATTR", "fatattr", 0x6b80000),
        ("FBSET", "fbset", 0x6bc0000),
        ("FDFORMAT", "fdformat", 0x6c00000),
        ("FDISK", "fdisk", 0x6c40000),
        ("FGCONSOLE", "fgconsole", 0x6c80000),
        ("FGREP", "fgrep", 0x6cc0000),
        ("FIND", "find", 0x6d00000),
        ("FINDFS", "findfs", 0x6d40000),
        ("FLOCK", "flock", 0x6d80000),
        ("FOLD", "fold", 0x6dc0000),
        ("FREE", "free", 0x6e00000),
        ("FREERAMDISK", "freeramdisk", 0x6e40000),
        ("FSCK", "fsck", 0x6e80000),
        ("FSCK_MINIX", "fsck_minix", 0x6ec0000),
        ("FSYNC", "fsync", 0x6f00000),
        ("FTPD", "ftpd", 0x6f40000),
        ("FTPGET", "ftpget", 0x6f80000),
        ("FTPPUT", "ftpput", 0x6fc0000),
        ("FUSER", "fuser", 0x7000000),
        ("GETOPT", "getopt", 0x7040000),
        ("GETTY", "getty", 0x7080000),
        ("GREP", "grep", 0x70c0000),
        ("GROUPS", "groups", 0x7100000),
        ("GUNZIP", "gunzip", 0x7140000),
        ("GZIP", "gzip", 0x7180000),
        ("HALT", "halt", 0x71c0000),
        ("HD", "hd", 0x7200000),
        ("HEXDUMP", "hexdump", 0x7240000),
        ("HEXEDIT", "hexedit", 0x7280000),
        ("HOSTID", "hostid", 0x72c0000),
        ("HTTPD", "httpd", 0x7300000),
        ("HWCLOCK", "hwclock", 0x7340000),
        ("IFCONFIG", "ifconfig", 0x7380000),
        ("IFDOWN", "ifdown", 0x73c0000),
        ("INETD", "inetd", 0x7400000),
        ("INOTIFYD", "inotifyd", 0x7440000),
        ("INSTALL", "install", 0x7480000),
        ("IOSTAT", "iostat", 0x74c0000),
        ("IPCALC", "ipcalc", 0x7500000),
        ("IPCRM", "ipcrm", 0x7540000),
        ("IPCS", "ipcs", 0x7580000),
        ("KILLALL5", "killall5", 0x75c0000),
        ("KLOGD", "klogd", 0x7600000),
        ("LESS", "less", 0x7640000),
        ("LINK", "link", 0x7680000),
        ("LINUX32", "linux32", 0x76c0000),
        ("LINUX64", "linux64", 0x7700000),
        ("LN", "ln", 0x7740000),
        ("LOADKMAP", "loadkmap", 0x7780000),
        ("LOGGER", "logger", 0x77c0000),
        ("LOGIN", "login", 0x7800000),
        ("LOGNAME", "logname", 0x7840000),
        ("LOGREAD", "logread", 0x7880000),
        ("LPD", "lpd", 0x78c0000),
        ("LPQ", "lpq", 0x7900000),
        ("LPR", "lpr", 0x7940000),
        ("LS", "ls", 0x7980000),
        ("LSATTR", "lsattr", 0x79c0000),
        ("LSOF", "lsof", 0x7a00000),
        ("LSPCI", "lspci", 0x7a40000),
        ("LSSCSI", "lsscsi", 0x7a80000),
        ("LSUSB", "lsusb", 0x7ac0000),
        ("LZCAT", "lzcat", 0x7b00000),
        ("LZOP", "lzop", 0x7b40000),
        ("MAKEDEVS", "makedevs", 0x7b80000),
        ("MAKEMIME", "makemime", 0x7bc0000),
        ("MAN", "man", 0x7c00000),
        ("MD5SUM", "md5sum", 0x7c40000),
        ("MESG", "mesg", 0x7c80000),
        ("MICROCOM", "microcom", 0x7cc0000),
        ("MINIPS", "minips", 0x7d00000),
        ("MKFIFO", "mkfifo", 0x7d40000),
        ("MKFS_MINIX", "mkfs", 0x7d80000),
        ("MKNOD", "mknod", 0x7dc0000),
        ("MKPASSWD", "mkpasswd", 0x7e00000),
        ("MKSWAP", "mkswap", 0x7e40000),
        ("MKTEMP", "mktemp", 0x7e80000),
        ("MODINFO", "modinfo", 0x7ec0000),
        ("MOUNT", "mount", 0x7f00000),
        ("MOUNTPOINT", "mountpoint", 0x7f40000),
        ("MPSTAT", "mpstat", 0x7f80000),
        ("MT", "mt", 0x7fc0000),
        ("NC", "nc", 0x8000000),
        ("NETCAT", "netcat", 0x8040000),
        ("NETSTAT", "netstat", 0x8080000),
        ("NICE", "nice", 0x80c0000),
        ("NL", "nl", 0x8100000),
        ("NMETER", "nmeter", 0x8140000),
        ("NOHUP", "nohup", 0x8180000),
        ("NPROC", "nproc", 0x81c0000),
        ("NSENTER", "nsenter", 0x8200000),
        ("NSLOOKUP", "nslookup", 0x8240000),
        ("NTPD", "ntpd", 0x8280000),
        ("NUKE", "nuke", 0x82c0000),
        ("OD", "od", 0x8300000),
        ("PASSWD", "passwd", 0x8340000),
        ("PASTE", "paste", 0x8380000),
        ("PATCH", "patch", 0x83c0000),
        ("PGREP", "pgrep", 0x8400000),
        ("PIDOF", "pidof", 0x8440000),
        ("PING", "ping", 0x8480000),
        ("PIPE_PROGRESS", "pipe_progress", 0x84c0000),
        ("PIVOT_ROOT", "pivot_root", 0x8500000),
        ("PKILL", "pkill", 0x8540000),
        ("PMAP", "pmap", 0x8580000),
        ("POPMAILDIR", "popmaildir", 0x85c0000),
        ("POWEROFF", "poweroff", 0x8600000),
        ("POWERTOP", "powertop", 0x8640000),
        ("PRINTENV", "printenv", 0x8680000),
        ("PSCAN", "pscan", 0x86c0000),
        ("PSTREE", "pstree", 0x8700000),
        ("PWD", "pwd", 0x8740000),
        ("PWDX", "pwdx", 0x8780000),
        ("RDATE", "rdate", 0x87c0000),
        ("RDEV", "rdev", 0x8800000),
        ("READLINK", "readlink", 0x8840000),
        ("READPROFILE", "readprofile", 0x8880000),
        ("REALPATH", "realpath", 0x88c0000),
        ("REFORMIME", "reformime", 0x8900000),
        ("REMOVE_SHELL", "remove", 0x8940000),
        ("RENICE", "renice", 0x8980000),
        ("RESET", "reset", 0x89c0000),
        ("RESIZE", "resize", 0x8a00000),
        ("RESUME", "resume", 0x8a40000),
        ("REV", "rev", 0x8a80000),
        ("ROUTE", "route", 0x8ac0000),
        ("RPM", "rpm", 0x8b00000),
        ("RPM2CPIO", "rpm2cpio", 0x8b40000),
        ("RTCWAKE", "rtcwake", 0x8b80000),
        ("RUNSV", "runsv", 0x8bc0000),
        ("RUNSVDIR", "runsvdir", 0x8c00000),
        ("RUN_PARTS", "run", 0x8c40000),
        ("RX", "rx", 0x8c80000),
        ("SCRIPT", "script", 0x8cc0000),
        ("SCRIPTREPLAY", "scriptreplay", 0x8d00000),
        ("SED", "sed", 0x8d40000),
        ("SENDMAIL", "sendmail", 0x8d80000),
        ("SETARCH", "setarch", 0x8dc0000),
        ("SETCONSOLE", "setconsole", 0x8e00000),
        ("SETFATTR", "setfattr", 0x8e40000),
        ("SETKEYCODES", "setkeycodes", 0x8e80000),
        ("SETLOGCONS", "setlogcons", 0x8ec0000),
        ("SETPRIV", "setpriv", 0x8f00000),
        ("SETSERIAL", "setserial", 0x8f40000),
        ("SETSID", "setsid", 0x8f80000),
        ("SETUIDGID", "setuidgid", 0x8fc0000),
        ("SHA1SUM", "sha1sum", 0x9000000),
        ("SHA256SUM", "sha256sum", 0x9040000),
        ("SHA3SUM", "sha3sum", 0x9080000),
        ("SHA512SUM", "sha512sum", 0x90c0000),
        ("SHRED", "shred", 0x9100000),
        ("SHUF", "shuf", 0x9140000),
        ("SLEEP", "sleep", 0x9180000),
        ("SMEMCAP", "smemcap", 0x91c0000),
        ("SOFTLIMIT", "softlimit", 0x9200000),
        ("SPLIT", "split", 0x9240000),
        ("SSL_CLIENT", "ssl_client", 0x9280000),
        ("START_STOP_DAEMON", "start", 0x92c0000),
        ("STAT", "stat", 0x9300000),
        ("STRINGS", "strings", 0x9340000),
        ("STTY", "stty", 0x9380000),
        ("SU", "su", 0x93c0000),
        ("SULOGIN", "sulogin", 0x9400000),
        ("SUM", "sum", 0x9440000),
        ("SVLOGD", "svlogd", 0x9480000),
        ("SVOK", "svok", 0x94c0000),
        ("SWAPOFF", "swapoff", 0x9500000),
        ("SWITCH_ROOT", "switch_root", 0x9540000),
        ("SYNC", "sync", 0x9580000),
        ("SYSLOGD", "syslogd", 0x95c0000),
        ("TAC", "tac", 0x9600000),
        ("TAR", "tar", 0x9640000),
        ("TASKSET", "taskset", 0x9680000),
        ("TCPSVD", "tcpsvd", 0x96c0000),
        ("TEE", "tee", 0x9700000),
        ("TELNET", "telnet", 0x9740000),
        ("TELNETD", "telnetd", 0x9780000),
        ("TEST", "test", 0x97c0000),
        ("TIME", "time", 0x9800000),
        ("TIMEOUT", "timeout", 0x9840000),
        ("TOP", "top", 0x9880000),
        ("TR", "tr", 0x98c0000),
        ("TRACEROUTE", "traceroute", 0x9900000),
        ("TREE", "tree", 0x9940000),
        ("TRUNCATE", "truncate", 0x9980000),
        ("TS", "ts", 0x99c0000),
        ("TSORT", "tsort", 0x9a00000),
        ("TTY", "tty", 0x9a40000),
        ("TTYSIZE", "ttysize", 0x9a80000),
        ("UDHCPD", "udhcpd", 0x9ac0000),
        ("UDPSVD", "udpsvd", 0x9b00000),
        ("UMOUNT", "umount", 0x9b40000),
        ("UNCOMPRESS", "uncompress", 0x9b80000),
        ("UNEXPAND", "unexpand", 0x9bc0000),
        ("UNIT_TEST", "unit", 0x9c00000),
        ("UNIX2DOS", "unix2dos", 0x9c40000),
        ("UNLINK", "unlink", 0x9c80000),
        ("UNLZMA", "unlzma", 0x9cc0000),
        ("UNSHARE", "unshare", 0x9d00000),
        ("UNXZ", "unxz", 0x9d40000),
        ("UNZIP", "unzip", 0x9d80000),
        ("UPTIME", "uptime", 0x9dc0000),
        ("USLEEP", "usleep", 0x9e00000),
        ("UUDECODE", "uudecode", 0x9e40000),
        ("UUENCODE", "uuencode", 0x9e80000),
        ("VCONFIG", "vconfig", 0x9ec0000),
        ("VI", "vi", 0x9f00000),
        ("VOLNAME", "volname", 0x9f40000),
        ("WATCH", "watch", 0x9f80000),
        ("WGET", "wget", 0x9fc0000),
        ("WHICH", "which", 0xa000000),
        ("WHOAMI", "whoami", 0xa040000),
        ("WHOIS", "whois", 0xa080000),
        ("XARGS", "xargs", 0xa0c0000),
        ("XXD", "xxd", 0xa100000),
        ("XZCAT", "xzcat", 0xa140000),
        ("ZCAT", "zcat", 0xa180000),
    ];

    // `BUSYBOX_APPLETS` above is a `&[(&str, &str, u64)]`, not `&[(&str, &str, u64); N]` --
    // `const` slice concatenation isn't expressible without either unstable const-eval tricks or a
    // build-dependency, so the ~300-entry second pass lives in its own array
    // (`BUSYBOX_APPLETS_PASS2`, right below) and gets flattened into one iterator at the actual
    // build-loop call site instead of at the `const` declaration itself.

    let busybox_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("third_party/busybox");
    println!("cargo:rerun-if-changed={}", busybox_dir.display());
    let busybox_source_mtime = latest_mtime(&busybox_dir);

    // Parallel across applets, `-j1` within each one (see `build_busybox_applet`'s own doc
    // comment) -- a plain work-stealing pool over a shared atomic index, not a thread per applet
    // (~300 of those would vastly oversubscribe an 8-core host) and not a chunked static split
    // (uneven applet build times would leave some workers idle while others queue up).
    let all_applets: Vec<(&str, &str, u64)> = BUSYBOX_APPLETS
        .iter()
        .copied()
        .chain(BUSYBOX_APPLETS_PASS2.iter().copied())
        .collect();
    let jobs = std::thread::available_parallelism().map_or(1, |n| n.get());
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(&(applet_symbol, out_name, load_addr)) = all_applets.get(i) else {
                    break;
                };
                build_busybox_applet(
                    applet_symbol,
                    out_name,
                    load_addr,
                    &musl_sysroot,
                    busybox_source_mtime,
                );
            });
        }
    });

    // modules/fat32 is kept in the workspace but no longer loaded at boot (see CLAUDE.md's oxfs
    // section) -- still built here unmodified so it keeps compiling and self-checking on every
    // `cargo build`, a still-working format-correctness proof, just not the live filesystem.
    // Deliberately passed an empty applet slice, not `&busybox_applet_elfs`: `BUSYBOX_APPLETS` grew
    // to ~300 entries once this build script started probing/embedding every applet that happens to
    // build (see CLAUDE.md's BusyBox section) -- `busybox_short_name`'s 8.3-short-name format can't
    // hold names over 8 characters at all (a real `assert!`, not a soft limit) and the image's own
    // fixed `FAT32_TOTAL_SECTORS` budget was sized for a much smaller roster. FAT32 not being the
    // live filesystem means neither limit is worth designing around just to keep embedding
    // applets nothing ever loads from this image.
    let fat32_image_path = write_fat32_image(&ring3_smoke_elf, &musl_smoke_elf, &[]);
    build_module_crate(
        "fat32",
        "FAT32",
        &[("FAT32_IMAGE_PATH", fat32_image_path.to_str().unwrap())],
    );

    // oxfs: the real, live filesystem now (see CLAUDE.md's oxfs section). Unlike FAT32, there's no
    // on-disk image format to generate -- oxfs's own module_init populates its inode table directly
    // via ordinary function calls, using each already-built ELF's path passed straight through as
    // its own env var (the same extra_env mechanism FAT32_IMAGE_PATH above already uses). Built
    // from BUSYBOX_APPLETS itself (not one hand-written `let ..._elf_path = ...` line per applet)
    // so the next applet added there doesn't need a matching edit here too -- `oxfs_env_var_name`
    // derives each one's `OXFS_<NAME>_ELF_PATH` env var straight from its own `out_name`, with one
    // explicit exception ("sh" -> "HUSH", matching `modules/oxfs/src/lib.rs`'s existing
    // `OXFS_HUSH_ELF_PATH`/`seed_file(root, b"sh.elf", ...)` naming, itself inherited from this
    // applet's own Kconfig symbol `HUSH`, not its embedded filename).
    let hush_elf_path_for_main = target_dir_busybox_elf("sh");
    println!("cargo:rustc-env=HUSH_ELF_PATH={hush_elf_path_for_main}");
    let oxfs_applet_paths: Vec<(String, String)> = all_applets
        .iter()
        .map(|&(_, out_name, _)| {
            (
                oxfs_env_var_name(out_name),
                target_dir_busybox_elf(out_name),
            )
        })
        .collect();
    let mut oxfs_extra_env: Vec<(&str, &str)> = vec![
        (
            "OXFS_SMOKE_ELF_PATH",
            ring3_smoke_elf_path.to_str().unwrap(),
        ),
        ("OXFS_MUSL_ELF_PATH", musl_smoke_elf_path.to_str().unwrap()),
    ];
    oxfs_extra_env.extend(
        oxfs_applet_paths
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str())),
    );
    build_module_crate("oxfs", "OXFS", &oxfs_extra_env);
}

fn oxfs_env_var_name(out_name: &str) -> String {
    let suffix = if out_name == "sh" {
        "HUSH".to_string()
    } else {
        out_name.to_uppercase()
    };
    format!("OXFS_{suffix}_ELF_PATH")
}

/// Each `BUSYBOX_APPLETS` entry's own out-of-tree build directory follows a fixed, predictable
/// shape (`target/busybox-<out_name>/busybox`, `build_busybox_applet`'s own `out_dir.join("busybox")`
/// return value) -- re-derived here rather than plumbed through as a second return value, since
/// `busybox_applet_elfs` (built above) only kept the *bytes*, not the path, and oxfs's own
/// `extra_env` needs a path string, not bytes.
fn target_dir_busybox_elf(out_name: &str) -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    Path::new(manifest_dir)
        .join(format!("target/busybox-{out_name}"))
        .join("busybox")
        .to_str()
        .unwrap()
        .to_string()
}

/// Configures, builds, and installs the vendored, OxideBSD-patched musl (`third_party/musl` -- a
/// submodule pointing at a personal fork, patched on its own `oxidebsd` branch to speak this
/// kernel's native ABI directly -- see `CLAUDE.md`'s musl section) into `target/musl-sysroot`,
/// producing a `musl-gcc`-style wrapper this build script can shell out to for
/// `userland/musl-smoke/`. Uses musl's own build system directly (`configure`/`make`/
/// `make install`) -- there's no Cargo/Rust involved at all, it's a plain C library. Skips
/// `./configure` if a `config.mak` already exists (configure itself takes several seconds
/// re-probing the host compiler on every run; `make`/`make install` are already fast, idempotent
/// no-ops when nothing changed, so only configure needs this guard).
fn build_musl_sysroot() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let musl_dir = Path::new(manifest_dir).join("third_party/musl");
    let sysroot = Path::new(manifest_dir).join("target/musl-sysroot");

    println!(
        "cargo:rerun-if-changed={}",
        musl_dir.join("arch/x86_64").display()
    );
    println!("cargo:rerun-if-changed={}", musl_dir.join("src").display());

    if !musl_dir.join("config.mak").exists() {
        // Run via its own path (not `sh configure`, and not a bare relative "configure"): the
        // script derives its own source directory from `${0%/configure}` (build.rs:201 in
        // musl's own configure) -- given anything that doesn't literally end in "/configure",
        // that substitution is a no-op and it tries to `cd` into a nonexistent directory named
        // after whatever `$0` was. `./configure` (a real, executable path ending in "/configure")
        // is the one invocation shape that satisfies its own self-location logic.
        let status = Command::new("./configure")
            .current_dir(&musl_dir)
            .args([
                "--disable-shared",
                &format!("--prefix={}", sysroot.display()),
            ])
            .status()
            .unwrap_or_else(|e| panic!("failed to run musl's configure: {e}"));
        if !status.success() {
            panic!("musl configure failed: {status}");
        }
    }

    let jobs = std::thread::available_parallelism().map_or(1, |n| n.get());
    let status = Command::new("make")
        .current_dir(&musl_dir)
        .args(["-j", &jobs.to_string()])
        .status()
        .unwrap_or_else(|e| panic!("failed to run make for musl: {e}"));
    if !status.success() {
        panic!("musl build failed: {status}");
    }

    let status = Command::new("make")
        .current_dir(&musl_dir)
        .arg("install")
        .status()
        .unwrap_or_else(|e| panic!("failed to run make install for musl: {e}"));
    if !status.success() {
        panic!("musl install failed: {status}");
    }

    sysroot
}

/// Cross-builds `userland/musl-smoke/main.c` against `sysroot` (see `build_musl_sysroot` above),
/// at a load address (`0x40c0000`, 64 MiB + `0xc0000`) clear of both the kernel's own image (the
/// actually-binding constraint today, not the bootloader's fixed ~6 MiB identity-mapped
/// low-memory region -- see `userland/ring3-smoke/linker.ld`'s own comment for the full story of
/// why this floor moved from an original `0xa00000` and how to re-derive it) and every other
/// userland crate's load base (`0x4000000`-`0x4080000`) -- confirmed empirically via `readelf -hl`
/// before this was written, the same discipline CLAUDE.md's own `ring3-smoke` load-address
/// collision story already established. Unlike every other `userland/*` crate this isn't a Rust
/// crate at all -- musl-smoke exists specifically to
/// exercise a real musl static binary, so it's built with `musl-gcc` directly, no cargo involved.
fn build_musl_smoke(sysroot: &Path) -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let src = Path::new(manifest_dir).join("userland/musl-smoke/main.c");
    let target_dir = Path::new(manifest_dir).join("target/musl-smoke");
    std::fs::create_dir_all(&target_dir).expect("failed to create target/musl-smoke");
    let out = target_dir.join("musl-smoke");

    println!("cargo:rerun-if-changed={}", src.display());

    let musl_gcc = sysroot.join("bin/musl-gcc");
    let status = Command::new(&musl_gcc)
        .arg("-static")
        .arg("-no-pie")
        .arg("-Wl,-Ttext-segment=0x40c0000")
        .arg("-O2")
        .arg("-o")
        .arg(&out)
        .arg(&src)
        .status()
        .unwrap_or_else(|e| panic!("failed to run musl-gcc for musl-smoke: {e}"));
    if !status.success() {
        panic!("building musl-smoke failed: {status}");
    }
    out
}

/// Cross-builds a single, standalone BusyBox applet (`applet_symbol`, e.g. `"TRUE"`/`"ECHO"` --
/// the exact Kconfig symbol name, confirmed against the vendored source's own `//config:` comments
/// in `coreutils/true.c`/`coreutils/echo.c`) against the musl sysroot `build_musl_sysroot` already
/// produced -- BusyBox's own build system (`make`), not Cargo, the same "shell out to the real
/// toolchain" idiom `build_musl_smoke` already uses for a single `.c` file.
///
/// Follows BusyBox's own documented recipe for a minimal, non-interactive single-applet config --
/// the comment at `third_party/busybox/scripts/kconfig/Makefile:22`: `make allnoconfig`, flip the
/// one applet's config line to `=y` by hand, then build directly. Confirmed empirically (not just
/// followed blindly) that this produces a real `NUM_APPLETS == 1` build -- checked below via
/// `include/NUM_APPLETS.h` -- which is what makes BusyBox's own `main()` (`libbb/appletlib.c`)
/// skip all argv[0]/basename-based applet dispatch entirely and call the applet's `_main` function
/// directly (the `SINGLE_APPLET_MAIN` path). That matters specifically because this codebase's
/// `execve` doesn't pass a real, chosen argv[0] through at all yet (see CLAUDE.md's BusyBox
/// section) -- a multi-applet `busybox` binary relying on argv[0] to pick an applet wouldn't work
/// here at all, but a genuinely single-applet binary doesn't need argv[0] for anything.
///
/// `allnoconfig`'s own defaults also have to be overridden for the "which shell provides `sh`"
/// choice (`SH_IS_ASH` by default, never `SH_IS_NONE`) -- left alone, that default drags in a
/// second applet (`ash`) and `NUM_APPLETS` becomes 2, not 1 (confirmed the hard way; BusyBox's own
/// `make_single_applets.sh` script carries a comment about this exact same trap).
///
/// **Staleness-checked, unlike `build_musl_sysroot`'s own `config.mak`-exists guard**: skips the
/// entire `allnoconfig`/flip/`oldconfig`/`make` sequence if `out_dir`'s own `busybox` binary
/// already exists and is newer than `busybox_source_mtime` (the latest mtime across all of
/// `third_party/busybox`, computed once by the caller -- see `latest_mtime`'s own doc comment --
/// not per applet), `build.rs` itself (so editing *this file's own recipe* -- flips, load
/// address, applet roster -- invalidates every cached binary too, not just a real source edit),
/// and `musl_sysroot`'s own `libc.a` (every applet links against it, so a musl source edit has to
/// invalidate cached applet binaries too -- missed in an earlier version of this function, which
/// only compared the first two and left every already-built applet silently linked against a
/// stale libc after a musl-side fix).
/// Went from "always regenerate, `allnoconfig` is roughly a second" to this once `BUSYBOX_APPLETS`
/// grew from ~24 entries to ~300 (see CLAUDE.md's BusyBox section): at that scale, "roughly a
/// second" per applet is minutes added to *every* `cargo build`, even when nothing changed.
/// `out_name` is used both for this applet's own `target/busybox-<out_name>` out-of-tree (`O=`)
/// build directory and to describe it in panics; `load_addr` becomes its
/// `-Wl,-Ttext-segment=` link address, which -- like every other userland binary's load base in
/// this codebase -- must stay clear of every other one already claimed.
fn build_busybox_applet(
    applet_symbol: &str,
    out_name: &str,
    load_addr: u64,
    musl_sysroot: &Path,
    busybox_source_mtime: std::time::SystemTime,
) -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let busybox_dir = Path::new(manifest_dir).join("third_party/busybox");
    let out_dir = Path::new(manifest_dir).join(format!("target/busybox-{out_name}"));
    let binary_path = out_dir.join("busybox");

    let build_rs_mtime = std::fs::metadata(Path::new(manifest_dir).join("build.rs"))
        .and_then(|m| m.modified())
        .unwrap_or(std::time::SystemTime::now());
    // Every applet links against musl_sysroot's own libc.a -- a musl source edit (this branch
    // patches it directly, see CLAUDE.md's musl section) has to invalidate cached applet binaries
    // too, or they keep silently linking a stale libc. Missed once already: a getdents64 fix here
    // rebuilt musl but every already-built applet's cached binary looked "fresh" regardless, since
    // only busybox_source_mtime/build_rs_mtime were ever compared.
    let musl_mtime = std::fs::metadata(musl_sysroot.join("lib/libc.a"))
        .and_then(|m| m.modified())
        .unwrap_or(std::time::SystemTime::now());
    let freshness_floor = busybox_source_mtime.max(build_rs_mtime).max(musl_mtime);
    if let Ok(modified) = std::fs::metadata(&binary_path).and_then(|m| m.modified()) {
        if modified >= freshness_floor {
            return binary_path;
        }
    }

    std::fs::create_dir_all(&out_dir)
        .unwrap_or_else(|e| panic!("failed to create {}: {e}", out_dir.display()));

    let out_arg = format!("O={}", out_dir.display());
    let status = Command::new("make")
        .current_dir(&busybox_dir)
        .arg(&out_arg)
        .arg("allnoconfig")
        .status()
        .unwrap_or_else(|e| panic!("failed to run make allnoconfig for busybox {out_name}: {e}"));
    if !status.success() {
        panic!("busybox allnoconfig for {out_name} failed: {status}");
    }

    configure_busybox_single_applet(&out_dir, applet_symbol);
    // Every applet's config tree can grow a cascade of newly-visible sub-options once its own
    // symbol flips on (not just HUSH's own FEATURE_EDITING/HUSH_INTERACTIVE tree -- see
    // `resolve_busybox_new_config_options`'s own doc comment) once the roster covers ~300 applets
    // instead of two dozen hand-picked ones, so this now runs unconditionally rather than being
    // special-cased to `applet_symbol == "HUSH"`.
    resolve_busybox_new_config_options(&busybox_dir, &out_arg);

    let musl_gcc = musl_sysroot.join("bin/musl-gcc");
    // `-j1`, not `available_parallelism()`: the real concurrency now comes from building many
    // applets at once (see this function's caller in `main`), not from parallelizing one applet's
    // own handful of source files -- oversubscribing both levels at once (N applets each spawning
    // N compiler jobs) thrashes far more than it helps.
    let status = Command::new("make")
        .current_dir(&busybox_dir)
        .arg(&out_arg)
        .arg(format!("CC={}", musl_gcc.display()))
        .arg(format!(
            "EXTRA_LDFLAGS=-static -no-pie -Wl,-Ttext-segment={load_addr:#x}"
        ))
        .args(["-j", "1"])
        .status()
        .unwrap_or_else(|e| panic!("failed to run make for busybox {out_name}: {e}"));
    if !status.success() {
        panic!("building busybox {out_name} failed: {status}");
    }

    let num_applets_path = out_dir.join("include/NUM_APPLETS.h");
    let num_applets = std::fs::read_to_string(&num_applets_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", num_applets_path.display()));
    assert!(
        num_applets.trim() == "#define NUM_APPLETS 1",
        "busybox {out_name} build produced {:?} ({}), not a standalone single-applet binary -- \
         argv[0]-based applet dispatch would be required, which this codebase's execve doesn't \
         support",
        num_applets.trim(),
        num_applets_path.display()
    );

    out_dir.join("busybox")
}

/// Rewrites the `.config` `make allnoconfig` (in `out_dir`) just produced so exactly one applet
/// (`applet_symbol`) plus `CONFIG_STATIC` are enabled and the `sh`-provider choice is forced to
/// `SH_IS_NONE` (see `build_busybox_applet`'s own doc comment for why) -- by direct text
/// replacement of the exact lines `allnoconfig` is known (confirmed empirically) to produce,
/// rather than shelling out to `sed` as BusyBox's own documented recipe does, so a shape this
/// doesn't expect fails loudly (the `assert!` below) instead of silently doing nothing.
///
/// For `HUSH` specifically (only), also flips on real interactive mode -- `CONFIG_HUSH_INTERACTIVE`
/// (prompt + `$-`), `CONFIG_HUSH_JOB` (needed to reach `hush`'s own real `FEATURE_EDITING`
/// initialization at all -- see CLAUDE.md's "Interactive shell" section for the exact code path
/// traced through `shell/hush.c` that makes this true, and why enabling it doesn't actually
/// require real job control to work despite the name: `tcgetpgrp` cleanly failing, via this
/// kernel's own `ENOTTY` for the unimplemented `TIOCGPGRP` request, degrades `hush`'s own job-
/// control setup into a no-op rather than a crash), `CONFIG_FEATURE_EDITING` (real line editing),
/// and `CONFIG_FEATURE_EDITING_FANCY_PROMPT` (a real `$PWD $`-style `PS1`, not a blank prompt).
/// Left off deliberately: `CONFIG_HUSH_SAVEHISTORY`/`CONFIG_FEATURE_EDITING_SAVEHISTORY` (no
/// `HISTFILE` persistence -- in-session history only, one less thing to get right this pass) and
/// `CONFIG_FEATURE_EDITING_WINCH` (nothing in this kernel ever sends `SIGWINCH`, so tracking it
/// would be pure unused surface). Every other applet stays exactly as narrow as it already was.
fn configure_busybox_single_applet(out_dir: &Path, applet_symbol: &str) {
    let config_path = out_dir.join(".config");
    let mut config = std::fs::read_to_string(&config_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", config_path.display()));
    let mut flips = vec![
        (
            format!("# CONFIG_{applet_symbol} is not set"),
            format!("CONFIG_{applet_symbol}=y"),
        ),
        (
            "# CONFIG_STATIC is not set".to_string(),
            "CONFIG_STATIC=y".to_string(),
        ),
        (
            "CONFIG_SH_IS_ASH=y".to_string(),
            "# CONFIG_SH_IS_ASH is not set".to_string(),
        ),
        (
            "# CONFIG_SH_IS_NONE is not set".to_string(),
            "CONFIG_SH_IS_NONE=y".to_string(),
        ),
        // Real usage text for `--help` (e.g. `cat`'s own "Usage: cat [FILE]... Concatenate
        // FILEs..."), not the generic "No help available." `bb_show_usage` falls back to when
        // this is off -- `allnoconfig` disables both despite their own `default y`, the same way
        // it disables everything else this function already has to flip back on. Discovered as a
        // real gap, not preemptively enabled: `cat.elf --help` printed nothing at all until
        // src/syscall.rs's stderr fix (fd 2) landed, and even with that fix would have only shown
        // the generic fallback without this -- see CLAUDE.md's BusyBox section.
        (
            "# CONFIG_SHOW_USAGE is not set".to_string(),
            "CONFIG_SHOW_USAGE=y".to_string(),
        ),
        (
            "# CONFIG_FEATURE_VERBOSE_USAGE is not set".to_string(),
            "CONFIG_FEATURE_VERBOSE_USAGE=y".to_string(),
        ),
    ];
    if applet_symbol == "HUSH" {
        flips.extend([
            (
                "# CONFIG_HUSH_INTERACTIVE is not set".to_string(),
                "CONFIG_HUSH_INTERACTIVE=y".to_string(),
            ),
            (
                "# CONFIG_HUSH_JOB is not set".to_string(),
                "CONFIG_HUSH_JOB=y".to_string(),
            ),
            (
                "# CONFIG_FEATURE_EDITING is not set".to_string(),
                "CONFIG_FEATURE_EDITING=y".to_string(),
            ),
            (
                "# CONFIG_FEATURE_EDITING_FANCY_PROMPT is not set".to_string(),
                "CONFIG_FEATURE_EDITING_FANCY_PROMPT=y".to_string(),
            ),
        ]);
    }
    for (from, to) in flips {
        assert!(
            config.contains(&from),
            "busybox .config for {applet_symbol} is missing the expected line {from:?} -- \
             allnoconfig's output shape may have changed"
        );
        config = config.replacen(&from, &to, 1);
    }
    std::fs::write(&config_path, config)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", config_path.display()));
}

/// Turning on `CONFIG_HUSH_INTERACTIVE`/`CONFIG_HUSH_JOB`/`CONFIG_FEATURE_EDITING` makes a whole
/// tree of previously-invisible sub-options (`FEATURE_EDITING_MAX_LEN`, `FEATURE_EDITING_HISTORY`,
/// `HUSH_BASH_COMPAT`, ...) newly visible -- Kconfig only ever emits a symbol into `.config` at
/// `allnoconfig` time if its own dependencies are already satisfied, so none of these existed as
/// lines `configure_busybox_single_applet`'s direct text-replacement approach could edit at all.
/// The normal build's own internal `silentoldconfig` step refuses to guess a default for a
/// genuinely new `int`/`string` option when stdin isn't a real terminal (`Console input/output is
/// redirected` -- a real failure hit and diagnosed live, not a hypothetical), so this runs an
/// explicit `make oldconfig` first, with stdin fed a large supply of blank lines (`\n`, matching
/// what pressing Enter at every prompt would do) rather than closed/`/dev/null` -- confirmed
/// empirically that `/dev/null` (immediate EOF) still hits the exact same "NEW... " hard failure
/// for `int`-typed options specifically (bool prompts alone tolerate EOF fine), while a live
/// stream of blank lines lets `conf` walk through and accept every prompt's own Kconfig-declared
/// default, `int`/`string` ones included, right through to a clean exit.
///
/// Originally only invoked for `HUSH` itself (`FEATURE_EDITING`/`HUSH_INTERACTIVE`'s own cascade
/// was the only one the original ~24-applet roster ever hit) -- now runs after every applet's
/// `configure_busybox_single_applet` call unconditionally, since a much broader roster (see
/// CLAUDE.md's BusyBox section) hits this same "single symbol flip reveals a whole options
/// sub-tree" shape for plenty of other applets too (anything pulling in its own `FEATURE_*` tree).
/// Cheap to run even when there's nothing new to resolve -- `conf` just exits quickly once no
/// prompt remains unanswered.
fn resolve_busybox_new_config_options(busybox_dir: &Path, out_arg: &str) {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new("make")
        .current_dir(busybox_dir)
        .arg(out_arg)
        .arg("oldconfig")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to run make oldconfig for busybox applet: {e}"));

    // A generous supply of "just press Enter" answers -- bounded (this config tree has, at most,
    // a few hundred prompts), written then dropped (closing the pipe) so `conf` sees EOF only
    // once every real prompt it could possibly ask has already been answered.
    let mut stdin = child.stdin.take().expect("child stdin was piped");
    let blank_lines = "\n".repeat(10_000);
    // A child process reading slower than this writes can deadlock a pipe write once the OS
    // buffer fills -- write from a separate thread so this function's own main-thread `wait()`
    // below can keep draining the child's stdout/stderr concurrently rather than blocking on it.
    std::thread::spawn(move || {
        let _ = stdin.write_all(blank_lines.as_bytes());
    });

    let output = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("failed to wait for busybox oldconfig: {e}"));
    if !output.status.success() {
        panic!(
            "busybox oldconfig failed: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Latest mtime across every real source file under `dir`, skipping VCS metadata (any directory
/// starting with `.`) -- computed once per `cargo build` invocation, not once per applet (see
/// `build_busybox_applet`'s own doc comment for why that distinction matters at ~300 applets). A
/// plain recursive walk rather than a crate dependency: this only runs when build.rs's own
/// `cargo:rerun-if-changed` gate already decided `third_party/busybox` changed, so it's a rare,
/// not hot-path, cost -- not worth a `walkdir`-style dependency for.
fn latest_mtime(dir: &Path) -> std::time::SystemTime {
    let mut latest = std::time::UNIX_EPOCH;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if entry
                .file_name()
                .to_str()
                .is_some_and(|s| s.starts_with('.'))
            {
                continue;
            }
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
                latest = latest.max(modified);
            }
        }
    }
    latest
}

/// Cross-builds the userland crate at `userland/<crate_name>/` and exposes its resulting ELF's
/// path via `cargo:rustc-env=<env_var>=<path>`, and returns that same path so callers that need
/// the raw bytes on the host side (`main`, for embedding `ring3-smoke` into the FAT32 image) don't
/// have to re-derive it.
fn build_userland_crate(crate_name: &str, env_var: &str) -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let userland_dir = Path::new(manifest_dir).join("userland").join(crate_name);
    let target_dir = Path::new(manifest_dir).join("target/userland");

    println!(
        "cargo:rerun-if-changed={}",
        userland_dir.join("src").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        userland_dir.join("Cargo.toml").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        userland_dir.join("linker.ld").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        userland_dir.join("build.rs").display()
    );

    let cargo = cargo_bin();
    let status = Command::new(&cargo)
        .current_dir(manifest_dir)
        .args([
            "build",
            "--manifest-path",
            userland_dir.join("Cargo.toml").to_str().unwrap(),
            "--release",
            "--target-dir",
            target_dir.to_str().unwrap(),
        ])
        // Building a userland crate is itself a `cargo build` invocation; without clearing these,
        // it inherits the outer build's CARGO_* env vars (wrong package name/manifest dir/etc.)
        // from when *this* build script was invoked.
        .env_remove("CARGO_MANIFEST_DIR")
        .env_remove("CARGO_PKG_NAME")
        .status()
        .unwrap_or_else(|e| panic!("failed to run cargo for {crate_name}: {e}"));

    if !status.success() {
        panic!("building the {crate_name} userland binary failed: {status}");
    }

    let elf_path: PathBuf = target_dir.join("x86_64-oxidebsd/release").join(crate_name);
    assert!(
        elf_path.exists(),
        "{crate_name} build reported success but {} doesn't exist",
        elf_path.display()
    );
    println!("cargo:rustc-env={env_var}={}", elf_path.display());
    elf_path
}

/// Cross-builds the kernel module crate at `modules/<crate_name>/` into a single relocatable
/// (`ET_REL`) object file, ready for `src/module.rs` to load and relocate at boot, and exposes it
/// via `cargo:rustc-env=<name_var>_MOD_PATH=<path>` (`name_var` is `env_var` upper-cased). See
/// `CLAUDE.md`'s module-loading section for the full rationale; in short:
///
/// - Module crates are plain `#![no_std]` `lib` crates -- no `_start`, no linker script, no final
///   link. `cargo rustc -- --emit=obj -C codegen-units=1` produces exactly one `ET_REL` object,
///   skipping the link step entirely.
/// - `-C relocation-model=static` (scoped to this nested build only, via `RUSTFLAGS`) keeps every
///   relocation a simple absolute/PC-relative form -- no GOT -- in exchange for requiring the
///   module's eventual mapped address to stay within the low 2 GiB (see `src/module.rs`'s
///   `MODULE_VA_BASE`).
/// - The module's own object alone has an open-ended, code-content-dependent set of undefined
///   symbols (anything from `memcpy` to `core::fmt::write` to panic machinery, depending on what
///   the module's code happens to do) -- not something a hand-curated kernel API table can
///   practically enumerate in advance. A build-time partial relink (`rust-lld -r`, not a final
///   link) against the exact `core`/`alloc`/`compiler_builtins` rlibs this same build produced
///   closes over all of that, leaving only the module's genuine calls into the kernel API plus
///   one fixed, compiler-synthesized panic-entry symbol (discovered below, not hardcoded, since
///   its exact mangled name is toolchain-dependent) unresolved.
///
/// `extra_env` is passed straight through to the nested `cargo rustc` invocation -- used by the
/// `fat32` module to receive its generated disk image's path (`FAT32_IMAGE_PATH`) for its own
/// `include_bytes!(env!("FAT32_IMAGE_PATH"))`, since that module has no `build.rs` of its own
/// (modules never do -- there's no linker script to pass, they're never linked at all).
fn build_module_crate(crate_name: &str, env_var: &str, extra_env: &[(&str, &str)]) {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let module_dir = Path::new(manifest_dir).join("modules").join(crate_name);
    let target_dir = Path::new(manifest_dir).join("target/modules");

    println!(
        "cargo:rerun-if-changed={}",
        module_dir.join("src").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        module_dir.join("Cargo.toml").display()
    );

    let cargo = cargo_bin();
    let mut command = Command::new(&cargo);
    command
        .current_dir(manifest_dir)
        .args([
            "rustc",
            "--manifest-path",
            module_dir.join("Cargo.toml").to_str().unwrap(),
            "--release",
            "--lib",
            "--target-dir",
            target_dir.to_str().unwrap(),
            "--",
            "--emit=obj",
            "-C",
            "codegen-units=1",
        ])
        .env_remove("CARGO_MANIFEST_DIR")
        .env_remove("CARGO_PKG_NAME")
        // See the doc comment above: eliminates GOT-indirected relocations everywhere, including
        // inside the precompiled core/alloc this nested `-Z build-std` invocation produces (which
        // doesn't inherit the trailing `--emit=obj`-style flags, only RUSTFLAGS).
        .env("RUSTFLAGS", "-C relocation-model=static");
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let status = command
        .status()
        .unwrap_or_else(|e| panic!("failed to run cargo rustc for module {crate_name}: {e}"));

    if !status.success() {
        panic!("building the {crate_name} module's object file failed: {status}");
    }

    let deps_dir = target_dir.join("x86_64-oxidebsd/release/deps");
    let module_obj =
        newest_matching(&deps_dir, &format!("{crate_name}-"), ".o").unwrap_or_else(|| {
            panic!(
                "{crate_name}: no object file found in {}",
                deps_dir.display()
            )
        });

    let sysroot = rustc_output(manifest_dir, &["--print", "sysroot"]);
    let host = host_triple(manifest_dir);
    let llvm_bin = Path::new(&sysroot)
        .join("lib/rustlib")
        .join(&host)
        .join("bin");

    let merged_obj = target_dir.join(format!("{crate_name}-merged.o"));
    partial_link(crate_name, &llvm_bin, &deps_dir, &module_obj, &merged_obj);

    let panic_symbol = discover_panic_symbol(&llvm_bin, &merged_obj);

    println!(
        "cargo:rustc-env={env_var}_MOD_PATH={}",
        merged_obj.display()
    );
    println!(
        "cargo:rustc-env={env_var}_MOD_PANIC_SYMBOL={}",
        panic_symbol.as_deref().unwrap_or("")
    );
}

/// Merges `module_obj` with the exact `core`/`alloc`/`compiler_builtins` rlibs found in
/// `deps_dir` via a relocatable ("partial") link -- `-r`, not a final link -- so that any symbol
/// the module's code references from those crates resolves at build time. Archive members are
/// pulled in only if actually referenced (ordinary linker semantics), wrapped in
/// `--start-group`/`--end-group` since `core`/`alloc`/`compiler_builtins` reference each other
/// and a single pass wouldn't otherwise guarantee a resolving order.
///
/// `--gc-sections -u module_init`: archive-member selection is coarse (a whole `.o` file, which
/// for `-Z build-std`'s own precompiled `core`/`alloc` can bundle many unrelated functions
/// together), so referencing just one symbol from a bundled member can otherwise pull in
/// everything else defined alongside it. This was discovered as a real, non-optional requirement
/// (not the "nice to have, defer it" size optimization an earlier draft of this design assumed
/// it'd be) when `modules/fat32/`'s very first boot attempt exhausted the kernel's small heap:
/// referencing `core::panicking::panic_bounds_check` (reachable from any ordinary slice
/// indexing) alone pulled in most of `core::fmt`'s numeric/Unicode tables, ballooning that one
/// module to 3+ MB across ~2900 sections. `-u module_init` marks every module's sole real entry
/// point as a GC root (`-r` produces no executable with an implicit entry point of its own, so
/// nothing is reachable by default) -- `--gc-sections` then prunes every section not transitively
/// reachable from it, which brought that same object down to ~60 sections.
fn partial_link(
    crate_name: &str,
    llvm_bin: &Path,
    deps_dir: &Path,
    module_obj: &Path,
    merged_obj: &Path,
) {
    let lld = llvm_bin.join("rust-lld");
    assert!(
        lld.exists(),
        "rust-lld not found at {} -- is the llvm-tools-preview rustup component installed? \
         (see rust-toolchain.toml)",
        lld.display()
    );

    let find_rlib = |name: &str| {
        newest_matching(deps_dir, &format!("lib{name}-"), ".rlib").unwrap_or_else(|| {
            panic!(
                "{crate_name}: no {name} rlib found in {} -- is `-Z build-std` producing one?",
                deps_dir.display()
            )
        })
    };
    let core_rlib = find_rlib("core");
    let alloc_rlib = find_rlib("alloc");
    let compiler_builtins_rlib = find_rlib("compiler_builtins");

    let status = Command::new(&lld)
        .args([
            "-flavor",
            "gnu",
            "-r",
            "--gc-sections",
            "-u",
            "module_init",
            "-o",
            merged_obj.to_str().unwrap(),
            module_obj.to_str().unwrap(),
            "--start-group",
        ])
        .args([&core_rlib, &alloc_rlib, &compiler_builtins_rlib])
        .arg("--end-group")
        .status()
        .unwrap_or_else(|e| panic!("failed to run rust-lld for module {crate_name}: {e}"));

    if !status.success() {
        panic!("partial link for module {crate_name} failed: {status}");
    }
}

/// Scans `object`'s undefined symbols for the compiler-synthesized panic entry point
/// (`core::panicking`'s internal `rust_begin_unwind` declaration, called by every panicking-path
/// function `core`/`alloc` contain). Its exact mangled name embeds a crate-metadata hash that's
/// toolchain-dependent and not worth hardcoding -- `rust_begin_unwind` still appears as a literal
/// substring of the mangled name (Rust's v0 mangling spells out path components as length-prefixed
/// text), so a substring search is enough to find it reliably. Returns `None` if the module's code
/// never actually references it (e.g. no panicking-capable operations survived optimization) --
/// that's fine, `src/module.rs`'s resolver only needs entries for symbols a module actually uses.
fn discover_panic_symbol(llvm_bin: &Path, object: &Path) -> Option<String> {
    let nm = llvm_bin.join("llvm-nm");
    let output = Command::new(&nm)
        .args(["--undefined-only", "--format=just-symbols"])
        .arg(object)
        .output()
        .unwrap_or_else(|e| panic!("failed to run llvm-nm on {}: {e}", object.display()));
    assert!(
        output.status.success(),
        "llvm-nm failed for {}",
        object.display()
    );
    String::from_utf8(output.stdout)
        .expect("llvm-nm output wasn't valid UTF-8")
        .lines()
        .find(|line| line.contains("rust_begin_unwind"))
        .map(|s| s.trim().to_string())
}

/// Generates a small, deliberately non-spec-minimum-sized but structurally correct FAT32 disk
/// image (own code, not `mkfs.fat` -- see `CLAUDE.md`'s module-loading/FAT32 section for why:
/// hermeticity, and a real `mkfs.fat`-produced FAT32 volume needs to be tens of megabytes to meet
/// Microsoft's minimum-cluster-count heuristic, impractical to embed), writes it to
/// `target/modules/fat32.img`, and returns that path for `build_module_crate`'s `extra_env` to
/// pass through as `FAT32_IMAGE_PATH`. Real BPB/FSInfo, 2 FAT copies, 32-bit FAT entries, and the
/// root directory as a proper cluster chain (not FAT16's fixed region) -- only this kernel's own
/// hand-rolled parser (`modules/fat32/`) ever needs to read it, so the "real minimum size" rule is
/// safe to deliberately violate.
fn write_fat32_image(
    smoke_elf_bytes: &[u8],
    musl_elf_bytes: &[u8],
    busybox_applet_elfs: &[(&str, Vec<u8>)],
) -> PathBuf {
    let image = generate_fat32_image(smoke_elf_bytes, musl_elf_bytes, busybox_applet_elfs);
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let target_dir = Path::new(manifest_dir).join("target/modules");
    std::fs::create_dir_all(&target_dir).expect("failed to create target/modules");
    let path = target_dir.join("fat32.img");
    std::fs::write(&path, &image)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
    path
}

const FAT32_BYTES_PER_SECTOR: usize = 512;
const FAT32_SECTORS_PER_CLUSTER: u8 = 1;
const FAT32_RESERVED_SECTORS: u32 = 32;
const FAT32_NUM_FATS: u32 = 2;
/// 8 MiB total (raised from 2 MiB once `BUSYBOX_APPLETS` grew past its original four entries --
/// this image still embeds every one of them, even applets never actually loaded/used at boot,
/// see this constant's own module-level context) -- still far below the ~65525-cluster count real
/// FAT32 volumes are conventionally expected to have, deliberately (see this function's caller's
/// doc comment).
const FAT32_TOTAL_SECTORS: u32 = 16384;

const FAT32_ROOT_CLUSTER: u32 = 2;
const FAT32_HELLO_CLUSTER: u32 = 3;
const FAT32_BIG_FIRST_CLUSTER: u32 = 4;
const FAT32_BIG_CLUSTER_COUNT: u32 = 3;
/// SMOKE.ELF's cluster count isn't a fixed constant like BIG.TXT's -- it depends on the built
/// `ring3-smoke` ELF's actual size, computed at image-generation time from `smoke_elf_bytes.len()`.
const FAT32_SMOKE_FIRST_CLUSTER: u32 = FAT32_BIG_FIRST_CLUSTER + FAT32_BIG_CLUSTER_COUNT;
/// MUSL.ELF's own first cluster isn't a fixed constant either -- it starts right after however
/// many clusters SMOKE.ELF ends up needing, computed at image-generation time just like
/// `FAT32_SMOKE_FIRST_CLUSTER`'s own runtime-computed cluster count is chained onto BIG.TXT's.
const FAT32_EOC: u32 = 0x0FFF_FFFF;

const FAT32_HELLO_CONTENTS: &[u8] = b"Hello from FAT32!\n";
/// Deliberately a formula-derived pattern (`b'A' + index % 26`), not a literal, so
/// `modules/fat32`'s own self-check can independently recompute the expected bytes rather than
/// needing a second copy of a large literal kept in sync by hand.
const FAT32_BIG_FILE_LEN: usize = 1224;

fn fat32_big_file_byte(index: usize) -> u8 {
    b'A' + (index % 26) as u8
}

/// One BusyBox applet's placement in the image, computed by `generate_fat32_image` by folding
/// over `busybox_applet_elfs` in order -- each applet's first cluster starts right after the
/// previous one's chain ends, the same "chain on after whatever came before" pattern MUSL.ELF
/// itself already uses to chain on after SMOKE.ELF.
struct PlacedApplet<'a> {
    short_name: [u8; 11],
    bytes: &'a [u8],
    first_cluster: u32,
    cluster_count: u32,
}

/// Builds a FAT 8.3 short name (`"NAME    ELF"`) from an applet's lowercase `out_name` (e.g.
/// `"true"`) -- uppercased, space-padded to 8 characters, `ELF` extension. Panics if `out_name` is
/// too long for an 8.3 basename; every applet name this codebase embeds is short enough that this
/// is a real assertion, not defensive dead code.
fn busybox_short_name(out_name: &str) -> [u8; 11] {
    assert!(
        out_name.len() <= 8 && out_name.is_ascii(),
        "BusyBox applet name {out_name:?} doesn't fit an 8.3 short name"
    );
    let mut name = [b' '; 11];
    for (i, b) in out_name.bytes().enumerate() {
        name[i] = b.to_ascii_uppercase();
    }
    name[8..11].copy_from_slice(b"ELF");
    name
}

fn generate_fat32_image(
    smoke_elf_bytes: &[u8],
    musl_elf_bytes: &[u8],
    busybox_applet_elfs: &[(&str, Vec<u8>)],
) -> Vec<u8> {
    let smoke_cluster_count =
        (smoke_elf_bytes.len().div_ceil(FAT32_BYTES_PER_SECTOR) as u32).max(1);
    let musl_first_cluster = FAT32_SMOKE_FIRST_CLUSTER + smoke_cluster_count;
    let musl_cluster_count = (musl_elf_bytes.len().div_ceil(FAT32_BYTES_PER_SECTOR) as u32).max(1);

    // Each BusyBox applet (see CLAUDE.md's BusyBox section) chains on after the previous one --
    // MUSL.ELF for the first applet, the previous applet for every one after that.
    let mut placed_applets: Vec<PlacedApplet> = Vec::new();
    let mut next_free_cluster = musl_first_cluster + musl_cluster_count;
    for (out_name, elf_bytes) in busybox_applet_elfs {
        let cluster_count = (elf_bytes.len().div_ceil(FAT32_BYTES_PER_SECTOR) as u32).max(1);
        placed_applets.push(PlacedApplet {
            short_name: busybox_short_name(out_name),
            bytes: elf_bytes,
            first_cluster: next_free_cluster,
            cluster_count,
        });
        next_free_cluster += cluster_count;
    }

    // Solve for the FAT size (in sectors) that exactly covers the clusters left over once that
    // same FAT size is reserved -- a small fixed-point iteration, since the FAT's own size is
    // tiny relative to the volume and converges in only a couple of passes.
    let mut fat_size_sectors: u32 = 1;
    for _ in 0..8 {
        let data_sectors =
            FAT32_TOTAL_SECTORS - FAT32_RESERVED_SECTORS - FAT32_NUM_FATS * fat_size_sectors;
        let total_clusters = data_sectors / FAT32_SECTORS_PER_CLUSTER as u32;
        let fat_bytes_needed = (total_clusters + 2) * 4;
        fat_size_sectors = fat_bytes_needed.div_ceil(FAT32_BYTES_PER_SECTOR as u32);
    }
    let data_start_sector = FAT32_RESERVED_SECTORS + FAT32_NUM_FATS * fat_size_sectors;

    let highest_cluster_used = next_free_cluster - 1;
    let data_clusters =
        (FAT32_TOTAL_SECTORS - data_start_sector) / FAT32_SECTORS_PER_CLUSTER as u32;
    assert!(
        highest_cluster_used < 2 + data_clusters,
        "ring3-smoke ({} bytes) + musl-smoke ({} bytes) + {} BusyBox applet(s) ({} bytes total) \
         no longer fit in the embedded FAT32 image ({} total bytes) -- raise FAT32_TOTAL_SECTORS",
        smoke_elf_bytes.len(),
        musl_elf_bytes.len(),
        placed_applets.len(),
        placed_applets.iter().map(|a| a.bytes.len()).sum::<usize>(),
        FAT32_TOTAL_SECTORS as usize * FAT32_BYTES_PER_SECTOR
    );

    let mut image = vec![0u8; FAT32_TOTAL_SECTORS as usize * FAT32_BYTES_PER_SECTOR];

    // --- Boot sector / BPB (sector 0) ---
    {
        let bs = &mut image[0..FAT32_BYTES_PER_SECTOR];
        bs[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]); // BS_jmpBoot
        bs[3..11].copy_from_slice(b"OXIDEBSD"); // BS_OEMName
        bs[11..13].copy_from_slice(&(FAT32_BYTES_PER_SECTOR as u16).to_le_bytes()); // BPB_BytsPerSec
        bs[13] = FAT32_SECTORS_PER_CLUSTER; // BPB_SecPerClus
        bs[14..16].copy_from_slice(&(FAT32_RESERVED_SECTORS as u16).to_le_bytes()); // BPB_RsvdSecCnt
        bs[16] = FAT32_NUM_FATS as u8; // BPB_NumFATs
        // BPB_RootEntCnt (17..19) and BPB_TotSec16 (19..21) are 0 for FAT32.
        bs[21] = 0xF8; // BPB_Media (fixed disk)
        // BPB_FATSz16 (22..24) is 0 for FAT32 -- BPB_FATSz32 below is authoritative.
        bs[24..26].copy_from_slice(&32u16.to_le_bytes()); // BPB_SecPerTrk (dummy geometry)
        bs[26..28].copy_from_slice(&64u16.to_le_bytes()); // BPB_NumHeads (dummy geometry)
        bs[32..36].copy_from_slice(&FAT32_TOTAL_SECTORS.to_le_bytes()); // BPB_TotSec32
        bs[36..40].copy_from_slice(&fat_size_sectors.to_le_bytes()); // BPB_FATSz32
        bs[44..48].copy_from_slice(&FAT32_ROOT_CLUSTER.to_le_bytes()); // BPB_RootClus
        bs[48..50].copy_from_slice(&1u16.to_le_bytes()); // BPB_FSInfo (sector 1)
        bs[50..52].copy_from_slice(&6u16.to_le_bytes()); // BPB_BkBootSec (sector 6)
        bs[64] = 0x80; // BS_DrvNum
        bs[66] = 0x29; // BS_BootSig (marks VolID/VolLab/FilSysType below as valid)
        bs[67..71].copy_from_slice(&0x0BAD_F32Fu32.to_le_bytes()); // BS_VolID
        bs[71..82].copy_from_slice(b"OXIDEBSD FS"); // BS_VolLab (11 bytes)
        bs[82..90].copy_from_slice(b"FAT32   "); // BS_FilSysType (informational only)
        bs[510] = 0x55;
        bs[511] = 0xAA;
    }

    // --- FSInfo sector (sector 1) --- structural authenticity only: modules/fat32's own parser
    // never reads this (real FAT32 drivers treat it as a non-authoritative performance hint), so
    // its free-cluster fields are left "unknown" rather than computed precisely.
    {
        let fs = &mut image[FAT32_BYTES_PER_SECTOR..2 * FAT32_BYTES_PER_SECTOR];
        fs[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes()); // LeadSig
        fs[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes()); // StrucSig
        fs[488..492].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // Free_Count (unknown)
        fs[492..496].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // Next_Free (unknown)
        fs[508..512].copy_from_slice(&0xAA55_0000u32.to_le_bytes()); // TrailSig
    }

    // --- Backup boot sector (sector 6, per BPB_BkBootSec) ---
    {
        let (before, after) = image.split_at_mut(6 * FAT32_BYTES_PER_SECTOR);
        after[0..FAT32_BYTES_PER_SECTOR].copy_from_slice(&before[0..FAT32_BYTES_PER_SECTOR]);
    }

    // --- FAT tables (both copies kept identical) ---
    for fat_index in 0..FAT32_NUM_FATS {
        write_fat_entry(&mut image, fat_index, fat_size_sectors, 0, 0x0FFF_FFF8);
        write_fat_entry(&mut image, fat_index, fat_size_sectors, 1, 0x0FFF_FFFF);
        write_fat_entry(
            &mut image,
            fat_index,
            fat_size_sectors,
            FAT32_ROOT_CLUSTER,
            FAT32_EOC,
        );
        write_fat_entry(
            &mut image,
            fat_index,
            fat_size_sectors,
            FAT32_HELLO_CLUSTER,
            FAT32_EOC,
        );
        for i in 0..FAT32_BIG_CLUSTER_COUNT {
            let cluster = FAT32_BIG_FIRST_CLUSTER + i;
            let value = if i + 1 == FAT32_BIG_CLUSTER_COUNT {
                FAT32_EOC
            } else {
                cluster + 1
            };
            write_fat_entry(&mut image, fat_index, fat_size_sectors, cluster, value);
        }
        for i in 0..smoke_cluster_count {
            let cluster = FAT32_SMOKE_FIRST_CLUSTER + i;
            let value = if i + 1 == smoke_cluster_count {
                FAT32_EOC
            } else {
                cluster + 1
            };
            write_fat_entry(&mut image, fat_index, fat_size_sectors, cluster, value);
        }
        for i in 0..musl_cluster_count {
            let cluster = musl_first_cluster + i;
            let value = if i + 1 == musl_cluster_count {
                FAT32_EOC
            } else {
                cluster + 1
            };
            write_fat_entry(&mut image, fat_index, fat_size_sectors, cluster, value);
        }
        for applet in &placed_applets {
            for i in 0..applet.cluster_count {
                let cluster = applet.first_cluster + i;
                let value = if i + 1 == applet.cluster_count {
                    FAT32_EOC
                } else {
                    cluster + 1
                };
                write_fat_entry(&mut image, fat_index, fat_size_sectors, cluster, value);
            }
        }
    }

    let cluster_offset = |cluster: u32| -> usize {
        (data_start_sector as usize + (cluster as usize - 2) * FAT32_SECTORS_PER_CLUSTER as usize)
            * FAT32_BYTES_PER_SECTOR
    };

    // --- Root directory (cluster 2): volume label + three file entries ---
    {
        let root_offset = cluster_offset(FAT32_ROOT_CLUSTER);
        let mut entry_offset = root_offset;
        write_dir_entry(&mut image, entry_offset, b"OXIDEBSD FS", 0x08, 0, 0);
        entry_offset += 32;
        write_dir_entry(
            &mut image,
            entry_offset,
            b"HELLO   TXT",
            0x20,
            FAT32_HELLO_CLUSTER,
            FAT32_HELLO_CONTENTS.len() as u32,
        );
        entry_offset += 32;
        write_dir_entry(
            &mut image,
            entry_offset,
            b"BIG     TXT",
            0x20,
            FAT32_BIG_FIRST_CLUSTER,
            FAT32_BIG_FILE_LEN as u32,
        );
        entry_offset += 32;
        write_dir_entry(
            &mut image,
            entry_offset,
            b"SMOKE   ELF",
            0x20,
            FAT32_SMOKE_FIRST_CLUSTER,
            smoke_elf_bytes.len() as u32,
        );
        entry_offset += 32;
        write_dir_entry(
            &mut image,
            entry_offset,
            b"MUSL    ELF",
            0x20,
            musl_first_cluster,
            musl_elf_bytes.len() as u32,
        );
        for applet in &placed_applets {
            entry_offset += 32;
            write_dir_entry(
                &mut image,
                entry_offset,
                &applet.short_name,
                0x20,
                applet.first_cluster,
                applet.bytes.len() as u32,
            );
        }
        // No further entries -- the byte after this one is already 0 (image starts zeroed),
        // which is the FAT directory end-of-listing marker.
    }

    // --- HELLO.TXT contents ---
    {
        let offset = cluster_offset(FAT32_HELLO_CLUSTER);
        image[offset..offset + FAT32_HELLO_CONTENTS.len()].copy_from_slice(FAT32_HELLO_CONTENTS);
    }

    // --- BIG.TXT contents (spans multiple clusters, exercising chain-following) ---
    {
        let mut remaining = FAT32_BIG_FILE_LEN;
        let mut written = 0usize;
        for i in 0..FAT32_BIG_CLUSTER_COUNT {
            let cluster = FAT32_BIG_FIRST_CLUSTER + i;
            let offset = cluster_offset(cluster);
            let chunk_len = remaining.min(FAT32_BYTES_PER_SECTOR);
            for j in 0..chunk_len {
                image[offset + j] = fat32_big_file_byte(written + j);
            }
            written += chunk_len;
            remaining -= chunk_len;
        }
    }

    // --- SMOKE.ELF contents (the built ring3-smoke binary, chunked across smoke_cluster_count
    // clusters exactly like BIG.TXT's chain above, generalized for an arbitrary byte length) ---
    {
        for (i, chunk) in smoke_elf_bytes.chunks(FAT32_BYTES_PER_SECTOR).enumerate() {
            let cluster = FAT32_SMOKE_FIRST_CLUSTER + i as u32;
            let offset = cluster_offset(cluster);
            image[offset..offset + chunk.len()].copy_from_slice(chunk);
        }
    }

    // --- MUSL.ELF contents (the built musl-smoke binary -- see CLAUDE.md's musl section --
    // chunked the same way SMOKE.ELF's own bytes are above) ---
    {
        for (i, chunk) in musl_elf_bytes.chunks(FAT32_BYTES_PER_SECTOR).enumerate() {
            let cluster = musl_first_cluster + i as u32;
            let offset = cluster_offset(cluster);
            image[offset..offset + chunk.len()].copy_from_slice(chunk);
        }
    }

    // --- BusyBox applet contents (see CLAUDE.md's BusyBox section -- chunked the same way
    // SMOKE.ELF/MUSL.ELF's own bytes are above) ---
    for applet in &placed_applets {
        for (i, chunk) in applet.bytes.chunks(FAT32_BYTES_PER_SECTOR).enumerate() {
            let cluster = applet.first_cluster + i as u32;
            let offset = cluster_offset(cluster);
            image[offset..offset + chunk.len()].copy_from_slice(chunk);
        }
    }

    image
}

fn write_fat_entry(
    image: &mut [u8],
    fat_index: u32,
    fat_size_sectors: u32,
    cluster: u32,
    value: u32,
) {
    let fat_start =
        (FAT32_RESERVED_SECTORS + fat_index * fat_size_sectors) as usize * FAT32_BYTES_PER_SECTOR;
    let offset = fat_start + cluster as usize * 4;
    image[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_dir_entry(
    image: &mut [u8],
    offset: usize,
    name_11: &[u8; 11],
    attr: u8,
    first_cluster: u32,
    size: u32,
) {
    let entry = &mut image[offset..offset + 32];
    entry[0..11].copy_from_slice(name_11);
    entry[11] = attr;
    entry[20..22].copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
    entry[26..28].copy_from_slice(&(first_cluster as u16).to_le_bytes());
    entry[28..32].copy_from_slice(&size.to_le_bytes());
}

/// Finds the file matching `<prefix>*<suffix>` most recently modified in `dir` -- filenames under
/// `deps/` carry a non-deterministic metadata hash, so an exact name can't be predicted, and
/// picking the newest (rather than asserting exactly one) tolerates stale artifacts left behind by
/// a prior build with different flags reusing the same target-dir.
fn newest_matching(dir: &Path, prefix: &str, suffix: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(prefix) && name.ends_with(suffix)
        })
        .max_by_key(|entry| entry.metadata().and_then(|m| m.modified()).ok())
        .map(|entry| entry.path())
}

fn cargo_bin() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

fn rustc_output(manifest_dir: &str, args: &[&str]) -> String {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let output = Command::new(&rustc)
        .current_dir(manifest_dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run rustc {args:?}: {e}"));
    assert!(output.status.success(), "rustc {args:?} failed");
    String::from_utf8(output.stdout)
        .expect("rustc output wasn't valid UTF-8")
        .trim()
        .to_string()
}

fn host_triple(manifest_dir: &str) -> String {
    let verbose = rustc_output(manifest_dir, &["-vV"]);
    verbose
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .expect("rustc -vV output missing a 'host:' line")
        .to_string()
}
