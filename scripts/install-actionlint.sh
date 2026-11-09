#!/bin/sh
# Install a checksum-verified actionlint release into the current directory
# (CI, x86_64 Linux). The Nix shell provides actionlint locally.
set -eu

version=1.7.11
sha256=900919a84f2229bac68ca9cd4103ea297abc35e9689ebb842c6e34a3d1b01b0a
archive="actionlint_${version}_linux_amd64.tar.gz"

curl -fsSL -o "$archive" \
    "https://github.com/rhysd/actionlint/releases/download/v$version/$archive"
echo "$sha256  $archive" | sha256sum -c -
tar -xzf "$archive" actionlint
rm "$archive"
