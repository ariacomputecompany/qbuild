#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PKG_DIR="$ROOT_DIR/packaging/macos-guest"
WORK_DIR="${WORK_DIR:-$ROOT_DIR/.cache/macos-guest-upstream}"
UPSTREAM_DIR="$WORK_DIR/containerization"
UPSTREAM_REF="${UPSTREAM_REF:-0.1.1}"
BUILD_CONFIGURATION="${BUILD_CONFIGURATION:-release}"
SWIFT_TOOLCHAIN_VERSION="${SWIFT_TOOLCHAIN_VERSION:-6.1.0}"
SWIFTLY_BIN="${SWIFTLY_BIN:-$HOME/.swiftly/bin/swiftly}"
SWIFTLY_ENV="${SWIFTLY_ENV:-$HOME/.swiftly/env.sh}"
SWIFTLY_PKG_URL="${SWIFTLY_PKG_URL:-https://download.swift.org/swiftly/darwin/swiftly.pkg}"
SWIFTLY_PKG_PATH="/var/tmp/$(basename "$SWIFTLY_PKG_URL")"
KERNEL_MODE="${KERNEL_MODE:-fetch}"
KERNEL_SOURCE_URL="${KERNEL_SOURCE_URL:-https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.14.9.tar.xz}"
BOOTSTRAP_KERNEL_MODE="${BOOTSTRAP_KERNEL_MODE:-fetch}"
KERNEL_OUTPUT_PATH="$UPSTREAM_DIR/bin/vmlinux"

die() {
  echo "error: $*" >&2
  exit 1
}

command -v git >/dev/null 2>&1 || die "git is required"
command -v curl >/dev/null 2>&1 || die "curl is required"
command -v installer >/dev/null 2>&1 || die "installer is required"
command -v make >/dev/null 2>&1 || die "make is required"
command -v script >/dev/null 2>&1 || die "script is required"

ensure_swift_toolchain() {
  local project_dir="$1"

  if [[ ! -x "$SWIFTLY_BIN" ]]; then
    curl -L -o "$SWIFTLY_PKG_PATH" "$SWIFTLY_PKG_URL"
    installer -pkg "$SWIFTLY_PKG_PATH" -target CurrentUserHomeDirectory
    rm -f "$SWIFTLY_PKG_PATH"
  fi

  if [[ -f "$SWIFTLY_ENV" ]]; then
    # shellcheck disable=SC1090
    source "$SWIFTLY_ENV"
  fi
  export PATH="$HOME/.swiftly/bin:$PATH"

  if ! "$SWIFTLY_BIN" list | grep -Fq "Swift $SWIFT_TOOLCHAIN_VERSION"; then
    "$SWIFTLY_BIN" install "$SWIFT_TOOLCHAIN_VERSION"
  fi

  (
    cd "$project_dir"
    "$SWIFTLY_BIN" use "$SWIFT_TOOLCHAIN_VERSION" --assume-yes >/dev/null
  )
}

run_with_terminal() {
  if [[ -t 0 && -t 1 ]]; then
    "$@"
    return
  fi

  script -q /dev/null "$@"
}

mkdir -p "$WORK_DIR"

if [[ ! -d "$UPSTREAM_DIR/.git" ]]; then
  git clone --depth 1 --branch "$UPSTREAM_REF" https://github.com/apple/containerization.git "$UPSTREAM_DIR"
else
  git -C "$UPSTREAM_DIR" fetch --depth 1 origin "refs/tags/$UPSTREAM_REF:refs/tags/$UPSTREAM_REF"
  git -C "$UPSTREAM_DIR" checkout -f "$UPSTREAM_REF"
fi

"$PKG_DIR/patch-containerization-checkout.sh" "$UPSTREAM_DIR"
ensure_swift_toolchain "$UPSTREAM_DIR"

if [[ ! -f "$UPSTREAM_DIR/bin/cctl" ]]; then
  make -C "$UPSTREAM_DIR" containerization BUILD_CONFIGURATION="$BUILD_CONFIGURATION"
fi

if [[ ! -f "$UPSTREAM_DIR/vminitd/bin/vminitd" || ! -f "$UPSTREAM_DIR/vminitd/bin/vmexec" ]]; then
  make -C "$UPSTREAM_DIR" cross-prep
  make -C "$UPSTREAM_DIR" vminitd BUILD_CONFIGURATION="$BUILD_CONFIGURATION"
fi

if [[ ! -f "$UPSTREAM_DIR/bin/init.block" ]]; then
  make -C "$UPSTREAM_DIR" init BUILD_CONFIGURATION="$BUILD_CONFIGURATION"
fi

if [[ "$BOOTSTRAP_KERNEL_MODE" == "fetch" && ! -f "$UPSTREAM_DIR/bin/vmlinux" ]]; then
  make -C "$UPSTREAM_DIR" fetch-default-kernel
fi

if [[ "$KERNEL_MODE" == "source" ]]; then
  if [[ ! -f "$UPSTREAM_DIR/bin/vmlinux" ]]; then
    die "bootstrap kernel missing at $UPSTREAM_DIR/bin/vmlinux; run fetch mode first"
  fi
  if [[ ! -f "$UPSTREAM_DIR/kernel/source.tar.xz" ]]; then
    curl -L -o "$UPSTREAM_DIR/kernel/source.tar.xz" "$KERNEL_SOURCE_URL"
  fi
  run_with_terminal make -C "$UPSTREAM_DIR/kernel"
  KERNEL_OUTPUT_PATH="$UPSTREAM_DIR/kernel/vmlinux"
elif [[ ! -f "$UPSTREAM_DIR/bin/vmlinux" ]]; then
  die "kernel missing at $UPSTREAM_DIR/bin/vmlinux; enable BOOTSTRAP_KERNEL_MODE=fetch or provide KERNEL_PATH"
fi

echo "UPSTREAM_DIR=$UPSTREAM_DIR"
echo "KERNEL_PATH=$KERNEL_OUTPUT_PATH"
echo "INIT_BLOCK_PATH=$UPSTREAM_DIR/bin/init.block"
echo "VMINITD_PATH=$UPSTREAM_DIR/vminitd/bin/vminitd"
echo "VMEXEC_PATH=$UPSTREAM_DIR/vminitd/bin/vmexec"
