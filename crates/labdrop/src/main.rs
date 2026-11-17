mod tui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use labdrop_core::{
    fs_space::{format_bytes, query_disk_space},
    i18n::Lang,
    network::discover_network_interfaces,
    session::{
        recover_orphaned_sessions, run_watchdog_process, ActiveSharingSession, SharingStrategy,
    },
    smb::{probe_smb_environment, SmbLinuxMode},
    staging::{default_staging_base_dir, stage_selection, SelectionSet},
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

static STOP_SIGNAL: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn handle_termination_signal(_sig: libc::c_int) {
    STOP_SIGNAL.store(true, Ordering::Relaxed);
}

#[derive(Parser, Debug)]
#[command(
    name = "labdrop",
    version,
    about = "LabDrop — Lightweight Cross-Platform Laboratory File Distribution Utility"
)]
struct Cli {
    /// Internal crash-recovery watchdog mode (invoked automatically by LabDrop).
    #[arg(long = "internal-watchdog", num_args = 2, hide = true)]
    internal_watchdog: Option<Vec<String>>,

    /// Interface language: 'en' or 'pt' (auto-detected from locale/config by default).
    #[arg(long, global = true)]
    lang: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,

    /// Optional files or folders to pre-select when launching the TUI (e.g. via drag-and-drop onto LabDrop.exe).
    #[arg(value_name = "PATHS")]
    paths: Vec<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Launch the interactive terminal user interface (default when no subcommand is given).
    Tui {
        /// Optional files or folders to pre-select on startup.
        #[arg(value_name = "PATHS")]
        paths: Vec<PathBuf>,
    },

    /// Stage selected files/folders and start a sharing session from the command line.
    Share {
        /// Files or directories to stage and share (may be specified multiple times).
        #[arg(short, long = "select", required = true)]
        select: Vec<PathBuf>,

        /// Network interface name or IPv4 address to bind to (defaults to preferred LAN interface).
        #[arg(short, long)]
        interface: Option<String>,

        /// Sharing protocol strategy.
        #[arg(short, long, value_enum, default_value_t = CliStrategy::Auto)]
        strategy: CliStrategy,

        /// Preferred HTTP / WebDAV port (0 for automatic ephemeral port).
        #[arg(long, default_value_t = 8080)]
        http_port: u16,

        /// Preferred Linux SMB port (445 for system Samba, or e.g. 4445 for rootless Samba).
        #[arg(long)]
        smb_port: Option<u16>,

        /// Custom base staging directory (defaults to internal storage /var/tmp or %TEMP%).
        #[arg(long)]
        staging_base: Option<PathBuf>,

        /// Write the active SessionLedger JSON to this file once sharing is ready.
        #[arg(long)]
        status_file: Option<PathBuf>,

        /// Automatically stop sharing after N seconds (useful for scripted runs/tests).
        #[arg(long)]
        duration_secs: Option<u64>,

        /// Retain staged files on disk after stopping the session instead of deleting them.
        #[arg(long)]
        retain_staged: bool,
    },

    /// Scan for and clean up any orphaned or crashed LabDrop sessions.
    Cleanup {
        /// Custom base staging directory to scan.
        #[arg(long)]
        staging_base: Option<PathBuf>,
    },

    /// Inspect system capabilities, network interfaces, Samba status, and available staging space.
    Doctor,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum, Debug)]
enum CliStrategy {
    Auto,
    Smb,
    Webdav,
    Http,
}

impl From<CliStrategy> for SharingStrategy {
    fn from(v: CliStrategy) -> Self {
        match v {
            CliStrategy::Auto => SharingStrategy::AutoLayered,
            CliStrategy::Smb => SharingStrategy::SmbOnly,
            CliStrategy::Webdav => SharingStrategy::WebDavSecondary,
            CliStrategy::Http => SharingStrategy::HttpFallback,
        }
    }
}

fn resolve_initial_lang(cli_lang: Option<&str>) -> Lang {
    if let Some(s) = cli_lang {
        if let Some(parsed) = Lang::parse_str(s) {
            return parsed;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let cfg = parent.join("config").join("labdrop.toml");
            if let Ok(content) = fs::read_to_string(cfg) {
                for line in content.lines() {
                    let trimmed = line.trim();
                    if let Some(rest) = trimmed.strip_prefix("language") {
                        if let Some((_, val)) = rest.split_once('=') {
                            let clean = val.split('#').next().unwrap_or("").trim().trim_matches('"');
                            if let Some(parsed) = Lang::parse_str(clean) {
                                return parsed;
                            }
                        }
                    }
                }
            }
        }
    }
    Lang::detect_from_env()
}

fn main() -> Result<()> {
    // Check for internal watchdog invocation before full CLI parsing
    let raw_args: Vec<String> = std::env::args().collect();
    if raw_args.len() >= 4 && raw_args[1] == "--internal-watchdog" {
        let reg_file = PathBuf::from(&raw_args[2]);
        let parent_pid = raw_args[3].parse::<u32>().unwrap_or(0);
        run_watchdog_process(&reg_file, parent_pid);
        return Ok(());
    }

    let cli = Cli::parse();
    if let Some(wd_args) = cli.internal_watchdog {
        if wd_args.len() == 2 {
            let reg_file = PathBuf::from(&wd_args[0]);
            let parent_pid = wd_args[1].parse::<u32>().unwrap_or(0);
            run_watchdog_process(&reg_file, parent_pid);
            return Ok(());
        }
    }

    let lang = resolve_initial_lang(cli.lang.as_deref());
    let top_paths = cli.paths;

    match cli.command {
        None => tui::run_tui(lang, top_paths),
        Some(Commands::Tui { paths }) => {
            let combined = if paths.is_empty() { top_paths } else { paths };
            tui::run_tui(lang, combined)
        }
        Some(Commands::Doctor) => run_doctor(),
        Some(Commands::Cleanup { staging_base }) => {
            let reports = recover_orphaned_sessions(staging_base.as_deref());
            if reports.is_empty() {
                println!("No orphaned LabDrop sessions found.");
            } else {
                for r in reports {
                    println!("Recovered session '{}':", r.session_id);
                    for a in r.actions_performed {
                        println!("  ✓ {a}");
                    }
                }
            }
            Ok(())
        }
        Some(Commands::Share {
            select,
            interface,
            strategy,
            http_port,
            smb_port,
            staging_base,
            status_file,
            duration_secs,
            retain_staged,
        }) => run_cli_share(
            select,
            interface,
            strategy.into(),
            http_port,
            smb_port,
            staging_base,
            status_file,
            duration_secs,
            retain_staged,
        ),
    }
}

fn run_doctor() -> Result<()> {
    let smb_env = probe_smb_environment();
    let ifaces = discover_network_interfaces();
    let base_dir = default_staging_base_dir();
    let space = query_disk_space(&base_dir)?;

    println!("=== LabDrop System Diagnostics ===");
    println!("OS Family:                {}", smb_env.os_family);
    if let Some(ref d) = smb_env.linux_distro {
        println!("Linux Distribution:       {}", d.display_name());
        println!("Package Install Command:  {}", d.install_command());
    }
    println!(
        "SMB / Samba Installed:    {} ({})",
        smb_env.smbd_installed,
        smb_env.smbd_version.as_deref().unwrap_or("N/A")
    );
    println!(
        "System SMB Service:       {} (Active: {})",
        smb_env.service_name, smb_env.service_active
    );
    println!("Process Elevated (Root):  {}", smb_env.is_elevated);
    println!(
        "Rootless SMB (unshare):   {}",
        smb_env.rootless_unshare_available
    );
    println!("Firewall Backend:         {}", smb_env.firewall_backend);
    println!("SELinux Enforcing:        {}", smb_env.selinux_enforcing);
    println!(
        "Internal Staging Base:    {} ({} free of {})",
        base_dir.display(),
        format_bytes(space.available_bytes),
        format_bytes(space.total_bytes)
    );
    println!("\nDiscovered Network Interfaces:");
    for iface in ifaces {
        println!("  - {}", iface.display_label());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_cli_share(
    select: Vec<PathBuf>,
    interface_filter: Option<String>,
    strategy: SharingStrategy,
    http_port: u16,
    smb_port: Option<u16>,
    staging_base: Option<PathBuf>,
    status_file: Option<PathBuf>,
    duration_secs: Option<u64>,
    retain_staged: bool,
) -> Result<()> {
    #[cfg(unix)]
    unsafe {
        libc::signal(
            libc::SIGINT,
            handle_termination_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            handle_termination_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGHUP,
            handle_termination_signal as *const () as libc::sighandler_t,
        );
    }

    let mut sel = SelectionSet::new();
    for path in &select {
        sel.add_path(path)
            .with_context(|| format!("Failed to add path '{}'", path.display()))?;
    }

    let ifaces = discover_network_interfaces();
    let chosen_iface = if let Some(ref f) = interface_filter {
        ifaces
            .iter()
            .find(|i| i.name.eq_ignore_ascii_case(f) || i.ipv4.to_string() == *f)
            .cloned()
            .with_context(|| format!("Network interface '{f}' not found"))?
    } else {
        ifaces
            .first()
            .cloned()
            .context("No network interfaces discovered")?
    };

    let base_dir = staging_base.unwrap_or_else(default_staging_base_dir);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let session_id = format!("ld-{:x}", now_ms & 0xFFFF_FFFF);
    let session_dir = base_dir.join(&session_id);
    let staged_dir = session_dir.join("staged");

    println!(
        "Staging {} item(s) ({} files, {}) to '{}'...",
        sel.items().len(),
        sel.total_files(),
        format_bytes(sel.total_bytes()),
        staged_dir.display()
    );

    let manifest = stage_selection(&session_id, &sel, &staged_dir, None, None, |_| {})?;
    for item in &manifest.selected_items {
        if item.renamed_due_to_collision {
            println!(
                "  • Collision-safe rename: '{}' -> '{}'",
                item.source_path.display(),
                item.staged_name
            );
        }
    }

    let linux_smb_mode = match smb_port {
        Some(445) => Some(SmbLinuxMode::SystemPort445),
        Some(p) => Some(SmbLinuxMode::RootlessPort(p)),
        None => None,
    };

    let mut session = ActiveSharingSession::start(
        manifest,
        session_dir,
        &chosen_iface,
        strategy,
        linux_smb_mode,
        http_port,
        true,
    )?;

    if let Some(ref sf) = status_file {
        let json = serde_json::to_string_pretty(&session.ledger)?;
        fs::write(sf, json)?;
    }

    println!("\n=== LabDrop Session '{}' ACTIVE ===", session.ledger.session_id);
    println!(
        "Interface: {} ({})",
        session.ledger.interface_name, session.ledger.interface_ip
    );
    println!("Notice: Unauthenticated read-only access is active on this interface.");
    println!("Endpoints:");
    for ep in &session.ledger.endpoints {
        println!(
            "  • {:<20} [{}] -> {}",
            ep.protocol, ep.target_os, ep.address
        );
    }
    for w in &session.ledger.warnings {
        println!("  ! {w}");
    }

    let start = Instant::now();
    while !STOP_SIGNAL.load(Ordering::Relaxed) {
        if let Some(limit) = duration_secs {
            if start.elapsed() >= Duration::from_secs(limit) {
                break;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }

    println!("\nStopping session '{}' and cleaning up...", session.ledger.session_id);
    let snap = session.tracker.snapshot();
    println!(
        "Transfer Summary: {} transferred across {} completed transfer(s).",
        format_bytes(snap.total_bytes_transferred),
        snap.completed_transfer_count
    );

    let report = session.stop_and_cleanup(!retain_staged);
    for act in &report.actions_performed {
        println!("  ✓ {act}");
    }
    Ok(())
}
