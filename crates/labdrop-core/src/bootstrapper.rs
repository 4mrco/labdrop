use crate::error::{LabDropError, Result};
use crate::fs_space::{format_bytes, query_disk_space};
use crate::staging::hex_encode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Default GitHub Releases base URL for LabDrop latest release downloads.
pub const DEFAULT_RELEASE_BASE_URL: &str =
    "https://github.com/4mrco/labdrop/releases/latest/download";

/// Source from which the bootstrapper obtains the compiled Linux (`labdrop`) and Windows (`LabDrop.exe`) binaries.
#[derive(Debug, Clone)]
pub enum ArtifactSource {
    /// Offline mode: load `labdrop` and `LabDrop.exe` (and optional `SHA256SUMS.txt`) from a local directory.
    LocalDirectory(PathBuf),
    /// Online mode: download `labdrop`, `LabDrop.exe`, and `SHA256SUMS.txt` from a base release URL (`http://` or `https://`).
    ReleaseBaseUrl(String),
}

/// Verified artifact summary produced after preparing a USB / destination package.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparedPackageReport {
    pub destination_root: PathBuf,
    pub labdrop_dir: PathBuf,
    pub linux_binary_path: PathBuf,
    pub linux_binary_size: u64,
    pub linux_binary_sha256: String,
    pub windows_binary_path: PathBuf,
    pub windows_binary_size: u64,
    pub windows_binary_sha256: String,
    pub files_dir: PathBuf,
    pub config_dir: PathBuf,
    pub checksums_file: PathBuf,
    pub available_space_bytes: u64,
    #[serde(default)]
    pub was_update: bool,
}

/// Candidate USB drive or destination directory detected on the system.
#[derive(Debug, Clone)]
pub struct CandidateDestination {
    pub path: PathBuf,
    pub label: String,
    pub available_bytes: u64,
    pub has_existing_labdrop: bool,
}

fn dir_has_existing_labdrop(dir: &Path) -> bool {
    let target = if dir
        .file_name()
        .map(|n| n.to_string_lossy().eq_ignore_ascii_case("LabDrop"))
        .unwrap_or(false)
    {
        dir.to_path_buf()
    } else {
        dir.join("LabDrop")
    };
    target.join("labdrop").is_file() || target.join("LabDrop.exe").is_file()
}

/// Discovers mounted USB drives / removable media and sensible local directories.
pub fn discover_candidate_destinations() -> Vec<CandidateDestination> {
    let mut candidates = Vec::new();
    let mut seen = std::collections::HashSet::new();

    #[cfg(unix)]
    {
        let mut mount_candidates: Vec<PathBuf> = Vec::new();
        if let Ok(mounts) = fs::read_to_string("/proc/mounts") {
            for line in mounts.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    let mp = parts[1].replace("\\040", " ");
                    if mp.starts_with("/run/media/")
                        || mp.starts_with("/media/")
                        || mp.starts_with("/mnt/")
                    {
                        mount_candidates.push(PathBuf::from(mp));
                    }
                }
            }
        }

        let user = std::env::var("USER").unwrap_or_default();
        let mut roots = Vec::new();
        if !user.is_empty() {
            roots.push(PathBuf::from(format!("/run/media/{user}")));
            roots.push(PathBuf::from(format!("/media/{user}")));
        }
        roots.push(PathBuf::from("/media"));
        roots.push(PathBuf::from("/mnt"));

        for r in roots {
            if let Ok(entries) = fs::read_dir(&r) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_dir() {
                        mount_candidates.push(p);
                    }
                }
            }
        }

        for p in mount_candidates {
            if !p.is_dir() || !seen.insert(p.clone()) {
                continue;
            }
            if let Ok(space) = query_disk_space(&p) {
                let has_existing = dir_has_existing_labdrop(&p);
                let tag = if has_existing {
                    "[UPDATE LabDrop]"
                } else {
                    "[USB / Media]"
                };
                candidates.push(CandidateDestination {
                    label: format!(
                        "{tag} {} ({} free)",
                        p.display(),
                        format_bytes(space.available_bytes)
                    ),
                    path: p,
                    available_bytes: space.available_bytes,
                    has_existing_labdrop: has_existing,
                });
            }
        }
    }

    #[cfg(windows)]
    {
        for letter in b'D'..=b'Z' {
            let drive = PathBuf::from(format!("{}:\\", letter as char));
            if drive.exists() && seen.insert(drive.clone()) {
                if let Ok(space) = query_disk_space(&drive) {
                    let has_existing = dir_has_existing_labdrop(&drive);
                    let tag = if has_existing {
                        "[UPDATE LabDrop]"
                    } else {
                        "[Drive]"
                    };
                    candidates.push(CandidateDestination {
                        label: format!(
                            "{tag} {} ({} free)",
                            drive.display(),
                            format_bytes(space.available_bytes)
                        ),
                        path: drive,
                        available_bytes: space.available_bytes,
                        has_existing_labdrop: has_existing,
                    });
                }
            }
        }
    }

    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        let home_path = PathBuf::from(&home);
        if home_path.is_dir() {
            let local_install = home_path.join("LabDrop");
            if seen.insert(local_install.clone()) {
                if let Ok(space) = query_disk_space(&home_path) {
                    let has_existing = dir_has_existing_labdrop(&local_install);
                    let tag = if has_existing {
                        "[UPDATE Local PC]"
                    } else {
                        "[Local PC Install]"
                    };
                    candidates.push(CandidateDestination {
                        label: format!(
                            "{tag} {} ({} free)",
                            local_install.display(),
                            format_bytes(space.available_bytes)
                        ),
                        path: local_install,
                        available_bytes: space.available_bytes,
                        has_existing_labdrop: has_existing,
                    });
                }
            }
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        let cwd_is_labdrop = cwd
            .file_name()
            .map(|n| n.to_string_lossy().eq_ignore_ascii_case("LabDrop"))
            .unwrap_or(false);
        let local_dir = if cwd_is_labdrop {
            cwd.clone()
        } else {
            cwd.join("LabDrop")
        };
        if seen.insert(local_dir.clone()) {
            if let Ok(space) = query_disk_space(&cwd) {
                let has_existing = dir_has_existing_labdrop(&local_dir);
                let tag = if has_existing {
                    "[UPDATE Folder]"
                } else {
                    "[Local Folder]"
                };
                candidates.push(CandidateDestination {
                    label: format!(
                        "{tag} {} ({} free)",
                        local_dir.display(),
                        format_bytes(space.available_bytes)
                    ),
                    path: local_dir,
                    available_bytes: space.available_bytes,
                    has_existing_labdrop: has_existing,
                });
            }
        }
    }

    candidates
}

/// Auto-detects a local release artifact directory if present near the bootstrapper executable or working directory.
pub fn find_default_local_artifact_dir() -> Option<PathBuf> {
    let mut search_dirs = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        search_dirs.push(cwd.join("dist"));
        search_dirs.push(cwd.join("releases"));
        search_dirs.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            search_dirs.push(parent.to_path_buf());
            search_dirs.push(parent.join("dist"));
            if let Some(gp) = parent.parent() {
                search_dirs.push(gp.join("dist"));
            }
        }
    }

    search_dirs
        .into_iter()
        .find(|dir| dir.join("labdrop").is_file() && dir.join("LabDrop.exe").is_file())
}

/// Prepares the complete cross-platform `LabDrop/` USB package inside `destination_root`.
/// Always installs BOTH `LabDrop.exe` (Windows x86-64) and `labdrop` (Linux x86-64), verifies their
/// binary headers and SHA-256 checksums, and creates `files/` and `config/` without modifying unrelated files.
pub fn prepare_usb_package<F>(
    destination_root: &Path,
    source: &ArtifactSource,
    mut on_step: F,
) -> Result<PreparedPackageReport>
where
    F: FnMut(&str),
{
    on_step("Checking destination directory and available space...");
    fs::create_dir_all(destination_root).map_err(|e| LabDropError::IoPath {
        path: destination_root.to_path_buf(),
        source: e,
    })?;

    let space = query_disk_space(destination_root)?;
    let min_required = 16 * 1024 * 1024; // 16 MiB minimum
    if space.available_bytes < min_required {
        return Err(LabDropError::InsufficientDiskSpace {
            staging_dir: destination_root.to_path_buf(),
            required_bytes: min_required,
            available_bytes: space.available_bytes,
        });
    }

    // If destination_root already ends with "LabDrop", use it directly; otherwise create "LabDrop/" inside it
    let labdrop_dir = if destination_root
        .file_name()
        .map(|n| n.to_string_lossy().eq_ignore_ascii_case("LabDrop"))
        .unwrap_or(false)
    {
        destination_root.to_path_buf()
    } else {
        destination_root.join("LabDrop")
    };

    let files_dir = labdrop_dir.join("files");
    let config_dir = labdrop_dir.join("config");
    let staging_work = labdrop_dir.join(".bootstrap_tmp");

    for d in [&labdrop_dir, &files_dir, &config_dir, &staging_work] {
        fs::create_dir_all(d).map_err(|e| LabDropError::IoPath {
            path: d.clone(),
            source: e,
        })?;
    }

    let tmp_linux = staging_work.join("labdrop.tmp");
    let tmp_windows = staging_work.join("LabDrop.exe.tmp");

    let prepare_result: Result<(String, String, u64, u64)> = (|| {
        let expected_hashes = match source {
            ArtifactSource::LocalDirectory(src_dir) => {
                on_step(&format!(
                    "Reading release binaries from local directory '{}'...",
                    src_dir.display()
                ));
                let src_linux = src_dir.join("labdrop");
                let src_win = src_dir.join("LabDrop.exe");
                if !src_linux.is_file() {
                    return Err(LabDropError::ArtifactVerification {
                        artifact: "labdrop".to_string(),
                        reason: format!(
                            "Linux executable not found at '{}'",
                            src_linux.display()
                        ),
                    });
                }
                if !src_win.is_file() {
                    return Err(LabDropError::ArtifactVerification {
                        artifact: "LabDrop.exe".to_string(),
                        reason: format!(
                            "Windows executable not found at '{}'",
                            src_win.display()
                        ),
                    });
                }

                fs::copy(&src_linux, &tmp_linux).map_err(|e| LabDropError::IoPath {
                    path: src_linux.clone(),
                    source: e,
                })?;
                fs::copy(&src_win, &tmp_windows).map_err(|e| LabDropError::IoPath {
                    path: src_win.clone(),
                    source: e,
                })?;

                let sums_path = src_dir.join("SHA256SUMS.txt");
                if sums_path.is_file() {
                    let text = fs::read_to_string(&sums_path).unwrap_or_default();
                    parse_sha256sums(&text)
                } else {
                    HashMap::new()
                }
            }
            ArtifactSource::ReleaseBaseUrl(base_url) => {
                let clean_base = base_url.trim_end_matches('/');
                let sums_url = format!("{clean_base}/SHA256SUMS.txt");
                let linux_url = format!("{clean_base}/labdrop");
                let win_url = format!("{clean_base}/LabDrop.exe");

                on_step(&format!("Downloading SHA256SUMS.txt from {sums_url}..."));
                let tmp_sums = staging_work.join("SHA256SUMS.txt.tmp");
                download_url_to_file(&sums_url, &tmp_sums)?;
                let sums_text = fs::read_to_string(&tmp_sums).map_err(LabDropError::Io)?;
                let parsed = parse_sha256sums(&sums_text);
                if !parsed.contains_key("labdrop") || !parsed.contains_key("LabDrop.exe") {
                    return Err(LabDropError::ArtifactVerification {
                        artifact: "SHA256SUMS.txt".to_string(),
                        reason: "Downloaded SHA256SUMS.txt does not contain checksums for both 'labdrop' and 'LabDrop.exe'".to_string(),
                    });
                }

                on_step(&format!("Downloading Linux binary from {linux_url}..."));
                download_url_to_file(&linux_url, &tmp_linux)?;

                on_step(&format!("Downloading Windows binary from {win_url}..."));
                download_url_to_file(&win_url, &tmp_windows)?;

                parsed
            }
        };

        on_step("Verifying Linux ELF x86-64 executable header and SHA-256 integrity...");
        verify_linux_elf_x86_64(&tmp_linux)?;
        let (linux_sha, linux_size) = compute_file_sha256(&tmp_linux)?;
        if let Some(expected) = expected_hashes.get("labdrop") {
            if !expected.eq_ignore_ascii_case(&linux_sha) {
                return Err(LabDropError::ArtifactVerification {
                    artifact: "labdrop".to_string(),
                    reason: format!(
                        "SHA-256 mismatch: expected {expected}, computed {linux_sha}"
                    ),
                });
            }
        }

        on_step("Verifying Windows PE32+ x86-64 executable header and SHA-256 integrity...");
        verify_windows_pe_x86_64(&tmp_windows)?;
        let (win_sha, win_size) = compute_file_sha256(&tmp_windows)?;
        if let Some(expected) = expected_hashes.get("LabDrop.exe") {
            if !expected.eq_ignore_ascii_case(&win_sha) {
                return Err(LabDropError::ArtifactVerification {
                    artifact: "LabDrop.exe".to_string(),
                    reason: format!("SHA-256 mismatch: expected {expected}, computed {win_sha}"),
                });
            }
        }

        Ok((linux_sha, win_sha, linux_size, win_size))
    })();

    let (linux_sha, win_sha, linux_size, win_size) = match prepare_result {
        Ok(vals) => vals,
        Err(e) => {
            let _ = fs::remove_dir_all(&staging_work);
            return Err(e);
        }
    };

    let final_linux = labdrop_dir.join("labdrop");
    let final_windows = labdrop_dir.join("LabDrop.exe");
    let was_update = final_linux.exists() || final_windows.exists();

    if was_update {
        let old_linux_sha = compute_file_sha256(&final_linux).ok().map(|(s, _)| s);
        let old_win_sha = compute_file_sha256(&final_windows).ok().map(|(s, _)| s);
        let linux_changed = old_linux_sha.as_deref() != Some(linux_sha.as_str());
        let win_changed = old_win_sha.as_deref() != Some(win_sha.as_str());
        on_step(&format!(
            "Updating existing LabDrop USB installation (labdrop: {}, LabDrop.exe: {}; preserving files/ and config/)...",
            if linux_changed { "updating" } else { "unchanged" },
            if win_changed { "updating" } else { "unchanged" }
        ));
    } else {
        on_step("Installing verified Linux and Windows executables into LabDrop/...");
    }

    fs::rename(&tmp_linux, &final_linux).or_else(|_| {
        fs::copy(&tmp_linux, &final_linux).map(|_| ())
    }).map_err(|e| LabDropError::IoPath {
        path: final_linux.clone(),
        source: e,
    })?;

    fs::rename(&tmp_windows, &final_windows).or_else(|_| {
        fs::copy(&tmp_windows, &final_windows).map(|_| ())
    }).map_err(|e| LabDropError::IoPath {
        path: final_windows.clone(),
        source: e,
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&final_linux, fs::Permissions::from_mode(0o755));
    }

    let _ = fs::remove_dir_all(&staging_work);

    // Write SHA256SUMS.txt
    let checksums_file = labdrop_dir.join("SHA256SUMS.txt");
    let sums_content = format!(
        "{linux_sha}  labdrop\n{win_sha}  LabDrop.exe\n"
    );
    fs::write(&checksums_file, sums_content).map_err(|e| LabDropError::IoPath {
        path: checksums_file.clone(),
        source: e,
    })?;

    // Write default config/labdrop.toml if not already present (never overwrite operator customizations)
    let config_file = config_dir.join("labdrop.toml");
    if !config_file.exists() {
        let default_cfg = "\
# LabDrop Configuration
# Place installers or classroom files inside the '../files/' directory for quick access.

language = \"auto\" # \"auto\", \"en\", or \"pt\"
preferred_interface = \"auto\" # Interface name (e.g. \"wlp2s0\", \"eth0\") or \"auto\"
default_protocol = \"auto\"
preferred_http_port = 8080
preferred_rootless_smb_port = 4445
delete_staged_on_exit = true
";
        let _ = fs::write(&config_file, default_cfg);
    }

    // Write README.txt inside LabDrop/
    let readme_file = labdrop_dir.join("README.txt");
    let readme_content = "\
========================================================================
LabDrop — Portable Cross-Platform Laboratory File Distribution Utility
========================================================================

Contents of this USB package:
  - LabDrop.exe  : Windows 10 / 11 (x86-64) standalone executable
  - labdrop      : Linux (Fedora, Ubuntu, Linux Mint x86-64) standalone executable
  - files/       : Optional folder to store installers/files on this USB drive
  - config/      : Portable configuration directory
  - SHA256SUMS.txt : Cryptographic SHA-256 digests of both executables

How to use on a Windows 10/11 transmitting computer:
  1. Double-click `LabDrop.exe` (or right-click -> 'Run as administrator' for port 445 SMB).
  2. Select files/folders, start the sharing session, and copy from destination PCs.

How to use on a Linux (Fedora / Ubuntu / Linux Mint) transmitting computer:
  1. Open a terminal in this `LabDrop` folder and run: `./labdrop`
     (If the USB drive is mounted with `noexec`, run:
      `cp ./labdrop /var/tmp/labdrop && chmod +x /var/tmp/labdrop && /var/tmp/labdrop`)
  2. Select files/folders, start the sharing session, and copy from destination PCs.
";
    let _ = fs::write(&readme_file, readme_content);

    // Final post-install verification on the committed files
    verify_linux_elf_x86_64(&final_linux)?;
    verify_windows_pe_x86_64(&final_windows)?;

    on_step(if was_update {
        "USB LabDrop update complete and verified (files/ and config/ preserved)."
    } else {
        "USB package preparation complete and verified."
    });

    Ok(PreparedPackageReport {
        destination_root: destination_root.to_path_buf(),
        labdrop_dir,
        linux_binary_path: final_linux,
        linux_binary_size: linux_size,
        linux_binary_sha256: linux_sha,
        windows_binary_path: final_windows,
        windows_binary_size: win_size,
        windows_binary_sha256: win_sha,
        files_dir,
        config_dir,
        checksums_file,
        available_space_bytes: space.available_bytes,
        was_update,
    })
}

/// Validates that `path` is a genuine 64-bit x86-64 ELF executable (`\x7fELF`, `EM_X86_64`).
pub fn verify_linux_elf_x86_64(path: &Path) -> Result<()> {
    let mut f = File::open(path).map_err(|e| LabDropError::ArtifactVerification {
        artifact: path.display().to_string(),
        reason: format!("Cannot open file: {e}"),
    })?;
    let meta = f.metadata().map_err(|e| LabDropError::ArtifactVerification {
        artifact: path.display().to_string(),
        reason: format!("Cannot read metadata: {e}"),
    })?;
    if meta.len() < 64 {
        return Err(LabDropError::ArtifactVerification {
            artifact: path.display().to_string(),
            reason: format!("File is too small ({} bytes) to be a valid ELF binary", meta.len()),
        });
    }

    let mut hdr = [0u8; 64];
    f.read_exact(&mut hdr)
        .map_err(|e| LabDropError::ArtifactVerification {
            artifact: path.display().to_string(),
            reason: format!("Failed to read ELF header: {e}"),
        })?;

    if &hdr[0..4] != b"\x7fELF" {
        return Err(LabDropError::ArtifactVerification {
            artifact: path.display().to_string(),
            reason: "Missing '\\x7fELF' magic header (not a Linux ELF binary)".to_string(),
        });
    }
    if hdr[4] != 2 || hdr[5] != 1 {
        return Err(LabDropError::ArtifactVerification {
            artifact: path.display().to_string(),
            reason: "ELF binary is not 64-bit little-endian".to_string(),
        });
    }
    let e_machine = u16::from_le_bytes([hdr[18], hdr[19]]);
    if e_machine != 0x3E {
        return Err(LabDropError::ArtifactVerification {
            artifact: path.display().to_string(),
            reason: format!(
                "ELF architecture is 0x{e_machine:04x}, expected 0x003e (x86-64)"
            ),
        });
    }
    Ok(())
}

/// Validates that `path` is a genuine 64-bit x86-64 Windows PE executable (`MZ` + `PE\0\0` + `0x8664`).
pub fn verify_windows_pe_x86_64(path: &Path) -> Result<()> {
    let mut f = File::open(path).map_err(|e| LabDropError::ArtifactVerification {
        artifact: path.display().to_string(),
        reason: format!("Cannot open file: {e}"),
    })?;
    let meta = f.metadata().map_err(|e| LabDropError::ArtifactVerification {
        artifact: path.display().to_string(),
        reason: format!("Cannot read metadata: {e}"),
    })?;
    if meta.len() < 128 {
        return Err(LabDropError::ArtifactVerification {
            artifact: path.display().to_string(),
            reason: format!("File is too small ({} bytes) to be a valid PE binary", meta.len()),
        });
    }

    let mut dos_hdr = [0u8; 64];
    f.read_exact(&mut dos_hdr)
        .map_err(|e| LabDropError::ArtifactVerification {
            artifact: path.display().to_string(),
            reason: format!("Failed to read DOS header: {e}"),
        })?;

    if &dos_hdr[0..2] != b"MZ" {
        return Err(LabDropError::ArtifactVerification {
            artifact: path.display().to_string(),
            reason: "Missing 'MZ' DOS header (not a Windows PE executable)".to_string(),
        });
    }

    let pe_offset = u32::from_le_bytes([dos_hdr[60], dos_hdr[61], dos_hdr[62], dos_hdr[63]]) as u64;
    if pe_offset + 6 > meta.len() {
        return Err(LabDropError::ArtifactVerification {
            artifact: path.display().to_string(),
            reason: "Corrupted PE header offset (e_lfanew out of bounds)".to_string(),
        });
    }

    f.seek(SeekFrom::Start(pe_offset))
        .map_err(|e| LabDropError::ArtifactVerification {
            artifact: path.display().to_string(),
            reason: format!("Failed to seek to PE header: {e}"),
        })?;

    let mut pe_sig = [0u8; 6];
    f.read_exact(&mut pe_sig)
        .map_err(|e| LabDropError::ArtifactVerification {
            artifact: path.display().to_string(),
            reason: format!("Failed to read PE signature: {e}"),
        })?;

    if &pe_sig[0..4] != b"PE\0\0" {
        return Err(LabDropError::ArtifactVerification {
            artifact: path.display().to_string(),
            reason: "Missing 'PE\\0\\0' signature".to_string(),
        });
    }

    let machine = u16::from_le_bytes([pe_sig[4], pe_sig[5]]);
    if machine != 0x8664 {
        return Err(LabDropError::ArtifactVerification {
            artifact: path.display().to_string(),
            reason: format!(
                "PE machine type is 0x{machine:04x}, expected 0x8664 (AMD64 / x86-64)"
            ),
        });
    }
    Ok(())
}

/// Computes the hex-encoded SHA-256 digest and byte size of `path`.
pub fn compute_file_sha256(path: &Path) -> Result<(String, u64)> {
    let mut f = File::open(path).map_err(|e| LabDropError::IoPath {
        path: path.to_path_buf(),
        source: e,
    })?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf).map_err(|e| LabDropError::IoPath {
            path: path.to_path_buf(),
            source: e,
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hex_encode(&hasher.finalize()), total))
}

/// Parses a `SHA256SUMS.txt` file into a map of `filename -> sha256_hex`.
pub fn parse_sha256sums(content: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let Some(hash) = parts.next() else {
            continue;
        };
        let Some(name_raw) = parts.next() else {
            continue;
        };
        let clean_name = name_raw.trim_start_matches('*').trim_start_matches("./");
        let base_name = Path::new(clean_name)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| clean_name.to_string());
        if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
            map.insert(base_name, hash.to_lowercase());
        }
    }
    map
}

/// Downloads a file from an `http://` (in-process TCP client) or `https://` (OS TLS via `curl` / PowerShell) URL.
pub fn download_url_to_file(url: &str, dest_path: &Path) -> Result<()> {
    if let Some(rest) = url.strip_prefix("http://") {
        download_http_plain(rest, dest_path)
    } else if url.starts_with("https://") {
        download_https_via_os_tool(url, dest_path)
    } else {
        Err(LabDropError::Network(format!(
            "Unsupported URL scheme in '{url}' (expected http:// or https://)"
        )))
    }
}

fn download_http_plain(url_without_scheme: &str, dest_path: &Path) -> Result<()> {
    let (host_port, path) = match url_without_scheme.split_once('/') {
        Some((hp, p)) => (hp, format!("/{p}")),
        None => (url_without_scheme, "/".to_string()),
    };
    let (host, port) = match host_port.split_once(':') {
        Some((h, p)) => (
            h,
            p.parse::<u16>()
                .map_err(|_| LabDropError::Network(format!("Invalid port in '{host_port}'")))?,
        ),
        None => (host_port, 80u16),
    };

    let mut stream = TcpStream::connect((host, port)).map_err(|e| {
        LabDropError::Network(format!("Failed to connect to {host}:{port}: {e}"))
    })?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));

    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_port}\r\nUser-Agent: LabDrop-Bootstrapper/0.1\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).map_err(LabDropError::Io)?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .map_err(LabDropError::Io)?;
    if !status_line.contains("200") {
        return Err(LabDropError::Network(format!(
            "HTTP server returned non-200 status for 'http://{url_without_scheme}': {}",
            status_line.trim()
        )));
    }

    loop {
        let mut hdr = String::new();
        let n = reader.read_line(&mut hdr).map_err(LabDropError::Io)?;
        if n == 0 || hdr.trim().is_empty() {
            break;
        }
    }

    let mut out = File::create(dest_path).map_err(|e| LabDropError::IoPath {
        path: dest_path.to_path_buf(),
        source: e,
    })?;
    std::io::copy(&mut reader, &mut out).map_err(LabDropError::Io)?;
    out.flush().map_err(LabDropError::Io)?;
    Ok(())
}

fn download_https_via_os_tool(url: &str, dest_path: &Path) -> Result<()> {
    // 1. Try `curl -fSL` (available on Linux and built into Windows 10/11 System32)
    if let Ok(status) = Command::new("curl")
        .args(["-fSL", "--connect-timeout", "15", "-o"])
        .arg(dest_path)
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        if status.success() && dest_path.is_file() {
            return Ok(());
        }
    }

    // 2. Fallback on Windows: PowerShell Invoke-WebRequest
    #[cfg(windows)]
    {
        let ps = format!(
            "$ProgressPreference = 'SilentlyContinue'; Invoke-WebRequest -Uri '{}' -OutFile '{}' -UseBasicParsing",
            url.replace('\'', "''"),
            dest_path.display().to_string().replace('\'', "''")
        );
        if let Ok(status) = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            if status.success() && dest_path.is_file() {
                return Ok(());
            }
        }
    }

    Err(LabDropError::Network(format!(
        "Failed to download '{url}'. Check internet connectivity or use `--source-dir <path>` for offline preparation."
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_synthetic_elf_x86_64() -> Vec<u8> {
        let mut b = vec![0u8; 256];
        b[0..4].copy_from_slice(b"\x7fELF");
        b[4] = 2; // 64-bit
        b[5] = 1; // little endian
        b[6] = 1; // version
        b[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
        b[18..20].copy_from_slice(&0x3Eu16.to_le_bytes()); // EM_X86_64
        b
    }

    fn make_synthetic_pe_x86_64() -> Vec<u8> {
        let mut b = vec![0u8; 256];
        b[0..2].copy_from_slice(b"MZ");
        b[60..64].copy_from_slice(&0x80u32.to_le_bytes()); // e_lfanew = 128
        b[128..132].copy_from_slice(b"PE\0\0");
        b[132..134].copy_from_slice(&0x8664u16.to_le_bytes()); // AMD64
        b
    }

    #[test]
    fn bootstrapper_should_prepare_both_executables_and_reject_corrupted_binaries() {
        let tmp = TempDir::new().unwrap();
        let src_dir = tmp.path().join("release_src");
        fs::create_dir_all(&src_dir).unwrap();

        let elf_bytes = make_synthetic_elf_x86_64();
        let pe_bytes = make_synthetic_pe_x86_64();
        let linux_p = src_dir.join("labdrop");
        let win_p = src_dir.join("LabDrop.exe");
        fs::write(&linux_p, &elf_bytes).unwrap();
        fs::write(&win_p, &pe_bytes).unwrap();

        let (l_sha, _) = compute_file_sha256(&linux_p).unwrap();
        let (w_sha, _) = compute_file_sha256(&win_p).unwrap();
        fs::write(
            src_dir.join("SHA256SUMS.txt"),
            format!("{l_sha}  labdrop\n{w_sha}  LabDrop.exe\n"),
        )
        .unwrap();

        let usb_dest = tmp.path().join("USB_DRIVE");
        // Put an unrelated user file on the USB drive to ensure it is never touched
        fs::create_dir_all(&usb_dest).unwrap();
        fs::write(usb_dest.join("my_thesis.pdf"), b"important user document").unwrap();

        let report = prepare_usb_package(
            &usb_dest,
            &ArtifactSource::LocalDirectory(src_dir.clone()),
            |_| {},
        )
        .unwrap();

        assert!(report.linux_binary_path.is_file());
        assert!(report.windows_binary_path.is_file());
        assert!(report.files_dir.is_dir());
        assert!(report.config_dir.is_dir());
        assert!(report.checksums_file.is_file());
        assert_eq!(
            fs::read(usb_dest.join("my_thesis.pdf")).unwrap(),
            b"important user document"
        );

        // Now corrupt SHA256SUMS.txt and verify bootstrapper rejects it
        fs::write(
            src_dir.join("SHA256SUMS.txt"),
            format!(
                "{}  labdrop\n{w_sha}  LabDrop.exe\n",
                "0".repeat(64)
            ),
        )
        .unwrap();
        let bad_dest = tmp.path().join("USB_BAD");
        let res = prepare_usb_package(
            &bad_dest,
            &ArtifactSource::LocalDirectory(src_dir),
            |_| {},
        );
        assert!(matches!(res, Err(LabDropError::ArtifactVerification { .. })));
    }

    #[test]
    fn bootstrapper_should_update_existing_usb_and_preserve_files_and_config() {
        let tmp = TempDir::new().unwrap();
        let src_v1 = tmp.path().join("src_v1");
        let src_v2 = tmp.path().join("src_v2");
        fs::create_dir_all(&src_v1).unwrap();
        fs::create_dir_all(&src_v2).unwrap();

        let elf_v1 = make_synthetic_elf_x86_64();
        let pe_v1 = make_synthetic_pe_x86_64();
        fs::write(src_v1.join("labdrop"), &elf_v1).unwrap();
        fs::write(src_v1.join("LabDrop.exe"), &pe_v1).unwrap();

        let usb_dest = tmp.path().join("VENTOY_USB");
        let rep1 = prepare_usb_package(
            &usb_dest,
            &ArtifactSource::LocalDirectory(src_v1),
            |_| {},
        )
        .unwrap();
        assert!(!rep1.was_update);

        // User adds custom files and custom config
        let carrier_file = rep1.files_dir.join("aula_01.pdf");
        fs::write(&carrier_file, b"classroom material").unwrap();
        let custom_cfg = rep1.config_dir.join("labdrop.toml");
        fs::write(&custom_cfg, b"language = \"pt\"\n").unwrap();

        // Build v2 binaries with modified payload
        let mut elf_v2 = make_synthetic_elf_x86_64();
        elf_v2.extend_from_slice(b"v2-payload");
        let mut pe_v2 = make_synthetic_pe_x86_64();
        pe_v2.extend_from_slice(b"v2-payload");
        fs::write(src_v2.join("labdrop"), &elf_v2).unwrap();
        fs::write(src_v2.join("LabDrop.exe"), &pe_v2).unwrap();

        let rep2 = prepare_usb_package(
            &usb_dest,
            &ArtifactSource::LocalDirectory(src_v2),
            |_| {},
        )
        .unwrap();
        assert!(rep2.was_update);
        assert_ne!(rep1.linux_binary_sha256, rep2.linux_binary_sha256);
        assert_ne!(rep1.windows_binary_sha256, rep2.windows_binary_sha256);
        assert_eq!(fs::read(&carrier_file).unwrap(), b"classroom material");
        assert_eq!(fs::read(&custom_cfg).unwrap(), b"language = \"pt\"\n");
    }

    #[test]
    fn bootstrapper_candidates_exclude_usb_ready_and_support_local_install_dir() {
        let candidates = discover_candidate_destinations();
        assert!(!candidates.is_empty());
        assert!(
            !candidates
                .iter()
                .any(|c| c.path.to_string_lossy().contains("usb_ready")),
            "discover_candidate_destinations must not use hardcoded usb_ready folder"
        );

        let tmp = TempDir::new().unwrap();
        let src_dir = tmp.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("labdrop"), make_synthetic_elf_x86_64()).unwrap();
        fs::write(src_dir.join("LabDrop.exe"), make_synthetic_pe_x86_64()).unwrap();

        // Local installation when destination already ends with "LabDrop"
        let local_target = tmp.path().join("LabDrop");
        let rep = prepare_usb_package(
            &local_target,
            &ArtifactSource::LocalDirectory(src_dir),
            |_| {},
        )
        .unwrap();
        assert_eq!(rep.labdrop_dir, local_target);
        assert!(local_target.join("labdrop").is_file());
        assert!(local_target.join("LabDrop.exe").is_file());
        assert!(!local_target.join("LabDrop").exists());
    }
}
