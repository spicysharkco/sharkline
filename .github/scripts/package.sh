#!/usr/bin/env bash
# Pack a release build of sharkline into a release archive.
#
#   .github/scripts/package.sh TARGET VERSION [OUT_DIR]
#
# Run `cargo build --release --locked --target TARGET` first. The archive is
# OUT_DIR/sharkline-VERSION-TARGET.tar.gz (OUT_DIR defaults to target/dist). It
# holds one directory, sharkline-VERSION-TARGET/, with the binary, LICENSE and
# README.md. The binary is already stripped: the release profile in Cargo.toml
# sets strip = true.
set -euo pipefail

die() { echo "package: $*" >&2; exit 1; }

(( $# == 2 || $# == 3 )) || { echo "usage: package.sh TARGET VERSION [OUT_DIR]" >&2; exit 2; }
target="$1"
version="$2"
[[ "$target" =~ ^[a-z0-9_]+(-[a-z0-9_]+)+$ ]] || die "not a target triple: $target"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.+-]+)?$ ]] || die "not a version: $version"

name=sharkline
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
out="${3:-$root/target/dist}"
bin="$root/target/$target/release/$name"
dir="$name-$version-$target"

[[ -f "$bin" && -x "$bin" ]] || die "no binary at $bin; run: cargo build --release --locked --target $target"

mkdir -p "$out"
stage="$(mktemp -d)"
trap 'rm -rf "${stage:?}"' EXIT

mkdir "$stage/$dir"
install -m 755 "$bin" "$stage/$dir/$name"
install -m 644 "$root/LICENSE" "$root/README.md" "$stage/$dir/"

# Entries belong to root:root whoever built the archive. GNU tar and bsdtar
# (macOS) spell this differently; bsdtar is also kept from writing extended
# attributes and AppleDouble files.
if [[ "$(tar --version 2>&1 || true)" == *"GNU tar"* ]]; then
  owner=(--owner=0 --group=0 --numeric-owner)
else
  owner=(--uid 0 --gid 0 --uname root --gname root --no-xattrs)
fi

archive="$out/$dir.tar.gz"
COPYFILE_DISABLE=1 tar "${owner[@]}" -C "$stage" -czf "$archive" "$dir"
echo "$archive"
