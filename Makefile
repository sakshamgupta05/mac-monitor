# mac-monitor build, test and install targets.
PREFIX  ?= $(HOME)/.local
BIN     := target/release/mac-monitor
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
TARGET  := aarch64-apple-darwin
DIST    := dist/mac-monitor-$(VERSION)-$(TARGET)

.PHONY: all web build test test-rust test-e2e install uninstall dist clean

all: build

web/node_modules: web/package-lock.json
	cd web && npm ci
	touch web/node_modules

## Bundle the dashboard into web/dist (embedded into the binary)
web: web/node_modules
	cd web && npm run typecheck && npm run build

## Release binary with the dashboard embedded
build: web
	cargo build --release

test: test-rust test-e2e

test-rust:
	cargo test

## Browser tests against a real server on a scratch database
test-e2e: build
	cd web && npx playwright install chromium && npx playwright test

## Install the binary into $(PREFIX)/bin and start it at login
install: build
	install -d $(PREFIX)/bin
	install -m 755 $(BIN) $(PREFIX)/bin/mac-monitor
	$(PREFIX)/bin/mac-monitor install

uninstall:
	-$(PREFIX)/bin/mac-monitor uninstall
	rm -f $(PREFIX)/bin/mac-monitor

## Release tarball and checksum in dist/
dist: build
	rm -rf $(DIST) && mkdir -p $(DIST)
	cp $(BIN) README.md LICENSE NOTICE $(DIST)/
	tar -C dist -czf $(DIST).tar.gz $(notdir $(DIST))
	cd dist && shasum -a 256 $(notdir $(DIST)).tar.gz > $(notdir $(DIST)).tar.gz.sha256
	@echo "Built $(DIST).tar.gz"

clean:
	cargo clean
	rm -rf dist web/dist
