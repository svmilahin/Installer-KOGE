use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

/// Определяем тип ОС по /etc/os-release
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

/// Получаем размер папки рекурсивно (в байтах)
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

/// Архивируем папку с пакетами в tar.gz
fn compress_folder(folder: &str, archive: &str) -> bool {
    println!("\n[📦] Архивирую папку {} в {}...", folder, archive);

    let size_before = get_folder_size(folder);
    println!("[i] Размер папки до сжатия: {:.2} MB", size_before as f64 / 1024.0 / 1024.0);

    // Удаляем старый архив, если есть
    let _ = fs::remove_file(archive);

    let status = Command::new("tar")
        .arg("-czf")
        .arg(archive)
        .arg(folder)
        .status();

    match status {
        Ok(s) if s.success() => {
            if let Ok(meta) = fs::metadata(archive) {
                let size_after = meta.len();
                let ratio = if size_before > 0 {
                    (1.0 - size_after as f64 / size_before as f64) * 100.0
                } else {
                    0.0
                };
                println!("[+] Архив создан: {:.2} MB", size_after as f64 / 1024.0 / 1024.0);
                println!("[+] Экономия места: {:.1}%", ratio);
            }
            true
        }
        Ok(s) => {
            println!("[-] Ошибка архивации. Код: {:?}", s.code());
            false
        }
        Err(e) => {
            println!("[-] Не удалось запустить tar: {}", e);
            false
        }
    }
}

/// Распаковываем архив tar.gz
fn decompress_archive(archive: &str, folder: &str) -> bool {
    println!("\n[📂] Распаковываю архив {}...", archive);

    if !Path::new(archive).exists() {
        println!("[-] Архив не найден: {}", archive);
        return false;
    }

    if let Ok(meta) = fs::metadata(archive) {
        println!("[i] Размер архива: {:.2} MB", meta.len() as f64 / 1024.0 / 1024.0);
    }

    // Создаём папку, если её нет
    let _ = fs::create_dir_all(folder);

    let status = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(".")
        .status();

    match status {
        Ok(s) if s.success() => {
            println!("[+] Архив распакован в {}", folder);
            true
        }
        Ok(s) => {
            println!("[-] Ошибка распаковки. Код: {:?}", s.code());
            false
        }
        Err(e) => {
            println!("[-] Не удалось запустить tar: {}", e);
            false
        }
    }
}

/// Скачиваем обновления системы в папку
fn download_updates(download_dir: &str, os_type: &str) -> bool {
    println!("\n[↓] Скачивание обновлений системы...");
    println!("[*] Папка: {}", download_dir);
    let _ = fs::create_dir_all(download_dir);

    let pm = if os_type == "rosa" { "dnf" } else { "apt-get" };

    match os_type {
        "rosa" => {
            let status = Command::new(pm)
                .arg("upgrade")
                .arg("--downloadonly")
                .arg(format!("--destdir={}", download_dir))
                .arg("-y")
                .status();

            match status {
                Ok(s) if s.success() => {
                    println!("[+] Обновления скачаны");
                    true
                }
                Ok(s) => {
                    println!("[-] Ошибка скачивания. Код: {:?}", s.code());
                    false
                }
                Err(e) => {
                    println!("[-] Не удалось запустить {}: {}", pm, e);
                    false
                }
            }
        }
        "alt" => {
            println!("[*] Обновляем списки пакетов...");
            let update_status = Command::new(pm).arg("update").status();
            if let Ok(s) = update_status {
                if !s.success() {
                    println!("[-] Не удалось обновить списки пакетов");
                    return false;
                }
            }

            println!("[*] Получаем список обновлений...");
            let sim_output = Command::new(pm)
                .arg("upgrade")
                .arg("--simulate")
                .output();

            let packages_to_update: Vec<String> = match sim_output {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    stdout
                        .lines()
                        .filter(|line| line.starts_with("Inst "))
                        .filter_map(|line| {
                            let parts: Vec<&str> = line.split_whitespace().collect();
                            if parts.len() >= 2 {
                                Some(parts[1].to_string())
                            } else {
                                None
                            }
                        })
                        .collect()
                }
                Err(e) => {
                    println!("[-] Ошибка получения списка обновлений: {}", e);
                    return false;
                }
            };

            if packages_to_update.is_empty() {
                println!("[i] Система уже обновлена, новых пакетов нет");
                return true;
            }

            println!("[*] Пакетов для обновления: {}", packages_to_update.len());

            let mut all_success = true;
            for pkg in &packages_to_update {
                println!("[*] Скачиваю: {}", pkg);
                let status = Command::new(pm)
                    .arg("install")
                    .arg("--download-only")
                    .arg("-y")
                    .arg(format!("-o Dir::Cache::Archives={}", download_dir))
                    .arg(pkg)
                    .status();

                if let Ok(s) = status {
                    if !s.success() {
                        println!("[-] Ошибка скачивания {}", pkg);
                        all_success = false;
                    }
                } else {
                    all_success = false;
                }
            }

            if all_success {
                println!("[+] Все обновления скачаны");
            } else {
                println!("[-] Некоторые пакеты не удалось скачать");
            }
            all_success
        }
        _ => {
            println!("[-] Неизвестная ОС");
            false
        }
    }
}

/// Скачиваем пакеты (task-koge и др.) со всеми зависимостями
fn download_packages(packages: &[&str], download_dir: &str, os_type: &str) -> bool {
    println!("\n[↓] Скачивание пакетов: {:?}", packages);
    println!("[*] Папка: {}", download_dir);
    let _ = fs::create_dir_all(download_dir);

    let pm = if os_type == "rosa" { "dnf" } else { "apt-get" };

    match os_type {
        "rosa" => {
            let mut cmd = Command::new(pm);
            cmd.arg("download")
                .arg("--resolve")
                .arg("--alldeps")
                .arg(format!("--destdir={}", download_dir));
            for pkg in packages {
                cmd.arg(pkg);
            }
            match cmd.status() {
                Ok(s) if s.success() => {
                    println!("[+] Пакеты скачаны");
                    true
                }
                Ok(s) => {
                    println!("[-] Ошибка скачивания. Код: {:?}", s.code());
                    false
                }
                Err(e) => {
                    println!("[-] Не удалось запустить {}: {}", pm, e);
                    false
                }
            }
        }
        "alt" => {
            let mut all_success = true;
            for pkg in packages {
                println!("[*] Скачиваю: {}", pkg);
                let status = Command::new(pm)
                    .arg("install")
                    .arg("--download-only")
                    .arg("-y")
                    .arg(format!("-o Dir::Cache::Archives={}", download_dir))
                    .arg(pkg)
                    .status();

                if let Ok(s) = status {
                    if !s.success() {
                        println!("[-] Ошибка скачивания {}", pkg);
                        all_success = false;
                    }
                } else {
                    all_success = false;
                }
            }
            if all_success {
                println!("[+] Все пакеты скачаны");
            } else {
                println!("[-] Некоторые пакеты не удалось скачать");
            }
            all_success
        }
        _ => {
            println!("[-] Неизвестная ОС");
            false
        }
    }
}

/// Устанавливаем пакеты из папки (с флагом --allowerasing для ROSA)
fn install_from_folder(download_dir: &str, os_type: &str) -> bool {
    println!("\n[↑] Установка пакетов из папки...");

    let extension = if os_type == "rosa" { "rpm" } else { "deb" };
    let pm = if os_type == "rosa" { "dnf" } else { "apt-get" };

    let files: Vec<String> = match fs::read_dir(download_dir) {
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
        Err(e) => {
            println!("[-] Ошибка чтения папки: {}", e);
            return false;
        }
    };

    if files.is_empty() {
        println!("[-] В папке не найдено .{} файлов", extension);
        return false;
    }

    println!("[*] Найдено {} пакетов", files.len());

    match os_type {
        "rosa" => {
            // --allowerasing разрешает замену конфликтующих пакетов (обои, брендинг)
            // --nobest позволяет использовать не только лучшие версии
            let mut cmd = Command::new(pm);
            cmd.arg("install")
                .arg("-y")
                .arg("--allowerasing")
                .arg("--nobest");
            for file in &files {
                cmd.arg(file);
            }
            match cmd.status() {
                Ok(s) if s.success() => {
                    println!("[+] Пакеты установлены");
                    true
                }
                Ok(s) => {
                    println!("[-] Ошибка установки. Код: {:?}", s.code());
                    false
                }
                Err(e) => {
                    println!("[-] Не удалось запустить установку: {}", e);
                    false
                }
            }
        }
        "alt" => {
            let mut cmd = Command::new("dpkg");
            cmd.arg("-i");
            for file in &files {
                cmd.arg(file);
            }
            match cmd.status() {
                Ok(s) if s.success() => {
                    println!("[+] Пакеты установлены через dpkg");
                    println!("[*] Исправляю зависимости...");
                    let _ = Command::new(pm)
                        .arg("install")
                        .arg("-f")
                        .arg("-y")
                        .status();
                    true
                }
                Ok(s) => {
                    println!("[-] Ошибка установки. Код: {:?}", s.code());
                    println!("[!] Попробуйте вручную: apt-get install -f -y");
                    false
                }
                Err(e) => {
                    println!("[-] Не удалось запустить dpkg: {}", e);
                    false
                }
            }
        }
        _ => {
            println!("[-] Неизвестная ОС");
            false
        }
    }
}

/// Список пакетов для установки (можно расширять)
fn get_packages() -> Vec<&'static str> {
    vec![
        "task-koge",        // Метапакет для экзамена по информатике
        "r7-office",        // Российский офисный пакет
        "notepadqq",        // Аналог Notepad++ для Linux
    ]
}

fn main() {
    let args: Vec<String> = env::args().collect();

    let mode = if args.contains(&"--download-updates".to_string()) {
        "download-updates"
    } else if args.contains(&"--install-updates".to_string()) {
        "install-updates"
    } else if args.contains(&"--download-packages".to_string()) {
        "download-packages"
    } else if args.contains(&"--install-packages".to_string()) {
        "install-packages"
    } else {
        "help"
    };

    let os_type = detect_os();
    println!("=== Школьный установщик ===");
    println!("[*] ОС: {}", os_type);
    println!("[*] Режим: {}", mode);

    if os_type == "unknown" {
        println!("[!] Не удалось определить ОС. Продолжаю в режиме ROSA (по умолчанию).");
    }

    let download_dir = "school_packages";
    let archive_name = "school_packages.tar.gz";

    match mode {
        "download-updates" => {
            if !download_updates(download_dir, &os_type) {
                println!("[-] Не удалось скачать обновления");
                std::process::exit(1);
            }
            if !compress_folder(download_dir, archive_name) {
                println!("[-] Не удалось создать архив");
                std::process::exit(1);
            }
            println!("\n[+] Готово! Перенеси файл {} на флешке в класс", archive_name);
        }
        "install-updates" => {
            if Path::new(archive_name).exists() {
                if !decompress_archive(archive_name, download_dir) {
                    std::process::exit(1);
                }
            } else if !Path::new(download_dir).exists() {
                println!("[-] Не найден ни архив {}, ни папка {}", archive_name, download_dir);
                std::process::exit(1);
            }
            if !install_from_folder(download_dir, &os_type) {
                std::process::exit(1);
            }
            println!("\n[+] Обновления установлены!");
        }
        "download-packages" => {
            let packages = get_packages();
            if !download_packages(&packages, download_dir, &os_type) {
                println!("[-] Не удалось скачать пакеты");
                std::process::exit(1);
            }
            if !compress_folder(download_dir, archive_name) {
                println!("[-] Не удалось создать архив");
                std::process::exit(1);
            }
            println!("\n[+] Готово! Перенеси файл {} на флешке в класс", archive_name);
        }
        "install-packages" => {
            if Path::new(archive_name).exists() {
                if !decompress_archive(archive_name, download_dir) {
                    std::process::exit(1);
                }
            } else if !Path::new(download_dir).exists() {
                println!("[-] Не найден ни архив {}, ни папка {}", archive_name, download_dir);
                std::process::exit(1);
            }
            if !install_from_folder(download_dir, &os_type) {
                std::process::exit(1);
            }
            println!("\n[+] Пакеты установлены!");
        }
        "help" | _ => {
            println!("\nИспользование:");
            println!("  ./school-installer --download-updates    # Скачать обновления системы + архив");
            println!("  ./school-installer --install-updates     # Распаковать и установить обновления");
            println!("  ./school-installer --download-packages   # Скачать пакеты (task-koge и др.) + архив");
            println!("  ./school-installer --install-packages    # Распаковать и установить пакеты");
            println!("\nПример для школы:");
            println!("  1. На машине с интернетом:");
            println!("     sudo ./school-installer --download-packages");
            println!("     -> Создаст school_packages.tar.gz");
            println!("  2. Скопировать school_packages.tar.gz на флешку");
            println!("  3. В классе без интернета:");
            println!("     sudo ./school-installer --install-packages");
            println!("     -> Автоматически распакует и установит");
            println!("\nПоддерживаемые ОС: МОС ОС (ROSA), Alt Linux Education");
        }
    }
}
