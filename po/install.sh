#!/usr/bin/env bash
# Compile and install the translation catalogs.
# Usage: po/install.sh [prefix]   (default prefix: $XDG_DATA_HOME or ~/.local/share)
set -euo pipefail

DOMAIN=aurveto
here="$(cd "$(dirname "$0")" && pwd)"
prefix="${1:-${XDG_DATA_HOME:-$HOME/.local/share}}"

for po in "$here"/*.po; do
  lang="$(basename "$po" .po)"
  dest="$prefix/locale/$lang/LC_MESSAGES"
  mkdir -p "$dest"
  msgfmt "$po" -o "$dest/$DOMAIN.mo"
  echo "installed $lang → $dest/$DOMAIN.mo"
done
