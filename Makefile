# Ranch — build, install, run
#
# First-time setup (builds libghostty-vt from the pinned ghostty source):
#   make setup          # clones ghostty, installs Zig 0.16, builds the VT lib
#   make install        # builds release binaries → ~/.local/bin
#   make service        # installs + starts the ranchd systemd user unit
#
# Day-to-day:
#   make                # = make build
#   make run            # foreground daemon (instead of systemd)
#   make test           # unit tests
#   make clean          # cargo clean

SHELL := /bin/bash
.DEFAULT_GOAL := build

BIN_DIR := ~/.local/bin
ZIG_VER := 0.16.0
ZIG_DIR := .tools/zig
GHOSTTY_PIN := 82232ecde55405559dec29c5466cb9e39938cb41
VT_LIB := vendor/lib/libghostty-vt.so

# ---- build ---------------------------------------------------------------

.PHONY: build
build: $(VT_LIB)
	cargo build --release
	@echo "binaries: target/release/{ranch-cli,ranch-daemon}"

$(VT_LIB): vendor/ghostty $(ZIG_DIR)/zig
	@echo "building libghostty-vt (pinned $(GHOSTTY_PIN))…"
	cd vendor/ghostty/example/c-vt-stream && \
	  PATH="$$PWD/../../$(ZIG_DIR):$$PATH" zig build
	mkdir -p vendor/lib
	cp vendor/ghostty/example/c-vt-stream/.zig-cache/o/*/libghostty-vt.so $(VT_LIB)
	ln -sf libghostty-vt.so vendor/lib/libghostty-vt.so.0
	@echo "built $(VT_LIB)"

vendor/ghostty:
	git clone --depth 1 https://github.com/ghostty-org/ghostty vendor/ghostty
	cd vendor/ghostty && git fetch --depth 1 origin $(GHOSTTY_PIN) && git checkout $(GHOSTTY_PIN)

$(ZIG_DIR)/zig:
	@echo "installing Zig $(ZIG_VER)…"
	mkdir -p .tools
	curl -Lo /tmp/zig.tar.xz https://ziglang.org/download/$(ZIG_VER)/zig-x86_64-linux-$(ZIG_VER).tar.xz
	tar xf /tmp/zig.tar.xz -C .tools
	mv .tools/zig-x86_64-linux-$(ZIG_VER) $(ZIG_DIR)

# ---- install ---------------------------------------------------------------

.PHONY: install
install: build
	install -Dm755 target/release/ranch-cli $(BIN_DIR)/ranch
	install -Dm755 target/release/ranch-daemon $(BIN_DIR)/ranch-daemon
	@echo "installed: $(BIN_DIR)/ranch, $(BIN_DIR)/ranch-daemon"
	@echo "next: ranch login  (then 'ranch register' on daemon hosts)"

.PHONY: service
service: install
	install -Dm644 systemd/ranchd.service ~/.config/systemd/user/ranchd.service
	sed -i 's|^ExecStart=.*|ExecStart='"$$(echo ~)"'/.local/bin/ranch-daemon|' \
	  ~/.config/systemd/user/ranchd.service
	systemctl --user daemon-reload
	systemctl --user enable --now ranchd
	@sleep 1
	systemctl --user status ranchd --no-pager | head -5 || true
	@echo "logs: journalctl --user -u ranchd -f"

.PHONY: service-stop
service-stop:
	systemctl --user disable --now ranchd || true

.PHONY: uninstall
uninstall: service-stop
	rm -f $(BIN_DIR)/ranch $(BIN_DIR)/ranch-daemon
	rm -f ~/.config/systemd/user/ranchd.service
	systemctl --user daemon-reload

# ---- dev -------------------------------------------------------------------

.PHONY: run
run: build
	./target/release/ranch-daemon

.PHONY: test
test:
	cargo test

.PHONY: fmt
fmt:
	cargo fmt --all

.PHONY: lint
lint:
	cargo clippy --all-targets

.PHONY: clean
clean:
	cargo clean

.PHONY: help
help:
	@echo "make setup    — one-time: zig + ghostty + libghostty-vt (via build)"
	@echo "make build    — release build (auto-builds libghostty-vt if missing)"
	@echo "make install  — copy binaries to ~/.local/bin"
	@echo "make service  — install + start the ranchd systemd user unit"
	@echo "make run      — foreground daemon"
	@echo "make test     — unit tests"
	@echo "make clean    — cargo clean"
