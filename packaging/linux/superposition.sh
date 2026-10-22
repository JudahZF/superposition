#!/bin/sh
# Launches Superposition from an unpacked Linux tarball: helpers, the Wine bridge, and the
# bundled Wine build sit beside the binary.
root=$(dirname "$(dirname "$(readlink -f "$0")")")/lib/superposition
export SUPERPOSITION_HELPERS_DIR="${SUPERPOSITION_HELPERS_DIR:-$root/helpers}"
export SUPERPOSITION_WINE="${SUPERPOSITION_WINE:-$root/wine/bin/wine}"
exec "$root/superposition" "$@"
