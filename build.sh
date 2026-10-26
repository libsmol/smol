#!/usr/bin/env bash
# Usage: ./build.sh [worker], where worker builds only the worker
set -euo pipefail
cd "$(dirname "$0")"

bin="$HOME/.local/bin"
# The worker links this wasi-libc until a wasi-sdk release has the fixes it carries
sysroot="smol-sysroot-34.0-2"
sysroot_sha256="20aab766330e1882683b3cc352e370c76156d6a966d4a0be044e219d0e03231e"

fail() {
    echo "build: $1" >&2
    exit 1
}

if [ "$(cat target/wasi-sysroot/release 2>/dev/null)" != "$sysroot" ]; then
    archive="target/$sysroot.tar.gz"
    mkdir -p target
    curl -fsSL "https://github.com/libsmol/wasi-libc/releases/download/$sysroot/$sysroot.tar.gz" -o "$archive"
    if command -v sha256sum >/dev/null; then
        sum=$(sha256sum "$archive")
    else
        sum=$(shasum -a 256 "$archive")
    fi
    [ "${sum%% *}" = "$sysroot_sha256" ] || fail "$archive: sha256 mismatch"

    # Cargo doesn't track the sysroot's files, so the worker would keep the old one
    rm -rf target/worker target/wasi-sysroot
    mkdir target/wasi-sysroot
    tar -xzf "$archive" -C target/wasi-sysroot --strip-components 1
    echo "$sysroot" >target/wasi-sysroot/release
    rm "${archive:?}"
fi

# Apple's clang has no wasm32 target, unlike the one in Homebrew's LLVM
# grep -q may exit before clang finishes writing, which pipefail turns into a failure
if [ -z "${CC_wasm32_wasip2:-}" ] && ! clang --print-targets 2>/dev/null | grep wasm32 >/dev/null; then
    command -v brew >/dev/null || fail "clang: no wasm32 target (install clang and llvm)"
    llvm="$(brew --prefix llvm)/bin"
    "$llvm/clang" --print-targets 2>/dev/null | grep wasm32 >/dev/null ||
        fail "clang: no wasm32 target (install LLVM with brew install llvm)"
    export CC_wasm32_wasip2="$llvm/clang"
fi

# Cargo and rustup read the worker's config and toolchain from the current directory
(cd crates/worker && cargo build --release)
if [ "${1:-}" = worker ]; then
    exit 0
fi

cargo build --release

mkdir -p "$bin"
for file in target/release/{smol,libsmol}; do
    name=$(basename "$file")

    # A running binary can't be overwritten, only replaced by a new file
    cp "$file" "$bin/.$name.new"
    mv "$bin/.$name.new" "$bin/$name"
    echo "$bin/$name"
done
