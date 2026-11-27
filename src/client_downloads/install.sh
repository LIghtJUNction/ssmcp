#!/bin/sh
# Same-origin compatible release; verified user installation, then an explicit relay connection.
set -eu
umask 077
server=@@ORIGIN@@
required_wire=@@WIRE@@
fail() { printf 'ssmcp: %s\n' "$*" >&2; exit 1; }
[ "$#" -gt 0 ] || fail 'Pass the controller account as username@relay-server.'
# Rotate argv once. Values stay literal even when a name contains spaces or shell syntax.
remaining=$#
selected=
while [ "$remaining" -gt 0 ]; do
    argument=$1; shift; remaining=$((remaining - 1))
    case "$argument" in
        --allow|--allow=*) fail '--allow was removed. Sign in, then use /allow add USER in the running session.' ;;
        --name|--title|--server|--username|--reconnect-delay|--ui-config|--theme|--log-format|--cwd|--shell|--input|--tty|--context-root|--cpu-limit|--memory-limit)
            [ "$remaining" -gt 0 ] || fail 'A connection option is missing its value.'
            set -- "$@" "$argument" "$1"; shift; remaining=$((remaining - 1))
            continue ;;
        --)
            set -- "$@" --
            while [ "$remaining" -gt 0 ]; do
                set -- "$@" "$1"; shift; remaining=$((remaining - 1))
            done
            break ;;
        *) ;;
    esac
    case "$argument" in
        *@*)
            case "$argument" in *,*|*@*@*) fail 'Choose one controller account for this device.' ;; esac
            [ -z "$selected" ] || fail 'Choose one controller account for this device.'
            selected=$argument ;;
    esac
    set -- "$@" "$argument"
done
case "$(uname -m)" in
    x86_64|amd64) arch=x86_64 ;;
    aarch64|arm64) arch=aarch64 ;;
    *) fail 'This CPU architecture is not supported.' ;;
esac
case "$(uname -s)" in
    Linux)
        target=$arch-unknown-linux-gnu
        glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null) || fail 'This client requires Linux GNU with glibc 2.35 or newer.'
        case "$glibc" in "glibc "*) glibc=${glibc#glibc } ;; *) fail 'This client requires glibc, not musl.' ;; esac
        major=${glibc%%.*}; minor=${glibc#*.}; minor=${minor%%.*}
        case "$major:$minor" in *[!0-9:]*|:*) fail 'Cannot verify the glibc version.' ;; esac
        [ "$major" -gt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -ge 35 ]; } || fail 'This client requires glibc 2.35 or newer.'
        ;;
    Darwin)
        if command -v sysctl >/dev/null 2>&1 && [ "$(sysctl -in hw.optional.arm64 2>/dev/null || :)" = 1 ]; then arch=aarch64; fi
        target=$arch-apple-darwin
        ;;
    *) fail 'Use the PowerShell installation command on Windows.' ;;
esac
if command -v sha256sum >/dev/null 2>&1; then hash_program=sha256sum
elif command -v shasum >/dev/null 2>&1; then hash_program=shasum
else fail 'A SHA-256 verifier is required: sha256sum or shasum.'
fi
[ -n "${HOME:-}" ] || fail 'HOME is required for a user installation.'
install_dir=${SSMCP_INSTALL_DIR:-"$HOME/.local/bin"}
case "$install_dir" in /*) ;; *) fail 'SSMCP_INSTALL_DIR must be an absolute path.' ;; esac
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/ssmcp-join.XXXXXXXX") || fail 'Cannot create a temporary directory.'
stage=
cleanup() { if [ -n "$stage" ]; then rm -f "$stage"; fi; rm -rf "$work_dir"; }
trap cleanup 0
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP
fetch() {
    output=$1; url=$2; limit=$3
    # A kernel file-size bound also covers chunked responses without Content-Length.
    (
        ulimit -f "$(((limit + 511) / 512))" || exit 1
        if command -v curl >/dev/null 2>&1; then
            curl --disable --fail --silent --show-error --connect-timeout 15 --max-time 180 \
                --location --max-redirs 0 --proto '=https' --tlsv1.2 --max-filesize "$limit" \
                "$url" --output "$output"
        elif command -v wget >/dev/null 2>&1; then
            wget -q -T 30 --tries=1 --max-redirect=0 -O "$output" "$url"
        else
            exit 1
        fi
    ) || fail "Cannot download a compatible client release for $target. Check the gateway's client_download_dir and HTTPS downloader."
    received=$(wc -c < "$output" | tr -d '[:space:]')
    [ "$received" -gt 0 ] && [ "$received" -le "$limit" ] || fail 'The download is empty or exceeds its size limit.'
}
metadata=$work_dir/release
fetch "$metadata" "$server/client/release/$target" 16384
{
    IFS= read -r marker &&
    IFS= read -r version &&
    IFS= read -r manifest_wire &&
    IFS= read -r size &&
    IFS= read -r digest &&
    ! IFS= read -r extra
} < "$metadata" || fail 'The gateway returned an invalid client release manifest.'
[ "$marker" = ssmcp-release-v1 ] || fail 'Unsupported client release manifest.'
[ "$manifest_wire" = "$required_wire" ] || fail 'The published client is incompatible with this gateway.'
case "$version" in ''|*[!0-9A-Za-z.+-]*) fail 'Invalid client release version.' ;; esac
[ "${#version}" -le 64 ] || fail 'Invalid client release version.'
case "$size" in ''|*[!0-9]*) fail 'Invalid client release size.' ;; esac
[ "${#size}" -le 9 ] && [ "$size" -gt 0 ] && [ "$size" -le 268435456 ] || fail 'Invalid client release size.'
case "$digest" in *[!0-9a-f]*) fail 'Invalid client release SHA-256.' ;; esac
[ "${#digest}" -eq 64 ] || fail 'Invalid client release SHA-256.'
binary=$work_dir/ssmcp
fetch "$binary" "$server/client/bin/$target" "$size"
[ "$received" = "$size" ] || fail 'The downloaded client is incomplete or changed during publication.'
if [ "$hash_program" = sha256sum ]; then actual=$(sha256sum "$binary")
else actual=$(shasum -a 256 "$binary")
fi
actual=${actual%% *}
[ "$actual" = "$digest" ] || fail 'Client SHA-256 mismatch; the existing installation was not changed.'
chmod 700 "$binary"
reported=$("$binary" --version) || fail 'The verified client could not report its version.'
[ "$reported" = "ssmcp $version" ] || fail 'Client version mismatch; the existing installation was not changed.'
# Walk before creating: a configured user-bin path must not traverse symlinks.
part=
saved_ifs=$IFS
IFS=/
set -f
for component in ${install_dir#/}; do
    case "$component" in ''|.|..) fail 'The installation path contains an unsafe component.' ;; esac
    part=$part/$component
    [ ! -L "$part" ] || fail 'The installation directory must not contain symlinks.'
    if [ ! -d "$part" ]; then mkdir -m 700 "$part" || fail 'Cannot create the user installation directory.'; fi
    [ ! -L "$part" ] && [ -d "$part" ] || fail 'The installation directory changed.'
done
set +f
IFS=$saved_ifs
installed=$install_dir/ssmcp
[ ! -L "$installed" ] || fail 'Refusing to replace a symlink at the client installation path.'
if [ -e "$installed" ] && [ ! -f "$installed" ]; then fail 'The client installation path is not a regular file.'; fi
stage=$(mktemp "$install_dir/.ssmcp.new.XXXXXXXX") || fail 'Cannot stage the verified client.'
cat "$binary" > "$stage"
chmod 700 "$stage"
mv -f "$stage" "$installed" || fail 'Cannot replace the client; the previous installation remains available.'
stage=
printf 'Installed ssmcp %s at %s\n' "$version" "$installed" >&2
printf 'Starting the controlled connection. Keep this terminal open; Ctrl-C stops it.\n' >&2
# Absolute path: installation does not depend on PATH or modify shell startup files.
if ( : < /dev/tty ) 2>/dev/null; then
    "$installed" --server "$server" "$@" < /dev/tty
else
    "$installed" --server "$server" "$@"
fi
