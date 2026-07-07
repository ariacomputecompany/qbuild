#!/usr/bin/env bash
set -euo pipefail

: "${QBUILD_ZIG_TARGET:?QBUILD_ZIG_TARGET must be set}"

exec zig cc -target "$QBUILD_ZIG_TARGET" "$@"
