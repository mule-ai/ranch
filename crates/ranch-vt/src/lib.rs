//! Thin safe wrapper over `libghostty-vt` (the Ghostty terminal-emulation C
//! library, pinned source — see SPEC §11.1).
//!
//! Single-threaded by contract: a `Vt` is used from exactly one thread
//! (the daemon event loop). No interior mutability.

use std::ffi::c_void;
use std::os::fd::RawFd;
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

// GhosttyTerminalOption enum values (include/ghostty/vt/terminal.h)
const OPT_USERDATA: i32 = 0;
const OPT_WRITE_PTY: i32 = 1;

unsafe extern "C" {
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
}

/// GhosttyTerminalWritePtyFn — responses to terminal queries (DSR, OSC
/// status, DA, mode reports) are written back to the PTY so the
/// application inside sees them, exactly like a real terminal.
/// userdata carries the PTY master fd as an integer.
unsafe extern "C" fn on_write_pty(
    _terminal: *mut c_void,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    let fd = userdata as i32;
    if fd > 0 && len > 0 && !data.is_null() {
        unsafe {
            write(fd, data, len);
        }
    }
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
    fn ghostty_terminal_set(
        terminal: *mut c_void,
        option: i32,
        value: *const c_void,
    ) -> GhosttyResult;
    fn ghostty_terminal_scroll_viewport(terminal: *mut c_void, behavior: ScrollViewport);
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

/// Mirror of GhosttyTerminalScrollViewport (include/ghostty/vt/terminal.h).
/// Tagged union: `tag: c_int` + 16-byte value union (alignment 8).
#[repr(C)]
struct ScrollViewport {
    tag: i32,
    _pad: i32,
    value: [u64; 2], // union { intptr_t delta; size_t row; uint64_t _padding[2]; }
}

const SCROLL_VIEWPORT_BOTTOM: i32 = 1; // TOP=0, BOTTOM=1, DELTA=2, ROW=3

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
        let rc =
            unsafe { ghostty_formatter_terminal_new(ptr::null(), &mut fmt, h, Self::formatter_opts()) };
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
        unsafe {
            ghostty_terminal_vt_write(self.h, data.as_ptr(), data.len());
        }
        self.pin_bottom();
    }

    fn pin_bottom(&self) {
        let behavior = ScrollViewport {
            tag: SCROLL_VIEWPORT_BOTTOM,
            _pad: 0,
            value: [0; 2],
        };
        unsafe { ghostty_terminal_scroll_viewport(self.h, behavior) };
    }

    /// Bind the terminal to a PTY master fd: terminal query responses
    /// (device attributes, colors, modes) are written back to the fd so
    /// applications inside receive them. Without this, capability
    /// queries go unanswered and TUIs degrade to fallback rendering.
    pub fn attach_pty(&self, master_fd: RawFd) {
        unsafe {
            ghostty_terminal_set(self.h, OPT_USERDATA, master_fd as *const c_void);
            ghostty_terminal_set(self.h, OPT_WRITE_PTY, on_write_pty as *const c_void);
        }
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
            if std::env::var("RANCH_VT_DEBUG").is_ok() {
                eprintln!("vt: format_alloc failed rc={rc} buf_null={}", buf.is_null());
            }
            return Vec::new();
        }
        let text = unsafe { std::slice::from_raw_parts(buf, len) };
        let s = String::from_utf8_lossy(text).into_owned();
        unsafe {
            ghostty_free(ptr::null(), buf, len);
        }
        // The formatter emits the whole scrollable area (scrollback +
        // active screen). Only the visible viewport — the last `rows`
        // lines — is the current screen.
        let (_, rows) = self.dims();
        let rows = rows as usize;
        let mut lines: Vec<String> = s.lines().map(|l| l.to_string()).collect();
        if std::env::var("RANCH_VT_DEBUG").is_ok() {
            eprintln!("vt: format ok raw={} lines={} rows={rows}", s.len(), lines.len());
        }
        if lines.len() > rows {
            lines.drain(..lines.len() - rows);
        }
        lines
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

    /// The whole scrollable area (scrollback + active screen), oldest
    /// first. Used for scrollback capture: diffing consecutive full
    /// areas tells us exactly which rows scrolled off the top.
    pub fn full_screen(&self) -> Vec<String> {
        self.pin_bottom();
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

    fn formatter_opts() -> FormatterOptions {
        FormatterOptions {
            size: std::mem::size_of::<FormatterOptions>(),
            emit: 1, // VT: one line per grid row with inline SGR runs
            // unwrap merges soft-wrapped lines — that breaks the
            // one-line-per-grid-row contract row updates depend on
            unwrap: false,
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
                    style: true,
                    hyperlink: false,
                    protection: false,
                    kitty_keyboard: false,
                    charsets: false,
                },
            },
            selection: ptr::null(),
        }
    }

    /// Scrollbar state:    /// Scrollbar state: (total scrollable rows, viewport offset, visible len).
    pub fn scrollbar(&self) -> (u64, u64, u64) {
        // Mirror of GhosttyTerminalScrollbar { total, offset, len }
        #[repr(C)]
        struct Scrollbar { total: u64, offset: u64, len: u64 }
        const DATA_SCROLLBAR: i32 = 9;
        let mut sb = Scrollbar { total: 0, offset: 0, len: 0 };
        unsafe {
            let _ = ghostty_terminal_get(
                self.h,
                DATA_SCROLLBAR,
                &mut sb as *mut Scrollbar as *mut c_void,
            );
        }
        (sb.total, sb.offset, sb.len)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_scrolls_when_full() {
        let vt = Vt::new(80, 6).unwrap();
        let mut input = String::new();
        for i in 1..=20 {
            input.push_str(&format!("LINE{i}\r\n"));
        }
        vt.write(input.as_bytes());
        let screen = vt.screen();
        let joined = screen.join("\n");
        assert!(
            joined.contains("LINE20"),
            "screen should show the newest line; got:\n{joined}"
        );
        assert!(!joined.contains("LINE14"), "scrolled-off lines must not leak; got:\n{joined}");
        assert_eq!(screen.len(), 6, "screen() must return exactly `rows` lines");
    }

    /// Reproduce the daemon's read pattern: many small write() batches.
    #[test]
    fn screen_scrolls_with_many_small_writes() {
        let vt = Vt::new(80, 24).unwrap();
        for i in 1..=30 {
            vt.write(format!("LINE{i}\r\n").as_bytes());
        }
        let screen = vt.screen();
        let joined = screen.join("\n");
        assert!(
            joined.contains("LINE30"),
            "screen should show the newest line; got:\n{joined}"
        );
    }

    /// The daemon resizes each pane right after spawn (session bootstrap).
    /// Does a resize break viewport tracking?
    #[test]
    fn screen_scrolls_after_resize() {
        let vt = Vt::new(80, 24).unwrap();
        vt.resize(96, 24);
        for i in 1..=30 {
            vt.write(format!("LINE{i}\r\n").as_bytes());
        }
        let screen = vt.screen();
        let joined = screen.join("\n");
        assert!(
            joined.contains("LINE30"),
            "screen should show the newest line; got:\n{joined}"
        );
    }

    /// Replay the exact bytes the daemon captured from a real bash/starship
    /// PTY (/tmp/pty-capture.bin) — reproduces the stale-formatter bug.
    #[test]
    fn replay_real_pty_capture() {
        let data = std::fs::read("/tmp/pty-capture.bin").expect("capture file");
        let vt = Vt::new(80, 24).unwrap();
        vt.write(&data);
        let screen = vt.screen();
        let joined = screen.join("\n");
        eprintln!("replay screen tail: {:?}", screen.last());
        eprintln!("replay has LINE30: {}", joined.contains("LINE30"));
        let (t, o, l) = vt.scrollbar();
        eprintln!("replay scrollbar: total={t} offset={o} len={l}");
        assert!(joined.contains("LINE30"), "formatter must follow the viewport; got:\n{joined}");
    }

    /// Replay with the daemon's exact batch boundaries + interleaved
    /// screen() calls (the daemon formats on every dirty tick).
    #[test]
    fn replay_in_daemon_batches() {
        let data = std::fs::read("/tmp/pty-capture.bin").expect("capture file");
        let vt = Vt::new(80, 24).unwrap();
        // daemon trace batches: 32, 123, 53, 231, 32, 123, 11, 32, 123
        let bounds = [32usize, 155, 208, 439, 471, 594];
        let mut start = 0;
        for &end in &bounds {
            if end <= data.len() {
                vt.write(&data[start..end]);
                let _ = vt.screen(); // dirty tick format
                start = end;
            }
        }
        if start < data.len() {
            vt.write(&data[start..]);
        }
        let screen = vt.screen();
        let joined = screen.join("\n");
        eprintln!("batched has LINE30: {}", joined.contains("LINE30"));
        let (t, o, l) = vt.scrollbar();
        eprintln!("batched scrollbar: total={t} offset={o} len={l}");
        assert!(joined.contains("LINE30"), "formatter must follow; got:\n{joined}");
    }

    /// Prove scroll_viewport works: pin to TOP must move the viewport
    /// offset to 0 (screen() re-pins to bottom, so assert on scrollbar).
    #[test]
    fn viewport_pin_takes_effect() {
        let vt = Vt::new(80, 6).unwrap();
        for i in 1..=20 {
            vt.write(format!("LINE{i}\r\n").as_bytes());
        }
        assert_eq!(vt.scrollbar(), (21, 15, 6), "write() pins viewport to bottom");
        unsafe {
            let behavior = ScrollViewport { tag: 0, _pad: 0, value: [0; 2] }; // TOP
            ghostty_terminal_scroll_viewport(vt.h, behavior);
        }
        assert_eq!(vt.scrollbar(), (21, 0, 6), "TOP pin must move offset to 0");
    }

    /// Starship-style prompt with escape sequences + scroll.
    #[test]
    fn screen_scrolls_with_escapes() {
        let vt = Vt::new(80, 24).unwrap();
        // prompt with color/cursor escapes
        let prompt = "\x1b[1;32m\u{276f}\x1b[0m \x1b[36m~/src\x1b[0m \r\n";
        vt.write(prompt.as_bytes());
        // command echo comes from the pty too
        vt.write(b"for i in $(seq 1 30); do echo LINE$i; done\r\n");
        for i in 1..=30 {
            vt.write(format!("LINE{i}\r\n").as_bytes());
        }
        let screen = vt.screen();
        let joined = screen.join("\n");
        assert!(
            joined.contains("LINE30"),
            "screen should show the newest line; got:\n{joined}"
        );
    }

    /// The daemon formats the screen between writes (every dirty tick).
    #[test]
    fn screen_scrolls_with_interleaved_format() {
        let vt = Vt::new(80, 24).unwrap();
        for i in 1..=30 {
            vt.write(format!("LINE{i}\r\n").as_bytes());
            let _ = vt.screen(); // the daemon formats every dirty tick
        }
        let screen = vt.screen();
        let joined = screen.join("\n");
        assert!(
            joined.contains("LINE30"),
            "screen should show the newest line; got:\n{joined}"
        );
    }
}

#[cfg(test)]
mod alt_tests {
    use super::*;

    #[test]
    fn alt_screen_content() {
        let vt = Vt::new(80, 24).unwrap();
        vt.write(b"\x1b[?1049h"); // enter alt screen
        vt.write(b"\x1b[2J\x1b[H"); // clear
        vt.write(b"ALT SCREEN TOP\r\n");
        vt.write(b"second line\r\n");
        let screen = vt.screen();
        eprintln!("alt screen: {:?}", screen);
        assert!(screen.iter().any(|l| l.contains("ALT SCREEN TOP")), "alt screen content must render; got {:?}", screen);
    }

    #[test]
    fn primary_after_alt_return() {
        let vt = Vt::new(80, 24).unwrap();
        vt.write(b"PRIMARY\r\n");
        vt.write(b"\x1b[?1049hALT\r\n");
        vt.write(b"\x1b[?1049l"); // back to primary
        let screen = vt.screen();
        eprintln!("after 1049l: {:?}", screen);
        assert!(screen.iter().any(|l| l.contains("PRIMARY")));
    }
}

#[cfg(test)]
mod vt_format_tests {
    use super::*;

    // VT-mode formatter: what shape is the output? (rows with SGR runs?)
    #[test]
    fn vt_emit_shape() {
        let vt = Vt::new(40, 6).unwrap();
        // colored text via direct escapes
        vt.write(b"\x1b[1;31mRED\x1b[0m plain\r\n");
        vt.write(b"\x1b[32mGREEN\x1b[0m\r\n");
        let out = vt.screen();
        eprintln!("VT OUTPUT: {:?}", out);
        assert!(out[0].contains("\u{1b}[1m") || out[0].contains("RED"), "styled rows expected");
        assert!(out[0].contains("plain"));
    }
}
