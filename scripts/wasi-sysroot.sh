#!/usr/bin/env bash
# Usage: scripts/wasi-sysroot.sh <tag>, which builds the sysroot that build.sh
# downloads from that tag of libsmol/wasi-libc, and prints its sha256
# Needs git, clang, llvm-ar and python3
set -euo pipefail
cd "$(dirname "$0")/.."

release="${1:?usage: scripts/wasi-sysroot.sh <tag>}"
work="$PWD/target/wasi-sysroot-build"

rm -rf "${work:?}"
mkdir -p "$work"
git clone --quiet --depth 1 --branch "$release" https://github.com/libsmol/wasi-libc "$work/wasi-libc"

python3 -m venv "$work/tools"
"$work/tools/bin/pip" install --quiet cmake ninja
PATH="$work/tools/bin:$PATH"

cmake -G Ninja -S "$work/wasi-libc" -B "$work/build" \
    -DTARGET_TRIPLE=wasm32-wasip2 -DCMAKE_C_COMPILER=clang -DCMAKE_AR=llvm-ar \
    -DBUILD_SHARED=OFF -DUSE_WASM_COMPONENT_LD=OFF
ninja -C "$work/build"

mkdir "$work/$release"
cp -R "$work/build/sysroot/include" "$work/build/sysroot/lib" "$work/$release"
tar -C "$work" -czf "$work/$release.tar.gz" "$release"
sha256sum "$work/$release.tar.gz"
