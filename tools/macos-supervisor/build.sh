#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECKOUT="$ROOT_DIR/.build/checkouts/containerization/Sources/Containerization/NATNetworkInterface.swift"

swift package resolve --package-path "$ROOT_DIR"

if [[ -f "$CHECKOUT" ]]; then
  chmod u+w "$CHECKOUT"
  python3 - "$CHECKOUT" <<'PY'
from pathlib import Path
import sys

path = Path(sys.argv[1])
text = path.read_text()
updated = text.replace("#if !CURRENT_SDK", "#if false")
if updated != text:
    path.write_text(updated)
PY
fi

swift build -c release --package-path "$ROOT_DIR"
