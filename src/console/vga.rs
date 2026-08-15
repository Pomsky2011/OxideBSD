use core::fmt;

use alloc::boxed::Box;
use spin::{Lazy, Mutex};
use x86_64::instructions::interrupts;
use x86_64::instructions::port::Port;

const BUFFER_HEIGHT: usize = 25;
const BUFFER_WIDTH: usize = 80;
const VGA_BUFFER_ADDR: usize = 0xb8000;

/// CRTC index/data port pair (standard VGA, unchanged since the original IBM CGA/MDA days) --
/// used only to move/show/hide the real blinking hardware cursor. Text content itself never goes
/// through these ports (that's the direct VRAM write in `write_char_at`); this is purely "where's
/// the cursor" state a real terminal always keeps in sync, which this writer didn't do at all
/// until now -- `cursor_row`/`cursor_col` were pure internal bookkeeping for text placement math,
/// invisible to anyone actually looking at the QEMU display.
const CRTC_INDEX_PORT: u16 = 0x3d4;
const CRTC_DATA_PORT: u16 = 0x3d5;
const CRTC_CURSOR_LOCATION_HIGH: u8 = 0x0e;
const CRTC_CURSOR_LOCATION_LOW: u8 = 0x0f;
const CRTC_CURSOR_START: u8 = 0x0a;
const CRTC_CURSOR_END: u8 = 0x0b;
/// Bit 5 of the "cursor start" register disables the cursor entirely -- the standard VGA idiom
/// for hiding it (there's no separate on/off register).
const CURSOR_DISABLE_BIT: u8 = 0x20;
/// A conventional block-ish underline cursor shape (scanlines 14-15 of a 16-scanline glyph cell,
/// the BIOS text-mode default) -- restored whenever the cursor is shown again.
const CURSOR_SHAPE_START: u8 = 0x0e;
const CURSOR_SHAPE_END: u8 = 0x0f;

/// Longest CSI parameter list this writer tracks (e.g. `ESC[24;80H`'s row/col pair). Real ANSI
/// sequences this kernel actually receives (BusyBox `vi`'s cursor moves/erases/SGR) never need
/// more than two; extra params beyond this are parsed (so the final byte is still found) but
/// dropped.
const MAX_CSI_PARAMS: usize = 4;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Color {
    Black = 0,
    Blue = 1,
    Green = 2,
    Cyan = 3,
    Red = 4,
    Magenta = 5,
    Brown = 6,
    LightGray = 7,
    DarkGray = 8,
    LightBlue = 9,
    LightGreen = 10,
    LightCyan = 11,
    LightRed = 12,
    Pink = 13,
    Yellow = 14,
    White = 15,
}

impl Color {
    /// The VGA "high intensity" sibling of a base color -- used for SGR bold (`ESC[1m`).
    fn intensify(self) -> Color {
        match self {
            Color::Black => Color::DarkGray,
            Color::Blue => Color::LightBlue,
            Color::Green => Color::LightGreen,
            Color::Cyan => Color::LightCyan,
            Color::Red => Color::LightRed,
            Color::Magenta => Color::Pink,
            Color::Brown => Color::Yellow,
            Color::LightGray => Color::White,
            already_bright => already_bright,
        }
    }

    /// ANSI SGR base colors 30-37/40-47 (mod 10), in their standard order.
    fn from_ansi_base(n: u16) -> Color {
        match n {
            0 => Color::Black,
            1 => Color::Red,
            2 => Color::Green,
            3 => Color::Brown,
            4 => Color::Blue,
            5 => Color::Magenta,
            6 => Color::Cyan,
            _ => Color::LightGray,
        }
    }

    /// ANSI SGR bright colors 90-97/100-107 (mod 10).
    fn from_ansi_bright(n: u16) -> Color {
        match n {
            0 => Color::DarkGray,
            1 => Color::LightRed,
            2 => Color::LightGreen,
            3 => Color::Yellow,
            4 => Color::LightBlue,
            5 => Color::Pink,
            6 => Color::LightCyan,
            _ => Color::White,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
struct ColorCode(u8);

impl ColorCode {
    fn new(foreground: Color, background: Color) -> ColorCode {
        ColorCode((background as u8) << 4 | (foreground as u8))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
struct ScreenChar {
    ascii_character: u8,
    color_code: ColorCode,
}

#[repr(transparent)]
struct Buffer {
    chars: [[ScreenChar; BUFFER_WIDTH]; BUFFER_HEIGHT],
}

/// A full screen's worth of content, detached from `Buffer`'s own fixed VRAM address -- what
/// `enter_alt_screen`/`exit_alt_screen` snapshot to/from the heap.
type ScreenGrid = [[ScreenChar; BUFFER_WIDTH]; BUFFER_HEIGHT];

/// Where a byte stream sits relative to an ANSI/VT100 escape sequence. Only `CSI` (`ESC [ ... `)
/// is actually interpreted -- see `handle_escape_byte`/`handle_csi_byte`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscState {
    Ground,
    Escape,
    Csi,
}

/// DECSC/DECRC (`ESC 7`/`ESC 8`, also the ANSI.SYS-style `CSI s`/`CSI u`) snapshot -- position plus
/// the SGR state a real terminal saves alongside it, not just the two coordinates.
#[derive(Debug, Clone, Copy)]
struct SavedCursorState {
    row: usize,
    col: usize,
    sgr_fg: Color,
    sgr_bg: Color,
    sgr_bold: bool,
    sgr_reverse: bool,
}

struct Writer {
    cursor_row: usize,
    cursor_col: usize,
    /// Whether the hardware cursor should actually be drawn (`ESC[?25l`/`h`) -- tracked
    /// separately from position since a hidden cursor still needs its position kept up to date
    /// for the moment it's shown again.
    cursor_visible: bool,
    /// The color actually written into the VGA buffer -- derived from `sgr_*` below by
    /// `recompute_color_code` every time an SGR sequence changes them.
    color_code: ColorCode,
    sgr_fg: Color,
    sgr_bg: Color,
    sgr_bold: bool,
    sgr_reverse: bool,
    esc_state: EscState,
    csi_params: [u16; MAX_CSI_PARAMS],
    csi_param_count: usize,
    csi_cur: u16,
    csi_cur_present: bool,
    /// Set when a CSI sequence's first byte after `[` is `?` -- disambiguates DEC private modes
    /// (`ESC[?25l`, `ESC[?1049h`) from standard ANSI `SM`/`RM` (`ESC[4h`, ...), which share the
    /// same final bytes but different parameter numbering. Only the private ones are implemented;
    /// a non-private `h`/`l` is silently swallowed, same as before.
    csi_private: bool,
    /// DECSTBM scroll margins (0-based, inclusive), defaulting to the whole screen -- `line_feed`/
    /// `reverse_index`/IL/DL/SU/SD all scroll within this range instead of the full buffer.
    scroll_top: usize,
    scroll_bottom: usize,
    saved_cursor: Option<SavedCursorState>,
    /// Set only while an alternate-screen app (`vi`, `hexedit`, ...) is active -- holds the main
    /// screen's real content plus its cursor, both restored verbatim on `ESC[?1049l`. A `Box`
    /// because this writer otherwise holds no heap state at all; the buffer's home page is a
    /// static VRAM pointer, not something that can just be swapped for a second static one.
    alt_screen_saved: Option<Box<(ScreenGrid, usize, usize)>>,
    buffer: &'static mut Buffer,
}

impl Writer {
    fn write_char_at(&mut self, row: usize, col: usize, screen_char: ScreenChar) {
        let ptr = &raw mut self.buffer.chars[row][col];
        // SAFETY: the write must not be optimized away or reordered, since nothing in the Rust
        // abstract machine ever reads this memory back — only the VGA hardware does.
        unsafe { ptr.write_volatile(screen_char) };
    }

    fn read_char_at(&self, row: usize, col: usize) -> ScreenChar {
        let ptr = &raw const self.buffer.chars[row][col];
        // SAFETY: see write_char_at.
        unsafe { ptr.read_volatile() }
    }

    fn write_byte(&mut self, byte: u8) {
        match self.esc_state {
            EscState::Ground => self.write_byte_ground(byte),
            EscState::Escape => self.handle_escape_byte(byte),
            EscState::Csi => self.handle_csi_byte(byte),
        }
        // Every byte can potentially move the cursor (a plain glyph, a control character, or a
        // CSI/escape final byte) -- rather than call this from each of those individually, just
        // resync once per byte. Two `outb` pairs is trivial next to VRAM writes at this scale.
        self.sync_hw_cursor();
    }

    fn write_byte_ground(&mut self, byte: u8) {
        match byte {
            0x1b => {
                self.reset_csi();
                self.esc_state = EscState::Escape;
            }
            b'\n' => self.line_feed(),
            // Carriage return: back to column 0 on the current row, no row change.
            b'\r' => self.cursor_col = 0,
            // Tab: advance to the next multiple-of-8 stop. The distance is at most 8 (and
            // BUFFER_WIDTH is itself a multiple of 8), so this can cross at most one line wrap.
            b'\t' => {
                let next_stop = (self.cursor_col / 8 + 1) * 8;
                for _ in self.cursor_col..next_stop {
                    self.put_char(b' ');
                }
            }
            // Backspace/DEL: step the cursor back a column and blank the character that was
            // there, mirroring `src/serial.rs`'s `SerialPort::send`, which expands a raw 0x08/0x7f
            // into the standard "\x08 \x08" terminal idiom for the same reason -- a caller (see
            // `userland/stsh/`'s `read_line`) just writes a single raw backspace byte and expects
            // *something* to actually erase the character, not just move a cursor over it.
            // Doesn't cross a line boundary.
            0x08 | 0x7f => {
                self.cursor_col = self.cursor_col.saturating_sub(1);
                let row = self.cursor_row;
                let col = self.cursor_col;
                let color_code = self.color_code;
                self.write_char_at(
                    row,
                    col,
                    ScreenChar {
                        ascii_character: b' ',
                        color_code,
                    },
                );
            }
            byte => self.put_char(byte),
        }
    }

    /// The very first byte after `ESC`. `[` starts a CSI sequence; `7`/`8` (DECSC/DECRC) and `M`
    /// (RI) are handled directly here since they're complete two-byte escapes with no CSI
    /// involved. Any other escape (e.g. a charset-select `ESC ( B`, or a bare `ESC c` reset) is
    /// swallowed as a single unrecognized byte rather than echoed as garbage -- full-screen
    /// programs like `vi` depend on unhandled sequences disappearing silently, not corrupting the
    /// display.
    fn handle_escape_byte(&mut self, byte: u8) {
        match byte {
            b'[' => {
                self.esc_state = EscState::Csi;
                return;
            }
            // DECSC/DECRC: save/restore cursor position + SGR state. Not CSI-prefixed (no `[`),
            // a bare two-byte escape -- `reset`'s own `ESC c ESC(B ESC[m ESC[J ESC[?25h` sequence
            // doesn't use these, but real vttest-style full-screen apps commonly do.
            b'7' => self.save_cursor(),
            b'8' => self.restore_cursor(),
            // RI (reverse index): cursor up one line, scrolling the region down if already at its
            // top margin -- the inverse of a line feed. Paired with DECSTBM the same way a normal
            // line feed is paired with the bottom margin.
            b'M' => self.reverse_index(),
            _ => {}
        }
        self.esc_state = EscState::Ground;
    }

    /// One byte of a `CSI` sequence's parameter/intermediate/final bytes (ECMA-48): digits and
    /// `;` accumulate parameters, `?` (a private-mode marker, e.g. `ESC[?1049h`) just records
    /// that this is a DEC private-mode sequence (see `csi_private`/`set_private_modes`), and a
    /// final byte in `@`..=`~` dispatches and returns to `Ground`. Anything else (rare
    /// intermediate bytes) is ignored in place, matching real terminals' tolerance of unexpected
    /// sequences.
    fn handle_csi_byte(&mut self, byte: u8) {
        match byte {
            b'0'..=b'9' => {
                self.csi_cur = self
                    .csi_cur
                    .saturating_mul(10)
                    .saturating_add((byte - b'0') as u16);
                self.csi_cur_present = true;
            }
            b';' => self.push_csi_param(),
            b'?' => self.csi_private = true,
            0x40..=0x7e => {
                self.push_csi_param();
                self.execute_csi(byte);
                self.reset_csi();
                self.esc_state = EscState::Ground;
            }
            _ => {}
        }
    }

    fn push_csi_param(&mut self) {
        if self.csi_param_count < MAX_CSI_PARAMS {
            self.csi_params[self.csi_param_count] = if self.csi_cur_present {
                self.csi_cur
            } else {
                0
            };
            self.csi_param_count += 1;
        }
        self.csi_cur = 0;
        self.csi_cur_present = false;
    }

    fn reset_csi(&mut self) {
        self.csi_params = [0; MAX_CSI_PARAMS];
        self.csi_param_count = 0;
        self.csi_cur = 0;
        self.csi_cur_present = false;
        self.csi_private = false;
    }

    /// A parsed CSI parameter, with `default` substituted both when the parameter was omitted
    /// entirely and when it was given as an explicit `0` -- true for every sequence this writer
    /// implements (cursor moves/positioning treat 0 the same as "unspecified"; erase-in-line/
    /// display's own default *is* 0, so the two cases coincide there too).
    fn csi_param(&self, index: usize, default: u16) -> u16 {
        let raw = self.csi_params.get(index).copied().unwrap_or(0);
        if raw == 0 { default } else { raw }
    }

    fn execute_csi(&mut self, final_byte: u8) {
        match final_byte {
            // CUP: cursor position, 1-based `row;col` (bare `H`/`f` means home, 1;1).
            b'H' | b'f' => {
                let row = self.csi_param(0, 1).saturating_sub(1) as usize;
                let col = self.csi_param(1, 1).saturating_sub(1) as usize;
                self.cursor_row = row.min(BUFFER_HEIGHT - 1);
                self.cursor_col = col.min(BUFFER_WIDTH - 1);
            }
            // CUU/CUD/CUF/CUB: relative cursor moves, clamped to the screen (no scrolling).
            b'A' => {
                self.cursor_row = self
                    .cursor_row
                    .saturating_sub(self.csi_param(0, 1) as usize)
            }
            b'B' => {
                self.cursor_row =
                    (self.cursor_row + self.csi_param(0, 1) as usize).min(BUFFER_HEIGHT - 1)
            }
            b'C' => {
                self.cursor_col =
                    (self.cursor_col + self.csi_param(0, 1) as usize).min(BUFFER_WIDTH - 1)
            }
            b'D' => {
                self.cursor_col = self
                    .cursor_col
                    .saturating_sub(self.csi_param(0, 1) as usize)
            }
            b'J' => self.erase_in_display(self.csi_param(0, 0)),
            b'K' => self.erase_in_line(self.csi_param(0, 0)),
            b'm' => self.apply_sgr(),
            // DECSTBM: set scroll margins.
            b'r' => self.set_scroll_region(),
            // IL/DL: insert/delete whole lines at the cursor row, within the scroll region.
            b'L' => self.insert_lines(self.csi_param(0, 1) as usize),
            b'M' => self.delete_lines(self.csi_param(0, 1) as usize),
            // ICH/DCH: insert/delete characters within the current line.
            b'@' => self.insert_chars(self.csi_param(0, 1) as usize),
            b'P' => self.delete_chars(self.csi_param(0, 1) as usize),
            // SU/SD: scroll the region without moving the cursor.
            b'S' => {
                let n = self.csi_param(0, 1) as usize;
                self.scroll_region_up_by(self.scroll_top, self.scroll_bottom, n);
            }
            b'T' => {
                let n = self.csi_param(0, 1) as usize;
                self.scroll_region_down_by(self.scroll_top, self.scroll_bottom, n);
            }
            // ANSI.SYS-style save/restore cursor (no `?` prefix, no parameters) -- the CSI
            // sibling of DECSC/DECRC (`ESC 7`/`ESC 8`).
            b's' => self.save_cursor(),
            b'u' => self.restore_cursor(),
            // DSR: device status report. Only mode 6 (CPR, "where's the cursor") has a real
            // caller anywhere in the roster (`less`'s own `ESC[999;999H ESC[6n` fallback, taken
            // only when `TIOCGWINSZ` itself fails, which it doesn't here -- implemented anyway
            // since it's cheap and any future full-screen program that queries it unconditionally
            // would otherwise hang forever waiting for a reply that never arrives).
            b'n' => self.device_status_report(self.csi_param(0, 0)),
            // DEC private mode set/reset -- only meaningful with the `?` prefix (`ESC[?25h`,
            // `ESC[?1049h`); a bare `ESC[4h` (ANSI IRM) has no VGA-text-mode equivalent to honor.
            b'h' if self.csi_private => self.set_private_modes(true),
            b'l' if self.csi_private => self.set_private_modes(false),
            // Everything else (device status reports, non-private mode set/reset, ...) has no
            // effect on this plain scrolling console -- swallow it.
            _ => {}
        }
    }

    /// ED: erase in display. `0` = cursor to end of screen, `1` = start of screen to cursor,
    /// anything else = entire screen. Cursor position itself is left unchanged, matching real
    /// terminals (`vi` always repositions explicitly afterward via a separate CUP).
    fn erase_in_display(&mut self, mode: u16) {
        match mode {
            0 => {
                self.clear_row_range(self.cursor_row, self.cursor_col, BUFFER_WIDTH);
                for row in (self.cursor_row + 1)..BUFFER_HEIGHT {
                    self.clear_row(row);
                }
            }
            1 => {
                for row in 0..self.cursor_row {
                    self.clear_row(row);
                }
                self.clear_row_range(self.cursor_row, 0, self.cursor_col + 1);
            }
            _ => {
                for row in 0..BUFFER_HEIGHT {
                    self.clear_row(row);
                }
            }
        }
    }

    /// EL: erase in line, same mode convention as `erase_in_display` but confined to the current
    /// row.
    fn erase_in_line(&mut self, mode: u16) {
        match mode {
            0 => self.clear_row_range(self.cursor_row, self.cursor_col, BUFFER_WIDTH),
            1 => self.clear_row_range(self.cursor_row, 0, self.cursor_col + 1),
            _ => self.clear_row(self.cursor_row),
        }
    }

    /// SGR: character attributes. Supports reset, bold, reverse-video, and the 8 base + 8 bright
    /// foreground/background colors (30-37/40-47, 90-97/100-107, plus the 39/49 "default"
    /// resets) -- enough for `vi`'s own bold-via-reverse highlighting (`ESC[7m`) and any other
    /// ANSI-colored userland output, without modeling the rest of SGR (underline, blink, ...)
    /// this kernel has no VGA attribute bits to represent anyway.
    fn apply_sgr(&mut self) {
        if self.csi_param_count == 0 {
            self.sgr_reset();
            return;
        }
        for i in 0..self.csi_param_count {
            match self.csi_params[i] {
                0 => self.sgr_reset(),
                1 => self.sgr_bold = true,
                22 => self.sgr_bold = false,
                7 => self.sgr_reverse = true,
                27 => self.sgr_reverse = false,
                n @ 30..=37 => self.sgr_fg = Color::from_ansi_base(n - 30),
                39 => self.sgr_fg = Color::LightGray,
                n @ 40..=47 => self.sgr_bg = Color::from_ansi_base(n - 40),
                49 => self.sgr_bg = Color::Black,
                n @ 90..=97 => self.sgr_fg = Color::from_ansi_bright(n - 90),
                n @ 100..=107 => self.sgr_bg = Color::from_ansi_bright(n - 100),
                _ => {}
            }
        }
        self.recompute_color_code();
    }

    fn sgr_reset(&mut self) {
        self.sgr_fg = Color::LightGray;
        self.sgr_bg = Color::Black;
        self.sgr_bold = false;
        self.sgr_reverse = false;
        self.recompute_color_code();
    }

    fn recompute_color_code(&mut self) {
        let (mut fg, bg) = if self.sgr_reverse {
            (self.sgr_bg, self.sgr_fg)
        } else {
            (self.sgr_fg, self.sgr_bg)
        };
        if self.sgr_bold {
            fg = fg.intensify();
        }
        self.color_code = ColorCode::new(fg, bg);
    }

    /// DECSC/DECRC and their CSI-form siblings (`ESC 7`/`ESC[s`, `ESC 8`/`ESC[u`) -- real
    /// terminals treat both spellings as the same single save slot, not two independent ones.
    fn save_cursor(&mut self) {
        self.saved_cursor = Some(SavedCursorState {
            row: self.cursor_row,
            col: self.cursor_col,
            sgr_fg: self.sgr_fg,
            sgr_bg: self.sgr_bg,
            sgr_bold: self.sgr_bold,
            sgr_reverse: self.sgr_reverse,
        });
    }

    /// Restoring with nothing saved is a real, POSIX-legal no-op (matches every real terminal --
    /// there's no error to report over this byte stream anyway).
    fn restore_cursor(&mut self) {
        if let Some(saved) = self.saved_cursor {
            self.cursor_row = saved.row.min(BUFFER_HEIGHT - 1);
            self.cursor_col = saved.col.min(BUFFER_WIDTH - 1);
            self.sgr_fg = saved.sgr_fg;
            self.sgr_bg = saved.sgr_bg;
            self.sgr_bold = saved.sgr_bold;
            self.sgr_reverse = saved.sgr_reverse;
            self.recompute_color_code();
        }
    }

    /// DEC private mode set (`h`)/reset (`l`) -- dispatches every parameter in the sequence (real
    /// terminals allow e.g. `ESC[?1049;25h` combining several). Only `25` (cursor visibility) and
    /// `1049` (alternate screen, save/restore cursor included) have any real effect here; anything
    /// else is silently accepted and ignored, matching this writer's existing tolerance for
    /// unsupported sequences elsewhere.
    fn set_private_modes(&mut self, enable: bool) {
        for i in 0..self.csi_param_count {
            match self.csi_params[i] {
                25 => self.set_cursor_visible(enable),
                1049 => {
                    if enable {
                        self.enter_alt_screen();
                    } else {
                        self.exit_alt_screen();
                    }
                }
                _ => {}
            }
        }
    }

    fn set_cursor_visible(&mut self, visible: bool) {
        self.cursor_visible = visible;
        self.apply_hw_cursor_visibility();
    }

    /// `ESC[?1049h`: snapshot the real, currently-visible screen content and cursor, then clear
    /// to a blank canvas for the alternate-screen app. A second `h` while already in the
    /// alternate screen is a no-op (matches real terminals -- there's only one save slot, and
    /// overwriting it with the app's *own* in-progress alt-screen content would lose the real
    /// main screen underneath).
    fn enter_alt_screen(&mut self) {
        if self.alt_screen_saved.is_some() {
            return;
        }
        let mut saved = [[ScreenChar {
            ascii_character: b' ',
            color_code: self.color_code,
        }; BUFFER_WIDTH]; BUFFER_HEIGHT];
        for (row, row_slice) in saved.iter_mut().enumerate() {
            for (col, cell) in row_slice.iter_mut().enumerate() {
                *cell = self.read_char_at(row, col);
            }
        }
        self.alt_screen_saved = Some(Box::new((saved, self.cursor_row, self.cursor_col)));
        self.erase_in_display(2);
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    /// `ESC[?1049l`: restore whatever `enter_alt_screen` snapshotted. Exiting without a matching
    /// enter (no save slot present) is a no-op, same reasoning as `restore_cursor`.
    fn exit_alt_screen(&mut self) {
        let Some(saved) = self.alt_screen_saved.take() else {
            return;
        };
        let (content, row, col) = *saved;
        for (r, row_slice) in content.iter().enumerate() {
            for (c, &character) in row_slice.iter().enumerate() {
                self.write_char_at(r, c, character);
            }
        }
        self.cursor_row = row;
        self.cursor_col = col;
    }

    /// Push the software cursor position out to the real CRTC registers. Cheap (two port-index
    /// writes plus two data writes) and called after every byte (`write_byte`), so the hardware
    /// cursor is never more than one byte behind what a full-screen app like `vi` thinks it just
    /// drew.
    fn sync_hw_cursor(&self) {
        if !self.cursor_visible {
            return;
        }
        let position = (self.cursor_row * BUFFER_WIDTH + self.cursor_col) as u16;
        let mut index_port: Port<u8> = Port::new(CRTC_INDEX_PORT);
        let mut data_port: Port<u8> = Port::new(CRTC_DATA_PORT);
        // SAFETY: 0x3D4/0x3D5 are the standard VGA CRTC index/data ports, always present on this
        // target (no more exotic display hardware exists here) -- ordinary MMIO-equivalent port
        // I/O, not memory-unsafe on its own.
        unsafe {
            index_port.write(CRTC_CURSOR_LOCATION_HIGH);
            data_port.write((position >> 8) as u8);
            index_port.write(CRTC_CURSOR_LOCATION_LOW);
            data_port.write((position & 0xff) as u8);
        }
    }

    /// CPR: report the cursor's 1-based `row;col` position by writing `ESC[row;colR` straight
    /// into the stdin ring buffer, exactly as if it had been typed -- the real mechanism every
    /// terminal uses to answer this query (the reply travels back over the *input* side, not
    /// this writer's own output). `crate::console::stdin::push_byte` is already `pub` and already the sole
    /// producer the keyboard IRQ handler uses, so this is just a second, synthetic producer of
    /// the same stream.
    fn device_status_report(&self, mode: u16) {
        if mode != 6 {
            return;
        }
        let reply = alloc::format!("\x1b[{};{}R", self.cursor_row + 1, self.cursor_col + 1);
        for byte in reply.bytes() {
            crate::console::stdin::push_byte(byte);
        }
    }

    /// Toggles the CRTC's own cursor-disable bit, and restores a normal cursor shape when turning
    /// it back on (the disable bit lives in the same register as the shape's start scanline, so
    /// re-enabling has to rewrite a real shape, not just clear the bit blindly).
    fn apply_hw_cursor_visibility(&self) {
        let mut index_port: Port<u8> = Port::new(CRTC_INDEX_PORT);
        let mut data_port: Port<u8> = Port::new(CRTC_DATA_PORT);
        // SAFETY: see sync_hw_cursor.
        unsafe {
            index_port.write(CRTC_CURSOR_START);
            if self.cursor_visible {
                data_port.write(CURSOR_SHAPE_START);
                index_port.write(CRTC_CURSOR_END);
                data_port.write(CURSOR_SHAPE_END);
            } else {
                data_port.write(CURSOR_DISABLE_BIT);
            }
        }
        if self.cursor_visible {
            self.sync_hw_cursor();
        }
    }

    /// Write one visible glyph at the cursor and advance it, wrapping (and scrolling, if already
    /// on the last row) at the end of a line.
    fn put_char(&mut self, byte: u8) {
        if self.cursor_col >= BUFFER_WIDTH {
            self.line_feed();
        }

        let row = self.cursor_row;
        let col = self.cursor_col;
        let color_code = self.color_code;
        self.write_char_at(
            row,
            col,
            ScreenChar {
                ascii_character: byte,
                color_code,
            },
        );
        self.cursor_col += 1;
    }

    fn write_string(&mut self, s: &str) {
        for byte in s.bytes() {
            match byte {
                // Printable ASCII, the control characters `write_byte` gives meaning to
                // (newline, carriage return, tab, backspace, DEL), and ESC (0x1b) -- every CSI
                // parameter/intermediate/final byte is itself printable ASCII, so ESC is the only
                // addition an escape sequence needs here.
                0x20..=0x7e | b'\n' | b'\r' | b'\t' | 0x08 | 0x7f | 0x1b => self.write_byte(byte),
                // Anything else isn't representable in code page 437; show a placeholder.
                _ => self.write_byte(0xfe),
            }
        }
    }

    /// Advance to column 0 of the next row, scrolling the active DECSTBM region up if already at
    /// its bottom margin (the whole screen, by default -- matches the pre-scroll-region
    /// behavior exactly when `scroll_top`/`scroll_bottom` are still at their defaults). Used both
    /// for an explicit `\n` and for wrapping a line that ran off the right edge.
    fn line_feed(&mut self) {
        self.cursor_col = 0;
        if self.cursor_row == self.scroll_bottom {
            self.scroll_region_up_by(self.scroll_top, self.scroll_bottom, 1);
        } else if self.cursor_row + 1 < BUFFER_HEIGHT {
            self.cursor_row += 1;
        }
    }

    /// RI: cursor up one line, scrolling the region down (inserting a blank line at the top
    /// margin) if already there instead of moving above it.
    fn reverse_index(&mut self) {
        if self.cursor_row == self.scroll_top {
            self.scroll_region_down_by(self.scroll_top, self.scroll_bottom, 1);
        } else {
            self.cursor_row = self.cursor_row.saturating_sub(1);
        }
    }

    /// Shifts rows `top+1..=bottom` up by one (into `top..=bottom-1`), blanking `bottom` --
    /// repeated `n` times. `n` is capped at the region's own height since scrolling a k-row
    /// region more than k times can only ever produce a fully blank region, same as any real
    /// terminal.
    fn scroll_region_up_by(&mut self, top: usize, bottom: usize, n: usize) {
        if top > bottom {
            return;
        }
        // A single-row region (or IL/DL landing exactly on the bottom margin) has nothing to
        // shift -- the real terminal behavior is just "blank that one row".
        if top == bottom {
            if n > 0 {
                self.clear_row(bottom);
            }
            return;
        }
        let n = n.min(bottom - top + 1);
        for _ in 0..n {
            for row in (top + 1)..=bottom {
                for col in 0..BUFFER_WIDTH {
                    let character = self.read_char_at(row, col);
                    self.write_char_at(row - 1, col, character);
                }
            }
            self.clear_row(bottom);
        }
    }

    /// The mirror image of `scroll_region_up_by`: shifts `top..=bottom-1` down by one (into
    /// `top+1..=bottom`), blanking `top`.
    fn scroll_region_down_by(&mut self, top: usize, bottom: usize, n: usize) {
        if top > bottom {
            return;
        }
        if top == bottom {
            if n > 0 {
                self.clear_row(top);
            }
            return;
        }
        let n = n.min(bottom - top + 1);
        for _ in 0..n {
            let mut row = bottom;
            while row > top {
                for col in 0..BUFFER_WIDTH {
                    let character = self.read_char_at(row - 1, col);
                    self.write_char_at(row, col, character);
                }
                row -= 1;
            }
            self.clear_row(top);
        }
    }

    /// DECSTBM: set the scrolling margins (1-based, inclusive `top;bottom`; `r` alone resets to
    /// the whole screen). An invalid range (`top >= bottom`) is ignored the same way real
    /// terminals reject it, rather than producing a zero/negative-height region. Real terminals
    /// also home the cursor after this -- there's no DECOM (origin mode) here, so that's always
    /// absolute `0,0`.
    fn set_scroll_region(&mut self) {
        let top = (self.csi_param(0, 1).saturating_sub(1) as usize).min(BUFFER_HEIGHT - 1);
        let bottom =
            (self.csi_param(1, BUFFER_HEIGHT as u16).saturating_sub(1) as usize).min(BUFFER_HEIGHT - 1);
        if top < bottom {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
        } else {
            self.scroll_top = 0;
            self.scroll_bottom = BUFFER_HEIGHT - 1;
        }
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    /// IL: insert `n` blank lines at the cursor row, pushing existing lines down within the
    /// scroll region (lines pushed past the bottom margin are discarded). A no-op outside the
    /// region, matching real terminal behavior.
    fn insert_lines(&mut self, n: usize) {
        if self.cursor_row < self.scroll_top || self.cursor_row > self.scroll_bottom {
            return;
        }
        // `scroll_region_down_by(cursor_row, scroll_bottom, n)` shifts `cursor_row..bottom-1`
        // down into `cursor_row+1..bottom` and blanks `cursor_row` itself -- exactly IL's
        // contract, with the cursor's own row as the region's top boundary.
        self.scroll_region_down_by(self.cursor_row, self.scroll_bottom, n);
    }

    /// DL: delete `n` lines at the cursor row, pulling lines below up within the scroll region
    /// (blank lines appear at the bottom margin). A no-op outside the region.
    fn delete_lines(&mut self, n: usize) {
        if self.cursor_row < self.scroll_top || self.cursor_row > self.scroll_bottom {
            return;
        }
        // Mirror of insert_lines: `scroll_region_up_by(cursor_row, scroll_bottom, n)` shifts
        // `cursor_row+1..bottom` up into `cursor_row..bottom-1` and blanks `scroll_bottom`.
        self.scroll_region_up_by(self.cursor_row, self.scroll_bottom, n);
    }

    /// ICH: insert `n` blank characters at the cursor column, shifting the rest of the line right
    /// (characters pushed past the right edge are discarded).
    fn insert_chars(&mut self, n: usize) {
        let row = self.cursor_row;
        let from_col = self.cursor_col;
        if from_col >= BUFFER_WIDTH {
            return;
        }
        let n = n.min(BUFFER_WIDTH - from_col);
        let mut col = BUFFER_WIDTH;
        while col > from_col + n {
            col -= 1;
            let character = self.read_char_at(row, col - n);
            self.write_char_at(row, col, character);
        }
        self.clear_row_range(row, from_col, from_col + n);
    }

    /// DCH: delete `n` characters at the cursor column, shifting the rest of the line left (blank
    /// fill at the right edge).
    fn delete_chars(&mut self, n: usize) {
        let row = self.cursor_row;
        let from_col = self.cursor_col;
        if from_col >= BUFFER_WIDTH {
            return;
        }
        let n = n.min(BUFFER_WIDTH - from_col);
        for col in from_col..(BUFFER_WIDTH - n) {
            let character = self.read_char_at(row, col + n);
            self.write_char_at(row, col, character);
        }
        self.clear_row_range(row, BUFFER_WIDTH - n, BUFFER_WIDTH);
    }

    fn clear_row(&mut self, row: usize) {
        self.clear_row_range(row, 0, BUFFER_WIDTH);
    }

    fn clear_row_range(&mut self, row: usize, start_col: usize, end_col: usize) {
        let blank = ScreenChar {
            ascii_character: b' ',
            color_code: self.color_code,
        };
        for col in start_col..end_col.min(BUFFER_WIDTH) {
            self.write_char_at(row, col, blank);
        }
    }
}

impl fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_string(s);
        Ok(())
    }
}

static WRITER: Lazy<Mutex<Writer>> = Lazy::new(|| {
    let writer = Writer {
        cursor_row: 0,
        cursor_col: 0,
        cursor_visible: true,
        color_code: ColorCode::new(Color::LightGray, Color::Black),
        sgr_fg: Color::LightGray,
        sgr_bg: Color::Black,
        sgr_bold: false,
        sgr_reverse: false,
        esc_state: EscState::Ground,
        csi_params: [0; MAX_CSI_PARAMS],
        csi_param_count: 0,
        csi_cur: 0,
        csi_cur_present: false,
        csi_private: false,
        scroll_top: 0,
        scroll_bottom: BUFFER_HEIGHT - 1,
        saved_cursor: None,
        alt_screen_saved: None,
        // SAFETY: 0xb8000 is the VGA text-mode buffer's physical address, identity-mapped by the
        // bootloader; this Writer is the only thing that ever accesses it.
        buffer: unsafe { &mut *(VGA_BUFFER_ADDR as *mut Buffer) },
    };
    // Establish a known cursor shape/position at boot -- the BIOS's own leftover cursor state is
    // otherwise whatever it happened to be, not necessarily even at 0,0.
    writer.apply_hw_cursor_visibility();
    Mutex::new(writer)
});

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use core::fmt::Write;

    interrupts::without_interrupts(|| {
        WRITER
            .lock()
            .write_fmt(args)
            .expect("printing to VGA buffer failed");
    });
}
