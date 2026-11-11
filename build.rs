//! Сборка: на Windows встраивает в исполняемый файл иконку и сведения о версии.
//!
//! Иконка — знак с сайта продукта (lastcopy.ru), файлы готовит `scripts/make-icons.py`.
//! Ресурс компилируется `rc.exe` из Windows SDK. Этот компилятор не понимает пути
//! с не-ASCII символами (кириллица в профиле пользователя его ломает), поэтому все
//! аргументы передаются в коротком формате 8.3. Если короткий путь получить не удалось
//! или rc.exe не найден, сборка продолжается без иконки — это предупреждение, не ошибка.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=packaging/windows/site-restorer.rc.in");
    println!("cargo:rerun-if-changed=packaging/icons/site-restorer.ico");
    println!("cargo:rerun-if-changed=packaging/windows/site-restorer.manifest");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let nums = version
        .split('.')
        .map(|p| p.parse::<u16>().unwrap_or(0))
        .collect::<Vec<_>>();
    let version_num = format!(
        "{},{},{},0",
        nums.first().copied().unwrap_or(0),
        nums.get(1).copied().unwrap_or(0),
        nums.get(2).copied().unwrap_or(0)
    );
    // Манифесту нужна версия с точками и четырьмя числами: с «3.5.2» Windows
    // не запускает программу (ошибка 14001, «параллельная конфигурация неверна»).
    let version_dot = format!(
        "{}.{}.{}.0",
        nums.first().copied().unwrap_or(0),
        nums.get(1).copied().unwrap_or(0),
        nums.get(2).copied().unwrap_or(0)
    );

    let template = match fs::read_to_string(manifest.join("packaging/windows/site-restorer.rc.in"))
    {
        Ok(t) => t,
        Err(err) => {
            println!("cargo:warning=шаблон ресурса не прочитан ({err}): сборка без иконки");
            return;
        }
    };
    // Иконку кладём рядом с сгенерированным ресурсом: rc.exe запускается в этом каталоге
    // и ищет её по относительному пути (кириллица в абсолютном ломает компилятор ресурсов).
    let icon = manifest.join("packaging/icons/site-restorer.ico");
    if fs::copy(&icon, out.join("site-restorer.ico")).is_err() {
        println!("cargo:warning=иконка не скопирована: сборка без иконки");
        return;
    }
    // Манифест тоже кладём рядом: rc.exe ищет его по относительному пути.
    let manifest_src = manifest.join("packaging/windows/site-restorer.manifest");
    match fs::read_to_string(&manifest_src) {
        Ok(text) => {
            let filled = text.replace("{{VERSION_DOT}}", &version_dot);
            if fs::write(out.join("site-restorer.manifest"), filled).is_err() {
                println!("cargo:warning=манифест не записан: сборка без манифеста");
            }
        }
        Err(err) => println!("cargo:warning=манифест не прочитан ({err}): сборка без манифеста"),
    }
    let source = template
        .replace("packaging/icons/site-restorer.ico", "site-restorer.ico")
        .replace(
            "packaging/windows/site-restorer.manifest",
            "site-restorer.manifest",
        )
        .replace("{{VERSION}}", &version)
        .replace("{{VERSION_NUM}}", &version_num);
    let rc_name = "site-restorer.rc";
    if fs::write(out.join(rc_name), source).is_err() {
        println!("cargo:warning=не удалось записать ресурс иконки, сборка без иконки");
        return;
    }

    let Some(rc_exe) = find_rc() else {
        println!("cargo:warning=rc.exe не найден: сборка без иконки и сведений о версии");
        return;
    };

    // rc.exe принимает только ASCII-пути: кириллица в профиле пользователя даёт RC1110.
    let Some(short_dir) = short_path(&out) else {
        println!("cargo:warning=короткий путь недоступен: сборка без иконки");
        return;
    };

    let status = Command::new(&rc_exe)
        .current_dir(&short_dir)
        .arg("/nologo")
        .arg("/fo")
        .arg("site-restorer.res")
        .arg(rc_name)
        .status();
    let res_path = out.join("site-restorer.res");
    match status {
        Ok(s) if s.success() && res_path.exists() => {
            println!(
                "cargo:rustc-link-arg={}",
                short_dir.join("site-restorer.res").display()
            );
        }
        Ok(s) => println!(
            "cargo:warning=rc.exe завершился с кодом {}: сборка без иконки",
            s
        ),
        Err(err) => println!("cargo:warning=rc.exe не запустился ({err}): сборка без иконки"),
    }
}

/// Ищет компилятор ресурсов: сначала рядом с установленным SDK из окружения, затем в Windows Kits.
fn find_rc() -> Option<PathBuf> {
    if let Ok(dir) = env::var("WindowsSdkDir")
        && let Some(found) = newest_rc(&PathBuf::from(dir).join("bin"))
    {
        return Some(found);
    }
    for root in [
        r"C:\Program Files (x86)\Windows Kits\10\bin",
        r"C:\Program Files\Windows Kits\10\bin",
    ] {
        if let Some(found) = newest_rc(Path::new(root)) {
            return Some(found);
        }
    }
    None
}

/// Берёт rc.exe из самого свежего каталога версии SDK, предпочитая x64, затем x86.
fn newest_rc(base: &Path) -> Option<PathBuf> {
    let mut versions: Vec<PathBuf> = fs::read_dir(base)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    versions.sort();
    for version in versions.iter().rev() {
        for arch in ["x64", "x86", "arm64"] {
            let candidate = version.join(arch).join("rc.exe");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(windows)]
fn short_path(path: &Path) -> Option<PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetShortPathNameW(long: *const u16, short: *mut u16, size: u32) -> u32;
    }

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut buffer = vec![0u16; 32_768];
    let len = unsafe { GetShortPathNameW(wide.as_ptr(), buffer.as_mut_ptr(), buffer.len() as u32) };
    if len == 0 || len as usize > buffer.len() {
        return None;
    }
    buffer.truncate(len as usize);
    Some(PathBuf::from(std::ffi::OsString::from_wide(&buffer)))
}

#[cfg(not(windows))]
fn short_path(path: &Path) -> Option<PathBuf> {
    Some(path.to_path_buf())
}
