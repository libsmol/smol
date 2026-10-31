#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

bin="$HOME/.local/bin"

cargo build --release

mkdir -p "$bin"
for file in target/release/{smol,libsmol}; do
    name=$(basename "$file")

    # A running binary can't be overwritten, only replaced by a new file
    cp "$file" "$bin/.$name.new"
    mv "$bin/.$name.new" "$bin/$name"
    echo "$bin/$name"
done

# libsmol looks for the worker module of its own version here
store="${SMOL_CONFIG_DIR:-$HOME/.smol}/python"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml)
case "$(uname -s)" in
Darwin) suffix=dylib ;;
*) suffix=so ;;
esac
module="$store/worker-$version.$suffix"
mkdir -p "$store"
cp "target/release/lib_smol.$suffix" "$module.new"
mv "$module.new" "$module"
echo "$module"
