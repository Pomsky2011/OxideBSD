use core::fmt;

use spin::{Lazy, Mutex};
use x86_64::instructions::interrupts;

const BUFFER_HEIGHT: usize = 25;
const BUFFER_WIDTH: usize = 80;
const VGA_BUFFER_ADDR: usize = 0xb8000;

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

/// Where a byte stream sits relative to an ANSI/VT100 escape sequence. Only `CSI` (`ESC [ ... `)
/// is actually interpreted -- see `handle_escape_byte`/`handle_csi_byte`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscState {
    Ground,
    Escape,
    Csi,
}

struct Writer {
    cursor_row: usize,
    cursor_col: usize,
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
            EscState::Ground => {}
            EscState::Escape => return self.handle_escape_byte(byte),
            EscState::Csi => return self.handle_csi_byte(byte),
        }

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

    /// The very first byte after `ESC`. Only `[` (starting a CSI sequence) is understood; any
    /// other escape (e.g. a charset-select `ESC ( B`, or a bare `ESC c` reset) is swallowed as a
    /// single unrecognized byte rather than echoed as garbage -- full-screen programs like `vi`
    /// depend on unhandled sequences disappearing silently, not corrupting the display.
    fn handle_escape_byte(&mut self, byte: u8) {
        if byte == b'[' {
            self.esc_state = EscState::Csi;
        } else {
            self.esc_state = EscState::Ground;
        }
    }

    /// One byte of a `CSI` sequence's parameter/intermediate/final bytes (ECMA-48): digits and
    /// `;` accumulate parameters, `?` (a private-mode marker, e.g. `ESC[?1049h`) is ignored since
    /// none of the private modes it introduces are implemented, and a final byte in `@`..=`~`
    /// dispatches and returns to `Ground`. Anything else (rare intermediate bytes) is ignored in
    /// place, matching real terminals' tolerance of unexpected sequences.
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
            b'?' => {}
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
    }

    /// A parsed CSI parameter, with `default` substituted both when the parameter was omitted
    /// entirely and when it was given as an explicit `0` -- true for every sequence this writer
    /// implements (cursor moves/positioning treat 0 the same as "unspecified"; erase-in-line/
    /// display's own default *is* 0, so the two cases coincide there too).
    fn csi_param(&self, index: usize, default: u16) -> u16 {
        let raw = self.csi_params.get(index).copied().unwrap_or(0);
        if raw == 0 {
            default
        } else {
            raw
        }
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
            // Everything else (device status reports, private-mode set/reset like the alternate
            // screen buffer, ...) has no effect on this plain scrolling console -- swallow it.
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

    /// Advance to column 0 of the next row, scrolling the whole screen up if already on the last
    /// row. Used both for an explicit `\n` and for wrapping a line that ran off the right edge.
    fn line_feed(&mut self) {
        self.cursor_col = 0;
        if self.cursor_row + 1 >= BUFFER_HEIGHT {
            self.scroll_up();
        } else {
            self.cursor_row += 1;
        }
    }

    fn scroll_up(&mut self) {
        for row in 1..BUFFER_HEIGHT {
            for col in 0..BUFFER_WIDTH {
                let character = self.read_char_at(row, col);
                self.write_char_at(row - 1, col, character);
            }
        }
        self.clear_row(BUFFER_HEIGHT - 1);
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
    Mutex::new(Writer {
        cursor_row: 0,
        cursor_col: 0,
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
        // SAFETY: 0xb8000 is the VGA text-mode buffer's physical address, identity-mapped by the
        // bootloader; this Writer is the only thing that ever accesses it.
        buffer: unsafe { &mut *(VGA_BUFFER_ADDR as *mut Buffer) },
    })
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
