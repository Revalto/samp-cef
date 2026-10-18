//! Запуск CEF в процессе хоста.

use std::sync::Arc;

use winapi::um::libloaderapi::GetModuleHandleA;

use cef::app::App;
use cef::command_line::CommandLine;
use cef::handlers::browser_process::BrowserProcessHandler;
use cef::handlers::render_process::RenderProcessHandler;
use cef::types::string::CefString;

use ipc::ToGame;
use ipc::channel::Writer;

/// Чем рисовать страницы — `render` в cef\cef_host.ini.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
    /// Видеокарта; Chromium сам откатится на процессор, если драйвер в его чёрном списке или
    /// его GPU-процесс падает
    Auto,
    /// Видеокарта, даже если драйвер в чёрном списке Chromium
    Gpu,
    /// Только процессор (как было в игре)
    Cpu,
    /// Видеокарта, кадр — общей текстурой D3D11, чтение в память без ожидания (gpu.rs)
    Shared,
}

/// Значение `key = value` из cef\cef_host.ini (cef-modern\cef_host.ini у современного хоста).
pub fn config_value(name: &str) -> Option<String> {
    let text = std::fs::read_to_string(crate::paths::cef_dir().join("cef_host.ini")).unwrap_or_default();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case(name)
        {
            return Some(value.trim().to_string());
        }
    }
    None
}

impl RenderMode {
    /// `render = auto | gpu | cpu | shared`; нет файла или строки — shared на CEF 150 (там общая
    /// текстура работает), auto на CEF 89.
    pub fn from_config() -> RenderMode {
        match config_value("render").map(|v| v.to_ascii_lowercase()).as_deref() {
            Some("gpu") => RenderMode::Gpu,
            Some("cpu") => RenderMode::Cpu,
            Some("shared") => RenderMode::Shared,
            Some("auto") => RenderMode::Auto,
            _ if cfg!(feature = "modern") => RenderMode::Shared,
            _ => RenderMode::Auto,
        }
    }
}

/// Лимит кучи JS (МБ): без него V8 растёт до сотен мегабайт и подолгу собирает мусор (рывки).
/// `js_heap_mb` в cef_host.ini.
fn js_heap_mb() -> u32 {
    config_value("js_heap_mb").and_then(|v| v.parse().ok()).unwrap_or(384)
}

/// CEF дошёл до готовности (main.rs: пока нет — раз в 2 с лог с дочерними процессами)
pub static READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[derive(Clone)]
struct HostApp {
    writer: Arc<Writer>,
    render: RenderMode,
}

impl RenderProcessHandler for HostApp {}

impl BrowserProcessHandler for HostApp {
    #[cfg(feature = "modern")]
    fn on_before_child_process_launch(&self, process_type: String) {
        log::info!("CEF запускает процесс: {}", if process_type.is_empty() { "?" } else { &process_type });
    }

    fn on_context_initialized(&self) {
        log::info!("CEF готов (on_context_initialized) — Ready игре; дочерние процессы: {}", ipc::priority::children_text());
        READY.store(true, std::sync::atomic::Ordering::SeqCst);
        crate::assets_scheme::register_scheme_handler_factory();
        self.writer.send(&ToGame::Ready);
    }
}

impl App for HostApp {
    type RenderProcessHandler = Self;
    type BrowserProcessHandler = Self;

    fn browser_process_handler(&self) -> Option<Self::BrowserProcessHandler> {
        Some(self.clone())
    }

    fn on_before_command_line_processing(&self, _process_type: CefString, command_line: CommandLine) {
        match self.render {
            // Кадр всё равно читается с видеокарты и копируется в текстуру GTA (D3D9), но страница
            // (слои, SVG, трансформации карты) рисуется и собирается на видеокарте
            RenderMode::Auto | RenderMode::Shared => {}
            RenderMode::Gpu => command_line.append_switch("ignore-gpu-blocklist"),
            RenderMode::Cpu => {
                command_line.append_switch("disable-gpu-compositing");
                command_line.append_switch("disable-gpu");
            }
        }
        command_line.append_switch("enable-begin-frame-scheduling");
        command_line.append_switch_with_value("autoplay-policy", "no-user-gesture-required");
        command_line.append_switch("enable-media-stream");
        // Не ждать вертикальную синхронизацию в GPU-процессе: кадр уходит в игру, а не на экран
        command_line.append_switch("disable-gpu-vsync");
        // Лишнее для интерфейса игры: изоляция сайтов по процессам (каждая — свой renderer.exe и
        // память), перевод, трансляции, подсказки, расчёт перекрытия окон (без окна — бессмысленно),
        // проверка подписи модулей в рендерере (с «Целостностью памяти» Windows 11 мешает загрузке)
        command_line.append_switch_with_value(
            "disable-features",
            "CalculateNativeWinOcclusion,IsolateOrigins,site-per-process,Translate,MediaRouter,\
             OptimizationHints,RendererCodeIntegrity,BackForwardCache,HardwareMediaKeyHandling",
        );
        command_line.append_switch_with_value("js-flags", &format!("--max-old-space-size={}", js_heap_mb()));
        // GPU-процесс в самом хосте (`gpu_process = inprocess`): меньше пересылок между процессами,
        // но падение драйвера роняет хост (игра перезапустит его через 3 с)
        if config_value("gpu_process").is_some_and(|v| v.eq_ignore_ascii_case("inprocess")) {
            command_line.append_switch("in-process-gpu");
        }
    }

    fn on_register_custom_schemes(&self, registrar: *mut cef_sys::cef_scheme_registrar_t) {
        crate::assets_scheme::register_custom_scheme(registrar);
    }
}

pub fn initialize(writer: Arc<Writer>, render: RenderMode) {
    let instance = unsafe { GetModuleHandleA(std::ptr::null()) };
    let main_args = cef_sys::cef_main_args_t {
        instance: instance as *mut _,
    };

    let dir = crate::paths::cef_dir();
    let mut settings = unsafe { std::mem::zeroed::<cef_sys::cef_settings_t>() };

    let subprocess = cef::types::string::into_cef_string(&dir.join("renderer.exe").to_string_lossy());
    let locales = cef::types::string::into_cef_string(&dir.join("locales").to_string_lossy());
    let resources = cef::types::string::into_cef_string(&dir.to_string_lossy());
    let log_file = cef::types::string::into_cef_string(&dir.join("cef.log").to_string_lossy());

    settings.size = std::mem::size_of::<cef_sys::cef_settings_t>();
    settings.no_sandbox = 1;
    settings.browser_subprocess_path = subprocess;
    settings.windowless_rendering_enabled = 1;
    settings.multi_threaded_message_loop = 1;
    // cef.log: `cef_log = info | warning | error` в cef_host.ini (на CEF 150 пока info — ищем,
    // почему из игры он не доходит до готовности)
    settings.log_severity = match config_value("cef_log").map(|v| v.to_ascii_lowercase()).as_deref() {
        Some("info") => cef_sys::cef_log_severity_t::LOGSEVERITY_INFO,
        Some("warning") => cef_sys::cef_log_severity_t::LOGSEVERITY_WARNING,
        Some("error") => cef_sys::cef_log_severity_t::LOGSEVERITY_ERROR,
        _ if cfg!(feature = "modern") => cef_sys::cef_log_severity_t::LOGSEVERITY_INFO,
        _ => cef_sys::cef_log_severity_t::LOGSEVERITY_ERROR,
    };
    settings.locales_dir_path = locales;
    settings.resources_dir_path = resources;
    settings.log_file = log_file;

    // CEF 150: кэш обязан лежать внутри root_cache_path; user_data_path и
    // ignore_certificate_errors убраны (сертификаты — OnCertificateError, у нас только свои адреса)
    #[cfg(feature = "modern")]
    {
        let root = dir.join("user_data");
        settings.root_cache_path = cef::types::string::into_cef_string(&root.to_string_lossy());
        settings.cache_path = cef::types::string::into_cef_string(&root.join("cache").to_string_lossy());
    }
    #[cfg(not(feature = "modern"))]
    {
        settings.cache_path = cef::types::string::into_cef_string(&dir.join("cache").to_string_lossy());
        settings.ignore_certificate_errors = 1;
        settings.user_data_path = cef::types::string::into_cef_string(&dir.join("user_data").to_string_lossy());
    }

    log::info!("cef::initialize: папка {}, процессы страниц {}", dir.display(), dir.join("renderer.exe").display());
    let result = cef::initialize(Some(&main_args), &settings, Some(HostApp { writer, render }));
    #[cfg(feature = "modern")]
    if result == 0 {
        // Причина отказа (ResultCode Chromium): 21 — кэш занят другим процессом CEF, 1001+ — CEF
        let code = unsafe { cef_sys::cef_get_exit_code() };
        log::error!("cef::initialize отказал, код {}; окружение: {}", code, env_names());
    }
    log::info!("cef::initialize вернул {}", result);
}

/// Имена переменных окружения хоста (без значений) — для лога.
#[cfg_attr(not(feature = "modern"), allow(dead_code))]
fn env_names() -> String {
    let mut names: Vec<String> = std::env::vars_os().map(|(n, _)| n.to_string_lossy().into_owned()).collect();
    names.sort();
    names.join(" ")
}
