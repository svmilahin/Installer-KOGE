use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

// ==================== СОБЫТИЯ ДЛЯ TUI ====================

#[derive(Clone)]
enum LogEvent {
    Line(String),
    Progress(f64, String),
    Done(bool, String),
}

// ==================== УТИЛИТЫ ====================

/// Убирает ANSI escape-коды и \r из строки, чтобы не ломать TUI
fn clean_line(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&c2) = chars.peek() {
                    chars.next();
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else if c == '\r' {
            // пропускаем
        } else {
            out.push(c);
        }
    }
    out
}

/// Парсит все "[ a/b]" в строке и возвращает (max_done, max_total)
fn parse_progress(s: &str) -> Option<(u32, u32)> {
    let mut best: Option<(u32, u32)> = None;
    for chunk in s.split('[').skip(1) {
        if let Some(end) = chunk.find(']') {
            let inner = &chunk[..end];
            if let Some(slash) = inner.find('/') {
                if let (Ok(a), Ok(b)) = (
                    inner[..slash].trim().parse::<u32>(),
                    inner[slash + 1..].trim().parse::<u32>(),
                ) {
                    if b > 0 && a <= b {
                        best = match best {
                            None => Some((a, b)),
                            Some((da, db)) => Some(if b > db { (a, b) } else { (da, db) }),
                        };
                    }
                }
            }
        }
    }
    best
}

// ==================== ЗАПУСК ПРОЦЕССА С ПЕРЕХВАТОМ ====================

/// Запускает команду, перехватывая stdout (в лог) и stderr (в прогресс).
/// Возвращает ExitStatus.
fn run_process(mut cmd: Command, tx: &mpsc::Sender<LogEvent>) -> io::Result<ExitStatus> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;

    // Поток для stderr: парсим прогресс "[ a/b]"
    let stderr = child.stderr.take().unwrap();
    let tx_err = tx.clone();
    let err_handle = thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            if let Ok(l) = line {
                if let Some((done, total)) = parse_progress(&l) {
                    let pct = done as f64 / total as f64;
                    let _ = tx_err.send(LogEvent::Progress(
                        pct,
                        format!("{} / {}", done, total),
                    ));
                }
            }
        }
    });

    // Текущий поток читает stdout -> в лог
    let stdout = child.stdout.take().unwrap();
    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        if let Ok(l) = line {
            let cleaned = clean_line(&l);
            if !cleaned.trim().is_empty() {
                let _ = tx.send(LogEvent::Line(cleaned));
            }
        }
    }

    let status = child.wait()?;
    let _ = err_handle.join();
    Ok(status)
}

// ==================== ЛОГИКА ====================

fn detect_os() -> String {
    if let Ok(content) = fs::read_to_string("/etc/os-release") {
        let lower = content.to_lowercase();
        if lower.contains("rosa") || lower.contains("mosos") || lower.contains("mos") {
            return "rosa".to_string();
        }
        if lower.contains("alt") {
            return "alt".to_string();
        }
    }
    "unknown".to_string()
}

fn detect_arch() -> String {
    Command::new("uname")
        .arg("-m")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_lowercase().to_string())
        .unwrap_or_else(|_| "x86_64".to_string())
}

fn get_folder_size(path: &str) -> u64 {
    let mut total: u64 = 0;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.is_file() {
                if let Ok(meta) = fs::metadata(&p) {
                    total += meta.len();
                }
            } else if p.is_dir() {
                total += get_folder_size(&p.to_string_lossy());
            }
        }
    }
    total
}

fn compress_folder(folder: &str, archive: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let _ = fs::create_dir_all(folder);
    let size_before = get_folder_size(folder);
    let _ = fs::remove_file(archive);

    let _ = tx.send(LogEvent::Progress(0.3, "Упаковка...".to_string()));
    let cmd = Command::new("tar");
    let status = run_process(cmd, tx);

    match status {
        Ok(s) if s.success() => {
            let info = if let Ok(meta) = fs::metadata(archive) {
                let size_after = meta.len();
                let ratio = if size_before > 0 {
                    (1.0 - size_after as f64 / size_before as f64) * 100.0
                } else {
                    0.0
                };
                format!(
                    "Архив: {:.2} MB (было: {:.2} MB, экономия: {:.1}%)",
                    size_after as f64 / 1024.0 / 1024.0,
                    size_before as f64 / 1024.0 / 1024.0,
                    ratio
                )
            } else {
                "Архив создан".to_string()
            };
            (true, info)
        }
        Ok(s) => (false, format!("Ошибка архивации. Код: {:?}", s.code())),
        Err(e) => (false, format!("Не удалось запустить tar: {}", e)),
    }
}

fn decompress_archive(archive: &str, folder: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    if !Path::new(archive).exists() {
        return (false, format!("Архив не найден: {}", archive));
    }
    let _ = fs::create_dir_all(folder);
    let _ = tx.send(LogEvent::Progress(0.5, "Распаковка...".to_string()));
    let mut cmd = Command::new("tar");
    cmd.arg("-xzf").arg(archive).arg("-C").arg(".");
    match run_process(cmd, tx) {
        Ok(s) if s.success() => (true, format!("Распаковано в {}", folder)),
        Ok(s) => (false, format!("Ошибка распаковки. Код: {:?}", s.code())),
        Err(e) => (false, format!("Не удалось запустить tar: {}", e)),
    }
}

fn download_updates(download_dir: &str, os_type: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let _ = fs::create_dir_all(download_dir);
    let pm = if os_type == "rosa" { "dnf" } else { "apt-get" };

    if os_type == "rosa" {
        let mut cmd = Command::new(pm);
        cmd.arg("upgrade")
            .arg("--downloadonly")
            .arg(format!("--destdir={}", download_dir))
            .arg("-y");
        match run_process(cmd, tx) {
            Ok(s) if s.success() => (true, "Обновления скачаны".to_string()),
            Ok(s) => (false, format!("Ошибка скачивания. Код: {:?}", s.code())),
            Err(e) => (false, format!("Не удалось запустить {}: {}", pm, e)),
        }
    } else {
        // Alt Linux
        let mut cmd1 = Command::new(pm);
        cmd1.arg("update");
        let _ = run_process(cmd1, tx);

        let sim_output = Command::new(pm)
            .arg("upgrade")
            .arg("--simulate")
            .output();

        let packages: Vec<String> = match sim_output {
            Ok(output) => String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|l| l.starts_with("Inst "))
                .filter_map(|l| l.split_whitespace().nth(1).map(String::from))
                .collect(),
            Err(e) => return (false, format!("Ошибка: {}", e)),
        };

        if packages.is_empty() {
            return (true, "Система уже обновлена".to_string());
        }

        let total = packages.len();
        let mut done = 0;
        let mut all_ok = true;
        for pkg in &packages {
            let mut cmd = Command::new(pm);
            cmd.arg("install")
                .arg("--download-only")
                .arg("-y")
                .arg(format!("-o Dir::Cache::Archives={}", download_dir))
                .arg(pkg);
            if let Ok(s) = run_process(cmd, tx) {
                if !s.success() {
                    all_ok = false;
                }
            } else {
                all_ok = false;
            }
            done += 1;
            let _ = tx.send(LogEvent::Progress(done as f64 / total as f64, format!("{} / {}", done, total)));
        }
        if all_ok {
            (true, format!("Скачано пакетов: {}", packages.len()))
        } else {
            (false, "Некоторые пакеты не удалось скачать".to_string())
        }
    }
}

fn download_packages(packages: &[&str], download_dir: &str, os_type: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let _ = fs::create_dir_all(download_dir);
    let pm = if os_type == "rosa" { "dnf" } else { "apt-get" };

    if os_type == "rosa" {
        let mut cmd = Command::new(pm);
        cmd.arg("download")
            .arg("--resolve")
            .arg("--alldeps")
            .arg("--exclude=*.i686")
            .arg("--exclude=*.i586")
            .arg("--exclude=*.i386")
            .arg(format!("--destdir={}", download_dir));
        for pkg in packages {
            cmd.arg(pkg);
        }
        match run_process(cmd, tx) {
            Ok(s) if s.success() => (true, "Пакеты скачаны".to_string()),
            Ok(s) => (false, format!("Ошибка. Код: {:?}", s.code())),
            Err(e) => (false, format!("Не удалось запустить {}: {}", pm, e)),
        }
    } else {
        let total = packages.len();
        let mut done = 0;
        let mut all_ok = true;
        for pkg in packages {
            let mut cmd = Command::new(pm);
            cmd.arg("install")
                .arg("--download-only")
                .arg("-y")
                .arg(format!("-o Dir::Cache::Archives={}", download_dir))
                .arg(pkg);
            if let Ok(s) = run_process(cmd, tx) {
                if !s.success() {
                    all_ok = false;
                }
            } else {
                all_ok = false;
            }
            done += 1;
            let _ = tx.send(LogEvent::Progress(done as f64 / total as f64, format!("{} / {}", done, total)));
        }
        if all_ok {
            (true, "Все пакеты скачаны".to_string())
        } else {
            (false, "Некоторые пакеты не удалось скачать".to_string())
        }
    }
}

fn is_package_for_arch(filename: &str, system_arch: &str) -> bool {
    let path = Path::new(filename);
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    if let Some(arch) = stem.split('.').last() {
        if arch == "noarch" || arch == system_arch {
            return true;
        }
        if system_arch == "x86_64" && arch == "i686" {
            return false;
        }
        return false;
    }
    true
}

fn install_from_folder(download_dir: &str, os_type: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let system_arch = detect_arch();
    let extension = if os_type == "rosa" { "rpm" } else { "deb" };
    let pm = if os_type == "rosa" { "dnf" } else { "apt-get" };

    let all_files: Vec<String> = match fs::read_dir(download_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let path = e.path();
                if path.extension()?.to_str()? == extension {
                    path.to_str().map(String::from)
                } else {
                    None
                }
            })
            .collect(),
        Err(e) => return (false, format!("Ошибка чтения папки: {}", e)),
    };

    if all_files.is_empty() {
        return (false, format!("В папке не найдено .{} файлов", extension));
    }

    let files: Vec<String> = all_files
        .into_iter()
        .filter(|f| is_package_for_arch(f, &system_arch))
        .collect();

    if files.is_empty() {
        return (false, format!("Нет пакетов под архитектуру {}", system_arch));
    }

    let _ = tx.send(LogEvent::Line(format!("Найдено пакетов для установки: {}", files.len())));

    if os_type == "rosa" {
        let mut cmd = Command::new(pm);
        cmd.arg("install")
            .arg("-y")
            .arg("--allowerasing")
            .arg("--nobest");
        for file in &files {
            cmd.arg(file);
        }
        match run_process(cmd, tx) {
            Ok(s) if s.success() => (true, format!("Установлено пакетов: {}", files.len())),
            Ok(s) => (false, format!("Ошибка установки. Код: {:?}", s.code())),
            Err(e) => (false, format!("Не удалось запустить установку: {}", e)),
        }
    } else {
        let mut cmd = Command::new("dpkg");
        cmd.arg("-i");
        for file in &files {
            cmd.arg(file);
        }
        match run_process(cmd, tx) {
            Ok(s) if s.success() => {
                let mut cmd2 = Command::new(pm);
                cmd2.arg("install").arg("-f").arg("-y");
                let _ = run_process(cmd2, tx);
                (true, format!("Установлено пакетов: {}", files.len()))
            }
            Ok(s) => (false, format!("Ошибка установки. Код: {:?}", s.code())),
            Err(e) => (false, format!("Не удалось запустить dpkg: {}", e)),
        }
    }
}

fn get_packages() -> Vec<&'static str> {
    vec!["task-koge", "r7-office", "notepadqq"]
}

/// Диспетчер команд — вызывается в worker-потоке
fn run_command(
    cmd_id: &str,
    os_type: String,
    download_dir: String,
    archive_name: String,
    tx: &mpsc::Sender<LogEvent>,
) {
    let result: (bool, String) = match cmd_id {
        "download_updates" => {
            let (ok, msg) = download_updates(&download_dir, &os_type, tx);
            if ok {
                compress_folder(&download_dir, &archive_name, tx)
            } else {
                (false, msg)
            }
        }
        "install_updates" => {
            if Path::new(&archive_name).exists() {
                let (ok, msg) = decompress_archive(&archive_name, &download_dir, tx);
                if !ok {
                    (false, msg)
                } else {
                    install_from_folder(&download_dir, &os_type, tx)
                }
            } else if Path::new(&download_dir).exists() {
                install_from_folder(&download_dir, &os_type, tx)
            } else {
                (false, "Не найден ни архив, ни папка".to_string())
            }
        }
        "download_packages" => {
            let packages = get_packages();
            let (ok, msg) = download_packages(&packages, &download_dir, &os_type, tx);
            if ok {
                compress_folder(&download_dir, &archive_name, tx)
            } else {
                (false, msg)
            }
        }
        "install_packages" => {
            if Path::new(&archive_name).exists() {
                let (ok, msg) = decompress_archive(&archive_name, &download_dir, tx);
                if !ok {
                    (false, msg)
                } else {
                    install_from_folder(&download_dir, &os_type, tx)
                }
            } else if Path::new(&download_dir).exists() {
                install_from_folder(&download_dir, &os_type, tx)
            } else {
                (false, "Не найден ни архив, ни папка".to_string())
            }
        }
        "detect_os" => {
            let mut info = format!("ОС: {} | Архитектура: {}", os_type, detect_arch());
            info += &format!(" | Папка: {}", Path::new(&download_dir).exists());
            info += &format!(" | Архив: {}", Path::new(&archive_name).exists());
            if Path::new(&download_dir).exists() {
                let size = get_folder_size(&download_dir);
                info += &format!(" | Размер папки: {:.2} MB", size as f64 / 1024.0 / 1024.0);
            }
            if Path::new(&archive_name).exists() {
                if let Ok(meta) = fs::metadata(&archive_name) {
                    info += &format!(" | Размер архива: {:.2} MB", meta.len() as f64 / 1024.0 / 1024.0);
                }
            }
            (true, info)
        }
        "exit" => (true, "Выход".to_string()),
        _ => (false, "Неизвестная команда".to_string()),
    };
    let _ = tx.send(LogEvent::Done(result.0, result.1));
}

// ==================== TUI ====================

#[derive(Clone)]
struct CommandItem {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    icon: &'static str,
}

const COMMANDS: &[CommandItem] = &[
    CommandItem { id: "download_updates", name: "Скачать обновления системы", description: "Скачивает все обновления в school_packages и создаёт архив. Нужен интернет.", icon: "⬇" },
    CommandItem { id: "install_updates", name: "Установить обновления", description: "Распаковывает архив и ставит обновления офлайн.", icon: "⬆" },
    CommandItem { id: "download_packages", name: "Скачать пакеты (task-koge)", description: "Скачивает task-koge + зависимости + офис. Создаёт архив.", icon: "📦" },
    CommandItem { id: "install_packages", name: "Установить пакеты", description: "Распаковывает архив и ставит весь софт офлайн.", icon: "🚀" },
    CommandItem { id: "detect_os", name: "Информация о системе", description: "ОС, архитектура, размер папки и архива.", icon: "ℹ" },
    CommandItem { id: "exit", name: "Выход", description: "Завершить программу.", icon: "✕" },
];

struct App {
    selected: usize,
    list_state: ListState,
    status: String,
    is_running: bool,
    os_type: String,
    arch: String,
    download_dir: String,
    archive_name: String,
    progress: f64,
    progress_label: String,
    log_lines: Vec<String>,
    log_rx: Option<mpsc::Receiver<LogEvent>>,
}

impl App {
    fn new() -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        App {
            selected: 0,
            list_state,
            status: "Готов к работе. Выберите команду (↑↓, Enter).".to_string(),
            is_running: false,
            os_type: detect_os(),
            arch: detect_arch(),
            download_dir: "school_packages".to_string(),
            archive_name: "school_packages.tar.gz".to_string(),
            progress: 0.0,
            progress_label: String::new(),
            log_lines: Vec::new(),
            log_rx: None,
        }
    }

    fn next(&mut self) {
        if self.is_running { return; }
        self.selected = if self.selected >= COMMANDS.len() - 1 { 0 } else { self.selected + 1 };
        self.list_state.select(Some(self.selected));
    }

    fn previous(&mut self) {
        if self.is_running { return; }
        self.selected = if self.selected == 0 { COMMANDS.len() - 1 } else { self.selected - 1 };
        self.list_state.select(Some(self.selected));
    }

    fn execute_selected(&mut self) {
        if self.is_running { return; }
        let cmd = COMMANDS[self.selected].clone();
        self.is_running = true;
        self.progress = 0.0;
        self.progress_label = format!("Выполняю: {}...", cmd.name);
        self.status = format!("▶ {}", cmd.name);
        self.log_lines.clear();

        let (tx, rx) = mpsc::channel::<LogEvent>();
        let os_type = self.os_type.clone();
        let download_dir = self.download_dir.clone();
        let archive_name = self.archive_name.clone();

        thread::spawn(move || {
            run_command(cmd.id, os_type, download_dir, archive_name, &tx);
        });

        self.log_rx = Some(rx);
    }

    /// Вызывается из главного цикла: неблокирующе читает канал событий
    fn poll_events(&mut self) {
        let mut rx = self.log_rx.take();
        if let Some(receiver) = &rx {
            while let Ok(ev) = receiver.try_recv() {
                match ev {
                    LogEvent::Line(s) => {
                        self.log_lines.push(s);
                        if self.log_lines.len() > 400 {
                            self.log_lines.remove(0);
                        }
                    }
                    LogEvent::Progress(p, label) => {
                        self.progress = self.progress.max(p);
                        self.progress_label = label;
                    }
                    LogEvent::Done(ok, msg) => {
                        self.progress = 1.0;
                        self.progress_label = String::new();
                        self.status = if ok { format!("✓ {}", msg) } else { format!("✗ {}", msg) };
                        self.is_running = false;
                        rx = None;
                        break;
                    }
                }
            }
        }
        self.log_rx = rx;
    }
}

fn ui(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
            Constraint::Length(3),
        ])
        .split(f.area());

    // Шапка
    let header = Paragraph::new(vec![Line::from(vec![
        Span::styled(" ШКОЛЬНЫЙ УСТАНОВЩИК ПО ", Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(format!("ОС: {} | Арх: {}", app.os_type.to_uppercase(), app.arch), Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
    ])])
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)));
    f.render_widget(header, chunks[0]);

    // Две колонки
    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(chunks[1]);

    // Список команд
    let items: Vec<ListItem> = COMMANDS
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            let style = if i == app.selected {
                Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else if app.is_running {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::White)
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {} ", cmd.icon), style),
                Span::styled(cmd.name, style),
            ]))
        })
        .collect();

    let commands_list = List::new(items)
        .block(Block::default().title(" КОМАНДЫ (↑↓, Enter, q) ").borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)))
        .highlight_symbol("▸ ");
    f.render_stateful_widget(commands_list, main_chunks[0], &mut app.list_state.clone());

    // Правая колонка: описание сверху + лог снизу
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(0)])
        .split(main_chunks[1]);

    let selected_cmd = &COMMANDS[app.selected];
    let desc = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("Команда: ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(selected_cmd.name, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(""),
        Line::from(selected_cmd.description),
    ])
    .block(Block::default().title(" ОПИСАНИЕ ").borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)))
    .wrap(Wrap { trim: false });
    f.render_widget(desc, right_chunks[0]);

    // Лог
    let log_widget = Paragraph::new(app.log_lines.clone())
        .block(Block::default().title(" ЛОГ ").borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)))
        .wrap(Wrap { trim: false });
    // автоскролл: показываем последние строки
    let height = right_chunks[1].height.saturating_sub(2) as usize;
    let start = app.log_lines.len().saturating_sub(height);
    f.render_widget(Paragraph::new(app.log_lines[start..].to_vec())
        .block(Block::default().title(" ЛОГ ").borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)))
        .wrap(Wrap { trim: false }), right_chunks[1]);
    let _ = log_widget;

    // Прогресс-бар
    let progress_width = (app.progress * 100.0) as u16;
    let filled = "█".repeat(progress_width as usize);
    let empty = "░".repeat((100 - progress_width) as usize);
    let pct_color = if app.progress >= 1.0 { Color::Green } else { Color::Yellow };
    let progress_bar = Paragraph::new(vec![Line::from(vec![
        Span::styled(" Прогресс: ", Style::default().fg(Color::Cyan)),
        Span::styled(format!("[{}{}]", filled, empty), Style::default().fg(pct_color)),
        Span::styled(format!(" {:.0}%", app.progress * 100.0), Style::default().fg(pct_color).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {}", app.progress_label), Style::default().fg(Color::White)),
    ])])
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)));
    f.render_widget(progress_bar, chunks[2]);

    // Статус
    let status_color = if app.status.starts_with("✓") {
        Color::Green
    } else if app.status.starts_with("✗") {
        Color::Red
    } else if app.status.starts_with("▶") {
        Color::Yellow
    } else {
        Color::White
    };
    let status_widget = Paragraph::new(vec![Line::from(vec![
        Span::styled(" Статус: ", Style::default().fg(Color::Cyan)),
        Span::styled(app.status.as_str(), Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
    ])])
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)));
    f.render_widget(status_widget, chunks[3]);
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();

    // CLI-режим (обратная совместимость)
    if args.len() > 1 {
        let mode = args[1].trim_start_matches("--");
        let os_type = detect_os();
        let download_dir = "school_packages".to_string();
        let archive_name = "school_packages.tar.gz".to_string();

        let (tx, rx) = mpsc::channel::<LogEvent>();
        let cmd_id = match mode {
            "download-updates" => "download_updates",
            "install-updates" => "install_updates",
            "download-packages" => "download_packages",
            "install-packages" => "install_packages",
            _ => { println!("Неизвестная команда: {}", mode); return Ok(()); }
        };
        let (os_c, dd_c, an_c) = (os_type.clone(), download_dir.clone(), archive_name.clone());
        thread::spawn(move || run_command(cmd_id, os_c, dd_c, an_c, &tx));

        loop {
            match rx.recv() {
                Ok(LogEvent::Line(s)) => println!("{}", s),
                Ok(LogEvent::Progress(p, l)) => println!("[{:>3.0}%] {}", p * 100.0, l),
                Ok(LogEvent::Done(ok, msg)) => {
                    println!("{}", msg);
                    if !ok { std::process::exit(1); }
                    break;
                }
                Err(_) => break,
            }
        }
        return Ok(());
    }

    // TUI-режим
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();

    loop {
        terminal.draw(|f| ui(f, &app))?;
        app.poll_events();

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                    break;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Up | KeyCode::Char('k') => app.previous(),
                    KeyCode::Down | KeyCode::Char('j') => app.next(),
                    KeyCode::Enter => {
                        if COMMANDS[app.selected].id == "exit" {
                            break;
                        }
                        app.execute_selected();
                    }
                    _ => {}
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    Ok(())
}
