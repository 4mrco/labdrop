use anyhow::Result;
use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use labdrop_core::{
    fs_space::{format_bytes, query_disk_space},
    i18n::Lang,
    network::{discover_network_interfaces, NetworkInterface},
    session::{
        is_terminal_disconnected, recover_orphaned_sessions, ActiveSharingSession, CleanupReport,
        SharingStrategy,
    },
    smb::{
        install_linux_samba_interactive, probe_smb_environment, SmbEnvironmentStatus, SmbLinuxMode,
    },
    staging::{default_staging_base_dir, parse_dropped_paths, stage_selection, ItemKind, SelectionSet},
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

static TUI_TERMINATION_REQUESTED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn handle_tui_signal(_sig: libc::c_int) {
    TUI_TERMINATION_REQUESTED.store(true, Ordering::Relaxed);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
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
    lang: Lang,
    screen: Screen,
    return_screen: Screen,
    should_quit: bool,
    menu_idx: usize,
    selection: SelectionSet,
    interfaces: Vec<NetworkInterface>,
    selected_iface_idx: usize,
    strategy: SharingStrategy,
    linux_smb_mode: SmbLinuxMode,
    preferred_http_port: u16,
    smb_env: SmbEnvironmentStatus,
    active_session: Option<ActiveSharingSession>,
    last_cleanup_report: Option<CleanupReport>,
    status_message: String,
    error_message: Option<String>,

    // File browser state
    browser_dir: PathBuf,
    browser_entries: Vec<BrowserEntry>,
    browser_idx: usize,
    show_hidden: bool,
    review_idx: usize,
    cleanup_choice_idx: usize,
    bookmarks: Vec<(String, PathBuf)>,

    // Paste / Drop path modal buffer
    paste_buffer: String,
}

impl LabDropTuiApp {
    pub fn new(lang: Lang, initial_paths: Vec<PathBuf>) -> Self {
        let interfaces = discover_network_interfaces();
        let smb_env = probe_smb_environment();
        let linux_smb_mode = if smb_env.is_elevated {
            SmbLinuxMode::SystemPort445
        } else {
            SmbLinuxMode::RootlessPort(4445)
        };

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
                "Ready. Drag & drop files here anytime, or use Up/Down + Enter (or 1-6).",
                "Pronto. Arraste arquivos para o terminal, ou use Setas + Enter (ou 1-6).",
            )
            .to_string()
        };

        let mut app = Self {
            lang,
            screen: Screen::Dashboard,
            return_screen: Screen::Dashboard,
            should_quit: false,
            menu_idx: 0,
            selection,
            interfaces,
            selected_iface_idx: 0,
            strategy: SharingStrategy::AutoLayered,
            linux_smb_mode,
            preferred_http_port: 8080,
            smb_env,
            active_session: None,
            last_cleanup_report: recovered.into_iter().last(),
            status_message,
            error_message: None,
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

    fn current_interface(&self) -> NetworkInterface {
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
                        "Opened '{}' in system file manager — drag files from it onto this terminal!",
                        dir.display()
                    ),
                    Lang::PtBr => format!(
                        "Pasta '{}' aberta no gerenciador — arraste os arquivos para este terminal!",
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
        if self.selection.is_empty() {
            self.error_message = Some(
                tr(
                    self.lang,
                    "Please select at least one file or folder before starting a session.",
                    "Selecione pelo menos um arquivo ou pasta antes de iniciar a sessão.",
                )
                .to_string(),
            );
            return;
        }

        // Stop any existing session first
        if let Some(mut prev) = self.active_session.take() {
            let _ = prev.stop_and_cleanup(true);
        }

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
            manifest,
            session_dir,
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

        // 1. Try Zenity (GNOME / Fedora / Ubuntu / Mint)
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
                return Ok(Vec::new()); // User pressed Cancel
            }
        }

        // 2. Fallback: KDialog (KDE)
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
        ).to_string())
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
                list.push(("USB files/".to_string(), usb_files));
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let cwd_files = cwd.join("files");
        if cwd_files.is_dir() && !list.iter().any(|(_, p)| p == &cwd_files) {
            list.push(("Local files/".to_string(), cwd_files));
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
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = LabDropTuiApp::new(lang, initial_paths);
    let res = run_event_loop(&mut terminal, &mut app);

    // Ensure any active session + smbd/smbd-notifyd/smbd-cleanupd are torn down cleanly on exit or SIGHUP
    if let Some(mut sess) = app.active_session.take() {
        let _ = sess.stop_and_cleanup(true);
    }

    if is_terminal_disconnected() {
        // Terminal/PTY is already closed; skip ratatui::Terminal::drop eprintln! on closed stderr
        std::mem::forget(terminal);
    } else {
        let _ = disable_raw_mode();
        let _ = execute!(
            terminal.backend_mut(),
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
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                // Ctrl+C anywhere initiates clean shutdown
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

fn handle_key_press(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut LabDropTuiApp,
    code: KeyCode,
) -> Result<()> {
    // Global language switch key 'L' (except inside PastePathModal text input)
    if app.screen != Screen::PastePathModal && matches!(code, KeyCode::Char('l') | KeyCode::Char('L')) {
        app.lang = app.lang.toggle();
        app.bookmarks = build_quick_bookmarks(app.lang);
        app.refresh_browser_entries();
        app.status_message = match app.lang {
            Lang::En => "Language switched to English.".to_string(),
            Lang::PtBr => "Idioma alterado para Português (BR).".to_string(),
        };
        return Ok(());
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
            KeyCode::Char('1') => {
                app.menu_idx = 0;
                app.refresh_browser_entries();
                app.screen = Screen::FileBrowser;
            }
            KeyCode::Char('2') => {
                app.menu_idx = 1;
                app.screen = Screen::ReviewSelection;
            }
            KeyCode::Char('3') => {
                app.menu_idx = 2;
                app.screen = Screen::NetworkConfig;
            }
            KeyCode::Char('4') => {
                app.menu_idx = 3;
                if app.active_session.is_some() {
                    app.screen = Screen::ActiveSession;
                } else {
                    app.stage_and_start_session(terminal);
                }
            }
            KeyCode::Char('5') => {
                app.menu_idx = 4;
                if app.active_session.is_some() {
                    app.cleanup_choice_idx = 0;
                    app.screen = Screen::ConfirmCleanup;
                } else {
                    app.status_message = tr(
                        app.lang,
                        "No active sharing session to stop.",
                        "Nenhuma sessão ativa para encerrar.",
                    )
                    .to_string();
                }
            }
            KeyCode::Char('6') => {
                app.menu_idx = 5;
                app.smb_env = probe_smb_environment();
                app.screen = Screen::Diagnostics;
            }
            KeyCode::Char('q') | KeyCode::Char('Q') => {
                if app.active_session.is_some() {
                    app.cleanup_choice_idx = 0;
                    app.screen = Screen::ConfirmCleanup;
                } else {
                    app.should_quit = true;
                }
            }
            KeyCode::Enter => match app.menu_idx {
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
                        app.status_message = tr(
                            app.lang,
                            "No active sharing session to stop.",
                            "Nenhuma sessão ativa para encerrar.",
                        )
                        .to_string();
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
            },
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
            KeyCode::Esc | KeyCode::Char('b') | KeyCode::Enter => app.screen = Screen::Dashboard,
            KeyCode::Up | KeyCode::Char('k') => {
                app.selected_iface_idx = app.selected_iface_idx.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if app.selected_iface_idx + 1 < app.interfaces.len() {
                    app.selected_iface_idx += 1;
                }
            }
            KeyCode::Tab | KeyCode::Char('p') => {
                app.strategy = match app.strategy {
                    SharingStrategy::AutoLayered => SharingStrategy::SmbOnly,
                    SharingStrategy::SmbOnly => SharingStrategy::WebDavSecondary,
                    SharingStrategy::WebDavSecondary => SharingStrategy::HttpFallback,
                    SharingStrategy::HttpFallback => SharingStrategy::AutoLayered,
                };
            }
            KeyCode::Char('m') => {
                app.linux_smb_mode = match app.linux_smb_mode {
                    SmbLinuxMode::RootlessPort(_) => SmbLinuxMode::SystemPort445,
                    SmbLinuxMode::SystemPort445 => SmbLinuxMode::RootlessPort(4445),
                };
            }
            KeyCode::Char('r') => {
                app.interfaces = discover_network_interfaces();
                app.selected_iface_idx = 0;
                app.smb_env = probe_smb_environment();
                app.status_message = tr(
                    app.lang,
                    "Refreshed network interfaces and SMB status.",
                    "Interfaces de rede e status SMB atualizados.",
                )
                .to_string();
            }
            KeyCode::Char('e') | KeyCode::Char('E')
                if !app.smb_env.is_elevated && app.smb_env.os_family == "Linux" =>
            {
                // Re-launch current LabDrop binary with sudo for Port 445 SMB + firewalld
                if let Ok(exe) = std::env::current_exe() {
                    let _ = disable_raw_mode();
                    let _ = execute!(
                        terminal.backend_mut(),
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
            KeyCode::Char('i')
                if !app.smb_env.smbd_installed && app.smb_env.os_family == "Linux" =>
            {
                let _ = disable_raw_mode();
                let _ = execute!(
                    terminal.backend_mut(),
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
                    EnableBracketedPaste
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
            _ => {}
        },

        Screen::ActiveSession => match code {
            KeyCode::Esc | KeyCode::Char('b') => app.screen = Screen::Dashboard,
            KeyCode::Char('s') | KeyCode::Enter | KeyCode::Char('q') => {
                app.cleanup_choice_idx = 0;
                app.screen = Screen::ConfirmCleanup;
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
                            format!("Recovered and cleaned up {} orphaned session(s).", reports.len())
                        }
                        Lang::PtBr => {
                            format!("Recuperadas e limpas {} sessão(ões) órfã(s).", reports.len())
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

fn jump_bookmark(app: &mut LabDropTuiApp, idx: usize) {
    if let Some((label, path)) = app.bookmarks.get(idx).cloned() {
        app.browser_dir = path;
        app.browser_idx = 0;
        app.refresh_browser_entries();
        app.status_message = format!(
            "{} {label}.",
            tr(app.lang, "Jumped to", "Navegou para")
        );
    }
}

fn execute_cleanup_choice(app: &mut LabDropTuiApp, delete_staged: bool) {
    if let Some(mut sess) = app.active_session.take() {
        let report = sess.stop_and_cleanup(delete_staged);
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

fn draw_ui(f: &mut Frame, app: &LabDropTuiApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(14),
            Constraint::Length(3),
        ])
        .split(f.area());

    let session_badge = if let Some(ref s) = app.active_session {
        Span::styled(
            format!(
                " ● {} ({}) ",
                tr(app.lang, "SHARING ACTIVE", "COMPARTILHANDO"),
                s.ledger.session_id
            ),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            format!(" ○ {} ", tr(app.lang, "IDLE", "INATIVO")),
            Style::default().fg(Color::Gray).bg(Color::DarkGray),
        )
    };

    let lang_badge = Span::styled(
        format!(" [L] {} ", app.lang.code()),
        Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    );

    let header_line = Line::from(vec![
        Span::styled(
            " LabDrop v0.1.0 ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(tr(
            app.lang,
            "— Laboratory File Distribution  ",
            "— Distribuição de Arquivos em Laboratório  ",
        )),
        session_badge,
        Span::raw(" "),
        lang_badge,
    ]);
    let header_block = Paragraph::new(header_line).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
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

    let footer_text = if let Some(ref err) = app.error_message {
        Line::from(vec![
            Span::styled(
                tr(app.lang, " ERROR: ", " ERRO: "),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(err.as_str(), Style::default().fg(Color::LightRed)),
        ])
    } else {
        Line::from(vec![
            Span::styled(" Status: ", Style::default().fg(Color::Cyan)),
            Span::raw(app.status_message.as_str()),
        ])
    };
    let footer = Paragraph::new(footer_text).block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[2]);
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
            "[2] Review / Remove Selected Items",
            "[2] Revisar / Remover Itens Selecionados",
        ),
        tr(
            app.lang,
            "[3] Configure Network Interface & Protocol",
            "[3] Configurar Interface de Rede e Protocolo",
        ),
        if app.active_session.is_some() {
            tr(
                app.lang,
                "[4] View Active Sharing Session Monitor",
                "[4] Ver Monitor da Sessão Ativa",
            )
        } else {
            tr(
                app.lang,
                "[4] Stage Files & Start Sharing Session",
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
            "[6] System Diagnostics & Orphan Recovery",
            "[6] Diagnóstico do Sistema e Recuperação",
        ),
        tr(app.lang, "[q] Quit LabDrop", "[q] Sair do LabDrop"),
    ];

    let list_items: Vec<ListItem> = menu_items
        .iter()
        .enumerate()
        .map(|(i, &label)| {
            let style = if i == app.menu_idx {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            ListItem::new(format!("  {label}  ")).style(style)
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
                " Main Menu (Up/Down + Enter) ",
                " Menu Principal (Setas + Enter) ",
            ))
            .borders(Borders::ALL),
    );
    f.render_widget(menu_list, menu_panel[0]);

    let quick_pick = Paragraph::new(vec![
        Line::from(tr(
            app.lang,
            " [o] Native File Picker  |  [f] Native Folder Picker",
            " [o] Seletor Nativo (Arquivos) | [f] Seletor Nativo (Pasta)",
        )),
        Line::from(tr(
            app.lang,
            " [p] Paste/Drop Path(s)  |  Or drag files straight onto terminal!",
            " [p] Colar Caminho(s)   |  Ou arraste arquivos direto no terminal!",
        )),
    ])
    .block(
        Block::default()
            .title(tr(
                app.lang,
                " Quick File Selection ",
                " Seleção Rápida / Arrastar ",
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Green)),
    );
    f.render_widget(quick_pick, menu_panel[1]);

    // Right column: current configuration & session overview
    let iface = app.current_interface();
    let base_dir = default_staging_base_dir();
    let free_space = query_disk_space(&base_dir)
        .map(|d| format_bytes(d.available_bytes))
        .unwrap_or_else(|_| "?".to_string());

    let mut summary_lines = vec![
        Line::from(vec![
            Span::styled(
                tr(app.lang, "Selected Items: ", "Itens Selecionados: "),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(format!(
                "{} ({} {}, {})",
                app.selection.items().len(),
                app.selection.total_files(),
                tr(app.lang, "files", "arquivos"),
                format_bytes(app.selection.total_bytes())
            )),
        ]),
        Line::from(vec![
            Span::styled(
                tr(app.lang, "Internal Staging: ", "Staging Interno: "),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(format!(
                "{} ({} {})",
                base_dir.display(),
                free_space,
                tr(app.lang, "free", "livres")
            )),
        ]),
        Line::from(vec![
            Span::styled(
                tr(app.lang, "Network Interface: ", "Interface de Rede: "),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(iface.display_label()),
        ]),
        Line::from(vec![
            Span::styled(
                tr(app.lang, "Sharing Strategy: ", "Estratégia: "),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(app.strategy.display_name()),
        ]),
        Line::from(vec![
            Span::styled(
                tr(app.lang, "SMB Backend: ", "Servidor SMB: "),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(if app.smb_env.smbd_installed {
                format!(
                    "{} ({})",
                    app.smb_env.smbd_version.as_deref().unwrap_or("Samba"),
                    match app.linux_smb_mode {
                        SmbLinuxMode::SystemPort445 => tr(
                            app.lang,
                            "Port 445 System",
                            "Porta 445 Sistema (sudo)"
                        ),
                        SmbLinuxMode::RootlessPort(_) => tr(
                            app.lang,
                            "Port 4445 Rootless",
                            "Porta 4445 Sem Root"
                        ),
                    }
                )
            } else {
                tr(
                    app.lang,
                    "Not installed (Press [3] then 'i' to install)",
                    "Não instalado (Pressione [3] e 'i' para instalar)",
                )
                .to_string()
            }),
        ]),
        Line::from(""),
    ];

    if let Some(ref sess) = app.active_session {
        summary_lines.push(Line::from(Span::styled(
            tr(
                app.lang,
                "Active Access Endpoints:",
                "Endereços de Acesso Ativos:",
            ),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )));
        for ep in &sess.ledger.endpoints {
            summary_lines.push(Line::from(format!(
                " • {} ({}): {}",
                ep.protocol, ep.target_os, ep.address
            )));
        }
    } else if let Some(ref rep) = app.last_cleanup_report {
        summary_lines.push(Line::from(Span::styled(
            format!(
                "{} ({}):",
                tr(app.lang, "Last Session Cleanup", "Última Limpeza de Sessão"),
                rep.session_id
            ),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        for act in &rep.actions_performed {
            summary_lines.push(Line::from(format!(" ✓ {act}")));
        }
    } else {
        summary_lines.push(Line::from(Span::styled(
            tr(app.lang, "Quick Workflow:", "Fluxo Rápido:"),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
        summary_lines.push(Line::from(tr(
            app.lang,
            " 1. Press [1], [o]/[f] (Native Explorer), or drag files onto terminal.",
            " 1. Use [1], [o]/[f] (Gerenciador Nativo) ou arraste arquivos aqui.",
        )));
        summary_lines.push(Line::from(tr(
            app.lang,
            " 2. Press [4] to copy to internal staging & start sharing.",
            " 2. Pressione [4] para copiar ao staging interno e compartilhar.",
        )));
        summary_lines.push(Line::from(tr(
            app.lang,
            " 3. Copy files on destination PCs / phones.",
            " 3. Copie os arquivos nos PCs / celulares de destino.",
        )));
        summary_lines.push(Line::from(tr(
            app.lang,
            " 4. Press [5] (or close terminal) to stop & clean up.",
            " 4. Pressione [5] (ou feche o terminal) para parar e limpar tudo.",
        )));
    }

    let summary_panel = Paragraph::new(summary_lines)
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(tr(
                    app.lang,
                    " Session & System Overview ",
                    " Resumo da Sessão e Sistema ",
                ))
                .borders(Borders::ALL),
        );
    f.render_widget(summary_panel, cols[1]);
}

fn draw_file_browser(f: &mut Frame, app: &LabDropTuiApp, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(10)])
        .split(area);

    let mut bm_spans = vec![Span::styled(
        tr(app.lang, " Quick Jump: ", " Atalhos: "),
        Style::default().fg(Color::Yellow),
    )];
    for (i, (label, _)) in app.bookmarks.iter().take(4).enumerate() {
        bm_spans.push(Span::styled(
            format!("[F{}] {}  ", i + 1, label),
            Style::default().fg(Color::Cyan),
        ));
    }

    let hidden_label = if app.show_hidden {
        tr(app.lang, "[.] Hidden (.*): ON", "[.] Ocultos (.*): SIM")
    } else {
        tr(app.lang, "[.] Hidden (.*): OFF", "[.] Ocultos (.*): NÃO")
    };

    let top_info = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                tr(app.lang, " Current Folder: ", " Pasta Atual: "),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(app.browser_dir.display().to_string()),
        ]),
        Line::from(bm_spans),
        Line::from(vec![
            Span::styled(
                format!(" {hidden_label}  "),
                Style::default()
                    .fg(if app.show_hidden {
                        Color::Green
                    } else {
                        Color::Gray
                    })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                tr(
                    app.lang,
                    "|  [o] Pick Files  |  [f] Pick Folder  |  [x] Open Explorer  |  [p] Paste Path",
                    "|  [o] Escolher Arquivos  |  [f] Escolher Pasta  |  [x] Abrir Pasta  |  [p] Colar Caminho",
                ),
                Style::default().fg(Color::Cyan),
            ),
        ]),
    ])
    .block(
        Block::default()
            .title(tr(
                app.lang,
                " File Browser (Space: Select | Enter: Open | Backspace: Up | s: Start | Esc: Back) ",
                " Navegador (Espaço: Marcar | Enter: Abrir | Backspace: Subir | s: Iniciar | Esc: Voltar) ",
            ))
            .borders(Borders::ALL),
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
            let line = format!(" {check} {:<44} {:>12}", entry.name, kind_str);
            let style = if selected {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else if entry.is_dir {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::White)
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
                .bg(Color::Cyan)
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
                .borders(Borders::ALL),
        );
    f.render_stateful_widget(list, rows[1], &mut state);
}

fn draw_review_selection(f: &mut Frame, app: &LabDropTuiApp, area: Rect) {
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
                " [{kind}] {} ({} {}, {}){} <- {}",
                item.staged_name,
                item.file_count,
                tr(app.lang, "files", "arqs"),
                format_bytes(item.total_bytes),
                rename_note,
                item.source_path.display()
            );
            ListItem::new(line)
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
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .block(
            Block::default()
                .title(tr(
                    app.lang,
                    " Selected Items (d/Del: Remove | c: Clear | o/f: Native Picker | p: Paste | s/Enter: Start | Esc: Back) ",
                    " Itens Selecionados (d/Del: Remover | c: Limpar | o/f: Seletor Nativo | p: Colar | s/Enter: Iniciar | Esc: Voltar) ",
                ))
                .borders(Borders::ALL),
        );
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_network_config(f: &mut Frame, app: &LabDropTuiApp, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(9)])
        .split(area);

    let items: Vec<ListItem> = app
        .interfaces
        .iter()
        .enumerate()
        .map(|(i, iface)| {
            let marker = if i == app.selected_iface_idx {
                "(●)"
            } else {
                "( )"
            };
            ListItem::new(format!(" {marker} {}", iface.display_label()))
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(app.selected_iface_idx));

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .block(
            Block::default()
                .title(tr(
                    app.lang,
                    " Select Laboratory Network Interface (Up/Down | r: Refresh | Esc/Enter: Done) ",
                    " Selecionar Interface de Rede (Setas | r: Atualizar | Esc/Enter: Voltar) ",
                ))
                .borders(Borders::ALL),
        );
    f.render_stateful_widget(list, rows[0], &mut state);

    let smb_mode_str = match app.linux_smb_mode {
        SmbLinuxMode::SystemPort445 => tr(
            app.lang,
            "System Port 445 (Requires root/sudo; native Windows, Linux & Mobile SMB)",
            "Porta 445 do Sistema (Requer sudo; compatível com Windows, Linux e Celular)",
        ),
        SmbLinuxMode::RootlessPort(_) => tr(
            app.lang,
            "Rootless Port 4445 (Zero root; direct share URL smb://<ip>:4445/LabDrop)",
            "Porta 4445 Sem Root (Acesso direto smb://<ip>:4445/LabDrop)",
        ),
    };

    let elevate_line = if !app.smb_env.is_elevated && app.smb_env.os_family == "Linux" {
        tr(
            app.lang,
            "Press [e] to restart with sudo for Port 445 SMB + firewalld",
            "Pressione [e] para reiniciar com sudo e ativar SMB na porta 445 + firewall",
        )
    } else if app.smb_env.is_elevated {
        tr(
            app.lang,
            "Already running elevated (Port 445 SMB ready)",
            "Executando com privilégios elevados (Porta 445 SMB pronta)",
        )
    } else {
        "N/A"
    };

    let bottom = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                tr(app.lang, " [Tab / p] Sharing Strategy: ", " [Tab / p] Estratégia: "),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(
                app.strategy.display_name(),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                tr(app.lang, " [m] Linux SMB Mode:         ", " [m] Modo SMB Linux:   "),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(smb_mode_str),
        ]),
        Line::from(vec![
            Span::styled(
                tr(app.lang, " [e] Port 445 Elevation:     ", " [e] Elevar Porta 445: "),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(elevate_line),
        ]),
        Line::from(vec![
            Span::styled(
                tr(app.lang, " [i] Install Samba Package:  ", " [i] Instalar Samba:   "),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw(if app.smb_env.smbd_installed {
                tr(app.lang, "Already installed", "Já instalado")
            } else {
                tr(
                    app.lang,
                    "Press 'i' to install Samba via package manager",
                    "Pressione 'i' para instalar o Samba via gerenciador de pacotes",
                )
            }),
        ]),
    ])
    .block(
        Block::default()
            .title(tr(
                app.lang,
                " Sharing Protocol Settings ",
                " Configurações de Protocolo ",
            ))
            .borders(Borders::ALL),
    );
    f.render_widget(bottom, rows[1]);
}

fn draw_active_session(f: &mut Frame, app: &LabDropTuiApp, area: Rect) {
    let Some(ref sess) = app.active_session else {
        let p = Paragraph::new(tr(app.lang, "No active session.", "Nenhuma sessão ativa."))
            .block(Block::default().borders(Borders::ALL));
        f.render_widget(p, area);
        return;
    };

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(62), Constraint::Min(7)])
        .split(area);

    let mut top_lines = vec![
        Line::from(vec![
            Span::styled(
                tr(app.lang, " SECURITY NOTICE: ", " AVISO DE SEGURANÇA: "),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                " {} {} ({}). {} ({}).",
                tr(
                    app.lang,
                    "Unauthenticated read-only share exposed on",
                    "Compartilhamento somente-leitura sem senha ativo em"
                ),
                sess.ledger.interface_name,
                sess.ledger.interface_ip,
                tr(
                    app.lang,
                    "Anyone on this network may read these files",
                    "Qualquer dispositivo nesta rede pode ler os arquivos"
                ),
                sess.manifest.total_files
            )),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            tr(
                app.lang,
                " How to Access from Destination Computers / Phones:",
                " Como Acessar nos Computadores / Celulares de Destino:",
            ),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )),
    ];

    for ep in &sess.ledger.endpoints {
        top_lines.push(Line::from(vec![
            Span::styled(
                format!("  • {:<20} ", ep.protocol),
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("[{:<28}] ", ep.target_os),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(
                format!("{} ", ep.address),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("({})", ep.instructions),
                Style::default().fg(Color::Gray),
            ),
        ]));
    }

    for warn in &sess.ledger.warnings {
        top_lines.push(Line::from(Span::styled(
            format!("  ! {warn}"),
            Style::default().fg(Color::LightYellow),
        )));
    }

    let top_panel = Paragraph::new(top_lines)
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(tr(
                    app.lang,
                    " Active Sharing Session (Press 's' or Enter to Stop & Clean Up | Esc: Main Menu) ",
                    " Sessão Ativa (Pressione 's' ou Enter para Parar e Limpar | Esc: Menu Principal) ",
                ))
                .borders(Borders::ALL),
        );
    f.render_widget(top_panel, rows[0]);

    let snap = sess.tracker.snapshot();
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(rows[1]);

    let mut client_lines = vec![
        Line::from(format!(
            " {}: {} | {}: {} | {}: {}",
            tr(app.lang, "Transferred", "Transferido"),
            format_bytes(snap.total_bytes_transferred),
            tr(app.lang, "Active", "Ativas"),
            snap.active_transfer_count,
            tr(app.lang, "Done", "Concluídas"),
            snap.completed_transfer_count
        )),
        Line::from(""),
    ];
    if snap.clients.is_empty() {
        client_lines.push(Line::from(Span::styled(
            tr(
                app.lang,
                " Waiting for destination computers to connect...",
                " Aguardando conexões de destino...",
            ),
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for c in &snap.clients {
            client_lines.push(Line::from(format!(
                " • {} [{}] — {} done, {} active ({})",
                c.client_ip,
                c.protocol.as_str(),
                c.completed_transfers,
                c.active_transfers,
                format_bytes(c.bytes_transferred)
            )));
        }
    }

    let clients_block = Paragraph::new(client_lines).block(
        Block::default()
            .title(tr(app.lang, " Connected Clients ", " Clientes Conectados "))
            .borders(Borders::ALL),
    );
    f.render_widget(clients_block, cols[0]);

    let mut xfer_lines = Vec::new();
    if snap.recent_transfers.is_empty() {
        xfer_lines.push(Line::from(Span::styled(
            tr(
                app.lang,
                " No transfers started yet. (HTTP/WebDAV show exact bytes; SMB shows file open/close.)",
                " Nenhuma transferência iniciada. (HTTP/WebDAV mostram bytes exatos; SMB mostra abertura/fechamento.)",
            ),
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for tr_rec in snap.recent_transfers.iter().take(12) {
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
                "SMB read".to_string()
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
                " Live File Transfer Activity ",
                " Atividade de Transferência ",
            ))
            .borders(Borders::ALL),
    );
    f.render_widget(xfers_block, cols[1]);
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
        Line::from(tr(
            app.lang,
            "Stopping the session terminates Samba/WebDAV/HTTP processes and removes",
            "Parar a sessão encerra os processos Samba/WebDAV/HTTP e remove",
        )),
        Line::from(tr(
            app.lang,
            "LabDrop share definitions and firewall rules.",
            "apenas os compartilhamentos e regras de firewall criados pelo LabDrop.",
        )),
        Line::from(""),
        Line::from(tr(
            app.lang,
            "How should LabDrop handle the temporary staged copy of your files?",
            "O que fazer com a cópia temporária dos arquivos preparados?",
        )),
        Line::from(""),
    ];

    for (i, &c) in choices.iter().enumerate() {
        let style = if i == app.cleanup_choice_idx {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(format!("  {c}  "), style)));
    }

    let modal = Paragraph::new(lines).block(
        Block::default()
            .title(tr(
                app.lang,
                " Confirm Session Stop & Cleanup ",
                " Confirmar Encerramento e Limpeza ",
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );
    f.render_widget(modal, area);
}

fn draw_paste_modal(f: &mut Frame, app: &LabDropTuiApp) {
    let area = centered_rect(72, 34, f.area());
    f.render_widget(Clear, area);

    let lines = vec![
        Line::from(tr(
            app.lang,
            "Drag and drop files/folders onto this terminal, or type/paste path(s):",
            "Arraste e solte arquivos/pastas neste terminal, ou digite/cole o(s) caminho(s):",
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!(" > {}_", app.paste_buffer),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            tr(
                app.lang,
                "[Enter] Add Path(s)   |   [Esc] Cancel",
                "[Enter] Adicionar Caminho(s)   |   [Esc] Cancelar",
            ),
            Style::default().fg(Color::Cyan),
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
            .border_style(Style::default().fg(Color::Green)),
    );
    f.render_widget(modal, area);
}

fn draw_diagnostics(f: &mut Frame, app: &LabDropTuiApp, area: Rect) {
    let env = &app.smb_env;
    let lines = vec![
        Line::from(format!(
            "{}: {}",
            tr(app.lang, "Operating System Family", "Sistema Operacional"),
            env.os_family
        )),
        Line::from(format!(
            "{}: {}",
            tr(app.lang, "Linux Distribution", "Distribuição Linux"),
            env.linux_distro
                .as_ref()
                .map(|d| d.display_name())
                .unwrap_or("N/A")
        )),
        Line::from(format!(
            "{}: {} ({})",
            tr(app.lang, "Samba / SMB Installed", "Samba / SMB Instalado"),
            env.smbd_installed,
            env.smbd_version.as_deref().unwrap_or("N/A")
        )),
        Line::from(format!(
            "{}: {} (Active: {})",
            tr(app.lang, "System Service Name", "Serviço do Sistema"),
            env.service_name,
            env.service_active
        )),
        Line::from(format!(
            "{}: {}",
            tr(app.lang, "Process Elevated (Root)", "Privilégios de Root/Admin"),
            env.is_elevated
        )),
        Line::from(format!(
            "{}: {}",
            tr(app.lang, "Rootless User Namespace", "Namespace Sem Root (unshare)"),
            env.rootless_unshare_available
        )),
        Line::from(format!(
            "{}: {}",
            tr(app.lang, "Active Firewall Manager", "Gerenciador de Firewall"),
            env.firewall_backend
        )),
        Line::from(format!(
            "{}: {}",
            tr(app.lang, "SELinux Enforcing", "SELinux Ativo (Enforcing)"),
            env.selinux_enforcing
        )),
        Line::from(""),
        Line::from(tr(
            app.lang,
            "Press 'c' to scan and clean up any orphaned sessions, or Esc to return.",
            "Pressione 'c' para limpar sessões órfãs, ou Esc para voltar.",
        )),
    ];
    let p = Paragraph::new(lines).block(
        Block::default()
            .title(tr(
                app.lang,
                " System Diagnostics & Environment Inspection ",
                " Diagnóstico do Sistema e Ambiente ",
            ))
            .borders(Borders::ALL),
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
        .border_style(Style::default().fg(Color::Cyan));
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
        .gauge_style(Style::default().fg(Color::Cyan))
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
