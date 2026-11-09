#!/bin/sh
# Install a checksum-verified cargo-deny release into ~/.cargo/bin (CI,
# x86_64 Linux). The Nix shell provides cargo-deny locally.
set -eu

version=0.19.0
sha256=0e8c2aa59128612c90d9e09c02204e912f29a5b8d9a64671b94608cbe09e064f
name="cargo-deny-$version-x86_64-unknown-linux-musl"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
curl -fsSL -o "$work/$name.tar.gz" \
    "https://github.com/EmbarkStudios/cargo-deny/releases/download/$version/$name.tar.gz"
echo "$sha256  $work/$name.tar.gz" | sha256sum -c -
tar -xzf "$work/$name.tar.gz" -C "$work"
mkdir -p "$HOME/.cargo/bin"
install -m 0755 "$work/$name/cargo-deny" "$HOME/.cargo/bin/cargo-deny"
