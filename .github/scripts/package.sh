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
#
# A binary built for another architecture than TARGET (a wrong runner or a
# cross-build) is refused, and so is a TARGET that is not one of the four release
# targets.
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

# The architecture the binary must report: lipo on macOS, file on Linux.
case "$target" in
  aarch64-apple-darwin) want=arm64 ;;
  x86_64-apple-darwin) want=x86_64 ;;
  aarch64-unknown-linux-musl) want="ARM aarch64" ;;
  x86_64-unknown-linux-musl) want=x86-64 ;;
  *) die "not a release target: $target" ;;
esac
if [[ "$target" == *-apple-darwin ]]; then
  have="$(lipo -archs "$bin")" || die "lipo could not read $bin"
  [[ "$have" == "$want" ]] || die "$bin is built for '$have', not '$want' ($target)"
else
  have="$(file -b "$bin")" || die "file could not read $bin"
  [[ "$have" == *", $want,"* ]] || die "$bin is not built for '$want' ($target): $have"
fi

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
