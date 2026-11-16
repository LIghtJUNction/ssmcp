PREFIX ?= /usr/local
DESTDIR ?=
SYSCONFDIR ?= /etc
SYSTEMD_DIR ?= /usr/lib/systemd/system
CARGO ?= cargo
CARGO_TARGET_DIR ?= target

.PHONY: all build check test install install-server
all: build

build:
	$(CARGO) build --locked --release

check:
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --locked --all-targets -- -D warnings

test:
	$(CARGO) test --locked
	$(CARGO) test --locked --lib code_mode::tests -- --include-ignored
	$(CARGO) test --locked --test two_tools -- --include-ignored
	$(CARGO) build --locked
	python3 scripts/e2e.py --bin-dir $(CARGO_TARGET_DIR)/debug
	python3 scripts/e2e_agents.py --bin-dir $(CARGO_TARGET_DIR)/debug

install:
	install -d $(DESTDIR)$(PREFIX)/bin $(DESTDIR)$(PREFIX)/share/man/man1 $(DESTDIR)$(PREFIX)/share/man/man8
	install -m 0755 $(CARGO_TARGET_DIR)/release/ssmcp $(CARGO_TARGET_DIR)/release/ssmcpd $(DESTDIR)$(PREFIX)/bin/
	install -d $(DESTDIR)$(PREFIX)/libexec/ssmcp
	install -m 0644 src/code_mode.js $(DESTDIR)$(PREFIX)/libexec/ssmcp/code-mode.js
	install -m 0644 packaging/man/ssmcp.1 $(DESTDIR)$(PREFIX)/share/man/man1/
	install -m 0644 packaging/man/ssmcpd.8 $(DESTDIR)$(PREFIX)/share/man/man8/
	install -d $(DESTDIR)$(PREFIX)/share/doc/ssmcp/examples $(DESTDIR)$(PREFIX)/share/doc/ssmcp/docs/assets
	install -m 0644 README.md $(DESTDIR)$(PREFIX)/share/doc/ssmcp/
	install -m 0644 docs/*.md $(DESTDIR)$(PREFIX)/share/doc/ssmcp/docs/
	install -m 0644 docs/assets/*.svg docs/assets/*.jpg $(DESTDIR)$(PREFIX)/share/doc/ssmcp/docs/assets/
	install -m 0644 packaging/sshd-ssmcp.conf packaging/Caddyfile packaging/ssmcpd.env packaging/ssmcpd.toml.example $(DESTDIR)$(PREFIX)/share/doc/ssmcp/examples/

# Installs the unit and a first-time config; account creation and activation are explicit.
install-server: install
	install -d $(DESTDIR)$(SYSTEMD_DIR) $(DESTDIR)$(SYSCONFDIR)/ssmcpd
	sed 's|/usr/local/bin/|$(PREFIX)/bin/|g; s|/etc/ssmcpd/|$(SYSCONFDIR)/ssmcpd/|g' packaging/ssmcpd.service > $(DESTDIR)$(SYSTEMD_DIR)/ssmcpd.service
	chmod 0644 $(DESTDIR)$(SYSTEMD_DIR)/ssmcpd.service
	test -e $(DESTDIR)$(SYSCONFDIR)/ssmcpd/ssmcpd.env || install -m 0644 packaging/ssmcpd.env $(DESTDIR)$(SYSCONFDIR)/ssmcpd/ssmcpd.env

# Optional portal integration; does not activate services or modify sshd configuration.
.PHONY: install-portal
install-portal:
	install -d $(DESTDIR)$(PREFIX)/libexec/ssmcp $(DESTDIR)$(SYSTEMD_DIR)
	install -m 0644 scripts/sync_portal_ssh.py $(DESTDIR)$(PREFIX)/libexec/ssmcp/
	sed 's|/usr/local/libexec/|$(PREFIX)/libexec/|g' packaging/ssmcp-portal-ssh.service > $(DESTDIR)$(SYSTEMD_DIR)/ssmcp-portal-ssh.service
	chmod 0644 $(DESTDIR)$(SYSTEMD_DIR)/ssmcp-portal-ssh.service
	install -m 0644 packaging/ssmcp-portal-ssh.timer $(DESTDIR)$(SYSTEMD_DIR)/
