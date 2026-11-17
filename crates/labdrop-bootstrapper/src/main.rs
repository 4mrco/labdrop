use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use labdrop_core::{
    bootstrapper::{
        discover_candidate_destinations, find_default_local_artifact_dir, prepare_usb_package,
        ArtifactSource, CandidateDestination, PreparedPackageReport, DEFAULT_RELEASE_BASE_URL,
    },
    fs_space::{format_bytes, query_disk_space},
    i18n::Lang,
    session::{ensure_interactive_terminal_on_linux, is_terminal_disconnected},
    staging::parse_dropped_paths,
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use std::{
    io::{self, Stdout},
    path::PathBuf,
    sync::mpsc,
    thread,
    time::Duration,
};

const ACCENT: Color = Color::Cyan;
const TEXT: Color = Color::White;
const MUTED: Color = Color::Gray;
const BORDER_MUTED: Color = Color::DarkGray;
const STATUS_OK: Color = Color::Green;
const STATUS_ERR: Color = Color::Red;

fn tr<'a>(lang: Lang, en: &'a str, pt: &'a str) -> &'a str {
    match lang {
        Lang::En => en,
        Lang::PtBr => pt,
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "labdrop-bootstrapper",
    version,
    about = "LabDrop Bootstrapper — Installs or updates LabDrop on a USB drive (Portable) or local computer folder"
)]
struct Cli {
    /// Destination directory or USB mount point (launches interactive TUI if omitted).
    #[arg(short, long = "dest")]
    dest: Option<PathBuf>,

    /// Offline mode: directory containing compiled `labdrop` and `LabDrop.exe` (and optional `SHA256SUMS.txt`).
    #[arg(short, long = "source-dir")]
    source_dir: Option<PathBuf>,

    /// Online mode: base release URL from which to download `labdrop`, `LabDrop.exe`, and `SHA256SUMS.txt`.
    #[arg(short, long = "release-url")]
    release_url: Option<String>,

    /// Interface language: 'en' or 'pt'.
    #[arg(long)]
    lang: Option<String>,

    /// Output preparation report as JSON (in CLI mode).
    #[arg(long)]
    json: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let lang = cli
        .lang
        .as_deref()
        .and_then(Lang::parse_str)
        .unwrap_or_else(Lang::detect_from_env);

    if let Some(dest) = cli.dest {
        let source = if let Some(dir) = cli.source_dir {
            ArtifactSource::LocalDirectory(dir)
        } else if let Some(url) = cli.release_url {
            ArtifactSource::ReleaseBaseUrl(url)
        } else if let Some(auto_local) = find_default_local_artifact_dir() {
            ArtifactSource::LocalDirectory(auto_local)
        } else {
            ArtifactSource::ReleaseBaseUrl(DEFAULT_RELEASE_BASE_URL.to_string())
        };

        let report = prepare_usb_package(&dest, &source, |step| {
            if !cli.json {
                println!("• {step}");
            }
        })?;

        if cli.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            let title = if report.was_update {
                "=== LabDrop Package Updated Successfully ==="
            } else {
                "=== LabDrop Package Installed Successfully ==="
            };
            println!("\n{title}");
            println!("Package Directory:   {}", report.labdrop_dir.display());
            println!(
                "Linux Executable:    {} ({}, SHA-256: {})",
                report.linux_binary_path.display(),
                format_bytes(report.linux_binary_size),
                report.linux_binary_sha256
            );
            println!(
                "Windows Executable:  {} ({}, SHA-256: {})",
                report.windows_binary_path.display(),
                format_bytes(report.windows_binary_size),
                report.windows_binary_sha256
            );
            println!("Carrier Files Dir:   {}", report.files_dir.display());
            println!("Config Directory:    {}", report.config_dir.display());
            println!("Checksums Manifest:  {}", report.checksums_file.display());
        }
        return Ok(());
    }

    // If launched by double-clicking in a Linux GUI file manager without a TTY, auto-spawn a terminal window
    if ensure_interactive_terminal_on_linux() {
        return Ok(());
    }

    run_bootstrapper_tui(lang, cli.source_dir, cli.release_url)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputModal {
    None,
    CustomDest,
    CustomSource,
}

struct BootstrapperTuiApp {
    lang: Lang,
    candidates: Vec<CandidateDestination>,
    selected_idx: usize,
    use_offline_local: bool,
    local_dir: PathBuf,
    release_url: String,
    logs: Vec<String>,
    report: Option<PreparedPackageReport>,
    error: Option<String>,
    modal: InputModal,
    modal_buffer: String,
    should_quit: bool,
}

fn run_bootstrapper_tui(
    lang: Lang,
    source_dir_opt: Option<PathBuf>,
    release_url_opt: Option<String>,
) -> Result<()> {
    let candidates = discover_candidate_destinations();
    let auto_local = source_dir_opt.or_else(find_default_local_artifact_dir);
    let use_offline_local = auto_local.is_some();
    let local_dir = auto_local.unwrap_or_else(|| PathBuf::from("./dist"));
    let release_url = release_url_opt.unwrap_or_else(|| DEFAULT_RELEASE_BASE_URL.to_string());

    let mut app = BootstrapperTuiApp {
        lang,
        candidates,
        selected_idx: 0,
        use_offline_local,
        local_dir,
        release_url,
        logs: vec![
            tr(
                lang,
                "Choose a USB drive (portable) or local computer folder, then press Enter or click [Install / Update].",
                "Escolha um pendrive USB (portátil) ou pasta local no computador e pressione Enter ou clique em [Instalar / Atualizar].",
            )
            .to_string(),
            tr(
                lang,
                "Safe operation: never formats drives; updates preserve your existing files/ and config/ folders.",
                "Operação segura: nunca formata unidades; atualizações preservam suas pastas files/ e config/.",
            )
            .to_string(),
            tr(
                lang,
                "Note: Installs LabDrop executables only (Windows + Linux). Does not install system packages like Samba.",
                "Nota: Instala apenas os executáveis do LabDrop (Windows + Linux). Não instala pacotes do sistema como o Samba.",
            )
            .to_string(),
        ],
        report: None,
        error: None,
        modal: InputModal::None,
        modal_buffer: String::new(),
        should_quit: false,
    };

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

    let res = run_loop(&mut terminal, &mut app);

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

fn add_or_select_custom_dest(app: &mut BootstrapperTuiApp, path: PathBuf) {
    if let Some(idx) = app.candidates.iter().position(|c| c.path == path) {
        app.selected_idx = idx;
        return;
    }
    let avail = query_disk_space(&path)
        .map(|s| s.available_bytes)
        .unwrap_or(0);
    let has_existing =
        path.join("LabDrop").join("labdrop").is_file() || path.join("labdrop").is_file();
    let tag = if has_existing {
        "[UPDATE Custom]"
    } else {
        "[Custom]"
    };
    app.candidates.insert(
        0,
        CandidateDestination {
            label: format!(
                "{tag} {} ({} free)",
                path.display(),
                format_bytes(avail)
            ),
            path,
            available_bytes: avail,
            has_existing_labdrop: has_existing,
        },
    );
    app.selected_idx = 0;
}

fn trigger_install(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut BootstrapperTuiApp,
) {
    if let Some(dest) = app.candidates.get(app.selected_idx).cloned() {
        app.error = None;
        app.report = None;
        app.logs.clear();
        let source = if app.use_offline_local {
            ArtifactSource::LocalDirectory(app.local_dir.clone())
        } else {
            ArtifactSource::ReleaseBaseUrl(app.release_url.clone())
        };
        let res = prepare_usb_package(&dest.path, &source, |step| {
            app.logs.push(step.to_string());
            let _ = terminal.draw(|f| draw_bootstrapper_ui(f, app));
        });
        match res {
            Ok(rep) => {
                app.report = Some(rep);
                app.candidates = discover_candidate_destinations();
            }
            Err(e) => app.error = Some(format!("{e}")),
        }
    }
}

fn spawn_input_reader() -> mpsc::Receiver<Event> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || loop {
        if is_terminal_disconnected() {
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

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut BootstrapperTuiApp,
) -> Result<()> {
    let rx = spawn_input_reader();
    while !app.should_quit {
        if is_terminal_disconnected() {
            break;
        }
        if terminal.draw(|f| draw_bootstrapper_ui(f, app)).is_err() {
            break;
        }

        match rx.recv_timeout(Duration::from_millis(150)) {
            Ok(Event::Paste(text)) => {
                if app.modal != InputModal::None {
                    app.modal_buffer.push_str(text.trim());
                } else if let Some(first_path) = parse_dropped_paths(&text).into_iter().next() {
                    if first_path.is_dir() {
                        add_or_select_custom_dest(app, first_path);
                    }
                }
            }
            Ok(Event::Mouse(mouse)) => {
                let area = terminal.size().map(|s| Rect::new(0, 0, s.width, s.height)).unwrap_or_default();
                handle_mouse_event(terminal, app, mouse, area);
            }
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')
                {
                    app.should_quit = true;
                    continue;
                }

                if app.modal != InputModal::None {
                    match key.code {
                        KeyCode::Esc => app.modal = InputModal::None,
                        KeyCode::Backspace => {
                            app.modal_buffer.pop();
                        }
                        KeyCode::Enter => {
                            let raw = std::mem::take(&mut app.modal_buffer);
                            let cleaned = raw.trim().trim_matches('"').trim_matches('\'');
                            match app.modal {
                                InputModal::CustomDest => {
                                    if !cleaned.is_empty() {
                                        add_or_select_custom_dest(app, PathBuf::from(cleaned));
                                    }
                                }
                                InputModal::CustomSource => {
                                    if !cleaned.is_empty() {
                                        if cleaned.starts_with("http://")
                                            || cleaned.starts_with("https://")
                                        {
                                            app.release_url = cleaned.to_string();
                                            app.use_offline_local = false;
                                        } else {
                                            app.local_dir = PathBuf::from(cleaned);
                                            app.use_offline_local = true;
                                        }
                                    }
                                }
                                InputModal::None => {}
                            }
                            app.modal = InputModal::None;
                        }
                        KeyCode::Char(c) => {
                            app.modal_buffer.push(c);
                        }
                        _ => {}
                    }
                    continue;
                }

                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
                    KeyCode::Char('l') | KeyCode::Char('L') => {
                        app.lang = app.lang.toggle();
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        app.selected_idx = app.selected_idx.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if app.selected_idx + 1 < app.candidates.len() {
                            app.selected_idx += 1;
                        }
                    }
                    KeyCode::Tab | KeyCode::Char('m') => {
                        app.use_offline_local = !app.use_offline_local;
                    }
                    KeyCode::Char('d') | KeyCode::Char('D') => {
                        app.modal_buffer.clear();
                        app.modal = InputModal::CustomDest;
                    }
                    KeyCode::Char('u') | KeyCode::Char('U') => {
                        app.modal_buffer = if app.use_offline_local {
                            app.local_dir.display().to_string()
                        } else {
                            app.release_url.clone()
                        };
                        app.modal = InputModal::CustomSource;
                    }
                    KeyCode::Char('r') => {
                        app.candidates = discover_candidate_destinations();
                        app.selected_idx = 0;
                    }
                    KeyCode::Enter => {
                        trigger_install(terminal, app);
                    }
                    _ => {}
                }
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

fn handle_mouse_event(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut BootstrapperTuiApp,
    mouse: MouseEvent,
    area: Rect,
) {
    if app.modal != InputModal::None {
        return;
    }

    match mouse.kind {
        MouseEventKind::ScrollUp => {
            app.selected_idx = app.selected_idx.saturating_sub(1);
        }
        MouseEventKind::ScrollDown => {
            if app.selected_idx + 1 < app.candidates.len() {
                app.selected_idx += 1;
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(8),
                    Constraint::Length(5),
                    Constraint::Min(6),
                    Constraint::Length(3),
                ])
                .split(area);

            let row = mouse.row;
            let col = mouse.column;

            // Top bar: clicking on the right side toggles language [L]
            if row >= chunks[0].y && row < chunks[0].y + chunks[0].height {
                if col + 16 >= chunks[0].x + chunks[0].width {
                    app.lang = app.lang.toggle();
                }
                return;
            }

            // Section 1: Destination list
            if row > chunks[1].y && row + 1 < chunks[1].y + chunks[1].height {
                let clicked_idx = (row - chunks[1].y - 1) as usize;
                if clicked_idx < app.candidates.len() {
                    app.selected_idx = clicked_idx;
                }
                return;
            }

            // Section 2: Action buttons
            if row > chunks[2].y && row + 1 < chunks[2].y + chunks[2].height {
                let rel_row = row - chunks[2].y - 1;
                if rel_row == 0 {
                    app.use_offline_local = !app.use_offline_local;
                } else if rel_row == 1 {
                    let third = chunks[2].width / 3;
                    if col < chunks[2].x + third {
                        trigger_install(terminal, app);
                    } else if col < chunks[2].x + third * 2 {
                        app.modal_buffer.clear();
                        app.modal = InputModal::CustomDest;
                    } else {
                        app.candidates = discover_candidate_destinations();
                        app.selected_idx = 0;
                    }
                }
                return;
            }

            // Bottom shortcut bar
            if row >= chunks[4].y && row < chunks[4].y + chunks[4].height {
                let w = chunks[4].width.max(1);
                let fifth = w / 5;
                if col < fifth {
                    trigger_install(terminal, app);
                } else if col < fifth * 2 {
                    app.modal_buffer.clear();
                    app.modal = InputModal::CustomDest;
                } else if col < fifth * 3 {
                    app.candidates = discover_candidate_destinations();
                    app.selected_idx = 0;
                } else if col < fifth * 4 {
                    app.lang = app.lang.toggle();
                } else {
                    app.should_quit = true;
                }
            }
        }
        _ => {}
    }
}

fn draw_bootstrapper_ui(f: &mut Frame, app: &BootstrapperTuiApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(8),
            Constraint::Length(5),
            Constraint::Min(6),
            Constraint::Length(3),
        ])
        .split(f.area());

    let hdr = Paragraph::new(Line::from(vec![
        Span::styled(
            concat!(" LabDrop Bootstrapper v", env!("CARGO_PKG_VERSION"), " "),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            tr(
                app.lang,
                "— Portable USB & Local PC Installer (Windows + Linux)  ",
                "— Instalador Portátil (USB) e Local (PC) (Windows + Linux)  ",
            ),
            Style::default().fg(MUTED),
        ),
        Span::styled(
            format!(" [L] {} ", app.lang.code()),
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
    ]))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT)),
    );
    f.render_widget(hdr, chunks[0]);

    let items: Vec<ListItem> = app
        .candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let m = if i == app.selected_idx { "▸" } else { " " };
            let style = if c.has_existing_labdrop {
                Style::default().fg(STATUS_OK)
            } else {
                Style::default().fg(TEXT)
            };
            ListItem::new(format!(" {m} {}", c.label)).style(style)
        })
        .collect();
    let mut state = ListState::default();
    if !items.is_empty() {
        state.select(Some(app.selected_idx));
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
                .title(tr(
                    app.lang,
                    " 1. Destination: USB Drive (Portable) or Local Folder ",
                    " 1. Destino: Pendrive USB (Portátil) ou Pasta Local no Computador ",
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT)),
        );
    f.render_stateful_widget(list, chunks[1], &mut state);

    let src_desc = if app.use_offline_local {
        format!(
            "{}: {}",
            tr(
                app.lang,
                "Offline Local Release Directory",
                "Diretório Local de Release (Offline)"
            ),
            app.local_dir.display()
        )
    } else {
        format!(
            "{}: {}",
            tr(
                app.lang,
                "GitHub Release Download (Online)",
                "Download do GitHub Releases (Online)"
            ),
            app.release_url
        )
    };
    let mode_box = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                tr(app.lang, " [Tab/m] Source: ", " [Tab/m] Origem: "),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                src_desc,
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                tr(app.lang, "   ([u] Edit URL/Path)", "   ([u] Editar URL/Caminho)"),
                Style::default().fg(MUTED),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                tr(
                    app.lang,
                    " [Enter] Install / Update LabDrop ",
                    " [Enter] Instalar / Atualizar LabDrop ",
                ),
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled(
                tr(app.lang, " [d] Custom Destination ", " [d] Outro Caminho "),
                Style::default().fg(ACCENT),
            ),
            Span::raw("   "),
            Span::styled(
                tr(app.lang, " [r] Refresh Drives ", " [r] Atualizar Unidades "),
                Style::default().fg(ACCENT),
            ),
        ]),
    ])
    .block(
        Block::default()
            .title(tr(
                app.lang,
                " 2. Source & Actions (Click or Press Key) ",
                " 2. Origem e Ações (Clique ou Teclado) ",
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(BORDER_MUTED)),
    );
    f.render_widget(mode_box, chunks[2]);

    let mut out_lines = Vec::new();
    for l in &app.logs {
        out_lines.push(Line::from(Span::styled(
            format!(" • {l}"),
            Style::default().fg(MUTED),
        )));
    }
    if let Some(ref rep) = app.report {
        out_lines.push(Line::from(""));
        let msg = if rep.was_update {
            tr(
                app.lang,
                "SUCCESS: Updated existing LabDrop installation",
                "SUCESSO: Instalação do LabDrop atualizada",
            )
        } else {
            tr(
                app.lang,
                "SUCCESS: LabDrop package installed and verified",
                "SUCESSO: Pacote LabDrop instalado e verificado",
            )
        };
        out_lines.push(Line::from(Span::styled(
            format!(" ✓ {msg} -> {}", rep.labdrop_dir.display()),
            Style::default().fg(STATUS_OK).add_modifier(Modifier::BOLD),
        )));
        out_lines.push(Line::from(format!(
            "   - Linux:   {} ({}, SHA-256: {}…)",
            rep.linux_binary_path.display(),
            format_bytes(rep.linux_binary_size),
            &rep.linux_binary_sha256[..16]
        )));
        out_lines.push(Line::from(format!(
            "   - Windows: {} ({}, SHA-256: {}…)",
            rep.windows_binary_path.display(),
            format_bytes(rep.windows_binary_size),
            &rep.windows_binary_sha256[..16]
        )));
    }
    if let Some(ref err) = app.error {
        out_lines.push(Line::from(""));
        out_lines.push(Line::from(Span::styled(
            format!(" ✗ {}: {err}", tr(app.lang, "ERROR", "ERRO")),
            Style::default().fg(STATUS_ERR).add_modifier(Modifier::BOLD),
        )));
    }

    let status_panel = Paragraph::new(out_lines)
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(tr(
                    app.lang,
                    " 3. Verification & Status Log ",
                    " 3. Verificação e Log de Status ",
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(BORDER_MUTED)),
        );
    f.render_widget(status_panel, chunks[3]);

    let shortcuts = Line::from(vec![
        Span::styled(" [Enter] ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(tr(app.lang, "Install/Update  ", "Instalar/Atualizar  "), Style::default().fg(MUTED)),
        Span::styled("[d] ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(tr(app.lang, "Custom Path  ", "Caminho Manual  "), Style::default().fg(MUTED)),
        Span::styled("[r] ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(tr(app.lang, "Refresh  ", "Atualizar  "), Style::default().fg(MUTED)),
        Span::styled("[L] ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(tr(app.lang, "Language  ", "Idioma  "), Style::default().fg(MUTED)),
        Span::styled("[q/Esc] ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(tr(app.lang, "Quit", "Sair"), Style::default().fg(MUTED)),
    ]);
    let footer = Paragraph::new(shortcuts).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(BORDER_MUTED)),
    );
    f.render_widget(footer, chunks[4]);

    if app.modal != InputModal::None {
        draw_input_modal(f, app);
    }
}

fn draw_input_modal(f: &mut Frame, app: &BootstrapperTuiApp) {
    let area = centered_rect(72, 30, f.area());
    f.render_widget(Clear, area);

    let title = match app.modal {
        InputModal::CustomDest => tr(
            app.lang,
            " Enter Custom Destination (USB Mount or Local Folder) ",
            " Digitar Caminho de Destino (Pendrive USB ou Pasta Local) ",
        ),
        InputModal::CustomSource => tr(
            app.lang,
            " Enter Local Release Dir or HTTP(S) URL ",
            " Digitar Diretório Local ou URL HTTP(S) ",
        ),
        InputModal::None => "",
    };

    let p = Paragraph::new(vec![
        Line::from(format!(" > {}_", app.modal_buffer)),
        Line::from(""),
        Line::from(Span::styled(
            tr(
                app.lang,
                "[Enter] Confirm  |  [Esc] Cancel",
                "[Enter] Confirmar  |  [Esc] Cancelar",
            ),
            Style::default().fg(ACCENT),
        )),
    ])
    .wrap(Wrap { trim: false })
    .block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT)),
    );
    f.render_widget(p, area);
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
