use crate::discovery::{DiscoveryConfig, NetworkDiscovery};
use crate::error::{LabDropError, Result};
use crate::http_webdav::{EmbeddedFileServer, EmbeddedServerMode};
use crate::i18n::Lang;
use crate::network::{get_local_hostname, MdnsAdvertiser, MdnsServiceSpec, NetworkInterface};
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
        self.display_name_localized(Lang::En)
    }

    pub fn display_name_localized(&self, lang: Lang) -> &'static str {
        match (self, lang) {
            (SharingStrategy::AutoLayered, Lang::En) => {
                "Auto Layered (SMB + WebDAV + HTTP)"
            }
            (SharingStrategy::AutoLayered, Lang::PtBr) => {
                "Automático em Camadas (SMB + WebDAV + HTTP)"
            }
            (SharingStrategy::SmbOnly, Lang::En) => "SMB Only (Samba / Windows Native SMB)",
            (SharingStrategy::SmbOnly, Lang::PtBr) => "Apenas SMB (Samba / SMB Nativo Windows)",
            (SharingStrategy::WebDavSecondary, Lang::En) => {
                "WebDAV + HTTP (Native File Manager without Root)"
            }
            (SharingStrategy::WebDavSecondary, Lang::PtBr) => {
                "WebDAV + HTTP (Gerenciador Nativo sem Root)"
            }
            (SharingStrategy::HttpFallback, Lang::En) => "HTTP Browser Only (Browser + .ZIP)",
            (SharingStrategy::HttpFallback, Lang::PtBr) => "Apenas Navegador HTTP (Browser + .ZIP)",
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
    discovery: Option<NetworkDiscovery>,
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

        let (short_host, mdns_host) = get_local_hostname();
        let mut discovery_cfg = DiscoveryConfig {
            session_id: session_id.clone(),
            device_uuid: DiscoveryConfig::deterministic_uuid(&session_id),
            hostname: short_host.clone(),
            interface_ip: iface.ipv4,
            http_port: None,
            smb_port: smb_share.as_ref().map(|s| s.port),
            webdav_enabled: matches!(
                strategy,
                SharingStrategy::AutoLayered | SharingStrategy::WebDavSecondary
            ),
        };

        if let Some(srv_mode) = server_mode_opt {
            let srv = EmbeddedFileServer::start_with_discovery(
                iface.ipv4,
                preferred_http_port,
                srv_mode,
                staged_dir.clone(),
                Some(manifest.clone()),
                Arc::clone(&tracker),
                Some(discovery_cfg.clone()),
            )?;
            let port = srv.port;
            discovery_cfg.http_port = Some(port);

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

        // 3. Start multi-protocol mDNS/Avahi DNS-SD + Windows SSDP/WSD/LLMNR Discovery
        let mut mdns_services = Vec::new();
        if let Some(ref smb) = smb_share {
            mdns_services.push(MdnsServiceSpec {
                service_type: "_smb._tcp".to_string(),
                port: smb.port,
                txt_path: format!("/{}", smb.share_name),
            });
        }
        if let Some(ref srv) = embedded_server {
            if srv.mode == EmbeddedServerMode::WebDavAndHttp {
                mdns_services.push(MdnsServiceSpec {
                    service_type: "_webdav._tcp".to_string(),
                    port: srv.port,
                    txt_path: "/LabDrop/".to_string(),
                });
            }
            mdns_services.push(MdnsServiceSpec {
                service_type: "_http._tcp".to_string(),
                port: srv.port,
                txt_path: "/".to_string(),
            });
        }

        let mdns = MdnsAdvertiser::start_services(iface.ipv4, &mdns_services);
        for avahi_pid in mdns.avahi_pids() {
            child_pids.push(avahi_pid);
        }

        let discovery = if !iface.ipv4.is_loopback() {
            let disc = NetworkDiscovery::start(discovery_cfg);
            if disc.ssdp_active {
                active_protocols.push("Windows Network Discovery (SSDP/UPnP)".to_string());
                endpoints.insert(
                    0,
                    AccessEndpoint {
                        protocol: "Auto-Discovery (Win)".to_string(),
                        target_os: "Windows 10/11 Explorer -> Network".to_string(),
                        address: format!("LabDrop ({short_host})"),
                        instructions: "Open File Explorer -> Network -> double-click under Other Devices (no IP needed)".to_string(),
                    },
                );
            }
            if disc.wsd_active {
                active_protocols.push("Windows Network Computers (WS-Discovery)".to_string());
                endpoints.insert(
                    0,
                    AccessEndpoint {
                        protocol: "Auto-Discovery (WSD)".to_string(),
                        target_os: "Windows 10/11 Explorer -> Network".to_string(),
                        address: format!("\\\\{}", short_host.to_uppercase()),
                        instructions: "Open File Explorer -> Network -> Computers -> double-click host".to_string(),
                    },
                );
            }
            Some(disc)
        } else {
            None
        };

        if !mdns_services.is_empty() {
            endpoints.push(AccessEndpoint {
                protocol: "Auto-Discovery (Linux)".to_string(),
                target_os: "Linux Files / Nautilus / Nemo".to_string(),
                address: format!("Network -> LabDrop ({short_host})"),
                instructions: "Open Files -> Other Locations -> click LabDrop SMB or WebDAV".to_string(),
            });
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
            discovery,
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

        // Stop in-process servers and discovery responders (sending SSDP byebye & WSD Bye)
        if let Some(mut disc) = self.discovery.take() {
            disc.stop();
        }
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

    /// Cleanly stops all active network listeners, SMB shares, and temporary firewall rules
    /// while preserving `staged_dir` and `manifest`, then restarts the sharing service on
    /// `new_iface` (and/or with updated `strategy` / `linux_smb_mode`).
    ///
    /// Guarantees that:
    /// - Old listeners and bindings are stopped before rebinding.
    /// - Old temporary firewall rules and SMB share definitions are cleaned up.
    /// - If starting on `new_iface` fails, no stale services or duplicate shares remain running.
    pub fn restart_on_interface(
        &mut self,
        new_iface: &NetworkInterface,
        strategy: SharingStrategy,
        linux_smb_mode: Option<SmbLinuxMode>,
        preferred_http_port: u16,
        spawn_watchdog: bool,
    ) -> Result<CleanupReport> {
        let old_iface_name = self.ledger.interface_name.clone();
        let old_iface_ip = self.ledger.interface_ip.clone();
        let manifest = self.manifest.clone();
        let session_dir = self.ledger.session_dir.clone();

        // 1. Stop current services while retaining staged_dir (`delete_staged_files = false`)
        let cleanup_report = self.stop_and_cleanup(false);

        // 2. Start a new session on `new_iface` reusing the existing `manifest` and `session_dir`
        let mut new_session = Self::start(
            manifest,
            session_dir,
            new_iface,
            strategy,
            linux_smb_mode,
            preferred_http_port,
            spawn_watchdog,
        )?;

        if old_iface_name != new_iface.name || old_iface_ip != new_iface.ipv4.to_string() {
            new_session.ledger.warnings.insert(
                0,
                format!(
                    "Sharing service switched from {} ({}) to {} ({}). Any clients connected to {} were disconnected and must use the updated address.",
                    old_iface_name, old_iface_ip, new_iface.name, new_iface.ipv4, old_iface_ip
                ),
            );
        } else {
            new_session.ledger.warnings.insert(
                0,
                format!(
                    "Sharing service restarted on {} ({}). Active client connections may have been briefly interrupted.",
                    new_iface.name, new_iface.ipv4
                ),
            );
        }

        // Update persisted ledger with the restart notice
        if let Ok(ledger_json) = serde_json::to_string_pretty(&new_session.ledger) {
            let _ = fs::write(new_session.ledger.session_dir.join("session.json"), &ledger_json);
            let _ = fs::write(&new_session.ledger.registry_file, &ledger_json);
        }

        *self = new_session;
        Ok(cleanup_report)
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

/// When `labdrop` or `labdrop-bootstrapper` is launched on a Linux desktop by double-clicking in a
/// file manager (Dolphin, Nautilus, Nemo, Thunar), `stdin` is not a TTY (`isatty(0) == 0`).
/// This helper detects that scenario and automatically re-launches the binary inside an available
/// desktop terminal emulator so the interactive TUI opens in a visible window.
/// Returns `true` if a terminal emulator was spawned (meaning the current headless process should exit `Ok(())`).
pub fn ensure_interactive_terminal_on_linux() -> bool {
    #[cfg(unix)]
    {
        let stdin_tty = unsafe { libc::isatty(libc::STDIN_FILENO) == 1 };
        let stdout_tty = unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 };
        if stdin_tty && stdout_tty {
            return false;
        }
        if std::env::var_os("LABDROP_TERMINAL_RELAUNCH").is_some()
            || std::env::var_os("CARGO").is_some()
            || std::env::var_os("RUST_TEST_THREADS").is_some()
        {
            return false;
        }
        let has_gui = std::env::var_os("DISPLAY").is_some()
            || std::env::var_os("WAYLAND_DISPLAY").is_some();
        if !has_gui {
            return false;
        }
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        let extra_args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        let cwd = exe
            .parent()
            .map(|p| p.to_path_buf())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(std::env::temp_dir);

        // Candidate desktop terminal emulators and the flag used before the command
        let terminals: &[(&str, &[&str])] = &[
            ("ptyxis", &["--"]),
            ("kgx", &["-e"]),
            ("gnome-terminal", &["--"]),
            ("konsole", &["-e"]),
            ("xfce4-terminal", &["-x"]),
            ("mate-terminal", &["-x"]),
            ("tilix", &["-e"]),
            ("alacritty", &["-e"]),
            ("kitty", &[]),
            ("wezterm", &["start", "--"]),
            ("foot", &[]),
            ("x-terminal-emulator", &["-e"]),
            ("xterm", &["-e"]),
        ];

        for (term_bin, flag_args) in terminals {
            let mut cmd = Command::new(term_bin);
            cmd.args(*flag_args)
                .arg(&exe)
                .args(&extra_args)
                .current_dir(&cwd)
                .env("LABDROP_TERMINAL_RELAUNCH", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if cmd.spawn().is_ok() {
                return true;
            }
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

    #[test]
    fn restart_on_interface_stops_old_listener_preserves_staged_files_and_binds_new_interface() {
        use crate::staging::{stage_selection, SelectionSet};
        use std::net::{Ipv4Addr, TcpStream};

        let tmp = TempDir::new().unwrap();
        let src_file = tmp.path().join("installer.bin");
        fs::write(&src_file, b"labdrop-switch-interface-payload").unwrap();

        let mut sel = SelectionSet::new();
        sel.add_path(&src_file).unwrap();

        let session_dir = tmp.path().join("sess-switch");
        let staged_dir = session_dir.join("staged");
        let manifest = stage_selection("sess-switch", &sel, &staged_dir, None, None, |_| {}).unwrap();

        let iface_eth = NetworkInterface {
            name: "eth0".to_string(),
            ipv4: Ipv4Addr::LOCALHOST,
            prefix_len: 8,
            subnet_cidr: "127.0.0.0/8".to_string(),
            is_loopback: true,
            is_wireless: false,
            is_virtual: false,
            is_default_route: true,
        };

        let mut sess = ActiveSharingSession::start(
            manifest,
            session_dir,
            &iface_eth,
            SharingStrategy::WebDavSecondary,
            None,
            0,
            false,
        )
        .unwrap();

        let old_port = sess.embedded_server.as_ref().unwrap().port;
        assert!(TcpStream::connect(("127.0.0.1", old_port)).is_ok());

        // Simulate switching to Wi-Fi interface and restarting the sharing service
        let iface_wifi = NetworkInterface {
            name: "wlan0".to_string(),
            ipv4: Ipv4Addr::LOCALHOST,
            prefix_len: 8,
            subnet_cidr: "127.0.0.0/8".to_string(),
            is_loopback: true,
            is_wireless: true,
            is_virtual: false,
            is_default_route: false,
        };

        let cleanup_rep = sess
            .restart_on_interface(
                &iface_wifi,
                SharingStrategy::WebDavSecondary,
                None,
                0,
                false,
            )
            .unwrap();

        assert!(!cleanup_rep.staged_files_deleted);
        assert!(staged_dir.join("installer.bin").is_file());
        assert_eq!(sess.ledger.interface_name, "wlan0");
        assert!(sess
            .ledger
            .warnings
            .iter()
            .any(|w| w.contains("eth0") && w.contains("wlan0")));

        let new_port = sess.embedded_server.as_ref().unwrap().port;
        assert!(TcpStream::connect(("127.0.0.1", new_port)).is_ok());

        let final_rep = sess.stop_and_cleanup(true);
        assert!(final_rep.staged_files_deleted);
        assert!(!staged_dir.exists());
    }
}
