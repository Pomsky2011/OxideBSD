//! Networking. Phase 1: PCI discovery (`crate::pci`) + a real NIC driver (`rtl8139`) sending and
//! receiving raw Ethernet frames, IRQ-driven. Phase 2: a real protocol stack on top of it
//! (`ethernet`/`arp`/`ipv4`/`icmp`) -- enough to answer/originate ICMP echo requests against real
//! (if virtualized) network traffic. No `modules/net` syscall shim yet -- see this repo's
//! networking plan for what's still deferred.

use crate::syscall::EINVAL;

pub mod arp;
pub mod ethernet;
pub mod icmp;
pub mod ipv4;
pub mod nic;
pub mod rtl8139;
pub mod tcp;
pub mod udp;

/// Drains every frame currently queued in the NIC's RX ring and dispatches each through the
/// protocol stack. Never blocks.
///
/// Not wired into the normal boot path yet -- nothing outside a dedicated test needs live
/// traffic processing until `modules/net`'s syscalls exist (a later phase) give userland a
/// reason to receive something. Callers today (`tests/icmp_smoke.rs`, `ipv4::send_packet`'s own
/// ARP-resolution wait) call this directly from their own loop, the same pattern
/// `tests/rtl8139_smoke.rs` established for raw frames.
pub fn poll() {
    tcp::check_retransmits();
    loop {
        let frame = {
            let mut guard = nic::NIC.lock();
            let Some(driver) = guard.as_mut() else {
                return;
            };
            driver.poll_recv()
        };
        match frame {
            Some(frame) => ethernet::handle_frame(&frame),
            None => return,
        }
    }
}

const POLLIN: i16 = 0x0001;
const POLLNVAL: i16 = 0x0020;

/// Real Linux/musl `struct pollfd` layout (`int fd; short events; short revents;`) -- no padding
/// needed, already 8-byte aligned as a whole.
#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

/// `SYS_POLL = 148` (see `bits/syscall.h.in`'s own comment on why `__NR_poll`'s real, unremapped
/// value can't be used here -- it collides with this ABI's own `SYS_WAIT4`). Exists to unblock a
/// real DNS resolver: musl's own stub resolver (`third_party/musl/src/network/res_msend.c`) is
/// already a real userspace UDP client built on `socket`/`sendto`/`recvfrom` -- it just also needs
/// `poll()` to multiplex retries across nameservers with a timeout, the one primitive this stack
/// didn't have yet.
///
/// Only ever reports `POLLIN` -- the only event class any fd in this kernel has real blocking
/// semantics for. A `real_fd` that doesn't belong to any protocol's socket table (a regular oxfs
/// file, a pipe, stdin, ...) is treated as always-ready, matching real POSIX behavior for regular
/// files and a reasonable stand-in for everything else this stack doesn't model blocking for.
pub extern "C" fn oxidebsd_sys_poll(fds_ptr: u64, nfds: u64, timeout_ms: u64) -> i64 {
    if fds_ptr == 0 && nfds > 0 {
        return -(EINVAL as i64);
    }
    // `timeout` is a signed `int` in the real ABI (`-1` means "block forever") -- R10/RDX only
    // ever carries its raw bit pattern, so reinterpret it here rather than truncating it to an
    // always-positive u64.
    let timeout_ms = timeout_ms as i32 as i64;
    let entries = unsafe { core::slice::from_raw_parts_mut(fds_ptr as *mut PollFd, nfds as usize) };

    // `crate::tsc`, not `crate::cpu::interrupts::ticks()`: this is a real syscall handler, and
    // `ticks()` is driven entirely by the timer IRQ, which can't fire for the syscall's *entire*
    // duration (`src/syscall.rs`'s SFMASK clears `RFLAGS::INTERRUPT_FLAG` at entry) -- a
    // tick-based deadline here would be frozen at the value it had when the syscall began and
    // could never actually elapse. Confirmed live: `tests/poll_syscall_smoke.rs`'s real `SYSCALL`
    // path hung solid on exactly this before `crate::tsc` existed -- see that module's own doc
    // comment. RDTSC keeps advancing regardless of the interrupt-enable state.
    let deadline =
        (timeout_ms >= 0).then(|| crate::cpu::tsc::now() + crate::cpu::tsc::ms_to_cycles(timeout_ms as u64));

    loop {
        poll(); // drain the NIC / run the protocol stack once per pass, same as recvfrom's self-poll
        let mut ready_count: i64 = 0;
        for entry in entries.iter_mut() {
            entry.revents = 0;
            if entry.fd < 0 {
                continue; // negative fd: real poll() skips these entirely, not an error
            }
            let Some(real_fd) = crate::fs::fd::real_fd_of(entry.fd as u64) else {
                entry.revents = POLLNVAL;
                ready_count += 1;
                continue;
            };
            let ready = udp::has_data_ready(real_fd)
                .or_else(|| tcp::has_data_ready(real_fd))
                .or_else(|| icmp::has_data_ready(real_fd))
                .unwrap_or(true);
            if ready {
                entry.revents = entry.events & POLLIN;
                ready_count += 1;
            }
        }
        if ready_count > 0 {
            return ready_count;
        }
        if deadline.is_some_and(|d| crate::cpu::tsc::now() >= d) {
            return 0;
        }
        // `hint::spin_loop()`, not `hlt()`: this is a real syscall handler, and
        // `src/syscall.rs`'s SFMASK setup clears `RFLAGS::INTERRUPT_FLAG` for the syscall's
        // *entire* duration -- `hlt()` only wakes on an unmasked interrupt or an NMI, so it would
        // freeze the CPU permanently the first time nothing was ready yet (no timer tick to ever
        // advance a tick-based deadline's own check either, no NMI under normal operation). See
        // `ipv4::resolve_with_retry`'s own doc comment for the fuller explanation and the same
        // fix, applied there for the identical reason.
        core::hint::spin_loop();
    }
}
