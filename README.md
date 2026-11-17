# LabDrop — Cross-Platform Laboratory File Distribution Utility

**LabDrop** is a lightweight, portable, cross-platform file distribution utility built for computer laboratories running mixed operating systems (**Windows 10/11**, **Fedora Linux**, **Ubuntu Linux**, and **Linux Mint**).

It solves a single focused problem: distributing installers, archives, or classroom materials from a USB drive (or local directories) on one transmitting computer to multiple destination computers on the local network **without installing any agent, runtime, or software on destination machines**.

---

## Key Features

- **Zero Destination Dependencies**: Destination computers access files using their native file manager (`File Explorer` on Windows 10/11, `Files / Nautilus` on Fedora & Ubuntu, `Nemo` on Linux Mint) or any standard web browser.
- **Internal Storage Staging**: Copies selected files and folders from the USB drive (or anywhere on the transmitting PC) to an internal temporary staging directory before sharing—protecting original files, preventing slow concurrent USB reads, checking available disk space upfront, and automatically disambiguating duplicate filenames (`installer (2).exe`).
- **Layered Network Sharing Architecture**:
  1. **Primary — SMB**:
     - **Linux Transmitters (Samba)**: Automatically detects Fedora (`dnf`, `smb.service`, `firewalld`) vs Ubuntu/Linux Mint (`apt-get`, `smbd.service`, `ufw`). Supports both **Standard Port 445 System Mode** (with administrator elevation) and **Rootless Port 4445 Mode** (isolated user-namespace `smbd` requiring zero root privileges).
     - **Windows Transmitters (Native SMB)**: Uses built-in Windows `LanmanServer` (`New-SmbShare` / `net share`) with locale-independent `Everyone` SID (`*S-1-1-0`) read-only permissions and a session-scoped Windows Firewall rule.
  2. **Secondary — Read-Only WebDAV (RFC 4918)**:
     - Built-in standards-compliant WebDAV server (`OPTIONS`, `PROPFIND`, `GET`, `HEAD`, HTTP Range requests) that works natively in Linux file managers (`dav://<ip>:<port>/LabDrop/`) and Windows Explorer (`\\<ip>@<port>\DavWWWRoot\LabDrop`) with zero external dependencies and no administrator privileges required.
  3. **Last Fallback — Embedded HTTP Server + On-the-Fly `.ZIP` Streaming**:
     - Clean web interface with individual file downloads, `SHA256SUMS.txt` manifest, and one-click **Download Folder as `.ZIP`** streaming (`?zip=1`), plus best-effort mDNS (`.local`) advertisement alongside direct IP URLs.
- **Crash-Safe Session Cleanup & Real-Time Watchdog**:
  - Tracks every session resource in an atomic `session.json` ownership ledger.
  - Spawns a detached watchdog process (`--internal-watchdog`) linked via an OS pipe so even an abrupt crash (`SIGKILL` / `kill -9`) immediately terminates temporary shares, daemons, firewall rules, and staged files.
- **Cross-Platform USB Bootstrapper**:
  - Standalone executables for both Linux (`labdrop-bootstrapper`) and Windows (`labdrop-bootstrapper.exe`) that prepare a complete portable `LabDrop/` USB directory containing **both** `LabDrop.exe` (Windows x86-64) and `labdrop` (Linux x86-64), verified by SHA-256 checksums and ELF/PE binary header inspection.

---

## USB Package Layout

When prepared by `labdrop-bootstrapper`, your USB drive contains:

```text
LabDrop/
├── LabDrop.exe          # Windows 10/11 (x86-64) standalone executable
├── labdrop              # Linux (Fedora, Ubuntu, Linux Mint x86-64) static executable
├── SHA256SUMS.txt       # Cryptographic SHA-256 digests of both executables
├── README.txt           # Quick-reference operator instructions
├── files/               # Optional folder for carrying installers/materials on the USB
└── config/
    └── labdrop.toml     # Portable configuration settings
```

---

## Preparing Your USB Drive (Bootstrapper Usage)

### 1. Interactive TUI Mode
Run `./dist/labdrop-bootstrapper` (on Linux) or `dist\labdrop-bootstrapper.exe` (on Windows) with no arguments:
1. Select your mounted USB drive or target directory (shows available free space).
2. Choose **Offline Local Release Directory** (`./dist`) or **Online Release Download**.
3. Press `Enter` to create `LabDrop/`, copy both `LabDrop.exe` and `labdrop`, and verify their SHA-256 checksums and ELF/PE x86-64 headers.

### 2. Command-Line Mode (Offline or Online)
```bash
# Offline preparation from locally built release binaries in ./dist:
./dist/labdrop-bootstrapper --dest /run/media/$USER/USB_DRIVE --source-dir ./dist

# Online preparation from a release server / GitHub Releases URL:
./dist/labdrop-bootstrapper --dest /run/media/$USER/USB_DRIVE --release-url https://github.com/4mrco/labdrop/releases/latest/download
```

On Windows PowerShell / Command Prompt:
```powershell
.\dist\labdrop-bootstrapper.exe --dest E:\ --source-dir .\dist
```

---

## Running LabDrop in the Laboratory

### Interactive TUI Workflow (Default)
1. Plug the USB drive into the transmitting computer.
2. Launch LabDrop:
   - **Linux (Fedora / Ubuntu / Linux Mint)**: Run `./labdrop` (or `sudo ./labdrop` if you want standard TCP port 445 SMB for Windows clients).
     - *Note*: If the USB drive is mounted with `noexec`, copy the binary to `/var/tmp/labdrop`, run `chmod +x /var/tmp/labdrop`, and execute `/var/tmp/labdrop`.
   - **Windows 10/11**: Double-click `LabDrop.exe` (or right-click -> **Run as administrator** to enable native Windows SMB share creation on port 445).
3. Select files and folders using **any** of these methods:
   - Press **`[1] Browse & Select Files / Folders`**: use `F1`–`F4` to jump between USB `files/`, `Downloads`, `Home`, or root drives; press `Space` to select, `Enter` to open a folder, `.` (or `h`) to toggle hidden dotfiles (`.*`, hidden by default).
   - **Drag & Drop / Paste** files or folders straight onto the terminal (or pass paths as arguments / drag onto `LabDrop.exe` on Windows, or press **`[p]`** to paste paths).
   - Press **`[o]`** (Native File Picker) or **`[f]`** (Native Folder Picker) to select items via the OS file dialog (`zenity`/`kdialog` on Linux, Windows Explorer dialog on Windows), or **`[x]`** to open the folder in your system file manager.
   - Press **`[L]`** at any time to switch between **English (`EN`)** and **Português (`PT-BR`)**.
4. Press **`[3] Configure Network Interface & Protocol`** (optional):
   - Select the laboratory LAN/Wi-Fi interface to bind to.
   - Choose `Layered Auto (SMB + WebDAV + HTTP)`, `SMB Only`, `WebDAV Secondary`, or `HTTP Fallback`.
5. Press **`[4] Stage Files & Start Sharing Session`**:
   - LabDrop verifies available disk space, copies your selections into internal storage staging, starts the sharing listeners and crash watchdog, and displays the copy-ready access addresses.
6. On destination computers, open the native file manager:
   - **Windows 10/11 File Explorer**:
     - Primary SMB: `\\<IP>\LabDrop`
     - Secondary WebDAV: `\\<IP>@8080\DavWWWRoot\LabDrop`
     - Fallback Browser: `http://<IP>:8080/`
   - **Linux (Fedora / Ubuntu / Linux Mint — Files / Nautilus / Nemo)**:
     - Primary SMB: `smb://<IP>/LabDrop` (or `smb://<IP>:4445/LabDrop` in rootless mode)
     - Secondary WebDAV: `dav://<IP>:8080/LabDrop/`
     - Fallback Browser: `http://<IP>:8080/`
7. When finished, press **`s`** or **`[5] Stop Sharing & Clean Up Session`**:
   - Choose whether to **Delete staged files (Recommended)** or **Retain staged files on disk**.
   - LabDrop stops all listeners, removes only LabDrop-created share definitions and firewall rules, and confirms every cleanup action.

### CLI Subcommands
LabDrop also supports non-interactive CLI subcommands for scripting and diagnostics:
```bash
# Inspect OS, Samba, firewall, network interfaces, and disk space:
./dist/labdrop doctor

# Stage and share selected files/folders from the CLI:
./dist/labdrop share \
  --select ./files/installer.exe \
  --select ~/Downloads/lab-materials \
  --strategy auto \
  --http-port 8080

# Recover and clean up any orphaned sessions after a power loss:
./dist/labdrop cleanup
```

---

## Building from Source

### Prerequisites (Fedora x86-64 Host)
- Rust stable toolchain (`rustc` / `cargo`) with targets:
  - `x86_64-unknown-linux-musl` (for static portable Linux binaries)
  - `x86_64-pc-windows-gnu` (for Windows x86-64 PE executables)
- MinGW-w64 GCC toolchain (`x86_64-w64-mingw32-gcc`) for Windows cross-linking

### Build All Release Binaries
```bash
./scripts/build-release.sh
```
This produces in `./dist/`:
- `dist/labdrop` (Linux x86-64 static ELF executable)
- `dist/LabDrop.exe` (Windows x86-64 PE32+ executable)
- `dist/labdrop-bootstrapper` (Linux x86-64 static ELF executable)
- `dist/labdrop-bootstrapper.exe` (Windows x86-64 PE32+ executable)
- `dist/SHA256SUMS.txt`

### Run Automated & Cross-Platform Tests
```bash
# Run Rust unit and integration tests:
cargo test --workspace

# Run cross-platform verification suite (Fedora host, Debian/Ubuntu container, Wine Windows PE, and SIGKILL watchdog):
./scripts/run-cross-tests.sh
```
