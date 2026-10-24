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
