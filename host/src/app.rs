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

impl RenderMode {
    /// `render = auto | gpu | cpu`; нет файла или строки — auto.
    pub fn from_config() -> RenderMode {
        let text = std::fs::read_to_string(crate::paths::cef_dir().join("cef_host.ini")).unwrap_or_default();
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with(';') || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=')
                && key.trim().eq_ignore_ascii_case("render")
            {
                return match value.trim().to_ascii_lowercase().as_str() {
                    "gpu" => RenderMode::Gpu,
                    "cpu" => RenderMode::Cpu,
                    "shared" => RenderMode::Shared,
                    _ => RenderMode::Auto,
                };
            }
        }
        RenderMode::Auto
    }
}

#[derive(Clone)]
struct HostApp {
    writer: Arc<Writer>,
    render: RenderMode,
}

impl RenderProcessHandler for HostApp {}

impl BrowserProcessHandler for HostApp {
    fn on_context_initialized(&self) {
        log::trace!("on_context_initialized");
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
    let cache = cef::types::string::into_cef_string(&dir.join("cache").to_string_lossy());
    let locales = cef::types::string::into_cef_string(&dir.join("locales").to_string_lossy());
    let resources = cef::types::string::into_cef_string(&dir.to_string_lossy());
    let log_file = cef::types::string::into_cef_string(&dir.join("cef.log").to_string_lossy());
    let user_data = cef::types::string::into_cef_string(&dir.join("user_data").to_string_lossy());

    settings.size = std::mem::size_of::<cef_sys::cef_settings_t>();
    settings.no_sandbox = 1;
    settings.browser_subprocess_path = subprocess;
    settings.windowless_rendering_enabled = 1;
    settings.multi_threaded_message_loop = 1;
    settings.log_severity = cef_sys::cef_log_severity_t::LOGSEVERITY_ERROR;
    settings.cache_path = cache;
    settings.locales_dir_path = locales;
    settings.resources_dir_path = resources;
    settings.ignore_certificate_errors = 1;
    settings.log_file = log_file;
    settings.user_data_path = user_data;

    cef::initialize(Some(&main_args), &settings, Some(HostApp { writer, render }));
}
