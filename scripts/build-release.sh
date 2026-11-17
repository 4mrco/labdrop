#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="${ROOT_DIR}/dist"

echo "=== LabDrop Release Builder ==="
echo "Workspace: ${ROOT_DIR}"
mkdir -p "${DIST_DIR}"

echo "[1/5] Building Linux x86-64 static executables (x86_64-unknown-linux-musl)..."
cargo build --manifest-path "${ROOT_DIR}/Cargo.toml" --release --target x86_64-unknown-linux-musl

echo "[2/5] Building Windows x86-64 native PE executables (x86_64-pc-windows-gnu)..."
cargo build --manifest-path "${ROOT_DIR}/Cargo.toml" --release --target x86_64-pc-windows-gnu

echo "[3/5] Copying release binaries to ${DIST_DIR}..."
cp -f "${ROOT_DIR}/target/x86_64-unknown-linux-musl/release/labdrop" "${DIST_DIR}/labdrop"
cp -f "${ROOT_DIR}/target/x86_64-unknown-linux-musl/release/labdrop-bootstrapper" "${DIST_DIR}/labdrop-bootstrapper"
cp -f "${ROOT_DIR}/target/x86_64-pc-windows-gnu/release/labdrop.exe" "${DIST_DIR}/LabDrop.exe"
cp -f "${ROOT_DIR}/target/x86_64-pc-windows-gnu/release/labdrop-bootstrapper.exe" "${DIST_DIR}/labdrop-bootstrapper.exe"

chmod +x "${DIST_DIR}/labdrop" "${DIST_DIR}/labdrop-bootstrapper"

echo "[4/5] Packaging Linux .tar.gz archive (preserving +x executable bit) and generating SHA256SUMS.txt..."
(
  cd "${DIST_DIR}"
  tar --mode='0755' -czf labdrop-bootstrapper-linux-x86_64.tar.gz labdrop-bootstrapper
  sha256sum labdrop LabDrop.exe labdrop-bootstrapper labdrop-bootstrapper.exe labdrop-bootstrapper-linux-x86_64.tar.gz > SHA256SUMS.txt
)

echo "[5/5] Verifying release artifacts..."
file "${DIST_DIR}/labdrop" "${DIST_DIR}/LabDrop.exe" "${DIST_DIR}/labdrop-bootstrapper" "${DIST_DIR}/labdrop-bootstrapper.exe"
cat "${DIST_DIR}/SHA256SUMS.txt"
echo "=== Release artifacts ready in ${DIST_DIR} ==="
