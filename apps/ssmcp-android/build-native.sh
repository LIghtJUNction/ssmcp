#!/usr/bin/env bash
# Stage the existing Android CLI as an APK-owned executable. Android extracts
# lib/<abi>/lib*.so to the package's trusted, executable nativeLibraryDir.
set -euo pipefail
app=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$app/../.." && pwd)
cd "$root"
ndk=${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-${ANDROID_NDK:-}}}
: "${ndk:?Set ANDROID_NDK_HOME to the Android NDK directory}"
case $(uname -s) in
    Linux) host=linux-x86_64 ;;
    Darwin) host=darwin-x86_64 ;;
    *) echo "Build the Android CLI on Linux or macOS" >&2; exit 2 ;;
esac
bin=$ndk/toolchains/llvm/prebuilt/$host/bin
: "${CARGO_TARGET_DIR:=$root/target}"
export CARGO_TARGET_DIR
export CARGO_PROFILE_RELEASE_LTO=${CARGO_PROFILE_RELEASE_LTO:-thin}
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=${CARGO_PROFILE_RELEASE_CODEGEN_UNITS:-8}
if (( $# == 0 )); then set -- arm64-v8a x86_64; fi
for abi do
    case $abi in
        arm64-v8a) target=aarch64-linux-android; cargo_target=AARCH64_LINUX_ANDROID ;;
        x86_64) target=x86_64-linux-android; cargo_target=X86_64_LINUX_ANDROID ;;
        *) echo "Unsupported Android ABI: $abi" >&2; exit 2 ;;
    esac
    api=26
    clang=$bin/$target$api-clang
    test -x "$clang"
    cc_target=${target//-/_}
    env "CARGO_TARGET_${cargo_target}_LINKER=$clang" "CC_$cc_target=$clang" \
        "CXX_$cc_target=$clang++" "AR_$cc_target=$bin/llvm-ar" \
        cargo rustc --locked --target "$target" --release --bin ssmcp -- \
        -C link-arg=-Wl,-z,max-page-size=16384
    exe=$CARGO_TARGET_DIR/$target/release/ssmcp
    # An executable PIE has the system interpreter. A shared JNI library alone
    # cannot serve as the argv[0] of ProcessBuilder.
    program_headers=$("$bin/llvm-readelf" --program-headers "$exe")
    [[ $program_headers == *'/system/bin/linker64]'* ]] || {
        echo "Missing Android 64-bit program interpreter in $exe" >&2
        exit 1
    }
    out=$app/app/src/main/jniLibs/$abi
    mkdir -p "$out"
    install -m 755 "$exe" "$out/libssmcp.so"
done
