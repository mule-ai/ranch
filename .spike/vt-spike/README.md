# M0 Spike: PTY → libghostty-vt → dirty rows

Proves the core Ranch architecture: the daemon owns the terminal state
via `libghostty-vt`, and clients only ever receive semantic row updates.

## What it does

- Spawns a shell on a PTY (`openpty` + `fork`/`execvp`)
- Feeds all PTY output into a `libghostty-vt` terminal instance
- Every 250 ms, formats the screen to plain text and diffs it against
  the previous snapshot
- Emits `[update row N] ...` lines for every changed row — the shape of
  the `update` frame in `docs/PROTOCOL.md`
- Forwards piped stdin into the PTY (input path)
- Detects shell exit via non-blocking `waitpid`

## Build

Prerequisites (one-time):

```sh
# 1. Zig 0.16.0
curl -LO https://ziglang.org/download/0.16.0/zig-x86_64-linux-0.16.0.tar.xz
tar xf zig-x86_64-linux-0.16.0.tar.xz && ln -sf zig-x86_64-linux-0.16.0 zig

# 2. libghostty-vt from pinned ghostty source (commit 82232ecb)
git clone --depth 1 https://github.com/ghostty-org/ghostty ../../vendor/ghostty
cd ../../vendor/ghostty/example/c-vt-stream
PATH="<repo>/.tools/zig:$PATH" zig build
cp .zig-cache/o/*/libghostty-vt.so lib/libghostty-vt.so
cd lib && ln -sf libghostty-vt.so libghostty-vt.so.0   # runtime SONAME

# 3. Rust spike
cd .spike/vt-spike && cargo build
```

## Run

```sh
printf 'echo hello\nsleep 1\nexit\n' | ./target/debug/vt-spike
```

Expected: `[update row N] ...` lines as the shell's output lands, then
`[spike] shell exited (status=0)` and a clean exit.

## M0 verdict

PASS (2026-09-07):
- `libghostty-vt.so.0` links cleanly into a Rust binary (only libc/libm deps)
- Terminal + PLAIN-text formatter work as documented in `include/ghostty/vt/`
- Row-diffing at 250 ms produces exactly the changed rows — confirmed
  droppable-update semantics (any missed frame is superseded by the next)
- PTY lifecycle (openpty, fork/exec, poll, waitpid WNOHANG) stable
