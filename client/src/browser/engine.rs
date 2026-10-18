//! Какой хост браузера запускать.
//!
//! * Современный — CEF 150, папка игры `cef-modern\`: кадр интерфейса общей текстурой видеокарты
//!   и по запросу хоста с частотой игры (host/src/pacing.rs).
//! * Совместимый — CEF 89, папка `cef\`, как было: кадр читается с видеокарты в память, не больше
//!   ~40–60 к/с. Запасной путь, если современный не работает на этом компьютере.
//!
//! Выбор:
//! * `DELIRIUM_CEF_ENGINE=legacy | modern | auto` — переменная окружения игры, её ставит лаунчер
//!   (галочка «Совместимый режим интерфейса»); нет — `engine` из `cef\cef_host.ini` (его раздаёт
//!   лаунчер — так мы можем вернуть всех на совместимый без новой сборки); нет и там — auto;
//! * auto — современный, если он есть и не отключён после сбоя (`cef-modern\disabled.txt`);
//! * сбой современного (не запустился, не ответил за [`MODERN_START_TIMEOUT`], упал дважды за
//!   [`MODERN_CRASH_WINDOW`]) — до конца игры совместимый (manager.rs), а в `disabled.txt` —
//!   размер и время изменения `cef_host.exe`: обновит лаунчер хост — отметка не совпадёт, и
//!   современный попробуется снова.

use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

pub const MODERN_START_TIMEOUT: Duration = Duration::from_secs(20);
pub const MODERN_CRASH_WINDOW: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Modern,
    Legacy,
}

impl Engine {
    /// Папка хоста (cef_host.exe, renderer.exe, libcef.dll и ресурсы CEF).
    pub fn dir(self) -> PathBuf {
        match self {
            Engine::Modern => crate::utils::game_dir().join("cef-modern"),
            Engine::Legacy => crate::utils::cef_dir(),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Engine::Modern => "современный (CEF 150, cef-modern)",
            Engine::Legacy => "совместимый (CEF 89, cef)",
        }
    }
}

fn marker() -> PathBuf {
    Engine::Modern.dir().join("disabled.txt")
}

/// Отпечаток cef_host.exe современного хоста: размер и время изменения.
fn stamp() -> Option<String> {
    let meta = std::fs::metadata(Engine::Modern.dir().join("cef_host.exe")).ok()?;
    let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(format!("{} {}", meta.len(), modified))
}

/// Современный отключён после сбоя этой же версии хоста.
fn modern_disabled() -> bool {
    let (Ok(text), Some(stamp)) = (std::fs::read_to_string(marker()), stamp()) else {
        return false;
    };
    text.lines().next().is_some_and(|line| line.trim() == stamp)
}

/// `engine = ...` из cef\cef_host.ini (выбор по умолчанию для всех).
fn ini_engine() -> Option<String> {
    let text = std::fs::read_to_string(crate::utils::cef_dir().join("cef_host.ini")).ok()?;
    text.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with(';') && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .find(|(key, _)| key.trim().eq_ignore_ascii_case("engine"))
        .map(|(_, value)| value.trim().to_ascii_lowercase())
}

pub fn choose() -> Engine {
    let from_env = std::env::var("DELIRIUM_CEF_ENGINE")
        .ok()
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| !v.is_empty() && v != "auto");
    let source = if from_env.is_some() { "DELIRIUM_CEF_ENGINE" } else { "cef_host.ini" };
    let wanted = from_env.or_else(ini_engine).unwrap_or_default();
    let available = stamp().is_some();
    let engine = match wanted.as_str() {
        "legacy" => Engine::Legacy,
        "modern" if available => Engine::Modern,
        _ if available && !modern_disabled() => Engine::Modern,
        _ => Engine::Legacy,
    };
    log::info!(
        "интерфейс: {} (выбор: {} из {}, современный {}{})",
        engine.name(),
        if wanted.is_empty() { "auto" } else { wanted.as_str() },
        source,
        if available { "есть" } else { "не установлен" },
        if available && modern_disabled() { ", отключён после сбоя — cef-modern\\disabled.txt" } else { "" }
    );
    engine
}

/// Современный сломался на этом компьютере — запомнить до обновления хоста.
pub fn disable_modern(reason: &str) {
    log::error!("современный интерфейс отключён: {} — дальше совместимый (CEF 89)", reason);
    if let Some(stamp) = stamp() {
        let _ = std::fs::write(marker(), format!("{}\n{}\n", stamp, reason));
    }
}
