# Ranch — build, install, run
#
# One static binary: `ranch` (client + daemon). `ranch daemon` (or a
# `ranchd` symlink/argv0) runs the daemon. Statically linked via musl +
# the static libghostty-vt archive — no runtime deps, drop it anywhere.
#
# First-time setup (builds the static libghostty-vt from pinned ghostty):
#   make setup          # clones ghostty, installs Zig 0.16, builds the VT lib
#   make install        # builds the static binary → ~/.local/bin/ranch
#   make service        # installs + starts the `ranchd` systemd user unit
#
# Day-to-day:
#   make run            # foreground daemon (instead of systemd)
#   make test           # unit tests
#   make clean          # cargo clean

SHELL := /bin/bash
.DEFAULT_GOAL := build

BIN_DIR := ~/.local/bin
ZIG_VER := 0.16.0
ZIG_DIR := .tools/zig
GHOSTTY_PIN := 82232ecde55405559dec29c5466cb9e39938cb41
MUSL_TARGET := x86_64-unknown-linux-musl
VT_STATIC := vendor/lib/libghostty-vt.a
STATIC_BIN := target/$(MUSL_TARGET)/release/ranch

# Cross target for the aarch64 lab demo ALC (mini). The static VT
# archive goes in its OWN dir so build.rs (RANCH_VT_LIB_DIR) picks the
# right one per target — vendor/lib/ stays the x86_64 archive.
CROSS_TARGET := aarch64-unknown-linux-musl
CROSS_VT_DIR := vendor/lib-aarch64
CROSS_VT_STATIC := $(CROSS_VT_DIR)/libghostty-vt.a
CROSS_BIN := target/$(CROSS_TARGET)/release/ranch

# zig's cc wrapper: rust triple -> zig triple (zig 0.16 dropped 'unknown')
ZIG_CC := $(CURDIR)/.tools/zig-cc

# ---- build ---------------------------------------------------------------

.PHONY: build
build: $(STATIC_BIN)
	@echo "binary: $(STATIC_BIN) (static)"

# host-default build (used by tests / `make run`)
.PHONY: build-native
build-native: $(VT_STATIC)
	cargo build --release

$(STATIC_BIN): $(VT_STATIC) $(ZIG_DIR)/zig crates/ranch-vt/build.rs
	cargo build --release --target $(MUSL_TARGET)
	@file $$($(CARGO) metadata --no-deps --format-version 1 >/dev/null 2>&1; echo target/$(MUSL_TARGET)/release/ranch) | grep -q "statically linked" || \
	  { echo "WARNING: binary is not statically linked"; }

$(VT_STATIC): vendor/ghostty $(ZIG_DIR)/zig
	@echo "building libghostty-vt STATIC archive (pinned $(GHOSTTY_PIN))…"
	cd vendor/ghostty && \
	  PATH="$(CURDIR)/$(ZIG_DIR):$$PATH" zig build -Demit-lib-vt=true -Dtarget=x86_64-linux-musl -Dcpu=baseline
	mkdir -p vendor/lib
	cp vendor/ghostty/zig-out/lib/libghostty-vt.a $(VT_STATIC)
	@echo "built $(VT_STATIC)"

# --- aarch64 cross build (lab demo ALC on mini) ---------------------------

.PHONY: build-aarch64
build-aarch64: $(CROSS_BIN)
	@echo "binary: $(CROSS_BIN) (static, aarch64)"

$(CROSS_BIN): $(CROSS_VT_STATIC) $(ZIG_DIR)/zig crates/ranch-vt/build.rs
	@command -v rustup >/dev/null && rustup target add $(CROSS_TARGET) >/dev/null 2>&1 || true
	@# Final link goes through tools/zig-link-aarch64 (zig cc -static for
	@# aarch64-linux-musl): it strips rustc's aarch64 erratum flag and
	@# rustc's self-contained crt*.o so zig's own start files are used.
	env CC_$(CROSS_TARGET)=$(CURDIR)/tools/zig-cc \
		CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=$(CURDIR)/tools/zig-link-aarch64 \
		RANCH_VT_LIB_DIR=$(CURDIR)/$(CROSS_VT_DIR) \
		cargo build --release --target $(CROSS_TARGET)
	@file $(CROSS_BIN) | grep -q "statically linked" || \
	  { echo "WARNING: cross binary is not statically linked"; }

$(CROSS_VT_STATIC): vendor/ghostty $(ZIG_DIR)/zig
	@echo "building libghostty-vt STATIC archive aarch64 (pinned $(GHOSTTY_PIN))…"
	cd vendor/ghostty && \
	  PATH="$(CURDIR)/$(ZIG_DIR):$$PATH" zig build -Demit-lib-vt=true -Dtarget=aarch64-linux-musl -Dcpu=baseline
	mkdir -p $(CROSS_VT_DIR)
	cp vendor/ghostty/zig-out/lib/libghostty-vt.a $(CROSS_VT_STATIC)
	@echo "built $(CROSS_VT_STATIC)"

# shared lib (fallback for `ranch-vt` builds without the archive; also
# used by the smoke `make run` path on glibc)
vendor/lib/libghostty-vt.so: vendor/ghostty $(ZIG_DIR)/zig
	@echo "building libghostty-vt shared (pinned $(GHOSTTY_PIN))…"
	cd vendor/ghostty/example/c-vt-stream && \
	  PATH="$(CURDIR)/$(ZIG_DIR):$$PATH" zig build
	mkdir -p vendor/lib
	cp vendor/ghostty/example/c-vt-stream/.zig-cache/o/*/libghostty-vt.so vendor/lib/libghostty-vt.so
	ln -sf libghostty-vt.so vendor/lib/libghostty-vt.so.0

vendor/ghostty:
	git clone --depth 1 https://github.com/ghostty-org/ghostty vendor/ghostty
	cd vendor/ghostty && git fetch --depth 1 origin $(GHOSTTY_PIN) && git checkout $(GHOSTTY_PIN)

$(ZIG_DIR)/zig:
	@echo "installing Zig $(ZIG_VER)…"
	mkdir -p .tools
	curl -Lo /tmp/zig.tar.xz https://ziglang.org/download/$(ZIG_VER)/zig-x86_64-linux-$(ZIG_VER).tar.xz
	tar xf /tmp/zig.tar.xz -C .tools
	mv .tools/zig-x86_64-linux-$(ZIG_VER) $(ZIG_DIR)
	@# zig cc wrapper (musl triple naming) for building ring et al
	printf '#!/bin/sh\nargs=""\nfor a in "$$@"; do\n  case "$$a" in\n    *-unknown-linux-musl) args="$$args $${a/-unknown-linux-musl/-linux-musl}" ;;\n    *) args="$$args $$a" ;;\n  esac\ndone\nexec $(CURDIR)/$(ZIG_DIR)/zig cc $$args\n' > $(ZIG_CC)
	chmod +x $(ZIG_CC)

# ---- install ---------------------------------------------------------------

.PHONY: install
install: build
	install -Dm755 $(STATIC_BIN) $(BIN_DIR)/ranch
	@# argv0 shim: `ranchd` runs the daemon (same binary)
	ln -sf ranch $(BIN_DIR)/ranchd
	@echo "installed: $(BIN_DIR)/ranch (+ ranchd symlink)"
	@echo "next: ranch login  (then 'ranch register' on daemon hosts)"

.PHONY: service
service: install
	install -Dm644 systemd/ranchd.service ~/.config/systemd/user/ranchd.service
	sed -i 's|^ExecStart=.*|ExecStart='"$$(echo ~)"'/.local/bin/ranchd|' \
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
	rm -f $(BIN_DIR)/ranch $(BIN_DIR)/ranchd
	rm -f ~/.config/systemd/user/ranchd.service
	systemctl --user daemon-reload

# ---- dev -------------------------------------------------------------------

.PHONY: run
run: build-native
	./target/release/ranch daemon

.PHONY: test
test: build-native
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
	@echo "make setup    — one-time: zig + ghostty + static libghostty-vt (via build)"
	@echo "make build         — static x86_64 binary (musl + static libghostty-vt)"
	@echo "make build-aarch64 — static aarch64 binary for the lab demo ALC (mini)"
	@echo "make install  — copy to ~/.local/bin/ranch (+ ranchd symlink)"
	@echo "make service  — install + start the ranchd systemd user unit"
	@echo "make run      — foreground daemon"
	@echo "make test     — unit tests"
	@echo "make clean    — cargo clean"
