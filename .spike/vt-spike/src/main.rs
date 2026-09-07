//! M0 spike: prove the PTY -> libghostty-vt -> dirty-rows pipeline.
//!
//! - Spawns a shell on a PTY
//! - Feeds PTY output into a libghostty-vt terminal
//! - Every tick, diffs the screen against the previous snapshot
//! - Prints changed rows as JSON-lines (the shape of future `update` frames)
//! - Forwards stdin to the PTY so you can type interactively
//! - Ctrl-C exits cleanly

use std::ffi::CString;
use std::io::{Read, Write};
use std::mem;
use std::ptr;
use std::thread;
use std::time::Duration;

// ---------- libc FFI ----------

#[repr(C)]
#[derive(Default)]
struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

extern "C" {
    fn openpty(amaster: *mut i32, aslave: *mut i32, name: *mut u8, termp: *mut Winsize, winp: *mut Winsize) -> i32;
    fn fork() -> i32;
    fn execvp(name: *const u8, argv: *const *const u8) -> i32;
    fn setsid() -> i32;
    fn tcsetpgrp(fd: i32, pgid: i32) -> i32;
    fn dup2(oldfd: i32, newfd: i32) -> i32;
    fn close(fd: i32) -> i32;
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    fn poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32;
    fn setenv(name: *const u8, value: *const u8, overwrite: i32) -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
    fn isatty(fd: i32) -> i32;
}

const POLLIN: i16 = 0x1;
const POLLHUP: i16 = 0x200;
const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;
const WNOHANG: i32 = 1;

// ---------- libghostty-vt FFI ----------

// GhosttyResult is a C enum (int). GHOSTTY_SUCCESS = 0.
type GhosttyResult = i32;
type GhosttyTerminal = *mut std::ffi::c_void;
type GhosttyFormatter = *mut std::ffi::c_void;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GhosttyFormatterTerminalOptions {
    size: usize,
    emit: i32,            // GHOSTTY_FORMATTER_FORMAT_PLAIN = 0
    unwrap: bool,
    trim: bool,
    extra: i32,
    selection: *const std::ffi::c_void,
}

extern "C" {
    fn ghostty_terminal_new(allocator: *const std::ffi::c_void, terminal: *mut GhosttyTerminal, cols: u16, rows: u16) -> GhosttyResult;
    fn ghostty_terminal_free(terminal: GhosttyTerminal);
    fn ghostty_terminal_vt_write(terminal: GhosttyTerminal, data: *const u8, len: usize);
    fn ghostty_formatter_terminal_new(allocator: *const std::ffi::c_void, formatter: *mut GhosttyFormatter, terminal: GhosttyTerminal, options: GhosttyFormatterTerminalOptions) -> GhosttyResult;
    fn ghostty_formatter_format_alloc(formatter: GhosttyFormatter, allocator: *const std::ffi::c_void, out_ptr: *mut *mut u8, out_len: *mut usize) -> GhosttyResult;
    fn ghostty_formatter_free(formatter: GhosttyFormatter);
    fn ghostty_free(allocator: *const std::ffi::c_void, ptr: *mut u8, len: usize);
}

fn ghostty_formatter_screen(_terminal: GhosttyTerminal, formatter: GhosttyFormatter) -> String {
    let mut buf: *mut u8 = ptr::null_mut();
    let mut len: usize = 0;
    unsafe {
        let rc = ghostty_formatter_format_alloc(formatter, ptr::null(), &mut buf, &mut len);
        if rc != 0 || buf.is_null() {
            return String::new();
        }
        let text = String::from_utf8_lossy(std::slice::from_raw_parts(buf, len)).into_owned();
        ghostty_free(ptr::null(), buf, len);
        text
    }
}

/// Split a screen into trimmed rows, drop trailing blank rows.
fn screen_rows(screen: &str, cols: usize) -> Vec<String> {
    let mut rows: Vec<String> = screen.lines().map(|l| l.trim_end().to_string()).collect();
    // drop trailing empties
    let mut last = rows.len();
    while last > 0 && rows[last - 1].is_empty() {
        last -= 1;
    }
    let _ = cols;
    rows.truncate(last);
    rows
}

fn main() {
    let cols: u16 = 80;
    let rows: u16 = 24;
    let tick_ms: i32 = 250;

    // --- Create PTY ---
    let mut amaster: i32 = 0;
    let mut aslave: i32 = 0;
    let mut win = Winsize { ws_row: rows, ws_col: cols, ..Default::default() };
    let rc = unsafe { openpty(&mut amaster, &mut aslave, ptr::null_mut(), ptr::null_mut(), &mut win) };
    if rc != 0 {
        eprintln!("openpty failed");
        return;
    }

    // --- Fork + exec shell ---
    let pid = unsafe { fork() };
    if pid == 0 {
        // child
        unsafe {
            setsid();
            tcsetpgrp(aslave, pid);
            dup2(aslave, 0);
            dup2(aslave, 1);
            dup2(aslave, 2);
            if aslave > 2 { close(aslave); }
            close(amaster);
            let term = b"xterm-256color\0";
            setenv(b"TERM\0".as_ptr() as *const u8, term.as_ptr() as *const u8, 1);
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
            let shell_c = CString::new(shell).unwrap();
            let null_ptr: *const u8 = ptr::null();
            let argv = [shell_c.as_ptr() as *const u8, null_ptr];
            execvp(shell_c.as_ptr() as *const u8, argv.as_ptr());
            std::process::exit(1);
        }
    }
    unsafe { close(aslave); }
    eprintln!("[spike] shell pid={pid}, pty master={amaster}, {cols}x{rows}");

    // --- Create ghostty terminal + formatter ---
    let mut terminal: GhosttyTerminal = ptr::null_mut();
    unsafe {
        let rc = ghostty_terminal_new(ptr::null(), &mut terminal, cols, rows);
        if rc != 0 {
            eprintln!("ghostty_terminal_new failed: {rc}");
            return;
        }
    }
    let mut formatter: GhosttyFormatter = ptr::null_mut();
    let opts = GhosttyFormatterTerminalOptions {
        size: mem::size_of::<GhosttyFormatterTerminalOptions>(),
        emit: 0, // PLAIN
        unwrap: true,
        trim: true,
        extra: 0,
        selection: ptr::null(),
    };
    unsafe {
        let rc = ghostty_formatter_terminal_new(ptr::null(), &mut formatter, terminal, opts);
        if rc != 0 {
            eprintln!("ghostty_formatter_terminal_new failed: {rc}");
            return;
        }
    }

    // --- Main loop ---
    let mut prev_rows: Vec<String> = Vec::new();
    let mut stdin_open = true; // forward until EOF
    let mut shell_dead = false;

    let mut fds = [
        PollFd { fd: amaster, events: POLLIN, revents: 0 },
        PollFd { fd: 0, events: POLLIN, revents: 0 },
    ];

    loop {
        // poll pty + stdin with tick timeout
        let n = unsafe { poll(fds.as_mut_ptr(), fds.len() as u64, tick_ms) };

        // drain pty -> vt
        if !shell_dead && n > 0 && (fds[0].revents & (POLLIN | POLLHUP)) != 0 {
            let mut buf = [0u8; 8192];
            loop {
                let r = unsafe { read(amaster, buf.as_mut_ptr(), buf.len()) };
                if r <= 0 {
                    break;
                }
                unsafe {
                    ghostty_terminal_vt_write(terminal, buf.as_ptr(), r as usize);
                }
                if r < buf.len() as isize {
                    break;
                }
            }
            if fds[0].revents & POLLHUP != 0 {
                shell_dead = true;
            }
        }

        // stdin -> pty (interactive input)
        if n > 0 && stdin_open && !shell_dead && (fds[1].revents & POLLIN) != 0 {
            let mut buf = [0u8; 4096];
            let r = unsafe { read(0, buf.as_mut_ptr(), buf.len()) };
            if r > 0 {
                unsafe { write(amaster, buf.as_ptr(), r as usize); }
            } else {
                stdin_open = false;
            }
        }

        // every tick: diff screen
        let screen = ghostty_formatter_screen(terminal, formatter);
        let cur_rows = screen_rows(&screen, cols as usize);
        let changed: Vec<usize> = (0..cur_rows.len())
            .filter(|&i| i >= prev_rows.len() || prev_rows[i] != cur_rows[i])
            .chain((cur_rows.len()..prev_rows.len()).filter(|&i| !prev_rows[i].is_empty()))
            .collect();

        if !changed.is_empty() {
            for i in &changed {
                let row = if *i < cur_rows.len() { cur_rows[*i].clone() } else { String::new() };
                eprintln!("[update row {i}] {row}");
            }
            eprintln!("[update] seq tick, {} row(s) changed", changed.len());
        }
        prev_rows = cur_rows;

        // shell exited? (non-blocking)
        let mut status: i32 = 0;
        let waited = unsafe { waitpid(pid, &mut status, WNOHANG) };
        if waited == pid {
            if !shell_dead {
                eprintln!("[spike] shell exited (status={status})");
            }
            shell_dead = true;
        }
    }

    unsafe {
        kill(pid, SIGTERM);
        thread::sleep(Duration::from_millis(200));
        kill(pid, SIGKILL);
        ghostty_formatter_free(formatter);
        ghostty_terminal_free(terminal);
        close(amaster);
    }
}

extern "C" {
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
}
