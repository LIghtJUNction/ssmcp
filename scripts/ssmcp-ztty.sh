#!/bin/sh
# Optional entry point: ssmcp-ztty.sh [ssmcp arguments...]
# SSMCP_ZTTY selects a ztty executable; SSMCP_LAUNCH_CLIENT selects the client.
# Other graphics-capable terminals can run ssmcp directly.
set -eu

fail() {
    printf 'ssmcp ztty: %s\n' "$*" >&2
    exit 1
}

ssmcp_ztty=${SSMCP_ZTTY-"${HOME:?HOME is required unless SSMCP_ZTTY is set}/.local/bin/ztty"}
[ -f "$ssmcp_ztty" ] && [ -x "$ssmcp_ztty" ] \
    || fail "ztty executable not found: $ssmcp_ztty; set SSMCP_ZTTY to a ztty 0.2.1 or newer executable"

if [ "${SSMCP_LAUNCH_CLIENT+x}" = x ]; then
    ssmcp_client=$SSMCP_LAUNCH_CLIENT
else
    ssmcp_client=$(command -v ssmcp) || fail 'ssmcp was not found on PATH; set SSMCP_LAUNCH_CLIENT to its executable'
fi
[ -f "$ssmcp_client" ] && [ -x "$ssmcp_client" ] \
    || fail "ssmcp executable not found: $ssmcp_client"

# ztty otherwise reuses a per-user broker, which may still be an older version.
# Use a fresh abstract socket even when the caller exported ZTTY_SOCKET.
ssmcp_uid=$(id -u) || fail 'cannot determine the current user ID'
ssmcp_stamp=$(date -u +%Y%m%d%H%M%S) || fail 'cannot determine the launch time'
ZTTY_SOCKET="@ssmcp-ztty-$ssmcp_uid-$$-$ssmcp_stamp"
export ZTTY_SOCKET

ssmcp_version=$("$ssmcp_ztty" --version) || fail "cannot read the ztty version: $ssmcp_ztty"
printf '%s\n' "$ssmcp_version" | awk '
    NR == 1 && $0 ~ /^ztty [0-9]+\.[0-9]+\.[0-9]+$/ {
        split($2, version, ".")
        supported = version[1] > 0 || (version[1] == 0 &&
            (version[2] > 2 || (version[2] == 2 && version[3] >= 1)))
    }
    END { exit !(NR == 1 && supported) }
' || fail "ztty 0.2.1 or newer is required; found: $ssmcp_version"

# Do not change directories or reinterpret client arguments.
exec "$ssmcp_ztty" --name ssmcp --geometry 120x30 -e "$ssmcp_client" "$@"
