//! Console/terminal I/O: the hand-rolled 16550 UART (`serial`), the VGA text-mode console with its
//! own minimal ANSI/VT100 CSI escape parser (`vga`), and the keyboard-IRQ-fed stdin ring buffer
//! plus real termios state (`stdin`) -- see CLAUDE.md's interactive-shell section for the full
//! design.

pub mod serial;
pub mod stdin;
pub mod vga;
