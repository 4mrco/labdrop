use crate::error::{LabDropError, Result};
use crate::http_webdav::{EmbeddedFileServer, EmbeddedServerMode};
use crate::network::{MdnsAdvertiser, NetworkInterface};
use crate::smb::{
    cleanup_smb_record, kill_pid_gracefully, start_smb_share, ActiveSmbShare, SmbCleanupRecord,
    SmbLinuxMode,
};
use crate::staging::{default_staging_base_dir, StagingManifest};
use crate::transfer::TransferTracker;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Sharing protocol strategy chosen by the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SharingStrategy {
    /// Layered default: starts SMB (Primary) + WebDAV (Secondary) + HTTP (Fallback).
    /// If SMB cannot start (e.g. missing Samba or no root when port 445 requested), automatically falls back to WebDAV + HTTP.
    AutoLayered,
    /// Primary SMB only (fails if SMB cannot start).
    SmbOnly,
    /// Secondary mechanism: Read-Only WebDAV (RFC 4918) + HTTP on unprivileged port.
    WebDavSecondary,
    /// Last fallback: Embedded HTTP browser server only.
    HttpFallback,
}

impl SharingStrategy {
    pub fn display_name(&self) -> &'static str {
        match self {
            SharingStrategy::AutoLayered => "Layered (SMB Primary + WebDAV Secondary + HTTP Fallback)",
            SharingStrategy::SmbOnly => "SMB Only (Samba / Windows Native SMB)",
            SharingStrategy::WebDavSecondary => "WebDAV Secondary (RFC 4918 Native File Manager + HTTP)",
            SharingStrategy::HttpFallback => "HTTP Fallback Only (Browser + .ZIP Stream)",
        }
    }
}

/// A clear, copy-ready access method displayed to the user for destination machines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessEndpoint {
    pub protocol: String,
    pub target_os: String,
    pub address: String,
    pub instructions: String,
}

/// Persistent session ownership record stored on disk (`session.json`) for crash-safe cleanup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLedger {
    pub session_id: String,
    pub owner_pid: u32,
    pub created_epoch_secs: u64,
    pub interface_name: String,
    pub interface_ip: String,
    pub session_dir: PathBuf,
    pub staged_dir: PathBuf,
    pub registry_file: PathBuf,
    pub smb_record: Option<SmbCleanupRecord>,
    pub child_pids: Vec<u32>,
    pub active_protocols: Vec<String>,
    pub endpoints: Vec<AccessEndpoint>,
    pub warnings: Vec<String>,
}

/// Summary report returned after stopping a session or recovering a crashed session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupReport {
    pub session_id: String,
    pub actions_performed: Vec<String>,
    pub staged_files_deleted: bool,
    pub retained_staged_dir: Option<PathBuf>,
}

/// Live handle to an active LabDrop sharing session.
pub struct ActiveSharingSession {
    pub ledger: SessionLedger,
    pub manifest: StagingManifest,
    pub tracker: Arc<TransferTracker>,
    smb_share: Option<ActiveSmbShare>,
    embedded_server: Option<EmbeddedFileServer>,
    mdns: Option<MdnsAdvertiser>,
    watchdog_stdin: Option<ChildStdin>,
    watchdog_child: Option<Child>,
    cleaned_up: bool,
}

impl ActiveSharingSession {
    /// Starts a sharing session for an already-staged `StagingManifest`.
    pub fn start(
        manifest: StagingManifest,
        session_dir: PathBuf,
        iface: &NetworkInterface,
        strategy: SharingStrategy,
        linux_smb_mode: Option<SmbLinuxMode>,
        preferred_http_port: u16,
        spawn_watchdog: bool,
    ) -> Result<Self> {
        let session_id = manifest.session_id.clone();
        let staged_dir = manifest.staged_dir.clone();
        let tracker = TransferTracker::shared();

        let registry_dir = session_registry_dir(&session_dir);
        fs::create_dir_all(&registry_dir).map_err(|e| LabDropError::IoPath {
            path: registry_dir.clone(),
            source: e,
        })?;
        let registry_file = registry_dir.join(format!("{session_id}.json"));

        let mut smb_share: Option<ActiveSmbShare> = None;
        let mut embedded_server: Option<EmbeddedFileServer> = None;
        let mut active_protocols = Vec::new();
        let mut endpoints = Vec::new();
        let mut warnings = Vec::new();
        let mut child_pids = Vec::new();

        // 1. Try starting Primary Mechanism (SMB) when requested
        if matches!(strategy, SharingStrategy::AutoLayered | SharingStrategy::SmbOnly) {
            match start_smb_share(
                &session_id,
                &session_dir,
                &staged_dir,
                iface,
                linux_smb_mode,
                Arc::clone(&tracker),
            ) {
                Ok(smb) => {
                    active_protocols.push(format!("SMB ({})", smb.mode_description));
                    if let Some(pid) = smb.cleanup_record.linux_daemon_pid {
                        child_pids.push(pid);
                    }
                    if smb.port == 445 {
                        endpoints.push(AccessEndpoint {
                            protocol: "SMB (Primary)".to_string(),
                            target_os: "Windows 10/11 Explorer".to_string(),
                            address: smb.access_unc_windows.clone(),
                            instructions: "Open File Explorer and paste into the address bar"
                                .to_string(),
                        });
                        endpoints.push(AccessEndpoint {
                            protocol: "SMB (Primary)".to_string(),
                            target_os: "Linux (Fedora / Ubuntu / Mint)".to_string(),
                            address: smb.access_uri_linux.clone(),
                            instructions: "Open Files (Nautilus / Nemo) -> Other Locations -> Connect to Server".to_string(),
                        });
                        endpoints.push(AccessEndpoint {
                            protocol: "SMB (Primary)".to_string(),
                            target_os: "Android / Mobile File Apps".to_string(),
                            address: format!("{}:445 (Share: {})", iface.ipv4, smb.share_name),
                            instructions: "Add Network Drive -> SMB -> Port 445 -> Anonymous".to_string(),
                        });
                    } else {
                        endpoints.push(AccessEndpoint {
                            protocol: format!("SMB (Port {})", smb.port),
                            target_os: "Linux / Samsung My Files".to_string(),
                            address: smb.access_uri_linux.clone(),
                            instructions: format!("Direct share URL; in Samsung My Files set Port {} (or run LabDrop with sudo for port 445)", smb.port),
                        });
                        warnings.push(format!(
                            "SMB is running in rootless mode on port {} (without share browsing due to system AppArmor/firewall). Apps locked to port 445 (Windows Explorer / File Manager Plus) should use WebDAV / HTTP below, or run LabDrop as root (`sudo`) for port 445.",
                            smb.port
                        ));
                    }
                    smb_share = Some(smb);
                }
                Err(e) => {
                    if strategy == SharingStrategy::SmbOnly {
                        return Err(e);
                    }
                    warnings.push(format!(
                        "Primary SMB share unavailable ({e}); automatically activated Secondary WebDAV + HTTP Fallback."
                    ));
                }
            }
        }

        // 2. Start Secondary (WebDAV) and/or Last Fallback (HTTP)
        let server_mode_opt = match strategy {
            SharingStrategy::AutoLayered | SharingStrategy::WebDavSecondary => {
                Some(EmbeddedServerMode::WebDavAndHttp)
            }
            SharingStrategy::HttpFallback => Some(EmbeddedServerMode::HttpOnly),
            SharingStrategy::SmbOnly => None,
        };

        if let Some(srv_mode) = server_mode_opt {
            let srv = EmbeddedFileServer::start(
                iface.ipv4,
                preferred_http_port,
                srv_mode,
                staged_dir.clone(),
                Some(manifest.clone()),
                Arc::clone(&tracker),
            )?;
            let port = srv.port;

            if srv_mode == EmbeddedServerMode::WebDavAndHttp {
                active_protocols.push(format!("WebDAV RFC 4918 (Port {port})"));
                endpoints.push(AccessEndpoint {
                    protocol: "WebDAV (Secondary)".to_string(),
                    target_os: "Linux (Fedora / Ubuntu / Mint)".to_string(),
                    address: format!("dav://{}:{port}/LabDrop/", iface.ipv4),
                    instructions: "Paste into Files / Nautilus / Nemo 'Connect to Server' (no browser needed)".to_string(),
                });
                endpoints.push(AccessEndpoint {
                    protocol: "WebDAV (Secondary)".to_string(),
                    target_os: "Windows 10/11 Explorer".to_string(),
                    address: format!("\\\\{}@{port}\\DavWWWRoot\\LabDrop", iface.ipv4),
                    instructions: "Paste into File Explorer address bar or `net use * http://<ip>:<port>/LabDrop`".to_string(),
                });
                endpoints.push(AccessEndpoint {
                    protocol: "WebDAV (Mobile)".to_string(),
                    target_os: "Android (File Manager Plus)".to_string(),
                    address: format!("http://{}:{port}/LabDrop/", iface.ipv4),
                    instructions: "Remote -> Add WebDAV -> Host/URL above -> Anonymous (works without root!)".to_string(),
                });
            }

            active_protocols.push(format!("HTTP Fallback (Port {port})"));
            let (_, mdns_host) = crate::network::get_local_hostname();
            endpoints.push(AccessEndpoint {
                protocol: "HTTP (Fallback)".to_string(),
                target_os: "Any Web Browser (Direct IP)".to_string(),
                address: format!("http://{}:{port}/", iface.ipv4),
                instructions: "Open in any browser to download individual files or full .ZIP bundle".to_string(),
            });
            endpoints.push(AccessEndpoint {
                protocol: "HTTP (mDNS .local)".to_string(),
                target_os: "LAN with mDNS / Avahi / Bonjour".to_string(),
                address: format!("http://{mdns_host}:{port}/"),
                instructions: "Optional hostname alias (use Direct IP above if institutional LAN blocks multicast)".to_string(),
            });

            embedded_server = Some(srv);
        }

        // 3. Start mDNS / Avahi advertiser for discovery
        let mdns_port = embedded_server
            .as_ref()
            .map(|s| s.port)
            .or_else(|| smb_share.as_ref().map(|s| s.port))
            .unwrap_or(8080);
        let mdns_type = if embedded_server.is_some() {
            "_http._tcp"
        } else {
            "_smb._tcp"
        };
        let mdns = MdnsAdvertiser::start(iface.ipv4, mdns_port, mdns_type);
        if let Some(avahi_pid) = mdns.avahi_pid() {
            child_pids.push(avahi_pid);
        }

        let ledger = SessionLedger {
            session_id,
            owner_pid: std::process::id(),
            created_epoch_secs: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            interface_name: iface.name.clone(),
            interface_ip: iface.ipv4.to_string(),
            session_dir: session_dir.clone(),
            staged_dir,
            registry_file: registry_file.clone(),
            smb_record: smb_share.as_ref().map(|s| s.cleanup_record.clone()),
            child_pids,
            active_protocols,
            endpoints,
            warnings,
        };

        // Persist ledger both in session_dir/session.json and registry_file
        let ledger_json = serde_json::to_string_pretty(&ledger)?;
        let local_session_file = session_dir.join("session.json");
        fs::write(&local_session_file, &ledger_json).map_err(|e| LabDropError::IoPath {
            path: local_session_file,
            source: e,
        })?;
        fs::write(&registry_file, &ledger_json).map_err(|e| LabDropError::IoPath {
            path: registry_file.clone(),
            source: e,
        })?;

        // 4. Spawn crash-recovery watchdog child process if requested
        let mut watchdog_stdin = None;
        let mut watchdog_child = None;
        if spawn_watchdog {
            if let Some((child, stdin)) = spawn_watchdog_child(&registry_file, ledger.owner_pid) {
                watchdog_stdin = Some(stdin);
                watchdog_child = Some(child);
            }
        }

        Ok(Self {
            ledger,
            manifest,
            tracker,
            smb_share,
            embedded_server,
            mdns: Some(mdns),
            watchdog_stdin,
            watchdog_child,
            cleaned_up: false,
        })
    }

    /// Stops all sharing services and performs full session cleanup.
    pub fn stop_and_cleanup(&mut self, delete_staged_files: bool) -> CleanupReport {
        if self.cleaned_up {
            return CleanupReport {
                session_id: self.ledger.session_id.clone(),
                actions_performed: vec!["Session was already cleaned up.".to_string()],
                staged_files_deleted: delete_staged_files,
                retained_staged_dir: if delete_staged_files {
                    None
                } else {
                    Some(self.ledger.staged_dir.clone())
                },
            };
        }
        self.cleaned_up = true;

        // Remove registry file FIRST so the watchdog knows this is an intentional, clean shutdown
        let _ = fs::remove_file(&self.ledger.registry_file);
        let _ = fs::remove_file(self.ledger.session_dir.join("session.json"));

        // Stop in-process servers and monitors
        if let Some(mut srv) = self.embedded_server.take() {
            srv.stop();
        }
        if let Some(mut mdns) = self.mdns.take() {
            mdns.stop();
        }
        if let Some(mut smb) = self.smb_share.take() {
            smb.stop_in_process_monitors();
        }

        // Perform OS-level cleanup from the ledger
        let report = cleanup_session_ledger(&self.ledger, delete_staged_files);

        // Close watchdog pipe and wait for watchdog to exit
        drop(self.watchdog_stdin.take());
        if let Some(mut wd) = self.watchdog_child.take() {
            let _ = wd.kill();
            let _ = wd.wait();
        }

        report
    }
}

impl Drop for ActiveSharingSession {
    fn drop(&mut self) {
        if !self.cleaned_up {
            let _ = self.stop_and_cleanup(true);
        }
    }
}

/// Returns the registry directory used to track active sessions for crash recovery.
pub fn session_registry_dir(session_dir: &Path) -> PathBuf {
    session_dir
        .parent()
        .unwrap_or_else(|| Path::new("/var/tmp"))
        .join("registry")
}

/// Executes deterministic cleanup of all resources recorded in `ledger`.
pub fn cleanup_session_ledger(ledger: &SessionLedger, delete_staged_files: bool) -> CleanupReport {
    let mut actions = Vec::new();

    // 1. Clean up SMB share, config blocks, and firewall rules
    if let Some(ref smb_rec) = ledger.smb_record {
        let mut smb_actions = cleanup_smb_record(smb_rec);
        actions.append(&mut smb_actions);
    }

    // 2. Terminate any remaining session child processes (e.g. avahi-publish, smbd)
    for &pid in &ledger.child_pids {
        if is_pid_alive(pid) {
            kill_pid_gracefully(pid);
            actions.push(format!("Stopped helper process (PID {pid})"));
        }
    }

    // 3. Remove registry file and session.json
    if ledger.registry_file.exists() && fs::remove_file(&ledger.registry_file).is_ok() {
        actions.push("Removed session ownership ledger".to_string());
    }
    let local_json = ledger.session_dir.join("session.json");
    let _ = fs::remove_file(local_json);

    // 4. Delete or retain staged files according to user preference
    let mut retained_staged_dir = None;
    if delete_staged_files {
        if ledger.session_dir.exists() {
            if fs::remove_dir_all(&ledger.session_dir).is_ok() {
                actions.push(format!(
                    "Deleted temporary staging directory '{}'",
                    ledger.session_dir.display()
                ));
            } else if ledger.staged_dir.exists() && fs::remove_dir_all(&ledger.staged_dir).is_ok() {
                actions.push(format!(
                    "Deleted staged files '{}'",
                    ledger.staged_dir.display()
                ));
            }
        }
    } else {
        // Remove temporary samba runtime directories while preserving `staged_dir`
        for sub in ["samba_run", "samba_lock", "samba_log", "samba_priv"] {
            let p = ledger.session_dir.join(sub);
            let _ = fs::remove_dir_all(p);
        }
        actions.push(format!(
            "Retained staged files at '{}'",
            ledger.staged_dir.display()
        ));
        retained_staged_dir = Some(ledger.staged_dir.clone());
    }

    if actions.is_empty() {
        actions.push("Stopped embedded network listeners and released ports".to_string());
    }

    CleanupReport {
        session_id: ledger.session_id.clone(),
        actions_performed: actions,
        staged_files_deleted: delete_staged_files,
        retained_staged_dir,
    }
}

/// Scans `base_staging_dir/registry` (or default staging base dir) for orphaned sessions whose
/// owner PID is no longer alive, and cleans them up safely.
pub fn recover_orphaned_sessions(custom_base_dir: Option<&Path>) -> Vec<CleanupReport> {
    let base = custom_base_dir
        .map(|p| p.to_path_buf())
        .unwrap_or_else(default_staging_base_dir);
    let registry_dir = base.join("registry");
    let mut reports = Vec::new();

    let Ok(entries) = fs::read_dir(&registry_dir) else {
        return reports;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(ledger) = serde_json::from_str::<SessionLedger>(&content) else {
            let _ = fs::remove_file(&path);
            continue;
        };

        if !is_pid_alive(ledger.owner_pid) {
            let mut report = cleanup_session_ledger(&ledger, true);
            report.actions_performed.insert(
                0,
                format!(
                    "Recovered crashed/abandoned session '{}' (dead owner PID {})",
                    ledger.session_id, ledger.owner_pid
                ),
            );
            reports.push(report);
        }
    }

    reports
}

/// Checks whether standard input (the controlling terminal / PTY) has been closed or disconnected.
/// Used by TUI event loops to exit cleanly and tear down Samba/WebDAV/HTTP when a terminal window
/// closes without delivering `SIGHUP`.
pub fn is_terminal_disconnected() -> bool {
    #[cfg(unix)]
    {
        let mut pfd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, 0) };
        if rc > 0 && (pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)) != 0 {
            return true;
        }
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(libc::STDIN_FILENO, &mut st) } == 0 && st.st_nlink == 0 {
            return true;
        }
        false
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Checks if a process with `pid` is currently alive on the host OS.
pub fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        let rc = unsafe { libc::kill(pid as i32, 0) };
        if rc == 0 {
            // Also check it is not a zombie ('Z') in /proc/<pid>/stat
            if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
                if let Some(rparen) = stat.rfind(')') {
                    let after = stat[rparen + 1..].trim_start();
                    if after.starts_with('Z') {
                        return false;
                    }
                }
            }
            true
        } else {
            std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        }
    }
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> *mut std::ffi::c_void;
            fn GetExitCodeProcess(hProcess: *mut std::ffi::c_void, lpExitCode: *mut u32) -> i32;
            fn CloseHandle(hObject: *mut std::ffi::c_void) -> i32;
        }
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        const STILL_ACTIVE: u32 = 259;

        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut exit_code: u32 = 0;
            let ok = GetExitCodeProcess(handle, &mut exit_code);
            let _ = CloseHandle(handle);
            ok != 0 && exit_code == STILL_ACTIVE
        }
    }
}

fn spawn_watchdog_child(registry_file: &Path, owner_pid: u32) -> Option<(Child, ChildStdin)> {
    let exe = std::env::current_exe().ok()?;
    // Only spawn if current_exe is our main binary (not cargo test runner unless it supports --internal-watchdog)
    let exe_name = exe.file_name()?.to_string_lossy().to_lowercase();
    if !exe_name.starts_with("labdrop") || exe_name.contains("bootstrapper") || exe_name.contains('-')
    {
        return None;
    }

    let mut cmd = Command::new(exe);
    cmd.args([
        "--internal-watchdog",
        &registry_file.to_string_lossy(),
        &owner_pid.to_string(),
    ])
    // Ensure watchdog never holds a removable USB drive as its working directory
    .current_dir(std::env::temp_dir())
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::null());

    // Detach watchdog into its own session so terminal SIGHUP does not kill it mid-cleanup
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    let mut child = cmd.spawn().ok()?;
    let stdin = child.stdin.take()?;
    Some((child, stdin))
}

/// Entry point for the detached crash watchdog process (`labdrop --internal-watchdog <registry_file> <parent_pid>`).
pub fn run_watchdog_process(registry_file: &Path, parent_pid: u32) {
    let _ = std::env::set_current_dir(std::env::temp_dir());
    // Wait until stdin closes (pipe EOF on parent exit or SIGKILL)
    let mut buf = [0u8; 16];
    let _ = std::io::stdin().read(&mut buf);

    // Wait briefly for parent PID to disappear or finish clean shutdown
    for _ in 0..10 {
        if !is_pid_alive(parent_pid) || !registry_file.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    // Give a small grace window in case normal cleanup is mid-flight
    thread::sleep(Duration::from_millis(100));

    if registry_file.exists() {
        if let Ok(content) = fs::read_to_string(registry_file) {
            if let Ok(ledger) = serde_json::from_str::<SessionLedger>(&content) {
                let _ = cleanup_session_ledger(&ledger, true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn simulated_crashed_session_should_be_recovered_and_cleaned_up() {
        let tmp = TempDir::new().unwrap();
        let base_dir = tmp.path().join("sessions");
        let sess_dir = base_dir.join("sess-crash-01");
        let staged_dir = sess_dir.join("staged");
        let reg_dir = base_dir.join("registry");
        fs::create_dir_all(&staged_dir).unwrap();
        fs::create_dir_all(&reg_dir).unwrap();
        fs::write(staged_dir.join("secret.bin"), b"temporary staged data").unwrap();

        let reg_file = reg_dir.join("sess-crash-01.json");
        // Use a guaranteed non-existent PID (999_999_999) to simulate a crashed process
        let ledger = SessionLedger {
            session_id: "sess-crash-01".to_string(),
            owner_pid: 999_999_999,
            created_epoch_secs: 100,
            interface_name: "lo".to_string(),
            interface_ip: "127.0.0.1".to_string(),
            session_dir: sess_dir.clone(),
            staged_dir,
            registry_file: reg_file.clone(),
            smb_record: None,
            child_pids: Vec::new(),
            active_protocols: vec!["HTTP".to_string()],
            endpoints: Vec::new(),
            warnings: Vec::new(),
        };
        fs::write(&reg_file, serde_json::to_string_pretty(&ledger).unwrap()).unwrap();

        let reports = recover_orphaned_sessions(Some(&base_dir));
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].session_id, "sess-crash-01");
        assert!(reports[0].staged_files_deleted);
        assert!(!sess_dir.exists());
        assert!(!reg_file.exists());
    }
}
