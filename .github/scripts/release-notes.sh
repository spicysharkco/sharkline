#!/usr/bin/env bash
# Print the CHANGELOG.md section for one version (the text under its
# "## [VERSION] - DATE" heading, up to the next "## " heading), for use as the
# release notes.
#
#   .github/scripts/release-notes.sh VERSION [CHANGELOG]
#
# Exits 1 when the section is missing or empty.
set -euo pipefail

die() { echo "release-notes: $*" >&2; exit 1; }

(( $# == 1 || $# == 2 )) || { echo "usage: release-notes.sh VERSION [CHANGELOG]" >&2; exit 2; }
version="$1"
file="${2:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/CHANGELOG.md}"
[[ -f "$file" ]] || die "no such file: $file"

# Link reference lines ("[1.0.0]: https://...") that close the file are not notes.
notes="$(awk -v v="$version" '
  /^## / {
    if (on) exit
    on = (index($0, "## [" v "]") == 1 || $0 == "## " v || index($0, "## " v " ") == 1)
    next
  }
  on && /^\[[^]]+\]: / { next }
  on { print }
' "$file")"

[[ -n "${notes//[[:space:]]/}" ]] || die "no section for version $version in $file"
printf '%s\n' "$notes"
