#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="$(
  awk '/^\[workspace.package\]/{in_workspace_package=1; next} /^\[/{in_workspace_package=0} in_workspace_package && /^version = /{gsub(/"/, "", $3); print $3; exit}' "$ROOT/Cargo.toml"
)"
NPM_DIR="$ROOT/npm/taskr"
ARTIFACTS_DIR="$NPM_DIR/artifacts"

OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH_RAW="$(uname -m)"

case "$ARCH_RAW" in
  x86_64|amd64) ARCH="x86_64" ;;
  arm64|aarch64) ARCH="arm64" ;;
  *) echo "Unsupported architecture: $ARCH_RAW" >&2; exit 1 ;;
esac

case "$OS" in
  linux|darwin) PLATFORM="${OS}-${ARCH}" ;;
  *) echo "Unsupported OS: $OS" >&2; exit 1 ;;
esac

ARCHIVE="taskr-${PLATFORM}.tar.gz"
PACKAGE_DIR="$ROOT/target/npm-package/taskr-${PLATFORM}"

echo "Preparing @mmux/taskr npm package for ${PLATFORM} at version ${VERSION}"

rm -rf "$PACKAGE_DIR"
mkdir -p "$PACKAGE_DIR" "$ARTIFACTS_DIR"

if [ "${TASKR_NPM_SKIP_BUILD:-0}" != "1" ]; then
  cargo build --release --bin taskr
fi

cp "$ROOT/target/release/taskr" "$PACKAGE_DIR/taskr"

tar -czf "$ARTIFACTS_DIR/$ARCHIVE" -C "$PACKAGE_DIR" .

node -e '
const fs = require("fs");
const path = process.argv[1];
const version = process.argv[2];
const pkg = JSON.parse(fs.readFileSync(path, "utf8"));
pkg.version = version;
fs.writeFileSync(path, JSON.stringify(pkg, null, 2) + "\n");
' "$NPM_DIR/package.json" "$VERSION"

echo "Created $ARTIFACTS_DIR/$ARCHIVE"
echo
echo "Next steps:"
echo "  make npm-pack-dry-run"
echo "  make npm-pack"
