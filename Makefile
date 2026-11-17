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
	node --test scripts/test_cua_facade.mjs scripts/test_execution_helpers.mjs
	$(CARGO) test --locked
	$(CARGO) test --locked --lib code_mode::tests -- --include-ignored
	$(CARGO) test --locked --test two_tools -- --include-ignored
	$(CARGO) test --locked --test workspace_bridge -- --ignored --test-threads=1
	$(CARGO) test --locked --test context_helpers -- --ignored --test-threads=1
	$(CARGO) build --locked --bins --example vt_snapshot
	python3 scripts/e2e.py --bin-dir $(CARGO_TARGET_DIR)/debug
	python3 scripts/e2e_session_features.py --bin-dir $(CARGO_TARGET_DIR)/debug
	python3 scripts/e2e_agents.py --bin-dir $(CARGO_TARGET_DIR)/debug
	python3 scripts/e2e_unified.py --bin-dir $(CARGO_TARGET_DIR)/debug --report $(CARGO_TARGET_DIR)/acceptance/unified.json
	python3 scripts/e2e_dashboard.py --bin-dir $(CARGO_TARGET_DIR)/debug --report $(CARGO_TARGET_DIR)/acceptance/dashboard.json

install:
	install -d $(DESTDIR)$(PREFIX)/bin $(DESTDIR)$(PREFIX)/share/man/man1 $(DESTDIR)$(PREFIX)/share/man/man8
	install -m 0755 $(CARGO_TARGET_DIR)/release/ssmcp $(CARGO_TARGET_DIR)/release/ssmcpd $(DESTDIR)$(PREFIX)/bin/
	install -d $(DESTDIR)$(PREFIX)/libexec/ssmcp
	install -m 0644 src/code_mode.js $(DESTDIR)$(PREFIX)/libexec/ssmcp/code-mode.js
	install -m 0644 packaging/man/ssmcp.1 $(DESTDIR)$(PREFIX)/share/man/man1/
	install -m 0644 packaging/man/ssmcpd.8 $(DESTDIR)$(PREFIX)/share/man/man8/
	install -d $(DESTDIR)$(PREFIX)/share/doc/ssmcp/examples $(DESTDIR)$(PREFIX)/share/doc/ssmcp/docs/assets
	install -m 0644 README.md $(DESTDIR)$(PREFIX)/share/doc/ssmcp/
	install -d $(DESTDIR)$(PREFIX)/share/doc/ssmcp/skills/ssmcp
	install -m 0644 skills/ssmcp/SKILL.md $(DESTDIR)$(PREFIX)/share/doc/ssmcp/skills/ssmcp/
	install -m 0644 docs/*.md $(DESTDIR)$(PREFIX)/share/doc/ssmcp/docs/
	install -m 0644 docs/assets/*.svg docs/assets/*.jpg docs/assets/*.png $(DESTDIR)$(PREFIX)/share/doc/ssmcp/docs/assets/
	install -m 0644 packaging/Caddyfile packaging/nginx-ssmcp.conf.example packaging/ssmcp-client.toml.example packaging/ssmcpd.env packaging/ssmcpd.toml.example $(DESTDIR)$(PREFIX)/share/doc/ssmcp/examples/

# Installs the unit and a first-time config; account creation and activation are explicit.
install-server: install
	install -m 0755 scripts/setup_server.py $(DESTDIR)$(PREFIX)/libexec/ssmcp/setup-server
	install -d $(DESTDIR)$(SYSTEMD_DIR) $(DESTDIR)$(SYSCONFDIR)/ssmcpd
	sed 's|/usr/local/bin/|$(PREFIX)/bin/|g; s|/etc/ssmcpd/|$(SYSCONFDIR)/ssmcpd/|g' packaging/ssmcpd.service > $(DESTDIR)$(SYSTEMD_DIR)/ssmcpd.service
	chmod 0644 $(DESTDIR)$(SYSTEMD_DIR)/ssmcpd.service
	test -e $(DESTDIR)$(SYSCONFDIR)/ssmcpd/ssmcpd.env || install -m 0644 packaging/ssmcpd.env $(DESTDIR)$(SYSCONFDIR)/ssmcpd/ssmcpd.env
