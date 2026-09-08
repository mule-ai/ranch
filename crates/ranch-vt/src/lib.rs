//! Thin safe wrapper over `libghostty-vt` (the Ghostty terminal-emulation C
//! library, pinned source — see SPEC §11.1).
//!
//! Single-threaded by contract: a `Vt` is used from exactly one thread
//! (the daemon event loop). No interior mutability.

use std::ffi::c_void;
use std::ptr;

type GhosttyResult = i32;

/// Mirror of `GhosttyFormatterScreenExtra` (include/ghostty/vt/formatter.h).
#[repr(C)]
#[derive(Copy, Clone)]
struct FormatterScreenExtra {
    size: usize,
    cursor: bool,
    style: bool,
    hyperlink: bool,
    protection: bool,
    kitty_keyboard: bool,
    charsets: bool,
}

/// Mirror of `GhosttyFormatterTerminalExtra`.
#[repr(C)]
#[derive(Copy, Clone)]
struct FormatterTerminalExtra {
    size: usize,
    palette: bool,
    modes: bool,
    scrolling_region: bool,
    tabstops: bool,
    pwd: bool,
    keyboard: bool,
    screen: FormatterScreenExtra,
}

/// Mirror of `GhosttyFormatterTerminalOptions` (include/ghostty/vt/formatter.h).
/// `#[repr(C)]` keeps this binary-compatible with the C struct.
#[repr(C)]
#[derive(Copy, Clone)]
struct FormatterOptions {
    size: usize,
    emit: i32, // GHOSTTY_FORMATTER_FORMAT_PLAIN = 0
    unwrap: bool,
    trim: bool,
    extra: FormatterTerminalExtra,
    selection: *const c_void,
}

// GhosttyTerminalData enum values (include/ghostty/vt/terminal.h)
const DATA_COLS: i32 = 1;
const DATA_ROWS: i32 = 2;
const DATA_CURSOR_X: i32 = 3;
const DATA_CURSOR_Y: i32 = 4;
const DATA_CURSOR_VISIBLE: i32 = 7;

unsafe extern "C" {
    fn ghostty_terminal_new(
        allocator: *const c_void,
        terminal: *mut *mut c_void,
        cols: u16,
        rows: u16,
    ) -> GhosttyResult;
    fn ghostty_terminal_free(terminal: *mut c_void);
    fn ghostty_terminal_vt_write(terminal: *mut c_void, data: *const u8, len: usize);
    fn ghostty_terminal_resize(
        terminal: *mut c_void,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> GhosttyResult;
    fn ghostty_terminal_get(terminal: *mut c_void, data: i32, out: *mut c_void) -> GhosttyResult;
    fn ghostty_formatter_terminal_new(
        allocator: *const c_void,
        formatter: *mut *mut c_void,
        terminal: *mut c_void,
        options: FormatterOptions,
    ) -> GhosttyResult;
    fn ghostty_formatter_format_alloc(
        formatter: *mut c_void,
        allocator: *const c_void,
        out_ptr: *mut *mut u8,
        out_len: *mut usize,
    ) -> GhosttyResult;
    fn ghostty_formatter_free(formatter: *mut c_void);
    fn ghostty_free(allocator: *const c_void, ptr: *mut u8, len: usize);
}

/// A live terminal instance: one ghostty-vt state machine + a plain-text
/// formatter bound to it.
pub struct Vt {
    h: *mut c_void,
    fmt: *mut c_void,
}

impl Vt {
    pub fn new(cols: u16, rows: u16) -> Result<Vt, String> {
        let mut h: *mut c_void = ptr::null_mut();
        let rc = unsafe { ghostty_terminal_new(ptr::null(), &mut h, cols, rows) };
        if rc != 0 || h.is_null() {
            return Err(format!("ghostty_terminal_new failed: rc={rc}"));
        }
        let mut fmt: *mut c_void = ptr::null_mut();
        let opts = FormatterOptions {
            size: std::mem::size_of::<FormatterOptions>(),
            emit: 0, // PLAIN
            unwrap: true,
            trim: true,
            extra: FormatterTerminalExtra {
                size: std::mem::size_of::<FormatterTerminalExtra>(),
                palette: false,
                modes: false,
                scrolling_region: false,
                tabstops: false,
                pwd: false,
                keyboard: false,
                screen: FormatterScreenExtra {
                    size: std::mem::size_of::<FormatterScreenExtra>(),
                    cursor: false,
                    style: false,
                    hyperlink: false,
                    protection: false,
                    kitty_keyboard: false,
                    charsets: false,
                },
            },
            selection: ptr::null(),
        };
        let rc = unsafe { ghostty_formatter_terminal_new(ptr::null(), &mut fmt, h, opts) };
        if rc != 0 || fmt.is_null() {
            unsafe { ghostty_terminal_free(h) };
            return Err(format!("ghostty_formatter_terminal_new failed: rc={rc}"));
        }
        Ok(Vt { h, fmt })
    }

    /// Feed raw PTY output into the terminal emulator.
    pub fn write(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        unsafe { ghostty_terminal_vt_write(self.h, data.as_ptr(), data.len()) };
    }

    /// Resize the grid (triggers reflow for the primary screen).
    pub fn resize(&self, cols: u16, rows: u16) {
        unsafe {
            let _ = ghostty_terminal_resize(self.h, cols, rows, 8, 16);
        };
    }

    /// Current active screen as plain text lines (trailing whitespace
    /// trimmed; soft-wrapped lines unwrapped by the formatter).
    pub fn screen(&self) -> Vec<String> {
        let mut buf: *mut u8 = ptr::null_mut();
        let mut len: usize = 0;
        let rc = unsafe {
            ghostty_formatter_format_alloc(self.fmt, ptr::null(), &mut buf, &mut len)
        };
        if rc != 0 || buf.is_null() {
            return Vec::new();
        }
        let text = unsafe { std::slice::from_raw_parts(buf, len) };
        let s = String::from_utf8_lossy(text).into_owned();
        unsafe {
            ghostty_free(ptr::null(), buf, len);
        }
        s.lines().map(|l| l.to_string()).collect()
    }

    /// (col, row, visible) cursor position in active-screen coordinates.
    pub fn cursor(&self) -> (u16, u16, bool) {
        let mut x: u16 = 0;
        let mut y: u16 = 0;
        let mut visible: bool = true;
        unsafe {
            let _ =
                ghostty_terminal_get(self.h, DATA_CURSOR_X, &mut x as *mut u16 as *mut c_void);
            let _ =
                ghostty_terminal_get(self.h, DATA_CURSOR_Y, &mut y as *mut u16 as *mut c_void);
            let _ = ghostty_terminal_get(
                self.h,
                DATA_CURSOR_VISIBLE,
                &mut visible as *mut bool as *mut c_void,
            );
        }
        (x, y, visible)
    }

    /// Current (cols, rows) of the grid.
    pub fn dims(&self) -> (u16, u16) {
        let mut cols: u16 = 0;
        let mut rows: u16 = 0;
        unsafe {
            let _ =
                ghostty_terminal_get(self.h, DATA_COLS, &mut cols as *mut u16 as *mut c_void);
            let _ =
                ghostty_terminal_get(self.h, DATA_ROWS, &mut rows as *mut u16 as *mut c_void);
        }
        (cols, rows)
    }
}

impl Drop for Vt {
    fn drop(&mut self) {
        if !self.fmt.is_null() {
            unsafe { ghostty_formatter_free(self.fmt) };
            self.fmt = ptr::null_mut();
        }
        if !self.h.is_null() {
            unsafe { ghostty_terminal_free(self.h) };
            self.h = ptr::null_mut();
        }
    }
}
