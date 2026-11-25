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
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

// ==================== НАСТРОЙКИ ====================

const SETTINGS_FILE: &str = "school-installer.conf";
const LOG_FILE: &str = "school-installer.log";
const MAX_LOG_SIZE_BYTES: u64 = 5 * 1024 * 1024;
const CERT_FILE_NAME: &str = "ca-root.crt";
const PROXY_TEST_URL: &str = "http://ya.ru";
const PROXY_TEST_TIMEOUT_SECS: u64 = 5;

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
    log_to_file(&format!("ОС: МОС ОС | Архитектура: {}", detect_arch()));
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
    eprintln!("║                                                              ║");
    eprintln!("║  💡  Запустите через sudo:                                   ║");
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
                return (false, format!("❌ КРИТИЧЕСКИ МАЛО МЕСТА: свободно {} на {}. {} требует минимум {}. Освободите место!", format_size_mb(free_mb), check_path, op_name, format_size_mb(min_mb)));
            }
            if free_mb < min_mb {
                return (false, format!("⚠ Недостаточно места: свободно {} на {}. {} требует минимум {}. Освободите место или выберите другой диск.", format_size_mb(free_mb), check_path, op_name, format_size_mb(min_mb)));
            }
            (true, format!("✅ Свободно {} на {} (требуется {})", format_size_mb(free_mb), check_path, format_size_mb(min_mb)))
        }
        None => (true, format!("⚠ Не удалось определить свободное место на {}. Продолжаем с осторожностью.", check_path)),
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

// ==================== УТИЛИТЫ И ЦВЕТА ====================

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
    } else if t.starts_with("✅") || t.starts_with("[OK]") || t.contains("успешно") || t.contains("скачан") || t.contains("готово") || t.contains("Обновлено") || t.contains("Найден") {
        Color::Green
    } else if t.starts_with("❌") || t.starts_with("[X]") || t.contains("Ошибка") || t.contains("не удалось") || t.contains("не найден") {
        Color::Red
    } else if t.starts_with("⚠") || t.starts_with("[!]") || t.contains("ВНИМАНИЕ") || t.contains("пропускаем") {
        Color::Yellow
    } else if t.starts_with("📊") || t.starts_with("📥") || t.starts_with("💾") || t.starts_with("🔒") || t.starts_with("🌐") || t.starts_with("🔐") || t.starts_with("📦") || t.starts_with("🚀") || t.starts_with("🔑") || t.starts_with("🔍") || t.starts_with("⚙") {
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
                    log_to_file(&format!("[PROGRESS] {}/{} {}", n, m, pkg));
                } else {
                    let _ = tx_err.send(LogEvent::Line(cleaned.clone()));
                    log_to_file(&cleaned);
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
            let _ = tx.send(LogEvent::Line(cleaned.clone()));
            log_to_file(&cleaned);
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

    let dirs_with_readme: Vec<(String, &str)> = vec![
        (base_packages.clone(), "📦 ПАКЕТЫ ДЛЯ МОС ОС (ROSA)\n========================================\nСюда автоматически скачиваются пакеты для установки.\n\nОсновные пакеты:\n- task-koge-*.rpm        (метапакет для ОГЭ/ЕГЭ по информатике)\n- r7-office-*.rpm        (или myoffice-education-*.rpm)\n- notepadqq-*.rpm        (редактор кода)\n\nКак получить:\n1. Запустите на машине с МОС ОС и интернетом:\n   sudo ./school-installer --download-packages\n2. Пакеты автоматически скачаются в эту папку\n3. Скопируйте папку school_packages/ на флешку"),
        (base_updates.clone(), "🔄 ОБНОВЛЕНИЯ ДЛЯ МОС ОС (ROSA)\n========================================\nСюда автоматически скачиваются обновления системы.\n\nКак получить:\nsudo ./school-installer --download-updates\n\nУстановка в классе:\nsudo ./school-installer --install-updates"),
        (format!("{}/linux-amd64", crypto_dir), "🔐 КРИПТОПРО CSP — ДИСТРИБУТИВ\n========================================\nСюда положите дистрибутив КриптоПро CSP.\n\nЧто сюда положить:\n1. Распакуйте архив КриптоПро CSP (например, linux-amd64.tgz)\n2. Скопируйте ВСЕ файлы из распакованной папки сюда:\n   - install.sh              (ОБЯЗАТЕЛЬНО)\n   - uninstall.sh            (ОБЯЗАТЕЛЬНО)\n   - lsb-cprocsp-base-*.rpm\n   - lsb-cprocsp-rdr-*.rpm\n   - lsb-cprocsp-kc1-*.rpm   (или kc2, если нужен)\n   - lsb-cprocsp-capilite-*.rpm\n   - cprocsp-curl-*.rpm\n   - lsb-cprocsp-ca-certs-*.rpm\n\nВАЖНО: install.sh сам выберет правильный менеджер пакетов.\n\nКонфигурация: kc1 (по умолчанию) — для экзаменов.\nkc2 используйте только если точно знаете, зачем."),
        (crypto_dir.clone(), "🔑 КРИПТОПРО + ГИА — ДОПОЛНИТЕЛЬНЫЕ ПАКЕТЫ\n========================================\nКорень папки cryptopro_gia/ — сюда кладите:\n\n1. newt-*.rpm\n   — нужен для install.sh КриптоПро\n\n2. pcsc-lite-*.rpm и pcsc-lite-ccid-*.rpm\n   — демон смарт-карт и драйвер USB-ридеров\n\n3. librtpkcs11ecp-*.rpm\n   — библиотека для токенов Рутокен/Jacarta\n\n4. GIA11_PPE-*.rpm\n   — платформа проведения экзамена (ОБЯЗАТЕЛЬНО)\n\nЕсли какого-то пакета нет — программа скачает его\nиз репозитория (нужен интернет через прокси)."),
        (cert_dir, "🔒 СЕРТИФИКАТЫ\n========================================\nСюда положите корневые сертификаты (.crt, .pem, .cer)\nдля прокси и внутренних сайтов школы.\n\nОбычно это файл ca-root.crt от вашего прокси-сервера.\n\nПрограмма установит сертификаты:\n1. В системное хранилище (update-ca-trust)\n2. В NSS-базу Chromium (для браузера)\n\nВАЖНО: не кладите файлы, начинающиеся с ._ (это\nmacOS-артефакты, они не являются сертификатами)."),
    ];

    for (dir, readme_content) in &dirs_with_readme {
        let path = Path::new(dir);
        if !path.exists() {
            let _ = fs::create_dir_all(path);
            let _ = fs::write(path.join("README.txt"), *readme_content);
        }
    }
}

fn print_setup_instructions(tx: &mpsc::Sender<LogEvent>) {
    let base_packages = get_school_packages_dir();
    let base_updates = get_updates_dir();
    let crypto_dir = get_crypto_dir();
    let cert_dir = get_cert_dir();

    let _ = tx.send(LogEvent::Line("========================================".to_string()));
    let _ = tx.send(LogEvent::Line("  📋 ИНСТРУКЦИЯ ПО ПОДГОТОВКЕ ФЛЕШКИ".to_string()));
    let _ = tx.send(LogEvent::Line("========================================".to_string()));
    let _ = tx.send(LogEvent::Line(String::new()));
    let _ = tx.send(LogEvent::Line(format!("🖥 ОС: МОС ОС (ROSA) | Архитектура: {}", detect_arch())));
    let _ = tx.send(LogEvent::Line(String::new()));

    if let Some(free_mb) = get_free_space_mb(&base_packages) {
        let _ = tx.send(LogEvent::Line(format!("💾 Свободно на диске: {}", format_size_mb(free_mb))));
        let _ = tx.send(LogEvent::Line(String::new()));
    }

    let _ = tx.send(LogEvent::Line("📦 1. school_packages/ — пакеты для установки".to_string()));
    let _ = tx.send(LogEvent::Line(format!("   ├── task-koge, r7-office, notepadqq ({})", Path::new(&base_packages).exists())));
    let _ = tx.send(LogEvent::Line(String::new()));

    let _ = tx.send(LogEvent::Line("🔄 2. updates/ — обновления системы".to_string()));
    let _ = tx.send(LogEvent::Line(format!("   ├── обновления МОС ОС ({})", Path::new(&base_updates).exists())));
    let _ = tx.send(LogEvent::Line(String::new()));

    let _ = tx.send(LogEvent::Line("🔐 3. cryptopro_gia/ — КриптоПро + ГИА".to_string()));
    let _ = tx.send(LogEvent::Line(format!("   ├── linux-amd64/  — дистрибутив КриптоПро ({})", Path::new(&format!("{}/linux-amd64", crypto_dir)).exists())));
    let _ = tx.send(LogEvent::Line("   │   ├── install.sh        (ОБЯЗАТЕЛЬНО)".to_string()));
    let _ = tx.send(LogEvent::Line("   │   ├── uninstall.sh      (ОБЯЗАТЕЛЬНО)".to_string()));
    let _ = tx.send(LogEvent::Line("   │   └── *.rpm             (все из архива КриптоПро)".to_string()));
    let _ = tx.send(LogEvent::Line("   ├── newt-*.rpm            (нужен для install.sh)".to_string()));
    let _ = tx.send(LogEvent::Line("   ├── pcsc-lite-*.rpm       (демон смарт-карт)".to_string()));
    let _ = tx.send(LogEvent::Line("   ├── librtpkcs11ecp-*.rpm  (токены Рутокен/Jacarta)".to_string()));
    let _ = tx.send(LogEvent::Line("   └── GIA11_PPE-*.rpm       (платформа экзамена)".to_string()));
    let _ = tx.send(LogEvent::Line(String::new()));

    let _ = tx.send(LogEvent::Line("🔒 4. cer/ — корневые сертификаты".to_string()));
    let _ = tx.send(LogEvent::Line(format!("   └── ca-root.crt     ({})", Path::new(&cert_dir).exists())));
    let _ = tx.send(LogEvent::Line(String::new()));

    let _ = tx.send(LogEvent::Line("========================================".to_string()));
    let _ = tx.send(LogEvent::Line("  🚀 БЫСТРЫЙ СТАРТ".to_string()));
    let _ = tx.send(LogEvent::Line("========================================".to_string()));
    let _ = tx.send(LogEvent::Line(String::new()));
    let _ = tx.send(LogEvent::Line("На машине с ИНТЕРНЕТОМ:".to_string()));
    let _ = tx.send(LogEvent::Line("  1. sudo ./school-installer --set-proxy <URL>".to_string()));
    let _ = tx.send(LogEvent::Line("  2. Положите ca-root.crt в папку cer/".to_string()));
    let _ = tx.send(LogEvent::Line("  3. sudo ./school-installer".to_string()));
    let _ = tx.send(LogEvent::Line("     -> 🌐 Настроить прокси + сертификат".to_string()));
    let _ = tx.send(LogEvent::Line("     -> 📦 Скачать пакеты".to_string()));
    let _ = tx.send(LogEvent::Line("  4. Скопируйте на флешку:".to_string()));
    let _ = tx.send(LogEvent::Line("     school-installer + school_packages/ + cryptopro_gia/ + cer/".to_string()));
    let _ = tx.send(LogEvent::Line(String::new()));
    let _ = tx.send(LogEvent::Line("В КЛАССЕ (без интернета):".to_string()));
    let _ = tx.send(LogEvent::Line("  1. sudo ./school-installer".to_string()));
    let _ = tx.send(LogEvent::Line("     -> 🚀 Установить пакеты".to_string()));
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
            if name == "README.txt" { continue; }
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
            if name == "README.txt" { continue; }
            if !name.ends_with(".rpm") { continue; }
            if !substring.is_empty() && !name.to_lowercase().contains(&substring.to_lowercase()) { continue; }
            if !is_package_for_arch(&name, system_arch) { continue; }
            result.push(path.to_string_lossy().to_string());
        }
    }
    result
}

// ==================== УЛУЧШЕНИЕ №7: ПОИСК СЕРТИФИКАТА НА USB ====================

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

fn unmount_device(mount_point: &str, tx: &mpsc::Sender<LogEvent>) {
    let _ = tx.send(LogEvent::Line("   Размонтирую флешку...".to_string()));
    let _ = Command::new("umount").arg(mount_point).status();
}

// ==================== УЛУЧШЕНИЕ №8: АВТОНАСТРОЙКА ПРОКСИ ====================

/// Проверяет доступность прокси через curl с таймаутом
fn test_proxy_connectivity(proxy_url: &str, tx: &mpsc::Sender<LogEvent>) -> bool {
    let _ = tx.send(LogEvent::Line(format!("   🌐 Проверяю доступность {} через {}...", PROXY_TEST_URL, proxy_url)));
    log_to_file(&format!("Тест прокси: {} -> {}", proxy_url, PROXY_TEST_URL));

    let mut cmd = Command::new("curl");
    cmd.arg("-s").arg("-o").arg("/dev/null").arg("-w").arg("%{http_code}")
        .arg("--max-time").arg(PROXY_TEST_TIMEOUT_SECS.to_string())
        .arg("--proxy").arg(proxy_url)
        .arg(PROXY_TEST_URL);

    match cmd.output() {
        Ok(out) => {
            let code = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let success = code == "200" || code == "301" || code == "302";
            if success {
                let _ = tx.send(LogEvent::Line(format!("   ✅ Прокси работает (HTTP {})", code)));
                log_to_file(&format!("Прокси работает: HTTP {}", code));
            } else {
                let _ = tx.send(LogEvent::Line(format!("   ⚠ Прокси ответил HTTP {}, возможно недоступен", code)));
                log_to_file(&format!("Прокси ответил: HTTP {}", code));
            }
            success
        }
        Err(e) => {
            let _ = tx.send(LogEvent::Line(format!("   ❌ Не удалось проверить прокси: {}", e)));
            log_to_file(&format!("Ошибка проверки прокси: {}", e));
            false
        }
    }
}

/// Проверяет, работает ли прокси БЕЗ явного указания (через системные настройки)
fn test_system_proxy(tx: &mpsc::Sender<LogEvent>) -> bool {
    let _ = tx.send(LogEvent::Line("   🌐 Проверяю системный прокси...".to_string()));
    let mut cmd = Command::new("curl");
    cmd.arg("-s").arg("-o").arg("/dev/null").arg("-w").arg("%{http_code}")
        .arg("--max-time").arg(PROXY_TEST_TIMEOUT_SECS.to_string())
        .arg(PROXY_TEST_URL);

    match cmd.output() {
        Ok(out) => {
            let code = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let success = code == "200" || code == "301" || code == "302";
            if success {
                let _ = tx.send(LogEvent::Line("   ✅ Системный прокси работает".to_string()));
            } else {
                let _ = tx.send(LogEvent::Line(format!("   ⚠ Системный прокси не работает (HTTP {})", code)));
            }
            success
        }
        Err(_) => false,
    }
}

/// Пытается автоматически определить прокси из окружения
fn detect_proxy_from_env() -> Option<String> {
    for var in &["http_proxy", "HTTP_PROXY", "https_proxy", "HTTPS_PROXY"] {
        if let Ok(val) = env::var(var) {
            if !val.is_empty() && val.starts_with("http") {
                return Some(val);
            }
        }
    }
    None
}

/// Пытается определить прокси через gsettings (GNOME/ROSA)
fn detect_proxy_from_gsettings() -> Option<String> {
    let output = Command::new("gsettings")
        .arg("get").arg("org.gnome.system.proxy.http").arg("host")
        .stdout(Stdio::piped()).stderr(Stdio::null()).output();
    if let Ok(out) = output {
        let host = String::from_utf8_lossy(&out.stdout).trim().trim_matches('\'').to_string();
        if !host.is_empty() && host != "''" {
            let port_output = Command::new("gsettings")
                .arg("get").arg("org.gnome.system.proxy.http").arg("port")
                .stdout(Stdio::piped()).stderr(Stdio::null()).output();
            if let Ok(port_out) = port_output {
                let port = String::from_utf8_lossy(&port_out.stdout).trim();
                if !port.is_empty() && port != "0" {
                    return Some(format!("http://{}:{}", host, port));
                }
            }
        }
    }
    None
}

/// Пытается определить прокси через wpad (автоматическое обнаружение)
fn detect_proxy_from_wpad() -> Option<String> {
    // Пробуем получить через curl wpad.dat
    let output = Command::new("curl")
        .arg("-s").arg("--max-time").arg("3")
        .arg("http://wpad/wpad.dat")
        .stdout(Stdio::piped()).stderr(Stdio::null()).output();
    if let Ok(out) = output {
        let content = String::from_utf8_lossy(&out.stdout);
        // Ищем первый PROXY в файле
        for line in content.lines() {
            if line.contains("PROXY") {
                if let Some(start) = line.find("PROXY") {
                    let rest = &line[start + 5..].trim();
                    if let Some(end) = rest.find(';') {
                        return Some(format!("http://{}", rest[..end].trim()));
                    }
                }
            }
        }
    }
    None
}

/// Главная функция автонастройки прокси
fn auto_setup_proxy(tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let _ = tx.send(LogEvent::Line("=== ⚙ Автонастройка прокси ===".to_string()));
    log_to_file("Запуск автонастройки прокси");

    // 1. Проверяем, уже ли настроен прокси
    let profile_path = "/etc/profile.d/proxy.sh";
    let dnf_conf = "/etc/dnf/dnf.conf";
    let profile_ok = fs::read_to_string(profile_path)
        .map(|c| c.contains("proxy=")).unwrap_or(false);
    let dnf_ok = fs::read_to_string(dnf_conf)
        .map(|c| c.contains("proxy=")).unwrap_or(false);

    if profile_ok && dnf_ok {
        let current_proxy = get_proxy_url();
        let _ = tx.send(LogEvent::Line(format!("   ℹ Прокси уже настроен: {}", current_proxy)));
        if test_proxy_connectivity(&current_proxy, tx) {
            return (true, format!("Прокси {} уже настроен и работает", current_proxy));
        }
        let _ = tx.send(LogEvent::Line("   ⚠ Настроенный прокси не отвечает. Пробую найти другой...".to_string()));
    }

    // 2. Пробуем системный прокси (без явного указания)
    if test_system_proxy(tx) {
        let _ = tx.send(LogEvent::Line("   ✅ Интернет работает через системный прокси".to_string()));
        // Пытаемся определить URL системного прокси
        if let Some(proxy) = detect_proxy_from_gsettings() {
            let _ = tx.send(LogEvent::Line(format!("   ℹ Найден в gsettings: {}", proxy)));
            if test_proxy_connectivity(&proxy, tx) {
                // Сохраняем в настройки
                let _ = save_setting("PROXY_URL", &proxy);
                let _ = tx.send(LogEvent::Line(format!("   ✅ Сохранён в настройки: {}", proxy)));
                return (true, format!("Автонастройка: прокси {} найден и сохранён", proxy));
            }
        }
        // Если системный прокси работает, но мы не можем определить URL — оставляем как есть
        return (true, "Системный прокси работает, настройка не требуется".to_string());
    }

    // 3. Ищем прокси в переменных окружения
    if let Some(proxy) = detect_proxy_from_env() {
        let _ = tx.send(LogEvent::Line(format!("   ℹ Найден в переменных окружения: {}", proxy)));
        if test_proxy_connectivity(&proxy, tx) {
            let _ = save_setting("PROXY_URL", &proxy);
            let _ = tx.send(LogEvent::Line(format!("   ✅ Сохранён в настройки: {}", proxy)));
            return (true, format!("Автонастройка: прокси {} найден в окружении", proxy));
        }
    }

    // 4. Пробуем WPAD
    if let Some(proxy) = detect_proxy_from_wpad() {
        let _ = tx.send(LogEvent::Line(format!("   ℹ Найден через WPAD: {}", proxy)));
        if test_proxy_connectivity(&proxy, tx) {
            let _ = save_setting("PROXY_URL", &proxy);
            let _ = tx.send(LogEvent::Line(format!("   ✅ Сохранён в настройки: {}", proxy)));
            return (true, format!("Автонастройка: прокси {} найден через WPAD", proxy));
        }
    }

    // 5. Пробуем дефолтный прокси школы
    let default_proxy = get_proxy_url();
    if default_proxy != DEFAULT_PROXY_URL || test_proxy_connectivity(&default_proxy, tx) {
        if default_proxy != DEFAULT_PROXY_URL {
            let _ = tx.send(LogEvent::Line(format!("   ℹ Пробую дефолтный прокси школы: {}", default_proxy)));
        }
        if test_proxy_connectivity(&default_proxy, tx) {
            let _ = save_setting("PROXY_URL", &default_proxy);
            return (true, format!("Автонастройка: прокси {} работает", default_proxy));
        }
    }

    // 6. Ничего не找到了 — предлагаем пользователю ввести вручную
    let _ = tx.send(LogEvent::Line("   ❌ Не удалось автоматически определить прокси".to_string()));
    let _ = tx.send(LogEvent::Line("   💡 Используйте: sudo ./school-installer --set-proxy <URL>".to_string()));
    let _ = tx.send(LogEvent::Line("   💡 Или в TUI: выберите пункт '⚙ Автонастройка прокси' и введите URL".to_string()));
    log_to_file("Автонастройка прокси не удалась");
    (false, "Не удалось автоматически определить прокси. Укажите вручную.".to_string())
}

// ==================== ШАГ 1: ПРОКСИ (ручная настройка) ====================

fn setup_proxy(tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let proxy_url = get_proxy_url();
    let profile_path = "/etc/profile.d/proxy.sh";
    let profile_ok = fs::read_to_string(profile_path).map(|c| c.contains(&proxy_url)).unwrap_or(false);
    let dnf_conf_ok = fs::read_to_string("/etc/dnf/dnf.conf").map(|c| c.contains(&format!("proxy={}", proxy_url))).unwrap_or(false);
    let already_configured = profile_ok && dnf_conf_ok;

    if already_configured {
        let _ = tx.send(LogEvent::Line(format!("✅ Прокси уже настроен ({})", proxy_url)));
        return (true, "Прокси уже настроен".to_string());
    }

    let _ = tx.send(LogEvent::Line(format!("=== 🌐 Настройка прокси {} ===", proxy_url)));
    let profile_content = format!("# Автоматическая настройка прокси (school-installer)\nexport http_proxy=\"{u}\"\nexport https_proxy=\"{u}\"\nexport no_proxy=\"localhost,127.0.0.1,::1\"\n", u = proxy_url);
    if let Err(e) = fs::write(profile_path, &profile_content) {
        return (false, format!("❌ Не удалось записать {}: {} (нужен root/sudo)", profile_path, e));
    }
    let _ = Command::new("chmod").arg("644").arg(profile_path).status();
    let _ = tx.send(LogEvent::Line(format!("✅ {} создан", profile_path)));

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

    // УЛУЧШЕНИЕ №8: проверяем работоспособность после настройки
    if test_proxy_connectivity(&proxy_url, tx) {
        (true, format!("Прокси {} настроен и работает", proxy_url))
    } else {
        (true, format!("Прокси {} настроен, но не отвечает (возможно, нет интернета)", proxy_url))
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
        let _ = tx.send(LogEvent::Line("🔍 Сертификат не найден в папке cer/ — ищу на USB-устройствах...".to_string()));
        log_to_file("Поиск сертификата на USB-устройствах...");

        if let Some(cert_path) = find_cert_on_mounted_devices() {
            let _ = tx.send(LogEvent::Line(format!("   ✅ Найден на смонтированном устройстве: {}", cert_path)));
            certs.push(cert_path);
        } else {
            let _ = tx.send(LogEvent::Line("   На смонтированных устройствах не найден. Ищу USB...".to_string()));
            if let Some(device) = find_first_usb_device() {
                let _ = tx.send(LogEvent::Line(format!("   Найдено USB-устройство: {}", device)));
                if let Some(mount_point) = mount_usb_device(&device, tx) {
                    let cert_path = Path::new(&mount_point).join(CERT_FILE_NAME);
                    if cert_path.exists() {
                        let _ = tx.send(LogEvent::Line(format!("   ✅ Найден на флешке: {}", cert_path.display())));
                        certs.push(cert_path.to_string_lossy().to_string());
                        unmount_device(&mount_point, tx);
                    }
                }
            } else {
                let _ = tx.send(LogEvent::Line("   ⚠ USB-устройства не найдены. Вставьте флешку с сертификатом.".to_string()));
            }
        }
    }

    if certs.is_empty() {
        let _ = tx.send(LogEvent::Line(format!("⚠ В '{}' и на USB нет валидных .crt/.pem — пропускаем", cert_dir)));
        return (true, "Сертификат пропущен (нет валидных файлов)".to_string());
    }

    let anchors_dir = "/etc/pki/ca-trust/source/anchors";
    let mut all_in_anchors = true;
    let mut all_in_nss = true;
    for cert in &certs {
        let name = Path::new(cert).file_name().unwrap_or_default().to_string_lossy().to_string();
        if !Path::new(&format!("{}/{}", anchors_dir, name)).exists() { all_in_anchors = false; }
        let nick = Path::new(cert).file_stem().unwrap_or_default().to_string_lossy().to_string();
        let (home, _) = real_home_and_user();
        let nssdb = format!("sql:{}/.pki/nssdb", home);
        let check_cmd = format!("echo '' | certutil -d '{}' -L 2>/dev/null | grep -q '{}'", nssdb, nick);
        let in_nss = Command::new("sh").arg("-c").arg(&check_cmd).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
        if !in_nss { all_in_nss = false; }
    }

    if all_in_anchors && all_in_nss {
        let _ = tx.send(LogEvent::Line(format!("✅ Все {} сертификат(ов) уже установлены", certs.len())));
        return (true, "Сертификаты уже установлены".to_string());
    }

    let _ = tx.send(LogEvent::Line(format!("=== Установка {} сертификат(ов) ===", certs.len())));
    let _ = fs::create_dir_all(anchors_dir);
    for cert in &certs {
        let name = Path::new(cert).file_name().unwrap_or_default().to_string_lossy().to_string();
        let dst = format!("{}/{}", anchors_dir, name);
        if !Path::new(&dst).exists() {
            if let Err(e) = fs::copy(cert, &dst) {
                return (false, format!("❌ Не удалось скопировать {} в {}: {}", cert, dst, e));
            }
            let _ = tx.send(LogEvent::Line(format!("   → {}", dst)));
        } else {
            let _ = tx.send(LogEvent::Line(format!("   ✅ {} уже на месте", name)));
        }
    }

    if command_exists("update-ca-trust") {
        let _ = tx.send(LogEvent::Line("Запускаю update-ca-trust...".to_string()));
        let cmd = Command::new("update-ca-trust");
        match run_process(cmd, tx) {
            Ok(s) if s.success() => { let _ = tx.send(LogEvent::Line("✅ Системное хранилище обновлено".to_string())); }
            Ok(_s) => return (false, "update-ca-trust завершился с ошибкой".to_string()),
            Err(e) => return (false, format!("❌ Не удалось запустить update-ca-trust: {}", e)),
        }
    }

    let (home, real_user) = real_home_and_user();
    let nssdb_path = format!("{}/.pki/nssdb", home);
    let nssdb = format!("sql:{}", nssdb_path);

    if !command_exists("certutil") {
        let mut ok = false;
        for pkg in ["nss-tools", "libnss3-tools"] {
            let mut cmd = Command::new("dnf");
            cmd.arg("install").arg("-y").arg(pkg);
            if let Ok(s) = run_process(cmd, tx) { if s.success() { ok = true; break; } }
        }
        if !ok {
            let _ = tx.send(LogEvent::Line("⚠ Не удалось установить nss-tools — NSS пропущен".to_string()));
            return (true, "Сертификат в системное хранилище (NSS пропущен)".to_string());
        }
    }

    let _ = fs::create_dir_all(&nssdb_path);
    let db_exists = Path::new(&format!("{}/cert9.db", nssdb_path)).exists() || Path::new(&format!("{}/cert8.db", nssdb_path)).exists();
    if !db_exists {
        let mut init = Command::new("certutil");
        init.arg("-d").arg(&nssdb).arg("-N").arg("--empty-password");
        let _ = init.stdout(Stdio::null()).stderr(Stdio::null()).status();
    }

    for cert in &certs {
        let nick = Path::new(cert).file_stem().unwrap_or_default().to_string_lossy().to_string();
        let check_cmd = format!("echo '' | certutil -d '{}' -L 2>/dev/null | grep -q '{}'", nssdb, nick);
        let already_in_nss = Command::new("sh").arg("-c").arg(&check_cmd).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
        if already_in_nss {
            let _ = tx.send(LogEvent::Line(format!("   ✅ NSS: {} уже установлен", nick)));
            continue;
        }
        let add_cmd = format!("echo '' | certutil -d '{}' -A -t 'C,,' -n '{}' -i '{}'", nssdb, nick, cert);
        let _ = tx.send(LogEvent::Line(format!("Добавляю в NSS: {}...", nick)));
        match Command::new("sh").arg("-c").arg(&add_cmd).stdout(Stdio::piped()).stderr(Stdio::piped()).status() {
            Ok(s) if s.success() => { let _ = tx.send(LogEvent::Line(format!("   ✅ NSS: {}", nick))); }
            Ok(_s) => { let _ = tx.send(LogEvent::Line(format!("   ⚠ NSS не добавлен: {}", nick))); }
            Err(e) => { let _ = tx.send(LogEvent::Line(format!("   ❌ NSS ошибка: {}", e))); }
        }
    }

    if let Some(user) = real_user {
        let mut ch = Command::new("chown");
        ch.arg("-R").arg(format!("{}:{}", user, user)).arg(&nssdb_path);
        let _ = ch.status();
        let _ = tx.send(LogEvent::Line(format!("   ✅ chown .pki → {}", user)));
    }

    (true, format!("Сертификатов установлено: {}", certs.len()))
}

fn prepare_environment(tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    // УЛУЧШЕНИЕ №8: сначала пытаемся автонастроить прокси
    let (auto_ok, auto_msg) = auto_setup_proxy(tx);
    if !auto_ok {
        let _ = tx.send(LogEvent::Line(format!("⚠ Автонастройка прокси: {}", auto_msg)));
        // Пробуем ручную настройку с текущим значением из конфига
        let (ok2, m2) = setup_proxy(tx);
        if !ok2 { return (false, format!("Прокси: {}", m2)); }
    }
    let cert_dir = get_cert_dir();
    let (ok2, m2) = install_cert(&cert_dir, tx);
    if !ok2 { return (false, format!("Сертификат: {}", m2)); }
    (true, format!("Прокси ✅ | {}", m2))
}

// ==================== СКАЧИВАНИЕ ====================

fn download_updates(download_dir: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let _ = fs::create_dir_all(download_dir);
    let (space_ok, space_msg) = check_free_space(download_dir, MIN_FREE_SPACE_UPDATES_MB, "Скачивание обновлений");
    let _ = tx.send(LogEvent::Line(space_msg.clone()));
    if !space_ok { return (false, space_msg); }

    let estimated_size = estimate_dnf_download_size(&["upgrade", "--downloadonly", "--exclude=*.i686", "--exclude=*.i586", "--exclude=*.i386"]);
    if let Some(size_mb) = estimated_size {
        let _ = tx.send(LogEvent::Line(format!("📊 Примерный размер скачивания: {}", format_size_mb(size_mb))));
        if size_mb > 0 {
            let (enough, msg) = check_free_space(download_dir, size_mb + 100, "Скачивание обновлений (с запасом 100 МБ)");
            let _ = tx.send(LogEvent::Line(msg));
            if !enough { return (false, format!("❌ Недостаточно места. Освободите {} и повторите.", format_size_mb(size_mb + 100))); }
        }
    }

    let abs_path = fs::canonicalize(download_dir).unwrap_or_else(|_| Path::new(download_dir).to_path_buf());
    let _ = tx.send(LogEvent::Line(format!("📥 Скачиваю обновления в: {}", abs_path.display())));

    let mut cmd = Command::new("dnf");
    cmd.arg("upgrade").arg("--downloadonly").arg("--exclude=*.i686").arg("--exclude=*.i586").arg("--exclude=*.i386").arg(format!("--destdir={}", abs_path.display())).arg("-y");

    match run_process(cmd, tx) {
        Ok(s) if s.success() => {
            let rpm_count = find_packages_flat(download_dir, "", &detect_arch()).len();
            if rpm_count > 0 { (true, format!("Обновления скачаны ({} пакетов)", rpm_count)) }
            else { (false, "dnf отработал, но файлы не найдены в папке. Проверьте /var/cache/dnf/".to_string()) }
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

    let mut dnf_args: Vec<String> = vec!["download".to_string(), "--resolve".to_string(), "--alldeps".to_string(), "--exclude=*.i686".to_string(), "--exclude=*.i586".to_string(), "--exclude=*.i386".to_string()];
    for pkg in packages { dnf_args.push(pkg.to_string()); }
    let estimated_size = estimate_dnf_download_size(&dnf_args.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    if let Some(size_mb) = estimated_size {
        let _ = tx.send(LogEvent::Line(format!("📊 Примерный размер скачивания: {}", format_size_mb(size_mb))));
        let (enough, msg) = check_free_space(download_dir, size_mb + 100, "Скачивание пакетов (с запасом 100 МБ)");
        let _ = tx.send(LogEvent::Line(msg));
        if !enough { return (false, format!("❌ Недостаточно места для скачивания пакетов. Освободите {} и повторите.", format_size_mb(size_mb + 100))); }
    }

    let mut cmd = Command::new("dnf");
    cmd.arg("download").arg("--resolve").arg("--alldeps").arg("--exclude=*.i686").arg("--exclude=*.i586").arg("--exclude=*.i386").arg(format!("--destdir={}", download_dir));
    for pkg in packages { cmd.arg(pkg); }
    match run_process(cmd, tx) {
        Ok(s) if s.success() => (true, "Пакеты скачаны".to_string()),
        Ok(_s) => (false, "Ошибка скачивания".to_string()),
        Err(e) => (false, format!("❌ Не удалось запустить dnf: {}", e)),
    }
}

// ==================== УСТАНОВКА ИЗ ПАПКИ ====================

fn install_from_folder(download_dir: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let system_arch = detect_arch();
    let files = find_packages_flat(download_dir, "", &system_arch);
    if files.is_empty() { return (false, format!("❌ В папке '{}' нет .rpm файлов", download_dir)); }
    let _ = tx.send(LogEvent::Line(format!("Найдено {} пакетов для установки", files.len())));

    let total_size_bytes: u64 = files.iter().filter_map(|f| fs::metadata(f).ok().map(|m| m.len())).sum();
    let total_size_mb = total_size_bytes / (1024 * 1024);
    let required_mb = total_size_mb * 3;
    let (space_ok, space_msg) = check_free_space(download_dir, required_mb.max(MIN_FREE_SPACE_CRITICAL_MB), "Установка пакетов");
    let _ = tx.send(LogEvent::Line(space_msg.clone()));
    if !space_ok { return (false, format!("{} Размер пакетов: {}, для установки нужно ~{}", space_msg, format_size_mb(total_size_mb), format_size_mb(required_mb))); }

    let mut installed = 0;
    for chunk in files.chunks(80) {
        let mut cmd = Command::new("dnf");
        cmd.arg("install").arg("-y").arg("--allowerasing").arg("--nobest");
        for file in chunk { cmd.arg(file); }
        match run_process(cmd, tx) {
            Ok(s) if s.success() => { installed += chunk.len(); }
            Ok(_s) => { /* пропускаем */ }
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
            if !files.is_empty() { return (true, format!("{} скачан ({} файлов)", package_name, files.len())); }
            let _ = tx.send(LogEvent::Line("Файлы не в папке, ищу в кэше dnf...".to_string()));
            let cache_search = format!("find /var/cache/dnf -name '*{}*.rpm' -type f 2>/dev/null | head -5", package_name);
            if let Ok(output) = Command::new("sh").arg("-c").arg(&cache_search).output() {
                let cache_files = String::from_utf8_lossy(&output.stdout);
                if !cache_files.trim().is_empty() {
                    for cache_file in cache_files.lines() {
                        if !cache_file.is_empty() { let _ = Command::new("cp").arg(cache_file).arg(folder).status(); }
                    }
                    let files = find_packages_by_substring(folder, package_name, &detect_arch());
                    if !files.is_empty() { return (true, format!("{} скачан через кэш", package_name)); }
                }
            }
            (false, format!("{} скачан, но файлы не найдены", package_name))
        }
        Ok(_s) => (false, format!("{} не найден в репозитории", package_name)),
        Err(e) => (false, format!("❌ Не удалось скачать {}: {}", package_name, e)),
    }
}

fn install_local_packages(folder: &str, pattern: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let files = find_packages_by_substring(folder, pattern, &detect_arch());
    if files.is_empty() { return (false, format!("❌ Не найдено *{}*.rpm в папке {}", pattern, folder)); }

    let total_size_bytes: u64 = files.iter().filter_map(|f| fs::metadata(f).ok().map(|m| m.len())).sum();
    let total_size_mb = total_size_bytes / (1024 * 1024);
    let required_mb = total_size_mb * 3;
    let (space_ok, space_msg) = check_free_space(folder, required_mb.max(100), format!("Установка {}", pattern));
    let _ = tx.send(LogEvent::Line(space_msg.clone()));
    if !space_ok { return (false, space_msg); }

    let _ = tx.send(LogEvent::Line(format!("Устанавливаю {} пакетов ({}): {}", files.len(), format_size_mb(total_size_mb), pattern)));
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
    if !install_script.exists() { return (false, "❌ В linux-amd64/ не найден install.sh КриптоПро".to_string()); }
    if !uninstall_script.exists() { return (false, "❌ В linux-amd64/ не найден uninstall.sh КриптоПро".to_string()); }

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
        let _ = tx.send(LogEvent::Line("newt не установлен — скачиваю...".to_string()));
        let (ok, msg) = download_package_to_folder("newt", crypto_dir, tx);
        if !ok { return (false, format!("Шаг 1 (newt): {}", msg)); }
    } else if newt_installed {
        let _ = tx.send(LogEvent::Line("✅ newt уже установлен".to_string()));
    }
    if !newt_installed {
        let (ok, msg) = install_local_packages(crypto_dir, "newt", tx);
        if !ok { return (false, format!("Шаг 1 (newt): {}", msg)); }
    }

    let _ = tx.send(LogEvent::Line("=== ШАГ 2.1/5: pcsc-lite + pcsc-lite-ccid ===".to_string()));
    let _ = tx.send(LogEvent::Progress(0.15, "Шаг 2.1: pcsc-lite...".to_string()));
    for pkg in &["pcsc-lite", "pcsc-lite-ccid"] {
        let pkg_installed = is_package_installed(pkg);
        if !pkg_installed && find_packages_by_substring(crypto_dir, pkg, &system_arch).is_empty() {
            let (ok, msg) = download_package_to_folder(pkg, crypto_dir, tx);
            if !ok { return (false, format!("Шаг 2.1 ({}): {}", pkg, msg)); }
        } else if pkg_installed {
            let _ = tx.send(LogEvent::Line(format!("✅ {} уже установлен", pkg)));
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
        if !ok { return (false, format!("Шаг 2.2 (librtpkcs11ecp): {}", msg)); }
    } else if lib_installed {
        let _ = tx.send(LogEvent::Line("✅ librtpkcs11ecp уже установлен".to_string()));
    }
    if !lib_installed {
        let (ok, msg) = install_local_packages(crypto_dir, "librtpkcs11ecp", tx);
        if !ok { return (false, format!("Шаг 2.2 (librtpkcs11ecp): {}", msg)); }
    }

    let _ = tx.send(LogEvent::Line("=== ШАГ 3/5: КриптоПро CSP ===".to_string()));
    let _ = tx.send(LogEvent::Progress(0.5, "Шаг 3: КриптоПро...".to_string()));
    let crypto_subdir = Path::new(crypto_dir).join("linux-amd64");
    let (ok, msg) = install_cryptopro_csp(&crypto_subdir.to_string_lossy(), tx);
    if !ok { return (false, format!("Шаг 3 (КриптоПро): {}", msg)); }

    let _ = tx.send(LogEvent::Line("=== ШАГ 4/5: GIA11_PPE ===".to_string()));
    let _ = tx.send(LogEvent::Progress(0.75, "Шаг 4: GIA11_PPE...".to_string()));
    if find_packages_by_substring(crypto_dir, "GIA11_PPE", &system_arch).is_empty() {
        return (false, "❌ GIA11_PPE*.rpm не найден в cryptopro_gia/. Положите файл и повторите.".to_string());
    }
    let (ok, msg) = install_local_packages(crypto_dir, "GIA11_PPE", tx);
    if !ok { return (false, format!("Шаг 4 (GIA11_PPE): {}", msg)); }

    let _ = tx.send(LogEvent::Line("=== ШАГ 5/5: Запуск pcscd ===".to_string()));
    let _ = tx.send(LogEvent::Progress(0.9, "Шаг 5: запуск pcscd...".to_string()));
    let mut cmd = Command::new("systemctl");
    cmd.arg("enable").arg("--now").arg("pcscd");
    match run_process(cmd, tx) {
        Ok(s) if s.success() => { let _ = tx.send(LogEvent::Line("✅ pcscd запущен".to_string())); }
        _ => { let _ = tx.send(LogEvent::Line("⚠ pcscd не запущен — вручную: systemctl enable --now pcscd".to_string())); }
    }

    (true, "Установлено: newt, pcsc-lite, librtpkcs11ecp, КриптоПро CSP, GIA11_PPE".to_string())
}

fn get_packages() -> Vec<&'static str> { vec!["task-koge", "r7-office", "notepadqq"] }

// ==================== ДИСПЕТЧЕР КОМАНД ====================

fn run_command(cmd_id: &str, tx: &mpsc::Sender<LogEvent>) {
    log_to_file(&format!("=== Запуск команды: {} ===", cmd_id));

    let pkg_dir = get_school_packages_dir();
    let upd_dir = get_updates_dir();
    if pkg_dir == upd_dir {
        let _ = tx.send(LogEvent::Line("⚠ ВНИМАНИЕ: school_packages и updates указывают на одну папку!".to_string()));
        let _ = tx.send(LogEvent::Line("   Проверьте school-installer.conf или переменные окружения.".to_string()));
    }

    ensure_directories();

    if let Some(free_mb) = get_free_space_mb(&pkg_dir) {
        if free_mb < MIN_FREE_SPACE_CRITICAL_MB {
            let _ = tx.send(LogEvent::Line(format!("❌ КРИТИЧЕСКИ МАЛО МЕСТА: свободно {} на диске. Программа может работать нестабильно.", format_size_mb(free_mb))));
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
        "setup_env" => {
            // УЛУЧШЕНИЕ №8: сначала автонастройка, потом ручная
            let (auto_ok, auto_msg) = auto_setup_proxy(tx);
            if auto_ok {
                let cert_dir = get_cert_dir();
                let (ok2, m2) = install_cert(&cert_dir, tx);
                if !ok2 { (false, format!("Сертификат: {}", m2)) }
                else { (true, format!("{} | {}", auto_msg, m2)) }
            } else {
                prepare_environment(tx)
            }
        }
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
                let _ = tx.send(LogEvent::Line(format!("⚠ На смонтированных не найден. Монтирую {}...", device)));
                if let Some(mount_point) = mount_usb_device(&device, tx) {
                    let cert_path = Path::new(&mount_point).join(CERT_FILE_NAME);
                    if cert_path.exists() {
                        let _ = tx.send(LogEvent::Line(format!("✅ Найден на флешке: {}", cert_path.display())));
                        unmount_device(&mount_point, tx);
                        (true, format!("Сертификат найден на USB: {}", cert_path.display()))
                    } else {
                        unmount_device(&mount_point, tx);
                        (false, "Сертификат не найден на USB-устройстве".to_string())
                    }
                } else {
                    (false, "Не удалось смонтировать USB-устройство".to_string())
                }
            } else {
                (false, "USB-устройства не найдены. Вставьте флешку с сертификатом.".to_string())
            }
        }
        "detect_os" => {
            print_setup_instructions(tx);
            let mut info = format!("🖥 ОС: МОС ОС (ROSA) | Арх: {}", system_arch);
            info += &format!("\n  🌐 Прокси: {}", get_proxy_url());
            info += &format!("\n  🔒 cer: {}", cert_dir);
            info += &format!("\n  📦 school_packages: {} ({} .rpm)", Path::new(&download_dir).exists(), find_packages_flat(&download_dir, "", &system_arch).len());
            info += &format!("\n  🔄 updates: {} ({} .rpm)", Path::new(&updates_dir).exists(), find_packages_flat(&updates_dir, "", &system_arch).len());
            info += &format!("\n  🔐 cryptopro_gia/linux-amd64/install.sh: {}", Path::new(&format!("{}/linux-amd64/install.sh", crypto_dir)).exists());
            info += &format!("\n  .rpm в cryptopro_gia/: {}", find_packages_flat(&crypto_dir, "", &system_arch).len());
            if let Some(free_mb) = get_free_space_mb(&download_dir) {
                info += &format!("\n  💾 Свободно на диске: {}", format_size_mb(free_mb));
            }
            info += &format!("\n\n  📝 Лог сохраняется в: {}", log_file_path());
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
    CommandItem { id: "detect_os", name: "Информация и инструкция", description: "Показывает структуру папок, свободное место и инструкцию по подготовке флешки.", icon: "ℹ", hotkey: '1' },
    CommandItem { id: "auto_proxy", name: "Автонастройка прокси", description: "Автоматически определяет прокси из окружения, gsettings, WPAD и проверяет доступность. УЛУЧШЕНИЕ №8.", icon: "⚙", hotkey: '9' },
    CommandItem { id: "setup_env", name: "Настроить прокси + сертификат", description: "Прокси в dnf.conf + /etc/profile.d/proxy.sh. Сертификаты из 'cer/' или с USB в системное хранилище и NSS Chromium.", icon: "🌐", hotkey: '2' },
    CommandItem { id: "find_cert", name: "Найти сертификат на USB", description: "Автоматически ищет ca-root.crt на всех смонтированных устройствах и USB-флешках. УЛУЧШЕНИЕ №7.", icon: "🔍", hotkey: '8' },
    CommandItem { id: "download_updates", name: "Скачать обновления системы", description: "Скачивает обновления в updates/. Исключает i686 пакеты. Проверяет свободное место (мин. 1 ГБ).", icon: "⬇", hotkey: '3' },
    CommandItem { id: "install_updates", name: "Установить обновления", description: "Устанавливает из updates/ офлайн. Проверяет свободное место перед установкой.", icon: "⬆", hotkey: '4' },
    CommandItem { id: "download_packages", name: "Скачать пакеты", description: "Скачивает task-koge + r7-office + notepadqq. Проверяет свободное место (мин. 2 ГБ).", icon: "📦", hotkey: '5' },
    CommandItem { id: "install_packages", name: "Установить пакеты", description: "Устанавливает из school_packages/ офлайн. Проверяет свободное место.", icon: "🚀", hotkey: '6' },
    CommandItem { id: "install_cryptopro_gia", name: "Установить КриптоПро + ГИА", description: "5 шагов: newt → pcsc-lite → librtpkcs11ecp → КриптоПро → GIA11_PPE → pcscd. Проверяет свободное место (мин. 500 МБ).", icon: "🔐", hotkey: '7' },
    CommandItem { id: "exit", name: "Выход", description: "Завершить программу.", icon: "✕", hotkey: '0' },
];

fn needs_confirmation(cmd_id: &str) -> bool {
    matches!(cmd_id, "setup_env" | "download_updates" | "install_updates" | "download_packages" | "install_packages" | "install_cryptopro_gia" | "find_cert" | "auto_proxy")
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default().direction(Direction::Vertical).constraints([Constraint::Percentage((100 - percent_y) / 2), Constraint::Percentage(percent_y), Constraint::Percentage((100 - percent_y) / 2)]).split(r);
    Layout::default().direction(Direction::Horizontal).constraints([Constraint::Percentage((100 - percent_x) / 2), Constraint::Percentage(percent_x), Constraint::Percentage((100 - percent_x) / 2)]).split(popup_layout[1])[1]
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
            status: "Готов к работе. Выберите ℹ для инструкции.".to_string(),
            is_running: false,
            arch,
            progress: 0.0, progress_label: String::new(),
            log_lines: Vec::new(), log_rx: None,
            first_run_shown: false,
            show_help: false,
            pending_command: None,
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
        match key {
            '1' => self.selected = 0,
            '2' => self.selected = 2,
            '3' => self.selected = 4,
            '4' => self.selected = 5,
            '5' => self.selected = 6,
            '6' => self.selected = 7,
            '7' => self.selected = 8,
            '8' => self.selected = 3,
            '9' => self.selected = 1,
            '0' => self.selected = 9,
            _ => return,
        }
        self.list_state.select(Some(self.selected));
        self.execute_selected();
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
                        log_to_file(&format!("[PROGRESS] {} {}", p, label));
                    }
                    LogEvent::Done(ok, msg) => {
                        self.progress = 1.0;
                        self.progress_label = String::new();
                        self.status = if ok { format!("✅ {}", msg) } else { format!("❌ {}", msg) };
                        self.is_running = false;
                        log_to_file(&format!("=== ЗАВЕРШЕНО: {} ===", msg));
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
    let chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(3), Constraint::Length(3)]).split(f.area());

    let header = Paragraph::new(vec![Line::from(vec![
        Span::styled(" ШКОЛЬНЫЙ УСТАНОВЩИК ПО ", Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(format!("ОС: МОС ОС | Арх: {}", app.arch), Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
    ])])
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)));
    f.render_widget(header, chunks[0]);

    let main_chunks = Layout::default().direction(Direction::Horizontal).constraints([Constraint::Percentage(45), Constraint::Percentage(55)]).split(chunks[1]);

    let items: Vec<ListItem> = COMMANDS.iter().enumerate().map(|(i, cmd)| {
        let style = if i == app.selected { Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD) }
                    else if app.is_running { Style::default().fg(Color::DarkGray) }
                    else { Style::default().fg(Color::White) };
        let hotkey_str = if cmd.hotkey == '0' { "0".to_string() } else { format!("{}", cmd.hotkey) };
        ListItem::new(Line::from(vec![
            Span::styled(format!("{} ", hotkey_str), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {} ", cmd.icon), style),
            Span::styled(cmd.name, style),
        ]))
    }).collect();

    let menu_title = if app.show_help { " ГОРЯЧИЕ КЛАВИШИ: 1-9=выбор, 0=выход, h=скрыть, ↑↓=навигация, q=выход " }
                     else if app.pending_command.is_some() { " ⚠️ ОЖИДАНИЕ ПОДТВЕРЖДЕНИЯ: Y=да, N=отмена " }
                     else { " КОМАНДЫ (1-9: выбор, h: помощь, ↑↓, Enter, q) " };

    let commands_list = List::new(items).block(Block::default().title(menu_title).borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan))).highlight_symbol("▸ ");
    f.render_stateful_widget(commands_list, main_chunks[0], &mut app.list_state.clone());

    let right_chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(8), Constraint::Min(0)]).split(main_chunks[1]);

    if app.show_help {
        let help_text = vec![
            Line::from(Span::styled("⌨️  ГОРЯЧИЕ КЛАВИШИ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))),
            Line::from(""),
            Line::from("  1 — ℹ Информация и инструкция"),
            Line::from("  9 — ⚙ Автонастройка прокси (NEW!)"),
            Line::from("  2 — 🌐 Настроить прокси + сертификат"),
            Line::from("  8 — 🔍 Найти сертификат на USB"),
            Line::from("  3 — ⬇ Скачать обновления системы"),
            Line::from("  4 — ⬆ Установить обновления"),
            Line::from("  5 — 📦 Скачать пакеты"),
            Line::from("  6 — 🚀 Установить пакеты"),
            Line::from("  7 — 🔐 Установить КриптоПро + ГИА"),
            Line::from("  0 — ✕ Выход"),
            Line::from(""),
            Line::from("  ↑/k — предыдущий пункт"),
            Line::from("  ↓/j — следующий пункт"),
            Line::from("  Enter — выполнить выбранный пункт"),
            Line::from("  h — показать/скрыть эту справку"),
            Line::from("  q/Esc — выход"),
            Line::from("  Ctrl+C — аварийный выход"),
            Line::from(""),
            Line::from(Span::styled("💡 Совет: нажмите цифру — команда запустится мгновенно!", Style::default().fg(Color::Green))),
        ];
        let help_widget = Paragraph::new(help_text).block(Block::default().title(" СПРАВКА ПО ГОРЯЧИМ КЛАВИШАМ ").borders(Borders::ALL).border_style(Style::default().fg(Color::Yellow))).wrap(Wrap { trim: false });
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

    let log_widget = Paragraph::new(visible_lines)
        .block(Block::default().title(" ЛОГ ").borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)))
        .wrap(Wrap { trim: false });
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
    ])])
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)));
    f.render_widget(progress_bar, chunks[2]);

    let status_color = if app.status.starts_with("✅") { Color::Green } else if app.status.starts_with("❌") { Color::Red } else if app.status.starts_with("▶") { Color::Yellow } else if app.status.starts_with("⚠") { Color::Yellow } else { Color::White };
    let status_widget = Paragraph::new(vec![Line::from(vec![
        Span::styled(" Статус: ", Style::default().fg(Color::Cyan)),
        Span::styled(app.status.as_str(), Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
    ])])
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)));
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
            Line::from(Span::styled("Это действие изменит систему или скачает данные.", Style::default().fg(Color::Red))),
            Line::from(""),
            Line::from(Span::styled("Нажмите [Y] для подтверждения", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))),
            Line::from(Span::styled("Нажмите [N] или Esc для отмены", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))),
        ];
        let paragraph = Paragraph::new(text).block(Block::default().borders(Borders::NONE)).wrap(Wrap { trim: false });
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
                log_to_file(&format!("CLI: set-proxy {}", args[2]));
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
                    println!("✅ Найден на смонтированном устройстве: {}", cert_path);
                } else if let Some(device) = find_first_usb_device() {
                    println!("⚠ На смонтированных не найден. Монтирую {}...", device);
                    let (tx, rx) = mpsc::channel::<LogEvent>();
                    if let Some(mount_point) = mount_usb_device(&device, &tx) {
                        let cert_path = Path::new(&mount_point).join(CERT_FILE_NAME);
                        if cert_path.exists() {
                            println!("✅ Найден на флешке: {}", cert_path.display());
                            unmount_device(&mount_point, &tx);
                        } else {
                            println!("❌ Файл не найден на флешке");
                            unmount_device(&mount_point, &tx);
                        }
                    } else {
                        println!("❌ Не удалось смонтировать USB");
                    }
                } else {
                    println!("❌ USB-устройства не найдены");
                }
                return Ok(());
            }
            "auto-proxy" => {
                // УЛУЧШЕНИЕ №8: CLI режим автонастройки
                println!("⚙ Запуск автонастройки прокси...");
                let (tx, rx) = mpsc::channel::<LogEvent>();
                thread::spawn(move || { run_command("auto_proxy", &tx); });
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
                return Ok(());
            }
            "reset-settings" => {
                let _ = fs::remove_file(SETTINGS_FILE);
                println!("✅ Настройки сброшены к значениям по умолчанию");
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
            "find-cert" => "find_cert", "auto-proxy" => "auto_proxy",
            _ => { println!("Неизвестная команда: {}", mode); return Ok(()); }
        };
        thread::spawn(move || run_command(&cmd_id, &tx));
        loop {
            match rx.recv() {
                Ok(LogEvent::Line(s)) => { println!("{}", s); log_to_file(&s); }
                Ok(LogEvent::Progress(p, l)) => { println!("[{:>3.0}%] {}", p * 100.0, l); log_to_file(&format!("[PROGRESS] {} {}", p, l)); }
                Ok(LogEvent::Done(ok, msg)) => {
                    println!("{}", msg);
                    log_to_file(&format!("=== ЗАВЕРШЕНО: {} ===", msg));
                    if !ok { std::process::exit(1); }
                    break;
                }
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
                        match c {
                            '1' | '2' | '3' | '4' | '5' | '6' | '7' | '8' | '9' | '0' => { app.select_by_hotkey(c); if c == '0' { break; } continue; }
                            'h' | 'H' | 'р' | 'Р' => { app.toggle_help(); continue; }
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
