//! Real hardware drivers not already grouped elsewhere (the network driver, `rtl8139`, lives under
//! `net/` alongside the protocol stack it exists for): `ata` is the hand-rolled legacy IDE PIO disk
//! driver backing oxfs's real disk persistence, `pci` is legacy I/O-port PCI config-space
//! enumeration (today only used to find the NIC, kept generic for any future PCI device).

pub mod ata;
pub mod pci;
