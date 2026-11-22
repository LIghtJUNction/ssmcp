PREFIX ?= /usr/local
DESTDIR ?=
SYSCONFDIR ?= /etc
SYSTEMD_DIR ?= /usr/lib/systemd/system
CARGO ?= cargo
CARGO_TARGET_DIR ?= target
# Packaged-source callers can supply the immutable source SHA explicitly.
SSMCP_SOURCE_REVISION ?= $(shell git rev-parse HEAD)

.PHONY: all build check test test-code-mode install install-server
all: build

build:
	$(CARGO) build --locked --release

check:
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --locked --all-targets -- -D warnings

test-code-mode:
	node --test scripts/test_cua_facade.mjs scripts/test_execution_helpers.mjs scripts/test_rtk_helpers.mjs scripts/test_tools_search.mjs scripts/test_client_capabilities.mjs

test: test-code-mode
	$(CARGO) test --locked
	$(CARGO) test --locked --lib code_mode::tests -- --include-ignored
	$(CARGO) test --locked --test two_tools -- --include-ignored
	$(CARGO) test --locked --test workspace_bridge -- --ignored --test-threads=1
	$(CARGO) test --locked --test context_helpers -- --ignored --test-threads=1
	$(CARGO) build --locked --bins --example vt_snapshot
	@set -eu; ssmcp_fixture="$${SSMCP_E2E_FIXTURE_BINARY:-}"; \
	if test -z "$$ssmcp_fixture"; then \
	  ssmcp_fixture="$$($(CARGO) test --locked --lib --no-run --message-format=json | python3 -c 'import json,sys; print(next(m["executable"] for m in map(json.loads,sys.stdin) if m.get("reason")=="compiler-artifact" and m.get("profile",{}).get("test") and m.get("executable")))')"; \
	fi; \
	SSMCP_E2E_FIXTURE_BINARY="$$ssmcp_fixture" $(MAKE) test-account-acceptance

.PHONY: test-account-acceptance
test-account-acceptance:
	python3 scripts/e2e_account_relay.py --binary $(CARGO_TARGET_DIR)/debug/ssmcp
	python3 scripts/e2e.py --bin-dir $(CARGO_TARGET_DIR)/debug
	python3 scripts/e2e_session_features.py --bin-dir $(CARGO_TARGET_DIR)/debug
	python3 scripts/e2e_agents.py --bin-dir $(CARGO_TARGET_DIR)/debug
	python3 scripts/test_e2e_unified.py
	python3 scripts/e2e_unified.py --bin-dir $(CARGO_TARGET_DIR)/debug --report $(CARGO_TARGET_DIR)/acceptance/unified.json
	python3 scripts/e2e_dashboard.py --bin-dir $(CARGO_TARGET_DIR)/debug --report $(CARGO_TARGET_DIR)/acceptance/dashboard.json
	$(MAKE) test-client-ui

# Linux acceptance: the router fixture uses a private synthetic identity provider.
.PHONY: test-client-ui
test-client-ui:
	$(CARGO) build --locked --bins --example vt_snapshot
	@set -eu; ssmcp_fixture="$${SSMCP_E2E_FIXTURE_BINARY:-}"; \
	if test -z "$$ssmcp_fixture"; then \
	  ssmcp_fixture="$$($(CARGO) test --locked --lib --no-run --message-format=json | python3 -c 'import json,sys; print(next(m["executable"] for m in map(json.loads,sys.stdin) if m.get("reason")=="compiler-artifact" and m.get("profile",{}).get("test") and m.get("executable")))')"; \
	fi; \
	SSMCP_E2E_FIXTURE_BINARY="$$ssmcp_fixture" $(MAKE) test-client-ui-ready

.PHONY: test-client-ui-ready
test-client-ui-ready:
	python3 scripts/e2e_client_ui.py --bin-dir $(CARGO_TARGET_DIR)/debug --out $(CARGO_TARGET_DIR)/acceptance/client-ui
	python3 scripts/e2e_canvas_logs.py --bin-dir $(CARGO_TARGET_DIR)/debug --out $(CARGO_TARGET_DIR)/acceptance/canvas-logs --report $(CARGO_TARGET_DIR)/acceptance/canvas-logs/report.json
	python3 scripts/e2e_composer.py --bin-dir $(CARGO_TARGET_DIR)/debug --report $(CARGO_TARGET_DIR)/acceptance/composer.json
	python3 scripts/e2e_questions.py --bin-dir $(CARGO_TARGET_DIR)/debug --report $(CARGO_TARGET_DIR)/acceptance/questions.json --out $(CARGO_TARGET_DIR)/acceptance/questions
	python3 scripts/e2e_input_history.py --bin-dir $(CARGO_TARGET_DIR)/debug --out $(CARGO_TARGET_DIR)/acceptance/input-history
	python3 scripts/e2e_presentations.py --bin-dir $(CARGO_TARGET_DIR)/debug --out $(CARGO_TARGET_DIR)/acceptance/presentations --report $(CARGO_TARGET_DIR)/acceptance/presentations.json
	python3 scripts/e2e_file_paste.py --bin-dir $(CARGO_TARGET_DIR)/debug --out $(CARGO_TARGET_DIR)/acceptance/file-paste --report $(CARGO_TARGET_DIR)/acceptance/file-paste.json
	python3 scripts/e2e_relay_composer.py --bin-dir $(CARGO_TARGET_DIR)/debug --fixture-binary "$$SSMCP_E2E_FIXTURE_BINARY" --report $(CARGO_TARGET_DIR)/acceptance/relay-composer.json
	@set -eu; mkdir -p "$(CARGO_TARGET_DIR)/acceptance"; \
	ssmcp_interactions_out="$$(mktemp -d "$(CARGO_TARGET_DIR)/acceptance/interactions-XXXXXX")"; \
	python3 scripts/e2e_interactions.py --bin-dir "$(CARGO_TARGET_DIR)/debug" \
	  --decoder "$(CARGO_TARGET_DIR)/debug/examples/vt_snapshot" \
	  --fixture-binary "$$SSMCP_E2E_FIXTURE_BINARY" \
	  --source-revision "$(SSMCP_SOURCE_REVISION)" --out "$$ssmcp_interactions_out"
	@set -eu; mkdir -p "$(CARGO_TARGET_DIR)/acceptance"; \
	ssmcp_worker_windows_out="$$(mktemp -d "$(CARGO_TARGET_DIR)/acceptance/worker-windows-XXXXXX")"; \
	python3 scripts/e2e_worker_windows.py --bin-dir "$(CARGO_TARGET_DIR)/debug" \
	  --decoder "$(CARGO_TARGET_DIR)/debug/examples/vt_snapshot" \
	  --fixture-binary "$$SSMCP_E2E_FIXTURE_BINARY" \
	  --source-revision "$(SSMCP_SOURCE_REVISION)" \
	  --report "$$ssmcp_worker_windows_out/report.json" \
	  --evidence-dir "$$ssmcp_worker_windows_out/evidence"

install:
	install -d $(DESTDIR)$(PREFIX)/bin $(DESTDIR)$(PREFIX)/share/man/man1 $(DESTDIR)$(PREFIX)/share/man/man8
	install -m 0755 $(CARGO_TARGET_DIR)/release/ssmcp $(CARGO_TARGET_DIR)/release/ssmcpd $(DESTDIR)$(PREFIX)/bin/
	install -d $(DESTDIR)$(PREFIX)/libexec/ssmcp
	install -m 0644 src/code_mode.js $(DESTDIR)$(PREFIX)/libexec/ssmcp/code-mode.js
	install -m 0644 packaging/man/ssmcp.1 $(DESTDIR)$(PREFIX)/share/man/man1/
	install -m 0644 packaging/man/ssmcpd.8 $(DESTDIR)$(PREFIX)/share/man/man8/
	install -d $(DESTDIR)$(PREFIX)/share/doc/ssmcp/examples $(DESTDIR)$(PREFIX)/share/doc/ssmcp/docs
	install -m 0644 README.md $(DESTDIR)$(PREFIX)/share/doc/ssmcp/
	install -d $(DESTDIR)$(PREFIX)/share/doc/ssmcp/skills/ssmcp
	install -m 0644 skills/ssmcp/SKILL.md $(DESTDIR)$(PREFIX)/share/doc/ssmcp/skills/ssmcp/
	cp -R docs/. "$(DESTDIR)$(PREFIX)/share/doc/ssmcp/docs/"
	install -m 0644 packaging/Caddyfile packaging/nginx-ssmcp.conf.example packaging/ssmcp-client.toml.example packaging/ssmcpd.env packaging/ssmcpd.toml.example $(DESTDIR)$(PREFIX)/share/doc/ssmcp/examples/

# Installs the unit and a first-time config; account creation and activation are explicit.
install-server: install install-portal
	install -m 0755 scripts/setup_server.py $(DESTDIR)$(PREFIX)/libexec/ssmcp/setup-server
	install -d $(DESTDIR)$(SYSTEMD_DIR) $(DESTDIR)$(SYSCONFDIR)/ssmcpd
	sed 's|/usr/local/bin/|$(PREFIX)/bin/|g; s|/etc/ssmcpd/|$(SYSCONFDIR)/ssmcpd/|g' packaging/ssmcpd.service > $(DESTDIR)$(SYSTEMD_DIR)/ssmcpd.service
	chmod 0644 $(DESTDIR)$(SYSTEMD_DIR)/ssmcpd.service
	test -e $(DESTDIR)$(SYSCONFDIR)/ssmcpd/ssmcpd.env || install -m 0644 packaging/ssmcpd.env $(DESTDIR)$(SYSCONFDIR)/ssmcpd/ssmcpd.env

# Optional split portal service; installation does not activate it.
.PHONY: install-portal
install-portal:
	install -d $(DESTDIR)$(PREFIX)/libexec/ssmcp $(DESTDIR)$(SYSTEMD_DIR) $(DESTDIR)$(PREFIX)/share/doc/ssmcp/examples
	sed 's|/usr/local/bin/|$(PREFIX)/bin/|g; s|/etc/ssmcpd/|$(SYSCONFDIR)/ssmcpd/|g' packaging/ssmcp-portal.service > $(DESTDIR)$(SYSTEMD_DIR)/ssmcp-portal.service
	chmod 0644 $(DESTDIR)$(SYSTEMD_DIR)/ssmcp-portal.service

