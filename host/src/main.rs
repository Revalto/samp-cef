//! cef_host.exe — процесс браузера samp-cef.
//!
//! Запускает его игра (client.dll) и передаёт концы двух анонимных каналов:
//!   cef_host.exe --in <дескриптор> --out <дескриптор>
//! Канал закрылся — игра завершилась или выгрузила клиент: закрываемся и мы.

#![windows_subsystem = "windows"]

// Одна из двух версий CEF под общими именами `cef` / `cef_sys` (Cargo.toml: legacy / modern)
#[cfg(feature = "modern")]
extern crate cef150 as cef;
#[cfg(feature = "modern")]
extern crate cef_sys150 as cef_sys;
#[cfg(not(feature = "modern"))]
extern crate cef89 as cef;
#[cfg(not(feature = "modern"))]
extern crate cef_sys89 as cef_sys;

mod app;
mod assets_scheme;
mod gpu;
#[cfg(feature = "modern")]
mod pacing;
mod paths;
mod view;

/// Версия CEF этой сборки — для логов (и cef_client.log игры)
#[cfg(feature = "modern")]
pub const ENGINE: &str = "CEF 150 (современный режим, cef-modern)";
#[cfg(not(feature = "modern"))]
pub const ENGINE: &str = "CEF 89 (совместимый режим)";

use std::collections::HashMap;
use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use parking_lot::Mutex;

use cef::types::string::CefString;

use ipc::channel::{Handle, Writer};
use ipc::surface::Surface;
use ipc::ToHost;

use crate::view::{View, ViewRef};

const DEFAULT_FRAME_RATE: i32 = 60;

struct Host {
    writer: Arc<Writer>,
    views: Arc<Mutex<HashMap<u32, Arc<View>>>>,
    /// Частота кадров игры (ToHost::FrameRate) — по ней темп кадров интерфейса (pacing.rs)
    frame_rate: Arc<AtomicI32>,
    /// render = shared и устройство D3D11 создалось
    gpu: Option<Arc<gpu::Device>>,
}

fn open_surface(name: &str) -> Option<Surface> {
    let surface = Surface::open(name);
    if surface.is_none() {
        log::error!("не открылась общая память {}", name);
    }
    surface
}

impl Host {
    fn view(&self, id: u32) -> Option<Arc<View>> {
        self.views.lock().get(&id).cloned()
    }

    fn handle(&self, message: ToHost) {
        match message {
            ToHost::CreateBrowser {
                id,
                url,
                surface,
                width,
                height,
                hidden,
            } => {
                log::info!("create browser {} {}x{} {}", id, width, height, url);
                let view = View::new(
                    id,
                    self.writer.clone(),
                    open_surface(&surface),
                    hidden,
                    self.frame_rate.load(Ordering::SeqCst),
                );
                if let Some(device) = self.gpu.as_ref() {
                    view.enable_shared(device.clone());
                }
                if let Some(old) = self.views.lock().insert(id, view.clone()) {
                    old.close();
                }
                create_browser(ViewRef(view), &url, self.gpu.is_some(), external_begin_frame(self.gpu.is_some()));
            }

            ToHost::Resize { id, surface, width, height } => {
                log::info!("resize browser {} {}x{}", id, width, height);
                if let Some(view) = self.view(id) {
                    view.set_surface(open_surface(&surface));
                }
            }

            ToHost::DestroyBrowser { id } => {
                if let Some(view) = self.views.lock().remove(&id) {
                    view.close();
                }
            }

            ToHost::Hide { id, hide } => {
                if let Some(view) = self.view(id) {
                    view.hide(hide);
                }
            }

            ToHost::MouseMove { id, x, y } => {
                if let Some(view) = self.view(id) {
                    view.mouse_move(x, y);
                }
            }

            ToHost::MouseClick { id, x, y, button, down } => {
                if let Some(view) = self.view(id) {
                    view.mouse_click(x, y, button, down);
                }
            }

            ToHost::MouseWheel { id, x, y, delta } => {
                if let Some(view) = self.view(id) {
                    view.mouse_wheel(x, y, delta);
                }
            }

            ToHost::Key { id, event } => {
                if let Some(view) = self.view(id) {
                    view.key(event);
                }
            }

            ToHost::TriggerEvent { name, args } => {
                let views: Vec<_> = self.views.lock().values().cloned().collect();
                for view in views {
                    view.trigger(&name, &args);
                }
            }

            ToHost::DevTools { id, enabled } => {
                if let Some(view) = self.view(id) {
                    view.dev_tools(enabled);
                }
            }

            ToHost::LoadUrl { id, url } => {
                if let Some(view) = self.view(id) {
                    view.load_url(&url);
                }
            }

            ToHost::FrameRate(fps) => {
                self.frame_rate.store(fps, Ordering::SeqCst);
                let views: Vec<_> = self.views.lock().values().cloned().collect();
                for view in views {
                    view.set_frame_rate(fps);
                }
            }
        }
    }

    fn close_all(&self) {
        let views: Vec<_> = self.views.lock().drain().map(|(_, view)| view).collect();
        for view in views {
            view.close();
        }
    }
}

/// Кадры по запросу хоста (pacing.rs), а не по таймеру Chromium: только CEF 150 с общей
/// текстурой (таймер Chromium отдаёт не больше 60 к/с; на CEF 89 кадр всё равно упирается в чтение).
fn external_begin_frame(shared_texture: bool) -> bool {
    cfg!(feature = "modern") && shared_texture && pacing_enabled()
}

/// `pacing = external | timer` в cef_host.ini (по умолчанию external)
fn pacing_enabled() -> bool {
    !app::config_value("pacing").is_some_and(|v| v.eq_ignore_ascii_case("timer"))
}

fn create_browser(view: ViewRef, url: &str, shared_texture: bool, external_begin_frame: bool) {
    let mut window_info = unsafe { std::mem::zeroed::<cef_sys::cef_window_info_t>() };
    #[cfg(feature = "modern")]
    {
        window_info.size = std::mem::size_of::<cef_sys::cef_window_info_t>();
    }
    window_info.windowless_rendering_enabled = 1;
    window_info.shared_texture_enabled = shared_texture as i32;
    window_info.external_begin_frame_enabled = external_begin_frame as i32;
    view.0.set_external_begin_frame(external_begin_frame);

    let url = crate::assets_scheme::resolve_browser_url(url);
    let url = CefString::new(&url);

    let mut settings = unsafe { std::mem::zeroed::<cef_sys::cef_browser_settings_t>() };
    settings.size = std::mem::size_of::<cef_sys::cef_browser_settings_t>();
    settings.windowless_frame_rate = DEFAULT_FRAME_RATE;
    settings.javascript_access_clipboard = cef_sys::cef_state_t::STATE_ENABLED;
    settings.javascript_dom_paste = cef_sys::cef_state_t::STATE_ENABLED;
    settings.remote_fonts = cef_sys::cef_state_t::STATE_ENABLED;
    settings.webgl = cef_sys::cef_state_t::STATE_ENABLED;
    settings.javascript = cef_sys::cef_state_t::STATE_ENABLED;

    let result = cef::browser::BrowserHost::create_browser(&window_info, Some(view), &url, &settings);
    log::info!("create_browser: {}", result);
}

fn initialize_logging() {
    let Ok(file) = File::create(paths::cef_dir().join("cef_host.log")) else {
        return;
    };
    let config = simplelog::ConfigBuilder::new()
        .add_filter_allow_str("cef_host")
        .build();
    let _ = simplelog::WriteLogger::init(simplelog::LevelFilter::Info, config, file);
}

fn arg(name: &str) -> Option<String> {
    let mut args = std::env::args();
    while let Some(arg) = args.next() {
        if arg == name {
            return args.next();
        }
    }
    None
}

fn main() {
    initialize_logging();
    log::info!("{}", ENGINE);
    // Интерфейс получает видеокарту раньше игры без ограничения FPS (ipc::priority)
    log::info!("приоритет хоста: {}", ipc::priority::raise_current(true));

    let (Some(input), Some(output)) = (
        arg("--in").and_then(|a| Handle::from_arg(&a)),
        arg("--out").and_then(|a| Handle::from_arg(&a)),
    ) else {
        log::error!("нет каналов --in/--out: cef_host.exe запускает игра");
        return;
    };

    let writer = Arc::new(Writer::new(output));

    let render = app::RenderMode::from_config();
    log::info!("отрисовка: {:?} (cef_host.ini, render = auto | gpu | cpu | shared)", render);
    let gpu = if render == app::RenderMode::Shared {
        let device = gpu::Device::create();
        if device.is_none() {
            log::error!("D3D11 не создался — кадры обычным путём (как render = auto)");
        }
        device
    } else {
        None
    };

    let host = Host {
        writer: writer.clone(),
        views: Arc::new(Mutex::new(HashMap::new())),
        frame_rate: Arc::new(AtomicI32::new(DEFAULT_FRAME_RATE)),
        gpu,
    };
    #[cfg(feature = "modern")]
    if external_begin_frame(host.gpu.is_some()) {
        pacing::start(host.views.clone(), host.frame_rate.clone());
    }
    app::initialize(writer, render);
    log::info!("CEF запущен, ждём команды игры");

    ipc::channel::read_loop::<ToHost>(&input, |message| host.handle(message));

    log::info!("игра закрыла канал — выходим");
    host.close_all();
    std::process::exit(0);
}
