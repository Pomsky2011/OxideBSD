//! ICMP: answers echo requests (`ping`) directed at us, and can originate our own echo requests
//! -- used by `tests/icmp_smoke.rs` to verify the whole stack against real (if virtualized)
//! network behavior. No other ICMP message types are handled.

use alloc::vec::Vec;

use spin::Mutex;

use super::ipv4::{self, Ipv4Addr};

const TYPE_ECHO_REPLY: u8 = 0;
const TYPE_ECHO_REQUEST: u8 = 8;
const HEADER_LEN: usize = 8;

/// The most recent echo reply seen, if any -- set by `handle_packet`, consumed by
/// `take_echo_reply`. A real `ping` program would instead get this via a socket `recv()` once
/// `modules/net`'s syscalls exist (a later phase); this flag-based hook is Phase 2's stand-in,
/// the same shape as `rtl8139::irq_fired`.
static LAST_ECHO_REPLY: Mutex<Option<(Ipv4Addr, u16, u16)>> = Mutex::new(None);

/// Takes (clears) the most recently observed echo reply's (source IP, identifier, sequence).
pub fn take_echo_reply() -> Option<(Ipv4Addr, u16, u16)> {
    LAST_ECHO_REPLY.lock().take()
}

pub fn handle_packet(payload: &[u8], src_ip: Ipv4Addr) {
    if payload.len() < HEADER_LEN {
        return;
    }
    let icmp_type = payload[0];
    let identifier = u16::from_be_bytes([payload[4], payload[5]]);
    let sequence = u16::from_be_bytes([payload[6], payload[7]]);

    match icmp_type {
        TYPE_ECHO_REQUEST => {
            reply_to_echo(src_ip, identifier, sequence, &payload[HEADER_LEN..]);
        }
        TYPE_ECHO_REPLY => {
            *LAST_ECHO_REPLY.lock() = Some((src_ip, identifier, sequence));
        }
        _ => {}
    }
}

fn build_packet(icmp_type: u8, identifier: u16, sequence: u16, data: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(HEADER_LEN + data.len());
    packet.push(icmp_type);
    packet.push(0); // code
    packet.extend_from_slice(&[0, 0]); // checksum placeholder
    packet.extend_from_slice(&identifier.to_be_bytes());
    packet.extend_from_slice(&sequence.to_be_bytes());
    packet.extend_from_slice(data);

    let sum = ipv4::checksum(&packet);
    packet[2..4].copy_from_slice(&sum.to_be_bytes());
    packet
}

fn reply_to_echo(dest_ip: Ipv4Addr, identifier: u16, sequence: u16, data: &[u8]) {
    let packet = build_packet(TYPE_ECHO_REPLY, identifier, sequence, data);
    if ipv4::send_packet(dest_ip, ipv4::PROTO_ICMP, &packet).is_none() {
        crate::serial_println!(
            "[net] icmp: failed to reply to echo request from {:?} (ARP resolution failed?)",
            dest_ip
        );
    }
}

/// Originates an echo request -- what `tests/icmp_smoke.rs` uses to ping the SLIRP gateway.
pub fn send_echo_request(
    dest_ip: Ipv4Addr,
    identifier: u16,
    sequence: u16,
    data: &[u8],
) -> Option<()> {
    let packet = build_packet(TYPE_ECHO_REQUEST, identifier, sequence, data);
    ipv4::send_packet(dest_ip, ipv4::PROTO_ICMP, &packet)
}
