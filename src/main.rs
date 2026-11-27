use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

// ==================== НАСТРОЙКИ ====================

const SETTINGS_FILE: &str = "school-installer.conf";
const LOG_FILE: &str = "school-installer.log";
const CHECKSUM_FILE: &str = "checksums.sha256";
const MAX_LOG_SIZE_BYTES: u64 = 5 * 1024 * 1024;
const CERT_FILE_NAME: &str = "ca-root.crt";
const PROXY_TEST_URL: &str = "http://ya.ru";
const PROXY_TEST_TIMEOUT_SECS: u64 = 5;
const VERSION: &str = "1.0.0";

const MAX_PARALLEL_DOWNLOADS: usize = 4;

const GITHUB_REPO_OWNER: &str = "your-org";
const GITHUB_REPO_NAME: &str = "school-installer";
const GITHUB_API_URL: &str = "https://api.github.com";

const DEFAULT_PROXY_URL: &str = "http://10.0.54.52:3128";
const DEFAULT_CERT_DIR: &str = "cer";
const DEFAULT_CRYPTO_DIR: &str = "cryptopro_gia";
const DEFAULT_SCHOOL_PACKAGES_DIR: &str = "school_packages";
const DEFAULT_UPDATES_DIR: &str = "updates";

const MIN_FREE_SPACE_UPDATES_MB: u64 = 1024;
const MIN_FREE_SPACE_PACKAGES_MB: u64 = 2048;
const MIN_FREE_SPACE_CRYPTO_MB: u64 = 500;
const MIN_FREE_SPACE_CRITICAL_MB: u64 = 200;

// ==================== ЛОГИРОВАНИЕ В ФАЙЛ ====================

fn log_file_path() -> String {
    if let Ok(exe) = env::current_exe() {
        if let Some(parent) = exe.parent() {
            return parent.join(LOG_FILE).to_string_lossy().to_string();
        }
    }
    LOG_FILE.to_string()
}

fn get_report_dir() -> String {
    if let Ok(exe) = env::current_exe() {
        if let Some(parent) = exe.parent() {
            return parent.to_string_lossy().to_string();
        }
    }
    ".".to_string()
}

fn rotate_log_if_needed() {
    let path = log_file_path();
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > MAX_LOG_SIZE_BYTES {
            let timestamp = chrono_now_approx();
            let rotated = format!("{}.{}", path, timestamp);
            let _ = fs::rename(&path, &rotated);
        }
    }
}

fn chrono_now_approx() -> String {
    if let Ok(out) = Command::new("date").arg("+%Y%m%d-%H%M%S").output() {
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    } else {
        "unknown".to_string()
    }
}

fn log_to_file(msg: &str) {
    rotate_log_if_needed();
    let path = log_file_path();
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let timestamp = chrono_now_approx();
        let _ = writeln!(file, "[{}] {}", timestamp, msg);
    }
}

fn log_session_start() {
    log_to_file("=== Новая сессия school-installer ===");
    log_to_file(&format!("Версия: {} | ОС: МОС ОС | Арх: {}", VERSION, detect_arch()));
    log_to_file(&format!("Прокси: {}", get_proxy_url()));
}

// ==================== ПРОВЕРКА ПРАВ ROOT ====================

fn check_root() -> bool {
    let output = Command::new("id").arg("-u").stdout(Stdio::piped()).stderr(Stdio::null()).output();
    match output {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim() == "0",
        Err(_) => false,
    }
}

fn require_root_and_exit() -> ! {
    eprintln!();
    eprintln!("╔══════════════════════════════════════════════════════════════╗");
    eprintln!("║  ❌  ОШИБКА: Программа должна быть запущена от root          ║");
    eprintln!("║      sudo ./school-installer                                 ║");
    eprintln!("╚══════════════════════════════════════════════════════════════╝");
    eprintln!();
    std::process::exit(1);
}

// ==================== ПРОВЕРКА СВОБОДНОГО МЕСТА ====================

fn get_free_space_mb(path: &str) -> Option<u64> {
    let output = Command::new("df").arg("-BM").arg(path).stdout(Stdio::piped()).stderr(Stdio::null()).output();
    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if let Some(line) = stdout.lines().nth(1) {
                let fields: Vec<&str> = line.split_whitespace().collect();
                if fields.len() >= 4 {
                    let size_str = fields[3].trim_end_matches('M');
                    if let Ok(size) = size_str.parse::<u64>() { return Some(size); }
                }
            }
            None
        }
        Err(_) => None,
    }
}

fn format_size_mb(mb: u64) -> String {
    if mb >= 1024 { format!("{:.1} ГБ", mb as f64 / 1024.0) } else { format!("{} МБ", mb) }
}

fn format_size_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB { format!("{:.1} ГБ", bytes as f64 / GB as f64) }
    else if bytes >= MB { format!("{:.0} МБ", bytes as f64 / MB as f64) }
    else if bytes >= KB { format!("{:.0} КБ", bytes as f64 / KB as f64) }
    else { format!("{} Б", bytes) }
}

fn check_free_space(path: &str, min_mb: u64, operation_name: impl AsRef<str>) -> (bool, String) {
    let op_name = operation_name.as_ref();
    let check_path = if Path::new(path).exists() {
        path.to_string()
    } else {
        let mut p = Path::new(path);
        while !p.exists() && p.parent().is_some() { p = p.parent().unwrap(); }
        p.to_string_lossy().to_string()
    };
    match get_free_space_mb(&check_path) {
        Some(free_mb) => {
            if free_mb < MIN_FREE_SPACE_CRITICAL_MB {
                return (false, format!("❌ КРИТИЧЕСКИ МАЛО МЕСТА: свободно {} на {}. {} требует минимум {}.", format_size_mb(free_mb), check_path, op_name, format_size_mb(min_mb)));
            }
            if free_mb < min_mb {
                return (false, format!("⚠ Недостаточно места: свободно {} на {}. {} требует минимум {}.", format_size_mb(free_mb), check_path, op_name, format_size_mb(min_mb)));
            }
            (true, format!("✅ Свободно {} на {} (требуется {})", format_size_mb(free_mb), check_path, format_size_mb(min_mb)))
        }
        None => (true, format!("⚠ Не удалось определить свободное место на {}.", check_path)),
    }
}

fn estimate_dnf_download_size(args: &[&str]) -> Option<u64> {
    let mut cmd = Command::new("dnf");
    for arg in args { cmd.arg(arg); }
    cmd.arg("--assumeno").stdout(Stdio::piped()).stderr(Stdio::piped());
    let output = cmd.output().ok()?;
    let combined = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    for line in combined.lines() {
        let lower = line.to_lowercase();
        if lower.contains("download size") || lower.contains("объем загрузки") || lower.contains("total size") {
            if let Some(pos) = line.rfind('M') {
                let before = &line[..pos];
                let parts: Vec<&str> = before.split_whitespace().collect();
                if let Some(last) = parts.last() {
                    if let Ok(size) = last.parse::<u64>() { return Some(size); }
                }
            }
        }
    }
    None
}

// ==================== УПРАВЛЕНИЕ НАСТРОЙКАМИ ====================

fn load_settings() -> HashMap<String, String> {
    let mut settings = HashMap::new();
    if let Ok(content) = fs::read_to_string(SETTINGS_FILE) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            if let Some((key, value)) = line.split_once('=') {
                settings.insert(key.trim().to_string(), value.trim().to_string());
            }
        }
    }
    settings
}

fn save_setting(key: &str, value: &str) -> io::Result<()> {
    let mut settings = load_settings();
    settings.insert(key.to_string(), value.to_string());
    let mut content = String::from("# Настройки школьного установщика (МОС ОС)\n");
    content += &format!("PROXY_URL={}\n", settings.get("PROXY_URL").map(|s| s.as_str()).unwrap_or(DEFAULT_PROXY_URL));
    content += &format!("CERT_DIR={}\n", settings.get("CERT_DIR").map(|s| s.as_str()).unwrap_or(DEFAULT_CERT_DIR));
    content += &format!("CRYPTO_DIR={}\n", settings.get("CRYPTO_DIR").map(|s| s.as_str()).unwrap_or(DEFAULT_CRYPTO_DIR));
    content += &format!("SCHOOL_PACKAGES_DIR={}\n", settings.get("SCHOOL_PACKAGES_DIR").map(|s| s.as_str()).unwrap_or(DEFAULT_SCHOOL_PACKAGES_DIR));
    content += &format!("UPDATES_DIR={}\n", settings.get("UPDATES_DIR").map(|s| s.as_str()).unwrap_or(DEFAULT_UPDATES_DIR));
    fs::write(SETTINGS_FILE, content)
}

fn get_setting(key: &str, default: &str) -> String {
    let env_key = format!("SCHOOL_{}", key);
    if let Ok(val) = env::var(&env_key) { if !val.is_empty() { return val; } }
    let settings = load_settings();
    if let Some(val) = settings.get(key) { return val.clone(); }
    default.to_string()
}

fn resolve_dir(name: &str) -> String {
    if Path::new(name).exists() { return name.to_string(); }
    if let Ok(exe) = env::current_exe() {
        if let Some(parent) = exe.parent() {
            let candidate = parent.join(name);
            if candidate.exists() { return candidate.to_string_lossy().to_string(); }
        }
    }
    name.to_string()
}

fn get_proxy_url() -> String { get_setting("PROXY_URL", DEFAULT_PROXY_URL) }
fn get_cert_dir() -> String { resolve_dir(&get_setting("CERT_DIR", DEFAULT_CERT_DIR)) }
fn get_crypto_dir() -> String { resolve_dir(&get_setting("CRYPTO_DIR", DEFAULT_CRYPTO_DIR)) }
fn get_school_packages_dir() -> String { resolve_dir(&get_setting("SCHOOL_PACKAGES_DIR", DEFAULT_SCHOOL_PACKAGES_DIR)) }
fn get_updates_dir() -> String { resolve_dir(&get_setting("UPDATES_DIR", DEFAULT_UPDATES_DIR)) }

// ==================== СОБЫТИЯ ДЛЯ TUI ====================

#[derive(Clone)]
enum LogEvent {
    Line(String),
    Progress(f64, String),
    Done(bool, String),
}

// ==================== УТИЛИТЫ ====================

fn clean_line(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&c2) = chars.peek() {
                    chars.next();
                    if c2.is_ascii_alphabetic() { break; }
                }
            }
        } else if c == '\r' {
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

fn parse_dnf_progress(line: &str) -> Option<(u32, u32, String)> {
    let cleaned = clean_line(line);
    if let Some(start) = cleaned.find('(') {
        if let Some(slash) = cleaned[start..].find('/') {
            if let Some(end) = cleaned[start..].find(')') {
                let n_str = &cleaned[start+1..start+slash];
                let m_str = &cleaned[start+slash+1..start+end];
                if let (Ok(n), Ok(m)) = (n_str.trim().parse::<u32>(), m_str.trim().parse::<u32>()) {
                    if m > 0 && n <= m {
                        let rest = &cleaned[start+end+1..];
                        let pkg_name = rest.trim().split_whitespace().next().unwrap_or("").to_string();
                        return Some((n, m, pkg_name));
                    }
                }
            }
        }
    }
    None
}

fn parse_progress(s: &str) -> Option<(u32, u32)> {
    let mut best: Option<(u32, u32)> = None;
    for chunk in s.split('[').skip(1) {
        if let Some(end) = chunk.find(']') {
            let inner = &chunk[..end];
            if let Some(slash) = inner.find('/') {
                if let (Ok(a), Ok(b)) = (inner[..slash].trim().parse::<u32>(), inner[slash + 1..].trim().parse::<u32>()) {
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

fn colorize_log_line(text: &str) -> Line<'static> {
    let t = text.trim();
    let color = if t.is_empty() {
        Color::White
    } else if t.starts_with("✅") || t.starts_with("[OK]") || t.contains("успешно") || t.contains("скачан") || t.contains("готово") || t.contains("Обновлено") || t.contains("Найден") || t.contains("Отчёт") || t.contains("сохранён") || t.contains("совпадает") || t.contains("целостны") {
        Color::Green
    } else if t.starts_with("❌") || t.starts_with("[X]") || t.contains("Ошибка") || t.contains("не удалось") || t.contains("не найден") || t.contains("НЕ СОВПАДАЕТ") || t.contains("повреждён") {
        Color::Red
    } else if t.starts_with("⚠") || t.starts_with("[!]") || t.contains("ВНИМАНИЕ") || t.contains("пропускаем") || t.contains("устаревш") {
        Color::Yellow
    } else if t.starts_with("📊") || t.starts_with("📥") || t.starts_with("💾") || t.starts_with("🔒") || t.starts_with("🌐") || t.starts_with("🔐") || t.starts_with("📦") || t.starts_with("🚀") || t.starts_with("🔑") || t.starts_with("🔍") || t.starts_with("⚙") || t.starts_with("📄") || t.starts_with("🔗") || t.starts_with("🧪") || t.starts_with("🔁") {
        Color::Cyan
    } else if t.starts_with("===") {
        Color::Magenta
    } else if t.starts_with("   ") && (t.contains("->") || t.contains("✅")) {
        if t.contains("✅") { Color::Green } else { Color::Gray }
    } else {
        Color::White
    };
    Line::from(Span::styled(text.to_string(), Style::default().fg(color)))
}

fn run_process(mut cmd: Command, tx: &mpsc::Sender<LogEvent>) -> io::Result<ExitStatus> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;

    let stderr = child.stderr.take().unwrap();
    let tx_err = tx.clone();
    let err_handle = thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            if let Ok(l) = line {
                let cleaned = clean_line(&l);
                if cleaned.is_empty() { continue; }
                if let Some((n, m, pkg)) = parse_dnf_progress(&l) {
                    let pct = n as f64 / m as f64;
                    let msg = if pkg.is_empty() { format!("Скачивание: {} / {}", n, m) } else { format!("Скачивание: {} / {} — {}", n, m, pkg) };
                    let _ = tx_err.send(LogEvent::Progress(pct, msg));
                } else {
                    let _ = tx_err.send(LogEvent::Line(cleaned.clone()));
                }
            }
        }
    });

    let stdout = child.stdout.take().unwrap();
    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        if let Ok(l) = line {
            let cleaned = clean_line(&l);
            if cleaned.is_empty() { continue; }
            let _ = tx.send(LogEvent::Line(cleaned));
        }
    }

    let status = child.wait()?;
    let _ = err_handle.join();
    Ok(status)
}

fn command_exists(name: &str) -> bool {
    Command::new("sh").arg("-c").arg(format!("command -v {}", name)).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

// ==================== ОПРЕДЕЛЕНИЕ СИСТЕМЫ ====================

fn detect_arch() -> String {
    Command::new("uname").arg("-m").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_lowercase().to_string()).unwrap_or_else(|_| "x86_64".to_string())
}

fn get_hostname() -> String {
    Command::new("hostname").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_else(|_| "unknown".to_string())
}

fn real_home_and_user() -> (String, Option<String>) {
    if let Ok(user) = env::var("SUDO_USER") {
        if !user.is_empty() {
            if let Ok(out) = Command::new("getent").arg("passwd").arg(&user).output() {
                let s = String::from_utf8_lossy(&out.stdout);
                if let Some(line) = s.lines().next() {
                    let parts: Vec<&str> = line.split(':').collect();
                    if parts.len() >= 6 && !parts[5].is_empty() { return (parts[5].to_string(), Some(user)); }
                }
            }
            return (format!("/home/{}", user), Some(user));
        }
    }
    (env::var("HOME").unwrap_or_else(|_| "/root".to_string()), None)
}

fn get_folder_size(path: &str) -> u64 {
    let mut total: u64 = 0;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.is_file() { if let Ok(meta) = fs::metadata(&p) { total += meta.len(); } }
            else if p.is_dir() { total += get_folder_size(&p.to_string_lossy()); }
        }
    }
    total
}

fn is_package_for_arch(filename: &str, system_arch: &str) -> bool {
    let path = Path::new(filename);
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    if let Some(arch) = stem.split('.').last() {
        if arch == "noarch" || arch == system_arch { return true; }
        if system_arch == "x86_64" && arch == "i686" { return false; }
        return false;
    }
    true
}

// ==================== АВТОСОЗДАНИЕ ПАПОК ====================

fn ensure_directories() {
    let base_packages = get_school_packages_dir();
    let base_updates = get_updates_dir();
    let crypto_dir = get_crypto_dir();
    let cert_dir = get_cert_dir();

    let dirs: Vec<String> = vec![
        base_packages.clone(),
        base_updates.clone(),
        format!("{}/linux-amd64", crypto_dir),
        crypto_dir.clone(),
        cert_dir,
    ];
    for dir in dirs {
        let path = Path::new(&dir);
        if !path.exists() {
            let _ = fs::create_dir_all(path);
        }
    }
}

fn folder_info_line(prefix: &str, path: &str, system_arch: &str) -> String {
    let exists = Path::new(path).exists();
    if !exists { return format!("{}{} (не существует)", prefix, path); }
    let size_bytes = get_folder_size(path);
    let rpm_count = find_packages_flat(path, "", system_arch).len();
    format!("{}{} ({} .rpm, {})", prefix, path, rpm_count, format_size_bytes(size_bytes))
}

fn print_setup_instructions(tx: &mpsc::Sender<LogEvent>) {
    let base_packages = get_school_packages_dir();
    let base_updates = get_updates_dir();
    let crypto_dir = get_crypto_dir();
    let cert_dir = get_cert_dir();
    let system_arch = detect_arch();

    let _ = tx.send(LogEvent::Line("========================================".to_string()));
    let _ = tx.send(LogEvent::Line("  📋 ИНСТРУКЦИЯ ПО ПОДГОТОВКЕ ФЛЕШКИ".to_string()));
    let _ = tx.send(LogEvent::Line(format!("  school-installer v{}", VERSION)));
    let _ = tx.send(LogEvent::Line("========================================".to_string()));
    let _ = tx.send(LogEvent::Line(String::new()));
    let _ = tx.send(LogEvent::Line(format!("🖥 ОС: МОС ОС (ROSA) | Архитектура: {}", system_arch)));
    let _ = tx.send(LogEvent::Line(String::new()));

    if let Some(free_mb) = get_free_space_mb(&base_packages) {
        let _ = tx.send(LogEvent::Line(format!("💾 Свободно на диске: {}", format_size_mb(free_mb))));
        let _ = tx.send(LogEvent::Line(String::new()));
    }

    let _ = tx.send(LogEvent::Line("📦 1. school_packages/ — пакеты для установки".to_string()));
    let _ = tx.send(LogEvent::Line(folder_info_line("   ├── ", &base_packages, &system_arch)));
    let _ = tx.send(LogEvent::Line(String::new()));

    let _ = tx.send(LogEvent::Line("🔄 2. updates/ — обновления системы".to_string()));
    let _ = tx.send(LogEvent::Line(folder_info_line("   ├── ", &base_updates, &system_arch)));
    let _ = tx.send(LogEvent::Line(String::new()));

    let _ = tx.send(LogEvent::Line("🔐 3. cryptopro_gia/ — КриптоПро + ГИА".to_string()));
    let _ = tx.send(LogEvent::Line(folder_info_line("   ├── ", &crypto_dir, &system_arch)));
    let _ = tx.send(LogEvent::Line(String::new()));

    let _ = tx.send(LogEvent::Line("🔒 4. cer/ — корневые сертификаты".to_string()));
    let _ = tx.send(LogEvent::Line(folder_info_line("   └── ", &cert_dir, &system_arch)));
    let _ = tx.send(LogEvent::Line(String::new()));

    let _ = tx.send(LogEvent::Line("🧪 5. Проверка целостности (SHA256)".to_string()));
    let _ = tx.send(LogEvent::Line("   ├── После скачивания создаётся checksums.sha256".to_string()));
    let _ = tx.send(LogEvent::Line("   ├── Перед установкой проверяются хеши".to_string()));
    let _ = tx.send(LogEvent::Line("   └── Битые файлы помечаются и перескачиваются".to_string()));
    let _ = tx.send(LogEvent::Line(String::new()));

    let _ = tx.send(LogEvent::Line("🔗 6. Автообновление".to_string()));
    let _ = tx.send(LogEvent::Line("   ├── Программа проверяет GitHub Releases".to_string()));
    let _ = tx.send(LogEvent::Line("   ├── Скачивает новую версию при наличии".to_string()));
    let _ = tx.send(LogEvent::Line("   └── Для настройки: GITHUB_REPO_OWNER в коде".to_string()));
    let _ = tx.send(LogEvent::Line(String::new()));

    let _ = tx.send(LogEvent::Line("🚀 7. Параллельное скачивание".to_string()));
    let _ = tx.send(LogEvent::Line(format!("   └── {} потоков для ускорения", MAX_PARALLEL_DOWNLOADS)));
    let _ = tx.send(LogEvent::Line(String::new()));

    let _ = tx.send(LogEvent::Line("========================================".to_string()));
    let _ = tx.send(LogEvent::Line("  📝 БЫСТРЫЙ СТАРТ".to_string()));
    let _ = tx.send(LogEvent::Line("========================================".to_string()));
    let _ = tx.send(LogEvent::Line(String::new()));
    let _ = tx.send(LogEvent::Line("На машине с ИНТЕРНЕТОМ:".to_string()));
    let _ = tx.send(LogEvent::Line("  1. sudo ./school-installer --set-proxy <URL>".to_string()));
    let _ = tx.send(LogEvent::Line("  2. Положите ca-root.crt в папку cer/".to_string()));
    let _ = tx.send(LogEvent::Line("  3. sudo ./school-installer".to_string()));
    let _ = tx.send(LogEvent::Line("     -> ⚙ Автонастройка прокси".to_string()));
    let _ = tx.send(LogEvent::Line("     -> 📦 Скачать пакеты (с зависимостями)".to_string()));
    let _ = tx.send(LogEvent::Line("  4. Скопируйте на флешку:".to_string()));
    let _ = tx.send(LogEvent::Line("     school-installer + school_packages/ + cryptopro_gia/ + cer/".to_string()));
    let _ = tx.send(LogEvent::Line(String::new()));
    let _ = tx.send(LogEvent::Line("В КЛАССЕ (без интернета):".to_string()));
    let _ = tx.send(LogEvent::Line("  1. sudo ./school-installer".to_string()));
    let _ = tx.send(LogEvent::Line("     -> 🚀 Установить пакеты (с проверкой SHA256)".to_string()));
    let _ = tx.send(LogEvent::Line("     -> 🔐 Установить КриптоПро + ГИА".to_string()));
    let _ = tx.send(LogEvent::Line(String::new()));
    let _ = tx.send(LogEvent::Line("В каждой папке есть README.txt с подробностями.".to_string()));
    let _ = tx.send(LogEvent::Line("========================================".to_string()));
}

// ==================== ПРОВЕРКА УСТАНОВКИ ПАКЕТА ====================

fn is_package_installed(package_name: &str) -> bool {
    let check_cmd = format!("rpm -q {} >/dev/null 2>&1", package_name);
    Command::new("sh").arg("-c").arg(&check_cmd).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

// ==================== ПОИСК ПАКЕТОВ ====================

fn find_packages_flat(folder: &str, pattern: &str, system_arch: &str) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(folder) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if !path.is_file() { continue; }
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            if name == "README.txt" || name == CHECKSUM_FILE { continue; }
            if !name.ends_with(".rpm") { continue; }
            if !pattern.is_empty() && !name.starts_with(pattern) { continue; }
            if !is_package_for_arch(&name, system_arch) { continue; }
            result.push(path.to_string_lossy().to_string());
        }
    }
    result
}

fn find_packages_by_substring(folder: &str, substring: &str, system_arch: &str) -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(folder) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if !path.is_file() { continue; }
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            if name == "README.txt" || name == CHECKSUM_FILE { continue; }
            if !name.ends_with(".rpm") { continue; }
            if !substring.is_empty() && !name.to_lowercase().contains(&substring.to_lowercase()) { continue; }
            if !is_package_for_arch(&name, system_arch) { continue; }
            result.push(path.to_string_lossy().to_string());
        }
    }
    result
}

// ==================== ПОИСК СЕРТИФИКАТА НА USB ====================

fn find_cert_on_mounted_devices() -> Option<String> {
    let search_paths = vec!["/run/media/*/*", "/media/*/*", "/mnt/*"];
    for pattern in &search_paths {
        if let Some(path) = search_pattern(pattern) {
            let cert_path = Path::new(&path).join(CERT_FILE_NAME);
            if cert_path.exists() { return Some(cert_path.to_string_lossy().to_string()); }
        }
    }
    None
}

fn search_pattern(pattern: &str) -> Option<String> {
    let output = Command::new("sh").arg("-c").arg(format!("ls -d {} 2>/dev/null | head -1", pattern))
        .stdout(Stdio::piped()).stderr(Stdio::null()).output();
    match output {
        Ok(out) => {
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if path.is_empty() { None } else { Some(path) }
        }
        Err(_) => None,
    }
}

fn find_first_usb_device() -> Option<String> {
    let output = Command::new("lsblk").arg("-dpno").arg("NAME,TRAN")
        .stdout(Stdio::piped()).stderr(Stdio::null()).output();
    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            for line in stdout.lines() {
                if line.contains("usb") {
                    let device = line.split_whitespace().next().unwrap_or("");
                    if !device.is_empty() { return Some(device.to_string()); }
                }
            }
            None
        }
        Err(_) => None,
    }
}

fn mount_usb_device(device: &str, tx: &mpsc::Sender<LogEvent>) -> Option<String> {
    let mount_point = "/mnt/usb_cert";
    let _ = tx.send(LogEvent::Line(format!("   Монтирую {} в {}...", device, mount_point)));
    let _ = Command::new("mkdir").arg("-p").arg(mount_point).status();

    let mount_result = Command::new("mount").arg(format!("{}1", device)).arg(mount_point)
        .stdout(Stdio::null()).stderr(Stdio::null()).status();
    let success = match mount_result { Ok(s) => s.success(), Err(_) => false };

    if !success {
        let _ = Command::new("umount").arg(mount_point).status();
        let mount_result = Command::new("mount").arg(device).arg(mount_point)
            .stdout(Stdio::null()).stderr(Stdio::null()).status();
        if let Ok(s) = mount_result {
            if !s.success() {
                let _ = tx.send(LogEvent::Line("   ❌ Не удалось смонтировать устройство".to_string()));
                return None;
            }
        } else { return None; }
    }

    let cert_path = Path::new(mount_point).join(CERT_FILE_NAME);
    if cert_path.exists() {
        Some(mount_point.to_string())
    } else {
        let _ = tx.send(LogEvent::Line(format!("   ❌ Файл {} не найден на флешке", CERT_FILE_NAME)));
        let _ = Command::new("umount").arg(mount_point).status();
        None
    }
}

fn is_mountpoint(path: &str) -> bool {
    Command::new("mountpoint").arg("-q").arg(path)
        .stdout(Stdio::null()).stderr(Stdio::null()).status()
        .map(|s| s.success()).unwrap_or(false)
}

fn unmount_device(mount_point: &str, tx: &mpsc::Sender<LogEvent>) {
    if is_mountpoint(mount_point) {
        let _ = tx.send(LogEvent::Line("   Размонтирую флешку...".to_string()));
        let _ = Command::new("umount").arg(mount_point).status();
        let _ = tx.send(LogEvent::Line("   ✅ Флешка размонтирована".to_string()));
    } else {
        let _ = tx.send(LogEvent::Line("   ℹ Флешка уже размонтирована".to_string()));
    }
}

// ==================== АВТОНАСТРОЙКА ПРОКСИ ====================

fn test_proxy_connectivity(proxy_url: &str, tx: &mpsc::Sender<LogEvent>) -> bool {
    let _ = tx.send(LogEvent::Line(format!("   🌐 Проверяю доступность через {}...", proxy_url)));
    let mut cmd = Command::new("curl");
    cmd.arg("-s").arg("-o").arg("/dev/null").arg("-w").arg("%{http_code}")
        .arg("--max-time").arg(PROXY_TEST_TIMEOUT_SECS.to_string())
        .arg("--proxy").arg(proxy_url).arg(PROXY_TEST_URL);

    match cmd.output() {
        Ok(out) => {
            let code = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let success = code == "200" || code == "301" || code == "302";
            if success {
                let _ = tx.send(LogEvent::Line(format!("   ✅ Прокси работает (HTTP {})", code)));
            } else {
                let _ = tx.send(LogEvent::Line(format!("   ⚠ Прокси ответил HTTP {}", code)));
            }
            success
        }
        Err(_) => false,
    }
}

fn test_system_proxy(tx: &mpsc::Sender<LogEvent>) -> bool {
    let _ = tx.send(LogEvent::Line("   🌐 Проверяю системный прокси...".to_string()));
    let mut cmd = Command::new("curl");
    cmd.arg("-s").arg("-o").arg("/dev/null").arg("-w").arg("%{http_code}")
        .arg("--max-time").arg(PROXY_TEST_TIMEOUT_SECS.to_string()).arg(PROXY_TEST_URL);
    match cmd.output() {
        Ok(out) => {
            let code = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let success = code == "200" || code == "301" || code == "302";
            if success { let _ = tx.send(LogEvent::Line("   ✅ Системный прокси работает".to_string())); }
            success
        }
        Err(_) => false,
    }
}

fn detect_proxy_from_env() -> Option<String> {
    for var in &["http_proxy", "HTTP_PROXY", "https_proxy", "HTTPS_PROXY"] {
        if let Ok(val) = env::var(var) {
            if !val.is_empty() && val.starts_with("http") { return Some(val); }
        }
    }
    None
}

fn detect_proxy_from_gsettings() -> Option<String> {
    let output = Command::new("gsettings").arg("get").arg("org.gnome.system.proxy.http").arg("host")
        .stdout(Stdio::piped()).stderr(Stdio::null()).output();
    if let Ok(out) = output {
        let host = String::from_utf8_lossy(&out.stdout).trim().trim_matches('\'').to_string();
        if !host.is_empty() && host != "''" {
            let port_output = Command::new("gsettings").arg("get").arg("org.gnome.system.proxy.http").arg("port")
                .stdout(Stdio::piped()).stderr(Stdio::null()).output();
            if let Ok(port_out) = port_output {
                let port_str = String::from_utf8_lossy(&port_out.stdout);
                let port = port_str.trim().to_string();
                if !port.is_empty() && port != "0" {
                    return Some(format!("http://{}:{}", host, port));
                }
            }
        }
    }
    None
}

fn auto_setup_proxy(tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let _ = tx.send(LogEvent::Line("=== ⚙ Автонастройка прокси ===".to_string()));

    let profile_path = "/etc/profile.d/proxy.sh";
    let dnf_conf = "/etc/dnf/dnf.conf";
    let profile_ok = fs::read_to_string(profile_path).map(|c| c.contains("proxy=")).unwrap_or(false);
    let dnf_ok = fs::read_to_string(dnf_conf).map(|c| c.contains("proxy=")).unwrap_or(false);

    if profile_ok && dnf_ok {
        let current_proxy = get_proxy_url();
        let _ = tx.send(LogEvent::Line(format!("   ℹ Прокси уже настроен: {}", current_proxy)));
        if test_proxy_connectivity(&current_proxy, tx) {
            return (true, format!("Прокси {} работает", current_proxy));
        }
    }

    if test_system_proxy(tx) {
        if let Some(proxy) = detect_proxy_from_gsettings() {
            if test_proxy_connectivity(&proxy, tx) {
                let _ = save_setting("PROXY_URL", &proxy);
                return (true, format!("Прокси {} найден и сохранён", proxy));
            }
        }
        return (true, "Системный прокси работает".to_string());
    }

    if let Some(proxy) = detect_proxy_from_env() {
        if test_proxy_connectivity(&proxy, tx) {
            let _ = save_setting("PROXY_URL", &proxy);
            return (true, format!("Прокси {} найден в окружении", proxy));
        }
    }

    let default_proxy = get_proxy_url();
    if test_proxy_connectivity(&default_proxy, tx) {
        return (true, format!("Прокси {} работает", default_proxy));
    }

    (false, "Не удалось определить прокси автоматически".to_string())
}

// ==================== ШАГ 1: ПРОКСИ ====================

fn setup_proxy(tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let proxy_url = get_proxy_url();
    let profile_path = "/etc/profile.d/proxy.sh";
    let profile_ok = fs::read_to_string(profile_path).map(|c| c.contains(&proxy_url)).unwrap_or(false);
    let dnf_conf_ok = fs::read_to_string("/etc/dnf/dnf.conf").map(|c| c.contains(&format!("proxy={}", proxy_url))).unwrap_or(false);

    if profile_ok && dnf_conf_ok {
        let _ = tx.send(LogEvent::Line(format!("✅ Прокси уже настроен ({})", proxy_url)));
        return (true, "Прокси уже настроен".to_string());
    }

    let _ = tx.send(LogEvent::Line(format!("=== 🌐 Настройка прокси {} ===", proxy_url)));
    let profile_content = format!("# school-installer\nexport http_proxy=\"{u}\"\nexport https_proxy=\"{u}\"\nexport no_proxy=\"localhost,127.0.0.1,::1\"\n", u = proxy_url);
    if let Err(e) = fs::write(profile_path, &profile_content) {
        return (false, format!("❌ Не удалось записать {}: {}", profile_path, e));
    }
    let _ = Command::new("chmod").arg("644").arg(profile_path).status();

    let conf = "/etc/dnf/dnf.conf";
    let content = fs::read_to_string(conf).unwrap_or_default();
    let mut new_lines: Vec<String> = Vec::new();
    let mut found = false;
    let mut inserted_after_main = false;
    for line in content.lines() {
        if line.trim_start().starts_with("proxy=") {
            new_lines.push(format!("proxy={}", proxy_url));
            found = true;
        } else {
            new_lines.push(line.to_string());
            if !found && !inserted_after_main && line.trim() == "[main]" {
                new_lines.push(format!("proxy={}", proxy_url));
                inserted_after_main = true;
                found = true;
            }
        }
    }
    if !found {
        new_lines.insert(0, "[main]".to_string());
        new_lines.insert(1, format!("proxy={}", proxy_url));
    }
    if let Err(e) = fs::write(conf, new_lines.join("\n") + "\n") {
        return (false, format!("❌ Не удалось записать {}: {}", conf, e));
    }
    let _ = tx.send(LogEvent::Line(format!("✅ {} обновлён", conf)));

    if test_proxy_connectivity(&proxy_url, tx) {
        (true, format!("Прокси {} настроен и работает", proxy_url))
    } else {
        (true, format!("Прокси {} настроен, но не отвечает", proxy_url))
    }
}

// ==================== ШАГ 2: СЕРТИФИКАТ ====================

fn install_cert(cert_dir: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let _ = tx.send(LogEvent::Line(format!("=== 🔒 Проверка сертификатов из '{}' ===", cert_dir)));

    let mut certs: Vec<String> = Vec::new();
    if Path::new(cert_dir).exists() {
        if let Ok(entries) = fs::read_dir(cert_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                if name == "README.txt" { continue; }
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
                if name.starts_with("._") { continue; }
                if ext != "crt" && ext != "pem" && ext != "cer" { continue; }
                if !path.is_file() { continue; }
                if let Ok(meta) = fs::metadata(&path) { if meta.len() < 100 { continue; } }
                let content = fs::read(&path).unwrap_or_default();
                let is_pem = content.starts_with(b"-----BEGIN CERTIFICATE-----");
                let is_der = content.len() > 2 && content[0] == 0x30 && (content[1] == 0x82 || content[1] == 0x81);
                if !is_pem && !is_der { continue; }
                certs.push(path.to_string_lossy().to_string());
            }
        }
    }

    if certs.is_empty() {
        if let Some(cert_path) = find_cert_on_mounted_devices() {
            let _ = tx.send(LogEvent::Line(format!("   ✅ Найден на USB: {}", cert_path)));
            certs.push(cert_path);
        }
    }

    if certs.is_empty() {
        let _ = tx.send(LogEvent::Line("⚠ Сертификаты не найдены — пропускаем".to_string()));
        return (true, "Сертификат пропущен".to_string());
    }

    let anchors_dir = "/etc/pki/ca-trust/source/anchors";
    let _ = fs::create_dir_all(anchors_dir);
    for cert in &certs {
        let name = Path::new(cert).file_name().unwrap_or_default().to_string_lossy().to_string();
        let dst = format!("{}/{}", anchors_dir, name);
        if !Path::new(&dst).exists() {
            if let Err(e) = fs::copy(cert, &dst) {
                return (false, format!("❌ Не удалось скопировать {}: {}", cert, e));
            }
            let _ = tx.send(LogEvent::Line(format!("   → {}", dst)));
        }
    }

    if command_exists("update-ca-trust") {
        let cmd = Command::new("update-ca-trust");
        match run_process(cmd, tx) {
            Ok(s) if s.success() => { let _ = tx.send(LogEvent::Line("✅ Системное хранилище обновлено".to_string())); }
            _ => {}
        }
    }

    let (home, real_user) = real_home_and_user();
    let nssdb_path = format!("{}/.pki/nssdb", home);
    let nssdb = format!("sql:{}", nssdb_path);

    if !command_exists("certutil") {
        let mut cmd = Command::new("dnf");
        cmd.arg("install").arg("-y").arg("nss-tools");
        let _ = run_process(cmd, tx);
    }

    let _ = fs::create_dir_all(&nssdb_path);
    let db_exists = Path::new(&format!("{}/cert9.db", nssdb_path)).exists();
    if !db_exists {
        let mut init = Command::new("certutil");
        init.arg("-d").arg(&nssdb).arg("-N").arg("--empty-password");
        let _ = init.stdout(Stdio::null()).stderr(Stdio::null()).status();
    }

    for cert in &certs {
        let nick = Path::new(cert).file_stem().unwrap_or_default().to_string_lossy().to_string();
        let del_cmd = format!("echo '' | certutil -d '{}' -D -n '{}' 2>/dev/null", nssdb, nick);
        let _ = Command::new("sh").arg("-c").arg(&del_cmd).stdout(Stdio::null()).stderr(Stdio::null()).status();

        let add_cmd = format!("echo '' | certutil -d '{}' -A -t 'C,,' -n '{}' -i '{}'", nssdb, nick, cert);
        match Command::new("sh").arg("-c").arg(&add_cmd).stdout(Stdio::piped()).stderr(Stdio::piped()).status() {
            Ok(s) if s.success() => {
                let verify_cmd = format!("echo '' | certutil -d '{}' -L 2>/dev/null | grep -q '{}'", nssdb, nick);
                let verified = Command::new("sh").arg("-c").arg(&verify_cmd).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
                if verified {
                    let _ = tx.send(LogEvent::Line(format!("   ✅ NSS: {} (проверено)", nick)));
                } else {
                    let _ = tx.send(LogEvent::Line(format!("   ⚠ NSS: {} не проверен", nick)));
                }
            }
            _ => { let _ = tx.send(LogEvent::Line(format!("   ❌ NSS не добавлен: {}", nick))); }
        }
    }

    if let Some(user) = real_user {
        let mut ch = Command::new("chown");
        ch.arg("-R").arg(format!("{}:{}", user, user)).arg(&nssdb_path);
        let _ = ch.status();
    }

    (true, format!("Сертификатов установлено: {}", certs.len()))
}

fn prepare_environment(tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let (ok1, m1) = setup_proxy(tx);
    if !ok1 { return (false, format!("Прокси: {}", m1)); }
    let cert_dir = get_cert_dir();
    let (ok2, m2) = install_cert(&cert_dir, tx);
    if !ok2 { return (false, format!("Сертификат: {}", m2)); }
    (true, format!("Прокси ✅ | {}", m2))
}

// ==================== УЛУЧШЕНИЕ №12: ПРОВЕРКА ЦЕЛОСТНОСТИ ====================

fn calculate_sha256(path: &str) -> Option<String> {
    let output = Command::new("sha256sum").arg(path)
        .stdout(Stdio::piped()).stderr(Stdio::null()).output();
    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            stdout.split_whitespace().next().map(|s| s.to_string())
        }
        Err(_) => None,
    }
}

fn save_checksums(folder: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let system_arch = detect_arch();
    let files = find_packages_flat(folder, "", &system_arch);
    if files.is_empty() {
        return (false, format!("В папке {} нет .rpm файлов", folder));
    }

    let checksum_path = Path::new(folder).join(CHECKSUM_FILE);
    let mut content = String::new();
    let mut count = 0;

    for file in &files {
        if let Some(hash) = calculate_sha256(file) {
            let filename = Path::new(file).file_name().unwrap_or_default().to_string_lossy().to_string();
            content += &format!("{}  {}\n", hash, filename);
            count += 1;
        }
    }

    match fs::write(&checksum_path, &content) {
        Ok(_) => {
            let _ = tx.send(LogEvent::Line(format!("✅ Сохранено {} хешей в {}", count, checksum_path.display())));
            (true, format!("Сохранено {} SHA256-хешей", count))
        }
        Err(e) => (false, format!("❌ Не удалось записать checksums: {}", e)),
    }
}

fn verify_checksums(folder: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String, Vec<String>) {
    let checksum_path = Path::new(folder).join(CHECKSUM_FILE);
    if !checksum_path.exists() {
        let _ = tx.send(LogEvent::Line("⚠ Файл checksums.sha256 не найден — пропускаем проверку".to_string()));
        return (true, "Проверка пропущена (нет checksums)".to_string(), vec![]);
    }

    let content = match fs::read_to_string(&checksum_path) {
        Ok(c) => c,
        Err(e) => return (false, format!("❌ Не удалось прочитать checksums: {}", e), vec![])
    };

    let mut broken: Vec<String> = Vec::new();
    let mut checked = 0;
    let mut ok_count = 0;

    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 { continue; }
        let expected_hash = parts[0];
        let filename = parts[1];
        let full_path = Path::new(folder).join(filename);

        if !full_path.exists() {
            broken.push(format!("{} (файл отсутствует)", filename));
            continue;
        }

        checked += 1;
        if let Some(actual_hash) = calculate_sha256(&full_path.to_string_lossy()) {
            if actual_hash == expected_hash {
                ok_count += 1;
            } else {
                broken.push(format!("{} (хеши не совпадают)", filename));
                let _ = tx.send(LogEvent::Line(format!("   ❌ НЕ СОВПАДАЕТ: {}", filename)));
            }
        }
    }

    if broken.is_empty() {
        let _ = tx.send(LogEvent::Line(format!("✅ Проверено {} файлов, все целостны", checked)));
        (true, format!("Все {} файлов целостны", checked), vec![])
    } else {
        let _ = tx.send(LogEvent::Line(format!("⚠ Повреждено {} из {} файлов", broken.len(), checked)));
        (false, format!("{} повреждённых файлов", broken.len()), broken)
    }
}

fn remove_broken_files(folder: &str, broken: &[String], tx: &mpsc::Sender<LogEvent>) {
    for entry in broken {
        let filename = entry.split_whitespace().next().unwrap_or("");
        let full_path = Path::new(folder).join(filename);
        if full_path.exists() {
            let _ = fs::remove_file(&full_path);
            let _ = tx.send(LogEvent::Line(format!("   🗑 Удалён повреждённый: {}", filename)));
        }
    }
}

// ==================== УЛУЧШЕНИЕ №11: ПАРАЛЛЕЛЬНОЕ СКАЧИВАНИЕ ====================

fn download_single_package(pkg: String, download_dir: String, proxy_url: String) -> (String, bool, String) {
    let mut cmd = Command::new("dnf");
    cmd.arg("download").arg("--resolve").arg("--alldeps")
        .arg("--exclude=*.i686").arg("--exclude=*.i586").arg("--exclude=*.i386")
        .arg(format!("--destdir={}", download_dir))
        .env("http_proxy", &proxy_url)
        .env("https_proxy", &proxy_url)
        .env("no_proxy", "localhost,127.0.0.1,::1")
        .arg(&pkg)
        .stdout(Stdio::piped()).stderr(Stdio::piped());

    match cmd.output() {
        Ok(out) if out.status.success() => {
            (pkg.clone(), true, format!("{} скачан", pkg))
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let msg = stderr.lines().last().unwrap_or("неизвестная ошибка").to_string();
            (pkg.clone(), false, format!("{}: {}", pkg, msg))
        }
        Err(e) => (pkg.clone(), false, format!("{}: spawn error: {}", pkg, e)),
    }
}

fn download_packages_parallel(packages: &[&str], download_dir: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let total = packages.len();
    if total == 0 {
        return (true, "Нет пакетов для скачивания".to_string());
    }

    let _ = tx.send(LogEvent::Line(format!("🚀 Запускаю параллельное скачивание {} пакетов ({} потоков)", total, MAX_PARALLEL_DOWNLOADS)));

    let proxy_url = get_proxy_url();
    let download_dir_owned = download_dir.to_string();
    let packages_owned: Vec<String> = packages.iter().map(|s| s.to_string()).collect();

    let queue: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(packages_owned));
    let completed = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));

    let (result_tx, result_rx) = mpsc::channel::<(String, bool, String)>();
    let mut handles = Vec::new();

    for _ in 0..MAX_PARALLEL_DOWNLOADS {
        let queue = Arc::clone(&queue);
        let completed = Arc::clone(&completed);
        let failed = Arc::clone(&failed);
        let tx_clone = tx.clone();
        let result_tx = result_tx.clone();
        let dir = download_dir_owned.clone();
        let proxy = proxy_url.clone();
        let total = total;

        let handle = thread::spawn(move || {
            loop {
                let pkg = {
                    let mut q = queue.lock().unwrap();
                    q.pop()
                };
                let pkg = match pkg {
                    Some(p) => p,
                    None => break,
                };

                let (name, ok, msg) = download_single_package(pkg.clone(), dir.clone(), proxy.clone());
                let _ = result_tx.send((name, ok, msg.clone()));

                let done = completed.fetch_add(1, Ordering::SeqCst) + 1;
                if !ok { failed.fetch_add(1, Ordering::SeqCst); }

                let pct = done as f64 / total as f64;
                let label = format!("Скачано: {} / {}", done, total);
                let _ = tx_clone.send(LogEvent::Progress(pct, label));
                let _ = tx_clone.send(LogEvent::Line(format!("   {}", msg)));
            }
        });
        handles.push(handle);
    }

    drop(result_tx);

    for h in handles {
        let _ = h.join();
    }

    let mut success_count = 0;
    let mut fail_count = 0;

    for (name, ok, _msg) in result_rx.iter() {
        if ok { success_count += 1; }
        else { fail_count += 1; }
        log_to_file(&format!("[PARALLEL] {} {}", name, if ok { "OK" } else { "FAIL" }));
    }

    let _ = save_checksums(download_dir, tx);

    if fail_count == 0 {
        (true, format!("Параллельно скачано {} пакетов", success_count))
    } else if success_count > 0 {
        (true, format!("Скачано {} пакетов, {} с ошибками", success_count, fail_count))
    } else {
        (false, format!("Все {} пакетов не удалось скачать", fail_count))
    }
}

// ==================== СКАЧИВАНИЕ ====================

fn download_updates(download_dir: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let _ = fs::create_dir_all(download_dir);
    let (space_ok, space_msg) = check_free_space(download_dir, MIN_FREE_SPACE_UPDATES_MB, "Скачивание обновлений");
    let _ = tx.send(LogEvent::Line(space_msg.clone()));
    if !space_ok { return (false, space_msg); }

    let abs_path = fs::canonicalize(download_dir).unwrap_or_else(|_| Path::new(download_dir).to_path_buf());
    let _ = tx.send(LogEvent::Line(format!("📥 Скачиваю обновления в: {}", abs_path.display())));

    let mut cmd = Command::new("dnf");
    cmd.arg("upgrade").arg("--downloadonly").arg("--exclude=*.i686").arg("--exclude=*.i586").arg("--exclude=*.i386")
        .arg(format!("--destdir={}", abs_path.display())).arg("-y");

    match run_process(cmd, tx) {
        Ok(s) if s.success() => {
            let rpm_count = find_packages_flat(download_dir, "", &detect_arch()).len();
            let _ = save_checksums(download_dir, tx);
            if rpm_count > 0 { (true, format!("Обновления скачаны ({} пакетов)", rpm_count)) }
            else { (false, "dnf отработал, но файлы не найдены".to_string()) }
        }
        Ok(_s) => (false, "Ошибка скачивания".to_string()),
        Err(e) => (false, format!("❌ Не удалось запустить dnf: {}", e)),
    }
}

fn download_packages(packages: &[&str], download_dir: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let _ = fs::create_dir_all(download_dir);
    let (space_ok, space_msg) = check_free_space(download_dir, MIN_FREE_SPACE_PACKAGES_MB, "Скачивание пакетов");
    let _ = tx.send(LogEvent::Line(space_msg.clone()));
    if !space_ok { return (false, space_msg); }

    let proxy_url = get_proxy_url();
    let _ = tx.send(LogEvent::Line("📦 Скачиваю пакеты с зависимостями (для офлайн-установки)...".to_string()));
    let _ = tx.send(LogEvent::Line("   ⚠ Будет скачано много файлов — это нормально!".to_string()));

    let mut cmd = Command::new("dnf");
    cmd.arg("download")
        .arg("--resolve")
        .arg("--alldeps")
        .arg("--exclude=*.i686")
        .arg("--exclude=*.i586")
        .arg("--exclude=*.i386")
        .arg("--destdir").arg(download_dir)
        .env("http_proxy", &proxy_url)
        .env("https_proxy", &proxy_url);

    for pkg in packages {
        cmd.arg(pkg);
    }

    match run_process(cmd, tx) {
        Ok(s) if s.success() => {
            let rpm_count = find_packages_flat(download_dir, "", &detect_arch()).len();
            let _ = tx.send(LogEvent::Line(format!("✅ Скачано {} пакетов (включая зависимости)", rpm_count)));
            let _ = save_checksums(download_dir, tx);
            (true, format!("Скачано {} пакетов с зависимостями", rpm_count))
        }
        Ok(_s) => (false, "Ошибка скачивания".to_string()),
        Err(e) => (false, format!("❌ Не удалось запустить dnf: {}", e)),
    }
}

// ==================== УСТАНОВКА ИЗ ПАПКИ ====================

fn install_from_folder(download_dir: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let system_arch = detect_arch();

    let _ = tx.send(LogEvent::Line("🧪 Проверяю целостность пакетов...".to_string()));
    let (integrity_ok, integrity_msg, broken) = verify_checksums(download_dir, tx);
    let _ = tx.send(LogEvent::Line(integrity_msg));

    if !integrity_ok && !broken.is_empty() {
        let _ = tx.send(LogEvent::Line("🗑 Удаляю повреждённые файлы...".to_string()));
        remove_broken_files(download_dir, &broken, tx);
        let _ = save_checksums(download_dir, tx);
    }

    let files = find_packages_flat(download_dir, "", &system_arch);
    if files.is_empty() { return (false, format!("❌ В папке '{}' нет .rpm файлов", download_dir)); }
    let _ = tx.send(LogEvent::Line(format!("Найдено {} пакетов для установки", files.len())));

    let total_size_bytes: u64 = files.iter().filter_map(|f| fs::metadata(f).ok().map(|m| m.len())).sum();
    let total_size_mb = total_size_bytes / (1024 * 1024);
    let required_mb = total_size_mb * 3;
    let (space_ok, space_msg) = check_free_space(download_dir, required_mb.max(MIN_FREE_SPACE_CRITICAL_MB), "Установка пакетов");
    let _ = tx.send(LogEvent::Line(space_msg.clone()));
    if !space_ok { return (false, space_msg); }

    let mut installed = 0;
    for chunk in files.chunks(80) {
        let mut cmd = Command::new("dnf");
        cmd.arg("install").arg("-y").arg("--allowerasing").arg("--nobest");
        for file in chunk { cmd.arg(file); }
        match run_process(cmd, tx) {
            Ok(s) if s.success() => { installed += chunk.len(); }
            Ok(_s) => {}
            Err(e) => { let _ = tx.send(LogEvent::Line(format!("❌ Ошибка: {}", e))); }
        }
    }
    if installed > 0 { (true, format!("Установлено пакетов: {}", installed)) }
    else { (false, "Не удалось установить ни одного пакета".to_string()) }
}

// ==================== КРИПТОПРО + ГИА ====================

fn download_package_to_folder(package_name: &str, folder: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let _ = fs::create_dir_all(folder);
    let (space_ok, space_msg) = check_free_space(folder, 100, format!("Скачивание {}", package_name));
    let _ = tx.send(LogEvent::Line(space_msg.clone()));
    if !space_ok { return (false, space_msg); }

    let mut cmd = Command::new("dnf");
    cmd.arg("download").arg(format!("--destdir={}", folder)).arg(package_name);

    match run_process(cmd, tx) {
        Ok(s) if s.success() => {
            let files = find_packages_by_substring(folder, package_name, &detect_arch());
            if !files.is_empty() { return (true, format!("{} скачан", package_name)); }
            (false, format!("{} скачан, но файлы не найдены", package_name))
        }
        Ok(_s) => (false, format!("{} не найден в репозитории", package_name)),
        Err(e) => (false, format!("❌ Не удалось скачать {}: {}", package_name, e)),
    }
}

fn install_local_packages(folder: &str, pattern: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let files = find_packages_by_substring(folder, pattern, &detect_arch());
    if files.is_empty() { return (false, format!("❌ Не найдено *{}*.rpm в папке {}", pattern, folder)); }

    let _ = tx.send(LogEvent::Line(format!("Устанавливаю {} пакетов: {}", files.len(), pattern)));
    let mut cmd = Command::new("dnf");
    cmd.arg("install").arg("-y").arg("--allowerasing").arg("--nobest");
    for file in &files { cmd.arg(file); }
    match run_process(cmd, tx) {
        Ok(s) if s.success() => (true, format!("{} установлены", pattern)),
        Ok(_s) => (false, format!("Ошибка установки {}", pattern)),
        Err(e) => (false, format!("❌ Не удалось установить {}: {}", pattern, e)),
    }
}

fn install_cryptopro_csp(crypto_subdir: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let install_script = Path::new(crypto_subdir).join("install.sh");
    let uninstall_script = Path::new(crypto_subdir).join("uninstall.sh");
    if !install_script.exists() { return (false, "❌ В linux-amd64/ не найден install.sh".to_string()); }
    if !uninstall_script.exists() { return (false, "❌ В linux-amd64/ не найден uninstall.sh".to_string()); }

    let (space_ok, space_msg) = check_free_space(crypto_subdir, MIN_FREE_SPACE_CRYPTO_MB, "Установка КриптоПро CSP");
    let _ = tx.send(LogEvent::Line(space_msg.clone()));
    if !space_ok { return (false, space_msg); }

    let _ = tx.send(LogEvent::Line("Запускаю install.sh КриптоПро (kc1, --yes)...".to_string()));
    let mut cmd = Command::new("sh");
    cmd.current_dir(crypto_subdir).arg("./install.sh").arg("kc1").arg("--yes");
    match run_process(cmd, tx) {
        Ok(s) if s.success() => (true, "КриптоПро CSP установлен".to_string()),
        Ok(_s) => (false, "install.sh завершился с ошибкой".to_string()),
        Err(e) => (false, format!("❌ Не удалось запустить install.sh: {}", e)),
    }
}

fn install_cryptopro_gia(crypto_dir: &str, school_dir: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let _ = fs::create_dir_all(crypto_dir);
    let _ = fs::create_dir_all(school_dir);
    let system_arch = detect_arch();

    let (space_ok, space_msg) = check_free_space(crypto_dir, MIN_FREE_SPACE_CRYPTO_MB, "Установка КриптоПро + ГИА");
    let _ = tx.send(LogEvent::Line(space_msg.clone()));
    if !space_ok { return (false, space_msg); }

    let _ = tx.send(LogEvent::Line("=== ШАГ 1/5: newt ===".to_string()));
    let _ = tx.send(LogEvent::Progress(0.0, "Шаг 1: newt...".to_string()));
    let newt_installed = is_package_installed("newt");
    if !newt_installed && find_packages_by_substring(crypto_dir, "newt", &system_arch).is_empty() {
        let (ok, msg) = download_package_to_folder("newt", crypto_dir, tx);
        if !ok { return (false, format!("Шаг 1 (newt): {}", msg)); }
    }
    if !newt_installed {
        let (ok, msg) = install_local_packages(crypto_dir, "newt", tx);
        if !ok { return (false, format!("Шаг 1 (newt): {}", msg)); }
    }

    let _ = tx.send(LogEvent::Line("=== ШАГ 2.1/5: pcsc-lite ===".to_string()));
    let _ = tx.send(LogEvent::Progress(0.15, "Шаг 2.1: pcsc-lite...".to_string()));
    for pkg in &["pcsc-lite", "pcsc-lite-ccid"] {
        let pkg_installed = is_package_installed(pkg);
        if !pkg_installed && find_packages_by_substring(crypto_dir, pkg, &system_arch).is_empty() {
            let (ok, msg) = download_package_to_folder(pkg, crypto_dir, tx);
            if !ok { return (false, format!("Шаг 2.1 ({}): {}", pkg, msg)); }
        }
        if !pkg_installed {
            let (ok, msg) = install_local_packages(crypto_dir, pkg, tx);
            if !ok { return (false, format!("Шаг 2.1 ({}): {}", pkg, msg)); }
        }
    }

    let _ = tx.send(LogEvent::Line("=== ШАГ 2.2/5: librtpkcs11ecp ===".to_string()));
    let _ = tx.send(LogEvent::Progress(0.3, "Шаг 2.2: librtpkcs11ecp...".to_string()));
    let lib_installed = is_package_installed("librtpkcs11ecp");
    if !lib_installed && find_packages_by_substring(crypto_dir, "librtpkcs11ecp", &system_arch).is_empty() {
        let (ok, msg) = download_package_to_folder("librtpkcs11ecp", crypto_dir, tx);
        if !ok { return (false, format!("Шаг 2.2: {}", msg)); }
    }
    if !lib_installed {
        let (ok, msg) = install_local_packages(crypto_dir, "librtpkcs11ecp", tx);
        if !ok { return (false, format!("Шаг 2.2: {}", msg)); }
    }

    let _ = tx.send(LogEvent::Line("=== ШАГ 3/5: КриптоПро CSP ===".to_string()));
    let _ = tx.send(LogEvent::Progress(0.5, "Шаг 3: КриптоПро...".to_string()));
    let crypto_subdir = Path::new(crypto_dir).join("linux-amd64");
    let (ok, msg) = install_cryptopro_csp(&crypto_subdir.to_string_lossy(), tx);
    if !ok { return (false, format!("Шаг 3: {}", msg)); }

    let _ = tx.send(LogEvent::Line("=== ШАГ 4/5: GIA11_PPE ===".to_string()));
    let _ = tx.send(LogEvent::Progress(0.75, "Шаг 4: GIA11_PPE...".to_string()));
    if find_packages_by_substring(crypto_dir, "GIA11_PPE", &system_arch).is_empty() {
        return (false, "❌ GIA11_PPE*.rpm не найден в cryptopro_gia/".to_string());
    }
    let (ok, msg) = install_local_packages(crypto_dir, "GIA11_PPE", tx);
    if !ok { return (false, format!("Шаг 4: {}", msg)); }

    let _ = tx.send(LogEvent::Line("=== ШАГ 5/5: pcscd ===".to_string()));
    let _ = tx.send(LogEvent::Progress(0.9, "Шаг 5: запуск pcscd...".to_string()));
    let mut cmd = Command::new("systemctl");
    cmd.arg("enable").arg("--now").arg("pcscd");
    match run_process(cmd, tx) {
        Ok(s) if s.success() => { let _ = tx.send(LogEvent::Line("✅ pcscd запущен".to_string())); }
        _ => { let _ = tx.send(LogEvent::Line("⚠ pcscd не запущен".to_string())); }
    }

    (true, "КриптоПро + ГИА установлены".to_string())
}

fn get_packages() -> Vec<&'static str> { vec!["task-koge", "r7-office", "notepadqq"] }

// ==================== УЛУЧШЕНИЕ №13: АВТООБНОВЛЕНИЕ ====================

fn extract_version_from_tag(tag: &str) -> String {
    tag.trim_start_matches('v').to_string()
}

fn is_newer_version(current: &str, new: &str) -> bool {
    let cur_parts: Vec<u32> = current.split('.').filter_map(|s| s.parse().ok()).collect();
    let new_parts: Vec<u32> = new.split('.').filter_map(|s| s.parse().ok()).collect();
    let max_len = cur_parts.len().max(new_parts.len());
    for i in 0..max_len {
        let c = cur_parts.get(i).copied().unwrap_or(0);
        let n = new_parts.get(i).copied().unwrap_or(0);
        if n > c { return true; }
        if n < c { return false; }
    }
    false
}

fn check_for_update(tx: &mpsc::Sender<LogEvent>) -> (bool, String, Option<String>, Option<String>) {
    let url = format!("{}/repos/{}/{}/releases/latest", GITHUB_API_URL, GITHUB_REPO_OWNER, GITHUB_REPO_NAME);
    let _ = tx.send(LogEvent::Line("🔗 Проверяю обновления на GitHub...".to_string()));
    log_to_file(&format!("Проверка обновлений: {}", url));

    let proxy_url = get_proxy_url();
    let mut cmd = Command::new("curl");
    cmd.arg("-s").arg("-L").arg("--max-time").arg("10")
        .arg("--proxy").arg(&proxy_url)
        .arg("-H").arg("Accept: application/vnd.github+json")
        .arg(&url);

    match cmd.output() {
        Ok(out) if out.status.success() => {
            let body = String::from_utf8_lossy(&out.stdout).to_string();
            if body.is_empty() || body.contains("Not Found") {
                return (false, "❌ Репозиторий не найден. Настрой GITHUB_REPO_OWNER и GITHUB_REPO_NAME в коде".to_string(), None, None);
            }

            let tag = extract_json_string(&body, "tag_name");
            let download_url = extract_release_asset_url(&body);

            match (tag, download_url) {
                (Some(tag), Some(url)) => {
                    let new_version = extract_version_from_tag(&tag);
                    if is_newer_version(VERSION, &new_version) {
                        let _ = tx.send(LogEvent::Line(format!("🆕 Найдена новая версия: v{} (текущая v{})", new_version, VERSION)));
                        (true, format!("Доступно обновление: v{} → v{}", VERSION, new_version), Some(new_version), Some(url))
                    } else {
                        let _ = tx.send(LogEvent::Line(format!("✅ Обновлений нет (текущая v{} — последняя)", VERSION)));
                        (false, format!("Обновлений нет, актуальная версия v{}", VERSION), None, None)
                    }
                }
                (Some(tag), None) => {
                    let new_version = extract_version_from_tag(&tag);
                    if is_newer_version(VERSION, &new_version) {
                        (false, format!("⚠ Найдена v{}, но нет download_url в релизе", new_version), Some(new_version), None)
                    } else {
                        (false, "✅ Обновлений нет".to_string(), None, None)
                    }
                }
                _ => (false, "❌ Не удалось разобрать ответ GitHub API".to_string(), None, None),
            }
        }
        Ok(_) => (false, "❌ GitHub API вернул ошибку".to_string(), None, None),
        Err(e) => (false, format!("❌ Не удалось проверить обновления: {}", e), None, None),
    }
}

fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let search = format!("\"{}\":", key);
    let start = json.find(&search)? + search.len();
    let rest = &json[start..];
    let first_quote = rest.find('"')?;
    let after_quote = &rest[first_quote + 1..];
    let end_quote = after_quote.find('"')?;
    Some(after_quote[..end_quote].to_string())
}

fn extract_release_asset_url(json: &str) -> Option<String> {
    let assets_start = json.find("\"assets\":")?;
    let rest = &json[assets_start..];
    let mut search_pos = 0;
    while let Some(idx) = rest[search_pos..].find("\"browser_download_url\":") {
        let abs_idx = search_pos + idx + "\"browser_download_url\":".len();
        if let Some(url) = extract_json_string(&rest[abs_idx..], "") {
            if url.contains("school-installer") && (url.ends_with(".tar.gz") || url.ends_with(".gz") || url.contains("linux")) {
                return Some(url);
            }
        }
        search_pos = abs_idx;
    }
    None
}

fn apply_update(download_url: &str, new_version: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let _ = tx.send(LogEvent::Line(format!("⬇ Скачиваю v{}...", new_version)));

    let current_exe = match env::current_exe() {
        Ok(e) => e,
        Err(e) => return (false, format!("❌ Не удалось получить путь к бинарнику: {}", e)),
    };
    let update_dir = Path::new(&get_report_dir());
    let download_path = update_dir.join(format!("school-installer-{}.tmp", new_version));

    let proxy_url = get_proxy_url();
    let mut cmd = Command::new("curl");
    cmd.arg("-L").arg("--max-time").arg("120")
        .arg("--proxy").arg(&proxy_url)
        .arg("-o").arg(&download_path)
        .arg(download_url);

    match cmd.status() {
        Ok(s) if s.success() => {}
        Ok(_) => { let _ = fs::remove_file(&download_path); return (false, "❌ Скачивание не удалось".to_string()); }
        Err(e) => return (false, format!("❌ curl error: {}", e)),
    }

    let meta = match fs::metadata(&download_path) {
        Ok(m) => m,
        Err(_) => { let _ = fs::remove_file(&download_path); return (false, "❌ Файл не скачан".to_string()); }
    };
    if meta.len() < 100_000 {
        let _ = fs::remove_file(&download_path);
        return (false, format!("⚠ Скачанный файл подозрительно мал ({} Б)", meta.len()));
    }

    let _ = tx.send(LogEvent::Line(format!("   Файл скачан: {}", format_size_bytes(meta.len()))));

    let final_path = if download_path.to_string_lossy().ends_with(".tar.gz") || download_path.to_string_lossy().ends_with(".gz") {
        let _ = tx.send(LogEvent::Line("   Распаковываю tar.gz...".to_string()));
        let extract_dir = update_dir.join("update_extract");
        let _ = fs::create_dir_all(&extract_dir);
        let mut untar = Command::new("tar");
        untar.arg("-xzf").arg(&download_path).arg("-C").arg(&extract_dir);
        if !untar.status().map(|s| s.success()).unwrap_or(false) {
            return (false, "❌ Не удалось распаковать".to_string());
        }
        find_extracted_binary(&extract_dir).unwrap_or_else(|| {
            let _ = fs::remove_file(&download_path);
            PathBuf::from("")
        })
    } else {
        download_path.clone()
    };

    if final_path.as_os_str().is_empty() || !final_path.exists() {
        let _ = fs::remove_file(&download_path);
        return (false, "❌ Бинарник не найден в архиве".to_string());
    }

    let new_hash = calculate_sha256(&final_path.to_string_lossy());
    let _ = tx.send(LogEvent::Line(format!("   SHA256: {}", new_hash.as_deref().unwrap_or("???"))));

    let backup_path = update_dir.join(format!("school-installer.backup-{}", VERSION));
    let _ = tx.send(LogEvent::Line("   Создаю backup текущей версии...".to_string()));
    let _ = fs::rename(&current_exe, &backup_path);

    match fs::rename(&final_path, &current_exe) {
        Ok(_) => {
            let _ = fs::set_permissions(&current_exe, fs::Permissions::from_mode(0o755));
            let _ = tx.send(LogEvent::Line(format!("✅ Обновление до v{} применено!", new_version)));
            let _ = tx.send(LogEvent::Line(format!("   Backup сохранён: {}", backup_path.display())));
            let _ = tx.send(LogEvent::Line("   🔄 Перезапустите программу для новой версии".to_string()));
            log_to_file(&format!("УСПЕШНОЕ ОБНОВЛЕНИЕ: v{} → v{}", VERSION, new_version));
            (true, format!("Обновлено до v{}. Перезапустите программу.", new_version))
        }
        Err(e) => {
            let _ = fs::rename(&backup_path, &current_exe);
            (false, format!("❌ Не удалось заменить бинарник: {}", e))
        }
    }
}

fn find_extracted_binary(dir: &Path) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        if let Ok(entries) = fs::read_dir(&current) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.file_name().map(|n| n == "school-installer").unwrap_or(false) {
                    return Some(path);
                }
            }
        }
    }
    None
}

// ==================== ЭКСПОРТ ОТЧЁТА ====================

struct InstalledPackage {
    name: String,
    version: String,
    arch: String,
    install_date: String,
    size_mb: f64,
}

fn get_installed_packages() -> Vec<InstalledPackage> {
    let output = Command::new("rpm")
        .arg("-qa")
        .arg("--qf")
        .arg("%{NAME}\t%{VERSION}-%{RELEASE}\t%{ARCH}\t%{INSTALLTIME:date:%Y-%m-%d %H:%M}\t%{SIZE}\n")
        .output();
    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            stdout.lines().filter_map(|line| {
                let parts: Vec<&str> = line.split('\t').collect();
                if parts.len() >= 5 {
                    let size_bytes: u64 = parts[4].parse().unwrap_or(0);
                    Some(InstalledPackage {
                        name: parts[0].to_string(),
                        version: parts[1].to_string(),
                        arch: parts[2].to_string(),
                        install_date: parts[3].to_string(),
                        size_mb: size_bytes as f64 / (1024.0 * 1024.0),
                    })
                } else { None }
            }).collect()
        }
        Err(_) => vec![],
    }
}

fn get_our_package_prefixes() -> Vec<&'static str> {
    vec![
        "task-koge", "r7-office", "notepadqq", "newt", "pcsc-lite", "librtpkcs11ecp",
        "lsb-cprocsp", "cprocsp", "GIA11_PPE", "nss-tools", "libnss3-tools",
        "kumir2", "python3", "fpc", "lazarus", "geany", "codeblocks", "libreoffice", "chromium", "gimp",
    ]
}

fn filter_our_packages(packages: &[InstalledPackage]) -> Vec<&InstalledPackage> {
    let prefixes = get_our_package_prefixes();
    packages.iter().filter(|p| prefixes.iter().any(|prefix| p.name.starts_with(prefix))).collect()
}

fn format_txt_report(packages: &[&InstalledPackage], timestamp: &str) -> String {
    let mut report = String::new();
    report += "ОТЧЁТ ПО УСТАНОВЛЕННОМУ ПО (school-installer)\n";
    report += "=============================================\n";
    report += &format!("Дата отчёта: {}\n", timestamp);
    report += &format!("ОС: МОС ОС (ROSA) | Архитектура: {}\n", detect_arch());
    report += &format!("Хост: {}\n", get_hostname());
    report += &format!("Версия установщика: {}\n\n", VERSION);
    report += "УСТАНОВЛЕННЫЕ ПАКЕТЫ:\n---------------------\n";
    if packages.is_empty() {
        report += "  (наши пакеты не найдены в системе)\n";
    } else {
        for pkg in packages {
            report += &format!("  {:<35} {:<18} {:<8} {:>8.1} МБ\n",
                format!("{}-{}", pkg.name, pkg.version), pkg.install_date, pkg.arch, pkg.size_mb);
        }
    }
    report += &format!("\nВсего пакетов: {}\n", packages.len());
    let total_size: f64 = packages.iter().map(|p| p.size_mb).sum();
    report += &format!("Общий размер: {:.1} МБ\n", total_size);
    report += &format!("\n---\nОтчёт создан school-installer v{}\n", VERSION);
    report
}

fn export_report(format: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let _ = tx.send(LogEvent::Line("📄 Генерирую отчёт...".to_string()));
    let all_packages = get_installed_packages();
    let our_packages = filter_our_packages(&all_packages);
    let timestamp = chrono_now_approx();
    let content = format_txt_report(&our_packages, &timestamp);
    let report_dir = get_report_dir();
    let filename = format!("school-installer-report-{}.{}", timestamp, format);
    let filepath = Path::new(&report_dir).join(&filename);
    match fs::write(&filepath, &content) {
        Ok(_) => {
            let _ = tx.send(LogEvent::Line(format!("✅ Отчёт сохранён: {}", filepath.display())));
            (true, format!("Отчёт сохранён: {}", filepath.display()))
        }
        Err(e) => (false, format!("❌ Ошибка записи: {}", e)),
    }
}

// ==================== ДИСПЕТЧЕР КОМАНД ====================

fn run_command(cmd_id: &str, tx: &mpsc::Sender<LogEvent>) {
    log_to_file(&format!("=== Запуск команды: {} ===", cmd_id));

    let pkg_dir = get_school_packages_dir();
    let upd_dir = get_updates_dir();
    if pkg_dir == upd_dir {
        let _ = tx.send(LogEvent::Line("⚠ school_packages и updates указывают на одну папку!".to_string()));
    }

    ensure_directories();

    if let Some(free_mb) = get_free_space_mb(&pkg_dir) {
        if free_mb < MIN_FREE_SPACE_CRITICAL_MB {
            let _ = tx.send(LogEvent::Line(format!("❌ КРИТИЧЕСКИ МАЛО МЕСТА: свободно {}", format_size_mb(free_mb))));
        } else {
            let _ = tx.send(LogEvent::Line(format!("💾 Свободно на диске: {}", format_size_mb(free_mb))));
        }
    }

    let needs_prepare = matches!(cmd_id, "download_updates" | "install_updates" | "download_packages" | "install_packages" | "install_cryptopro_gia");
    if needs_prepare {
        let (ok, msg) = prepare_environment(tx);
        if !ok {
            let _ = tx.send(LogEvent::Done(false, format!("Подготовка окружения: {}", msg)));
            return;
        }
    }

    let download_dir = get_school_packages_dir();
    let updates_dir = get_updates_dir();
    let cert_dir = get_cert_dir();
    let crypto_dir = get_crypto_dir();
    let system_arch = detect_arch();

    let result: (bool, String) = match cmd_id {
        "setup_env" => prepare_environment(tx),
        "auto_proxy" => auto_setup_proxy(tx),
        "download_updates" => download_updates(&updates_dir, tx),
        "install_updates" => {
            if !Path::new(&updates_dir).exists() { (false, "Папка не найдена. Сначала скачайте обновления.".to_string()) }
            else { install_from_folder(&updates_dir, tx) }
        }
        "download_packages" => download_packages(&get_packages(), &download_dir, tx),
        "install_packages" => {
            if !Path::new(&download_dir).exists() { (false, "Папка не найдена. Сначала скачайте пакеты.".to_string()) }
            else { install_from_folder(&download_dir, tx) }
        }
        "install_cryptopro_gia" => install_cryptopro_gia(&crypto_dir, &download_dir, tx),
        "find_cert" => {
            let _ = tx.send(LogEvent::Line("🔍 Поиск сертификата ca-root.crt...".to_string()));
            if let Some(cert_path) = find_cert_on_mounted_devices() {
                let _ = tx.send(LogEvent::Line(format!("✅ Найден: {}", cert_path)));
                (true, format!("Сертификат найден: {}", cert_path))
            } else if let Some(device) = find_first_usb_device() {
                if let Some(mount_point) = mount_usb_device(&device, tx) {
                    let cert_path = Path::new(&mount_point).join(CERT_FILE_NAME);
                    if cert_path.exists() {
                        unmount_device(&mount_point, tx);
                        (true, format!("Сертификат найден на USB: {}", cert_path.display()))
                    } else {
                        unmount_device(&mount_point, tx);
                        (false, "Сертификат не найден на USB".to_string())
                    }
                } else { (false, "Не удалось смонтировать USB".to_string()) }
            } else {
                (false, "USB-устройства не найдены".to_string())
            }
        }
        "export_report" => export_report("txt", tx),
        "check_integrity" => {
            let (ok, msg, _) = verify_checksums(&download_dir, tx);
            (ok, msg)
        }
        "check_update" => {
            let (has_update, msg, new_version, url) = check_for_update(tx);
            if has_update {
                match (new_version, url) {
                    (Some(_v), Some(_u)) => {
                        let _ = tx.send(LogEvent::Line("   💡 Для применения: команда '🔁 Применить обновление'".to_string()));
                        (true, msg)
                    }
                    _ => (false, msg)
                }
            } else {
                (false, msg)
            }
        }
        "apply_update" => {
            let (has_update, _, new_version, url) = check_for_update(tx);
            if has_update {
                if let (Some(v), Some(u)) = (new_version, url) {
                    apply_update(&u, &v, tx)
                } else {
                    (false, "❌ Нет download_url для обновления".to_string())
                }
            } else {
                (false, "ℹ Обновлений нет".to_string())
            }
        }
        "detect_os" => {
            print_setup_instructions(tx);
            let mut info = format!("🖥 ОС: МОС ОС (ROSA) | Арх: {}", system_arch);
            info += &format!("\n  🌐 Прокси: {}", get_proxy_url());
            info += &format!("\n  📦 school_packages: {}", folder_info_line("", &download_dir, &system_arch));
            info += &format!("\n  🔄 updates: {}", folder_info_line("", &updates_dir, &system_arch));
            info += &format!("\n  🔐 cryptopro_gia: {}", folder_info_line("", &crypto_dir, &system_arch));
            info += &format!("\n  🔒 cer: {}", folder_info_line("", &cert_dir, &system_arch));
            if let Some(free_mb) = get_free_space_mb(&download_dir) {
                info += &format!("\n  💾 Свободно: {}", format_size_mb(free_mb));
            }
            info += &format!("\n\n  📝 Лог: {}", log_file_path());
            (true, info)
        }
        "exit" => (true, "Выход".to_string()),
        _ => (false, "Неизвестная команда".to_string()),
    };
    let _ = tx.send(LogEvent::Done(result.0, result.1));
    log_to_file(&format!("=== Команда {} завершена: {} ===", cmd_id, if result.0 { "УСПЕХ" } else { "ОШИБКА" }));
}

// ==================== TUI ====================

#[derive(Clone)]
struct CommandItem {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    icon: &'static str,
    hotkey: char,
}

const COMMANDS: &[CommandItem] = &[
    CommandItem { id: "detect_os", name: "Информация и инструкция", description: "Показывает структуру папок, размеры, свободное место и инструкцию.", icon: "ℹ", hotkey: '1' },
    CommandItem { id: "auto_proxy", name: "Автонастройка прокси", description: "Определяет прокси из окружения/gsettings и проверяет доступность.", icon: "⚙", hotkey: '9' },
    CommandItem { id: "setup_env", name: "Настроить прокси + сертификат", description: "Прокси в dnf.conf + сертификаты в системное хранилище и NSS.", icon: "🌐", hotkey: '2' },
    CommandItem { id: "find_cert", name: "Найти сертификат на USB", description: "Ищет ca-root.crt на смонтированных устройствах и USB.", icon: "🔍", hotkey: '8' },
    CommandItem { id: "download_updates", name: "Скачать обновления системы", description: "Скачивает обновления в updates/ с проверкой места.", icon: "⬇", hotkey: '3' },
    CommandItem { id: "install_updates", name: "Установить обновления", description: "Устанавливает из updates/ с проверкой целостности.", icon: "⬆", hotkey: '4' },
    CommandItem { id: "download_packages", name: "Скачать пакеты (с зависимостями)", description: "Скачивает task-koge + r7-office + notepadqq со всеми зависимостями для офлайн-установки.", icon: "📦", hotkey: '5' },
    CommandItem { id: "install_packages", name: "Установить пакеты", description: "Устанавливает из school_packages/ с проверкой SHA256.", icon: "🚀", hotkey: '6' },
    CommandItem { id: "install_cryptopro_gia", name: "Установить КриптоПро + ГИА", description: "5 шагов: newt → pcsc-lite → librtpkcs11ecp → КриптоПро → GIA11_PPE.", icon: "🔐", hotkey: '7' },
    CommandItem { id: "check_integrity", name: "Проверить целостность пакетов", description: "Сверяет SHA256 всех .rpm с checksums.sha256.", icon: "🧪", hotkey: 'c' },
    CommandItem { id: "check_update", name: "Проверить обновления", description: "Запрашивает GitHub Releases и сравнивает версии.", icon: "🔗", hotkey: 'u' },
    CommandItem { id: "apply_update", name: "Применить обновление", description: "Скачивает новую версию, проверяет SHA256, заменяет бинарник.", icon: "🔁", hotkey: 'r' },
    CommandItem { id: "export_report", name: "Экспорт отчёта (TXT)", description: "Создаёт TXT-отчёт об установленных пакетах.", icon: "📄", hotkey: 'e' },
    CommandItem { id: "exit", name: "Выход", description: "Завершить программу.", icon: "✕", hotkey: '0' },
];

fn needs_confirmation(cmd_id: &str) -> bool {
    matches!(cmd_id, "setup_env" | "download_updates" | "install_updates" | "download_packages" | "install_packages" | "install_cryptopro_gia" | "find_cert" | "auto_proxy" | "export_report" | "apply_update")
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default().direction(Direction::Vertical)
        .constraints([Constraint::Percentage((100 - percent_y) / 2), Constraint::Percentage(percent_y), Constraint::Percentage((100 - percent_y) / 2)]).split(r);
    Layout::default().direction(Direction::Horizontal)
        .constraints([Constraint::Percentage((100 - percent_x) / 2), Constraint::Percentage(percent_x), Constraint::Percentage((100 - percent_x) / 2)]).split(popup_layout[1])[1]
}

struct App {
    selected: usize,
    list_state: ListState,
    status: String,
    is_running: bool,
    arch: String,
    progress: f64,
    progress_label: String,
    log_lines: Vec<Line<'static>>,
    log_rx: Option<mpsc::Receiver<LogEvent>>,
    first_run_shown: bool,
    show_help: bool,
    pending_command: Option<usize>,
}

impl App {
    fn new() -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        let arch = detect_arch();
        ensure_directories();
        App {
            selected: 0, list_state,
            status: format!("Готов. v{} | Выберите ℹ для инструкции", VERSION),
            is_running: false, arch,
            progress: 0.0, progress_label: String::new(),
            log_lines: Vec::new(), log_rx: None,
            first_run_shown: false, show_help: false, pending_command: None,
        }
    }
    fn next(&mut self) {
        if self.is_running || self.pending_command.is_some() { return; }
        self.selected = if self.selected >= COMMANDS.len() - 1 { 0 } else { self.selected + 1 };
        self.list_state.select(Some(self.selected));
    }
    fn previous(&mut self) {
        if self.is_running || self.pending_command.is_some() { return; }
        self.selected = if self.selected == 0 { COMMANDS.len() - 1 } else { self.selected - 1 };
        self.list_state.select(Some(self.selected));
    }
    fn select_by_hotkey(&mut self, key: char) {
        if self.is_running || self.pending_command.is_some() { return; }
        for (i, cmd) in COMMANDS.iter().enumerate() {
            if cmd.hotkey == key {
                self.selected = i;
                self.list_state.select(Some(i));
                self.execute_selected();
                return;
            }
        }
    }
    fn execute_selected(&mut self) {
        if self.is_running || self.pending_command.is_some() { return; }
        let cmd_idx = self.selected;
        let cmd = COMMANDS[cmd_idx].clone();
        if needs_confirmation(cmd.id) {
            self.pending_command = Some(cmd_idx);
            self.status = format!("⚠️ Требуется подтверждение: {}", cmd.name);
        } else {
            self.start_command(cmd_idx);
        }
    }
    fn start_command(&mut self, idx: usize) {
        let cmd = COMMANDS[idx].clone();
        self.is_running = true;
        self.progress = 0.0;
        self.progress_label = format!("Выполняю: {}...", cmd.name);
        self.status = format!("▶ {}", cmd.name);
        self.log_lines.clear();
        self.pending_command = None;
        self.show_help = false;
        let (tx, rx) = mpsc::channel::<LogEvent>();
        let cmd_id = cmd.id.to_string();
        thread::spawn(move || { run_command(&cmd_id, &tx); });
        self.log_rx = Some(rx);
    }
    fn toggle_help(&mut self) {
        if self.is_running || self.pending_command.is_some() { return; }
        self.show_help = !self.show_help;
    }
    fn poll_events(&mut self) {
        let mut rx = self.log_rx.take();
        if let Some(receiver) = &rx {
            while let Ok(ev) = receiver.try_recv() {
                match ev {
                    LogEvent::Line(s) => {
                        self.log_lines.push(colorize_log_line(&s));
                        log_to_file(&s);
                        if self.log_lines.len() > 800 { self.log_lines.remove(0); }
                    }
                    LogEvent::Progress(p, label) => {
                        self.progress = p;
                        self.progress_label = label.clone();
                    }
                    LogEvent::Done(ok, msg) => {
                        self.progress = 1.0;
                        self.progress_label = String::new();
                        self.status = if ok { format!("✅ {}", msg) } else { format!("❌ {}", msg) };
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
    let chunks = Layout::default().direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(3), Constraint::Length(3)]).split(f.area());

    let header = Paragraph::new(vec![Line::from(vec![
        Span::styled(" ШКОЛЬНЫЙ УСТАНОВЩИК ПО ", Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(format!("ОС: МОС ОС | Арх: {} | v{}", app.arch, VERSION), Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
    ])]).block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)));
    f.render_widget(header, chunks[0]);

    let main_chunks = Layout::default().direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)]).split(chunks[1]);

    let items: Vec<ListItem> = COMMANDS.iter().enumerate().map(|(i, cmd)| {
        let style = if i == app.selected { Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD) }
                    else if app.is_running { Style::default().fg(Color::DarkGray) }
                    else { Style::default().fg(Color::White) };
        ListItem::new(Line::from(vec![
            Span::styled(format!("{} ", cmd.hotkey), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {} ", cmd.icon), style),
            Span::styled(cmd.name, style),
        ]))
    }).collect();

    let menu_title = if app.show_help { " ГОРЯЧИЕ КЛАВИШИ: 1-9,c,u,r,e=выбор, 0=выход, h=скрыть " }
                     else if app.pending_command.is_some() { " ⚠️ ОЖИДАНИЕ ПОДТВЕРЖДЕНИЯ: Y=да, N=отмена " }
                     else { " КОМАНДЫ (цифры+буквы: выбор, h: помощь, ↑↓, Enter, q) " };

    let commands_list = List::new(items).block(Block::default().title(menu_title).borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan))).highlight_symbol("▸ ");
    f.render_stateful_widget(commands_list, main_chunks[0], &mut app.list_state.clone());

    let right_chunks = Layout::default().direction(Direction::Vertical)
        .constraints([Constraint::Length(10), Constraint::Min(0)]).split(main_chunks[1]);

    if app.show_help {
        let help_text = vec![
            Line::from(Span::styled("⌨️  ГОРЯЧИЕ КЛАВИШИ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))),
            Line::from(""),
            Line::from("  1 — ℹ Информация и инструкция"),
            Line::from("  9 — ⚙ Автонастройка прокси"),
            Line::from("  2 — 🌐 Настроить прокси + сертификат"),
            Line::from("  8 — 🔍 Найти сертификат на USB"),
            Line::from("  3 — ⬇ Скачать обновления системы"),
            Line::from("  4 — ⬆ Установить обновления"),
            Line::from("  5 — 📦 Скачать пакеты (с зависимостями)"),
            Line::from("  6 — 🚀 Установить пакеты"),
            Line::from("  7 — 🔐 Установить КриптоПро + ГИА"),
            Line::from("  C — 🧪 Проверить целостность (SHA256)"),
            Line::from("  U — 🔗 Проверить обновления"),
            Line::from("  R — 🔁 Применить обновление"),
            Line::from("  E — 📄 Экспорт отчёта (TXT)"),
            Line::from("  0 — ✕ Выход"),
            Line::from(""),
            Line::from("  ↑/k ↓/j — навигация"),
            Line::from("  Enter — выполнить"),
            Line::from("  h — показать/скрыть справку"),
            Line::from("  q/Esc — выход"),
        ];
        let help_widget = Paragraph::new(help_text).block(Block::default().title(" СПРАВКА ").borders(Borders::ALL).border_style(Style::default().fg(Color::Yellow))).wrap(Wrap { trim: false });
        f.render_widget(help_widget, right_chunks[0]);
    } else {
        let selected_cmd = &COMMANDS[app.selected];
        let desc = Paragraph::new(vec![
            Line::from(vec![Span::styled("Команда: ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)), Span::styled(format!("[{}] {}", selected_cmd.hotkey, selected_cmd.name), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))]),
            Line::from(""),
            Line::from(selected_cmd.description),
        ]).block(Block::default().title(" ОПИСАНИЕ ").borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan))).wrap(Wrap { trim: false });
        f.render_widget(desc, right_chunks[0]);
    }

    let height = right_chunks[1].height.saturating_sub(2) as usize;
    let start = app.log_lines.len().saturating_sub(height);
    let visible_lines: Vec<Line> = app.log_lines[start..].to_vec();
    let log_widget = Paragraph::new(visible_lines).block(Block::default().title(" ЛОГ ").borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan))).wrap(Wrap { trim: false });
    f.render_widget(log_widget, right_chunks[1]);

    let progress_width = (app.progress * 100.0) as u16;
    let filled = "█".repeat(progress_width as usize);
    let empty = "░".repeat((100 - progress_width) as usize);
    let pct_color = if app.progress >= 1.0 { Color::Green } else { Color::Yellow };
    let progress_bar = Paragraph::new(vec![Line::from(vec![
        Span::styled(" Прогресс: ", Style::default().fg(Color::Cyan)),
        Span::styled(format!("[{}{}]", filled, empty), Style::default().fg(pct_color)),
        Span::styled(format!(" {:.0}%", app.progress * 100.0), Style::default().fg(pct_color).add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {}", app.progress_label), Style::default().fg(Color::White)),
    ])]).block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)));
    f.render_widget(progress_bar, chunks[2]);

    let status_color = if app.status.starts_with("✅") { Color::Green } else if app.status.starts_with("❌") { Color::Red } else if app.status.starts_with("▶") { Color::Yellow } else if app.status.starts_with("⚠") { Color::Yellow } else { Color::White };
    let status_widget = Paragraph::new(vec![Line::from(vec![
        Span::styled(" Статус: ", Style::default().fg(Color::Cyan)),
        Span::styled(app.status.as_str(), Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
    ])]).block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)));
    f.render_widget(status_widget, chunks[3]);

    if let Some(idx) = app.pending_command {
        let area = centered_rect(60, 30, f.area());
        f.render_widget(Clear, area);
        let block = Block::default().title(" ⚠️ ПОДТВЕРЖДЕНИЕ ДЕЙСТВИЯ ").borders(Borders::ALL).border_style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD));
        f.render_widget(block, area);
        let inner_area = area.inner(Margin { vertical: 2, horizontal: 3 });
        let text = vec![
            Line::from(""),
            Line::from(Span::styled("Вы собираетесь выполнить:", Style::default().fg(Color::White))),
            Line::from(""),
            Line::from(Span::styled(format!("  [{}] {}", COMMANDS[idx].hotkey, COMMANDS[idx].name), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))),
            Line::from(""),
            Line::from(Span::styled("Нажмите [Y] для подтверждения", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))),
            Line::from(Span::styled("Нажмите [N] или Esc для отмены", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))),
        ];
        let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
        f.render_widget(paragraph, inner_area);
    }
}

// ==================== ТОЧКА ВХОДА ====================

fn main() -> io::Result<()> {
    if !check_root() { require_root_and_exit(); }
    log_session_start();

    let args: Vec<String> = env::args().collect();

    if args.len() >= 2 {
        let mode = args[1].trim_start_matches("--");
        match mode {
            "set-proxy" => {
                if args.len() < 3 { println!("Использование: --set-proxy <URL>"); return Ok(()); }
                save_setting("PROXY_URL", &args[2])?;
                println!("✅ Прокси установлен: {}", args[2]);
                return Ok(());
            }
            "set-cert-dir" => {
                if args.len() < 3 { println!("Использование: --set-cert-dir <PATH>"); return Ok(()); }
                save_setting("CERT_DIR", &args[2])?;
                println!("✅ Папка сертификатов: {}", args[2]);
                return Ok(());
            }
            "set-crypto-dir" => {
                if args.len() < 3 { println!("Использование: --set-crypto-dir <PATH>"); return Ok(()); }
                save_setting("CRYPTO_DIR", &args[2])?;
                println!("✅ Папка КриптоПро: {}", args[2]);
                return Ok(());
            }
            "set-packages-dir" => {
                if args.len() < 3 { println!("Использование: --set-packages-dir <PATH>"); return Ok(()); }
                save_setting("SCHOOL_PACKAGES_DIR", &args[2])?;
                println!("✅ Папка пакетов: {}", args[2]);
                return Ok(());
            }
            "set-updates-dir" => {
                if args.len() < 3 { println!("Использование: --set-updates-dir <PATH>"); return Ok(()); }
                save_setting("UPDATES_DIR", &args[2])?;
                println!("✅ Папка обновлений: {}", args[2]);
                return Ok(());
            }
            "find-cert" => {
                println!("🔍 Поиск сертификата {}...", CERT_FILE_NAME);
                if let Some(cert_path) = find_cert_on_mounted_devices() {
                    println!("✅ Найден: {}", cert_path);
                } else {
                    println!("❌ Не найден");
                }
                return Ok(());
            }
            "auto-proxy" => {
                println!("⚙ Запуск автонастройки прокси...");
                let (tx, rx) = mpsc::channel::<LogEvent>();
                thread::spawn(move || { run_command("auto_proxy", &tx); });
                loop {
                    match rx.recv() {
                        Ok(LogEvent::Line(s)) => println!("{}", s),
                        Ok(LogEvent::Progress(p, l)) => println!("[{:>3.0}%] {}", p * 100.0, l),
                        Ok(LogEvent::Done(ok, msg)) => { println!("{}", msg); if !ok { std::process::exit(1); } break; }
                        Err(_) => break,
                    }
                }
                return Ok(());
            }
            "export-report" => {
                let format = if args.len() >= 3 { args[2].as_str() } else { "txt" };
                println!("📄 Генерирую отчёт в формате {}...", format);
                let (tx, rx) = mpsc::channel::<LogEvent>();
                let fmt = format.to_string();
                thread::spawn(move || { let _ = export_report(&fmt, &tx); });
                loop {
                    match rx.recv() {
                        Ok(LogEvent::Line(s)) => println!("{}", s),
                        Ok(LogEvent::Done(ok, msg)) => { println!("{}", msg); if !ok { std::process::exit(1); } break; }
                        Err(_) => break,
                        _ => {}
                    }
                }
                return Ok(());
            }
            "check-integrity" => {
                println!("🧪 Проверка целостности пакетов...");
                let (tx, rx) = mpsc::channel::<LogEvent>();
                thread::spawn(move || { run_command("check_integrity", &tx); });
                loop {
                    match rx.recv() {
                        Ok(LogEvent::Line(s)) => println!("{}", s),
                        Ok(LogEvent::Done(ok, msg)) => { println!("{}", msg); if !ok { std::process::exit(1); } break; }
                        Err(_) => break,
                        _ => {}
                    }
                }
                return Ok(());
            }
            "check-update" => {
                println!("🔗 Проверка обновлений...");
                let (tx, rx) = mpsc::channel::<LogEvent>();
                thread::spawn(move || { run_command("check_update", &tx); });
                loop {
                    match rx.recv() {
                        Ok(LogEvent::Line(s)) => println!("{}", s),
                        Ok(LogEvent::Done(_ok, msg)) => { println!("{}", msg); break; }
                        Err(_) => break,
                        _ => {}
                    }
                }
                return Ok(());
            }
            "apply-update" => {
                println!("🔁 Применение обновления...");
                let (tx, rx) = mpsc::channel::<LogEvent>();
                thread::spawn(move || { run_command("apply_update", &tx); });
                loop {
                    match rx.recv() {
                        Ok(LogEvent::Line(s)) => println!("{}", s),
                        Ok(LogEvent::Done(ok, msg)) => { println!("{}", msg); if !ok { std::process::exit(1); } break; }
                        Err(_) => break,
                        _ => {}
                    }
                }
                return Ok(());
            }
            "show-settings" => {
                println!("=== Текущие настройки ===\n");
                println!("Файл: {}", SETTINGS_FILE);
                println!("Прокси: {}", get_proxy_url());
                println!("Папка сертификатов: {}", get_cert_dir());
                println!("Папка КриптоПро: {}", get_crypto_dir());
                println!("Папка пакетов: {}", get_school_packages_dir());
                println!("Папка обновлений: {}", get_updates_dir());
                if let Some(free_mb) = get_free_space_mb(".") { println!("💾 Свободно на диске: {}", format_size_mb(free_mb)); }
                println!("\n📝 Лог: {}", log_file_path());
                println!("📦 Проверка целостности: {}", Path::new(&get_school_packages_dir()).join(CHECKSUM_FILE).display());
                println!("🔗 GitHub: {}/{}", GITHUB_REPO_OWNER, GITHUB_REPO_NAME);
                return Ok(());
            }
            "reset-settings" => {
                let _ = fs::remove_file(SETTINGS_FILE);
                println!("✅ Настройки сброшены");
                return Ok(());
            }
            _ => {}
        }
    }

    if args.len() > 1 {
        let mode = args[1].trim_start_matches("--");
        ensure_directories();
        let (tx, rx) = mpsc::channel::<LogEvent>();
        let cmd_id = match mode {
            "setup-env" => "setup_env", "download-updates" => "download_updates", "install-updates" => "install_updates",
            "download-packages" => "download_packages", "install-packages" => "install_packages", "install-crypto-gia" => "install_cryptopro_gia",
            "find-cert" => "find_cert", "auto-proxy" => "auto_proxy", "export-report" => "export_report",
            "check-integrity" => "check_integrity", "check-update" => "check_update", "apply-update" => "apply_update",
            _ => { println!("Неизвестная команда: {}", mode); return Ok(()); }
        };
        thread::spawn(move || run_command(&cmd_id, &tx));
        loop {
            match rx.recv() {
                Ok(LogEvent::Line(s)) => { println!("{}", s); log_to_file(&s); }
                Ok(LogEvent::Progress(p, l)) => { println!("[{:>3.0}%] {}", p * 100.0, l); }
                Ok(LogEvent::Done(ok, msg)) => { println!("{}", msg); if !ok { std::process::exit(1); } break; }
                Err(_) => break,
            }
        }
        return Ok(());
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut app = App::new();

    if !app.first_run_shown {
        app.first_run_shown = true;
        let (tx, rx) = mpsc::channel::<LogEvent>();
        thread::spawn(move || { run_command("detect_os", &tx); });
        app.log_rx = Some(rx);
    }

    loop {
        terminal.draw(|f| ui(f, &app))?;
        app.poll_events();
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') { break; }

                if let Some(pending_idx) = app.pending_command {
                    if let KeyCode::Char(c) = key.code {
                        match c {
                            'y' | 'Y' | 'н' | 'Н' => { app.start_command(pending_idx); }
                            'n' | 'N' | 'т' | 'Т' | 'q' | 'Q' => { app.pending_command = None; app.status = "❌ Отменено пользователем".to_string(); }
                            _ => {}
                        }
                    }
                    if key.code == KeyCode::Esc { app.pending_command = None; app.status = "❌ Отменено пользователем".to_string(); }
                    continue;
                }

                if let KeyCode::Char(c) = key.code {
                    if !app.is_running {
                        let lower = c.to_ascii_lowercase();
                        match lower {
                            '1' | '2' | '3' | '4' | '5' | '6' | '7' | '8' | '9' | '0' | 'c' | 'u' | 'r' | 'e' => {
                                app.select_by_hotkey(lower);
                                if lower == '0' { break; }
                                continue;
                            }
                            'h' => { app.toggle_help(); continue; }
                            _ => {}
                        }
                    }
                }

                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Up | KeyCode::Char('k') => app.previous(),
                    KeyCode::Down | KeyCode::Char('j') => app.next(),
                    KeyCode::Enter => {
                        if COMMANDS[app.selected].id == "exit" { break; }
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
    log_to_file("=== Сессия завершена ===");
    Ok(())
}
