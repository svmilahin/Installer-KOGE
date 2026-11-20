use std::env;
use std::fs;
use std::process::Command;

/// Определяем ID операционной системы
fn detect_os_id() -> String {
    if let Ok(content) = fs::read_to_string("/etc/os-release") {
        for line in content.lines() {
            if line.starts_with("ID=") {
                return line
                    .trim_start_matches("ID=")
                    .trim_matches('"')
                    .to_lowercase();
            }
        }
    }
    "unknown".to_string()
}

/// Формируем список пакетов.
/// Главное волшебство здесь — метапакет task-koge!
fn get_packages() -> Vec<&'static str> {
    vec![
        // 🎯 ОФИЦИАЛЬНЫЙ метапакет для ОГЭ/ЕГЭ по информатике в МОС ОС.
        // Он автоматически потянет за собой: Кумир, Python, PascalABC, CodeBlocks, нужные IDE и т.д.
        "task-koge",
        // Дополнительные полезные программы для школы, которые могут не входить в экзаменационный набор,
        // но всегда нужны учителю и ученикам:
        "r7-office", // Российский офисный пакет
        "notepadqq", // Отличный аналог Notepad++ для Linux
    ]
}

/// Скачиваем пакеты со всеми зависимостями в локальную папку
fn download_packages(packages: &[&str], download_dir: &str) -> bool {
    println!("\n[↓] Режим: СКАЧИВАНИЕ (требуется интернет)");
    println!("[*] Создаю папку: {}", download_dir);
    let _ = fs::create_dir_all(download_dir);

    println!("[*] Скачиваю метапакет и разрешаю все его зависимости...");
    println!("[!] Это может занять время, так как будет скачан весь экзаменационный софт.");

    let mut cmd = Command::new("dnf");
    cmd.arg("download")
        .arg("--resolve") // Разрешить зависимости
        .arg("--alldeps") // Скачать все зависимости, а не только недостающие
        .arg(format!("--destdir={}", download_dir));

    for pkg in packages {
        cmd.arg(pkg);
    }

    match cmd.status() {
        Ok(status) if status.success() => {
            println!("[+] Пакеты успешно скачаны в папку '{}'", download_dir);

            // Небольшая проверка: покажем, сколько файлов скачалось, чтобы пользователь не пугался,
            // что task-koge весит мало (он мало весит, но зависимости тяжелые).
            if let Ok(entries) = fs::read_dir(download_dir) {
                let count = entries.filter_map(|e| e.ok()).count();
                println!("[i] Всего файлов в папке: {}", count);
            }
            true
        }
        Ok(status) => {
            println!("[-] Ошибка скачивания. Код: {:?}", status.code());
            false
        }
        Err(e) => {
            println!("[-] Не удалось запустить dnf: {}", e);
            false
        }
    }
}

/// Устанавливаем пакеты из локальной папки
fn install_from_local(download_dir: &str) -> bool {
    println!("\n[↑] Режим: УСТАНОВКА (работает без интернета)");
    println!("[*] Ищу .rpm файлы в папке: {}", download_dir);

    let rpm_files: Vec<String> = match fs::read_dir(download_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let path = e.path();
                if path.extension()?.to_str()? == "rpm" {
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

    if rpm_files.is_empty() {
        println!("[-] В папке не найдено .rpm файлов!");
        println!("[!] Сначала запусти программу с флагом --download на машине с интернетом.");
        return false;
    }

    println!(
        "[*] Найдено {} пакетов для локальной установки",
        rpm_files.len()
    );

    let mut cmd = Command::new("dnf");
    cmd.arg("install").arg("-y");

    for rpm in &rpm_files {
        cmd.arg(rpm);
    }

    println!("[*] Выполняю локальную установку (может занять время)...");
    match cmd.status() {
        Ok(status) if status.success() => {
            println!("[+] Установка из локальной папки завершена успешно!");
            true
        }
        Ok(status) => {
            println!("[-] Ошибка установки. Код: {:?}", status.code());
            false
        }
        Err(e) => {
            println!("[-] Не удалось запустить установку: {}", e);
            false
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();

    let mode = if args.contains(&"--download".to_string()) {
        "download"
    } else if args.contains(&"--install".to_string()) {
        "install"
    } else {
        "both"
    };

    println!("=== Школьный установщик ПО (МОС ОС / РОСА) ===");
    println!(
        "[*] Режим работы: {}",
        match mode {
            "download" => "Только скачивание",
            "install" => "Только установка из папки",
            "both" => "Скачивание и установка",
            _ => "Неизвестный",
        }
    );

    let os_id = detect_os_id();
    println!("[*] Обнаружена ОС: {}", os_id);

    if !os_id.contains("rosa") && !os_id.contains("mosos") {
        println!("[!] Внимание: ОС не похожа на РОСА/МОС ОС. Убедись, что установлен dnf.");
    }

    let download_dir = "school_packages";
    let packages = get_packages();

    println!("[*] Целевые пакеты: {:?}", packages);

    if mode == "download" || mode == "both" {
        if !download_packages(&packages, download_dir) {
            println!("[-] Не удалось скачать пакеты. Завершаю работу.");
            std::process::exit(1);
        }
    }

    if mode == "install" || mode == "both" {
        if !install_from_local(download_dir) {
            println!("[-] Ошибка при установке. Завершаю работу.");
            std::process::exit(1);
        }
    }

    println!("\n[+] Готово! Компьютер полностью настроен для экзамена.");
}
