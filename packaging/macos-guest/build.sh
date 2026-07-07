#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT_DIR="${OUT_DIR:-$ROOT_DIR/packaging/macos-guest/out}"
TARGET="${TARGET:-aarch64-unknown-linux-gnu}"
BASE_IMAGE="${BASE_IMAGE:-docker.io/library/alpine:3.20}"
BASE_IMAGE_PLATFORM="${BASE_IMAGE_PLATFORM:-linux/arm64/v8}"
KERNEL_PATH="${KERNEL_PATH:-}"
VMINITD_PATH="${VMINITD_PATH:-}"
VMEXEC_PATH="${VMEXEC_PATH:-}"
QBUILD_LINUX_BINARY="${QBUILD_LINUX_BINARY:-}"
UPSTREAM_WORK_DIR="${UPSTREAM_WORK_DIR:-$ROOT_DIR/.cache/macos-guest-upstream}"
ZIG_LINKER="$ROOT_DIR/packaging/macos-guest/zig-cc-target.sh"

die() {
  echo "error: $*" >&2
  exit 1
}

zig_target_for_rust_target() {
  case "$1" in
    aarch64-unknown-linux-gnu) echo "aarch64-linux-gnu" ;;
    x86_64-unknown-linux-gnu) echo "x86_64-linux-gnu" ;;
    *)
      die "unsupported Rust target for Zig cross-linking: $1"
      ;;
  esac
}

command -v swift >/dev/null 2>&1 || die "swift is required"
command -v cargo >/dev/null 2>&1 || die "cargo is required"

if [[ -z "$KERNEL_PATH" ]]; then
  PREP_OUTPUT="$("$ROOT_DIR/packaging/macos-guest/prepare-upstream.sh")"
  echo "$PREP_OUTPUT"
  while IFS='=' read -r key value; do
    case "$key" in
      KERNEL_PATH) KERNEL_PATH="$value" ;;
      VMINITD_PATH) VMINITD_PATH="$value" ;;
      VMEXEC_PATH) VMEXEC_PATH="$value" ;;
    esac
  done <<< "$PREP_OUTPUT"
fi

mkdir -p "$OUT_DIR"

if [[ -z "$QBUILD_LINUX_BINARY" ]]; then
  TARGET_ENV_SUFFIX_LOWER="${TARGET//-/_}"
  TARGET_ENV_SUFFIX_LOWER="${TARGET_ENV_SUFFIX_LOWER//./_}"
  TARGET_ENV_SUFFIX_UPPER="$(printf '%s' "$TARGET_ENV_SUFFIX_LOWER" | tr '[:lower:]' '[:upper:]')"
  ZIG_TARGET="$(zig_target_for_rust_target "$TARGET")"
  CC_VAR="CC_${TARGET_ENV_SUFFIX_LOWER}"
  LINKER_VAR="CARGO_TARGET_${TARGET_ENV_SUFFIX_UPPER}_LINKER"
  CFLAGS_VAR="CFLAGS_${TARGET_ENV_SUFFIX_LOWER}"
  CFLAGS_VALUE="${!CFLAGS_VAR:-}"

  [[ -x "$ZIG_LINKER" ]] || chmod +x "$ZIG_LINKER"

  export QBUILD_ZIG_TARGET="$ZIG_TARGET"
  export "${CC_VAR}=${!CC_VAR:-$ZIG_LINKER}"
  export "${LINKER_VAR}=${!LINKER_VAR:-$ZIG_LINKER}"
  export CC_SHELL_ESCAPED_FLAGS=1
  export "${CFLAGS_VAR}=${CFLAGS_VALUE:---target=${ZIG_TARGET}}"
  cargo build --release --target "$TARGET" --manifest-path "$ROOT_DIR/Cargo.toml"
  QBUILD_LINUX_BINARY="$ROOT_DIR/target/$TARGET/release/qbuild"
fi

[[ -f "$QBUILD_LINUX_BINARY" ]] || die "missing Linux qbuild binary at $QBUILD_LINUX_BINARY"
[[ -n "$KERNEL_PATH" && -f "$KERNEL_PATH" ]] || die "missing kernel at ${KERNEL_PATH:-<unset>}"
[[ -n "$VMINITD_PATH" && -f "$VMINITD_PATH" ]] || die "missing vminitd at ${VMINITD_PATH:-<unset>}"
[[ -n "$VMEXEC_PATH" && -f "$VMEXEC_PATH" ]] || die "missing vmexec at ${VMEXEC_PATH:-<unset>}"

if [[ -x "$ROOT_DIR/tools/macos-supervisor/build.sh" ]]; then
  "$ROOT_DIR/tools/macos-supervisor/build.sh"
else
  swift build -c release --package-path "$ROOT_DIR/tools/macos-supervisor"
fi
"$ROOT_DIR/tools/macos-supervisor/.build/release/qbuild-macos-supervisor" \
  build-assets \
  --output-dir "$OUT_DIR" \
  --qbuild-linux-binary "$QBUILD_LINUX_BINARY" \
  --vminitd "$VMINITD_PATH" \
  --vmexec "$VMEXEC_PATH" \
  --kernel "$KERNEL_PATH" \
  --base-image "$BASE_IMAGE" \
  --base-image-platform "$BASE_IMAGE_PLATFORM"

echo "guest asset bundle created at $OUT_DIR"
