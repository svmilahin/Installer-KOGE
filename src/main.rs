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

/// Загружает настройки из файла
fn load_settings() -> HashMap<String, String> {
    let mut settings = HashMap::new();
    if let Ok(content) = fs::read_to_string(SETTINGS_FILE) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                settings.insert(key.trim().to_string(), value.trim().to_string());
            }
        }
    }
    settings
}

/// Сохраняет одну настройку в файл
fn save_setting(key: &str, value: &str) -> io::Result<()> {
    let mut settings = load_settings();
    settings.insert(key.to_string(), value.to_string());

    let mut content = String::from("# Настройки школьного установщика\n");
    content += &format!("PROXY_URL={}\n", settings.get("PROXY_URL").map(|s| s.as_str()).unwrap_or(DEFAULT_PROXY_URL));
    content += &format!("CERT_DIR={}\n", settings.get("CERT_DIR").map(|s| s.as_str()).unwrap_or(DEFAULT_CERT_DIR));
    content += &format!("CRYPTO_DIR={}\n", settings.get("CRYPTO_DIR").map(|s| s.as_str()).unwrap_or(DEFAULT_CRYPTO_DIR));
    content += &format!("SCHOOL_PACKAGES_DIR={}\n", settings.get("SCHOOL_PACKAGES_DIR").map(|s| s.as_str()).unwrap_or(DEFAULT_SCHOOL_PACKAGES_DIR));

    fs::write(SETTINGS_FILE, content)
}

/// Получает значение настройки (CLI > env > файл > дефолт)
fn get_setting(key: &str, default: &str) -> String {
    // 1. Переменная окружения
    let env_key = format!("SCHOOL_{}", key);
    if let Ok(val) = env::var(&env_key) {
        if !val.is_empty() {
            return val;
        }
    }

    // 2. Файл настроек
    let settings = load_settings();
    if let Some(val) = settings.get(key) {
        return val.clone();
    }

    // 3. Дефолт
    default.to_string()
}

fn get_proxy_url() -> String {
    get_setting("PROXY_URL", DEFAULT_PROXY_URL)
}

fn get_cert_dir() -> String {
    get_setting("CERT_DIR", DEFAULT_CERT_DIR)
}

fn get_crypto_dir() -> String {
    get_setting("CRYPTO_DIR", DEFAULT_CRYPTO_DIR)
}

fn get_school_packages_dir() -> String {
    get_setting("SCHOOL_PACKAGES_DIR", DEFAULT_SCHOOL_PACKAGES_DIR)
}

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

// ==================== ШАГ 1: НАСТРОЙКА ПРОКСИ ====================

fn setup_proxy(os_type: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let proxy_url = get_proxy_url();
    let _ = tx.send(LogEvent::Line(format!("=== Настройка прокси {} ===", proxy_url)));

    let profile_path = "/etc/profile.d/proxy.sh";
    let profile_content = format!(
        "# Автоматическая настройка прокси (school-installer)\nexport http_proxy=\"{u}\"\nexport https_proxy=\"{u}\"\nexport no_proxy=\"localhost,127.0.0.1,::1\"\n",
        u = proxy_url
    );
    if let Err(e) = fs::write(profile_path, &profile_content) {
        return (false, format!("Не удалось записать {}: {} (нужен root/sudo)", profile_path, e));
    }
    let _ = Command::new("chmod").arg("644").arg(profile_path).status();
    let _ = tx.send(LogEvent::Line(format!("✅ {} создан", profile_path)));

    if os_type == "rosa" {
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
            return (false, format!("Не удалось записать {}: {}", conf, e));
        }
        let _ = tx.send(LogEvent::Line(format!("✅ {} обновлён", conf)));
    } else {
        let dir = "/etc/apt/apt.conf.d";
        let _ = fs::create_dir_all(dir);
        let path = format!("{}/95proxy", dir);
        let content = format!(
            "Acquire::http::Proxy \"{}\";\nAcquire::https::Proxy \"{}\";\n",
            proxy_url, proxy_url
        );
        if let Err(e) = fs::write(&path, &content) {
            return (false, format!("Не удалось записать {}: {}", path, e));
        }
        let _ = tx.send(LogEvent::Line(format!("✅ {} создан", path)));
    }

    (true, "Прокси настроен".to_string())
}

// ==================== ШАГ 2: УСТАНОВКА СЕРТИФИКАТА ====================

fn install_cert(cert_dir: &str, os_type: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let _ = tx.send(LogEvent::Line(format!("=== Установка сертификатов из '{}' ===", cert_dir)));

    if !Path::new(cert_dir).exists() {
        let _ = tx.send(LogEvent::Line(format!("⚠ Папка '{}' не найдена — пропускаем", cert_dir)));
        return (true, "Сертификат пропущен (нет папки)".to_string());
    }

    let mut certs: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(cert_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();

            // ❌ Пропускаем macOS resource forks (._*)
            if name.starts_with("._") {
                let _ = tx.send(LogEvent::Line(format!("   ⚠ Пропущен macOS-файл: {}", name)));
                continue;
            }
            if ext != "crt" && ext != "pem" && ext != "cer" { continue; }
            if !path.is_file() { continue; }

            // ❌ Пропускаем файлы меньше 100 байт (не могут быть сертификатами)
            if let Ok(meta) = fs::metadata(&path) {
                if meta.len() < 100 {
                    let _ = tx.send(LogEvent::Line(format!("   ⚠ Пропущен слишком маленький файл: {} ({} байт)", name, meta.len())));
                    continue;
                }
            }

            // ❌ Проверяем содержимое: должен быть PEM или DER
            let content = fs::read(&path).unwrap_or_default();
            let is_pem = content.starts_with(b"-----BEGIN CERTIFICATE-----");
            let is_der = content.len() > 2 && content[0] == 0x30 && (content[1] == 0x82 || content[1] == 0x81);
            if !is_pem && !is_der {
                let _ = tx.send(LogEvent::Line(format!("   ⚠ Пропущен невалидный файл (не PEM/DER): {}", name)));
                continue;
            }

            certs.push(path.to_string_lossy().to_string());
        }
    }

    if certs.is_empty() {
        let _ = tx.send(LogEvent::Line(format!("⚠ В '{}' нет валидных .crt/.pem — пропускаем", cert_dir)));
        return (true, "Сертификат пропущен (нет валидных файлов)".to_string());
    }

    let _ = tx.send(LogEvent::Line(format!("Найдено валидных сертификатов: {}", certs.len())));

    // === 1. Системное хранилище ===
    let anchors_dir = "/etc/pki/ca-trust/source/anchors";
    let _ = fs::create_dir_all(anchors_dir);
    for cert in &certs {
        let name = Path::new(cert).file_name().unwrap_or_default().to_string_lossy().to_string();
        let dst = format!("{}/{}", anchors_dir, name);
        if let Err(e) = fs::copy(cert, &dst) {
            return (false, format!("Не удалось скопировать {} в {}: {}", cert, dst, e));
        }
        let _ = tx.send(LogEvent::Line(format!("   → {}", dst)));
    }
    if command_exists("update-ca-trust") {
        let _ = tx.send(LogEvent::Line("Запускаю update-ca-trust...".to_string()));
        let mut cmd = Command::new("update-ca-trust");
        match run_process(cmd, tx) {
            Ok(s) if s.success() => { let _ = tx.send(LogEvent::Line("✅ Системное хранилище обновлено".to_string())); }
            Ok(s) => return (false, format!("update-ca-trust завершился с ошибкой. Код: {:?}", s.code())),
            Err(e) => return (false, format!("Не удалось запустить update-ca-trust: {}", e)),
        }
    } else {
        let _ = tx.send(LogEvent::Line("⚠ update-ca-trust не найден — пропускаю".to_string()));
    }

    // === 2. NSS-база Chromium ===
    let (home, real_user) = real_home_and_user();
    let nssdb_path = format!("{}/.pki/nssdb", home);
    let nssdb = format!("sql:{}", nssdb_path);

    if !command_exists("certutil") {
        let _ = tx.send(LogEvent::Line("certutil нет — ставлю nss-tools...".to_string()));
        let pm = if os_type == "rosa" { "dnf" } else { "apt-get" };
        let mut ok = false;
        for pkg in ["nss-tools", "libnss3-tools"] {
            let mut cmd = Command::new(pm);
            cmd.arg("install").arg("-y").arg(pkg);
            if let Ok(s) = run_process(cmd, tx) {
                if s.success() { ok = true; break; }
            }
        }
        if !ok {
            let _ = tx.send(LogEvent::Line("⚠ Не удалось установить nss-tools — NSS пропущен".to_string()));
            return (true, "Сертификат в системное хранилище (NSS пропущен)".to_string());
        }
    }

    let _ = fs::create_dir_all(&nssdb_path);

    // Инициализация базы ТОЛЬКО если её ещё нет (иначе certutil запросит пароль)
    let db_exists = Path::new(&format!("{}/cert9.db", nssdb_path)).exists()
                 || Path::new(&format!("{}/cert8.db", nssdb_path)).exists();
    if !db_exists {
        let _ = tx.send(LogEvent::Line("Создаю новую NSS-базу...".to_string()));
        let mut init = Command::new("certutil");
        init.arg("-d").arg(&nssdb).arg("-N").arg("--empty-password");
        let _ = init.stdout(Stdio::null()).stderr(Stdio::null()).status();
    } else {
        let _ = tx.send(LogEvent::Line("NSS-база уже существует — пропускаю инициализацию".to_string()));
    }

    // Добавление сертификатов. Ключевой фикс: передаём пустой пароль через stdin,
    // чтобы certutil не запрашивал его интерактивно (и не зацикливался).
    for cert in &certs {
        let nick = Path::new(cert).file_stem().unwrap_or_default().to_string_lossy().to_string();

        // Сначала удаляем старый с тем же nick (игнорируем ошибку)
        let del_cmd = format!(
            "echo '' | certutil -d '{}' -D -n '{}' 2>/dev/null",
            nssdb, nick
        );
        let _ = Command::new("sh").arg("-c").arg(&del_cmd)
            .stdout(Stdio::null()).stderr(Stdio::null()).status();

        // Добавляем новый — пустой пароль через stdin
        let add_cmd = format!(
            "echo '' | certutil -d '{}' -A -t 'C,,' -n '{}' -i '{}'",
            nssdb, nick, cert
        );
        let _ = tx.send(LogEvent::Line(format!("Добавляю в NSS: {}...", nick)));
        match Command::new("sh").arg("-c").arg(&add_cmd)
            .stdout(Stdio::piped()).stderr(Stdio::piped())
            .status()
        {
            Ok(s) if s.success() => {
                let _ = tx.send(LogEvent::Line(format!("   ✅ NSS: {}", nick)));
            }
            Ok(s) => {
                let _ = tx.send(LogEvent::Line(format!("   ⚠ NSS не добавлен (код {:?}): {}", s.code(), nick)));
            }
            Err(e) => {
                let _ = tx.send(LogEvent::Line(format!("   ⚠ NSS ошибка: {}", e)));
            }
        }
    }

    // Вернуть права реальному юзеру
    if let Some(user) = real_user {
        let mut ch = Command::new("chown");
        ch.arg("-R").arg(format!("{}:{}", user, user)).arg(&nssdb_path);
        let _ = ch.status();
        let _ = tx.send(LogEvent::Line(format!("   ✅ chown .pki → {}", user)));
    }

    (true, format!("Сертификатов установлено: {}", certs.len()))
}

fn prepare_environment(os_type: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let (ok1, m1) = setup_proxy(os_type, tx);
    if !ok1 { return (false, format!("Прокси: {}", m1)); }
    let cert_dir = get_cert_dir();
    let (ok2, m2) = install_cert(&cert_dir, os_type, tx);
    if !ok2 { return (false, format!("Сертификат: {}", m2)); }
    (true, format!("Прокси ✓ | {}", m2))
}

// ==================== СКАЧИВАНИЕ ====================

fn download_updates(download_dir: &str, os_type: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let _ = fs::create_dir_all(download_dir);
    let pm = if os_type == "rosa" { "dnf" } else { "apt-get" };

    if os_type == "rosa" {
        let mut cmd = Command::new(pm);
        cmd.arg("upgrade").arg("--downloadonly").arg(format!("--destdir={}", download_dir)).arg("-y");
        match run_process(cmd, tx) {
            Ok(s) if s.success() => (true, "Обновления скачаны".to_string()),
            Ok(s) => (false, format!("Ошибка скачивания. Код: {:?}", s.code())),
            Err(e) => (false, format!("Не удалось запустить {}: {}", pm, e)),
        }
    } else {
        let mut cmd1 = Command::new(pm); cmd1.arg("update"); let _ = run_process(cmd1, tx);
        let sim_output = Command::new(pm).arg("upgrade").arg("--simulate").output();
        let packages: Vec<String> = match sim_output {
            Ok(output) => String::from_utf8_lossy(&output.stdout).lines().filter(|l| l.starts_with("Inst ")).filter_map(|l| l.split_whitespace().nth(1).map(String::from)).collect(),
            Err(e) => return (false, format!("Ошибка: {}", e)),
        };
        if packages.is_empty() { return (true, "Система уже обновлена".to_string()); }
        let total = packages.len(); let mut done = 0; let mut all_ok = true;
        for pkg in &packages {
            let mut cmd = Command::new(pm);
            cmd.arg("install").arg("--download-only").arg("-y").arg(format!("-o Dir::Cache::Archives={}", download_dir)).arg(pkg);
            if let Ok(s) = run_process(cmd, tx) { if !s.success() { all_ok = false; } } else { all_ok = false; }
            done += 1;
            let _ = tx.send(LogEvent::Progress(done as f64 / total as f64, format!("{} / {}", done, total)));
        }
        if all_ok { (true, format!("Скачано пакетов: {}", packages.len())) } else { (false, "Некоторые пакеты не удалось скачать".to_string()) }
    }
}

fn download_packages(packages: &[&str], download_dir: &str, os_type: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let _ = fs::create_dir_all(download_dir);
    let pm = if os_type == "rosa" { "dnf" } else { "apt-get" };

    if os_type == "rosa" {
        let mut cmd = Command::new(pm);
        cmd.arg("download").arg("--resolve").arg("--alldeps").arg("--exclude=*.i686").arg("--exclude=*.i586").arg("--exclude=*.i386").arg(format!("--destdir={}", download_dir));
        for pkg in packages { cmd.arg(pkg); }
        match run_process(cmd, tx) {
            Ok(s) if s.success() => (true, "Пакеты скачаны".to_string()),
            Ok(s) => (false, format!("Ошибка. Код: {:?}", s.code())),
            Err(e) => (false, format!("Не удалось запустить {}: {}", pm, e)),
        }
    } else {
        let total = packages.len(); let mut done = 0; let mut all_ok = true;
        for pkg in packages {
            let mut cmd = Command::new(pm);
            cmd.arg("install").arg("--download-only").arg("-y").arg(format!("-o Dir::Cache::Archives={}", download_dir)).arg(pkg);
            if let Ok(s) = run_process(cmd, tx) { if !s.success() { all_ok = false; } } else { all_ok = false; }
            done += 1;
            let _ = tx.send(LogEvent::Progress(done as f64 / total as f64, format!("{} / {}", done, total)));
        }
        if all_ok { (true, "Все пакеты скачаны".to_string()) } else { (false, "Некоторые пакеты не удалось скачать".to_string()) }
    }
}

// ==================== УСТАНОВКА ИЗ ПАПКИ ====================

fn install_from_folder(download_dir: &str, os_type: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let system_arch = detect_arch();
    let extension = if os_type == "rosa" { "rpm" } else { "deb" };
    let pm = if os_type == "rosa" { "dnf" } else { "apt-get" };

    let all_files: Vec<String> = match fs::read_dir(download_dir) {
        Ok(entries) => entries.filter_map(|e| e.ok()).filter_map(|e| {
            let path = e.path();
            if path.extension()?.to_str()? == extension { path.to_str().map(String::from) } else { None }
        }).collect(),
        Err(e) => return (false, format!("Ошибка чтения папки: {}", e)),
    };
    if all_files.is_empty() { return (false, format!("В папке '{}' не найдено .{} файлов", download_dir, extension)); }
    let files: Vec<String> = all_files.into_iter().filter(|f| is_package_for_arch(f, &system_arch)).collect();
    if files.is_empty() { return (false, format!("Нет пакетов под архитектуру {}", system_arch)); }
    let _ = tx.send(LogEvent::Line(format!("Найдено {} пакетов для установки", files.len())));

    if os_type == "rosa" {
        let mut cmd = Command::new(pm);
        cmd.arg("install").arg("-y").arg("--allowerasing").arg("--nobest");
        for file in &files { cmd.arg(file); }
        match run_process(cmd, tx) {
            Ok(s) if s.success() => (true, format!("Установлено пакетов: {}", files.len())),
            Ok(s) => (false, format!("Ошибка установки. Код: {:?}", s.code())),
            Err(e) => (false, format!("Не удалось запустить установку: {}", e)),
        }
    } else {
        let mut cmd = Command::new(pm);
        cmd.arg("install").arg("-f").arg("-y");
        for file in &files { cmd.arg(file); }
        match run_process(cmd, tx) {
            Ok(s) if s.success() => (true, format!("Установлено пакетов: {}", files.len())),
            Ok(s) => (false, format!("Ошибка установки. Код: {:?}", s.code())),
            Err(e) => (false, format!("Не удалось запустить {}: {}", pm, e)),
        }
    }
}

// ==================== КРИПТОПРО + ГИА ====================

fn find_local_packages(folder: &str, pattern: &str, os_type: &str) -> Vec<String> {
    let system_arch = detect_arch();
    let extension = if os_type == "rosa" { "rpm" } else { "deb" };
    let mut found = Vec::new();
    if let Ok(entries) = fs::read_dir(folder) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            let name = match path.file_name().map(|n| n.to_string_lossy().to_string()) { Some(n) => n, None => continue };
            if !name.ends_with(extension) { continue; }
            if !name.starts_with(pattern) { continue; }
            if !is_package_for_arch(&name, &system_arch) { continue; }
            found.push(path.to_string_lossy().to_string());
        }
    }
    found
}

fn download_package_to_folder(package_name: &str, folder: &str, os_type: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let _ = fs::create_dir_all(folder);
    let pm = if os_type == "rosa" { "dnf" } else { "apt-get" };
    let _ = tx.send(LogEvent::Line(format!("Скачиваю {} из репозитория...", package_name)));
    if os_type == "rosa" {
        let mut cmd = Command::new(pm);
        cmd.arg("download").arg("--resolve").arg("--alldeps").arg(format!("--destdir={}", folder)).arg(package_name);
        match run_process(cmd, tx) {
            Ok(s) if s.success() => (true, format!("{} скачан", package_name)),
            Ok(s) => (false, format!("Ошибка скачивания {}. Код: {:?}", package_name, s.code())),
            Err(e) => (false, format!("Не удалось скачать {}: {}", package_name, e)),
        }
    } else {
        let mut cmd = Command::new(pm);
        cmd.arg("install").arg("--download-only").arg("-y").arg(format!("-o Dir::Cache::Archives={}", folder)).arg(package_name);
        match run_process(cmd, tx) {
            Ok(s) if s.success() => (true, format!("{} скачан", package_name)),
            Ok(s) => (false, format!("Ошибка скачивания {}. Код: {:?}", package_name, s.code())),
            Err(e) => (false, format!("Не удалось скачать {}: {}", package_name, e)),
        }
    }
}

fn install_local_packages(folder: &str, pattern: &str, os_type: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let files = find_local_packages(folder, pattern, os_type);
    if files.is_empty() { return (false, format!("Не найдено {}*.rpm в папке {}", pattern, folder)); }
    let _ = tx.send(LogEvent::Line(format!("Устанавливаю {} пакетов: {}", files.len(), pattern)));
    let pm = if os_type == "rosa" { "dnf" } else { "apt-get" };
    if os_type == "rosa" {
        let mut cmd = Command::new(pm);
        cmd.arg("install").arg("-y").arg("--allowerasing").arg("--nobest");
        for file in &files { cmd.arg(file); }
        match run_process(cmd, tx) {
            Ok(s) if s.success() => (true, format!("{} установлены", pattern)),
            Ok(s) => (false, format!("Ошибка установки {}. Код: {:?}", pattern, s.code())),
            Err(e) => (false, format!("Не удалось установить {}: {}", pattern, e)),
        }
    } else {
        let mut cmd = Command::new(pm);
        cmd.arg("install").arg("-f").arg("-y");
        for file in &files { cmd.arg(file); }
        match run_process(cmd, tx) {
            Ok(s) if s.success() => (true, format!("{} установлены", pattern)),
            Ok(s) => (false, format!("Ошибка установки {}. Код: {:?}", pattern, s.code())),
            Err(e) => (false, format!("Не удалось установить {}: {}", pattern, e)),
        }
    }
}

fn install_cryptopro_csp(crypto_subdir: &str, os_type: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let install_script = Path::new(crypto_subdir).join("install.sh");
    let uninstall_script = Path::new(crypto_subdir).join("uninstall.sh");
    if !install_script.exists() { return (false, "В linux-amd64/ не найден install.sh КриптоПро".to_string()); }
    if !uninstall_script.exists() { return (false, "В linux-amd64/ не найден uninstall.sh КриптоПро".to_string()); }
    let _ = tx.send(LogEvent::Line("Запускаю install.sh КриптоПро (kc1, --yes)...".to_string()));
    let mut cmd = Command::new("sh");
    cmd.current_dir(crypto_subdir).arg("./install.sh").arg("kc1").arg("--yes");
    match run_process(cmd, tx) {
        Ok(s) if s.success() => (true, "КриптоПро CSP установлен".to_string()),
        Ok(s) => (false, format!("install.sh завершился с ошибкой. Код: {:?}", s.code())),
        Err(e) => (false, format!("Не удалось запустить install.sh: {}", e)),
    }
}

fn install_cryptopro_gia(crypto_dir: &str, school_dir: &str, os_type: &str, tx: &mpsc::Sender<LogEvent>) -> (bool, String) {
    let _ = fs::create_dir_all(crypto_dir);
    let _ = fs::create_dir_all(school_dir);

    // --- ШАГ 1: newt ---
    let _ = tx.send(LogEvent::Line("=== ШАГ 1/5: newt ===".to_string()));
    let _ = tx.send(LogEvent::Progress(0.0, "Шаг 1: newt...".to_string()));
    if find_local_packages(crypto_dir, "newt", os_type).is_empty() {
        let _ = tx.send(LogEvent::Line("newt не найден локально — скачиваю из репозитория...".to_string()));
        let (ok, msg) = download_package_to_folder("newt", crypto_dir, os_type, tx);
        if !ok { return (false, format!("Шаг 1 (newt): {}", msg)); }
    }
    let (ok, msg) = install_local_packages(crypto_dir, "newt", os_type, tx);
    if !ok { return (false, format!("Шаг 1 (newt): {}", msg)); }

    // --- ШАГ 2.1: pcsc-lite (зависимость для librtpkcs11ecp) ---
    let _ = tx.send(LogEvent::Line("=== ШАГ 2.1/5: pcsc-lite + pcsc-lite-ccid ===".to_string()));
    let _ = tx.send(LogEvent::Progress(0.15, "Шаг 2.1: pcsc-lite...".to_string()));
    for pkg in &["pcsc-lite", "pcsc-lite-ccid"] {
        if find_local_packages(crypto_dir, pkg, os_type).is_empty() {
            let _ = tx.send(LogEvent::Line(format!("{} не найден — скачиваю из репозитория...", pkg)));
            let (ok, msg) = download_package_to_folder(pkg, crypto_dir, os_type, tx);
            if !ok { return (false, format!("Шаг 2.1 ({}): {}", pkg, msg)); }
        }
        let (ok, msg) = install_local_packages(crypto_dir, pkg, os_type, tx);
        if !ok { return (false, format!("Шаг 2.1 ({}): {}", pkg, msg)); }
    }

    // --- ШАГ 2.2: librtpkcs11ecp ---
    let _ = tx.send(LogEvent::Line("=== ШАГ 2.2/5: librtpkcs11ecp ===".to_string()));
    let _ = tx.send(LogEvent::Progress(0.3, "Шаг 2.2: librtpkcs11ecp...".to_string()));
    if find_local_packages(crypto_dir, "librtpkcs11ecp", os_type).is_empty() {
        let _ = tx.send(LogEvent::Line("librtpkcs11ecp не найден — скачиваю с зависимостями...".to_string()));
        let (ok, msg) = download_package_to_folder("librtpkcs11ecp", crypto_dir, os_type, tx);
        if !ok { return (false, format!("Шаг 2.2 (librtpkcs11ecp): {}", msg)); }
    }
    let (ok, msg) = install_local_packages(crypto_dir, "librtpkcs11ecp", os_type, tx);
    if !ok { return (false, format!("Шаг 2.2 (librtpkcs11ecp): {}", msg)); }

    // --- ШАГ 3: КриптоПро CSP ---
    let _ = tx.send(LogEvent::Line("=== ШАГ 3/5: КриптоПро CSP ===".to_string()));
    let _ = tx.send(LogEvent::Progress(0.5, "Шаг 3: КриптоПро...".to_string()));
    let crypto_subdir = Path::new(crypto_dir).join("linux-amd64");
    let (ok, msg) = install_cryptopro_csp(&crypto_subdir.to_string_lossy(), os_type, tx);
    if !ok { return (false, format!("Шаг 3 (КриптоПро): {}", msg)); }

    // --- ШАГ 4: GIA11_PPE ---
    let _ = tx.send(LogEvent::Line("=== ШАГ 4/5: GIA11_PPE ===".to_string()));
    let _ = tx.send(LogEvent::Progress(0.75, "Шаг 4: GIA11_PPE...".to_string()));
    if find_local_packages(crypto_dir, "GIA11_PPE", os_type).is_empty() {
        return (false, "GIA11_PPE*.rpm не найден в cryptopro_gia. Положите файл и повторите.".to_string());
    }
    let (ok, msg) = install_local_packages(crypto_dir, "GIA11_PPE", os_type, tx);
    if !ok { return (false, format!("Шаг 4 (GIA11_PPE): {}", msg)); }

    // --- ШАГ 5: Запуск pcscd (демон смарт-карт) ---
    let _ = tx.send(LogEvent::Line("=== ШАГ 5/5: Запуск pcscd ===".to_string()));
    let _ = tx.send(LogEvent::Progress(0.9, "Шаг 5: запуск pcscd...".to_string()));
    let mut cmd = Command::new("systemctl");
    cmd.arg("enable").arg("--now").arg("pcscd");
    match run_process(cmd, tx) {
        Ok(s) if s.success() => { let _ = tx.send(LogEvent::Line("✅ pcscd запущен и добавлен в автозагрузку".to_string())); }
        _ => { let _ = tx.send(LogEvent::Line("⚠ Не удалось запустить pcscd — запустите вручную: systemctl enable --now pcscd".to_string())); }
    }

    (true, "Установлено: newt, pcsc-lite, librtpkcs11ecp, КриптоПро CSP, GIA11_PPE".to_string())
}

fn get_packages() -> Vec<&'static str> {
    vec!["task-koge", "r7-office", "notepadqq"]
}

// ==================== ДИСПЕТЧЕР КОМАНД ====================

fn run_command(cmd_id: &str, os_type: String, tx: &mpsc::Sender<LogEvent>) {
    let needs_prepare = matches!(cmd_id,
        "download_updates" | "install_updates" | "download_packages" | "install_packages" | "install_cryptopro_gia");
    if needs_prepare {
        let (ok, msg) = prepare_environment(&os_type, tx);
        if !ok {
            let _ = tx.send(LogEvent::Done(false, format!("Подготовка окружения: {}", msg)));
            return;
        }
    }

    let download_dir = get_school_packages_dir();
    let cert_dir = get_cert_dir();
    let crypto_dir = get_crypto_dir();

    let result: (bool, String) = match cmd_id {
        "setup_env" => prepare_environment(&os_type, tx),
        "download_updates" => download_updates(&download_dir, &os_type, tx),
        "install_updates" => {
            if !Path::new(&download_dir).exists() {
                (false, "Папка не найдена. Сначала скачайте обновления.".to_string())
            } else { install_from_folder(&download_dir, &os_type, tx) }
        }
        "download_packages" => download_packages(&get_packages(), &download_dir, &os_type, tx),
        "install_packages" => {
            if !Path::new(&download_dir).exists() {
                (false, "Папка не найдена. Сначала скачайте пакеты.".to_string())
            } else { install_from_folder(&download_dir, &os_type, tx) }
        }
        "install_cryptopro_gia" => install_cryptopro_gia(&crypto_dir, &download_dir, &os_type, tx),
        "detect_os" => {
            let mut info = format!("ОС: {} | Арх: {}", os_type, detect_arch());
            info += &format!("\n\nТекущие настройки:");
            info += &format!("\n  Прокси: {}", get_proxy_url());
            info += &format!("\n  Папка сертификатов: {}", cert_dir);
            info += &format!("\n  Папка КриптоПро: {}", crypto_dir);
            info += &format!("\n  Папка пакетов: {}", download_dir);
            info += &format!("\n\nПапки:");
            info += &format!("\n  school_packages: {}", Path::new(&download_dir).exists());
            info += &format!("\n  cryptopro_gia: {}", Path::new(&crypto_dir).exists());
            info += &format!("\n  cer: {}", Path::new(&cert_dir).exists());
            info += &format!("\n  install.sh: {}", Path::new(&format!("{}/linux-amd64/install.sh", crypto_dir)).exists());
            if Path::new(&download_dir).exists() {
                info += &format!("\n  school_packages: {:.2} MB", get_folder_size(&download_dir) as f64 / 1048576.0);
            }
            if Path::new(&crypto_dir).exists() {
                info += &format!("\n  cryptopro_gia: {:.2} MB", get_folder_size(&crypto_dir) as f64 / 1048576.0);
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
    CommandItem { id: "setup_env", name: "Настроить прокси + сертификат", description: "ШАГ 1: прокси (из настроек) в dnf.conf/apt.conf + /etc/profile.d/proxy.sh. ШАГ 2: сертификаты из папки 'cer' (или из настроек) в системное хранилище и NSS-базу Chromium.", icon: "🌐" },
    CommandItem { id: "download_updates", name: "Скачать обновления системы", description: "Прокси+сертификат → скачивает обновления в папку пакетов. Нужен интернет.", icon: "⬇" },
    CommandItem { id: "install_updates", name: "Установить обновления", description: "Прокси+сертификат → устанавливает из папки пакетов офлайн.", icon: "⬆" },
    CommandItem { id: "download_packages", name: "Скачать пакеты (task-koge)", description: "Прокси+сертификат → скачивает task-koge + зависимости + офис.", icon: "📦" },
    CommandItem { id: "install_packages", name: "Установить пакеты", description: "Прокси+сертификат → устанавливает из папки пакетов офлайн.", icon: "🚀" },
    CommandItem { id: "install_cryptopro_gia", name: "Установить КриптоПро + ГИА", description: "Прокси+сертификат → 4 шага: newt → librtpkcs11ecp → КриптоПро → GIA11_PPE.", icon: "🔐" },
    CommandItem { id: "detect_os", name: "Информация о системе", description: "ОС, архитектура, текущие настройки, наличие папок и install.sh, размеры.", icon: "ℹ" },
    CommandItem { id: "exit", name: "Выход", description: "Завершить программу.", icon: "✕" },
];

struct App {
    selected: usize,
    list_state: ListState,
    status: String,
    is_running: bool,
    os_type: String,
    arch: String,
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
            selected: 0, list_state,
            status: "Готов к работе. Выберите команду (↑↓, Enter).".to_string(),
            is_running: false,
            os_type: detect_os(), arch: detect_arch(),
            progress: 0.0, progress_label: String::new(),
            log_lines: Vec::new(), log_rx: None,
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
        thread::spawn(move || { run_command(cmd.id, os_type, &tx); });
        self.log_rx = Some(rx);
    }
    fn poll_events(&mut self) {
        let mut rx = self.log_rx.take();
        if let Some(receiver) = &rx {
            while let Ok(ev) = receiver.try_recv() {
                match ev {
                    LogEvent::Line(s) => {
                        self.log_lines.push(s);
                        if self.log_lines.len() > 600 { self.log_lines.remove(0); }
                    }
                    LogEvent::Progress(p, label) => { self.progress = p; self.progress_label = label; }
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
        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(3), Constraint::Length(3)])
        .split(f.area());

    let header = Paragraph::new(vec![Line::from(vec![
        Span::styled(" ШКОЛЬНЫЙ УСТАНОВЩИК ПО ", Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(format!("ОС: {} | Арх: {}", app.os_type.to_uppercase(), app.arch), Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
    ])])
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)));
    f.render_widget(header, chunks[0]);

    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(chunks[1]);

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
        .block(Block::default().title(" КОМАНДЫ (↑↓, Enter, q) ").borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)))
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

    let status_color = if app.status.starts_with("✓") { Color::Green } else if app.status.starts_with("✗") { Color::Red } else if app.status.starts_with("▶") { Color::Yellow } else { Color::White };
    let status_widget = Paragraph::new(vec![Line::from(vec![
        Span::styled(" Статус: ", Style::default().fg(Color::Cyan)),
        Span::styled(app.status.as_str(), Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
    ])])
    .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)));
    f.render_widget(status_widget, chunks[3]);
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();

    // CLI-режим для управления настройками
    if args.len() >= 2 {
        let mode = args[1].trim_start_matches("--");
        match mode {
            "set-proxy" => {
                if args.len() < 3 {
                    println!("Использование: --set-proxy <URL>");
                    println!("Пример: --set-proxy http://10.0.54.52:3128");
                    return Ok(());
                }
                let url = &args[2];
                save_setting("PROXY_URL", url)?;
                println!("✅ Прокси установлен: {}", url);
                println!("   Файл настроек: {}", SETTINGS_FILE);
                return Ok(());
            }
            "set-cert-dir" => {
                if args.len() < 3 {
                    println!("Использование: --set-cert-dir <PATH>");
                    println!("Пример: --set-cert-dir /media/usb/certificates");
                    return Ok(());
                }
                let path = &args[2];
                save_setting("CERT_DIR", path)?;
                println!("✅ Папка сертификатов: {}", path);
                return Ok(());
            }
            "set-crypto-dir" => {
                if args.len() < 3 {
                    println!("Использование: --set-crypto-dir <PATH>");
                    return Ok(());
                }
                let path = &args[2];
                save_setting("CRYPTO_DIR", path)?;
                println!("✅ Папка КриптоПро: {}", path);
                return Ok(());
            }
            "set-packages-dir" => {
                if args.len() < 3 {
                    println!("Использование: --set-packages-dir <PATH>");
                    return Ok(());
                }
                let path = &args[2];
                save_setting("SCHOOL_PACKAGES_DIR", path)?;
                println!("✅ Папка пакетов: {}", path);
                return Ok(());
            }
            "show-settings" => {
                println!("=== Текущие настройки ===\n");
                println!("Файл: {}", SETTINGS_FILE);
                println!("Прокси: {}", get_proxy_url());
                println!("Папка сертификатов: {}", get_cert_dir());
                println!("Папка КриптоПро: {}", get_crypto_dir());
                println!("Папка пакетов: {}", get_school_packages_dir());
                println!("\n=== Приоритет настроек ===");
                println!("1. CLI-аргументы (этот запуск)");
                println!("2. Переменные окружения (SCHOOL_PROXY_URL, SCHOOL_CERT_DIR, ...)");
                println!("3. Файл настроек ({})", SETTINGS_FILE);
                println!("4. Значения по умолчанию");
                return Ok(());
            }
            "reset-settings" => {
                let _ = fs::remove_file(SETTINGS_FILE);
                println!("✅ Настройки сброшены к значениям по умолчанию");
                println!("   Прокси: {}", DEFAULT_PROXY_URL);
                println!("   Папка сертификатов: {}", DEFAULT_CERT_DIR);
                println!("   Папка КриптоПро: {}", DEFAULT_CRYPTO_DIR);
                println!("   Папка пакетов: {}", DEFAULT_SCHOOL_PACKAGES_DIR);
                return Ok(());
            }
            _ => {}
        }
    }

    // CLI-режим для рабочих команд
    if args.len() > 1 {
        let mode = args[1].trim_start_matches("--");
        let os_type = detect_os();
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
        let os_c = os_type.clone();
        thread::spawn(move || run_command(cmd_id, os_c, &tx));
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
