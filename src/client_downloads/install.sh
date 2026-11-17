#!/bin/sh
# Served from the account server; no SSH, archive tool or language runtime.
set -eu
umask 077
server=@@ORIGIN@@
fail() { printf 'ssmcp: %s\n' "$*" >&2; exit 1; }
[ "$#" -gt 0 ] || fail 'Generate a temporary command with ssmcp invite on device A first.'
case "$(uname -m)" in
    x86_64|amd64) arch=x86_64 ;;
    aarch64|arm64) arch=aarch64 ;;
    *) fail 'This CPU architecture is not supported.' ;;
esac
case "$(uname -s)" in
    Linux) target=$arch-unknown-linux-gnu ;;
    Darwin) target=$arch-apple-darwin ;;
    *) fail 'Use the PowerShell temporary command on Windows.' ;;
esac
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/ssmcp-join.XXXXXXXX") || fail 'Cannot create a temporary directory.'
cleanup() { rm -rf "$work_dir"; }
trap cleanup 0
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP
binary=$work_dir/ssmcp
url=$server/client/bin/$target
if command -v curl >/dev/null 2>&1; then
    curl --disable --fail --silent --show-error --connect-timeout 15 --max-time 180 \
        --proto '=https' --tlsv1.2 --max-filesize 268435456 \
        "$url" --output "$binary" || fail "Cannot download the client for $target from this server. Ask its administrator to configure client_download_dir."
elif command -v wget >/dev/null 2>&1; then
    wget -q -T 30 --tries=1 --max-redirect=0 -O "$binary" "$url" || fail "Cannot download the client for $target from this server. Ask its administrator to configure client_download_dir."
else
    fail 'A standard HTTPS downloader is required: curl or wget.'
fi
[ -s "$binary" ] || fail 'The server returned an empty client.'
chmod 700 "$binary"
# Restore terminal input when the bootstrap itself was piped into sh.
if ( : < /dev/tty ) 2>/dev/null; then
    "$binary" join "$@" < /dev/tty
else
    "$binary" join "$@"
fi
