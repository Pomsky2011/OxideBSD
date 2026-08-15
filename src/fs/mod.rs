//! Filesystem-adjacent primitives shared across independently-loaded kernel modules: the
//! per-process `(Pid, fd)` table (`fd`, the only coordination point between e.g. `oxfs` and
//! `native_abi` -- modules can't call each other directly) and real blocking pipes (`pipe`). Real
//! filesystems themselves (`oxfs`, `fat32`) live in `modules/`, not here -- see CLAUDE.md's
//! filesystem section.

pub mod fd;
pub mod pipe;
