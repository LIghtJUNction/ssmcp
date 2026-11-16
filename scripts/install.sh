#!/bin/sh
# Install the native client from a GitHub Release. No root access is needed.
set -eu

fail() {
    printf 'ssmcp install: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<'USAGE'
Usage: sh install.sh [--version vMAJOR.MINOR.PATCH] [--dir DIRECTORY]

Installs the latest stable ssmcp client to ~/.local/bin by default.
Linux x86_64/ARM64 requires glibc 2.35 or newer; macOS supports Intel/Apple Silicon.
SSMCP_VERSION and SSMCP_INSTALL_DIR provide the same defaults as the options.
Downloads are verified against the release's SHA256SUMS before installation.
USAGE
}

release_version=${SSMCP_VERSION:-latest}
install_dir=${SSMCP_INSTALL_DIR:-"$HOME/.local/bin"}
while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || fail '--version requires a value'
            release_version=$2
            shift 2
            ;;
        --dir)
            [ "$#" -ge 2 ] || fail '--dir requires a value'
            install_dir=$2
            shift 2
            ;;
        -h|--help) usage; exit 0 ;;
        *) fail "unknown option: $1" ;;
    esac
done
[ -n "$install_dir" ] || fail 'installation directory cannot be empty'

if [ "$release_version" != latest ]; then
    release_version=${release_version#v}
    printf '%s\n' "$release_version" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' \
        || fail 'version must be vMAJOR.MINOR.PATCH'
    release_version=v$release_version
fi

os=$(uname -s)
arch=$(uname -m)
case "$arch" in
    x86_64|amd64) arch=x86_64 ;;
    arm64|aarch64) arch=aarch64 ;;
    *) fail "unsupported CPU architecture: $arch" ;;
esac
case "$os" in
    Linux) target=$arch-unknown-linux-gnu ;;
    Darwin)
        # Prefer the native binary when this shell is running under Rosetta.
        if [ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = 1 ]; then
            arch=aarch64
        fi
        target=$arch-apple-darwin
        ;;
    *) fail "unsupported OS: $os; Windows uses install.ps1" ;;
esac

for program in curl tar mktemp; do
    command -v "$program" >/dev/null 2>&1 || fail "required command not found: $program"
done
if command -v sha256sum >/dev/null 2>&1; then
    hash_command=sha256sum
elif command -v shasum >/dev/null 2>&1; then
    hash_command=shasum
else
    fail 'sha256sum or shasum is required to verify the download'
fi

release_url=https://github.com/LIghtJUNction/ssmcp/releases
if [ "$release_version" = latest ]; then
    release_url=$release_url/latest/download
else
    release_url=$release_url/download/$release_version
fi
archive=ssmcp-$target.tar.gz
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/ssmcp-install.XXXXXXXX")
pending=
cleanup() {
    rm -rf "$work_dir"
    if [ -n "$pending" ]; then rm -f "$pending"; fi
}
trap cleanup 0
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

# Metadata is fetched only from the official release. Public binary mirrors
# never supply the trust root, and curl's user config cannot add credentials.
download_url() {
    # Older curl only checks --max-filesize when the server declares a size.
    # POSIX /bin/sh uses 512-byte ulimit blocks; the kernel also bounds unknown
    # or chunked transfers. A fresh shell keeps each limit out of later phases,
    # even when the installer itself was invoked using non-POSIX bash.
    /bin/sh -c '
        case "$3" in ""|*[!0-9]*) exit 1 ;; esac
        [ "$3" -gt 0 ] && [ "$(($3 % 512))" -eq 0 ] || exit 1
        ulimit -c 0 || exit 1
        ulimit -f "$(($3 / 512))" || {
            printf "%s\n" "ssmcp install: cannot enforce the download file-size limit" >&2
            exit 1
        }
        exec curl --disable --fail --silent --show-error --location \
            --connect-timeout 8 --speed-time 12 --speed-limit 65536 \
            --max-time 180 --max-filesize "$3" \
            --proto "=https" --proto-redir "=https" --tlsv1.2 \
            "$1" --output "$2"
    ' ssmcp-download "$1" "$2" "$3"
}
digest_file() {
    if [ "$hash_command" = sha256sum ]; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}
printf 'Downloading trusted checksums for %s…\n' "$target"
download_url "$release_url/SHA256SUMS" "$work_dir/SHA256SUMS" 262144 \
    || fail 'could not fetch trusted checksums from the official release'
expected=$(awk -v name="$archive" '$2 == name { print $1; count++ } END { if (count != 1) exit 1 }' "$work_dir/SHA256SUMS") \
    || fail 'release checksum entry is missing or duplicated'
printf '%s\n' "$expected" | grep -Eq '^[0-9a-f]{64}$' || fail 'invalid release checksum'
verified=false
for download_source in \
    "$release_url/$archive" \
    "https://ghfast.top/$release_url/$archive" \
    "https://gh-proxy.com/$release_url/$archive"; do
    rm -f "$work_dir/$archive"
    printf 'Downloading public archive: %s\n' "$download_source"
    if download_url "$download_source" "$work_dir/$archive" 134217728; then
        actual=$(digest_file "$work_dir/$archive")
        if [ "$actual" = "$expected" ]; then
            verified=true
            break
        fi
        printf 'ssmcp install: checksum mismatch; rejecting this source\n' >&2
    else
        printf 'ssmcp install: download failed, stalled or stayed too slow; trying the next source\n' >&2
    fi
done
[ "$verified" = true ] || fail 'all public download routes failed verification; existing installation was preserved'

tar -xzf "$work_dir/$archive" -C "$work_dir" ssmcp
binary=$work_dir/ssmcp
if [ ! -f "$binary" ] || [ -L "$binary" ]; then
    fail 'release archive does not contain a regular ssmcp executable'
fi
chmod 755 "$binary"
reported=$("$binary" --version) || fail 'downloaded executable cannot run on this system'
printf '%s\n' "$reported" | grep -Eq '^ssmcp [0-9]+\.[0-9]+\.[0-9]+$' || fail 'unexpected executable version output'
if [ "$release_version" != latest ]; then
    [ "$reported" = "ssmcp ${release_version#v}" ] || fail 'executable version differs from the requested release'
fi

mkdir -p "$install_dir"
[ ! -d "$install_dir/ssmcp" ] || fail 'installation path is a directory'
pending=$(mktemp "$install_dir/.ssmcp.XXXXXXXX")
cat "$binary" > "$pending"
chmod 755 "$pending"
mv -f "$pending" "$install_dir/ssmcp"
pending=
printf 'Installed %s → %s/ssmcp\n' "$reported" "$install_dir"
case ":$PATH:" in
    *":$install_dir:"*) ;;
    *) printf 'Add this directory to PATH: %s\n' "$install_dir" ;;
esac
printf 'Connect from your project directory: ssmcp user@host\n'
