//! `SYS_CLOCK_GETTIME = 138` and `SYS_NANOSLEEP = 139` (see CLAUDE.md's "BusyBox gap analysis" —
//! the "clock + nanosleep" gap), continuing the sequence past `modules/posix_compat`'s
//! `SYS_UNAME = 137`. Same "module registers, kernel implements" split every other syscall module
//! in this codebase uses: real logic is kernel-resident, since this module can't use `alloc` (see
//! CLAUDE.md's module-loading section) —
//! `src/syscall.rs`'s `sys_clock_gettime` (backed by `src/pit.rs`'s reprogrammed timer tick rate
//! for `CLOCK_MONOTONIC` and `src/rtc.rs`'s CMOS read for `CLOCK_REALTIME`) for the former,
//! `src/process.rs`'s `do_nanosleep` (a real block-then-`scheduler::schedule()`, woken by
//! `interrupts::timer_interrupt_handler` once the requested tick deadline passes — genuinely
//! scheduler-resident work, not something this module could implement by itself) for the latter.
#![no_std]

unsafe extern "C" {
    fn oxidebsd_log(ptr: *const u8, len: u64);
    fn oxidebsd_register_syscall(
        number: u64,
        handler: extern "C" fn(u64, u64, u64, u64) -> i64,
    ) -> i32;
    fn oxidebsd_sys_clock_gettime(clockid: u64, ts_ptr: u64) -> i64;
    fn oxidebsd_sys_nanosleep(req_ptr: u64, rem_ptr: u64) -> i64;
}

fn log(message: &str) {
    unsafe { oxidebsd_log(message.as_ptr(), message.len() as u64) };
}

const SYS_CLOCK_GETTIME: u64 = 138;
const SYS_NANOSLEEP: u64 = 139;

extern "C" fn handle_clock_gettime(clockid: u64, ts_ptr: u64, _arg2: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_clock_gettime(clockid, ts_ptr) }
}

extern "C" fn handle_nanosleep(req_ptr: u64, rem_ptr: u64, _arg2: u64, _arg3: u64) -> i64 {
    unsafe { oxidebsd_sys_nanosleep(req_ptr, rem_ptr) }
}

#[unsafe(no_mangle)]
pub extern "C" fn module_init() -> i32 {
    unsafe {
        oxidebsd_register_syscall(SYS_CLOCK_GETTIME, handle_clock_gettime);
        oxidebsd_register_syscall(SYS_NANOSLEEP, handle_nanosleep);
    }
    log("[module] clock: module_init running (registered SYS_CLOCK_GETTIME/SYS_NANOSLEEP)\n");
    0
}
