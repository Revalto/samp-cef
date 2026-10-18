use std::net::SocketAddr;
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use ipc::{KeyEvent, KeyEventKind, ToGame, Value};

use winapi::shared::minwindef::{LPARAM, UINT, WPARAM};
use winapi::um::winuser::*;

use crate::browser::manager::{Manager, MouseKey};
use crate::external::CallbackList;
use crate::network::NetworkClient;
use crate::static_cell::StaticCell;

use client_api::gta::menu_manager::CMenuManager;
use client_api::samp::inputs;
use client_api::samp::netgame::NetGame;

use client_api::wndproc;

use crossbeam_channel::{Receiver, Sender};

use retour::GenericDetour;

const CEF_SERVER_PORT_OFFSET: u16 = 2;
pub const CEF_PLUGIN_VERSION: i32 = 0x00_01_00;
const CONNECT_BACKOFF_BASE: Duration = Duration::from_secs(1);
const CONNECT_BACKOFF_MAX: Duration = Duration::from_secs(10);

static APP: StaticCell<App> = StaticCell::new();

pub enum Event {
    Connect(SocketAddr),
    Timeout,
    NetworkError,
    NetworkJoined,
    BadVersion,

    CreateBrowser {
        id: u32,
        url: String,
        hidden: bool,
        focused: bool,
    },

    CreateExternBrowser(ExternalBrowser),

    DestroyBrowser(u32),
    HideBrowser(u32, bool),
    FocusBrowser(u32, bool),
    /// В JS всех браузеров: от сервера или плагина
    EmitEvent(String, Vec<Value>),
    EmitEventOnServer(String, String),
    BrowserCreated(u32, i32),
    AppendToObject(u32, i32),
    RemoveFromObject(u32, i32),
    ToggleDevTools(u32, bool),
    /// Звук браузеров на объектах — не поддерживается (браузеры на объектах тоже)
    SetAudioSettings,
    LoadUrl(u32, String),

    CefInitialize,

    AlwaysListenKeys(u32, bool),
    Terminate,

    /// Сообщение от cef_host.exe (номер запуска хоста, см. browser/host.rs)
    Host(u32, ToGame),
    /// cef_host.exe завершился
    HostLost(u32),
}

#[derive(Debug)]
pub struct ExternalBrowser {
    pub id: u32,
    pub texture: String,
    pub url: String,
    pub scale: i32,
}

pub struct App {
    connected: bool,
    window_focused: bool,
    cef_ready: bool,
    samp_ready: bool,
    bad_version_notified: bool,
    connect_backoff: Duration,
    next_connect_attempt: Instant,

    manager: Arc<Mutex<Manager>>,
    plugins_initialized: bool,
    network: Option<NetworkClient>,
    callbacks: CallbackList,
    keystate_hook: GenericDetour<extern "stdcall" fn(i32) -> u16>,
    event_tx: Sender<Event>,
    event_rx: Receiver<Event>,

    //
    key_state: [bool; 512],

    // debug
    initialization: Instant,
}

impl Drop for App {
    fn drop(&mut self) {
        log::trace!("App::drop");

        {
            let mut manager = self.manager.lock();
            manager.close_all_browsers();
            manager.shutdown_cef();
        }

        self.network.take();

        quit();
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    #[allow(clippy::arc_with_non_send_sync)]
    pub fn new() -> App {
        log::trace!("App::new()");

        let (event_tx, event_rx) = crossbeam_channel::unbounded();

        log::trace!("Manager::new()");

        let manager = Arc::new(Mutex::new(Manager::new(event_tx.clone())));

        log::trace!("crate::external::initialize");

        let callbacks = crate::external::initialize(event_tx.clone(), manager.clone());

        let keystate_hook = client_api::utils::find_function::<extern "stdcall" fn(i32) -> u16>(
            "user32.dll",
            "GetAsyncKeyState",
        )
        .map(|func| unsafe {
            let hook = GenericDetour::new(func, async_key_state).unwrap();
            hook.enable().unwrap();
            hook
        })
        .unwrap();

        App {
            connected: false,
            cef_ready: false,
            samp_ready: false,
            window_focused: true,
            bad_version_notified: false,
            connect_backoff: CONNECT_BACKOFF_BASE,
            next_connect_attempt: Instant::now(),
            network: None,
            initialization: Instant::now(),
            manager,
            keystate_hook,
            event_tx,
            event_rx,
            callbacks,
            plugins_initialized: false,

            //
            key_state: [false; 512],
        }
    }

    pub fn initialize_hooks() {
        log::trace!("App::initialize_hooks()");
        log::trace!("Trying to hook WndProc.");

        // apply hook to WndProc
        while !wndproc::initialize(&wndproc::WndProcSettings {
            callback: shitty,
            hwnd: client_api::gta::hwnd(),
        }) {
            std::thread::sleep(Duration::from_millis(10));
        }

        log::trace!("Append WndProc callback.");

        client_api::wndproc::append_callback(win_event);

        log::trace!("Hooking destroy functions.");

        NetGame::on_destroy(|| {
            log::trace!("NetGame::on_destroy calling unitialize");
            uninitialize();
        });

        NetGame::on_reconnect(|| {
            if let Some(app) = App::get() {
                app.disconnect();
            }
        });

        client_api::samp::deathwindow::DeathWindow::on_draw(|| {
            crate::render::render();
        });

        client_api::gta::game::on_shutdown(|| {
            log::trace!("gta::game::on_shutdown calling unitialize");
            uninitialize();
        });

        log::trace!("Initialize done.");
    }

    pub fn connect(&mut self) {
        if self.network.is_some() {
            return;
        }

        let now = Instant::now();
        if now < self.next_connect_attempt {
            return;
        }

        if let Some(mut addr) = NetGame::get().addr() {
            if !self.samp_ready {
                App::initialize_hooks();
                self.samp_ready = true;
                self.manager.lock().initialize_cef();
            }

            log::trace!("SAMP: CNetGame address: {}", addr);

            addr.set_port(addr.port() + CEF_SERVER_PORT_OFFSET);

            log::trace!(
                "Event::Connect({}). Elapsed {:?}",
                addr,
                self.initialization.elapsed()
            );

            log::trace!("NetworkClient::new");

            let network = NetworkClient::new(self.event_tx.clone());
            network.send(Event::Connect(addr));

            self.network = Some(network);
            self.next_connect_attempt = now + self.connect_backoff;
        }
    }

    pub fn disconnect(&mut self) {
        // disconnected
        log::trace!("App::disconnect");

        self.reset_connection(true);
    }

    fn reset_connection(&mut self, notify_disconnect: bool) {
        if notify_disconnect {
            crate::external::call_disconnect();
        }

        let mut manager = self.manager.lock();
        manager.close_all_browsers();
        self.network.take();
        self.connected = false;
    }

    fn reset_connect_backoff(&mut self) {
        self.connect_backoff = CONNECT_BACKOFF_BASE;
        self.next_connect_attempt = Instant::now();
    }

    fn bump_connect_backoff(&mut self) {
        self.next_connect_attempt = Instant::now() + self.connect_backoff;
        let doubled = self.connect_backoff + self.connect_backoff;
        self.connect_backoff = std::cmp::min(doubled, CONNECT_BACKOFF_MAX);
    }

    pub fn manager(&self) -> Arc<Mutex<Manager>> {
        self.manager.clone()
    }

    /// Сообщение от cef_host.exe (в потоке игры).
    fn handle_host(&mut self, message: ToGame) {
        match message {
            ToGame::Ready => {
                let _ = self.event_tx.send(Event::CefInitialize);
            }

            ToGame::BrowserCreated { id, status } => {
                let _ = self.event_tx.send(Event::BrowserCreated(id, status));
            }

            ToGame::Focus { id, focus } => {
                let _ = self.event_tx.send(Event::FocusBrowser(id, focus));
            }

            ToGame::Hide { id, hide } => {
                let _ = self.event_tx.send(Event::HideBrowser(id, hide));
            }

            ToGame::JsEvent { id: _, name, args } => {
                // Сначала плагины: событие могли перехватить и не отдавать серверу
                if crate::external::dispatch_js_event(&self.callbacks, &name, &args) {
                    return;
                }
                let _ = self
                    .event_tx
                    .send(Event::EmitEventOnServer(name, server_arguments(&args)));
            }
        }
    }

    fn get<'a>() -> Option<&'a mut App> {
        unsafe { APP.get_mut() }
    }
}

pub fn initialize() {
    log::trace!("app::initialize()");
    log::trace!("App::new() ->");

    let app = App::new();
    let manager = app.manager();

    log::trace!("CEF: crate::render::initalize()");

    crate::render::initialize(manager);

    unsafe {
        APP.set(app);
    }

    if client_api::samp::version::is_unknown_version() {
        log::error!("unknown samp version");

        client_api::utils::error_message_box(
            "Unsupported SA:MP",
            "You have installed an unsupported SA:MP version.\nCurrently supported versions are 0.3.7 R1 and R3.",
        );

        // don't waste time
    }
}

pub fn uninitialize() {
    static DESTROY: Once = Once::new();

    log::trace!("app::uninitialize()");

    DESTROY.call_once(|| {
        log::trace!("app::uninitialize call once ->");
        unsafe {
            APP.take();
        }
    });
}

fn quit() {
    log::trace!("app::quit()");

    crate::external::quit();
    crate::render::uninitialize();

    client_api::wndproc::uninitialize();
}

fn shitty() {
    if let Some(app) = App::get() {
        if !app.samp_ready {
            log::trace!("SAMP init within {:?}", app.initialization.elapsed());
            app.samp_ready = true;
            app.manager.lock().initialize_cef();
        } else if !app.window_focused {
            mainloop(); //
        }
    }
}

// inside GTA thread
pub fn mainloop() {
    if let Some(app) = App::get() {
        if !app.connected {
            app.connect();
        }

        if !app.samp_ready {
            return;
        }

        {
            let menu = CMenuManager::get();
            let paused = menu.is_active() || !app.window_focused;

            let show_cursor = {
                let mut manager = app.manager.lock();
                manager.tick();
                manager.set_corrupted(paused);
                manager.is_input_blocked() && !menu.is_active()
            };

            // do not redraw default cursor
            if show_cursor {
                client_api::samp::inputs::show_cursor(true);
            }
        }

        while let Ok(event) = app.event_rx.try_recv() {
            match event {
                Event::AlwaysListenKeys(browser_id, listen) => {
                    let mut manager = app.manager.lock();
                    manager.always_listen_keys(browser_id, listen);
                }

                Event::CreateBrowser {
                    id,
                    url,
                    hidden,
                    focused,
                } => {
                    log::trace!(
                        "Request to create browser view with id: {}. URL: {}",
                        id,
                        url
                    );

                    let show_cursor = {
                        let mut manager = app.manager.lock();
                        manager.create_browser(id, &url);
                        manager.hide_browser(id, hidden);
                        manager.browser_focus(id, focused);

                        manager.is_input_blocked() && !CMenuManager::is_menu_active()
                    };

                    client_api::samp::inputs::show_cursor(show_cursor);
                }

                Event::CreateExternBrowser(ext) => {
                    log::trace!(
                        "Request from server to create external browser with id {}. Texture name: {}",
                        ext.id,
                        ext.texture
                    );
                    let mut manager = app.manager.lock();

                    manager.create_browser_on_texture(&ext);
                }

                Event::DestroyBrowser(id) => {
                    let mut manager = app.manager.lock();
                    manager.close_browser(id, true);
                }

                Event::HideBrowser(id, hide) => {
                    let mut manager = app.manager.lock();
                    manager.hide_browser(id, hide);
                }

                Event::FocusBrowser(id, focus) => {
                    let show_cursor = {
                        let mut manager = app.manager.lock();
                        manager.browser_focus(id, focus);
                        manager.is_input_blocked() && !CMenuManager::is_menu_active()
                    };

                    client_api::samp::inputs::show_cursor(show_cursor);
                }

                Event::EmitEvent(event, list) => {
                    let manager = app.manager.lock();
                    manager.trigger_event(&event, list);
                }

                Event::EmitEventOnServer(event, arguments) => {
                    if let Some(network) = app.network.as_mut() {
                        let event = Event::EmitEventOnServer(event, arguments);
                        network.send(event);
                    }
                }

                Event::BrowserCreated(id, code) => {
                    log::trace!(
                        "Browser {} created. Status code: {}. Network available? {}",
                        id,
                        code,
                        app.network.is_some()
                    );

                    if let Some(network) = app.network.as_mut() {
                        let event = Event::BrowserCreated(id, code);
                        network.send(event);
                    }

                    app.manager.lock().on_browser_created(id);
                    crate::external::browser_created(id, code);
                }

                Event::Host(generation, message) => {
                    if app.manager.lock().is_current_host(generation) {
                        app.handle_host(message);
                    }
                }

                Event::HostLost(generation) => app.manager.lock().on_host_lost(generation),

                Event::CefInitialize => {
                    log::trace!("Initialized. Elapsed: {:?}", app.initialization.elapsed());

                    app.cef_ready = true;
                    app.manager.lock().on_host_ready();
                    // Хост мог перезапуститься — плагины инициализируем один раз
                    if !app.plugins_initialized {
                        app.plugins_initialized = true;
                        crate::external::call_initialize();
                    }
                }

                Event::AppendToObject(..) | Event::RemoveFromObject(..) | Event::SetAudioSettings => {}

                Event::ToggleDevTools(browser, enabled) => {
                    let manager = app.manager.lock();
                    manager.toggle_dev_tools(browser, enabled);
                }

                Event::NetworkJoined => {
                    app.connected = true;
                    app.bad_version_notified = false;
                    app.reset_connect_backoff();
                    crate::external::call_connect();
                }

                Event::BadVersion => {
                    log::trace!("CEF Network: BadVersion");
                    if !app.bad_version_notified {
                        app.bad_version_notified = true;
                        client_api::utils::error_message_box(
                            "CEF version mismatch",
                            "Client version is incompatible with the server.\nPlease update your client or server plugin.",
                        );
                    }

                    app.reset_connection(false);
                    app.bump_connect_backoff();
                }

                Event::NetworkError => {
                    log::trace!("CEF Network: NetworkError");
                    let notify_disconnect = app.connected;
                    app.reset_connection(notify_disconnect);
                    app.bump_connect_backoff();
                }

                Event::Timeout => {
                    log::trace!("CEF Network: Timeout");
                    let notify_disconnect = app.connected;
                    app.reset_connection(notify_disconnect);
                    app.bump_connect_backoff();
                }

                Event::LoadUrl(browser, url) => {
                    let mut manager = app.manager.lock();
                    manager.load_url(browser, &url);
                }

                _ => (),
            }
        }

        if app.cef_ready && app.connected {
            crate::external::call_mainloop();
        }
    }
}

/// Аргументы события для сервера — строка через пробел, как было в samp-cef.
fn server_arguments(args: &[Value]) -> String {
    args.iter()
        .map(|arg| match arg {
            Value::String(s) => s.clone(),
            Value::Bool(b) => (if *b { 1 } else { 0 }).to_string(),
            Value::Double(d) => d.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Null => "CEF_NULL".to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// TODO: Save key state. Mouse too?
fn win_event(msg: UINT, wparam: WPARAM, lparam: LPARAM) -> bool {
    if let Some(app) = App::get() {
        let mut manager = app.manager.lock();
        let mut notify_key_down = false;

        match msg {
            WM_MOUSEMOVE => {
                let [x, y] = [(lparam as u16) as i32, (lparam >> 16) as u16 as i32];

                manager.send_mouse_move_event(x, y);
            }

            WM_LBUTTONDOWN => manager.send_mouse_click_event(MouseKey::Left, true),
            WM_LBUTTONUP => manager.send_mouse_click_event(MouseKey::Left, false),
            WM_RBUTTONDOWN => manager.send_mouse_click_event(MouseKey::Right, true),
            WM_RBUTTONUP => manager.send_mouse_click_event(MouseKey::Right, false),
            WM_MBUTTONDOWN => manager.send_mouse_click_event(MouseKey::Middle, true),
            WM_MBUTTONUP => manager.send_mouse_click_event(MouseKey::Middle, false),

            WM_MOUSEWHEEL => {
                let delta = if (wparam >> 16) as i16 > 0 { 1 } else { -1 };
                manager.send_mouse_wheel(delta);
            }

            WM_KEYDOWN | WM_KEYUP | WM_CHAR | WM_SYSCHAR | WM_SYSKEYDOWN | WM_SYSKEYUP => {
                let is_system_key = msg == WM_SYSCHAR || msg == WM_SYSKEYDOWN || msg == WM_SYSKEYUP;

                let mut event = KeyEvent {
                    kind: KeyEventKind::Char,
                    modifiers: crate::utils::cef_keyboard_modifiers(wparam, lparam),
                    windows_key_code: wparam as i32,
                    native_key_code: lparam as i32,
                    is_system_key,
                };

                if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
                    event.kind = KeyEventKind::RawKeyDown;
                } else if msg == WM_KEYUP || msg == WM_SYSKEYUP {
                    event.kind = KeyEventKind::KeyUp;
                } else if msg == WM_CHAR || msg == WM_SYSCHAR {
                    event.kind = KeyEventKind::Char;

                    let bytes = [wparam as u8];
                    if let Some(ch) = encoding_rs::WINDOWS_1251.decode(&bytes).0.chars().next() {
                        event.windows_key_code = ch as _;
                    }
                }

                // notify GTA. should be notified only once
                let key_index = wparam;
                if key_index < 512 {
                    if manager.is_input_blocked() {
                        // allowed keys (screenshot and chat cycle)
                        let is_allowed_key = wparam == VK_F8 as usize || wparam == VK_F7 as usize;

                        if (app.key_state[key_index]
                            && event.kind == KeyEventKind::KeyUp)
                            || is_allowed_key
                        {
                            app.key_state[key_index] = false;
                            notify_key_down = true;
                        }
                    } else if event.kind != KeyEventKind::Char {
                        app.key_state[key_index] = event.kind == KeyEventKind::RawKeyDown;
                    }
                }

                let input_active = inputs::Input::is_active()
                    || inputs::Dialog::is_input_focused()
                    || CMenuManager::is_menu_active();

                if !input_active {
                    manager.send_keyboard_event(event);
                }
            }

            WM_ACTIVATE => {
                let status = (wparam & 0xFFFF) as u16;
                let active = status != WA_INACTIVE;

                crate::external::window_active(active);
                app.window_focused = active;
                manager.set_corrupted(!active);
                manager.do_not_draw(!active);

                return false;
            }

            _ => return false,
        }

        // game on pause or the window isn't active
        // allow user to use menu ...
        if manager.is_input_corrupted() {
            return false;
        }

        return manager.is_input_blocked() && !notify_key_down;
    }

    false
}

// TODO: Add ability to return the right AsyncKeyState result
extern "stdcall" fn async_key_state(key: i32) -> u16 {
    if let Some(app) = App::get() {
        let result = app.keystate_hook.call(key);

        if let Some(manager) = app.manager.try_lock() {
            if manager.is_input_blocked() {
                return 0;
            } else {
                return result;
            }
        }
    }

    0
}
