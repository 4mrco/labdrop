use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind,
        KeyModifiers,
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
    session::is_terminal_disconnected,
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
    about = "LabDrop USB Bootstrapper — Prepares or updates a cross-platform LabDrop USB drive containing both Windows and Linux executables"
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
                "=== LabDrop USB Package Updated Successfully ==="
            } else {
                "=== LabDrop USB Package Prepared Successfully ==="
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
                "Select a USB drive or folder and press Enter to install or update LabDrop.",
                "Selecione um pendrive ou pasta e pressione Enter para instalar ou atualizar o LabDrop.",
            )
            .to_string(),
            tr(
                lang,
                "Note: Never formats your drive; updates preserve existing files/ and config/.",
                "Nota: Nunca formata o pendrive; atualizações preservam suas pastas files/ e config/.",
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
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run_loop(&mut terminal, &mut app);

    if is_terminal_disconnected() {
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
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c')
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

fn draw_bootstrapper_ui(f: &mut Frame, app: &BootstrapperTuiApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(8),
            Constraint::Length(5),
            Constraint::Min(8),
        ])
        .split(f.area());

    let hdr = Paragraph::new(Line::from(vec![
        Span::styled(
            " LabDrop USB Bootstrapper v0.1.0 ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(tr(
            app.lang,
            "— Installs or Updates Windows (LabDrop.exe) + Linux (labdrop)  ",
            "— Instala ou Atualiza Windows (LabDrop.exe) + Linux (labdrop)  ",
        )),
        Span::styled(
            format!(" [L] {} ", app.lang.code()),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ]))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(hdr, chunks[0]);

    let items: Vec<ListItem> = app
        .candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let m = if i == app.selected_idx { "(●)" } else { "( )" };
            let style = if c.has_existing_labdrop {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::White)
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
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .block(
            Block::default()
                .title(tr(
                    app.lang,
                    " 1. Select USB Drive / Destination (Up/Down | d: Custom Path | r: Refresh) ",
                    " 1. Selecionar Pendrive / Destino (Setas | d: Caminho Manual | r: Atualizar) ",
                ))
                .borders(Borders::ALL),
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
                "Online Release Download",
                "Download Online de Release"
            ),
            app.release_url
        )
    };
    let mode_box = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                tr(app.lang, " [Tab / m] Source: ", " [Tab / m] Origem: "),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(
                src_desc,
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(tr(
            app.lang,
            " [Enter] Install / Update Pendrive   |   [u] Edit Source/URL   |   [q / Esc] Quit",
            " [Enter] Instalar / Atualizar Pendrive   |   [u] Editar Origem/URL   |   [q / Esc] Sair",
        )),
    ])
    .block(
        Block::default()
            .title(tr(
                app.lang,
                " 2. Artifact Source & Action ",
                " 2. Origem dos Binários e Ação ",
            ))
            .borders(Borders::ALL),
    );
    f.render_widget(mode_box, chunks[2]);

    let mut out_lines = Vec::new();
    for l in &app.logs {
        out_lines.push(Line::from(format!(" • {l}")));
    }
    if let Some(ref rep) = app.report {
        out_lines.push(Line::from(""));
        let msg = if rep.was_update {
            tr(
                app.lang,
                "SUCCESS: Updated existing LabDrop installation",
                "SUCESSO: Instalação LabDrop existente atualizada",
            )
        } else {
            tr(
                app.lang,
                "SUCCESS: LabDrop USB package ready",
                "SUCESSO: Pacote USB LabDrop pronto",
            )
        };
        out_lines.push(Line::from(Span::styled(
            format!(" ✓ {msg} at '{}'", rep.labdrop_dir.display()),
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
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
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
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
                .borders(Borders::ALL),
        );
    f.render_widget(status_panel, chunks[3]);

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
            " Enter Custom Destination / USB Mount Path ",
            " Digitar Caminho do Pendrive / Destino ",
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
            tr(app.lang, "[Enter] Confirm  |  [Esc] Cancel", "[Enter] Confirmar  |  [Esc] Cancelar"),
            Style::default().fg(Color::Cyan),
        )),
    ])
    .wrap(Wrap { trim: false })
    .block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
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
