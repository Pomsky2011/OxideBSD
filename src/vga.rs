use core::fmt;

use spin::{Lazy, Mutex};
use x86_64::instructions::interrupts;

const BUFFER_HEIGHT: usize = 25;
const BUFFER_WIDTH: usize = 80;
const VGA_BUFFER_ADDR: usize = 0xb8000;

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

struct Writer {
    column_position: usize,
    color_code: ColorCode,
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
        match byte {
            b'\n' => self.new_line(),
            // Backspace: step the cursor back a column and blank the character that was there,
            // mirroring `src/serial.rs`'s `SerialPort::send`, which expands a raw 0x08/0x7f into
            // the standard "\x08 \x08" terminal idiom for the same reason -- a caller (see
            // `userland/stsh/`'s `read_line`) just writes a single raw backspace byte and expects
            // *something* to actually erase the character, not just move a cursor over it.
            // Doesn't cross a line boundary; nothing in this kernel tracks cursor position across
            // wrapped rows.
            0x08 => {
                self.column_position = self.column_position.saturating_sub(1);
                let row = BUFFER_HEIGHT - 1;
                let col = self.column_position;
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
            byte => {
                if self.column_position >= BUFFER_WIDTH {
                    self.new_line();
                }

                let row = BUFFER_HEIGHT - 1;
                let col = self.column_position;
                let color_code = self.color_code;
                self.write_char_at(
                    row,
                    col,
                    ScreenChar {
                        ascii_character: byte,
                        color_code,
                    },
                );
                self.column_position += 1;
            }
        }
    }

    fn write_string(&mut self, s: &str) {
        for byte in s.bytes() {
            match byte {
                // Printable ASCII, plus newline and backspace (see `write_byte`).
                0x20..=0x7e | b'\n' | 0x08 => self.write_byte(byte),
                // Anything else isn't representable in code page 437; show a placeholder.
                _ => self.write_byte(0xfe),
            }
        }
    }

    fn new_line(&mut self) {
        for row in 1..BUFFER_HEIGHT {
            for col in 0..BUFFER_WIDTH {
                let character = self.read_char_at(row, col);
                self.write_char_at(row - 1, col, character);
            }
        }
        self.clear_row(BUFFER_HEIGHT - 1);
        self.column_position = 0;
    }

    fn clear_row(&mut self, row: usize) {
        let blank = ScreenChar {
            ascii_character: b' ',
            color_code: self.color_code,
        };
        for col in 0..BUFFER_WIDTH {
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
        column_position: 0,
        color_code: ColorCode::new(Color::LightGray, Color::Black),
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
