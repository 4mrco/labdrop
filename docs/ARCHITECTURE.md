# LabDrop — Engineering & Architecture Specification

## 1. Technology & Language Choice

**Selected Language**: **Rust (2021 Edition)**

### Rationale
1. **True Zero-Dependency Cross-Platform Binaries**:
   - **Linux (`x86_64-unknown-linux-musl`)**: Produces a 100% statically linked ELF 64-bit executable (`static-pie linked`). Unlike dynamically linked binaries that fail across Linux distributions due to `GLIBC_2.xx` mismatches (e.g., compiling on Fedora 44 with glibc 2.41+ and running on Ubuntu 22.04 LTS or Linux Mint 21 with older glibc), a static MUSL binary runs identically on Fedora, Ubuntu, and Linux Mint with zero runtime libraries.
   - **Windows (`x86_64-pc-windows-gnu` with `+crt-static`)**: Produces a standalone PE32+ x86-64 executable (`LabDrop.exe`) that statically links `libgcc` and `winpthreads`, importing only built-in Windows system DLLs (`KERNEL32.dll`, `WS2_32.dll`, `ntdll.dll`, `USERENV.dll`, `msvcrt.dll`).
2. **Memory & Crash Safety**:
   - Deterministic RAII resource management (`Drop` implementations on `ActiveSharingSession`, `EmbeddedFileServer`, and `MdnsAdvertiser`) paired with a detached OS-pipe watchdog process ensures temporary shares are never left exposed.
3. **Low Resource Footprint TUI**:
   - Built with `ratatui` + `crossterm`, consuming <10 MiB RAM and requiring no X11/Wayland desktop environment on Linux or GUI runtime on Windows.

---

## 2. Internal Storage Staging Architecture (`crates/labdrop-core/src/staging.rs`)

### Why Staging on Internal Storage is Mandatory
Sharing directly from a USB drive or from arbitrary user directories (`~/Downloads`, `~/Documents`) introduces two serious problems:
1. **Performance & Hardware Wear**: Concurrent network reads from 15–30 laboratory machines thrash USB flash media.
2. **Security & Traversal Permissions**: On Linux (especially Fedora and Ubuntu), `/home/<user>` defaults to mode `0700`. Granting a guest Samba daemon (`nobody`) traversal access into `/home/<user>/Downloads` would require weakening home directory permissions. Furthermore, `/tmp` on Fedora is a RAM-backed `tmpfs` (often limited to a few gigabytes).

### LabDrop Staging Solution
- **Linux Staging Root**: `/var/tmp/labdrop-sessions/<session_id>/staged` (located on the internal storage disk, world-traversable `1777` parent with `0755` session directory and `0644` staged files).
- **Windows Staging Root**: `%TEMP%\labdrop-sessions\<session_id>\staged`.
- **Pre-Flight Disk Space Verification**: Queries `statvfs` (Linux) or `GetDiskFreeSpaceExW` (Windows) before copying a single byte, requiring `total_selected_bytes + 8 MiB` safety margin.
- **Collision-Safe Disambiguation**: When multiple files or folders with identical names are selected from different source directories (e.g., `/run/media/usb/files/setup.exe` and `~/Downloads/setup.exe`), LabDrop automatically renames collisions to `setup (2).exe`, `setup (3).exe` while recording the mapping in `StagingManifest`.
- **Single-Pass Copy + SHA-256 Digest + Atomic Rollback**: Copies files in 64 KiB chunks while computing each file's SHA-256 checksum. If any read/write error or user cancellation occurs, the entire `session_staged_dir` is immediately removed.

---

## 3. Layered Network Sharing Architecture

### Layer 1 (Primary): SMB (`crates/labdrop-core/src/smb.rs`)
- **Linux Transmitters (Samba)**:
  - Inspects `/etc/os-release` to distinguish **Fedora / RHEL** (`dnf`, `smb.service`, `firewalld`, SELinux) from **Ubuntu / Linux Mint / Debian** (`apt-get`, `smbd.service`, `ufw`).
  - Supports two operating modes:
    1. **System Port 445 Mode (Elevated)**: Appends a clearly delimited `# BEGIN LABDROP SESSION <id>` include block to `/etc/samba/smb.conf` (preserving all existing shares), sets `hosts allow = 127.0.0.1 ::1 <interface_subnet>` and `hosts deny = ALL` on `[LabDrop]`, applies `chcon -Rt samba_share_t` if SELinux is enforcing, reloads `smb.service`/`smbd.service`, and opens a runtime-only firewall rule if needed.
    2. **Rootless Port 4445 Mode (Unprivileged)**: Launches an isolated `smbd` daemon inside a Linux user namespace (`unshare --user --map-root-user --map-users=auto --map-groups=auto`) bound strictly to `<interface_ip>:4445` and `127.0.0.1:4445` using a session-local `smb.rootless.conf` validated by `testparm -s`. Requires zero root privileges.
- **Windows Transmitters (Native LanmanServer SMB)**:
  - Resolves the universal `Everyone` SID (`S-1-1-0`) via PowerShell so permissions work across all Windows display languages (English `Everyone`, Portuguese/Spanish `Todos`, German `Jeder`).
  - Applies read-only ACLs (`icacls "<staged_dir>" /grant "*S-1-1-0:(OI)(CI)R" /T /C`), creates a read-only SMB share via `New-SmbShare` (with `net share` fallback), and adds a session-scoped Windows Firewall rule (`LabDrop-Session-<id>`) restricted to `localip=<interface_ip>`.

### Layer 2 (Secondary): Read-Only WebDAV — RFC 4918 (`crates/labdrop-core/src/http_webdav.rs`)
- **Evaluation of Alternatives (WebDAV vs. SFTP)**:
  - **SFTP** requires an SSH server (`sshd`) and OS user credentials or SSH keys. Enabling unauthenticated/empty-password guest SFTP on a transmitting machine would dangerously weaken system-wide SSH security and is not natively mountable in Windows 10/11 File Explorer without third-party software.
  - **WebDAV (RFC 4918)** is natively supported as a network filesystem by both **Linux file managers** (`dav://<ip>:<port>/LabDrop/` in GNOME Files/Nautilus, Cinnamon Nemo, KDE Dolphin) and **Windows 10/11 File Explorer** (`\\<ip>@<port>\DavWWWRoot\LabDrop` via the built-in `WebClient` service), requires zero OS account changes, and runs completely unprivileged inside the LabDrop process.
- **Implementation**:
  - Supports `OPTIONS` (advertising `DAV: 1, 2` and `MS-Author-Via: DAV`), `PROPFIND` (`Depth: 0`, `1`, `infinity` returning `207 Multi-Status` XML), `GET`, `HEAD`, and HTTP `Range` (`206 Partial Content`).
  - Strictly blocks all write/mutation methods (`PUT`, `POST`, `DELETE`, `MKCOL`, `MOVE`, `COPY`, `PROPPATCH`, `LOCK`, `UNLOCK`) with `403 Forbidden`.

### Layer 3 (Last Fallback): Embedded HTTP Server + `.ZIP` Stream + mDNS
- Serves a dark-themed HTML5 directory browser on the same unprivileged port as WebDAV.
- Includes a streaming PKZIP (`STORE` + CRC32) generator (`?zip=1`) so operators or students using a web browser can download an entire folder hierarchy as a `.zip` archive in one click without extra disk space on the transmitter.
- Provides best-effort mDNS discovery (`avahi-publish` helper on Linux + embedded UDP multicast responder on `224.0.0.251:5353` for `<hostname>.local` and `labdrop.local`), while always displaying the direct IPv4 URL first.

---

## 4. Crash-Safe Cleanup & Watchdog (`crates/labdrop-core/src/session.rs`)

LabDrop uses a two-layer defense to ensure a crash or power failure never leaves a share exposed:
1. **Real-Time Pipe + PID Watchdog (`labdrop --internal-watchdog <registry_file> <parent_pid>`)**:
   - Spawned as a detached background process when a session starts, holding the read end of an anonymous OS pipe whose write end lives in the main LabDrop process.
   - If the main LabDrop process panics or is killed with `SIGKILL` (`kill -9`), the OS kernel immediately closes the pipe (`EOF`). Within ~150ms, the watchdog reads `session.json` and tears down child daemons (`smbd`, `avahi-publish`), removes LabDrop's SMB share/config block, removes LabDrop's firewall rule, and deletes the staged directory.
2. **Persistent Session Ledger (`registry/<session_id>.json`)**:
   - On every startup (and via `labdrop cleanup`), LabDrop scans the session registry for any `session.json` whose `owner_pid` is no longer alive and cleans up all recorded resources.
