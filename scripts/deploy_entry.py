#!/usr/bin/python3
"""Restricted SSH entry: status or one stable release identity, never shell code."""
import os
import re
import sys

HELPER = "/usr/local/libexec/ssmcp/deploy-release"


def command(original):
    if original == "status":
        return ["/usr/bin/sudo", "-n", HELPER, "--status"]
    match = re.fullmatch(
        r"deploy (v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)) "
        r"([0-9a-f]{40}) ([0-9]{1,20}-[0-9]{1,6})", original)
    if not match:
        raise ValueError("deployment command rejected")
    return ["/usr/bin/sudo", "-n", HELPER, "--tag", match[1], "--commit", match[2], "--run-id", match[3]]


if __name__ == "__main__":
    try:
        args = command(os.environ.get("SSH_ORIGINAL_COMMAND", ""))
        os.execv(args[0], args)
    except ValueError:
        print("deployment command rejected", file=sys.stderr)
        raise SystemExit(2)
