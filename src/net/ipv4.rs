//! Minimal IPv4: parses inbound packets and dispatches by protocol number, builds and sends
//! outbound ones. No fragmentation, no options, no routing (`GUEST_IP`/`GATEWAY_IP` are the only
//! two IPs that matter on this Phase 2 network) -- see this repo's networking plan for what's
//! deferred to later phases.

use alloc::vec::Vec;

use super::{arp, ethernet, icmp, tcp, udp};

pub type Ipv4Addr = [u8; 4];

/// SLIRP's default guest IP under QEMU's `-nic user` backend. No DHCP client exists -- an
/// explicit non-goal until something needs one (see this repo's networking plan).
pub const GUEST_IP: Ipv4Addr = [10, 0, 2, 15];
/// SLIRP's default gateway -- also answers ICMP echo requests directed at itself, which is what
/// `tests/icmp_smoke.rs` uses to verify this stack against real (if virtualized) network
/// behavior without needing host-side raw-socket privileges.
pub const GATEWAY_IP: Ipv4Addr = [10, 0, 2, 2];

pub const PROTO_ICMP: u8 = 1;

const VERSION_IHL: u8 = 0x45; // version 4, IHL 5 (20-byte header, no options)
const DEFAULT_TTL: u8 = 64;
const HEADER_LEN: usize = 20;

/// Internet checksum (RFC 1071): ones'-complement sum of 16-bit words, carries folded back in,
/// then complemented. Shared by the IPv4 header itself and, with no pseudo-header (unlike UDP/
/// TCP), ICMP.
pub fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let (pairs, remainder) = data.as_chunks::<2>();
    for chunk in pairs {
        sum += u16::from_be_bytes(*chunk) as u32;
    }
    if let [last] = remainder {
        sum += (*last as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Parses one IPv4 packet and dispatches by protocol. Anything but ICMP is later phases' concern
/// (UDP/TCP) or simply unsupported -- silently dropped.
pub fn handle_packet(payload: &[u8]) {
    if payload.len() < HEADER_LEN || payload[0] != VERSION_IHL {
        return;
    }
    let total_length = u16::from_be_bytes([payload[2], payload[3]]) as usize;
    let protocol = payload[9];
    let src_ip: Ipv4Addr = payload[12..16].try_into().unwrap();
    let dst_ip: Ipv4Addr = payload[16..20].try_into().unwrap();

    if dst_ip != GUEST_IP || total_length < HEADER_LEN || total_length > payload.len() {
        return;
    }
    let ip_payload = &payload[HEADER_LEN..total_length];

    match protocol {
        PROTO_ICMP => icmp::handle_packet(ip_payload, src_ip),
        udp::PROTO_UDP => udp::handle_packet(ip_payload, src_ip),
        tcp::PROTO_TCP => tcp::handle_packet(ip_payload, src_ip),
        _ => {}
    }
}

/// Builds and sends one IPv4 packet. Resolves `dest_ip`'s MAC via `arp`, sending a request and
/// giving it a bounded wait if it isn't already known -- callers run in a normal (non-interrupt)
/// context where a short busy-wait is acceptable, matching how `tests/rtl8139_smoke.rs`'s own
/// test loop already waits for a reply. A known simplification: a real stack would queue the
/// packet and retry asynchronously instead of blocking the caller.
pub fn send_packet(dest_ip: Ipv4Addr, protocol: u8, payload: &[u8]) -> Option<()> {
    let dest_mac = resolve_with_retry(dest_ip)?;

    let total_length = HEADER_LEN + payload.len();
    let mut packet = Vec::with_capacity(total_length);
    packet.push(VERSION_IHL);
    packet.push(0); // DSCP/ECN
    packet.extend_from_slice(&(total_length as u16).to_be_bytes());
    packet.extend_from_slice(&[0, 0]); // identification
    packet.extend_from_slice(&[0, 0]); // flags/fragment offset
    packet.push(DEFAULT_TTL);
    packet.push(protocol);
    packet.extend_from_slice(&[0, 0]); // header checksum placeholder
    packet.extend_from_slice(&GUEST_IP);
    packet.extend_from_slice(&dest_ip);
    packet.extend_from_slice(payload);

    let sum = checksum(&packet[..HEADER_LEN]);
    packet[10..12].copy_from_slice(&sum.to_be_bytes());

    ethernet::send_frame(dest_mac, ethernet::ETHERTYPE_IPV4, &packet)
}

fn resolve_with_retry(ip: Ipv4Addr) -> Option<[u8; 6]> {
    if let Some(mac) = arp::resolve(ip) {
        return Some(mac);
    }
    arp::send_request(ip);
    // Bounded by PIT ticks, not an arbitrary spin count -- see rtl8139_smoke's own precedent for
    // why a few seconds of budget is generous headroom, not a tight timing assumption.
    let deadline = crate::interrupts::ticks() + 500;
    while crate::interrupts::ticks() < deadline {
        super::poll();
        if let Some(mac) = arp::resolve(ip) {
            return Some(mac);
        }
        x86_64::instructions::hlt();
    }
    None
}
