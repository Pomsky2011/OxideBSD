//! Filesystem-adjacent primitives shared across independently-loaded kernel modules: the
//! per-process `(Pid, fd)` table (`fd`, the only coordination point between e.g. `oxfs` and
//! `native_abi` -- modules can't call each other directly), real blocking pipes (`pipe`), real
//! POSIX message queues (`mqueue`), and real SysV message queues (`sysv_msg` -- a distinct
//! subsystem with a distinct lifecycle, see that module's own doc comment). Real filesystems
//! themselves (`oxfs`, `fat32`) live in `modules/`, not here -- see CLAUDE.md's filesystem
//! section.

pub mod fd;
pub mod mqueue;
pub mod pipe;
pub mod sysv_msg;
