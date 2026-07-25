//! `SYS_CLOCK_GETTIME = 138` (see CLAUDE.md's "BusyBox gap analysis" — the "clock + nanosleep"
//! gap, clock half), continuing the sequence past `modules/posix_compat`'s `SYS_UNAME = 137`. Same
//! "module registers, kernel implements" split every other syscall module in this codebase uses:
//! real logic (`src/syscall.rs`'s `sys_clock_gettime`, backed by `src/pit.rs`'s reprogrammed timer
//! tick rate for `CLOCK_MONOTONIC` and `src/rtc.rs`'s CMOS read for `CLOCK_REALTIME`) is
//! kernel-resident, since this module can't use `alloc` (see CLAUDE.md's module-loading section).
//!
//! Deliberately doesn't register `nanosleep`/`clock_nanosleep` -- unlike a plain clock read, a
//! real sleep needs to actually block the calling process (a new `BlockReason::Sleeping`, woken
//! from the timer IRQ handler once its deadline passes), which is scheduler-resident work, not
//! something a syscall module can implement by itself.
#![no_std]

unsafe extern "C" {
    fn oxidebsd_log(ptr: *const u8, len: u64);
    fn oxidebsd_register_syscall(
        number: u64,
        handler: extern "C" fn(u64, u64, u64, u64) -> i64,
    ) -> i32;
    fn oxidebsd_sys_clock_gettime(clockid: u64, ts_ptr: u64) -> i64;
}

fn log(message: &str) {
    unsafe { oxidebsd_log(message.as_ptr(), message.len() as u64) };
}

const SYS_CLOCK_GETTIME: u64 = 138;

extern "C" fn handle_clock_gettime(clockid: u64, ts_ptr: u64, _arg2: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_clock_gettime(clockid, ts_ptr) }
}

#[unsafe(no_mangle)]
pub extern "C" fn module_init() -> i32 {
    unsafe {
        oxidebsd_register_syscall(SYS_CLOCK_GETTIME, handle_clock_gettime);
    }
    log("[module] clock: module_init running (registered SYS_CLOCK_GETTIME)\n");
    0
}
