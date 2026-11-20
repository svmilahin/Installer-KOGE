use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

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

fn compress_folder(folder: &str, archive: &str) -> (bool, String) {
    let _ = fs::create_dir_all(folder);
    let size_before = get_folder_size(folder);
    let _ = fs::remove_file(archive);

    let status = Command::new("tar")
        .arg("-czf")
        .arg(archive)
        .arg(folder)
        .status();

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

fn decompress_archive(archive: &str, folder: &str) -> (bool, String) {
    if !Path::new(archive).exists() {
        return (false, format!("Архив не найден: {}", archive));
    }
    let _ = fs::create_dir_all(folder);
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(".")
        .status();

    match status {
        Ok(s) if s.success() => (true, format!("Распаковано в {}", folder)),
        Ok(s) => (false, format!("Ошибка распаковки. Код: {:?}", s.code())),
        Err(e) => (false, format!("Не удалось запустить tar: {}", e)),
    }
}

fn download_updates(download_dir: &str, os_type: &str) -> (bool, String) {
    let _ = fs::create_dir_all(download_dir);
    let pm = if os_type == "rosa" { "dnf" } else { "apt-get" };

    if os_type == "rosa" {
        let status = Command::new(pm)
            .arg("upgrade")
            .arg("--downloadonly")
            .arg(format!("--destdir={}", download_dir))
            .arg("-y")
            .status();
        match status {
            Ok(s) if s.success() => (true, "Обновления скачаны".to_string()),
            Ok(s) => (false, format!("Ошибка скачивания. Код: {:?}", s.code())),
            Err(e) => (false, format!("Не удалось запустить {}: {}", pm, e)),
        }
    } else {
        // Alt Linux
        let _ = Command::new(pm).arg("update").status();
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

        let mut all_ok = true;
        for pkg in &packages {
            let status = Command::new(pm)
                .arg("install")
                .arg("--download-only")
                .arg("-y")
                .arg(format!("-o Dir::Cache::Archives={}", download_dir))
                .arg(pkg)
                .status();
            if let Ok(s) = status {
                if !s.success() {
                    all_ok = false;
                }
            } else {
                all_ok = false;
            }
        }
        if all_ok {
            (true, format!("Скачано пакетов: {}", packages.len()))
        } else {
            (false, "Некоторые пакеты не удалось скачать".to_string())
        }
    }
}

fn download_packages(packages: &[&str], download_dir: &str, os_type: &str) -> (bool, String) {
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
        match cmd.status() {
            Ok(s) if s.success() => (true, "Пакеты скачаны".to_string()),
            Ok(s) => (false, format!("Ошибка. Код: {:?}", s.code())),
            Err(e) => (false, format!("Не удалось запустить {}: {}", pm, e)),
        }
    } else {
        let mut all_ok = true;
        for pkg in packages {
            let status = Command::new(pm)
                .arg("install")
                .arg("--download-only")
                .arg("-y")
                .arg(format!("-o Dir::Cache::Archives={}", download_dir))
                .arg(pkg)
                .status();
            if let Ok(s) = status {
                if !s.success() {
                    all_ok = false;
                }
            } else {
                all_ok = false;
            }
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
        if arch == "noarch" {
            return true;
        }
        if arch == system_arch {
            return true;
        }
        if system_arch == "x86_64" && arch == "i686" {
            return false;
        }
        return false;
    }
    true
}

fn install_from_folder(download_dir: &str, os_type: &str) -> (bool, String) {
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

    if os_type == "rosa" {
        let mut cmd = Command::new(pm);
        cmd.arg("install")
            .arg("-y")
            .arg("--allowerasing")
            .arg("--nobest");
        for file in &files {
            cmd.arg(file);
        }
        match cmd.status() {
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
        match cmd.status() {
            Ok(s) if s.success() => {
                let _ = Command::new(pm).arg("install").arg("-f").arg("-y").status();
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

// ==================== TUI ====================

#[derive(Clone)]
struct CommandItem {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    icon: &'static str,
}

const COMMANDS: &[CommandItem] = &[
    CommandItem {
        id: "download_updates",
        name: "Скачать обновления системы",
        description: "Скачивает все доступные обновления системы в папку school_packages и создаёт архив school_packages.tar.gz. Работает на машине с интернетом.",
        icon: "⬇",
    },
    CommandItem {
        id: "install_updates",
        name: "Установить обновления",
        description: "Распаковывает архив school_packages.tar.gz (если есть) и устанавливает все обновления из папки. Работает без интернета.",
        icon: "⬆",
    },
    CommandItem {
        id: "download_packages",
        name: "Скачать пакеты (task-koge)",
        description: "Скачивает метапакет task-koge и все зависимости для экзамена по информатике + офисные программы. Создаёт архив.",
        icon: "📦",
    },
    CommandItem {
        id: "install_packages",
        name: "Установить пакеты",
        description: "Распаковывает архив и устанавливает все пакеты (task-koge, офис, редакторы). Работает без интернета.",
        icon: "🚀",
    },
    CommandItem {
        id: "detect_os",
        name: "Информация о системе",
        description: "Показывает определнную ОС, архитектуру и статус папок/архивов.",
        icon: "ℹ",
    },
    CommandItem {
        id: "exit",
        name: "Выход",
        description: "Завершить программу.",
        icon: "✕",
    },
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
}

impl App {
    fn new() -> Self {
        let os_type = detect_os();
        let arch = detect_arch();
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        App {
            selected: 0,
            list_state,
            status: "Готов к работе. Выберите команду.".to_string(),
            is_running: false,
            os_type,
            arch,
            download_dir: "school_packages".to_string(),
            archive_name: "school_packages.tar.gz".to_string(),
            progress: 0.0,
            progress_label: String::new(),
        }
    }

    fn next(&mut self) {
        let i = if self.selected >= COMMANDS.len() - 1 {
            0
        } else {
            self.selected + 1
        };
        self.selected = i;
        self.list_state.select(Some(i));
    }

    fn previous(&mut self) {
        let i = if self.selected == 0 {
            COMMANDS.len() - 1
        } else {
            self.selected - 1
        };
        self.selected = i;
        self.list_state.select(Some(i));
    }

    fn execute_selected(&mut self) {
        let cmd = &COMMANDS[self.selected];
        self.is_running = true;
        self.progress = 0.0;
        self.progress_label = format!("Выполняю: {}...", cmd.name);
        self.status = format!("▶ {}", cmd.name);

        let (tx, rx) = mpsc::channel();
        let os_type = self.os_type.clone();
        let download_dir = self.download_dir.clone();
        let archive_name = self.archive_name.clone();
        let cmd_id = cmd.id.to_string();

        thread::spawn(move || {
            let result = match cmd_id.as_str() {
                "download_updates" => {
                    let (ok, msg) = download_updates(&download_dir, &os_type);
                    if ok {
                        compress_folder(&download_dir, &archive_name)
                    } else {
                        (false, msg)
                    }
                }
                "install_updates" => {
                    if Path::new(&archive_name).exists() {
                        let (ok, msg) = decompress_archive(&archive_name, &download_dir);
                        if !ok {
                            return (false, msg);
                        }
                    } else if !Path::new(&download_dir).exists() {
                        return (false, "Не найден ни архив, ни папка".to_string());
                    }
                    install_from_folder(&download_dir, &os_type)
                }
                "download_packages" => {
                    let packages = get_packages();
                    let (ok, msg) = download_packages(&packages, &download_dir, &os_type);
                    if ok {
                        compress_folder(&download_dir, &archive_name)
                    } else {
                        (false, msg)
                    }
                }
                "install_packages" => {
                    if Path::new(&archive_name).exists() {
                        let (ok, msg) = decompress_archive(&archive_name, &download_dir);
                        if !ok {
                            return (false, msg);
                        }
                    } else if !Path::new(&download_dir).exists() {
                        return (false, "Не найден ни архив, ни папка".to_string());
                    }
                    install_from_folder(&download_dir, &os_type)
                }
                "detect_os" => {
                    let mut info = format!("ОС: {}\nАрхитектура: {}\n", os_type, detect_arch());
                    info += &format!("Папка {}: {}\n", download_dir, Path::new(&download_dir).exists());
                    info += &format!("Архив {}: {}\n", archive_name, Path::new(&archive_name).exists());
                    if Path::new(&download_dir).exists() {
                        let size = get_folder_size(&download_dir);
                        info += &format!("Размер папки: {:.2} MB\n", size as f64 / 1024.0 / 1024.0);
                    }
                    if Path::new(&archive_name).exists() {
                        if let Ok(meta) = fs::metadata(&archive_name) {
                            info += &format!("Размер архива: {:.2} MB\n", meta.len() as f64 / 1024.0 / 1024.0);
                        }
                    }
                    (true, info)
                }
                "exit" => (true, "Выход".to_string()),
                _ => (false, "Неизвестная команда".to_string()),
            };
            let _ = tx.send(result);
        });

        // Ждём результат с обновлением прогресса
        loop {
            match rx.try_recv() {
                Ok((ok, msg)) => {
                    self.progress = 1.0;
                    self.progress_label = String::new();
                    if ok {
                        self.status = format!("✓ {}", msg);
                    } else {
                        self.status = format!("✗ {}", msg);
                    }
                    self.is_running = false;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // Имитация прогресса пока ждём
                    self.progress = (self.progress + 0.02).min(0.95);
                    thread::sleep(Duration::from_millis(200));
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.status = "Ошибка связи с потоком".to_string();
                    self.is_running = false;
                    break;
                }
            }
        }
    }
}

fn ui(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // Шапка
            Constraint::Min(0),     // Основной контент (2 колонки)
            Constraint::Length(3),  // Прогресс-бар
            Constraint::Length(3),  // Статус
        ])
        .split(f.area());

    // === ШАПКА ===
    let header = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(" ШКОЛЬНЫЙ УСТАНОВЩИК ПО ", Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(format!("ОС: {} | Арх: {}", app.os_type.to_uppercase(), app.arch), Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        ]),
    ])
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)));
    f.render_widget(header, chunks[0]);

    // === ОСНОВНОЙ КОНТЕНТ (2 колонки) ===
    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[1]);

    // Левая панель — список команд
    let items: Vec<ListItem> = COMMANDS
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            let style = if i == app.selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
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
        .block(
            Block::default()
                .title(" КОМАНДЫ (↑↓ навигация, Enter — выполнить, q — выход) ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .highlight_style(Style::default().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD))
        .highlight_symbol("▸ ");
    f.render_stateful_widget(commands_list, main_chunks[0], &mut app.list_state.clone());

    // Правая панель — описание
    let selected_cmd = &COMMANDS[app.selected];
    let description_text = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("Команда: ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(selected_cmd.name, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("Описание:\n", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(selected_cmd.description),
        Line::from(""),
        Line::from(vec![
            Span::styled("Статус: ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(&app.status),
    ];

    let description = Paragraph::new(description_text)
        .block(
            Block::default()
                .title(" ОПИСАНИЕ ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(description, main_chunks[1]);

    // === ПРОГРЕСС-БАР ===
    let progress_width = (app.progress * 100.0) as u16;
    let progress_bar = if app.is_running {
        let filled = "█".repeat(progress_width as usize);
        let empty = "░".repeat((100 - progress_width) as usize);
        Paragraph::new(vec![Line::from(vec![
            Span::styled(" Прогресс: ", Style::default().fg(Color::Cyan)),
            Span::styled(format!("[{}{}]", filled, empty), Style::default().fg(Color::Green)),
            Span::styled(format!(" {:.0}%", app.progress * 100.0), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {}", app.progress_label), Style::default().fg(Color::White)),
        ])])
    } else {
        Paragraph::new(vec![Line::from(vec![
            Span::styled(" Прогресс: ", Style::default().fg(Color::DarkGray)),
            Span::styled("[████████████████████████████████████████████████████████████████████████████████]", Style::default().fg(Color::DarkGray)),
            Span::styled(" 100%  Готов", Style::default().fg(Color::DarkGray)),
        ])])
    }
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)));
    f.render_widget(progress_bar, chunks[2]);

    // === СТАТУС ===
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
        Span::styled(&app.status, Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
    ])])
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)));
    f.render_widget(status_widget, chunks[3]);
}

fn main() -> io::Result<()> {
    // Проверяем аргументы командной строки (для обратной совместимости)
    let args: Vec<String> = env::args().collect();
    if args.len() > 1 {
        // Старый режим работы через аргументы
        let mode = args[1].trim_start_matches("--");
        let os_type = detect_os();
        let download_dir = "school_packages";
        let archive_name = "school_packages.tar.gz";

        match mode {
            "download-updates" => {
                let (ok, msg) = download_updates(download_dir, &os_type);
                if ok { compress_folder(download_dir, archive_name); }
                println!("{}", msg);
            }
            "install-updates" => {
                if Path::new(archive_name).exists() { decompress_archive(archive_name, download_dir); }
                let (ok, msg) = install_from_folder(download_dir, &os_type);
                println!("{}", msg);
                if !ok { std::process::exit(1); }
            }
            "download-packages" => {
                let packages = get_packages();
                let (ok, msg) = download_packages(&packages, download_dir, &os_type);
                if ok { compress_folder(download_dir, archive_name); }
                println!("{}", msg);
            }
            "install-packages" => {
                if Path::new(archive_name).exists() { decompress_archive(archive_name, download_dir); }
                let (ok, msg) = install_from_folder(download_dir, &os_type);
                println!("{}", msg);
                if !ok { std::process::exit(1); }
            }
            _ => println!("Неизвестная команда: {}", mode),
        }
        return Ok(());
    }

    // TUI режим
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();

    loop {
        terminal.draw(|f| ui(f, &app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                    break;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Up | KeyCode::Char('k') => {
                        if !app.is_running { app.previous(); }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if !app.is_running { app.next(); }
                    }
                    KeyCode::Enter => {
                        if !app.is_running {
                            if COMMANDS[app.selected].id == "exit" {
                                break;
                            }
                            app.execute_selected();
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}
