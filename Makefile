# berth: build, install and test.
#
#   make                 static release binary
#   sudo make install    install berth, berthd and the systemd unit
#   make test            unit and CLI tests
#   make e2e             end-to-end tests against a running berthd
#   make lint            rustfmt and clippy, as CI runs them

TARGET  ?= x86_64-unknown-linux-musl
PREFIX  ?= /usr/local
BIN     := target/$(TARGET)/release/berth
UNITDIR ?= /etc/systemd/system

.PHONY: build install uninstall test e2e lint

build:
	cargo build --release --target $(TARGET)

install:
	install -Dm755 $(BIN) $(DESTDIR)$(PREFIX)/bin/berth
	ln -sf berth $(DESTDIR)$(PREFIX)/bin/berthd
	install -Dm644 dist/berthd.service $(DESTDIR)$(UNITDIR)/berthd.service
	getent group berth >/dev/null || groupadd --system berth

uninstall:
	rm -f $(DESTDIR)$(PREFIX)/bin/berth $(DESTDIR)$(PREFIX)/bin/berthd $(DESTDIR)$(UNITDIR)/berthd.service

test:
	cargo test

# Runs as you, in the berth group, so the tests reach the daemon's socket.
e2e:
	sudo -u $(USER) -g berth env PATH="$(PATH)" HOME="$(HOME)" cargo test --test e2e -- --ignored

lint:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
