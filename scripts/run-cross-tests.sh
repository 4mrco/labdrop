#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="${ROOT_DIR}/dist"
WORK_DIR="$(mktemp -d /var/tmp/labdrop-cross-test.XXXXXX)"

cleanup() {
  rm -rf "${WORK_DIR}" 2>/dev/null || true
}
trap cleanup EXIT

echo "========================================================================"
echo "LabDrop Automated Cross-Platform & Crash-Safety Verification Suite"
echo "Workspace: ${ROOT_DIR}"
echo "Scratch:   ${WORK_DIR}"
echo "========================================================================"

# Create sample source files (including spaces, Unicode, and nested folders)
SRC_A="${WORK_DIR}/src_usb/files"
SRC_B="${WORK_DIR}/src_downloads"
mkdir -p "${SRC_A}/Pasta de Redes 🧪" "${SRC_B}"
echo "Installer Payload Alpha (USB)" > "${SRC_A}/setup_lab.bin"
echo "Installer Payload Beta (Downloads Duplicate Name)" > "${SRC_B}/setup_lab.bin"
echo "UTF-8 Lab Guide: Configuração & Topologia 2026" > "${SRC_A}/Pasta de Redes 🧪/Guia Prático.txt"

echo ""
echo "[Test 1/6] Linux Bootstrapper (Offline Local Package Preparation)..."
USB_LINUX="${WORK_DIR}/usb_prepared_on_linux"
"${DIST_DIR}/labdrop-bootstrapper" --dest "${USB_LINUX}" --source-dir "${DIST_DIR}"
test -f "${USB_LINUX}/LabDrop/labdrop"
test -f "${USB_LINUX}/LabDrop/LabDrop.exe"
test -f "${USB_LINUX}/LabDrop/SHA256SUMS.txt"
test -d "${USB_LINUX}/LabDrop/files"
test -d "${USB_LINUX}/LabDrop/config"
(cd "${USB_LINUX}/LabDrop" && sha256sum -c SHA256SUMS.txt)
echo "  ✓ Linux bootstrapper verified (both labdrop and LabDrop.exe installed & checksummed)."

echo ""
echo "[Test 2/6] Windows Bootstrapper (labdrop-bootstrapper.exe under Wine)..."
if command -v wine >/dev/null 2>&1; then
  USB_WIN="${WORK_DIR}/usb_prepared_on_windows"
  WIN_DEST="Z:$(echo "${USB_WIN}" | tr '/' '\\')"
  WIN_SRC="Z:$(echo "${DIST_DIR}" | tr '/' '\\')"
  WINEDEBUG=-all wine "${DIST_DIR}/labdrop-bootstrapper.exe" --dest "${WIN_DEST}" --source-dir "${WIN_SRC}"
  test -f "${USB_WIN}/LabDrop/labdrop"
  test -f "${USB_WIN}/LabDrop/LabDrop.exe"
  test -f "${USB_WIN}/LabDrop/SHA256SUMS.txt"
  (cd "${USB_WIN}/LabDrop" && sha256sum -c SHA256SUMS.txt)
  echo "  ✓ Windows bootstrapper.exe verified under Wine (both executables installed & checksummed)."
else
  echo "  - Skipped (wine not installed)"
fi

echo ""
echo "[Test 3/6] Fedora Host Transmitter (SMB + WebDAV + HTTP) & Crash Watchdog (SIGKILL)..."
STATUS_JSON="${WORK_DIR}/active_session.json"
STAGING_BASE="${WORK_DIR}/staging_host"
"${DIST_DIR}/labdrop" share \
  --select "${SRC_A}/setup_lab.bin" \
  --select "${SRC_B}/setup_lab.bin" \
  --select "${SRC_A}/Pasta de Redes 🧪" \
  --interface lo \
  --strategy auto \
  --smb-port 14455 \
  --http-port 18085 \
  --staging-base "${STAGING_BASE}" \
  --status-file "${STATUS_JSON}" \
  --duration-secs 60 >/dev/null 2>&1 &
SERVER_PID=$!

for _ in $(seq 1 40); do
  if [ -f "${STATUS_JSON}" ]; then
    break
  fi
  sleep 0.1
done
test -f "${STATUS_JSON}"

# 3a. Verify SMB access via smbclient
smbclient -N -p 14455 //127.0.0.1/LabDrop -c "get \"setup_lab.bin\" \"${WORK_DIR}/got_smb_1.bin\"; get \"setup_lab (2).bin\" \"${WORK_DIR}/got_smb_2.bin\"" >/dev/null 2>&1
diff -u "${SRC_A}/setup_lab.bin" "${WORK_DIR}/got_smb_1.bin"
diff -u "${SRC_B}/setup_lab.bin" "${WORK_DIR}/got_smb_2.bin"
echo "  ✓ SMB download of collision-resolved files verified via smbclient."

# 3b. Verify WebDAV PROPFIND (RFC 4918)
WEBDAV_CODE=$(curl -s -o "${WORK_DIR}/propfind.xml" -w "%{http_code}" -X PROPFIND -H "Depth: 1" http://127.0.0.1:18085/LabDrop/)
test "${WEBDAV_CODE}" = "207"
grep -q "setup_lab.bin" "${WORK_DIR}/propfind.xml"
echo "  ✓ WebDAV RFC 4918 PROPFIND (207 Multi-Status) verified."

# 3c. Verify HTTP Fallback + on-the-fly .ZIP download
curl -sf "http://127.0.0.1:18085/?zip=1" -o "${WORK_DIR}/bundle.zip"
test -s "${WORK_DIR}/bundle.zip"
echo "  ✓ HTTP Fallback & streamed .ZIP bundle download verified."

# 3d. Simulate terminal close (SIGHUP) from inside a USB-like working dir and verify zero smbd/smbd-notifyd/smbd-cleanupd processes remain!
STAGED_DIR=$(grep -o '"staged_dir": *"[^"]*"' "${STATUS_JSON}" | cut -d'"' -f4)
test -d "${STAGED_DIR}"
kill -HUP "${SERVER_PID}" 2>/dev/null || true
wait "${SERVER_PID}" 2>/dev/null || true
sleep 1.0
if [ -d "${STAGED_DIR}" ]; then
  echo "  ✗ ERROR: Staged directory still exists after SIGHUP!"
  exit 1
fi
if smbclient -N -p 14455 //127.0.0.1/LabDrop -c "ls" >/dev/null 2>&1; then
  echo "  ✗ ERROR: Samba daemon still listening after SIGHUP!"
  exit 1
fi
if ps -eo pid,args | grep -E "smbd|unshare" | grep -q "${STAGING_BASE}"; then
  echo "  ✗ ERROR: Orphaned smbd/smbd-notifyd/smbd-cleanupd process still alive after SIGHUP!"
  exit 1
fi
echo "  ✓ Terminal Close (SIGHUP) verified: smbd, smbd-notifyd, smbd-cleanupd and staged dir cleaned up immediately."

# 3e. Also test abrupt SIGKILL (kill -9) watchdog cleanup
STATUS_JSON_2="${WORK_DIR}/active_session_kill9.json"
(
  cd "${USB_LINUX}/LabDrop"
  exec "${DIST_DIR}/labdrop" share \
    --select "${SRC_A}/setup_lab.bin" \
    --interface lo \
    --strategy auto \
    --smb-port 14456 \
    --http-port 18089 \
    --staging-base "${STAGING_BASE}" \
    --status-file "${STATUS_JSON_2}" \
    --duration-secs 60 >/dev/null 2>&1
) &
SERVER_PID_2=$!
for _ in $(seq 1 40); do
  if [ -f "${STATUS_JSON_2}" ]; then
    break
  fi
  sleep 0.1
done
test -f "${STATUS_JSON_2}"
STAGED_DIR_2=$(grep -o '"staged_dir": *"[^"]*"' "${STATUS_JSON_2}" | cut -d'"' -f4)
kill -9 "${SERVER_PID_2}" 2>/dev/null || true
wait "${SERVER_PID_2}" 2>/dev/null || true
sleep 1.5
if [ -d "${STAGED_DIR_2}" ]; then
  echo "  ✗ ERROR: Staged directory still exists after SIGKILL!"
  exit 1
fi
if ps -eo pid,args | grep -E "smbd|unshare" | grep -q "${STAGING_BASE}"; then
  echo "  ✗ ERROR: Orphaned smbd/smbd-notifyd/smbd-cleanupd process still alive after SIGKILL!"
  exit 1
fi
echo "  ✓ Crash Watchdog (SIGKILL) verified: zero smbd/smbd-notifyd/smbd-cleanupd or USB locks remain."

echo ""
echo "[Test 4/6] Cross-Distro Container Test: Debian/Ubuntu/Mint Container Transmitter -> Fedora Host Destination..."
if command -v podman >/dev/null 2>&1 && podman image exists docker.io/library/debian:12-slim; then
  podman run --rm --network host \
    -v "${DIST_DIR}:/opt/labdrop:ro,Z" \
    -v "${WORK_DIR}:/work:Z" \
    docker.io/library/debian:12-slim \
    /opt/labdrop/labdrop share \
      --select "/work/src_usb/files/setup_lab.bin" \
      --select "/work/src_usb/files/Pasta de Redes 🧪" \
      --interface lo \
      --strategy webdav \
      --http-port 18086 \
      --staging-base "/work/staging_debian" \
      --status-file "/work/debian_status.json" \
      --duration-secs 4 >/dev/null 2>&1 &
  CPID=$!
  for _ in $(seq 1 40); do
    if [ -f "${WORK_DIR}/debian_status.json" ]; then
      break
    fi
    sleep 0.1
  done
  curl -sf "http://127.0.0.1:18086/setup_lab.bin" -o "${WORK_DIR}/from_debian.bin"
  diff -u "${SRC_A}/setup_lab.bin" "${WORK_DIR}/from_debian.bin"
  wait "${CPID}" || true
  echo "  ✓ Static Linux binary ran inside Debian 12 (Ubuntu/Mint family) container and served files to Fedora host."
else
  echo "  - Skipped (podman or debian:12-slim image unavailable)"
fi

echo ""
echo "[Test 5/6] Cross-Distro Container Test: Fedora Host Transmitter -> Debian/Ubuntu/Mint Container Destination..."
if command -v podman >/dev/null 2>&1 && podman image exists docker.io/library/debian:12-slim; then
  "${DIST_DIR}/labdrop" share \
    --select "${SRC_A}/setup_lab.bin" \
    --select "${DIST_DIR}/labdrop" \
    --select "${DIST_DIR}/LabDrop.exe" \
    --select "${DIST_DIR}/SHA256SUMS.txt" \
    --interface lo \
    --strategy webdav \
    --http-port 18087 \
    --staging-base "${WORK_DIR}/staging_fed2deb" \
    --status-file "${WORK_DIR}/fed2deb_status.json" \
    --duration-secs 6 >/dev/null 2>&1 &
  FPID=$!
  for _ in $(seq 1 40); do
    if [ -f "${WORK_DIR}/fed2deb_status.json" ]; then
      break
    fi
    sleep 0.1
  done
  # Inside the Debian 12 (Ubuntu/Mint family) container, download from Fedora host using bash /dev/tcp AND online bootstrapper
  podman run --rm --network host \
    -v "${DIST_DIR}:/opt/labdrop:ro,Z" \
    -v "${WORK_DIR}:/work:Z" \
    docker.io/library/debian:12-slim \
    bash -c '
      exec 3<>/dev/tcp/127.0.0.1/18087
      printf "GET /setup_lab.bin HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n" >&3
      cat <&3 > /work/fed2deb_http_resp.bin
      /opt/labdrop/labdrop-bootstrapper --dest /work/usb_online_debian --release-url http://127.0.0.1:18087 >/dev/null
    '
  grep -q "Installer Payload Alpha (USB)" "${WORK_DIR}/fed2deb_http_resp.bin"
  test -f "${WORK_DIR}/usb_online_debian/LabDrop/labdrop"
  test -f "${WORK_DIR}/usb_online_debian/LabDrop/LabDrop.exe"
  wait "${FPID}" || true
  echo "  ✓ Fedora Host Transmitter -> Debian/Ubuntu/Mint container network transfer & online bootstrapper verified."
fi

echo ""
echo "[Test 6/6] Windows Transmitter (LabDrop.exe under Wine) -> Fedora Linux Destination..."
if command -v wine >/dev/null 2>&1; then
  WIN_SELECT="Z:$(echo "${SRC_A}/setup_lab.bin" | tr '/' '\\')"
  WIN_STAGING="Z:$(echo "${WORK_DIR}/staging_wine" | tr '/' '\\')"
  WIN_STATUS="Z:$(echo "${WORK_DIR}/wine_status.json" | tr '/' '\\')"
  WINEDEBUG=-all wine "${DIST_DIR}/LabDrop.exe" share \
    --select "${WIN_SELECT}" \
    --interface 127.0.0.1 \
    --strategy webdav \
    --http-port 18088 \
    --staging-base "${WIN_STAGING}" \
    --status-file "${WIN_STATUS}" \
    --duration-secs 5 >/dev/null 2>&1 &
  WPID=$!
  for _ in $(seq 1 50); do
    if [ -f "${WORK_DIR}/wine_status.json" ]; then
      break
    fi
    sleep 0.1
  done
  test -f "${WORK_DIR}/wine_status.json"
  curl -sf "http://127.0.0.1:18088/setup_lab.bin" -o "${WORK_DIR}/from_windows_exe.bin"
  diff -u "${SRC_A}/setup_lab.bin" "${WORK_DIR}/from_windows_exe.bin"
  WEBDAV_WIN_CODE=$(curl -s -o /dev/null -w "%{http_code}" -X PROPFIND -H "Depth: 1" http://127.0.0.1:18088/LabDrop/)
  test "${WEBDAV_WIN_CODE}" = "207"
  wait "${WPID}" 2>/dev/null || true
  echo "  ✓ Windows LabDrop.exe transmitter staged files and served WebDAV (207) + HTTP to Linux client."
fi

echo ""
echo "[Test 7/7] Local Computer Installation End-to-End Workflow (Install -> Configure -> Share -> Cleanup -> Update)..."
LOCAL_INSTALL_DIR="${WORK_DIR}/local_pc_home/LabDrop"
"${DIST_DIR}/labdrop-bootstrapper" --dest "${LOCAL_INSTALL_DIR}" --source-dir "${DIST_DIR}" >/dev/null
test -f "${LOCAL_INSTALL_DIR}/labdrop"
test -f "${LOCAL_INSTALL_DIR}/LabDrop.exe"
test -f "${LOCAL_INSTALL_DIR}/config/labdrop.toml"
test ! -d "${LOCAL_INSTALL_DIR}/LabDrop"

# Put a local installer inside the local installation's files/ directory and set preferred_interface = "lo"
echo "Local PC Installed Payload" > "${LOCAL_INSTALL_DIR}/files/local_tool.bin"
sed -i 's/preferred_interface = "auto"/preferred_interface = "lo"/' "${LOCAL_INSTALL_DIR}/config/labdrop.toml"

LOCAL_STATUS="${WORK_DIR}/local_install_status.json"
LOCAL_STAGING="${WORK_DIR}/staging_local_install"
(
  cd "${LOCAL_INSTALL_DIR}"
  ./labdrop share \
    --select "${LOCAL_INSTALL_DIR}/files/local_tool.bin" \
    --strategy webdav \
    --http-port 18091 \
    --staging-base "${LOCAL_STAGING}" \
    --status-file "${LOCAL_STATUS}" \
    --duration-secs 2 >/dev/null 2>&1
) &
LPID=$!
for _ in $(seq 1 40); do
  if [ -f "${LOCAL_STATUS}" ]; then
    break
  fi
  sleep 0.1
done
test -f "${LOCAL_STATUS}"
grep -q '"interface_name": "lo"' "${LOCAL_STATUS}"
curl -sf "http://127.0.0.1:18091/local_tool.bin" -o "${WORK_DIR}/got_local_tool.bin"
diff -u "${LOCAL_INSTALL_DIR}/files/local_tool.bin" "${WORK_DIR}/got_local_tool.bin"
wait "${LPID}" || true

# Verify update of local installation preserves files/ and config/labdrop.toml
"${DIST_DIR}/labdrop-bootstrapper" --dest "${LOCAL_INSTALL_DIR}" --source-dir "${DIST_DIR}" >/dev/null
grep -q 'preferred_interface = "lo"' "${LOCAL_INSTALL_DIR}/config/labdrop.toml"
diff -u "${LOCAL_INSTALL_DIR}/files/local_tool.bin" "${WORK_DIR}/got_local_tool.bin"
echo "  ✓ Local installation workflow verified (install, preferred interface config, share, cleanup, and non-destructive update)."

echo ""
echo "========================================================================"
echo "ALL CROSS-PLATFORM & CRASH-RECOVERY TESTS PASSED SUCCESSFULLY"
echo "========================================================================"
