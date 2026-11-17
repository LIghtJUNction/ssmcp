#!/usr/bin/env python3
"""Exercise browser setup against a real installed Playwright MCP CLI.

No browser is started or approved. A temporary client configuration keeps these
checks independent from the operator's saved relay and browser preferences.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--playwright-mcp", required=True, type=Path)
    options = parser.parse_args()
    binary = options.binary.resolve(strict=True)
    entry = options.playwright_mcp.resolve(strict=True)
    checks: list[str] = []

    with tempfile.TemporaryDirectory(prefix="ssmcp-browser-setup-e2e-") as directory:
        root = Path(directory)
        config = root / "config.toml"
        original = (
            "# Preserve the operator's settings\n"
            "[client]\nserver = 'https://relay.example.invalid'\n"
            "[ui]\ntheme = 'nord'\n"
        )
        config.write_text(original, encoding="utf-8")
        env = os.environ.copy()
        env["SSMCP_CLIENT_CONFIG"] = str(config)
        env.pop("SSMCP_PLAYWRIGHT_MCP", None)

        def run(*arguments: str, fail: bool = False, override: str | None = None) -> dict:
            environment = env.copy()
            if override is not None:
                environment["SSMCP_PLAYWRIGHT_MCP"] = override
            process = subprocess.run(
                [str(binary), "setup", "browser", *arguments],
                env=environment,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=90,
                check=False,
            )
            if fail:
                if process.returncode == 0:
                    raise AssertionError(f"setup unexpectedly accepted {arguments!r}")
                return {}
            if process.returncode != 0:
                raise AssertionError(
                    f"setup failed for {arguments!r}: {process.stderr[-2048:]}"
                )
            result = json.loads(process.stdout)
            assert result["playwright"]["browser_connection"] == "not_attempted"
            return result

        initial = run("--check", "--json")
        assert initial["configuration"]["saved"] is False
        assert config.read_text(encoding="utf-8") == original
        checks.append("check_preserves_configuration")

        configured = run("--playwright-mcp", str(entry), "--json")
        assert configured["configuration"]["saved"] is True
        assert configured["configuration"]["background"] is True
        assert Path(configured["playwright"]["entry"]) == entry
        assert configured["playwright"]["state"] == "cli_verified"
        saved = config.read_bytes()
        assert original.encode() in saved
        checks.append("real_cli_background_support_and_setting_preservation")

        repeated = run("--json")
        assert Path(repeated["playwright"]["entry"]) == entry
        assert repeated["configuration"]["background"] is True
        assert config.read_bytes() == saved
        checks.append("repeated_setup_is_idempotent")

        # Exercise the real client's UI load path without connecting to any
        # relay or discovering installed external harnesses. --list-tools is
        # explicitly local, and an empty PATH excludes optional MCP providers.
        if os.name != "nt":
            isolated_path = root / "empty-bin"
            isolated_path.mkdir()
            startup_env = env.copy()
            startup_env["PATH"] = str(isolated_path)
            startup_env["SHELL"] = "/bin/sh"
            startup_env.pop("SSMCP_UI_CONFIG", None)
            startup_env.pop("SSMCP_PLAYWRIGHT_MCP", None)
            startup = subprocess.run(
                [str(binary), "--list-tools", "--shell", "/bin/sh"],
                env=startup_env,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                timeout=90,
                check=False,
            )
            if startup.returncode != 0:
                raise AssertionError(
                    "client startup rejected setup configuration: "
                    + startup.stderr[-2048:]
                )
            catalog = json.loads(startup.stdout)
            assert catalog["type"] == "shell"
            assert any(
                tool["name"] == "ssmcp.browser.status" for tool in catalog["tools"]
            )
            assert config.read_bytes() == saved
            checks.append("real_client_ui_load_accepts_setup_configuration_offline")

        checked = run("--check", "--json")
        assert checked["configuration"]["saved"] is False
        assert checked["configuration"]["effective_entry"] == str(entry)
        assert config.read_bytes() == saved
        checks.append("configured_check_is_read_only")

        foreground = run("--foreground", "--json")
        assert foreground["configuration"]["background"] is False
        assert run("--json")["configuration"]["background"] is False
        assert run("--background", "--json")["configuration"]["background"] is True
        saved = config.read_bytes()
        checks.append("explicit_foreground_preference_persists")

        for arguments in [
            ("--check", "--install", "--json"),
            ("--check", "--foreground", "--json"),
            ("--playwright-mcp", "relative/cli.js", "--json"),
            ("--playwright-mcp", str(root / "missing.js"), "--json"),
        ]:
            run(*arguments, fail=True)
            assert config.read_bytes() == saved
        checks.append("invalid_requests_never_mutate_configuration")

        unavailable = run("--check", "--json", override=str(root / "missing.js"))
        assert unavailable["playwright"]["state"] == "cli_unavailable"
        assert unavailable["configuration"]["environment_override"] == str(root / "missing.js")
        assert config.read_bytes() == saved
        checks.append("environment_override_is_reported_without_fallback")

        for extension in configured["extensions"]:
            assert extension["paired"] == "not_inspected"
            if extension["family"] != "playwright":
                assert extension["adapter"] == "no_public_adapter_configured"
        assert configured["native_browser"]["uses_system_pointer"] is False
        assert set(configured["isolated_desktop"]["programs"]) == {
            "Xvfb", "xdotool", "magick", "xprop", "dbus-run-session", "dbus-daemon"
        }
        checks.append("diagnostics_preserve_native_approval_boundaries")

    print(json.dumps({"passed": len(checks), "checks": checks}, indent=2))


if __name__ == "__main__":
    main()
