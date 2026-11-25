#!/usr/bin/env bash
set -euo pipefail
module=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$module/../.."
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
export CARGO_PROFILE_RELEASE_LTO="${CARGO_PROFILE_RELEASE_LTO:-false}"
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS="${CARGO_PROFILE_RELEASE_CODEGEN_UNITS:-16}"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
grep -Fxq "version = \"v$version\"" "$module/kam.toml"
cargo ndk -t arm64-v8a -P 26 build --locked --release --bin ssmcp
install -Dm755 "$CARGO_TARGET_DIR/aarch64-linux-android/release/ssmcp" "$module/src/ssmcp/bin/ssmcp"
