#!/bin/sh
# Install a native Android CLI: sh install-ssmcp-termux.sh [bin|source] [latest|vX.Y.Z]
set -eu
fail() { printf '%s\n' "$*" >&2; exit 1; }
valid_tag() {
    case "$1" in ''|*[!v0-9.]*) return 1 ;; esac
    printf '%s\n' "$1" | grep -Exq 'v[0-9]+\.[0-9]+\.[0-9]+'
}
fetch() { curl -q --proto '=https' --proto-redir '=https' -fsSL --retry 2 --connect-timeout 15 "$@"; }
mode=${1:-bin}
tag=${2:-latest}
check=false
[ "$#" -le 2 ] || fail 'Too many arguments.'
case "$mode" in
    -h|--help) printf '%s\n' 'sh install-ssmcp-termux.sh [bin|source] [latest|vX.Y.Z]' 'sh install-ssmcp-termux.sh update [--check]'; exit 0 ;;
    update)
        case "${2:-}" in --check) check=true ;; '') ;; *) fail 'Expected update [--check].' ;; esac
        tag=latest ;;
    bin|source) [ "$tag" = latest ] || valid_tag "$tag" || fail 'Expected latest or vX.Y.Z.' ;;
    *) fail 'Choose bin, source or update.' ;;
esac
[ -n "${PREFIX:-}" ] && command -v pkg >/dev/null || fail 'Run this script in Termux.'
case "$(uname -m)" in
    aarch64|arm64) asset=ssmcp-magisk-arm64.zip; entry=bin/ssmcp ;;
    x86_64|amd64) asset=ssmcp-android-debug.apk; entry=lib/x86_64/libssmcp.so ;;
    *) fail 'Use a 64-bit ARM or x86 Termux installation.' ;;
esac
dir=$PREFIX/bin
dest=$dir/ssmcp
[ ! -d "$dest" ] && [ ! -L "$dest" ] || fail "The CLI destination must be a regular file: $dest"
command -v curl >/dev/null || pkg install -y curl
repo=https://github.com/LIghtJUNction/ssmcp
if [ "$tag" = latest ]; then
    url=$(fetch -o /dev/null -w '%{url_effective}' "$repo/releases/latest")
    case "$url" in "$repo"/releases/tag/*) tag=${url##*/} ;; *) fail 'Unexpected release URL.' ;; esac
    valid_tag "$tag" || fail 'Unexpected release tag.'
fi
if [ "$mode" = update ]; then
    [ -x "$dest" ] || fail "Install SSMCP first: $dest"
    installed=$("$dest" --version)
    current=v${installed#ssmcp }
    valid_tag "$current" || fail 'The installed CLI reported an invalid version.'
    newest=$(printf '%s\n' "$current" "$tag" | sort -V | tail -n 1)
    if [ "$newest" = "$current" ]; then printf 'Installed %s; latest release %s.\n' "$current" "$tag"; exit 0; fi
    if [ "$check" = true ]; then printf 'Update available: %s → %s.\n' "$current" "$tag"; exit 0; fi
    mode=bin
fi
mkdir -p "$dir"
tmp=$(mktemp -d)
staged=
cleanup() { rm -rf -- "$tmp"; if [ -n "$staged" ]; then rm -f -- "$staged"; fi; }
trap cleanup 0
trap 'exit 130' INT
trap 'exit 143' TERM
cd "$tmp"
staged=$(mktemp "$dir/.ssmcp.XXXXXXXX")
if [ "$mode" = bin ]; then
    command -v unzip >/dev/null || pkg install -y unzip
    base=$repo/releases/download/$tag
    fetch "$base/SHA256SUMS" -o SHA256SUMS
    awk -v a="$asset" '$2==a || $2=="*"a {print $1 "  " a; n++} END {if(n!=1) exit 1}' SHA256SUMS > client.sha256 ||
        fail "$tag has no unique checksum for $asset. Choose a release that includes the native Android CLI, or install in source mode."
    fetch "$base/$asset" -o "$asset"
    sha256sum -c client.sha256
    unzip -p "$asset" "$entry" > "$staged"
else
    pkg install -y git rust clang cmake make pkg-config
    git init -q source
    cd source
    git remote add origin "$repo.git"
    git fetch --depth 1 origin "refs/tags/$tag"
    git checkout -q --detach FETCH_HEAD
    git submodule update --init --recursive --depth 1
    export CARGO_TARGET_DIR=$tmp/target
    CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2} cargo build --locked --release --bin ssmcp
    cp "$CARGO_TARGET_DIR/release/ssmcp" "$staged"
fi
chmod 755 "$staged"
[ "$("$staged" --version)" = "ssmcp ${tag#v}" ] || fail 'The installed CLI reported a different version.'
mv -f -- "$staged" "$dest"
staged=
printf 'Installed %s at %s\n' "$tag" "$dest"
printf '%s\n' 'TUI: ssmcp username@server' 'WebUI: ssmcp username@server --webui'
