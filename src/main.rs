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
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

// ==================== НАСТРОЙКИ ====================

const SETTINGS_FILE: &str = "school-installer.conf";
const DEFAULT_PROXY_URL: &str = "http://10.0.54.52:3128";
const DEFAULT_CERT_DIR: &str = "cer";
const DEFAULT_CRYPTO_DIR: &str = "cryptopro_gia";
const DEFAULT_SCHOOL_PACKAGES_DIR: &str = "school_packages";
const DEFAULT_UPDATES_DIR: &str = "updates";

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
    if let Ok(val) = env::var(&env_key) {
        if !val.is_empty() { return val; }
    }
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
    out
}

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
                if !cleaned.trim().is_empty() {
                    let _ = tx_err.send(LogEvent::Line(cleaned.clone()));
                }
                if let Some((done, total)) = parse_progress(&l) {
                    let pct = done as f64 / total as f64;
                    let _ = tx_err.send(LogEvent::Progress(pct, format!("{} / {}", done, total)));
                }
            }
        }
    });

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

fn command_exists(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {}", name))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ==================== ОПРЕДЕЛЕНИЕ СИСТЕМЫ ====================

fn detect_arch() -> String {
    Command::new("uname")
        .arg("-m")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_lowercase().to_string())
        .unwrap_or_else(|_| "x86_64".to_string())
}

fn real_home_and_user() -> (String, Option<String>) {
    if let Ok(user) = env::var("SUDO_USER") {
        if !user.is_empty() {
            if let Ok(out) = Command::new("getent").arg("passwd").arg(&user).output() {
                let s = String::from_utf8_lossy(&out.stdout);
                if let Some(line) = s.lines().next() {
                    let parts: Vec<&str> = line.split(':').collect();
                    if parts.len() >= 6 && !parts[5].is_empty() {
                        return (parts[5].to_string(), Some(user));
                    }
                }
            }
            return (format!("/home/{}", user), Some(user));
        }
    }
    let home = env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    (home, None)
}

fn get_folder_size(path: &str) -> u64 {
    let mut total: u64 = 0;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.is_file() {
                if let Ok(meta) = fs::metadata(&p) { total += meta.len(); }
            } else if p.is_dir() {
                total += get_folder_size(&p.to_string_lossy());
            }
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

// ==================== АВТОСОЗДАНИЕ ПАПОК И ИНСТРУКЦИЙ ====================

fn ensure_directories() {
    let base_packages = get_school_packages_dir();
    let base_updates = get_updates_dir();
    let crypto_dir = get_crypto_dir();
    let cert_dir = get_cert_dir();

    let dirs_with_readme: Vec<(String, &str)> = vec![
        (
            base_packages.clone(),
            "📦 ПАКЕТЫ ДЛЯ МОС ОС (ROSA)\n\
             ========================================\n\
             Сюда автоматически скачиваются пакеты для установки.\n\
             \n\
             Основные пакеты:\n\
             - task-koge-*.rpm        (метапакет для ОГЭ/ЕГЭ по информатике)\n\
             - r7-office-*.rpm        (или myoffice-education-*.rpm)\n\
             - notepadqq-*.rpm        (редактор кода)\n\
             \n\
             Как получить:\n\
             1. Запустите на машине с МОС ОС и интернетом:\n\
                sudo ./school-installer --download-packages\n\
             2. Пакеты автоматически скачаются в эту папку\n\
             3. Скопируйте папку school_packages/ на флешку"
        ),
        (
            base_updates.clone(),
            "🔄 ОБНОВЛЕНИЯ ДЛЯ МОС ОС (ROSA)\n\
             ========================================\n\
             Сюда автоматически скачиваются обновления системы.\n\
             \n\
             Как получить:\n\
             sudo ./school-installer --download-updates\n\
             \n\
             Установка в классе:\n\
             sudo ./school-installer --install-updates"
        ),
        (
            format!("{}/linux-amd64", crypto_dir),
            "🔐 КРИПТОПРО CSP — ДИСТРИБУТИВ\n\
             ========================================\n\
             Сюда положите дистрибутив КриптоПро CSP.\n\
             \n\
             Что сюда положить:\n\
             1. Распакуйте архив КриптоПро CSP (например, linux-amd64.tgz)\n\
             2. Скопируйте ВСЕ файлы из распакованной папки сюда:\n\
                - install.sh              (ОБЯЗАТЕЛЬНО)\n\
                - uninstall.sh            (ОБЯЗАТЕЛЬНО)\n\
                - lsb-cprocsp-base-*.rpm\n\
                - lsb-cprocsp-rdr-*.rpm\n\
                - lsb-cprocsp-kc1-*.rpm   (или kc2, если нужен)\n\
                - lsb-cprocsp-capilite-*.rpm\n\
                - cprocsp-curl-*.rpm\n\
                - lsb-cprocsp-ca-certs-*.rpm\n\
             \n\
             ВАЖНО: install.sh сам выберет правильный менеджер пакетов.\n\
             \n\
             Конфигурация: kc1 (по умолчанию) — для экзаменов.\n\
             kc2 используйте только если точно знаете, зачем."
        ),
        (
            crypto_dir.clone(),
            "🔑 КРИПТОПРО + ГИА — ДОПОЛНИТЕЛЬНЫЕ ПАКЕТЫ\n\
             ========================================\n\
             Корень папки cryptopro_gia/ — сюда кладите:\n\
             \n\
             1. newt-*.rpm\n\
                — нужен для install.sh КриптоПро\n\
             \n\
             2. pcsc-lite-*.rpm и pcsc-lite-ccid-*.rpm\n\
                — демон смарт-карт и драйвер USB-ридеров\n\
             \n\
             3. librtpkcs11ecp-*.rpm\n\
                — библиотека для токенов Рутокен/Jacarta\n\
             \n\
             4. GIA11_PPE-*.rpm\n\
                — платформа проведения экзамена (ОБЯЗАТЕЛЬНО)\n\
             \n\
             Если какого-то пакета нет — программа скачает его\n\
             из репозитория (нужен интернет через прокси)."
        ),
        (
            cert_dir,
            "🔒 СЕРТИФИКАТЫ\n\
             ========================================\n\
             Сюда положите корневые сертификаты (.crt, .pem, .cer)\n\
             для прокси и внутренних сайтов школы.\n\
             \n\
             Обычно это файл ca-root.crt от вашего прокси-сервера.\n\
             \n\
             Программа установит сертификаты:\n\
             1. В системное хранилище (update-ca-trust)\n\
             2. В NSS-базу Chromium (для браузера)\n\
             \n\
             ВАЖНО: не кладите файлы, начинающиеся с ._ (это\n\
             macOS-артефакты, они не являются сертификатами)."
        ),
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
    Command::new("sh").arg("-c").arg(&check_cmd)
        .stdout(Stdio::null()).stderr(Stdio::null()).status()
        .map(|s| s.success()).unwrap_or(false)
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

// ==================== ШАГ 1: ПРОКСИ ====================

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
    let profile_content = format!(
        "# Автоматическая настройка прокси (school-installer)\nexport http_proxy=\"{u}\"\nexport https_proxy=\"{u}\"\nexport no_proxy=\"localhost,127.0.0.1,::1\"\n",
        u = proxy_url
    );
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

    (true, "Прокси настроен".to_string())
}

// ==================== ШАГ 2: СЕРТИФИКАТ ====================

fn install_cert(cert_dir: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let _ = tx.send(LogEvent::Line(format!("=== 🔒 Проверка сертификатов из '{}' ===", cert_dir)));
    if !Path::new(cert_dir).exists() {
        let _ = tx.send(LogEvent::Line(format!("⚠ Папка '{}' не найдена — пропускаем", cert_dir)));
        return (true, "Сертификат пропущен (нет папки)".to_string());
    }
    let mut certs: Vec<String> = Vec::new();
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
    if certs.is_empty() {
        let _ = tx.send(LogEvent::Line(format!("⚠ В '{}' нет валидных .crt/.pem — пропускаем", cert_dir)));
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
        let in_nss = Command::new("sh").arg("-c").arg(&check_cmd)
            .stdout(Stdio::null()).stderr(Stdio::null()).status()
            .map(|s| s.success()).unwrap_or(false);
        if !in_nss { all_in_nss = false; }
    }
    if all_in_anchors && all_in_nss {
        let _ = tx.send(LogEvent::Line(format!("✅ Все {} сертификат(ов) уже установлены", certs.len())));
        return (true, "Сертификаты уже установлены".to_string());
    }
    let _ = tx.send(LogEvent::Line(format!("=== Установка сертификатов из '{}' ===", cert_dir)));
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
        let already_in_nss = Command::new("sh").arg("-c").arg(&check_cmd)
            .stdout(Stdio::null()).stderr(Stdio::null()).status()
            .map(|s| s.success()).unwrap_or(false);
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
    let (ok1, m1) = setup_proxy(tx);
    if !ok1 { return (false, format!("Прокси: {}", m1)); }
    let cert_dir = get_cert_dir();
    let (ok2, m2) = install_cert(&cert_dir, tx);
    if !ok2 { return (false, format!("Сертификат: {}", m2)); }
    (true, format!("Прокси ✅ | {}", m2))
}

// ==================== СКАЧИВАНИЕ ====================

fn download_updates(download_dir: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let _ = fs::create_dir_all(download_dir);

    // Получаем абсолютный путь, чтобы dnf точно скачал туда
    let abs_path = fs::canonicalize(download_dir)
        .unwrap_or_else(|_| Path::new(download_dir).to_path_buf());
    let _ = tx.send(LogEvent::Line(format!("📥 Скачиваю обновления в: {}", abs_path.display())));

    let mut cmd = Command::new("dnf");
    cmd.arg("upgrade")
        .arg("--downloadonly")
        .arg(format!("--destdir={}", abs_path.display()))
        .arg("-y");

    match run_process(cmd, tx) {
        Ok(s) if s.success() => {
            let rpm_count = find_packages_flat(download_dir, "", &detect_arch()).len();
            if rpm_count > 0 {
                (true, format!("Обновления скачаны ({} пакетов)", rpm_count))
            } else {
                (false, "dnf отработал, но файлы не найдены в папке. Проверьте /var/cache/dnf/".to_string())
            }
        }
        Ok(_s) => (false, "Ошибка скачивания".to_string()),
        Err(e) => (false, format!("❌ Не удалось запустить dnf: {}", e)),
    }
}

fn download_packages(packages: &[&str], download_dir: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let _ = fs::create_dir_all(download_dir);
    let mut cmd = Command::new("dnf");
    cmd.arg("download").arg("--resolve").arg("--alldeps")
        .arg("--exclude=*.i686").arg("--exclude=*.i586").arg("--exclude=*.i386")
        .arg(format!("--destdir={}", download_dir));
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
    if files.is_empty() {
        return (false, format!("❌ В папке '{}' нет .rpm файлов", download_dir));
    }
    let _ = tx.send(LogEvent::Line(format!("Найдено {} пакетов для установки", files.len())));

    let mut installed = 0;
    for chunk in files.chunks(80) {
        let mut cmd = Command::new("dnf");
        cmd.arg("install").arg("-y").arg("--allowerasing").arg("--nobest");
        for file in chunk { cmd.arg(file); }
        match run_process(cmd, tx) {
            Ok(s) if s.success() => { installed += chunk.len(); }
            Ok(_s) => { /* пропускаем, продолжаем со следующими батчами */ }
            Err(e) => { let _ = tx.send(LogEvent::Line(format!("❌ Ошибка: {}", e))); }
        }
    }
    if installed > 0 {
        (true, format!("Установлено пакетов: {}", installed))
    } else {
        (false, "Не удалось установить ни одного пакета".to_string())
    }
}

// ==================== КРИПТОПРО + ГИА ====================

fn download_package_to_folder(package_name: &str, folder: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let _ = fs::create_dir_all(folder);

    let mut cmd = Command::new("dnf");
    cmd.arg("download").arg("--resolve").arg("--alldeps")
        .arg(format!("--destdir={}", folder))
        .arg(package_name);

    match run_process(cmd, tx) {
        Ok(s) if s.success() => {
            let files = find_packages_flat(folder, package_name, &detect_arch());
            if !files.is_empty() {
                return (true, format!("{} скачан", package_name));
            }
            // Fallback: ищем в кэше dnf
            let _ = tx.send(LogEvent::Line("Файлы не в папке, ищу в кэше dnf...".to_string()));
            let cache_search = format!("find /var/cache/dnf -name '{}*.rpm' -type f 2>/dev/null | head -5", package_name);
            if let Ok(output) = Command::new("sh").arg("-c").arg(&cache_search).output() {
                let cache_files = String::from_utf8_lossy(&output.stdout);
                if !cache_files.trim().is_empty() {
                    for cache_file in cache_files.lines() {
                        if !cache_file.is_empty() {
                            let _ = Command::new("cp").arg(cache_file).arg(folder).status();
                        }
                    }
                    let files = find_packages_flat(folder, package_name, &detect_arch());
                    if !files.is_empty() {
                        return (true, format!("{} скачан через кэш", package_name));
                    }
                }
            }
            (false, format!("{} скачан, но файлы не найдены", package_name))
        }
        Ok(_s) => (false, format!("{} не найден в репозитории", package_name)),
        Err(e) => (false, format!("❌ Не удалось скачать {}: {}", package_name, e)),
    }
}

fn install_local_packages(folder: &str, pattern: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let files = find_packages_flat(folder, pattern, &detect_arch());
    if files.is_empty() { return (false, format!("❌ Не найдено {}*.rpm в папке {}", pattern, folder)); }
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
    if !install_script.exists() { return (false, "❌ В linux-amd64/ не найден install.sh КриптоПро".to_string()); }
    if !uninstall_script.exists() { return (false, "❌ В linux-amd64/ не найден uninstall.sh КриптоПро".to_string()); }
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

    let _ = tx.send(LogEvent::Line("=== ШАГ 1/5: newt ===".to_string()));
    let _ = tx.send(LogEvent::Progress(0.0, "Шаг 1: newt...".to_string()));
    let newt_installed = is_package_installed("newt");
    if !newt_installed && find_packages_flat(crypto_dir, "newt", &system_arch).is_empty() {
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
        if !pkg_installed && find_packages_flat(crypto_dir, pkg, &system_arch).is_empty() {
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
    if !lib_installed && find_packages_flat(crypto_dir, "librtpkcs11ecp", &system_arch).is_empty() {
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
    if find_packages_flat(crypto_dir, "GIA11_PPE", &system_arch).is_empty() {
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

fn get_packages() -> Vec<&'static str> {
    vec!["task-koge", "r7-office", "notepadqq"]
}

// ==================== ДИСПЕТЧЕР КОМАНД ====================

fn run_command(cmd_id: &str, tx: &mpsc::Sender<LogEvent>) {
    // Проверка совпадения путей (защита от неправильного конфига)
    let pkg_dir = get_school_packages_dir();
    let upd_dir = get_updates_dir();
    if pkg_dir == upd_dir {
        let _ = tx.send(LogEvent::Line("⚠ ВНИМАНИЕ: school_packages и updates указывают на одну папку!".to_string()));
        let _ = tx.send(LogEvent::Line("   Проверьте school-installer.conf или переменные окружения.".to_string()));
    }

    ensure_directories();

    let needs_prepare = matches!(cmd_id,
        "download_updates" | "install_updates" | "download_packages" | "install_packages" | "install_cryptopro_gia");
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
        "detect_os" => {
            print_setup_instructions(tx);
            let mut info = format!("🖥 ОС: МОС ОС (ROSA) | Арх: {}", system_arch);
            info += &format!("\n  🌐 Прокси: {}", get_proxy_url());
            info += &format!("\n  🔒 cer: {}", cert_dir);
            info += &format!("\n  📦 school_packages: {} ({} .rpm)", Path::new(&download_dir).exists(), find_packages_flat(&download_dir, "", &system_arch).len());
            info += &format!("\n  🔄 updates: {} ({} .rpm)", Path::new(&updates_dir).exists(), find_packages_flat(&updates_dir, "", &system_arch).len());
            info += &format!("\n  🔐 cryptopro_gia/linux-amd64/install.sh: {}", Path::new(&format!("{}/linux-amd64/install.sh", crypto_dir)).exists());
            info += &format!("\n  .rpm в cryptopro_gia/: {}", find_packages_flat(&crypto_dir, "", &system_arch).len());
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

// ⚠ ВАЖНО: эмодзи только в поле icon, в name их нет — иначе в меню будет дублирование
const COMMANDS: &[CommandItem] = &[
    CommandItem { id: "detect_os", name: "Информация и инструкция", description: "Показывает структуру папок, что где лежит, и полную инструкцию по подготовке флешки. Папки создаются автоматически.", icon: "ℹ" },
    CommandItem { id: "setup_env", name: "Настроить прокси + сертификат", description: "Прокси в dnf.conf + /etc/profile.d/proxy.sh. Сертификаты из 'cer/' в системное хранилище и NSS Chromium.", icon: "🌐" },
    CommandItem { id: "download_updates", name: "Скачать обновления системы", description: "Скачивает обновления в updates/. Нужен интернет через прокси.", icon: "⬇" },
    CommandItem { id: "install_updates", name: "Установить обновления", description: "Устанавливает из updates/ офлайн.", icon: "⬆" },
    CommandItem { id: "download_packages", name: "Скачать пакеты", description: "Скачивает task-koge + r7-office + notepadqq. Сохраняет в school_packages/.", icon: "📦" },
    CommandItem { id: "install_packages", name: "Установить пакеты", description: "Устанавливает из school_packages/ офлайн.", icon: "🚀" },
    CommandItem { id: "install_cryptopro_gia", name: "Установить КриптоПро + ГИА", description: "5 шагов: newt → pcsc-lite → librtpkcs11ecp → КриптоПро (из cryptopro_gia/linux-amd64/) → GIA11_PPE → pcscd.", icon: "🔐" },
    CommandItem { id: "exit", name: "Выход", description: "Завершить программу.", icon: "✕" },
];

struct App {
    selected: usize,
    list_state: ListState,
    status: String,
    is_running: bool,
    arch: String,
    progress: f64,
    progress_label: String,
    log_lines: Vec<String>,
    log_rx: Option<mpsc::Receiver<LogEvent>>,
    first_run_shown: bool,
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
        thread::spawn(move || { run_command(cmd.id, &tx); });
        self.log_rx = Some(rx);
    }
    fn poll_events(&mut self) {
        let mut rx = self.log_rx.take();
        if let Some(receiver) = &rx {
            while let Ok(ev) = receiver.try_recv() {
                match ev {
                    LogEvent::Line(s) => {
                        self.log_lines.push(s);
                        if self.log_lines.len() > 800 { self.log_lines.remove(0); }
                    }
                    LogEvent::Progress(p, label) => { self.progress = p; self.progress_label = label; }
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
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(3), Constraint::Length(3)])
        .split(f.area());

    let header = Paragraph::new(vec![Line::from(vec![
        Span::styled(" ШКОЛЬНЫЙ УСТАНОВЩИК ПО ", Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(format!("ОС: МОС ОС | Арх: {}", app.arch), Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
    ])])
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)));
    f.render_widget(header, chunks[0]);

    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(chunks[1]);

    // Рендер меню: иконка + имя (без дублирования)
    let items: Vec<ListItem> = COMMANDS.iter().enumerate().map(|(i, cmd)| {
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
    }).collect();

    let commands_list = List::new(items)
        .block(Block::default().title(" КОМАНДЫ (стрелки, Enter, q) ").borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)))
        .highlight_symbol("▸ ");
    f.render_stateful_widget(commands_list, main_chunks[0], &mut app.list_state.clone());

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

    let height = right_chunks[1].height.saturating_sub(2) as usize;
    let start = app.log_lines.len().saturating_sub(height);
    let log_text = app.log_lines[start..].join("\n");
    let log_widget = Paragraph::new(log_text)
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

    let status_color = if app.status.starts_with("✅") { Color::Green } else if app.status.starts_with("❌") { Color::Red } else if app.status.starts_with("▶") { Color::Yellow } else { Color::White };
    let status_widget = Paragraph::new(vec![Line::from(vec![
        Span::styled(" Статус: ", Style::default().fg(Color::Cyan)),
        Span::styled(app.status.as_str(), Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
    ])])
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)));
    f.render_widget(status_widget, chunks[3]);
}

fn main() -> io::Result<()> {
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
            "show-settings" => {
                println!("=== Текущие настройки ===\n");
                println!("Файл: {}", SETTINGS_FILE);
                println!("Прокси: {}", get_proxy_url());
                println!("Папка сертификатов: {}", get_cert_dir());
                println!("Папка КриптоПро: {}", get_crypto_dir());
                println!("Папка пакетов: {}", get_school_packages_dir());
                println!("Папка обновлений: {}", get_updates_dir());
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
            "setup-env" => "setup_env",
            "download-updates" => "download_updates",
            "install-updates" => "install_updates",
            "download-packages" => "download_packages",
            "install-packages" => "install_packages",
            "install-crypto-gia" => "install_cryptopro_gia",
            _ => { println!("Неизвестная команда: {}", mode); return Ok(()); }
        };
        thread::spawn(move || run_command(cmd_id, &tx));
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

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut app = App::new();

    if !app.first_run_shown {
        app.first_run_shown = true;
        let (tx, rx) = mpsc::channel::<LogEvent>();
        thread::spawn(move || {
            run_command("detect_os", &tx);
        });
        app.log_rx = Some(rx);
    }

    loop {
        terminal.draw(|f| ui(f, &app))?;
        app.poll_events();
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') { break; }
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
    Ok(())
}
