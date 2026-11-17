# LabDrop — Compatibility & Test Report

## 1. Test Environment Summary

- **Primary Development & Test Host**: Fedora Linux 44 (Nobara x86-64, Kernel `7.2.11-200.nobara.fc44.x86_64`)
- **Installed Host Tooling Used**:
  - Rust `1.99.0` (`x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`, `x86_64-pc-windows-gnu`)
  - MinGW-w64 GCC `16.2.1` (`x86_64-w64-mingw32-gcc`)
  - Samba `4.24.2` (`smbd`, `testparm`, `smbclient`)
  - Wine `11.17` (for executing compiled Windows x86-64 `.exe` binaries)
  - Podman rootless container engine with `debian:12-slim` (Ubuntu/Linux Mint/Debian family userland)
- **Virtualization Note**: Neither VirtualBox (`VBoxManage`) nor QEMU (`qemu-system-x86_64`) was pre-installed on the development host, and `sudo` requires an interactive password. Cross-platform verification was therefore executed using native Fedora execution, Debian/Ubuntu-family rootless Podman containers, and Wine for Windows PE32+ execution, supplemented by a GitHub Actions CI matrix (`.github/workflows/release.yml`) configured for `ubuntu-latest` and `windows-latest`.

---

## 2. Feature & Requirement Verification Matrix

| Requirement / Test Case | Status | Verification Evidence |
|---|---|---|
| Individual file & whole-folder selection | **Executed & Passed** | Unit & E2E tests (`staging::tests`, `e2e_integration.rs`) |
| Staging on internal storage & directory structure preservation | **Executed & Passed** | Verified nested folders copy to `/var/tmp/labdrop-sessions/<id>/staged` |
| Duplicate filename collision handling | **Executed & Passed** | Verified `installer.exe` + `installer.exe` -> `installer (2).exe` without overwriting |
| Large files (10+ MiB) & SHA-256 integrity | **Executed & Passed** | `e2e_large_file_unicode_duplicate_staging_and_layered_webdav_http_session` |
| Spaces, Unicode (PT-BR, CJK, Emoji), and long filenames | **Executed & Passed** | `unicode_spaces_and_nested_directory_structure_should_be_preserved` |
| Pre-flight disk space check & rollback on failure/cancel | **Executed & Passed** | `insufficient_disk_space_should_abort_before_staging` & `cancelled_staging_should_rollback` |
| Linux Samba SMB share creation (`smbd` + `testparm`) | **Executed & Passed** | `e2e_linux_samba_smb_share_read_only_and_cleanup` & `run-cross-tests.sh` |
| SMB Share accessibility & download via `smbclient` | **Executed & Passed** | Downloaded normal and collision-renamed files via `smbclient -N` |
| SMB Read-Only enforcement (reject upload/delete) | **Executed & Passed** | Verified `put` and `del` via `smbclient` are rejected (`NT_STATUS_MEDIA_WRITE_PROTECTED`) |
| Secondary Mechanism: WebDAV (RFC 4918 `OPTIONS`, `PROPFIND`, `GET`, Range) | **Executed & Passed** | Verified `207 Multi-Status` XML, `206 Partial Content`, and `403 Forbidden` on `PUT` |
| Last Fallback: Embedded HTTP Server + `.ZIP` archive stream | **Executed & Passed** | Verified HTML listing, `/SHA256SUMS.txt`, and `GET /?zip=1` streaming PKZIP |
| Normal session stop & cleanup (delete vs. retain staged files) | **Executed & Passed** | Verified both delete and retain modes in `e2e_integration.rs` |
| Crash cleanup after `SIGKILL` (`kill -9`) via Watchdog | **Executed & Passed** | `run-cross-tests.sh` Test 3: `kill -9` on `labdrop` terminated `smbd` and deleted staged dir within 1.5s |
| Linux Bootstrapper (`labdrop-bootstrapper`) | **Executed & Passed** | Prepared USB layout with both `labdrop` and `LabDrop.exe`, verified SHA-256 & ELF/PE headers |
| Windows Bootstrapper (`labdrop-bootstrapper.exe`) | **Executed & Passed** | Executed under Wine (`run-cross-tests.sh` Test 2); prepared & verified both executables |
| Offline & Online Bootstrapper modes | **Executed & Passed** | Verified `--source-dir` (offline) and `--release-url http://...` (online) + corrupted hash rejection |
| Local Computer Installation Workflow (`~/LabDrop`) | **Executed & Passed** | `run-cross-tests.sh` Test 7 & `bootstrapper::tests`: verified local install, config, share, cleanup & non-destructive update without `usb_ready` |
| Hot-Switching Network Interfaces & Restarting Sharing Service | **Executed & Passed** | `session::tests::restart_on_interface_stops_old_listener_preserves_staged_files_and_binds_new_interface` |
| Preferred Network Interface Persistence (`labdrop.toml`) | **Executed & Passed** | `config::tests` & `resolve_preferred_interface` (including fallback warning when preferred interface is disconnected) |
| TUI Mouse Support & Expandable Sharing Details | **Executed & Passed** | `tui::tests::tui_top_bar_mouse_click_switches_screens_and_toggles_details` |
| Linux GUI Double-Click Terminal Auto-Launch & `.tar.gz` `+x` Archive | **Executed & Passed** | Verified `ensure_interactive_terminal_on_linux` and `labdrop-bootstrapper-linux-x86_64.tar.gz` (`0755` permissions) |

---

## 3. Cross-Platform Transmitter / Destination Matrix

| Transmitter | Destination | Status | How Verified in Available Environment |
|---|---|---|---|
| **Fedora Linux** | **Fedora Linux** | **Executed & Passed** | Native `smbd` (SMB), WebDAV (`PROPFIND`/`GET`), and HTTP (`.zip` & file downloads) |
| **Fedora Linux** | **Ubuntu / Mint (Debian container)** | **Executed & Passed** | `run-cross-tests.sh` Test 5: Fedora transmitter -> Debian 12 container client & online bootstrapper |
| **Fedora Linux** | **Windows** | **Partially Executed** | Verified SMB protocol compliance via `smbclient`, WebDAV `MS-Author-Via: DAV` headers, and HTTP; live Windows Explorer GUI mount requires physical/VM Windows |
| **Ubuntu / Mint (Debian container)** | **Fedora Linux** | **Executed & Passed** | `run-cross-tests.sh` Test 4: Static `labdrop` binary running inside Debian 12 container serving to Fedora host |
| **Ubuntu / Mint (Debian container)** | **Ubuntu / Mint (Debian container)** | **Executed & Passed** | Static MUSL `labdrop` + `labdrop-bootstrapper` executed inside Debian 12 container |
| **Ubuntu / Mint** | **Windows** | **Partially Executed** | Static Linux binary verified in Debian container; Windows Explorer client requires physical/VM Windows |
| **Windows (`LabDrop.exe` via Wine)** | **Fedora Linux** | **Executed & Passed** | `run-cross-tests.sh` Test 6: `LabDrop.exe` staged files under Wine and served WebDAV (`207`) + HTTP to Fedora |
| **Windows (`LabDrop.exe` via Wine)** | **Ubuntu / Mint** | **Executed & Passed** | `LabDrop.exe` WebDAV/HTTP server accessible from Linux userland |
| **Windows (`LabDrop.exe`)** | **Windows** | **Partially Executed** | `LabDrop.exe` and `labdrop-bootstrapper.exe` execute cleanly under Wine; PowerShell `New-SmbShare` / `icacls *S-1-1-0` / `netsh` command generation verified by unit tests, but live Windows `LanmanServer` kernel SMB requires a real Windows 10/11 OS |

---

## 4. What Still Requires Real Laboratory Testing

Because container and Wine environments cannot replicate institutional network hardware or Windows kernel drivers, the following items must be validated on physical laboratory machines:

1. **Windows 10/11 Guest SMB Policies (`LanmanWorkstation`)**:
   - Modern Windows 10 (2004+) and Windows 11 editions (especially Pro/Education/Enterprise) often disable *Insecure Guest Logons* by default in Group Policy (`Computer Configuration -> Administrative Templates -> Network -> Lanman Workstation -> Enable insecure guest logons`).
   - If destination Windows 10/11 lab PCs block unauthenticated guest SMB connections to `\\<IP>\LabDrop`, operators should use LabDrop's **Secondary WebDAV** endpoint (`\\<IP>@8080\DavWWWRoot\LabDrop`) or **HTTP Fallback** (`http://<IP>:8080/`), both of which work without guest SMB registry changes.
2. **Windows Native SMB Elevation (`New-SmbShare` & Windows Defender Firewall)**:
   - Verify on a physical Windows 10/11 machine that running `LabDrop.exe` as Administrator creates the `LabDrop` share and `LabDrop-Session-<id>` firewall rule under the active Network Profile (Public vs. Private/Domain) and removes both on exit.
3. **Institutional Switch ACLs & Client Isolation**:
   - University/school networks frequently enable **AP Client Isolation** on Wi-Fi or port-level ACLs blocking TCP port 445 between student VLAN ports. Verify whether port 445 (SMB) and port 8080 (WebDAV/HTTP) are reachable between adjacent lab benches.
4. **Multicast mDNS (`.local`) Filtering**:
   - Many managed enterprise switches drop UDP port 5353 multicast traffic (`224.0.0.251`). LabDrop displays the direct IPv4 address (`http://<IP>:8080/` and `\\<IP>\LabDrop`) prominently so operators never depend solely on `.local` resolution.
5. **USB Filesystem Mount Flags on Linux (`noexec` on vfat/exfat)**:
   - Some Linux desktop automounters mount FAT32/exFAT USB drives with `noexec`. Verify on lab distros whether `./labdrop` launches directly from `/run/media/<user>/<usb>/LabDrop/labdrop` or requires the one-line copy to `/var/tmp/labdrop` documented in `LabDrop/README.txt`.
6. **Android / Mobile SMB Clients (`Samsung My Files` & `File Manager Plus`)**:
   - Fedora Workstation's `firewalld` blocks port `445` by default (`1025-65535/tcp` open) and AppArmor restricts `samba-dcerpcd` share enumeration (`srvsvc`) to `/etc/samba/*`. Rootless mode on port `4445` supports direct share access (`//<ip>:4445/LabDrop`), whereas standard port `445` with share browsing requires running `labdrop` elevated (`sudo` or `[e]` in Network Config). Alternatively, **WebDAV** (`http://<ip>:8080/LabDrop/`) works out of the box in `File Manager Plus` without root.
