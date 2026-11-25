#!/bin/sh
# Run in Termux: sh install-ssmcp-termux.sh [bin|source] [latest|vX.Y.Z]
# Install the GNU/Linux CLI in a private Debian userland (64-bit ARM/x86).
set -eu
fail() { printf '%s\n' "$*" >&2; exit 1; }
mode=${1:-bin}
tag=${2:-latest}
case "$mode" in
    bin|source) ;;
    -h|--help) echo 'sh install-ssmcp-termux.sh [bin|source] [latest|vX.Y.Z]'; exit 0 ;;
    *) fail 'Choose bin or source.' ;;
esac
[ "$#" -le 2 ] || fail 'Too many arguments.'
case "$tag" in latest) ;; *[!v0-9.]*) fail 'Expected latest or vX.Y.Z.' ;; esac
[ "$tag" = latest ] || printf '%s\n' "$tag" | grep -Exq 'v[0-9]+\.[0-9]+\.[0-9]+' || fail 'Expected latest or vX.Y.Z.'
case "$(uname -m)" in aarch64|arm64|x86_64|amd64) ;; *) fail 'Use a 64-bit ARM or x86 Termux installation.' ;; esac
command -v pkg >/dev/null && [ -n "${PREFIX:-}" ] || fail 'Run this script in Termux.'
if ! command -v proot-distro >/dev/null; then pkg install -y proot-distro; fi
container=ssmcp-debian
installed=$(proot-distro list --quiet) || fail 'Current proot-distro required; update the Termux package.'
if ! printf '%s\n' "$installed" | grep -Fxq "$container"; then
    proot-distro install debian:bookworm --name "$container"
fi

proot-distro login "$container" -- /bin/sh -s -- "$mode" "$tag" <<'GUEST'
set -eu
mode=$1
tag=$2
case "$(getconf GNU_LIBC_VERSION)" in glibc\ *) ;; *) echo 'Expected a GNU libc guest.' >&2; exit 1 ;; esac
export DEBIAN_FRONTEND=noninteractive
if [ ! -s /etc/ssl/certs/ca-certificates.crt ] || ! command -v curl >/dev/null || [ "$mode" = source ]; then
    apt-get update
    if [ "$mode" = source ]; then
        apt-get install -y --no-install-recommends ca-certificates curl git build-essential pkg-config cmake
    else
        apt-get install -y --no-install-recommends ca-certificates curl
    fi
fi
fetch() { curl -q --proto '=https' --proto-redir '=https' -fsSL --retry 2 --connect-timeout 15 "$@"; }
repo=https://github.com/LIghtJUNction/ssmcp
if [ "$tag" = latest ]; then
    url=$(fetch -o /dev/null -w '%{url_effective}' "$repo/releases/latest")
    case "$url" in "$repo"/releases/tag/*) tag=${url##*/} ;; *) echo 'Unexpected release URL.' >&2; exit 1 ;; esac
fi
printf '%s\n' "$tag" | grep -Exq 'v[0-9]+\.[0-9]+\.[0-9]+' || exit 1
dir=$HOME/.local/bin
mkdir -p "$dir"
[ ! -d "$dir/ssmcp" ] || { echo 'The CLI destination is a directory.' >&2; exit 1; }
tmp=$(mktemp -d)
staged=
cleanup() { rm -rf -- "$tmp"; if [ -n "$staged" ]; then rm -f -- "$staged"; fi; }
trap cleanup 0
trap 'exit 130' INT
trap 'exit 143' TERM
cd "$tmp"
if [ "$mode" = bin ]; then
    base=$repo/releases/download/$tag
    fetch "$base/SHA256SUMS" -o SHA256SUMS
    fetch "$base/install.sh" -o install.sh
    # Verify the installer itself before it verifies the release archive and CLI.
    awk '$2=="install.sh" || $2=="*install.sh" {print $1 "  install.sh"; n++} END {if(n!=1) exit 1}' SHA256SUMS > installer.sha256
    sha256sum -c installer.sha256
    sh install.sh --version "$tag" --dir "$dir"
else
    git init -q source
    cd source
    git remote add origin "$repo.git"
    git fetch --depth 1 origin "refs/tags/$tag"
    git checkout -q --detach FETCH_HEAD
    printf 'Building %s at %s\n' "$tag" "$(git rev-parse HEAD)"
    git submodule update --init --recursive --depth 1
    export PATH="$HOME/.cargo/bin:$PATH"
    if ! command -v rustup >/dev/null; then
        fetch https://sh.rustup.rs -o "$tmp/rustup.sh"
        sh "$tmp/rustup.sh" -y --profile minimal --no-modify-path --default-toolchain none
    fi
    # rustup selects the checkout's pinned rust-toolchain.toml; Cargo verifies Cargo.lock.
    CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2} rustup run --install "$(sed -n 's/^channel = "\([^"]*\)"$/\1/p' rust-toolchain.toml)" cargo build --locked --release --bin ssmcp
    staged=$(mktemp "$dir/.ssmcp.XXXXXXXX")
    install -m 755 target/release/ssmcp "$staged"
    [ "$("$staged" --version)" = "ssmcp ${tag#v}" ] || { echo 'Built CLI reported a different version.' >&2; exit 1; }
    mv -f -- "$staged" "$dir/ssmcp"
    staged=
fi
"$dir/ssmcp" --version
GUEST
printf 'Start the CLI: proot-distro login %s -- /root/.local/bin/ssmcp\n' "$container"
