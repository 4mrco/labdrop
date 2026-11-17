use crate::error::{LabDropError, Result};
use crate::network::NetworkInterface;
use crate::transfer::{ProtocolKind, TransferTracker};
use serde::{Deserialize, Serialize};
use std::fs;
#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::io::{BufRead, BufReader};
use std::net::Ipv4Addr;
#[cfg(unix)]
use std::net::{SocketAddrV4, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
#[cfg(unix)]
use std::thread;
use std::thread::JoinHandle;
#[cfg(unix)]
use std::time::Duration;

/// Detected Linux distribution family for Samba service and package management.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinuxDistroFamily {
    FedoraRhel,
    UbuntuDebianMint,
    Other(String),
}

impl LinuxDistroFamily {
    pub fn display_name(&self) -> &str {
        match self {
            LinuxDistroFamily::FedoraRhel => "Fedora / RHEL",
            LinuxDistroFamily::UbuntuDebianMint => "Ubuntu / Linux Mint / Debian",
            LinuxDistroFamily::Other(s) => s.as_str(),
        }
    }

    pub fn samba_service_name(&self) -> &'static str {
        match self {
            LinuxDistroFamily::FedoraRhel => "smb.service",
            LinuxDistroFamily::UbuntuDebianMint => "smbd.service",
            LinuxDistroFamily::Other(_) => "smbd.service",
        }
    }

    pub fn install_command(&self) -> &'static str {
        match self {
            LinuxDistroFamily::FedoraRhel => "sudo dnf install -y samba samba-common",
            LinuxDistroFamily::UbuntuDebianMint => {
                "sudo apt-get update && sudo apt-get install -y samba smbclient"
            }
            LinuxDistroFamily::Other(_) => "sudo dnf install -y samba || sudo apt-get install -y samba",
        }
    }
}

/// Diagnostic snapshot of SMB / Samba capabilities on the transmitting computer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmbEnvironmentStatus {
    pub os_family: String,
    pub linux_distro: Option<LinuxDistroFamily>,
    pub smbd_installed: bool,
    pub smbd_version: Option<String>,
    pub testparm_installed: bool,
    pub service_name: String,
    pub service_active: bool,
    pub is_elevated: bool,
    pub rootless_unshare_available: bool,
    pub firewall_backend: String,
    pub selinux_enforcing: bool,
}

/// Linux Samba operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SmbLinuxMode {
    /// Standard port 445 via system Samba configuration (requires root/sudo; works with Windows & Linux).
    SystemPort445,
    /// Unprivileged isolated Samba `smbd` via `unshare --map-users=auto` on custom port (default 4445; zero root required).
    RootlessPort(u16),
}

/// Persistent cleanup metadata for an SMB share created by LabDrop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmbCleanupRecord {
    pub share_name: String,
    pub port: u16,
    pub is_windows: bool,
    pub linux_config_marker_id: Option<String>,
    pub linux_include_file: Option<PathBuf>,
    pub linux_service_started_by_labdrop: Option<String>,
    pub linux_daemon_pid: Option<u32>,
    #[serde(default)]
    pub linux_daemon_pgid: Option<u32>,
    pub linux_pid_file: Option<PathBuf>,
    pub firewall_rule_id: Option<String>,
    pub windows_firewall_rule_name: Option<String>,
}

/// Active SMB session handle.
pub struct ActiveSmbShare {
    pub share_name: String,
    pub port: u16,
    pub access_unc_windows: String,
    pub access_uri_linux: String,
    pub mode_description: String,
    pub cleanup_record: SmbCleanupRecord,
    child_process: Option<Child>,
    log_stop_flag: Arc<AtomicBool>,
    log_thread: Option<JoinHandle<()>>,
}

impl ActiveSmbShare {
    pub fn stop_in_process_monitors(&mut self) {
        self.log_stop_flag.store(true, Ordering::Relaxed);
        if let Some(handle) = self.log_thread.take() {
            let _ = handle.join();
        }
        if let Some(pgid) = self.cleanup_record.linux_daemon_pgid {
            kill_process_group_gracefully(pgid);
        }
        if let Some(mut child) = self.child_process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Detects the Linux distribution family from `/etc/os-release`.
pub fn detect_linux_distro() -> LinuxDistroFamily {
    let Ok(content) = fs::read_to_string("/etc/os-release") else {
        return LinuxDistroFamily::Other("Linux".to_string());
    };
    let lower = content.to_lowercase();
    if lower.contains("id=fedora")
        || lower.contains("id=\"fedora\"")
        || lower.contains("id=nobara")
        || lower.contains("id_like=\"fedora\"")
        || lower.contains("fedora")
    {
        LinuxDistroFamily::FedoraRhel
    } else if lower.contains("ubuntu")
        || lower.contains("linuxmint")
        || lower.contains("debian")
    {
        LinuxDistroFamily::UbuntuDebianMint
    } else {
        LinuxDistroFamily::Other("Linux".to_string())
    }
}

/// Checks whether the current process has root (Unix) or Administrator (Windows) privileges.
pub fn is_process_elevated() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(windows)]
    {
        Command::new("net")
            .arg("session")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

/// Probes the transmitting machine's SMB / Samba environment.
pub fn probe_smb_environment() -> SmbEnvironmentStatus {
    #[cfg(unix)]
    {
        let distro = detect_linux_distro();
        let smbd_out = Command::new("smbd")
            .arg("--version")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok();
        let smbd_installed = smbd_out.as_ref().map(|o| o.status.success()).unwrap_or(false);
        let smbd_version = smbd_out.and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        });

        let testparm_installed = Command::new("testparm")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        let svc_name = distro.samba_service_name().to_string();
        let service_active = Command::new("systemctl")
            .args(["is-active", "--quiet", &svc_name])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        let rootless_unshare_available = Command::new("unshare")
            .args([
                "--user",
                "--map-root-user",
                "--map-users=auto",
                "--map-groups=auto",
                "true",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        let firewall_backend = if Command::new("firewall-cmd")
            .arg("--state")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            "firewalld (active)".to_string()
        } else if Command::new("ufw")
            .arg("status")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("Status: active"))
            .unwrap_or(false)
        {
            "ufw (active)".to_string()
        } else {
            "none / inactive".to_string()
        };

        let selinux_enforcing = Command::new("getenforce")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .eq_ignore_ascii_case("enforcing")
            })
            .unwrap_or(false);

        SmbEnvironmentStatus {
            os_family: "Linux".to_string(),
            linux_distro: Some(distro),
            smbd_installed,
            smbd_version,
            testparm_installed,
            service_name: svc_name,
            service_active,
            is_elevated: is_process_elevated(),
            rootless_unshare_available,
            firewall_backend,
            selinux_enforcing,
        }
    }

    #[cfg(windows)]
    {
        SmbEnvironmentStatus {
            os_family: "Windows".to_string(),
            linux_distro: None,
            smbd_installed: true,
            smbd_version: Some("Windows Native LanmanServer SMB".to_string()),
            testparm_installed: true,
            service_name: "LanmanServer".to_string(),
            service_active: true,
            is_elevated: is_process_elevated(),
            rootless_unshare_available: false,
            firewall_backend: "Windows Defender Firewall (netsh advfirewall)".to_string(),
            selinux_enforcing: false,
        }
    }
}

/// Offers to install Samba on Linux using the distribution's native package manager (`dnf` or `apt-get`).
pub fn install_linux_samba_interactive() -> Result<()> {
    let distro = detect_linux_distro();
    let status = match distro {
        LinuxDistroFamily::FedoraRhel => {
            if is_process_elevated() {
                Command::new("dnf")
                    .args(["install", "-y", "samba", "samba-common"])
                    .status()
            } else {
                Command::new("sudo")
                    .args(["dnf", "install", "-y", "samba", "samba-common"])
                    .status()
            }
        }
        LinuxDistroFamily::UbuntuDebianMint | LinuxDistroFamily::Other(_) => {
            if is_process_elevated() {
                let _ = Command::new("apt-get").arg("update").status();
                Command::new("apt-get")
                    .args(["install", "-y", "samba", "smbclient"])
                    .status()
            } else {
                let _ = Command::new("sudo").args(["apt-get", "update"]).status();
                Command::new("sudo")
                    .args(["apt-get", "install", "-y", "samba", "smbclient"])
                    .status()
            }
        }
    }
    .map_err(LabDropError::Io)?;

    if status.success() {
        Ok(())
    } else {
        Err(LabDropError::Smb(format!(
            "Package installation exited with status: {status}"
        )))
    }
}

/// Starts an SMB share for `staged_dir` on the selected network interface.
pub fn start_smb_share(
    session_id: &str,
    session_dir: &Path,
    staged_dir: &Path,
    iface: &NetworkInterface,
    linux_mode_pref: Option<SmbLinuxMode>,
    tracker: Arc<TransferTracker>,
) -> Result<ActiveSmbShare> {
    #[cfg(unix)]
    {
        let env = probe_smb_environment();
        if !env.smbd_installed {
            let install_hint = env
                .linux_distro
                .as_ref()
                .map(|d| d.install_command())
                .unwrap_or("sudo dnf install -y samba");
            return Err(LabDropError::Smb(format!(
                "Samba (smbd) is not installed on this Linux system. Install it with: `{install_hint}`"
            )));
        }

        let mode = match linux_mode_pref {
            Some(m) => m,
            None => {
                if env.is_elevated {
                    SmbLinuxMode::SystemPort445
                } else if env.rootless_unshare_available {
                    SmbLinuxMode::RootlessPort(4445)
                } else {
                    SmbLinuxMode::SystemPort445
                }
            }
        };

        match mode {
            SmbLinuxMode::SystemPort445 => {
                start_linux_system_smb(session_id, session_dir, staged_dir, iface, &env, tracker)
            }
            SmbLinuxMode::RootlessPort(port) => start_linux_rootless_smb(
                session_id,
                session_dir,
                staged_dir,
                iface,
                port,
                tracker,
            ),
        }
    }

    #[cfg(windows)]
    {
        let _ = (session_dir, linux_mode_pref);
        start_windows_smb_share(session_id, staged_dir, iface, tracker)
    }
}

#[cfg(unix)]
fn start_linux_rootless_smb(
    session_id: &str,
    session_dir: &Path,
    staged_dir: &Path,
    iface: &NetworkInterface,
    preferred_port: u16,
    tracker: Arc<TransferTracker>,
) -> Result<ActiveSmbShare> {
    let run_dir = session_dir.join("samba_run");
    let lock_dir = session_dir.join("samba_lock");
    let log_dir = session_dir.join("samba_log");
    let priv_dir = session_dir.join("samba_priv");
    let ncalrpc_dir = run_dir.join("ncalrpc");

    for d in [&run_dir, &lock_dir, &log_dir, &priv_dir, &ncalrpc_dir] {
        fs::create_dir_all(d).map_err(|e| LabDropError::IoPath {
            path: d.clone(),
            source: e,
        })?;
    }

    let log_file = log_dir.join("smbd.log");
    let pid_file = run_dir.join("smbd.pid");
    let conf_file = session_dir.join("smb.rootless.conf");
    let share_name = "LabDrop".to_string();

    let interfaces_line = if iface.ipv4.is_loopback() {
        "127.0.0.1".to_string()
    } else {
        format!("127.0.0.1 {}", iface.ipv4)
    };

    let hosts_allow = if iface.ipv4.is_loopback() {
        "127.0.0.1 ::1".to_string()
    } else {
        format!("127.0.0.1 ::1 {}", iface.subnet_cidr)
    };

    let conf_content = format!(
        "[global]\n\
         workgroup = WORKGROUP\n\
         server string = LabDrop SMB ({session_id})\n\
         security = user\n\
         map to guest = Bad User\n\
         guest account = root\n\
         restrict anonymous = 0\n\
         server min protocol = SMB2_02\n\
         interfaces = {interfaces_line}\n\
         bind interfaces only = yes\n\
         smb ports = {preferred_port}\n\
         disable netbios = yes\n\
         multicast dns register = no\n\
         pid directory = {run}\n\
         lock directory = {lock}\n\
         state directory = {lock}\n\
         cache directory = {lock}\n\
         private dir = {priv_d}\n\
         ncalrpc dir = {ncalrpc}\n\
         log file = {log}\n\
         log level = 2\n\
         max log size = 5000\n\
         load printers = no\n\
         printing = bsd\n\
         printcap name = /dev/null\n\
         disable spoolss = yes\n\n\
         [{share_name}]\n\
         comment = LabDrop Temporary File Distribution\n\
         path = {staged}\n\
         browseable = yes\n\
         read only = yes\n\
         writable = no\n\
         guest ok = yes\n\
         public = yes\n\
         force user = root\n\
         hosts allow = {hosts_allow}\n\
         hosts deny = ALL\n",
        run = run_dir.display(),
        lock = lock_dir.display(),
        priv_d = priv_dir.display(),
        ncalrpc = ncalrpc_dir.display(),
        log = log_file.display(),
        staged = staged_dir.display(),
    );

    fs::write(&conf_file, &conf_content).map_err(|e| LabDropError::IoPath {
        path: conf_file.clone(),
        source: e,
    })?;

    // Validate with testparm if installed
    if let Ok(tp) = Command::new("testparm")
        .args(["-s", &conf_file.to_string_lossy()])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
    {
        if !tp.status.success() {
            let err_txt = String::from_utf8_lossy(&tp.stderr);
            return Err(LabDropError::Smb(format!(
                "Samba config validation (testparm) failed: {err_txt}"
            )));
        }
    }

    let shell_cmd = format!(
        "exec </dev/null; exec smbd -F --no-process-group --configfile='{}'",
        conf_file.display()
    );
    let mut cmd = Command::new("unshare");
    cmd.args([
        "--user",
        "--map-root-user",
        "--map-users=auto",
        "--map-groups=auto",
        "sh",
        "-c",
        &shell_cmd,
    ])
    // Ensure Samba never inherits a removable USB mount as its working directory
    .current_dir(session_dir)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());

    // Place unshare + smbd + smbd-notifyd + smbd-cleanupd in an isolated session/process group
    // so cleanup kills the entire process tree via killpg(-pgid).
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    let mut child = cmd.spawn().map_err(|e| {
        LabDropError::Smb(format!("Failed to launch rootless smbd via unshare: {e}"))
    })?;

    // Wait up to 3 seconds for TCP port to accept connections
    let probe_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, preferred_port);
    let mut bound_ok = false;
    for _ in 0..30 {
        if TcpStream::connect_timeout(&probe_addr.into(), Duration::from_millis(100)).is_ok() {
            bound_ok = true;
            break;
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(LabDropError::Smb(format!(
                "Rootless smbd exited prematurely with status {status}"
            )));
        }
        thread::sleep(Duration::from_millis(100));
    }

    if !bound_ok {
        kill_process_group_gracefully(child.id());
        let _ = child.kill();
        let _ = child.wait();
        return Err(LabDropError::Smb(format!(
            "Timed out waiting for Samba smbd to listen on {}:{}",
            iface.ipv4, preferred_port
        )));
    }

    let child_pid = child.id();
    let log_stop_flag = Arc::new(AtomicBool::new(false));
    let log_thread = Some(spawn_samba_log_tailer(
        log_file,
        Arc::clone(&log_stop_flag),
        tracker,
    ));

    let cleanup_record = SmbCleanupRecord {
        share_name: share_name.clone(),
        port: preferred_port,
        is_windows: false,
        linux_config_marker_id: None,
        linux_include_file: Some(conf_file),
        linux_service_started_by_labdrop: None,
        linux_daemon_pid: Some(child_pid),
        linux_daemon_pgid: Some(child_pid),
        linux_pid_file: Some(pid_file),
        firewall_rule_id: None,
        windows_firewall_rule_name: None,
    };

    Ok(ActiveSmbShare {
        access_unc_windows: format!("\\\\{}\\{share_name} (port {preferred_port})", iface.ipv4),
        access_uri_linux: format!("smb://{}:{preferred_port}/{share_name}", iface.ipv4),
        share_name,
        port: preferred_port,
        mode_description: format!(
            "Samba Isolated User-Namespace Daemon (Port {preferred_port}, Read-Only Guest)"
        ),
        cleanup_record,
        child_process: Some(child),
        log_stop_flag,
        log_thread,
    })
}

#[cfg(unix)]
fn start_linux_system_smb(
    session_id: &str,
    session_dir: &Path,
    staged_dir: &Path,
    iface: &NetworkInterface,
    env: &SmbEnvironmentStatus,
    tracker: Arc<TransferTracker>,
) -> Result<ActiveSmbShare> {
    if !env.is_elevated {
        return Err(LabDropError::ElevationRequired(
            "Configuring system Samba on standard TCP port 445 and opening the session firewall rule requires administrator privileges (root/sudo).".to_string(),
        ));
    }

    let smb_conf_path = Path::new("/etc/samba/smb.conf");
    let existing_conf = fs::read_to_string(smb_conf_path).unwrap_or_default();

    // Never overwrite a pre-existing non-LabDrop [LabDrop] share
    let share_name = if existing_conf.contains("[LabDrop]")
        && !existing_conf.contains("BEGIN LABDROP SESSION")
    {
        let short = &session_id[..6.min(session_id.len())];
        format!("LabDrop_{short}")
    } else {
        "LabDrop".to_string()
    };

    // Relabel staged_dir for SELinux on Fedora/RHEL if SELinux is active
    if env.selinux_enforcing {
        let _ = Command::new("chcon")
            .args(["-Rt", "samba_share_t", &staged_dir.to_string_lossy()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    let log_dir = session_dir.join("samba_log");
    let _ = fs::create_dir_all(&log_dir);
    let log_file = log_dir.join("smbd.log");

    let hosts_allow = if iface.ipv4.is_loopback() {
        "127.0.0.1 ::1".to_string()
    } else {
        format!("127.0.0.1 ::1 {}", iface.subnet_cidr)
    };

    // Place include file directly in /etc/samba/ so AppArmor (`/etc/samba/* r`) permits
    // samba-dcerpcd & rpcd_classic to read it for share enumeration (`smbclient -L` / mobile apps).
    let include_path = if Path::new("/etc/samba").is_dir() {
        PathBuf::from(format!("/etc/samba/labdrop-{session_id}.conf"))
    } else {
        session_dir.join("smb.include.conf")
    };
    let include_content = format!(
        "[{share_name}]\n\
         \tcomment = LabDrop Temporary File Distribution (Session {session_id})\n\
         \tpath = {}\n\
         \tbrowseable = yes\n\
         \tread only = yes\n\
         \twritable = no\n\
         \tguest ok = yes\n\
         \tpublic = yes\n\
         \tforce user = nobody\n\
         \thosts allow = {hosts_allow}\n\
         \thosts deny = ALL\n",
        staged_dir.display()
    );
    fs::write(&include_path, &include_content).map_err(|e| LabDropError::IoPath {
        path: include_path.clone(),
        source: e,
    })?;

    // Inject marked include block into /etc/samba/smb.conf (preserving all existing configuration)
    let updated_conf = inject_samba_marker_block(&existing_conf, session_id, &include_path);
    fs::write(smb_conf_path, &updated_conf).map_err(|e| LabDropError::IoPath {
        path: smb_conf_path.to_path_buf(),
        source: e,
    })?;

    let mut started_service = None;
    let mut daemon_child: Option<Child> = None;
    let mut pid_file_opt: Option<PathBuf> = None;

    if env.service_active {
        let _ = Command::new("smbcontrol")
            .args(["all", "reload-config"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = Command::new("systemctl")
            .args(["reload", &env.service_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    } else if Command::new("systemctl")
        .args(["start", &env.service_name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        started_service = Some(env.service_name.clone());
    } else {
        // Container or non-systemd Linux environment: launch smbd directly on port 445
        let pid_file = session_dir.join("smbd_445.pid");
        let mut cmd = Command::new("smbd");
        cmd.args([
            "-F",
            "--no-process-group",
            "-s",
            "/etc/samba/smb.conf",
            "--option=log level=2",
            &format!("--option=log file={}", log_file.display()),
        ])
        .current_dir(session_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }
        let child = cmd
            .spawn()
            .map_err(|e| LabDropError::Smb(format!("Failed to start smbd on port 445: {e}")))?;
        let _ = fs::write(&pid_file, child.id().to_string());
        pid_file_opt = Some(pid_file);
        daemon_child = Some(child);
        thread::sleep(Duration::from_millis(600));
    }

    // Add temporary runtime firewall rule only if needed
    let firewall_rule_id = add_linux_firewall_rule(session_id, &iface.name);

    let log_stop_flag = Arc::new(AtomicBool::new(false));
    let log_thread = Some(spawn_samba_log_tailer(
        log_file,
        Arc::clone(&log_stop_flag),
        tracker,
    ));

    let daemon_pid = daemon_child.as_ref().map(|c| c.id());
    let cleanup_record = SmbCleanupRecord {
        share_name: share_name.clone(),
        port: 445,
        is_windows: false,
        linux_config_marker_id: Some(session_id.to_string()),
        linux_include_file: Some(include_path),
        linux_service_started_by_labdrop: started_service,
        linux_daemon_pid: daemon_pid,
        linux_daemon_pgid: daemon_pid,
        linux_pid_file: pid_file_opt,
        firewall_rule_id,
        windows_firewall_rule_name: None,
    };

    Ok(ActiveSmbShare {
        access_unc_windows: format!("\\\\{}\\{share_name}", iface.ipv4),
        access_uri_linux: format!("smb://{}/{share_name}", iface.ipv4),
        share_name,
        port: 445,
        mode_description: "Samba System Port 445 (Read-Only Guest Share)".to_string(),
        cleanup_record,
        child_process: daemon_child,
        log_stop_flag,
        log_thread,
    })
}

/// Pure function that injects a session-scoped LabDrop configuration block into `smb.conf`
/// while preserving all pre-existing shares and settings.
pub fn inject_samba_marker_block(
    existing_conf: &str,
    session_id: &str,
    include_path: &Path,
) -> String {
    let cleaned = remove_samba_marker_block(existing_conf, session_id);
    let has_map_to_guest = cleaned
        .lines()
        .any(|l| l.trim().to_lowercase().starts_with("map to guest"));

    let mut result = String::with_capacity(cleaned.len() + 256);
    if !has_map_to_guest && cleaned.contains("[global]") {
        let mut injected_global = false;
        for line in cleaned.lines() {
            result.push_str(line);
            result.push('\n');
            if !injected_global && line.trim().eq_ignore_ascii_case("[global]") {
                result.push_str(&format!(
                    "\t# BEGIN LABDROP GLOBAL {session_id}\n\tmap to guest = Bad User\n\trestrict anonymous = 0\n\t# END LABDROP GLOBAL {session_id}\n"
                ));
                injected_global = true;
            }
        }
    } else if !cleaned.contains("[global]") {
        result.push_str(&format!(
            "# BEGIN LABDROP GLOBAL {session_id}\n[global]\n\tworkgroup = WORKGROUP\n\tsecurity = user\n\tmap to guest = Bad User\n\trestrict anonymous = 0\n# END LABDROP GLOBAL {session_id}\n"
        ));
        result.push_str(&cleaned);
    } else {
        result.push_str(&cleaned);
    }

    if !result.ends_with('\n') && !result.is_empty() {
        result.push('\n');
    }
    result.push_str(&format!(
        "# BEGIN LABDROP SESSION {session_id}\ninclude = {}\n# END LABDROP SESSION {session_id}\n",
        include_path.display()
    ));
    result
}

/// Removes only the marker blocks belonging to `session_id` from `smb.conf`.
pub fn remove_samba_marker_block(conf: &str, session_id: &str) -> String {
    let begin_sess = format!("# BEGIN LABDROP SESSION {session_id}");
    let end_sess = format!("# END LABDROP SESSION {session_id}");
    let begin_glob = format!("# BEGIN LABDROP GLOBAL {session_id}");
    let end_glob = format!("# END LABDROP GLOBAL {session_id}");

    let mut out = String::with_capacity(conf.len());
    let mut skip_sess = false;
    let mut skip_glob = false;

    for line in conf.lines() {
        let trimmed = line.trim();
        if trimmed == begin_sess {
            skip_sess = true;
            continue;
        }
        if trimmed == end_sess {
            skip_sess = false;
            continue;
        }
        if trimmed == begin_glob {
            skip_glob = true;
            continue;
        }
        if trimmed == end_glob {
            skip_glob = false;
            continue;
        }
        if !skip_sess && !skip_glob {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[cfg(unix)]
fn add_linux_firewall_rule(session_id: &str, iface_name: &str) -> Option<String> {
    // 1. Check firewalld (Fedora)
    if Command::new("firewall-cmd")
        .arg("--state")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        let already_open = Command::new("firewall-cmd")
            .arg("--query-service=samba")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !already_open
            && Command::new("firewall-cmd")
                .arg("--add-service=samba")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        {
            return Some("firewalld:samba".to_string());
        }
    }

    // 2. Check ufw (Ubuntu / Linux Mint)
    if let Ok(out) = Command::new("ufw")
        .arg("status")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        if String::from_utf8_lossy(&out.stdout).contains("Status: active") {
            let comment = format!("LabDrop-{session_id}");
            if Command::new("ufw")
                .args([
                    "allow",
                    "in",
                    "on",
                    iface_name,
                    "to",
                    "any",
                    "port",
                    "445",
                    "proto",
                    "tcp",
                    "comment",
                    &comment,
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                return Some(format!("ufw:{iface_name}:445"));
            }
        }
    }
    None
}

#[cfg(unix)]
fn spawn_samba_log_tailer(
    log_file: PathBuf,
    stop_flag: Arc<AtomicBool>,
    tracker: Arc<TransferTracker>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut lines_seen = 0usize;
        let mut active_files: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();

        while !stop_flag.load(Ordering::Relaxed) {
            if let Ok(f) = File::open(&log_file) {
                let reader = BufReader::new(f);
                for (idx, line) in reader.lines().map_while(|r| r.ok()).enumerate() {
                    if idx < lines_seen {
                        continue;
                    }
                    lines_seen = idx + 1;
                    parse_samba_log_line(&line, &tracker, &mut active_files);
                }
            }
            thread::sleep(Duration::from_millis(250));
        }
    })
}

/// Parses a Samba `smbd.log` (log level 2) line to update `TransferTracker` with SMB client and file activity.
pub fn parse_samba_log_line(
    line: &str,
    tracker: &TransferTracker,
    active_files: &mut std::collections::HashMap<String, u64>,
) {
    // Example 1: `ipv4:192.168.18.42:51234) connect to service LabDrop`
    if line.contains("connect to service") {
        if let Some(ip_part) = line.split("ipv4:").nth(1) {
            if let Some(ip) = ip_part.split(':').next() {
                tracker.record_client_activity(ip, ProtocolKind::Smb);
            }
        }
    }
    // Example 2: `  root opened file test.txt read=Yes write=No (numopen=1)`
    if let Some((_, after)) = line.split_once("opened file ") {
        if let Some((fname, _)) = after.split_once(" read=") {
            let clean_name = fname.trim();
            if !clean_name.is_empty() && clean_name != "." && !active_files.contains_key(clean_name)
            {
                let tid = tracker.start_transfer(
                    ProtocolKind::Smb,
                    "SMB Client",
                    clean_name,
                    None,
                    false,
                );
                active_files.insert(clean_name.to_string(), tid);
            }
        }
    }
    // Example 3: `  root closed file test.txt (numopen=0) NT_STATUS_OK`
    if let Some((_, after)) = line.split_once("closed file ") {
        if let Some((fname, _)) = after.split_once(" (numopen=") {
            let clean_name = fname.trim();
            if let Some(tid) = active_files.remove(clean_name) {
                tracker.finish_transfer(tid, true);
            }
        }
    }
}

/// Generates the Windows native SMB setup commands (PowerShell + `icacls` + `netsh`) for `session_id`.
pub fn build_windows_smb_commands(
    session_id: &str,
    share_name: &str,
    staged_dir: &Path,
    iface_ip: Ipv4Addr,
) -> (Vec<String>, String) {
    let fw_rule_name = format!("LabDrop-Session-{session_id}");
    let staged_str = staged_dir.to_string_lossy();

    let icacls_cmd = format!(
        "icacls \"{staged_str}\" /grant \"*S-1-1-0:(OI)(CI)R\" /T /C"
    );
    let ps_share_cmd = format!(
        "$ev = ([System.Security.Principal.SecurityIdentifier]::new('S-1-1-0')).Translate([System.Security.Principal.NTAccount]).Value; \
         New-SmbShare -Name '{share_name}' -Path '{staged_str}' -ReadAccess $ev -Description 'LabDrop Session {session_id}' -ErrorAction Stop"
    );
    let fw_cmd = format!(
        "netsh advfirewall firewall add rule name=\"{fw_rule_name}\" dir=in action=allow protocol=TCP localport=445 localip={iface_ip}"
    );

    (vec![icacls_cmd, ps_share_cmd, fw_cmd], fw_rule_name)
}

#[cfg(windows)]
fn start_windows_smb_share(
    session_id: &str,
    staged_dir: &Path,
    iface: &NetworkInterface,
    tracker: Arc<TransferTracker>,
) -> Result<ActiveSmbShare> {
    if !is_process_elevated() {
        return Err(LabDropError::ElevationRequired(
            "Creating a native Windows SMB share (`New-SmbShare` / `net share`) and temporary Windows Firewall rule requires running LabDrop as Administrator.".to_string(),
        ));
    }

    // Check if 'LabDrop' share already exists
    let check_out = Command::new("net")
        .args(["share", "LabDrop"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let share_name = if check_out.map(|s| s.success()).unwrap_or(false) {
        let short = &session_id[..6.min(session_id.len())];
        format!("LabDrop_{short}")
    } else {
        "LabDrop".to_string()
    };

    let (_, fw_rule_name) =
        build_windows_smb_commands(session_id, &share_name, staged_dir, iface.ipv4);
    let staged_str = staged_dir.to_string_lossy().into_owned();

    // 1. Grant read-only ACL via universal SID *S-1-1-0 (Everyone)
    let _ = Command::new("icacls")
        .args([&staged_str, "/grant", "*S-1-1-0:(OI)(CI)R", "/T", "/C"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    // 2. Create share via PowerShell New-SmbShare (fallback to net share)
    let ps_script = format!(
        "$ev = ([System.Security.Principal.SecurityIdentifier]::new('S-1-1-0')).Translate([System.Security.Principal.NTAccount]).Value; \
         New-SmbShare -Name '{share_name}' -Path '{staged_str}' -ReadAccess $ev -Description 'LabDrop Session {session_id}' -ErrorAction Stop"
    );
    let ps_ok = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &ps_script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !ps_ok {
        let net_status = Command::new("net")
            .args([
                "share",
                &format!("{share_name}={staged_str}"),
                "/GRANT:Everyone,READ",
                &format!("/REMARK:LabDrop Session {session_id}"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(LabDropError::Io)?;
        if !net_status.status.success() {
            return Err(LabDropError::Smb(format!(
                "Failed to create Windows SMB share '{share_name}': {}",
                String::from_utf8_lossy(&net_status.stderr)
            )));
        }
    }

    // 3. Add temporary Windows Firewall rule scoped to the selected interface IP
    let _ = Command::new("netsh")
        .args([
            "advfirewall",
            "firewall",
            "add",
            "rule",
            &format!("name={fw_rule_name}"),
            "dir=in",
            "action=allow",
            "protocol=TCP",
            "localport=445",
            &format!("localip={}", iface.ipv4),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let cleanup_record = SmbCleanupRecord {
        share_name: share_name.clone(),
        port: 445,
        is_windows: true,
        linux_config_marker_id: None,
        linux_include_file: None,
        linux_service_started_by_labdrop: None,
        linux_daemon_pid: None,
        linux_daemon_pgid: None,
        linux_pid_file: None,
        firewall_rule_id: None,
        windows_firewall_rule_name: Some(fw_rule_name),
    };

    let _ = tracker;
    Ok(ActiveSmbShare {
        access_unc_windows: format!("\\\\{}\\{share_name}", iface.ipv4),
        access_uri_linux: format!("smb://{}/{share_name}", iface.ipv4),
        share_name,
        port: 445,
        mode_description: "Windows Native SMB (LanmanServer, Read-Only)".to_string(),
        cleanup_record,
        child_process: None,
        log_stop_flag: Arc::new(AtomicBool::new(false)),
        log_thread: None,
    })
}

/// Cleans up all OS resources recorded in `SmbCleanupRecord`, returning human-readable step descriptions.
pub fn cleanup_smb_record(record: &SmbCleanupRecord) -> Vec<String> {
    let mut actions = Vec::new();

    if record.is_windows {
        let _ = Command::new("net")
            .args(["share", &record.share_name, "/DELETE", "/Y"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        actions.push(format!("Removed Windows SMB share '{}'", record.share_name));

        if let Some(ref fw_name) = record.windows_firewall_rule_name {
            let _ = Command::new("netsh")
                .args([
                    "advfirewall",
                    "firewall",
                    "delete",
                    "rule",
                    &format!("name={fw_name}"),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            actions.push(format!("Removed Windows Firewall rule '{fw_name}'"));
        }
        return actions;
    }

    // Linux cleanup:
    // 1. Stop the isolated process group first (terminates unshare + smbd + smbd-notifyd + smbd-cleanupd)
    if let Some(pgid) = record.linux_daemon_pgid {
        kill_process_group_gracefully(pgid);
        actions.push(format!(
            "Terminated Samba process group (PGID {pgid}: smbd, smbd-notifyd, smbd-cleanupd)"
        ));
    }
    if let Some(ref pid_file) = record.linux_pid_file {
        if let Ok(s) = fs::read_to_string(pid_file) {
            if let Ok(pid) = s.trim().parse::<u32>() {
                kill_pid_gracefully(pid);
                actions.push(format!("Stopped Samba daemon process (PID {pid})"));
            }
        }
    }
    if let Some(pid) = record.linux_daemon_pid {
        kill_pid_gracefully(pid);
        actions.push(format!("Terminated session SMB process (PID {pid})"));
    }

    // 2. Remove session marker block from /etc/samba/smb.conf if present
    if let Some(ref marker_id) = record.linux_config_marker_id {
        let smb_conf_path = Path::new("/etc/samba/smb.conf");
        if let Ok(existing) = fs::read_to_string(smb_conf_path) {
            let cleaned = remove_samba_marker_block(&existing, marker_id);
            if cleaned != existing && fs::write(smb_conf_path, cleaned).is_ok() {
                actions.push(format!(
                    "Removed LabDrop session '{marker_id}' block from /etc/samba/smb.conf"
                ));
                let _ = Command::new("smbcontrol")
                    .args(["all", "reload-config"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }

    // 3. Remove include/config file
    if let Some(ref inc) = record.linux_include_file {
        if inc.exists() && fs::remove_file(inc).is_ok() {
            actions.push(format!(
                "Removed temporary Samba config '{}'",
                inc.display()
            ));
        }
    }

    // 4. Stop system service ONLY if LabDrop started it
    if let Some(ref svc) = record.linux_service_started_by_labdrop {
        let _ = Command::new("systemctl")
            .args(["stop", svc])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        actions.push(format!(
            "Stopped system service '{svc}' (started by this session)"
        ));
    }

    // 5. Remove temporary firewall rule added by LabDrop
    if let Some(ref fw_id) = record.firewall_rule_id {
        if fw_id == "firewalld:samba" {
            let _ = Command::new("firewall-cmd")
                .arg("--remove-service=samba")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            actions.push("Removed temporary firewalld 'samba' service rule".to_string());
        } else if let Some(rest) = fw_id.strip_prefix("ufw:") {
            let parts: Vec<&str> = rest.split(':').collect();
            if parts.len() == 2 {
                let _ = Command::new("ufw")
                    .args([
                        "delete",
                        "allow",
                        "in",
                        "on",
                        parts[0],
                        "to",
                        "any",
                        "port",
                        parts[1],
                        "proto",
                        "tcp",
                    ])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                actions.push(format!(
                    "Removed temporary ufw rule on {} port {}",
                    parts[0], parts[1]
                ));
            }
        }
    }

    actions
}

pub fn kill_process_group_gracefully(pgid: u32) {
    if pgid == 0 {
        return;
    }
    #[cfg(unix)]
    {
        unsafe {
            let _ = libc::kill(-(pgid as i32), libc::SIGTERM);
        }
        thread::sleep(Duration::from_millis(150));
        unsafe {
            let _ = libc::kill(-(pgid as i32), libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        kill_pid_gracefully(pgid);
    }
}

pub fn kill_pid_gracefully(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(unix)]
    {
        unsafe {
            let _ = libc::kill(pid as i32, libc::SIGTERM);
        }
        thread::sleep(Duration::from_millis(100));
        unsafe {
            let _ = libc::kill(pid as i32, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samba_marker_block_injection_and_removal_should_preserve_existing_shares() {
        let original = "[global]\n\tworkgroup = SAMBA\n\tsecurity = user\n\n[existing_share]\n\tpath = /srv/data\n";
        let inc_path = Path::new("/var/tmp/labdrop/smb.include.conf");
        let injected = inject_samba_marker_block(original, "sess-123", inc_path);

        assert!(injected.contains("[existing_share]"));
        assert!(injected.contains("map to guest = Bad User"));
        assert!(injected.contains("# BEGIN LABDROP SESSION sess-123"));
        assert!(injected.contains("include = /var/tmp/labdrop/smb.include.conf"));

        let restored = remove_samba_marker_block(&injected, "sess-123");
        assert_eq!(restored, original);
    }

    #[test]
    fn windows_smb_command_builder_should_use_universal_everyone_sid_and_scoped_firewall() {
        let (cmds, fw_name) = build_windows_smb_commands(
            "abc999",
            "LabDrop",
            Path::new("C:\\Temp\\staged"),
            Ipv4Addr::new(192, 168, 10, 50),
        );
        assert_eq!(fw_name, "LabDrop-Session-abc999");
        assert!(cmds[0].contains("*S-1-1-0:(OI)(CI)R"));
        assert!(cmds[1].contains("S-1-1-0"));
        assert!(cmds[1].contains("New-SmbShare"));
        assert!(cmds[2].contains("localip=192.168.10.50"));
    }
}
