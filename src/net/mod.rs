//! Networking. Phase 1: PCI discovery (`crate::pci`) + a real NIC driver (`rtl8139`) sending and
//! receiving raw Ethernet frames, IRQ-driven. Phase 2: a real protocol stack on top of it
//! (`ethernet`/`arp`/`ipv4`/`icmp`) -- enough to answer/originate ICMP echo requests against real
//! (if virtualized) network traffic. No `modules/net` syscall shim yet -- see this repo's
//! networking plan for what's still deferred.

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
