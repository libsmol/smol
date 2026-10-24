#!/bin/sh
# Usage: curl -fsSL https://raw.githubusercontent.com/libsmol/smol/main/install.sh | sh
set -eu

repo="libsmol/smol"
version="${SMOL_VERSION:-latest}"
dir="${SMOL_INSTALL_DIR:-$HOME/.local/bin}"

fail() {
    printf 'install: %s\n' "$1" >&2
    exit 1
}

case "$(uname -s)" in
Linux) system=unknown-linux-gnu ;;
Darwin) system=apple-darwin ;;
*) fail "$(uname -s): unsupported system (on Windows use install.ps1)" ;;
esac
case "$(uname -m)" in
x86_64 | amd64) arch=x86_64 ;;
arm64 | aarch64) arch=aarch64 ;;
*) fail "$(uname -m): unsupported architecture" ;;
esac
target="$arch-$system"
asset="smol-$target.tar.gz"

case "$version" in
latest) base="https://github.com/$repo/releases/latest/download" ;;
*) base="https://github.com/$repo/releases/download/$version" ;;
esac

command -v curl >/dev/null || fail "curl: not found"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

printf 'downloading %s\n' "$base/$asset"
curl -fsSL "$base/$asset" -o "$tmp/$asset" || fail "$asset: download failed"
curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" || fail "SHA256SUMS: download failed"

expected=$(awk -v name="$asset" '$2 == name { print $1 }' "$tmp/SHA256SUMS")
[ -n "$expected" ] || fail "$asset: not in SHA256SUMS"
if command -v sha256sum >/dev/null; then
    actual=$(sha256sum "$tmp/$asset" | awk '{ print $1 }')
else
    actual=$(shasum -a 256 "$tmp/$asset" | awk '{ print $1 }')
fi
[ "$actual" = "$expected" ] || fail "$asset: checksum mismatch"

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$dir"
for name in smol libsmol; do
    # A running binary can't be overwritten, only replaced by a new file
    cp "$tmp/smol-$target/$name" "$dir/.$name.new"
    chmod 755 "$dir/.$name.new"
    mv "$dir/.$name.new" "$dir/$name"
    printf '%s\n' "$dir/$name"
done

case ":$PATH:" in
*":$dir:"*) ;;
*) printf '%s is not in PATH; add it to run smol directly\n' "$dir" ;;
esac
