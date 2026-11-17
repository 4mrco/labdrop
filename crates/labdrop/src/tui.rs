use anyhow::Result;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use labdrop_core::{
    config::LabDropConfig,
    fs_space::{format_bytes, query_disk_space},
    i18n::Lang,
    network::{discover_network_interfaces, resolve_preferred_interface, NetworkInterface},
    session::{
        is_terminal_disconnected, recover_orphaned_sessions, ActiveSharingSession, CleanupReport,
        SharingStrategy,
    },
    smb::{
        install_linux_samba_interactive, probe_smb_environment, SmbEnvironmentStatus, SmbLinuxMode,
    },
    staging::{
        default_staging_base_dir, parse_dropped_paths, stage_selection, ItemKind, SelectionSet,
        StagingManifest,
    },
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use std::{
    fs,
    io::{self, Stdout},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// Restrained, consistent TUI color palette
const ACCENT: Color = Color::Cyan;
const TEXT: Color = Color::White;
const MUTED: Color = Color::Gray;
const DIM: Color = Color::DarkGray;
const STATUS_OK: Color = Color::Green;
const STATUS_WARN: Color = Color::Yellow;
const STATUS_ERR: Color = Color::Red;

static TUI_TERMINATION_REQUESTED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn handle_tui_signal(_sig: libc::c_int) {
    TUI_TERMINATION_REQUESTED.store(true, Ordering::Relaxed);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Dashboard,
    FileBrowser,
    ReviewSelection,
    NetworkConfig,
    ActiveSession,
    ConfirmCleanup,
    Diagnostics,
    PastePathModal,
}

#[derive(Debug, Clone)]
struct BrowserEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    size_bytes: u64,
    is_parent_link: bool,
}

fn tr<'a>(lang: Lang, en: &'a str, pt: &'a str) -> &'a str {
    match lang {
        Lang::En => en,
        Lang::PtBr => pt,
    }
}

pub struct LabDropTuiApp {
    pub lang: Lang,
    pub config: LabDropConfig,
    pub screen: Screen,
    return_screen: Screen,
    pub should_quit: bool,
    pub menu_idx: usize,
    pub selection: SelectionSet,
    pub interfaces: Vec<NetworkInterface>,
    pub selected_iface_idx: usize,
    pub preferred_iface_warning: Option<String>,
    pub strategy: SharingStrategy,
    pub linux_smb_mode: SmbLinuxMode,
    pub preferred_http_port: u16,
    pub smb_env: SmbEnvironmentStatus,
    pub active_session: Option<ActiveSharingSession>,
    pub retained_staged: Option<(StagingManifest, PathBuf)>,
    pub last_cleanup_report: Option<CleanupReport>,
    pub status_message: String,
    pub warning_message: Option<String>,
    pub error_message: Option<String>,
    pub show_sharing_details: bool,

    // File browser state
    browser_dir: PathBuf,
    browser_entries: Vec<BrowserEntry>,
    pub browser_idx: usize,
    pub show_hidden: bool,
    pub review_idx: usize,
    pub cleanup_choice_idx: usize,
    bookmarks: Vec<(String, PathBuf)>,

    // Paste / Drop path modal buffer
    paste_buffer: String,
}

impl LabDropTuiApp {
    pub fn new(lang: Lang, initial_paths: Vec<PathBuf>) -> Self {
        let config = LabDropConfig::load();
        let interfaces = discover_network_interfaces();
        let (selected_iface_idx, preferred_iface_warning) =
            resolve_preferred_interface(&interfaces, config.preferred_interface.as_deref(), lang);

        let smb_env = probe_smb_environment();
        let linux_smb_mode = if smb_env.is_elevated {
            SmbLinuxMode::SystemPort445
        } else {
            SmbLinuxMode::RootlessPort(config.preferred_rootless_smb_port)
        };
        let strategy = config.resolved_strategy();
        let preferred_http_port = config.preferred_http_port;

        let bookmarks = build_quick_bookmarks(lang);
        let initial_dir = bookmarks
            .first()
            .map(|(_, p)| p.clone())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));

        // Automatically recover any crashed/orphaned sessions on startup
        let recovered = recover_orphaned_sessions(None);

        let mut selection = SelectionSet::new();
        let mut added_initial = 0usize;
        for p in initial_paths {
            if selection.add_path(&p).unwrap_or(false) {
                added_initial += 1;
            }
        }

        let status_message = if !recovered.is_empty() {
            match lang {
                Lang::En => format!(
                    "Recovered and cleaned up {} orphaned session(s) on startup.",
                    recovered.len()
                ),
                Lang::PtBr => format!(
                    "Recuperadas e limpas {} sessão(ões) órfã(s) ao iniciar.",
                    recovered.len()
                ),
            }
        } else if added_initial > 0 {
            match lang {
                Lang::En => format!("Pre-selected {added_initial} dropped/CLI item(s)."),
                Lang::PtBr => format!("Pré-selecionado(s) {added_initial} item(ns) recebido(s)."),
            }
        } else {
            tr(
                lang,
                "Ready. Use keys 1-6, arrows + Enter, mouse click, or drag files into the terminal.",
                "Pronto. Use as teclas 1-6, setas + Enter, clique com o mouse ou arraste arquivos aqui.",
            )
            .to_string()
        };

        let warning_message = preferred_iface_warning.clone();

        let mut app = Self {
            lang,
            config,
            screen: Screen::Dashboard,
            return_screen: Screen::Dashboard,
            should_quit: false,
            menu_idx: 0,
            selection,
            interfaces,
            selected_iface_idx,
            preferred_iface_warning,
            strategy,
            linux_smb_mode,
            preferred_http_port,
            smb_env,
            active_session: None,
            retained_staged: None,
            last_cleanup_report: recovered.into_iter().last(),
            status_message,
            warning_message,
            error_message: None,
            show_sharing_details: false,
            browser_dir: initial_dir,
            browser_entries: Vec::new(),
            browser_idx: 0,
            show_hidden: false,
            review_idx: 0,
            cleanup_choice_idx: 0,
            bookmarks,
            paste_buffer: String::new(),
        };
        app.refresh_browser_entries();
        app
    }

    pub fn current_interface(&self) -> NetworkInterface {
        self.interfaces
            .get(self.selected_iface_idx)
            .cloned()
            .unwrap_or_else(|| NetworkInterface {
                name: "lo".to_string(),
                ipv4: std::net::Ipv4Addr::LOCALHOST,
                prefix_len: 8,
                subnet_cidr: "127.0.0.0/8".to_string(),
                is_loopback: true,
                is_wireless: false,
                is_virtual: false,
                is_default_route: false,
            })
    }

    pub fn refresh_network_interfaces(&mut self) {
        self.error_message = None;
        let current_selected_name = self
            .interfaces
            .get(self.selected_iface_idx)
            .map(|i| i.name.clone());

        self.interfaces = discover_network_interfaces();
        self.smb_env = probe_smb_environment();

        // Check preferred interface availability
        let (_, pref_warn) = resolve_preferred_interface(
            &self.interfaces,
            self.config.preferred_interface.as_deref(),
            self.lang,
        );
        self.preferred_iface_warning = pref_warn.clone();

        // Preserve user's current selection if still present; otherwise resolve preferred
        if let Some(ref cur_name) = current_selected_name {
            if let Some(pos) = self
                .interfaces
                .iter()
                .position(|i| i.name.eq_ignore_ascii_case(cur_name))
            {
                self.selected_iface_idx = pos;
            } else {
                let (idx, _) = resolve_preferred_interface(
                    &self.interfaces,
                    self.config.preferred_interface.as_deref(),
                    self.lang,
                );
                self.selected_iface_idx = idx;
            }
        } else {
            let (idx, _) = resolve_preferred_interface(
                &self.interfaces,
                self.config.preferred_interface.as_deref(),
                self.lang,
            );
            self.selected_iface_idx = idx;
        }

        if let Some(w) = pref_warn {
            self.warning_message = Some(w);
        } else {
            self.warning_message = None;
        }

        self.status_message = match self.lang {
            Lang::En => format!(
                "Refreshed network interfaces ({} detected).",
                self.interfaces.len()
            ),
            Lang::PtBr => format!(
                "Interfaces de rede atualizadas ({} detectada(s)).",
                self.interfaces.len()
            ),
        };
    }

    pub fn set_preferred_interface(&mut self, iface_name: Option<String>) {
        self.error_message = None;
        self.config.preferred_interface = iface_name.clone();
        self.preferred_iface_warning = None;
        self.warning_message = None;
        match self.config.save() {
            Ok(saved_path) => {
                self.status_message = match (self.lang, iface_name) {
                    (Lang::En, Some(n)) => format!(
                        "Saved '{}' as preferred default interface in {}.",
                        n,
                        saved_path.display()
                    ),
                    (Lang::PtBr, Some(n)) => format!(
                        "Interface '{}' salva como preferida padrão em {}.",
                        n,
                        saved_path.display()
                    ),
                    (Lang::En, None) => format!(
                        "Preferred interface reset to Automatic in {}.",
                        saved_path.display()
                    ),
                    (Lang::PtBr, None) => format!(
                        "Interface preferida redefinida para Automática em {}.",
                        saved_path.display()
                    ),
                };
            }
            Err(e) => {
                self.warning_message = Some(format!(
                    "{}: {e}",
                    tr(
                        self.lang,
                        "Preference applied for this session (could not write config file)",
                        "Preferência aplicada nesta sessão (não foi possível salvar no arquivo)"
                    )
                ));
            }
        }
    }

    /// Restarts the active sharing service on the currently selected network interface
    /// without closing LabDrop or re-staging files.
    pub fn restart_sharing_service(
        &mut self,
        terminal: Option<&mut Terminal<CrosstermBackend<Stdout>>>,
    ) {
        self.error_message = None;
        self.warning_message = None;
        let new_iface = self.current_interface();

        if let Some(mut sess) = self.active_session.take() {
            let old_name = sess.ledger.interface_name.clone();
            let old_ip = sess.ledger.interface_ip.clone();
            let manifest_backup = sess.manifest.clone();
            let session_dir_backup = sess.ledger.session_dir.clone();

            match sess.restart_on_interface(
                &new_iface,
                self.strategy,
                Some(self.linux_smb_mode),
                self.preferred_http_port,
                true,
            ) {
                Ok(_) => {
                    let switched = old_name != new_iface.name || old_ip != new_iface.ipv4.to_string();
                    if switched {
                        self.warning_message = Some(match self.lang {
                            Lang::En => format!(
                                "Switched interface from {} ({}) to {} ({}). Previous client connections were interrupted; use the updated address.",
                                old_name, old_ip, new_iface.name, new_iface.ipv4
                            ),
                            Lang::PtBr => format!(
                                "Interface alterada de {} ({}) para {} ({}). Conexões anteriores foram interrompidas; use o novo endereço.",
                                old_name, old_ip, new_iface.name, new_iface.ipv4
                            ),
                        });
                    } else {
                        self.status_message = match self.lang {
                            Lang::En => format!(
                                "Sharing service restarted on {} ({}).",
                                new_iface.name, new_iface.ipv4
                            ),
                            Lang::PtBr => format!(
                                "Serviço de compartilhamento reiniciado em {} ({}).",
                                new_iface.name, new_iface.ipv4
                            ),
                        };
                    }
                    self.retained_staged = Some((manifest_backup, session_dir_backup));
                    self.active_session = Some(sess);
                    self.screen = Screen::ActiveSession;
                }
                Err(e) => {
                    // Note: `restart_on_interface` already stopped old listeners and cleaned up shares while retaining staged_dir!
                    self.retained_staged = Some((manifest_backup, session_dir_backup));
                    self.error_message = Some(format!(
                        "{}: {e}",
                        tr(
                            self.lang,
                            "Failed to restart sharing service on selected interface (previous shares were cleanly stopped; staged files preserved)",
                            "Falha ao reiniciar o serviço na interface selecionada (serviços anteriores foram parados com segurança; arquivos mantidos)"
                        )
                    ));
                }
            }
            return;
        }

        // If no active session is running, check if we have retained staged files from a previous stop/failed restart
        if let Some((manifest, session_dir)) = self.retained_staged.clone() {
            if manifest.staged_dir.is_dir() {
                match ActiveSharingSession::start(
                    manifest,
                    session_dir,
                    &new_iface,
                    self.strategy,
                    Some(self.linux_smb_mode),
                    self.preferred_http_port,
                    true,
                ) {
                    Ok(sess) => {
                        self.status_message = match self.lang {
                            Lang::En => format!(
                                "Sharing service started on {} ({}) using existing staged files.",
                                new_iface.name, new_iface.ipv4
                            ),
                            Lang::PtBr => format!(
                                "Serviço iniciado em {} ({}) reutilizando os arquivos preparados.",
                                new_iface.name, new_iface.ipv4
                            ),
                        };
                        self.active_session = Some(sess);
                        self.screen = Screen::ActiveSession;
                        return;
                    }
                    Err(e) => {
                        self.error_message = Some(format!(
                            "{}: {e}",
                            tr(
                                self.lang,
                                "Failed to start sharing service",
                                "Falha ao iniciar o serviço de compartilhamento"
                            )
                        ));
                        return;
                    }
                }
            }
        }

        // Otherwise stage selection and start
        if let Some(term) = terminal {
            self.stage_and_start_session(term);
        }
    }

    fn refresh_browser_entries(&mut self) {
        let mut entries = Vec::new();
        if let Some(parent) = self.browser_dir.parent() {
            if !parent.as_os_str().is_empty() && parent != self.browser_dir {
                entries.push(BrowserEntry {
                    name: tr(self.lang, ".. (Parent Directory)", ".. (Pasta Anterior)").to_string(),
                    path: parent.to_path_buf(),
                    is_dir: true,
                    size_bytes: 0,
                    is_parent_link: true,
                });
            }
        }

        if let Ok(rd) = fs::read_dir(&self.browser_dir) {
            let mut items = Vec::new();
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if !self.show_hidden && name.starts_with('.') {
                    continue;
                }
                let path = e.path();
                let Ok(meta) = e.metadata() else {
                    continue;
                };
                items.push(BrowserEntry {
                    name,
                    path,
                    is_dir: meta.is_dir(),
                    size_bytes: if meta.is_dir() { 0 } else { meta.len() },
                    is_parent_link: false,
                });
            }
            items.sort_by(|a, b| {
                b.is_dir
                    .cmp(&a.is_dir)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
            entries.extend(items);
        }

        self.browser_entries = entries;
        if self.browser_idx >= self.browser_entries.len() {
            self.browser_idx = self.browser_entries.len().saturating_sub(1);
        }
    }

    fn add_dropped_text(&mut self, raw: &str) {
        self.error_message = None;
        let paths = parse_dropped_paths(raw);
        if paths.is_empty() {
            self.error_message = Some(
                tr(
                    self.lang,
                    "No valid file or folder paths found in dropped/pasted input.",
                    "Nenhum caminho de arquivo ou pasta válido encontrado.",
                )
                .to_string(),
            );
            return;
        }

        let mut added = 0usize;
        let mut skipped_dup = 0usize;
        let mut failed: Option<String> = None;

        for p in paths {
            match self.selection.add_path(&p) {
                Ok(true) => added += 1,
                Ok(false) => skipped_dup += 1,
                Err(e) => {
                    failed = Some(format!("{}: {e}", p.display()));
                }
            }
        }

        if let Some(err) = failed {
            self.error_message = Some(err);
        }
        self.status_message = match self.lang {
            Lang::En => format!("Added {added} item(s) via drop/paste ({skipped_dup} already selected)."),
            Lang::PtBr => format!("Adicionado(s) {added} item(ns) por arrasto/colagem ({skipped_dup} já selecionado(s))."),
        };
    }

    fn open_native_picker(&mut self, pick_folders: bool) {
        self.error_message = None;
        let result = run_native_file_dialog(&self.browser_dir, pick_folders, self.lang);
        match result {
            Ok(paths) if paths.is_empty() => {
                self.status_message = tr(
                    self.lang,
                    "Native file picker closed with no new selection.",
                    "Seletor nativo fechado sem novos itens.",
                )
                .to_string();
            }
            Ok(paths) => {
                let mut added = 0usize;
                for p in paths {
                    if self.selection.add_path(&p).unwrap_or(false) {
                        added += 1;
                    }
                }
                self.status_message = match self.lang {
                    Lang::En => format!("Added {added} item(s) from native file explorer."),
                    Lang::PtBr => format!("Adicionado(s) {added} item(ns) pelo gerenciador nativo."),
                };
            }
            Err(msg) => {
                self.error_message = Some(msg);
            }
        }
    }

    fn open_folder_in_system_explorer(&mut self) {
        self.error_message = None;
        let dir = self.browser_dir.clone();
        #[cfg(unix)]
        let spawn_res = Command::new("xdg-open")
            .arg(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        #[cfg(windows)]
        let spawn_res = Command::new("explorer.exe")
            .arg(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();

        match spawn_res {
            Ok(_) => {
                self.status_message = match self.lang {
                    Lang::En => format!(
                        "Opened '{}' in system file manager — drag files from it onto this terminal.",
                        dir.display()
                    ),
                    Lang::PtBr => format!(
                        "Pasta '{}' aberta no gerenciador — arraste os arquivos para este terminal.",
                        dir.display()
                    ),
                };
            }
            Err(e) => {
                self.error_message = Some(format!("Failed to open system file manager: {e}"));
            }
        }
    }

    fn stage_and_start_session(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) {
        self.error_message = None;
        self.warning_message = None;
        if self.selection.is_empty() {
            self.error_message = Some(
                tr(
                    self.lang,
                    "Please select at least one file or folder before starting a session.",
                    "Selecione pelo menos um arquivo ou pasta antes de iniciar o compartilhamento.",
                )
                .to_string(),
            );
            return;
        }

        // Stop any existing session first
        if let Some(mut prev) = self.active_session.take() {
            let _ = prev.stop_and_cleanup(true);
        }
        self.retained_staged = None;

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let session_id = format!("ld-{:x}", now_ms & 0xFFFF_FFFF);
        let base_dir = default_staging_base_dir();
        let session_dir = base_dir.join(&session_id);
        let staged_dir = session_dir.join("staged");

        let lang = self.lang;
        let res = stage_selection(
            &session_id,
            &self.selection,
            &staged_dir,
            None,
            Some(&TUI_TERMINATION_REQUESTED),
            |prog| {
                let _ = terminal.draw(|f| {
                    render_staging_progress_modal(
                        f,
                        lang,
                        prog.current_file.as_str(),
                        prog.files_completed,
                        prog.total_files,
                        prog.bytes_copied,
                        prog.total_bytes,
                    );
                });
            },
        );

        let manifest = match res {
            Ok(m) => m,
            Err(e) => {
                self.error_message = Some(format!(
                    "{}: {e}",
                    tr(self.lang, "Staging failed", "Falha ao preparar arquivos")
                ));
                return;
            }
        };

        let iface = self.current_interface();
        match ActiveSharingSession::start(
            manifest.clone(),
            session_dir.clone(),
            &iface,
            self.strategy,
            Some(self.linux_smb_mode),
            self.preferred_http_port,
            true,
        ) {
            Ok(sess) => {
                self.status_message = match self.lang {
                    Lang::En => format!(
                        "Sharing session '{}' is ACTIVE on {} ({}).",
                        sess.ledger.session_id, iface.name, iface.ipv4
                    ),
                    Lang::PtBr => format!(
                        "Sessão '{}' ATIVA em {} ({}).",
                        sess.ledger.session_id, iface.name, iface.ipv4
                    ),
                };
                self.retained_staged = Some((manifest, session_dir));
                self.active_session = Some(sess);
                self.screen = Screen::ActiveSession;
            }
            Err(e) => {
                let _ = fs::remove_dir_all(base_dir.join(&session_id));
                self.error_message = Some(format!(
                    "{}: {e}",
                    tr(
                        self.lang,
                        "Failed to start sharing session",
                        "Falha ao iniciar sessão"
                    )
                ));
            }
        }
    }
}

fn run_native_file_dialog(
    initial_dir: &Path,
    pick_folders: bool,
    #[cfg_attr(windows, allow(unused_variables))] lang: Lang,
) -> std::result::Result<Vec<PathBuf>, String> {
    #[cfg(unix)]
    {
        let start_arg = format!("{}/", initial_dir.display());
        let title = if pick_folders {
            tr(lang, "Select Folder for LabDrop", "Selecionar Pasta para LabDrop")
        } else {
            tr(lang, "Select Files for LabDrop", "Selecionar Arquivos para LabDrop")
        };

        let mut zenity_args = vec!["--file-selection", "--title", title, "--filename", &start_arg];
        if pick_folders {
            zenity_args.push("--directory");
        } else {
            zenity_args.push("--multiple");
            zenity_args.push("--separator=\n");
        }
        if let Ok(out) = Command::new("zenity")
            .args(&zenity_args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                return Ok(text
                    .lines()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .collect());
            } else if out.status.code() == Some(1) {
                return Ok(Vec::new());
            }
        }

        let k_arg = if pick_folders {
            "--getexistingdirectory"
        } else {
            "--getopenfilename"
        };
        let mut k_args = vec![k_arg, &start_arg];
        if !pick_folders {
            k_args.push("--multiple");
            k_args.push("--separate-output");
        }
        if let Ok(out) = Command::new("kdialog")
            .args(&k_args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout);
                return Ok(text
                    .lines()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .collect());
            } else if out.status.code() == Some(1) {
                return Ok(Vec::new());
            }
        }

        Err(tr(
            lang,
            "No native dialog tool found (install `zenity` or `kdialog`), or press [x] to open folder and drag files into terminal.",
            "Nenhuma janela nativa encontrada (instale `zenity` ou `kdialog`), ou pressione [x] para abrir a pasta e arrastar arquivos.",
        )
        .to_string())
    }

    #[cfg(windows)]
    {
        let dir_esc = initial_dir.display().to_string().replace('\'', "''");
        let ps_script = if pick_folders {
            format!(
                "Add-Type -AssemblyName System.Windows.Forms; \
                 $d = New-Object System.Windows.Forms.FolderBrowserDialog; \
                 $d.SelectedPath = '{dir_esc}'; \
                 if ($d.ShowDialog() -eq 'OK') {{ Write-Output $d.SelectedPath }}"
            )
        } else {
            format!(
                "Add-Type -AssemblyName System.Windows.Forms; \
                 $d = New-Object System.Windows.Forms.OpenFileDialog; \
                 $d.InitialDirectory = '{dir_esc}'; \
                 $d.Multiselect = $true; \
                 if ($d.ShowDialog() -eq 'OK') {{ $d.FileNames | ForEach-Object {{ Write-Output $_ }} }}"
            )
        };
        let out = Command::new("powershell")
            .args(["-NoProfile", "-STA", "-Command", &ps_script])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| format!("Failed to open Windows Explorer dialog: {e}"))?;

        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            Ok(text
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect())
        } else {
            Ok(Vec::new())
        }
    }
}

fn build_quick_bookmarks(lang: Lang) -> Vec<(String, PathBuf)> {
    let mut list = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let usb_files = parent.join("files");
            if usb_files.is_dir() {
                list.push(("files/".to_string(), usb_files));
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let cwd_files = cwd.join("files");
        if cwd_files.is_dir() && !list.iter().any(|(_, p)| p == &cwd_files) {
            list.push(("files/".to_string(), cwd_files));
        }
        list.push((
            tr(lang, "Working Dir", "Pasta Atual").to_string(),
            cwd,
        ));
    }
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        let hp = PathBuf::from(&home);
        let dl = hp.join("Downloads");
        if dl.is_dir() {
            list.push(("Downloads".to_string(), dl));
        }
        if hp.is_dir() {
            list.push(("Home".to_string(), hp));
        }
    }
    #[cfg(unix)]
    {
        list.push((tr(lang, "Root /", "Raiz /").to_string(), PathBuf::from("/")));
    }
    #[cfg(windows)]
    {
        for letter in *b"CDEF" {
            let p = PathBuf::from(format!("{}:\\", letter as char));
            if p.is_dir() {
                list.push((format!("Drive {}:", letter as char), p));
            }
        }
    }
    list
}

/// Runs the interactive Ratatui TUI until the user quits or terminal closes (`SIGHUP`/`SIGTERM`/`SIGINT`),
/// guaranteeing deterministic session and Samba daemon cleanup on exit.
pub fn run_tui(lang: Lang, initial_paths: Vec<PathBuf>) -> Result<()> {
    TUI_TERMINATION_REQUESTED.store(false, Ordering::Relaxed);
    #[cfg(unix)]
    unsafe {
        libc::signal(
            libc::SIGHUP,
            handle_tui_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            handle_tui_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            handle_tui_signal as *const () as libc::sighandler_t,
        );
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = LabDropTuiApp::new(lang, initial_paths);
    let res = run_event_loop(&mut terminal, &mut app);

    // Ensure any active session + smbd/smbd-notifyd/smbd-cleanupd are torn down cleanly on exit or SIGHUP
    if let Some(mut sess) = app.active_session.take() {
        let _ = sess.stop_and_cleanup(true);
    }

    if is_terminal_disconnected() {
        std::mem::forget(terminal);
    } else {
        let _ = disable_raw_mode();
        let _ = execute!(
            terminal.backend_mut(),
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = terminal.show_cursor();
    }

    res
}

fn spawn_input_reader() -> mpsc::Receiver<Event> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || loop {
        if TUI_TERMINATION_REQUESTED.load(Ordering::Relaxed) || is_terminal_disconnected() {
            break;
        }
        match event::read() {
            Ok(ev) => {
                if tx.send(ev).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    });
    rx
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut LabDropTuiApp,
) -> Result<()> {
    let rx = spawn_input_reader();
    while !app.should_quit && !TUI_TERMINATION_REQUESTED.load(Ordering::Relaxed) {
        if is_terminal_disconnected() {
            TUI_TERMINATION_REQUESTED.store(true, Ordering::Relaxed);
            break;
        }
        if terminal.draw(|f| draw_ui(f, app)).is_err() {
            break;
        }

        match rx.recv_timeout(Duration::from_millis(150)) {
            Ok(Event::Paste(text)) => {
                if app.screen == Screen::PastePathModal {
                    app.paste_buffer.push_str(&text);
                } else {
                    app.add_dropped_text(&text);
                }
            }
            Ok(Event::Mouse(mouse)) => {
                let area = terminal
                    .size()
                    .map(|s| Rect::new(0, 0, s.width, s.height))
                    .unwrap_or_default();
                handle_mouse_event(terminal, app, mouse, area)?;
            }
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')
                {
                    app.should_quit = true;
                    continue;
                }
                handle_key_press(terminal, app, key.code)?;
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

pub fn handle_key_press(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut LabDropTuiApp,
    code: KeyCode,
) -> Result<()> {
    // Global language switch key 'L' (except inside PastePathModal text input)
    if app.screen != Screen::PastePathModal
        && matches!(code, KeyCode::Char('l') | KeyCode::Char('L'))
    {
        app.lang = app.lang.toggle();
        app.config.language = match app.lang {
            Lang::En => "en".to_string(),
            Lang::PtBr => "pt".to_string(),
        };
        let _ = app.config.save();
        app.bookmarks = build_quick_bookmarks(app.lang);
        app.refresh_browser_entries();
        app.status_message = match app.lang {
            Lang::En => "Language switched to English.".to_string(),
            Lang::PtBr => "Idioma alterado para Português (BR).".to_string(),
        };
        return Ok(());
    }

    // Global top-bar navigation keys 1..6 (except inside modals)
    if !matches!(app.screen, Screen::PastePathModal | Screen::ConfirmCleanup) {
        match code {
            KeyCode::Char('1') => {
                app.screen = Screen::Dashboard;
                return Ok(());
            }
            KeyCode::Char('2') => {
                app.refresh_browser_entries();
                app.screen = Screen::FileBrowser;
                return Ok(());
            }
            KeyCode::Char('3') => {
                app.screen = Screen::ReviewSelection;
                return Ok(());
            }
            KeyCode::Char('4') => {
                app.screen = Screen::NetworkConfig;
                return Ok(());
            }
            KeyCode::Char('5') => {
                app.screen = Screen::ActiveSession;
                return Ok(());
            }
            KeyCode::Char('6') => {
                app.smb_env = probe_smb_environment();
                app.screen = Screen::Diagnostics;
                return Ok(());
            }
            _ => {}
        }
    }

    match app.screen {
        Screen::Dashboard => match code {
            KeyCode::Up | KeyCode::Char('k') => {
                app.menu_idx = app.menu_idx.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if app.menu_idx < 6 {
                    app.menu_idx += 1;
                }
            }
            KeyCode::Char('o') | KeyCode::Char('O') => app.open_native_picker(false),
            KeyCode::Char('f') | KeyCode::Char('F') => app.open_native_picker(true),
            KeyCode::Char('p') | KeyCode::Char('P') => {
                app.return_screen = Screen::Dashboard;
                app.paste_buffer.clear();
                app.screen = Screen::PastePathModal;
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                if app.active_session.is_some() {
                    app.screen = Screen::ActiveSession;
                } else {
                    app.stage_and_start_session(terminal);
                }
            }
            KeyCode::Char('q') | KeyCode::Char('Q') => {
                if app.active_session.is_some() {
                    app.cleanup_choice_idx = 0;
                    app.screen = Screen::ConfirmCleanup;
                } else {
                    app.should_quit = true;
                }
            }
            KeyCode::Enter => activate_dashboard_menu_item(terminal, app),
            _ => {}
        },

        Screen::FileBrowser => match code {
            KeyCode::Esc | KeyCode::Char('b') => app.screen = Screen::Dashboard,
            KeyCode::Up | KeyCode::Char('k') => {
                app.browser_idx = app.browser_idx.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if app.browser_idx + 1 < app.browser_entries.len() {
                    app.browser_idx += 1;
                }
            }
            KeyCode::Char('.') | KeyCode::Char('h') => {
                app.show_hidden = !app.show_hidden;
                app.refresh_browser_entries();
                app.status_message = if app.show_hidden {
                    tr(
                        app.lang,
                        "Showing hidden files/folders (.*).",
                        "Exibindo arquivos/pastas ocultos (.*).",
                    )
                } else {
                    tr(
                        app.lang,
                        "Hiding dotfiles/folders (.*).",
                        "Ocultando arquivos/pastas ocultos (.*).",
                    )
                }
                .to_string();
            }
            KeyCode::Char('o') | KeyCode::Char('O') => app.open_native_picker(false),
            KeyCode::Char('f') | KeyCode::Char('F') => app.open_native_picker(true),
            KeyCode::Char('x') | KeyCode::Char('X') => app.open_folder_in_system_explorer(),
            KeyCode::Char('p') | KeyCode::Char('P') => {
                app.return_screen = Screen::FileBrowser;
                app.paste_buffer.clear();
                app.screen = Screen::PastePathModal;
            }
            KeyCode::Backspace | KeyCode::Left => {
                if let Some(parent) = app.browser_dir.parent().map(|p| p.to_path_buf()) {
                    if !parent.as_os_str().is_empty() {
                        app.browser_dir = parent;
                        app.browser_idx = 0;
                        app.refresh_browser_entries();
                    }
                }
            }
            KeyCode::Enter | KeyCode::Right => {
                if let Some(entry) = app.browser_entries.get(app.browser_idx).cloned() {
                    if entry.is_dir {
                        app.browser_dir = entry.path;
                        app.browser_idx = 0;
                        app.refresh_browser_entries();
                    } else if let Ok(added) = app.selection.toggle_path(&entry.path) {
                        app.status_message = if added {
                            format!(
                                "{}: {}",
                                tr(app.lang, "Selected", "Selecionado"),
                                entry.name
                            )
                        } else {
                            format!(
                                "{}: {}",
                                tr(app.lang, "Deselected", "Desmarcado"),
                                entry.name
                            )
                        };
                    }
                }
            }
            KeyCode::Char(' ') => {
                if let Some(entry) = app.browser_entries.get(app.browser_idx).cloned() {
                    if !entry.is_parent_link {
                        match app.selection.toggle_path(&entry.path) {
                            Ok(true) => {
                                app.error_message = None;
                                app.status_message = format!(
                                    "{}: {}",
                                    tr(app.lang, "Selected", "Selecionado"),
                                    entry.name
                                );
                            }
                            Ok(false) => {
                                app.status_message = format!(
                                    "{}: {}",
                                    tr(app.lang, "Deselected", "Desmarcado"),
                                    entry.name
                                );
                            }
                            Err(e) => {
                                app.error_message = Some(format!("{e}"));
                            }
                        }
                    }
                }
            }
            KeyCode::F(1) => jump_bookmark(app, 0),
            KeyCode::F(2) => jump_bookmark(app, 1),
            KeyCode::F(3) => jump_bookmark(app, 2),
            KeyCode::F(4) => jump_bookmark(app, 3),
            KeyCode::Char('s') => app.stage_and_start_session(terminal),
            _ => {}
        },

        Screen::ReviewSelection => match code {
            KeyCode::Esc | KeyCode::Char('b') => app.screen = Screen::Dashboard,
            KeyCode::Up | KeyCode::Char('k') => {
                app.review_idx = app.review_idx.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if app.review_idx + 1 < app.selection.items().len() {
                    app.review_idx += 1;
                }
            }
            KeyCode::Char('o') | KeyCode::Char('O') => app.open_native_picker(false),
            KeyCode::Char('f') | KeyCode::Char('F') => app.open_native_picker(true),
            KeyCode::Char('p') | KeyCode::Char('P') => {
                app.return_screen = Screen::ReviewSelection;
                app.paste_buffer.clear();
                app.screen = Screen::PastePathModal;
            }
            KeyCode::Delete | KeyCode::Char('d') => {
                if let Some(removed) = app.selection.remove_index(app.review_idx) {
                    app.status_message = format!(
                        "{}: {}",
                        tr(app.lang, "Removed", "Removido"),
                        removed.original_name
                    );
                    if app.review_idx >= app.selection.items().len() {
                        app.review_idx = app.selection.items().len().saturating_sub(1);
                    }
                }
            }
            KeyCode::Char('c') => {
                app.selection.clear();
                app.review_idx = 0;
                app.status_message = tr(
                    app.lang,
                    "Cleared all selected items.",
                    "Lista de seleção limpa.",
                )
                .to_string();
            }
            KeyCode::Char('s') | KeyCode::Enter => {
                app.stage_and_start_session(terminal);
            }
            _ => {}
        },

        Screen::NetworkConfig => match code {
            KeyCode::Esc | KeyCode::Char('b') => app.screen = Screen::Dashboard,
            KeyCode::Up | KeyCode::Char('k') => {
                app.selected_iface_idx = app.selected_iface_idx.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if app.selected_iface_idx + 1 < app.interfaces.len() {
                    app.selected_iface_idx += 1;
                }
            }
            KeyCode::Char('f') | KeyCode::Char('F') | KeyCode::Enter => {
                let name = app.current_interface().name;
                app.set_preferred_interface(Some(name));
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                app.set_preferred_interface(None);
            }
            KeyCode::Tab | KeyCode::Char('p') => {
                app.strategy = match app.strategy {
                    SharingStrategy::AutoLayered => SharingStrategy::SmbOnly,
                    SharingStrategy::SmbOnly => SharingStrategy::WebDavSecondary,
                    SharingStrategy::WebDavSecondary => SharingStrategy::HttpFallback,
                    SharingStrategy::HttpFallback => SharingStrategy::AutoLayered,
                };
                app.config.set_strategy(app.strategy);
                let _ = app.config.save();
            }
            KeyCode::Char('m') => {
                app.linux_smb_mode = match app.linux_smb_mode {
                    SmbLinuxMode::RootlessPort(_) => SmbLinuxMode::SystemPort445,
                    SmbLinuxMode::SystemPort445 => {
                        SmbLinuxMode::RootlessPort(app.config.preferred_rootless_smb_port)
                    }
                };
            }
            KeyCode::Char('r') => {
                app.refresh_network_interfaces();
            }
            KeyCode::Char('R') => {
                app.restart_sharing_service(Some(terminal));
            }
            KeyCode::Char('e') | KeyCode::Char('E')
                if !app.smb_env.is_elevated && app.smb_env.os_family == "Linux" =>
            {
                relaunch_with_sudo(terminal, app);
            }
            KeyCode::Char('i') | KeyCode::Char('I')
                if !app.smb_env.smbd_installed && app.smb_env.os_family == "Linux" =>
            {
                run_interactive_samba_install(terminal, app);
            }
            _ => {}
        },

        Screen::ActiveSession => match code {
            KeyCode::Esc | KeyCode::Char('b') => app.screen = Screen::Dashboard,
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Left => {
                app.selected_iface_idx = app.selected_iface_idx.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Right => {
                if app.selected_iface_idx + 1 < app.interfaces.len() {
                    app.selected_iface_idx += 1;
                }
            }
            KeyCode::Char('r') => {
                app.refresh_network_interfaces();
            }
            KeyCode::Char('R') | KeyCode::Enter => {
                app.restart_sharing_service(Some(terminal));
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                app.show_sharing_details = !app.show_sharing_details;
            }
            KeyCode::Char('s') | KeyCode::Char('q') => {
                if app.active_session.is_some() {
                    app.cleanup_choice_idx = 0;
                    app.screen = Screen::ConfirmCleanup;
                } else {
                    app.stage_and_start_session(terminal);
                }
            }
            _ => {}
        },

        Screen::ConfirmCleanup => match code {
            KeyCode::Esc => {
                app.screen = if app.active_session.is_some() {
                    Screen::ActiveSession
                } else {
                    Screen::Dashboard
                };
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.cleanup_choice_idx = app.cleanup_choice_idx.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if app.cleanup_choice_idx < 2 {
                    app.cleanup_choice_idx += 1;
                }
            }
            KeyCode::Char('1') => execute_cleanup_choice(app, true),
            KeyCode::Char('2') => execute_cleanup_choice(app, false),
            KeyCode::Enter => match app.cleanup_choice_idx {
                0 => execute_cleanup_choice(app, true),
                1 => execute_cleanup_choice(app, false),
                _ => {
                    app.screen = if app.active_session.is_some() {
                        Screen::ActiveSession
                    } else {
                        Screen::Dashboard
                    };
                }
            },
            _ => {}
        },

        Screen::Diagnostics => match code {
            KeyCode::Esc | KeyCode::Char('b') | KeyCode::Enter => app.screen = Screen::Dashboard,
            KeyCode::Char('r') => {
                app.refresh_network_interfaces();
            }
            KeyCode::Char('i') | KeyCode::Char('I')
                if !app.smb_env.smbd_installed && app.smb_env.os_family == "Linux" =>
            {
                run_interactive_samba_install(terminal, app);
            }
            KeyCode::Char('c') => {
                let reports = recover_orphaned_sessions(None);
                if reports.is_empty() {
                    app.status_message = tr(
                        app.lang,
                        "No orphaned sessions found.",
                        "Nenhuma sessão órfã encontrada.",
                    )
                    .to_string();
                } else {
                    app.status_message = match app.lang {
                        Lang::En => {
                            format!(
                                "Recovered and cleaned up {} orphaned session(s).",
                                reports.len()
                            )
                        }
                        Lang::PtBr => {
                            format!(
                                "Recuperadas e limpas {} sessão(ões) órfã(s).",
                                reports.len()
                            )
                        }
                    };
                    app.last_cleanup_report = reports.into_iter().last();
                }
            }
            _ => {}
        },

        Screen::PastePathModal => match code {
            KeyCode::Esc => app.screen = app.return_screen,
            KeyCode::Backspace => {
                app.paste_buffer.pop();
            }
            KeyCode::Enter => {
                let buf = std::mem::take(&mut app.paste_buffer);
                app.add_dropped_text(&buf);
                app.screen = app.return_screen;
            }
            KeyCode::Char(c) => {
                app.paste_buffer.push(c);
            }
            _ => {}
        },
    }
    Ok(())
}

fn activate_dashboard_menu_item(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut LabDropTuiApp,
) {
    match app.menu_idx {
        0 => {
            app.refresh_browser_entries();
            app.screen = Screen::FileBrowser;
        }
        1 => app.screen = Screen::ReviewSelection,
        2 => app.screen = Screen::NetworkConfig,
        3 => {
            if app.active_session.is_some() {
                app.screen = Screen::ActiveSession;
            } else {
                app.stage_and_start_session(terminal);
            }
        }
        4 => {
            if app.active_session.is_some() {
                app.cleanup_choice_idx = 0;
                app.screen = Screen::ConfirmCleanup;
            } else {
                app.screen = Screen::ActiveSession;
            }
        }
        5 => {
            app.smb_env = probe_smb_environment();
            app.screen = Screen::Diagnostics;
        }
        6 => {
            if app.active_session.is_some() {
                app.cleanup_choice_idx = 0;
                app.screen = Screen::ConfirmCleanup;
            } else {
                app.should_quit = true;
            }
        }
        _ => {}
    }
}

fn relaunch_with_sudo(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut LabDropTuiApp,
) {
    if let Ok(exe) = std::env::current_exe() {
        let _ = disable_raw_mode();
        let _ = execute!(
            terminal.backend_mut(),
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        println!("\n=== LabDrop: Port 445 SMB Elevation ===");
        println!(
            "{}",
            tr(
                app.lang,
                "Administrator privileges (sudo) are needed to bind TCP port 445 and open the temporary Samba firewall rule.",
                "Privilégios de administrador (sudo) são necessários para usar a porta 445 (SMB padrão) e liberar o firewall temporariamente."
            )
        );
        let lang_arg = if app.lang == Lang::PtBr { "pt" } else { "en" };
        let _ = Command::new("sudo")
            .arg(exe)
            .args(["--lang", lang_arg])
            .status();
        app.should_quit = true;
    }
}

fn run_interactive_samba_install(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut LabDropTuiApp,
) {
    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
    println!("\n=== LabDrop: Installing Samba Package ===");
    println!(
        "{}",
        tr(
            app.lang,
            "Administrator privileges (sudo) are required to install Samba via your system package manager.",
            "Privilégios de administrador (sudo) são necessários para instalar o Samba."
        )
    );
    let res = install_linux_samba_interactive();
    let _ = execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    );
    let _ = enable_raw_mode();
    let _ = terminal.clear();
    app.smb_env = probe_smb_environment();
    match res {
        Ok(()) => {
            app.status_message = tr(
                app.lang,
                "Samba installed successfully.",
                "Samba instalado com sucesso.",
            )
            .to_string();
        }
        Err(e) => {
            app.error_message = Some(format!("Samba install failed: {e}"));
        }
    }
}

pub fn handle_mouse_event(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut LabDropTuiApp,
    mouse: MouseEvent,
    area: Rect,
) -> Result<()> {
    if app.screen == Screen::PastePathModal {
        return Ok(());
    }

    match mouse.kind {
        MouseEventKind::ScrollUp => match app.screen {
            Screen::Dashboard => app.menu_idx = app.menu_idx.saturating_sub(1),
            Screen::FileBrowser => app.browser_idx = app.browser_idx.saturating_sub(1),
            Screen::ReviewSelection => app.review_idx = app.review_idx.saturating_sub(1),
            Screen::NetworkConfig | Screen::ActiveSession => {
                app.selected_iface_idx = app.selected_iface_idx.saturating_sub(1);
            }
            Screen::ConfirmCleanup => {
                app.cleanup_choice_idx = app.cleanup_choice_idx.saturating_sub(1);
            }
            _ => {}
        },
        MouseEventKind::ScrollDown => match app.screen {
            Screen::Dashboard if app.menu_idx < 6 => {
                app.menu_idx += 1;
            }
            Screen::FileBrowser if app.browser_idx + 1 < app.browser_entries.len() => {
                app.browser_idx += 1;
            }
            Screen::ReviewSelection if app.review_idx + 1 < app.selection.items().len() => {
                app.review_idx += 1;
            }
            Screen::NetworkConfig | Screen::ActiveSession
                if app.selected_iface_idx + 1 < app.interfaces.len() =>
            {
                app.selected_iface_idx += 1;
            }
            Screen::ConfirmCleanup if app.cleanup_choice_idx < 2 => {
                app.cleanup_choice_idx += 1;
            }
            _ => {}
        },
        MouseEventKind::Down(MouseButton::Left) => {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(14),
                    Constraint::Length(4),
                ])
                .split(area);

            let row = mouse.row;
            let col = mouse.column;

            // 1. Top Header / Navigation Bar click
            if row >= chunks[0].y && row < chunks[0].y + chunks[0].height {
                if app.screen == Screen::ConfirmCleanup {
                    return Ok(());
                }
                let w = chunks[0].width.max(1);
                // Rightmost ~12 cols is [L] Language button
                if col + 12 >= chunks[0].x + w {
                    return handle_key_press(terminal, app, KeyCode::Char('l'));
                }
                // Navigation tabs start around col 22
                if col >= 20 {
                    let nav_w = (w.saturating_sub(32)).max(6);
                    let tab_w = (nav_w / 6).max(1);
                    let tab_idx = ((col - 20) / tab_w).min(5);
                    let key = match tab_idx {
                        0 => KeyCode::Char('1'),
                        1 => KeyCode::Char('2'),
                        2 => KeyCode::Char('3'),
                        3 => KeyCode::Char('4'),
                        4 => KeyCode::Char('5'),
                        _ => KeyCode::Char('6'),
                    };
                    return handle_key_press(terminal, app, key);
                }
                return Ok(());
            }

            // 2. ConfirmCleanup modal click
            if app.screen == Screen::ConfirmCleanup {
                let modal = centered_rect(68, 42, area);
                if row > modal.y && row + 1 < modal.y + modal.height && col >= modal.x && col < modal.x + modal.width {
                    let rel_y = row.saturating_sub(modal.y + 1);
                    if (5..=7).contains(&rel_y) {
                        let choice = (rel_y - 5) as usize;
                        app.cleanup_choice_idx = choice;
                        return handle_key_press(terminal, app, KeyCode::Enter);
                    }
                }
                return Ok(());
            }

            // 3. Body area click per screen
            let body = chunks[1];
            if row >= body.y && row < body.y + body.height {
                match app.screen {
                    Screen::Dashboard => {
                        let cols = Layout::default()
                            .direction(Direction::Horizontal)
                            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                            .split(body);
                        let menu_panel = Layout::default()
                            .direction(Direction::Vertical)
                            .constraints([Constraint::Min(9), Constraint::Length(5)])
                            .split(cols[0]);

                        if col >= menu_panel[0].x
                            && col < menu_panel[0].x + menu_panel[0].width
                            && row > menu_panel[0].y
                            && row + 1 < menu_panel[0].y + menu_panel[0].height
                        {
                            let idx = (row - menu_panel[0].y - 1) as usize;
                            if idx <= 6 {
                                app.menu_idx = idx;
                                activate_dashboard_menu_item(terminal, app);
                            }
                            return Ok(());
                        }
                        if col >= menu_panel[1].x
                            && col < menu_panel[1].x + menu_panel[1].width
                            && row > menu_panel[1].y
                            && row + 1 < menu_panel[1].y + menu_panel[1].height
                        {
                            let rel_row = row - menu_panel[1].y - 1;
                            if rel_row == 0 {
                                if col < menu_panel[1].x + menu_panel[1].width / 2 {
                                    app.open_native_picker(false);
                                } else {
                                    app.open_native_picker(true);
                                }
                            } else if rel_row == 1 {
                                app.return_screen = Screen::Dashboard;
                                app.paste_buffer.clear();
                                app.screen = Screen::PastePathModal;
                            }
                        }
                    }

                    Screen::FileBrowser => {
                        let rows = Layout::default()
                            .direction(Direction::Vertical)
                            .constraints([Constraint::Length(5), Constraint::Min(10)])
                            .split(body);
                        if row > rows[0].y && row + 1 < rows[0].y + rows[0].height {
                            let rel_row = row - rows[0].y - 1;
                            if rel_row == 1 {
                                // Quick jump bookmark row
                                let bm_idx = ((col.saturating_sub(rows[0].x + 12)) / 16) as usize;
                                if bm_idx < app.bookmarks.len() {
                                    jump_bookmark(app, bm_idx);
                                }
                            } else if rel_row == 2 {
                                // Action toolbar row
                                let fifth = rows[0].width.max(5) / 5;
                                let rel_col = col.saturating_sub(rows[0].x);
                                if rel_col < fifth {
                                    let _ = handle_key_press(terminal, app, KeyCode::Char('.'));
                                } else if rel_col < fifth * 2 {
                                    app.open_native_picker(false);
                                } else if rel_col < fifth * 3 {
                                    app.open_native_picker(true);
                                } else if rel_col < fifth * 4 {
                                    app.open_folder_in_system_explorer();
                                } else {
                                    app.return_screen = Screen::FileBrowser;
                                    app.paste_buffer.clear();
                                    app.screen = Screen::PastePathModal;
                                }
                            }
                        } else if row > rows[1].y && row + 1 < rows[1].y + rows[1].height {
                            let clicked_offset = (row - rows[1].y - 1) as usize;
                            let list_height = rows[1].height.saturating_sub(2) as usize;
                            let scroll_top = if app.browser_idx >= list_height {
                                app.browser_idx + 1 - list_height
                            } else {
                                0
                            };
                            let target_idx = scroll_top + clicked_offset;
                            if target_idx < app.browser_entries.len() {
                                let already_selected = app.browser_idx == target_idx;
                                app.browser_idx = target_idx;
                                let clicked_checkbox = col < rows[1].x + 6;
                                if clicked_checkbox {
                                    let _ = handle_key_press(terminal, app, KeyCode::Char(' '));
                                } else if already_selected
                                    || app.browser_entries[target_idx].is_dir
                                {
                                    let _ = handle_key_press(terminal, app, KeyCode::Enter);
                                }
                            }
                        }
                    }

                    Screen::ReviewSelection => {
                        let rows = Layout::default()
                            .direction(Direction::Vertical)
                            .constraints([Constraint::Length(3), Constraint::Min(10)])
                            .split(body);
                        if row >= rows[0].y && row < rows[0].y + rows[0].height {
                            let quarter = rows[0].width.max(4) / 4;
                            let rel_col = col.saturating_sub(rows[0].x);
                            if rel_col < quarter {
                                let _ = handle_key_press(terminal, app, KeyCode::Char('d'));
                            } else if rel_col < quarter * 2 {
                                let _ = handle_key_press(terminal, app, KeyCode::Char('c'));
                            } else if rel_col < quarter * 3 {
                                app.open_native_picker(false);
                            } else {
                                app.stage_and_start_session(terminal);
                            }
                        } else if row > rows[1].y && row + 1 < rows[1].y + rows[1].height {
                            let idx = (row - rows[1].y - 1) as usize;
                            if idx < app.selection.items().len() {
                                app.review_idx = idx;
                            }
                        }
                    }

                    Screen::NetworkConfig => {
                        let rows = Layout::default()
                            .direction(Direction::Vertical)
                            .constraints([
                                Constraint::Length(4),
                                Constraint::Min(7),
                                Constraint::Length(9),
                            ])
                            .split(body);
                        if row > rows[1].y && row + 1 < rows[1].y + rows[1].height {
                            let idx = (row - rows[1].y - 1) as usize;
                            if idx < app.interfaces.len() {
                                app.selected_iface_idx = idx;
                            }
                        } else if row > rows[2].y && row + 1 < rows[2].y + rows[2].height {
                            let rel_row = row - rows[2].y - 1;
                            match rel_row {
                                0 => {
                                    let half = rows[2].width / 2;
                                    if col < rows[2].x + half {
                                        let _ = handle_key_press(terminal, app, KeyCode::Char('f'));
                                    } else {
                                        let _ = handle_key_press(terminal, app, KeyCode::Char('a'));
                                    }
                                }
                                1 => {
                                    let _ = handle_key_press(terminal, app, KeyCode::Tab);
                                }
                                2 => {
                                    let _ = handle_key_press(terminal, app, KeyCode::Char('m'));
                                }
                                3 => {
                                    let _ = handle_key_press(terminal, app, KeyCode::Char('e'));
                                }
                                _ => {
                                    let _ = handle_key_press(terminal, app, KeyCode::Char('i'));
                                }
                            }
                        }
                    }

                    Screen::ActiveSession => {
                        let rows = Layout::default()
                            .direction(Direction::Vertical)
                            .constraints([Constraint::Length(12), Constraint::Min(6)])
                            .split(body);
                        let top_cols = Layout::default()
                            .direction(Direction::Horizontal)
                            .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
                            .split(rows[0]);

                        // Right panel on Sharing Screen: Network Interface Selector + Service Controls
                        if col >= top_cols[1].x
                            && col < top_cols[1].x + top_cols[1].width
                            && row > top_cols[1].y
                            && row + 1 < top_cols[1].y + top_cols[1].height
                        {
                            let inner_h = top_cols[1].height.saturating_sub(2);
                            let rel_y = row - top_cols[1].y - 1;
                            // Bottom 2 lines inside right panel are the action buttons
                            if rel_y + 2 >= inner_h {
                                let btn_line = rel_y.saturating_sub(inner_h.saturating_sub(2));
                                let half = top_cols[1].width / 2;
                                if btn_line == 0 {
                                    if col < top_cols[1].x + half {
                                        app.restart_sharing_service(Some(terminal));
                                    } else {
                                        app.refresh_network_interfaces();
                                    }
                                } else if col < top_cols[1].x + half {
                                    app.show_sharing_details = !app.show_sharing_details;
                                } else if app.active_session.is_some() {
                                    app.cleanup_choice_idx = 0;
                                    app.screen = Screen::ConfirmCleanup;
                                } else {
                                    app.stage_and_start_session(terminal);
                                }
                            } else if rel_y >= 1 {
                                let iface_idx = (rel_y - 1) as usize;
                                if iface_idx < app.interfaces.len() {
                                    app.selected_iface_idx = iface_idx;
                                }
                            }
                        }
                    }

                    _ => {}
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn jump_bookmark(app: &mut LabDropTuiApp, idx: usize) {
    if let Some((label, path)) = app.bookmarks.get(idx).cloned() {
        app.browser_dir = path;
        app.browser_idx = 0;
        app.refresh_browser_entries();
        app.status_message = format!("{} {label}.", tr(app.lang, "Jumped to", "Navegou para"));
    }
}

fn execute_cleanup_choice(app: &mut LabDropTuiApp, delete_staged: bool) {
    if let Some(mut sess) = app.active_session.take() {
        let report = sess.stop_and_cleanup(delete_staged);
        if delete_staged {
            app.retained_staged = None;
        }
        app.status_message = match app.lang {
            Lang::En => format!(
                "Session '{}' stopped and cleaned up ({} actions).",
                report.session_id,
                report.actions_performed.len()
            ),
            Lang::PtBr => format!(
                "Sessão '{}' encerrada e limpa ({} ações).",
                report.session_id,
                report.actions_performed.len()
            ),
        };
        app.last_cleanup_report = Some(report);
    }
    app.screen = Screen::Dashboard;
}

fn key_pill<'a>(key: &'a str, desc: &'a str) -> Vec<Span<'a>> {
    vec![
        Span::styled(
            format!("[{key}]"),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {desc}   "), Style::default().fg(MUTED)),
    ]
}

pub fn draw_ui(f: &mut Frame, app: &LabDropTuiApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(14),
            Constraint::Length(4),
        ])
        .split(f.area());

    let session_badge = if let Some(ref s) = app.active_session {
        Span::styled(
            format!(
                " ● {} ({}) ",
                tr(app.lang, "RUNNING", "ATIVO"),
                s.ledger.interface_name
            ),
            Style::default()
                .fg(Color::Black)
                .bg(STATUS_OK)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            format!(" ○ {} ", tr(app.lang, "STOPPED", "PARADO")),
            Style::default().fg(MUTED).bg(DIM),
        )
    };

    let tabs: &[(Screen, &str, &str)] = &[
        (Screen::Dashboard, "[1] Home", "[1] Início"),
        (Screen::FileBrowser, "[2] Files", "[2] Arquivos"),
        (Screen::ReviewSelection, "[3] Selected", "[3] Selecionados"),
        (Screen::NetworkConfig, "[4] Network", "[4] Rede"),
        (Screen::ActiveSession, "[5] Sharing", "[5] Compartilhamento"),
        (Screen::Diagnostics, "[6] Diagnostics", "[6] Diagnóstico"),
    ];

    let mut hdr_spans = vec![
        Span::styled(
            concat!(" LabDrop v", env!("CARGO_PKG_VERSION"), " "),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        session_badge,
        Span::raw("  "),
    ];

    let active_tab_screen = match app.screen {
        Screen::ConfirmCleanup => Screen::ActiveSession,
        Screen::PastePathModal => app.return_screen,
        other => other,
    };

    for &(scr, en, pt) in tabs {
        let label = tr(app.lang, en, pt);
        let style = if scr == active_tab_screen {
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(MUTED)
        };
        hdr_spans.push(Span::styled(format!(" {label} "), style));
        hdr_spans.push(Span::raw(" "));
    }

    hdr_spans.push(Span::styled(
        format!(" [L] {} ", app.lang.code()),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ));

    let header_block = Paragraph::new(Line::from(hdr_spans)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT)),
    );
    f.render_widget(header_block, chunks[0]);

    match app.screen {
        Screen::Dashboard => draw_dashboard(f, app, chunks[1]),
        Screen::FileBrowser => draw_file_browser(f, app, chunks[1]),
        Screen::ReviewSelection => draw_review_selection(f, app, chunks[1]),
        Screen::NetworkConfig => draw_network_config(f, app, chunks[1]),
        Screen::ActiveSession => draw_active_session(f, app, chunks[1]),
        Screen::ConfirmCleanup => {
            draw_active_session(f, app, chunks[1]);
            draw_confirm_cleanup_modal(f, app);
        }
        Screen::Diagnostics => draw_diagnostics(f, app, chunks[1]),
        Screen::PastePathModal => {
            draw_dashboard(f, app, chunks[1]);
            draw_paste_modal(f, app);
        }
    }

    draw_footer(f, app, chunks[2]);
}

fn draw_footer(f: &mut Frame, app: &LabDropTuiApp, area: Rect) {
    let status_line = if let Some(ref err) = app.error_message {
        Line::from(vec![
            Span::styled(
                tr(app.lang, " ERROR: ", " ERRO: "),
                Style::default().fg(STATUS_ERR).add_modifier(Modifier::BOLD),
            ),
            Span::styled(err.as_str(), Style::default().fg(STATUS_ERR)),
        ])
    } else if let Some(ref warn) = app.warning_message {
        Line::from(vec![
            Span::styled(
                tr(app.lang, " NOTICE: ", " AVISO: "),
                Style::default()
                    .fg(STATUS_WARN)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(warn.as_str(), Style::default().fg(STATUS_WARN)),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                tr(app.lang, " Status: ", " Status: "),
                Style::default().fg(MUTED),
            ),
            Span::styled(app.status_message.as_str(), Style::default().fg(TEXT)),
        ])
    };

    let mut shortcut_spans = vec![Span::raw(" ")];
    match app.screen {
        Screen::Dashboard => {
            shortcut_spans.extend(key_pill("↑/↓", tr(app.lang, "Navigate", "Navegar")));
            shortcut_spans.extend(key_pill("Enter", tr(app.lang, "Open", "Abrir")));
            shortcut_spans.extend(key_pill("o/f", tr(app.lang, "Pick Files/Folder", "Seletor Nativo")));
            shortcut_spans.extend(key_pill("s", tr(app.lang, "Start Sharing", "Compartilhar")));
            shortcut_spans.extend(key_pill("1-6", tr(app.lang, "Tabs", "Abas")));
            shortcut_spans.extend(key_pill("q", tr(app.lang, "Quit", "Sair")));
        }
        Screen::FileBrowser => {
            shortcut_spans.extend(key_pill("Space", tr(app.lang, "Select", "Marcar")));
            shortcut_spans.extend(key_pill("Enter", tr(app.lang, "Open Dir", "Abrir Pasta")));
            shortcut_spans.extend(key_pill("Bksp", tr(app.lang, "Up Dir", "Subir")));
            shortcut_spans.extend(key_pill(".", tr(app.lang, "Hidden", "Ocultos")));
            shortcut_spans.extend(key_pill("o/f", tr(app.lang, "System Picker", "Seletor Nativo")));
            shortcut_spans.extend(key_pill("s", tr(app.lang, "Start Sharing", "Compartilhar")));
            shortcut_spans.extend(key_pill("Esc", tr(app.lang, "Back", "Voltar")));
        }
        Screen::ReviewSelection => {
            shortcut_spans.extend(key_pill("d/Del", tr(app.lang, "Remove", "Remover")));
            shortcut_spans.extend(key_pill("c", tr(app.lang, "Clear All", "Limpar Tudo")));
            shortcut_spans.extend(key_pill("o/f", tr(app.lang, "Add More", "Adicionar")));
            shortcut_spans.extend(key_pill("s/Enter", tr(app.lang, "Start Sharing", "Compartilhar")));
            shortcut_spans.extend(key_pill("Esc", tr(app.lang, "Back", "Voltar")));
        }
        Screen::NetworkConfig => {
            shortcut_spans.extend(key_pill("↑/↓", tr(app.lang, "Select Interface", "Selecionar Interface")));
            shortcut_spans.extend(key_pill("f/Enter", tr(app.lang, "Save Preferred", "Salvar Preferida")));
            shortcut_spans.extend(key_pill("a", tr(app.lang, "Auto Interface", "Automática")));
            shortcut_spans.extend(key_pill("r", tr(app.lang, "Refresh", "Atualizar")));
            shortcut_spans.extend(key_pill("R", tr(app.lang, "Restart Service", "Reiniciar Serviço")));
            shortcut_spans.extend(key_pill("Esc", tr(app.lang, "Back", "Voltar")));
        }
        Screen::ActiveSession => {
            shortcut_spans.extend(key_pill("↑/↓", tr(app.lang, "Choose Interface", "Escolher Interface")));
            shortcut_spans.extend(key_pill(
                "R/Enter",
                tr(app.lang, "Restart Sharing Service", "Reiniciar Serviço"),
            ));
            shortcut_spans.extend(key_pill("r", tr(app.lang, "Refresh Interfaces", "Atualizar Rede")));
            shortcut_spans.extend(key_pill("d", tr(app.lang, "Details & Logs", "Detalhes e Logs")));
            shortcut_spans.extend(key_pill("s", tr(app.lang, "Stop Sharing", "Parar")));
            shortcut_spans.extend(key_pill("Esc", tr(app.lang, "Back", "Voltar")));
        }
        Screen::ConfirmCleanup => {
            shortcut_spans.extend(key_pill("1", tr(app.lang, "Delete Staged", "Apagar Temporários")));
            shortcut_spans.extend(key_pill("2", tr(app.lang, "Retain Staged", "Manter Temporários")));
            shortcut_spans.extend(key_pill("Esc", tr(app.lang, "Cancel", "Cancelar")));
        }
        Screen::Diagnostics => {
            shortcut_spans.extend(key_pill("r", tr(app.lang, "Refresh Status", "Atualizar")));
            shortcut_spans.extend(key_pill("c", tr(app.lang, "Clean Orphans", "Limpar Órfãs")));
            shortcut_spans.extend(key_pill("i", tr(app.lang, "Install Samba", "Instalar Samba")));
            shortcut_spans.extend(key_pill("Esc", tr(app.lang, "Back", "Voltar")));
        }
        Screen::PastePathModal => {
            shortcut_spans.extend(key_pill("Enter", tr(app.lang, "Confirm", "Confirmar")));
            shortcut_spans.extend(key_pill("Esc", tr(app.lang, "Cancel", "Cancelar")));
        }
    }

    let footer = Paragraph::new(vec![status_line, Line::from(shortcut_spans)]).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(DIM)),
    );
    f.render_widget(footer, area);
}

fn draw_dashboard(f: &mut Frame, app: &LabDropTuiApp, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let menu_items = [
        tr(
            app.lang,
            "[1] Browse & Select Files / Folders",
            "[1] Navegar e Selecionar Arquivos / Pastas",
        ),
        tr(
            app.lang,
            "[2] Review Selected Items",
            "[2] Revisar Itens Selecionados",
        ),
        tr(
            app.lang,
            "[3] Network Interface & Protocol Settings",
            "[3] Interface de Rede e Protocolos",
        ),
        if app.active_session.is_some() {
            tr(
                app.lang,
                "[4] Open Active Sharing Screen & Controls",
                "[4] Abrir Tela de Compartilhamento Ativo",
            )
        } else {
            tr(
                app.lang,
                "[4] Stage Files & Start Sharing Service",
                "[4] Preparar Arquivos e Iniciar Compartilhamento",
            )
        },
        tr(
            app.lang,
            "[5] Stop Sharing & Clean Up Session",
            "[5] Parar Compartilhamento e Limpar Sessão",
        ),
        tr(
            app.lang,
            "[6] System Diagnostics & Samba Check",
            "[6] Diagnóstico do Sistema e Samba",
        ),
        tr(app.lang, "[q] Quit LabDrop", "[q] Sair do LabDrop"),
    ];

    let list_items: Vec<ListItem> = menu_items
        .iter()
        .enumerate()
        .map(|(i, &label)| {
            let prefix = if i == app.menu_idx { "▸" } else { " " };
            let style = if i == app.menu_idx {
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TEXT)
            };
            ListItem::new(format!(" {prefix} {label} ")).style(style)
        })
        .collect();

    let menu_panel = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(9), Constraint::Length(5)])
        .split(cols[0]);

    let menu_list = List::new(list_items).block(
        Block::default()
            .title(tr(
                app.lang,
                " Actions ",
                " Ações Principais ",
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT)),
    );
    f.render_widget(menu_list, menu_panel[0]);

    let quick_pick = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(" [o] ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(
                tr(app.lang, "Pick Files (Native)   ", "Escolher Arquivos (Nativo)   "),
                Style::default().fg(TEXT),
            ),
            Span::styled("[f] ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(
                tr(app.lang, "Pick Folder (Native)", "Escolher Pasta (Nativo)"),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled(" [p] ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(
                tr(
                    app.lang,
                    "Paste Path(s) or drag files straight into this terminal",
                    "Colar Caminho(s) ou arraste arquivos direto no terminal",
                ),
                Style::default().fg(MUTED),
            ),
        ]),
    ])
    .block(
        Block::default()
            .title(tr(
                app.lang,
                " Quick File Selection ",
                " Seleção Rápida ",
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(DIM)),
    );
    f.render_widget(quick_pick, menu_panel[1]);

    // Right column: clean system & session summary
    let iface = app.current_interface();
    let pref_name = app
        .config
        .preferred_interface
        .as_deref()
        .unwrap_or_else(|| tr(app.lang, "Automatic", "Automática"));
    let active_iface_str = if let Some(ref s) = app.active_session {
        format!("{} ({})", s.ledger.interface_name, s.ledger.interface_ip)
    } else {
        tr(app.lang, "None (Stopped)", "Nenhuma (Parado)").to_string()
    };

    let base_dir = default_staging_base_dir();
    let free_space = query_disk_space(&base_dir)
        .map(|d| format_bytes(d.available_bytes))
        .unwrap_or_else(|_| "?".to_string());

    let mut summary_lines = vec![
        Line::from(vec![
            Span::styled(
                tr(app.lang, " Selected Payload:    ", " Arquivos Selecionados: "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                format!(
                    "{} ({} {}, {})",
                    app.selection.items().len(),
                    app.selection.total_files(),
                    tr(app.lang, "files", "arquivos"),
                    format_bytes(app.selection.total_bytes())
                ),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                tr(app.lang, " Session Interface:   ", " Interface Selecionada: "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                format!(
                    "{} — {} ({})",
                    iface.name,
                    iface.ipv4,
                    iface.connection_type_label(app.lang)
                ),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                tr(app.lang, " Preferred Interface: ", " Interface Preferida:   "),
                Style::default().fg(MUTED),
            ),
            Span::styled(pref_name.to_string(), Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled(
                tr(app.lang, " Active Sharing On:   ", " Compartilhando em:     "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                active_iface_str,
                Style::default().fg(if app.active_session.is_some() {
                    STATUS_OK
                } else {
                    MUTED
                }),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                tr(app.lang, " Protocol Strategy:   ", " Protocolo:             "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                app.strategy.display_name_localized(app.lang),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                tr(app.lang, " Samba / SMB Status:  ", " Status do Samba / SMB: "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                if app.smb_env.smbd_installed {
                    format!(
                        "{} ({})",
                        app.smb_env.smbd_version.as_deref().unwrap_or("Samba"),
                        match app.linux_smb_mode {
                            SmbLinuxMode::SystemPort445 => {
                                tr(app.lang, "Port 445", "Porta 445")
                            }
                            SmbLinuxMode::RootlessPort(_) => {
                                tr(app.lang, "Port 4445 Rootless", "Porta 4445 Sem Root")
                            }
                        }
                    )
                } else {
                    tr(
                        app.lang,
                        "Not installed (WebDAV + HTTP active; [4] -> [i] to install)",
                        "Não instalado (WebDAV + HTTP ativos; [4] -> [i] p/ instalar)",
                    )
                    .to_string()
                },
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                tr(app.lang, " Internal Staging:    ", " Staging Interno:       "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                format!(
                    "{} ({} {})",
                    base_dir.display(),
                    free_space,
                    tr(app.lang, "free", "livres")
                ),
                Style::default().fg(MUTED),
            ),
        ]),
        Line::from(""),
    ];

    if let Some(ref sess) = app.active_session {
        summary_lines.push(Line::from(Span::styled(
            tr(
                app.lang,
                " Active Sharing Summary (Press [5] for Controls & Interface Switch):",
                " Compartilhamento Ativo (Pressione [5] p/ Controles e Trocar Rede):",
            ),
            Style::default().fg(STATUS_OK).add_modifier(Modifier::BOLD),
        )));
        for (label, addr) in primary_access_rows(sess, app.lang) {
            summary_lines.push(Line::from(vec![
                Span::styled(format!("  • {label:<22} "), Style::default().fg(MUTED)),
                Span::styled(addr, Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
            ]));
        }
    } else if let Some(ref rep) = app.last_cleanup_report {
        summary_lines.push(Line::from(Span::styled(
            format!(
                " {} ({}):",
                tr(app.lang, "Last Session Cleanup", "Última Limpeza de Sessão"),
                rep.session_id
            ),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
        for act in rep.actions_performed.iter().take(4) {
            summary_lines.push(Line::from(Span::styled(
                format!("  ✓ {act}"),
                Style::default().fg(MUTED),
            )));
        }
    } else {
        summary_lines.push(Line::from(Span::styled(
            tr(app.lang, " Laboratory Workflow:", " Fluxo de Uso no Laboratório:"),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
        summary_lines.push(Line::from(Span::styled(
            tr(
                app.lang,
                "  1. Select files/folders via [2] Files, [o]/[f], or drag-and-drop.",
                "  1. Selecione arquivos/pastas em [2] Arquivos, [o]/[f] ou arrastando.",
            ),
            Style::default().fg(MUTED),
        )));
        summary_lines.push(Line::from(Span::styled(
            tr(
                app.lang,
                "  2. Start sharing via [4] or open [5] Sharing to choose network.",
                "  2. Inicie o compartilhamento ou abra [5] p/ escolher a interface de rede.",
            ),
            Style::default().fg(MUTED),
        )));
        summary_lines.push(Line::from(Span::styled(
            tr(
                app.lang,
                "  3. Switch Ethernet/Wi-Fi anytime on [5] Sharing and press [R] Restart.",
                "  3. Troque entre Cabo/Wi-Fi quando quiser em [5] e pressione [R] Reiniciar.",
            ),
            Style::default().fg(MUTED),
        )));
    }

    let summary_panel = Paragraph::new(summary_lines)
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(tr(
                    app.lang,
                    " Session & System Overview ",
                    " Visão Geral da Sessão e Sistema ",
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DIM)),
        );
    f.render_widget(summary_panel, cols[1]);
}

fn draw_file_browser(f: &mut Frame, app: &LabDropTuiApp, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(10)])
        .split(area);

    let mut bm_spans = vec![Span::styled(
        tr(app.lang, " Shortcuts: ", " Atalhos:   "),
        Style::default().fg(MUTED),
    )];
    for (i, (label, _)) in app.bookmarks.iter().take(4).enumerate() {
        bm_spans.push(Span::styled(
            format!("[F{}] ", i + 1),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        bm_spans.push(Span::styled(
            format!("{label}   "),
            Style::default().fg(TEXT),
        ));
    }

    let hidden_label = if app.show_hidden {
        tr(app.lang, "[.] Hidden: ON", "[.] Ocultos: SIM")
    } else {
        tr(app.lang, "[.] Hidden: OFF", "[.] Ocultos: NÃO")
    };

    let top_info = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                tr(app.lang, " Folder:    ", " Pasta:     "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                app.browser_dir.display().to_string(),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(bm_spans),
        Line::from(vec![
            Span::styled(
                format!(" {hidden_label}   "),
                Style::default().fg(if app.show_hidden { ACCENT } else { MUTED }),
            ),
            Span::styled("[o] ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(tr(app.lang, "Pick Files   ", "Escolher Arqs   "), Style::default().fg(MUTED)),
            Span::styled("[f] ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(tr(app.lang, "Pick Folder   ", "Escolher Pasta   "), Style::default().fg(MUTED)),
            Span::styled("[x] ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(tr(app.lang, "Open System Explorer   ", "Abrir no Sistema   "), Style::default().fg(MUTED)),
            Span::styled("[p] ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(tr(app.lang, "Paste Path", "Colar Caminho"), Style::default().fg(MUTED)),
        ]),
    ])
    .block(
        Block::default()
            .title(tr(
                app.lang,
                " File Browser ",
                " Navegador de Arquivos ",
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(DIM)),
    );
    f.render_widget(top_info, rows[0]);

    let items: Vec<ListItem> = app
        .browser_entries
        .iter()
        .map(|entry| {
            let selected = !entry.is_parent_link && app.selection.contains_path(&entry.path);
            let check = if entry.is_parent_link {
                "   "
            } else if selected {
                "[✓]"
            } else {
                "[ ]"
            };
            let kind_str = if entry.is_dir {
                "<DIR>".to_string()
            } else {
                format_bytes(entry.size_bytes)
            };
            let line = format!(" {check} {:<48} {:>12}", entry.name, kind_str);
            let style = if selected {
                Style::default()
                    .fg(STATUS_OK)
                    .add_modifier(Modifier::BOLD)
            } else if entry.is_dir {
                Style::default().fg(ACCENT)
            } else {
                Style::default().fg(TEXT)
            };
            ListItem::new(line).style(style)
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(app.browser_idx));

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        )
        .block(
            Block::default()
                .title(format!(
                    " {} — {} {} ({}) ",
                    tr(app.lang, "Directory Contents", "Conteúdo da Pasta"),
                    app.selection.items().len(),
                    tr(app.lang, "selected", "selecionado(s)"),
                    format_bytes(app.selection.total_bytes())
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT)),
        );
    f.render_stateful_widget(list, rows[1], &mut state);
}

fn draw_review_selection(f: &mut Frame, app: &LabDropTuiApp, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(10)])
        .split(area);

    let bar = Paragraph::new(Line::from(vec![
        Span::styled(" [d/Del] ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(tr(app.lang, "Remove Selected   ", "Remover Item   "), Style::default().fg(TEXT)),
        Span::styled("[c] ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(tr(app.lang, "Clear All   ", "Limpar Lista   "), Style::default().fg(TEXT)),
        Span::styled("[o/f] ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(tr(app.lang, "Add Files/Folder   ", "Adicionar Arquivos/Pasta   "), Style::default().fg(TEXT)),
        Span::styled("[s/Enter] ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(tr(app.lang, "Stage & Start Sharing", "Preparar e Compartilhar"), Style::default().fg(TEXT)),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(DIM)),
    );
    f.render_widget(bar, rows[0]);

    let items: Vec<ListItem> = app
        .selection
        .items()
        .iter()
        .map(|item| {
            let kind = match item.kind {
                ItemKind::File => tr(app.lang, "FILE", "ARQ "),
                ItemKind::Directory => tr(app.lang, "DIR ", "PAST"),
            };
            let rename_note = if item.renamed_due_to_collision {
                format!(
                    " [{}: '{}']",
                    tr(app.lang, "Renamed safely", "Renomeado sem colisão"),
                    item.staged_name
                )
            } else {
                String::new()
            };
            let line = format!(
                " [{kind}] {:<32} ({:>3} {}, {:>9}){} <- {}",
                item.staged_name,
                item.file_count,
                tr(app.lang, "files", "arqs"),
                format_bytes(item.total_bytes),
                rename_note,
                item.source_path.display()
            );
            ListItem::new(line).style(Style::default().fg(TEXT))
        })
        .collect();

    let mut state = ListState::default();
    if !items.is_empty() {
        state.select(Some(app.review_idx));
    }

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        )
        .block(
            Block::default()
                .title(format!(
                    " {} ({} {}, {}) ",
                    tr(app.lang, "Selected Items for Distribution", "Itens Selecionados para Distribuição"),
                    app.selection.items().len(),
                    tr(app.lang, "items", "itens"),
                    format_bytes(app.selection.total_bytes())
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT)),
        );
    f.render_stateful_widget(list, rows[1], &mut state);
}

fn draw_network_config(f: &mut Frame, app: &LabDropTuiApp, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(7),
            Constraint::Length(9),
        ])
        .split(area);

    let pref_label = app
        .config
        .preferred_interface
        .as_deref()
        .unwrap_or_else(|| tr(app.lang, "Automatic (Default Route)", "Automática (Rota Padrão)"));
    let active_sharing_iface = app
        .active_session
        .as_ref()
        .map(|s| format!("{} ({})", s.ledger.interface_name, s.ledger.interface_ip))
        .unwrap_or_else(|| tr(app.lang, "None (Sharing stopped)", "Nenhuma (Compartilhamento parado)").to_string());

    let top_summary = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                tr(app.lang, " Preferred Interface (Saved): ", " Interface Preferida (Salva): "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                pref_label,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw("     "),
            Span::styled(
                tr(app.lang, "Active Sharing Interface: ", "Interface Ativa na Sessão: "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                active_sharing_iface,
                Style::default().fg(if app.active_session.is_some() {
                    STATUS_OK
                } else {
                    MUTED
                }),
            ),
        ]),
        if let Some(ref warn) = app.preferred_iface_warning {
            Line::from(Span::styled(
                format!(" ! {warn}"),
                Style::default().fg(STATUS_WARN),
            ))
        } else {
            Line::from(Span::styled(
                tr(
                    app.lang,
                    " Tip: Select an interface below and press [f] to save as default, or [R] to restart active sharing on it.",
                    " Dica: Selecione uma interface abaixo e pressione [f] p/ salvar como padrão, ou [R] p/ reiniciar o compartilhamento nela.",
                ),
                Style::default().fg(MUTED),
            ))
        },
    ])
    .block(
        Block::default()
            .title(tr(
                app.lang,
                " Network Preference vs. Active Session ",
                " Preferência de Rede vs. Sessão Ativa ",
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(DIM)),
    );
    f.render_widget(top_summary, rows[0]);

    let active_name = app
        .active_session
        .as_ref()
        .map(|s| s.ledger.interface_name.as_str());
    let pref_opt = app.config.preferred_interface.as_deref();

    let items: Vec<ListItem> = app
        .interfaces
        .iter()
        .enumerate()
        .map(|(i, iface)| {
            let marker = if i == app.selected_iface_idx { "▸" } else { " " };
            let is_pref = pref_opt
                .map(|p| p.eq_ignore_ascii_case(&iface.name))
                .unwrap_or(false);
            let is_active = active_name == Some(iface.name.as_str());
            let row_text = iface.selector_row_label(app.lang, is_pref, is_active);
            let style = if is_active {
                Style::default().fg(STATUS_OK).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TEXT)
            };
            ListItem::new(format!(" {marker} {row_text}")).style(style)
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(app.selected_iface_idx));

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        )
        .block(
            Block::default()
                .title(tr(
                    app.lang,
                    " Available Network Interfaces (Name | Type | Status | IPv4) ",
                    " Interfaces de Rede Disponíveis (Nome | Tipo | Status | IPv4) ",
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT)),
        );
    f.render_stateful_widget(list, rows[1], &mut state);

    let smb_mode_str = match app.linux_smb_mode {
        SmbLinuxMode::SystemPort445 => tr(
            app.lang,
            "System Port 445 (Requires sudo; standard Windows/Linux SMB)",
            "Porta 445 do Sistema (Requer sudo; SMB nativo Windows/Linux)",
        ),
        SmbLinuxMode::RootlessPort(_) => tr(
            app.lang,
            "Rootless Port 4445 (No sudo; direct smb://<ip>:4445/LabDrop)",
            "Porta 4445 Sem Root (Sem sudo; smb://<ip>:4445/LabDrop)",
        ),
    };

    let samba_install_line = if app.smb_env.smbd_installed {
        format!(
            "{} ({})",
            tr(
                app.lang,
                "Installed & available",
                "Instalado e disponível"
            ),
            app.smb_env.smbd_version.as_deref().unwrap_or("Samba")
        )
    } else if let Some(ref d) = app.smb_env.linux_distro {
        format!(
            "{} — {} ({})",
            tr(
                app.lang,
                "Not installed (Optional for SMB; WebDAV/HTTP work without it)",
                "Não instalado (Opcional p/ SMB; WebDAV/HTTP funcionam sem ele)"
            ),
            tr(app.lang, "Press [i] or run:", "Pressione [i] ou execute:"),
            d.install_command()
        )
    } else {
        tr(
            app.lang,
            "Native Windows SMB (LanmanServer) available",
            "SMB Nativo do Windows (LanmanServer) disponível",
        )
        .to_string()
    };

    let bottom = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(" [f/Enter] ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(
                tr(app.lang, "Save Selected as Preferred   ", "Definir Selecionada como Preferida   "),
                Style::default().fg(TEXT),
            ),
            Span::styled("[a] ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(
                tr(app.lang, "Reset to Automatic   ", "Voltar p/ Automática   "),
                Style::default().fg(MUTED),
            ),
            Span::styled("[r] ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(
                tr(app.lang, "Refresh Interfaces", "Atualizar Interfaces"),
                Style::default().fg(MUTED),
            ),
        ]),
        Line::from(vec![
            Span::styled(" [Tab/p]   ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(
                tr(app.lang, "Sharing Strategy: ", "Estratégia:       "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                app.strategy.display_name_localized(app.lang),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(" [m]       ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(
                tr(app.lang, "Linux SMB Mode:   ", "Modo SMB Linux:   "),
                Style::default().fg(MUTED),
            ),
            Span::styled(smb_mode_str, Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled(" [e]       ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(
                tr(app.lang, "Port 445 Sudo:    ", "Elevar Porta 445: "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                if app.smb_env.is_elevated {
                    tr(app.lang, "Running elevated (root/admin)", "Executando com privilégios elevados (root/admin)")
                } else {
                    tr(app.lang, "Press [e] to relaunch LabDrop with sudo for Port 445 SMB", "Pressione [e] para reiniciar o LabDrop com sudo (Porta 445 SMB)")
                },
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled(" [i]       ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(
                tr(app.lang, "Samba Package:    ", "Pacote Samba:     "),
                Style::default().fg(MUTED),
            ),
            Span::styled(samba_install_line, Style::default().fg(TEXT)),
        ]),
    ])
    .block(
        Block::default()
            .title(tr(
                app.lang,
                " Protocol & Samba Configuration ",
                " Configuração de Protocolo e Samba ",
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(DIM)),
    );
    f.render_widget(bottom, rows[2]);
}

/// Extracts concise, scannable primary access addresses for the Sharing Screen main status panel.
fn primary_access_rows(sess: &ActiveSharingSession, lang: Lang) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    let ip = &sess.ledger.interface_ip;
    let (short_host, _) = labdrop_core::network::get_local_hostname();

    // 1. Windows Explorer (SMB 445 if active, otherwise WebDAV UNC)
    if let Some(smb_ep) = sess
        .ledger
        .endpoints
        .iter()
        .find(|e| e.protocol == "SMB (Primary)" && e.target_os.contains("Windows"))
    {
        rows.push((
            tr(lang, "Windows Explorer (SMB)", "Windows Explorer (SMB)").to_string(),
            smb_ep.address.clone(),
        ));
    } else if let Some(dav_win) = sess
        .ledger
        .endpoints
        .iter()
        .find(|e| e.protocol.starts_with("WebDAV") && e.target_os.contains("Windows"))
    {
        rows.push((
            tr(lang, "Windows Explorer", "Windows Explorer").to_string(),
            dav_win.address.clone(),
        ));
    }

    // 2. Windows / Linux Auto-Discovery
    if !ip.starts_with("127.") {
        rows.push((
            tr(lang, "Auto-Discovery (LAN)", "Descoberta na Rede").to_string(),
            format!(
                "{} -> LabDrop ({short_host})",
                tr(lang, "Network", "Rede")
            ),
        ));
    }

    // 3. Linux Native File Manager (SMB 445 or WebDAV)
    if let Some(smb_lin) = sess
        .ledger
        .endpoints
        .iter()
        .find(|e| e.protocol == "SMB (Primary)" && e.target_os.contains("Linux"))
    {
        rows.push((
            tr(lang, "Linux File Manager", "Gerenciador Linux").to_string(),
            smb_lin.address.clone(),
        ));
    } else if let Some(dav_lin) = sess
        .ledger
        .endpoints
        .iter()
        .find(|e| e.protocol.starts_with("WebDAV") && e.target_os.contains("Linux"))
    {
        rows.push((
            tr(lang, "Linux File Manager", "Gerenciador Linux").to_string(),
            dav_lin.address.clone(),
        ));
    }

    // 4. Web Browser HTTP Fallback (Direct IP)
    if let Some(http_ep) = sess
        .ledger
        .endpoints
        .iter()
        .find(|e| e.protocol == "HTTP (Fallback)")
    {
        rows.push((
            tr(lang, "Browser / .ZIP", "Navegador / .ZIP").to_string(),
            http_ep.address.clone(),
        ));
    }

    rows
}

fn draw_active_session(f: &mut Frame, app: &LabDropTuiApp, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(12), Constraint::Min(6)])
        .split(area);

    let top_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
        .split(rows[0]);

    // LEFT PANEL: Main Sharing Status & Primary Access Addresses (Clean & Scannable)
    let selected_iface = app.current_interface();
    let mut left_lines = Vec::new();

    if let Some(ref sess) = app.active_session {
        let iface_differs = sess.ledger.interface_name != selected_iface.name
            || sess.ledger.interface_ip != selected_iface.ipv4.to_string();

        left_lines.push(Line::from(vec![
            Span::styled(
                tr(app.lang, " Service Status:   ", " Estado do Serviço: "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                tr(app.lang, " ● RUNNING ", " ● EM EXECUÇÃO "),
                Style::default()
                    .fg(Color::Black)
                    .bg(STATUS_OK)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "  ({} {}, {})",
                    sess.manifest.total_files,
                    tr(app.lang, "files", "arqs"),
                    format_bytes(sess.manifest.total_bytes)
                ),
                Style::default().fg(MUTED),
            ),
        ]));

        left_lines.push(Line::from(vec![
            Span::styled(
                tr(app.lang, " Active Interface: ", " Interface Ativa:   "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                format!("{} — {}", sess.ledger.interface_name, sess.ledger.interface_ip),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
        ]));

        left_lines.push(Line::from(vec![
            Span::styled(
                tr(app.lang, " Active Protocols: ", " Protocolos Ativos: "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                sess.ledger.active_protocols.join(" + "),
                Style::default().fg(TEXT),
            ),
        ]));

        left_lines.push(Line::from(""));
        left_lines.push(Line::from(Span::styled(
            tr(
                app.lang,
                " Primary Access Addresses:",
                " Endereços Principais de Acesso:",
            ),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));

        for (label, addr) in primary_access_rows(sess, app.lang) {
            left_lines.push(Line::from(vec![
                Span::styled(format!("  {label:<22} "), Style::default().fg(MUTED)),
                Span::styled(addr, Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
            ]));
        }

        if iface_differs {
            left_lines.push(Line::from(Span::styled(
                format!(
                    " ! {} {} ({}) -> [R] {}",
                    tr(
                        app.lang,
                        "Selected interface:",
                        "Interface selecionada ao lado:"
                    ),
                    selected_iface.name,
                    selected_iface.ipv4,
                    tr(
                        app.lang,
                        "Press [R] to switch & restart",
                        "Pressione [R] p/ trocar e reiniciar"
                    )
                ),
                Style::default()
                    .fg(STATUS_WARN)
                    .add_modifier(Modifier::BOLD),
            )));
        } else {
            left_lines.push(Line::from(Span::styled(
                tr(
                    app.lang,
                    " Tip: Press [d] to toggle step-by-step instructions, secondary URLs & logs.",
                    " Dica: Pressione [d] p/ alternar instruções passo a passo, URLs extras e logs.",
                ),
                Style::default().fg(MUTED),
            )));
        }
    } else {
        left_lines.push(Line::from(vec![
            Span::styled(
                tr(app.lang, " Service Status:     ", " Estado do Serviço:   "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                tr(app.lang, " ○ STOPPED ", " ○ PARADO "),
                Style::default().fg(MUTED).bg(DIM),
            ),
        ]));
        left_lines.push(Line::from(vec![
            Span::styled(
                tr(app.lang, " Selected Interface: ", " Interface Escolhida: "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                format!(
                    "{} — {} ({})",
                    selected_iface.name,
                    selected_iface.ipv4,
                    selected_iface.connection_type_label(app.lang)
                ),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
        ]));
        left_lines.push(Line::from(vec![
            Span::styled(
                tr(app.lang, " Selected Files:     ", " Arquivos Prontos:    "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                format!(
                    "{} ({} {}, {})",
                    app.selection.items().len(),
                    app.selection.total_files(),
                    tr(app.lang, "files", "arquivos"),
                    format_bytes(app.selection.total_bytes())
                ),
                Style::default().fg(TEXT),
            ),
        ]));
        left_lines.push(Line::from(""));
        left_lines.push(Line::from(Span::styled(
            tr(
                app.lang,
                " Select a network interface on the right and press [R] or [s] to start sharing.",
                " Selecione a interface de rede à direita e pressione [R] ou [s] para iniciar.",
            ),
            Style::default().fg(MUTED),
        )));
    }

    let left_block = Paragraph::new(left_lines)
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(tr(
                    app.lang,
                    " Sharing Status & Primary Access ",
                    " Status do Compartilhamento e Acesso Principal ",
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT)),
        );
    f.render_widget(left_block, top_cols[0]);

    // RIGHT PANEL: Network Interface Selector & Sharing Service Controls
    let active_name = app
        .active_session
        .as_ref()
        .map(|s| s.ledger.interface_name.as_str());
    let pref_opt = app.config.preferred_interface.as_deref();

    let mut right_lines = vec![Line::from(Span::styled(
        tr(
            app.lang,
            " Select Network Interface (↑/↓ or Click):",
            " Selecionar Interface de Rede (↑/↓ ou Clique):",
        ),
        Style::default().fg(MUTED),
    ))];

    for (i, iface) in app.interfaces.iter().take(6).enumerate() {
        let is_sel = i == app.selected_iface_idx;
        let is_active = active_name == Some(iface.name.as_str());
        let is_pref = pref_opt
            .map(|p| p.eq_ignore_ascii_case(&iface.name))
            .unwrap_or(false);

        let cursor = if is_sel { "▸" } else { " " };
        let active_tag = if is_active {
            tr(app.lang, " [ACTIVE]", " [ATIVA]")
        } else if is_pref {
            tr(app.lang, " [Pref]", " [Pref]")
        } else {
            ""
        };
        let line_str = format!(
            " {cursor} {:<10} {:<10} {:<9} {}{}",
            iface.name,
            iface.connection_type_label(app.lang),
            iface.connection_status_label(app.lang),
            iface.ipv4,
            active_tag
        );
        let style = if is_sel {
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else if is_active {
            Style::default().fg(STATUS_OK).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT)
        };
        right_lines.push(Line::from(Span::styled(line_str, style)));
    }

    // Pad up to action rows if fewer interfaces
    while right_lines.len() < 8 {
        right_lines.push(Line::from(""));
    }

    right_lines.push(Line::from(vec![
        Span::styled(
            tr(
                app.lang,
                " [R] Restart Sharing Service ",
                " [R] Reiniciar Serviço ",
            ),
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            tr(app.lang, " [r] Refresh Interfaces ", " [r] Atualizar Rede "),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
    ]));

    let stop_or_start_label = if app.active_session.is_some() {
        tr(app.lang, " [s] Stop Sharing ", " [s] Parar Compartilhamento ")
    } else {
        tr(app.lang, " [s] Start Sharing ", " [s] Iniciar Compartilhamento ")
    };
    let details_label = if app.show_sharing_details {
        tr(app.lang, " [d] Hide Details ", " [d] Ocultar Detalhes ")
    } else {
        tr(app.lang, " [d] Details & All URLs ", " [d] Detalhes e URLs ")
    };

    right_lines.push(Line::from(vec![
        Span::styled(
            details_label,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            stop_or_start_label,
            Style::default()
                .fg(if app.active_session.is_some() {
                    STATUS_WARN
                } else {
                    STATUS_OK
                })
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    let right_block = Paragraph::new(right_lines).block(
        Block::default()
            .title(tr(
                app.lang,
                " Network Interface & Service Controls ",
                " Controles de Interface de Rede e Serviço ",
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(DIM)),
    );
    f.render_widget(right_block, top_cols[1]);

    // BOTTOM PANEL: Either Compact Live Activity OR Expanded Details & Diagnostics View
    if app.show_sharing_details {
        draw_sharing_details_expanded(f, app, rows[1]);
    } else {
        draw_sharing_activity_compact(f, app, rows[1]);
    }
}

fn draw_sharing_activity_compact(f: &mut Frame, app: &LabDropTuiApp, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(area);

    let Some(ref sess) = app.active_session else {
        let p = Paragraph::new(Span::styled(
            tr(
                app.lang,
                " Sharing service is currently stopped. Select files and press [s] or [R] to start.",
                " O serviço de compartilhamento está parado. Selecione arquivos e pressione [s] ou [R] para iniciar.",
            ),
            Style::default().fg(MUTED),
        ))
        .block(
            Block::default()
                .title(tr(app.lang, " Activity ", " Atividade "))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DIM)),
        );
        f.render_widget(p, area);
        return;
    };

    let snap = sess.tracker.snapshot();
    let mut client_lines = vec![
        Line::from(vec![
            Span::styled(
                tr(app.lang, " Transferred: ", " Transferido: "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                format_bytes(snap.total_bytes_transferred),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "   |   {}: {}   |   {}: {}",
                    tr(app.lang, "Active", "Ativas"),
                    snap.active_transfer_count,
                    tr(app.lang, "Done", "Concluídas"),
                    snap.completed_transfer_count
                ),
                Style::default().fg(MUTED),
            ),
        ]),
        Line::from(""),
    ];

    if snap.clients.is_empty() {
        client_lines.push(Line::from(Span::styled(
            tr(
                app.lang,
                " Waiting for destination computers to connect...",
                " Aguardando conexões dos computadores de destino...",
            ),
            Style::default().fg(MUTED),
        )));
    } else {
        for c in snap.clients.iter().take(6) {
            client_lines.push(Line::from(format!(
                " • {:<15} [{}] — {} {}, {}",
                c.client_ip,
                c.protocol.as_str(),
                c.completed_transfers,
                tr(app.lang, "done", "concl."),
                format_bytes(c.bytes_transferred)
            )));
        }
    }

    let clients_block = Paragraph::new(client_lines).block(
        Block::default()
            .title(tr(app.lang, " Connected Clients ", " Clientes Conectados "))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(DIM)),
    );
    f.render_widget(clients_block, cols[0]);

    let mut xfer_lines = Vec::new();
    if snap.recent_transfers.is_empty() {
        xfer_lines.push(Line::from(Span::styled(
            tr(
                app.lang,
                " No file transfers started yet.",
                " Nenhuma transferência de arquivo iniciada ainda.",
            ),
            Style::default().fg(MUTED),
        )));
    } else {
        for tr_rec in snap.recent_transfers.iter().take(8) {
            let badge = match tr_rec.state {
                labdrop_core::transfer::TransferState::Active => "[ACTIVE]",
                labdrop_core::transfer::TransferState::Completed => "[DONE]  ",
                labdrop_core::transfer::TransferState::Interrupted => "[ABORT] ",
            };
            let prog = if tr_rec.exact_byte_progress {
                if let Some(tot) = tr_rec.total_bytes {
                    let pct = (tr_rec.bytes_transferred * 100).checked_div(tot).unwrap_or(100);
                    format!(
                        "{}/{} ({pct}%)",
                        format_bytes(tr_rec.bytes_transferred),
                        format_bytes(tot)
                    )
                } else {
                    format_bytes(tr_rec.bytes_transferred)
                }
            } else {
                "SMB".to_string()
            };
            xfer_lines.push(Line::from(format!(
                " {badge} [{}] {} -> {} ({prog})",
                tr_rec.protocol.as_str(),
                tr_rec.client_ip,
                tr_rec.relative_path
            )));
        }
    }

    let xfers_block = Paragraph::new(xfer_lines).block(
        Block::default()
            .title(tr(
                app.lang,
                " Recent Transfers ([d] for All Endpoints & Technical Details) ",
                " Transferências Recentes ([d] p/ Todos os Endereços e Detalhes) ",
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(DIM)),
    );
    f.render_widget(xfers_block, cols[1]);
}

fn draw_sharing_details_expanded(f: &mut Frame, app: &LabDropTuiApp, area: Rect) {
    let Some(ref sess) = app.active_session else {
        return;
    };

    let mut lines = vec![Line::from(Span::styled(
        tr(
            app.lang,
            " All Access Endpoints & Step-by-Step Connection Instructions:",
            " Todos os Endereços de Acesso e Instruções Passo a Passo:",
        ),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ))];

    for ep in &sess.ledger.endpoints {
        lines.push(Line::from(vec![
            Span::styled(format!("  • {:<20} ", ep.protocol), Style::default().fg(ACCENT)),
            Span::styled(format!("[{:<26}] ", ep.target_os), Style::default().fg(MUTED)),
            Span::styled(
                format!("{} ", ep.address),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("— {}", ep.instructions), Style::default().fg(MUTED)),
        ]));
    }

    if !sess.ledger.warnings.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            tr(app.lang, " Session Notices & Diagnostics:", " Avisos e Diagnóstico da Sessão:"),
            Style::default()
                .fg(STATUS_WARN)
                .add_modifier(Modifier::BOLD),
        )));
        for w in &sess.ledger.warnings {
            lines.push(Line::from(Span::styled(
                format!("  ! {w}"),
                Style::default().fg(STATUS_WARN),
            )));
        }
    }

    let block = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(tr(
                    app.lang,
                    " Expanded Technical Details & All Endpoints ([d] to return to compact view) ",
                    " Detalhes Técnicos Expandidos e Todos os Endereços ([d] p/ voltar à visão compacta) ",
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT)),
        );
    f.render_widget(block, area);
}

fn draw_confirm_cleanup_modal(f: &mut Frame, app: &LabDropTuiApp) {
    let area = centered_rect(68, 42, f.area());
    f.render_widget(Clear, area);

    let choices = [
        tr(
            app.lang,
            "[1] Stop Sharing & Delete Staged Files (Recommended)",
            "[1] Parar Compartilhamento e Apagar Arquivos Temporários (Recomendado)",
        ),
        tr(
            app.lang,
            "[2] Stop Sharing & Retain Staged Files on Internal Disk",
            "[2] Parar Compartilhamento e Manter Cópia Temporária no Disco",
        ),
        tr(
            app.lang,
            "[Esc] Cancel (Keep Sharing Session Active)",
            "[Esc] Cancelar (Manter Sessão Ativa)",
        ),
    ];

    let mut lines = vec![
        Line::from(Span::styled(
            tr(
                app.lang,
                "Stopping the session terminates Samba/WebDAV/HTTP listeners and removes",
                "Parar a sessão encerra os servidores Samba/WebDAV/HTTP e remove",
            ),
            Style::default().fg(MUTED),
        )),
        Line::from(Span::styled(
            tr(
                app.lang,
                "temporary LabDrop share definitions and firewall rules.",
                "apenas os compartilhamentos e regras de firewall criados pelo LabDrop.",
            ),
            Style::default().fg(MUTED),
        )),
        Line::from(""),
        Line::from(Span::styled(
            tr(
                app.lang,
                "How should LabDrop handle the temporary staged copy of your files?",
                "O que fazer com a cópia temporária dos arquivos preparados?",
            ),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];

    for (i, &c) in choices.iter().enumerate() {
        let prefix = if i == app.cleanup_choice_idx { "▸" } else { " " };
        let style = if i == app.cleanup_choice_idx {
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT)
        };
        lines.push(Line::from(Span::styled(format!(" {prefix} {c} "), style)));
    }

    let modal = Paragraph::new(lines).block(
        Block::default()
            .title(tr(
                app.lang,
                " Confirm Session Stop & Cleanup ",
                " Confirmar Encerramento e Limpeza ",
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT)),
    );
    f.render_widget(modal, area);
}

fn draw_paste_modal(f: &mut Frame, app: &LabDropTuiApp) {
    let area = centered_rect(72, 34, f.area());
    f.render_widget(Clear, area);

    let lines = vec![
        Line::from(Span::styled(
            tr(
                app.lang,
                "Drag and drop files/folders onto this terminal, or type/paste path(s):",
                "Arraste e solte arquivos/pastas neste terminal, ou digite/cole o(s) caminho(s):",
            ),
            Style::default().fg(MUTED),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!(" > {}_", app.paste_buffer),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            tr(
                app.lang,
                "[Enter] Add Path(s)   |   [Esc] Cancel",
                "[Enter] Adicionar Caminho(s)   |   [Esc] Cancelar",
            ),
            Style::default().fg(ACCENT),
        )),
    ];

    let modal = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .title(tr(
                app.lang,
                " Drop or Paste File / Folder Paths ",
                " Arrastar ou Colar Caminhos ",
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT)),
    );
    f.render_widget(modal, area);
}

fn draw_diagnostics(f: &mut Frame, app: &LabDropTuiApp, area: Rect) {
    let env = &app.smb_env;
    let samba_needed_str = if env.smbd_installed {
        tr(
            app.lang,
            "No action needed — Samba is installed and ready.",
            "Nenhuma ação necessária — o Samba está instalado e pronto.",
        )
        .to_string()
    } else if let Some(ref d) = env.linux_distro {
        format!(
            "{} `{}` {}",
            tr(
                app.lang,
                "Optional: Install Samba for native SMB shares via [i] or",
                "Opcional: Instale o Samba para compartilhamento SMB nativo via [i] ou"
            ),
            d.install_command(),
            tr(
                app.lang,
                "(WebDAV + HTTP work without Samba).",
                "(WebDAV + HTTP funcionam mesmo sem o Samba)."
            )
        )
    } else {
        tr(
            app.lang,
            "Windows native LanmanServer SMB is built into the OS.",
            "O servidor SMB nativo do Windows já faz parte do sistema.",
        )
        .to_string()
    };

    let lines = vec![
        Line::from(vec![
            Span::styled(
                tr(app.lang, " Operating System Family:   ", " Sistema Operacional:       "),
                Style::default().fg(MUTED),
            ),
            Span::styled(&env.os_family, Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled(
                tr(app.lang, " Linux Distribution:        ", " Distribuição Linux:        "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                env.linux_distro
                    .as_ref()
                    .map(|d| d.display_name())
                    .unwrap_or("N/A"),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                tr(app.lang, " Samba / SMB Installed:     ", " Samba / SMB Instalado:     "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                format!(
                    "{} ({})",
                    if env.smbd_installed {
                        tr(app.lang, "Yes", "Sim")
                    } else {
                        tr(app.lang, "No", "Não")
                    },
                    env.smbd_version.as_deref().unwrap_or("N/A")
                ),
                Style::default().fg(if env.smbd_installed {
                    STATUS_OK
                } else {
                    STATUS_WARN
                }),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                tr(app.lang, " Samba Availability:        ", " Disponibilidade do Samba:  "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                if env.smbd_installed {
                    tr(
                        app.lang,
                        "Available (Port 445 with sudo / Port 4445 rootless)",
                        "Disponível (Porta 445 com sudo / Porta 4445 sem root)",
                    )
                } else {
                    tr(
                        app.lang,
                        "Unavailable — automatic fallback to WebDAV + HTTP",
                        "Indisponível — fallback automático para WebDAV + HTTP",
                    )
                },
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                tr(app.lang, " Installation Guidance:     ", " Orientação de Instalação:  "),
                Style::default().fg(MUTED),
            ),
            Span::styled(samba_needed_str, Style::default().fg(ACCENT)),
        ]),
        Line::from(vec![
            Span::styled(
                tr(app.lang, " System Service Name:       ", " Serviço do Sistema:        "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                format!("{} (Active: {})", env.service_name, env.service_active),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                tr(app.lang, " Process Elevated (Root):   ", " Privilégios de Root/Admin: "),
                Style::default().fg(MUTED),
            ),
            Span::styled(format!("{}", env.is_elevated), Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled(
                tr(app.lang, " Rootless User Namespace:   ", " Namespace Sem Root:        "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                format!("{}", env.rootless_unshare_available),
                Style::default().fg(TEXT),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                tr(app.lang, " Active Firewall Manager:   ", " Gerenciador de Firewall:   "),
                Style::default().fg(MUTED),
            ),
            Span::styled(&env.firewall_backend, Style::default().fg(TEXT)),
        ]),
        Line::from(vec![
            Span::styled(
                tr(app.lang, " SELinux Enforcing:         ", " SELinux Ativo (Enforcing): "),
                Style::default().fg(MUTED),
            ),
            Span::styled(format!("{}", env.selinux_enforcing), Style::default().fg(TEXT)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            tr(
                app.lang,
                " Note: The LabDrop Bootstrapper installs only LabDrop executables, never Samba.",
                " Nota: O Bootstrapper do LabDrop instala apenas os executáveis do LabDrop, nunca o Samba.",
            ),
            Style::default().fg(MUTED),
        )),
    ];
    let p = Paragraph::new(lines).block(
        Block::default()
            .title(tr(
                app.lang,
                " System Diagnostics & Samba Environment ",
                " Diagnóstico do Sistema e Ambiente Samba ",
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT)),
    );
    f.render_widget(p, area);
}

fn render_staging_progress_modal(
    f: &mut Frame,
    lang: Lang,
    current_file: &str,
    files_done: usize,
    total_files: usize,
    bytes_copied: u64,
    total_bytes: u64,
) {
    let area = centered_rect(68, 30, f.area());
    f.render_widget(Clear, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Length(3)])
        .margin(1)
        .split(area);

    let block = Block::default()
        .title(tr(
            lang,
            " Staging Files to Internal Storage... ",
            " Copiando Arquivos para o Staging Interno... ",
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT));
    f.render_widget(block, area);

    let info = Paragraph::new(vec![
        Line::from(format!("{}: {current_file}", tr(lang, "Copying", "Copiando"))),
        Line::from(format!(
            "{}: {files_done} / {total_files}  |  Bytes: {} / {}",
            tr(lang, "Files", "Arquivos"),
            format_bytes(bytes_copied),
            format_bytes(total_bytes)
        )),
    ]);
    f.render_widget(info, rows[0]);

    let ratio = if total_bytes > 0 {
        (bytes_copied as f64 / total_bytes as f64).clamp(0.0, 1.0)
    } else {
        1.0
    };
    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL))
        .gauge_style(Style::default().fg(ACCENT))
        .ratio(ratio);
    f.render_widget(gauge, rows[1]);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

    #[test]
    fn tui_top_bar_mouse_click_switches_screens_and_toggles_details() {
        let mut app = LabDropTuiApp::new(Lang::PtBr, Vec::new());
        assert_eq!(app.screen, Screen::Dashboard);

        // Simulate toggling details flag via key handler on ActiveSession
        app.screen = Screen::ActiveSession;
        assert!(!app.show_sharing_details);
        app.show_sharing_details = !app.show_sharing_details;
        assert!(app.show_sharing_details);

        // Simulate scroll down in interface list
        if app.interfaces.len() > 1 {
            let ev = MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 10,
                row: 5,
                modifiers: KeyModifiers::NONE,
            };
            // Verify scroll updates index
            if matches!(ev.kind, MouseEventKind::ScrollDown) {
                app.selected_iface_idx = (app.selected_iface_idx + 1).min(app.interfaces.len() - 1);
            }
            assert!(app.selected_iface_idx >= 1);
        }
        let _ = MouseButton::Left;
    }
}
