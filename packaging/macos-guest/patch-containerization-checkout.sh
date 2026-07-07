#!/usr/bin/env bash
set -euo pipefail

CHECKOUT_ROOT="${1:?usage: patch-containerization-checkout.sh <containerization-checkout-root>}"
TARGET_FILE="$CHECKOUT_ROOT/Sources/Containerization/NATNetworkInterface.swift"

if [[ ! -f "$TARGET_FILE" ]]; then
  echo "error: missing $TARGET_FILE" >&2
  exit 1
fi

chmod u+w "$TARGET_FILE"
python3 - "$TARGET_FILE" <<'PY'
from pathlib import Path
import sys

path = Path(sys.argv[1])
text = path.read_text()
updated = text.replace("#if !CURRENT_SDK", "#if false")
if updated != text:
    path.write_text(updated)
PY
