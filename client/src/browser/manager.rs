//! Браузеры со стороны игры. Сами браузеры живут в cef_host.exe (browser/host.rs), здесь —
//! их состояние (скрыт, фокус, загружен), текстуры в игре и пересылка ввода.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;

use ipc::surface::{self, Surface};
use ipc::{KeyEvent, ToHost, Value};

pub use ipc::MouseButton as MouseKey;

use crate::app::{Event, ExternalBrowser};
use crate::browser::engine::{self, Engine};
use crate::browser::host::{self, HostProcess};
use crate::browser::view::View;
use crate::external::BrowserReadyCallback;

/// Хост упал — через сколько запускать заново.
const RESPAWN_DELAY: Duration = Duration::from_secs(3);

struct Remote {
    url: String,
    hidden: bool,
    prev_hidden: bool,
    listen_keys: bool,
    loaded: bool,
    view: View,
    surface: Option<Surface>,
    surface_name: String,
}

pub struct Manager {
    host: Option<HostProcess>,
    /// Какой хост (engine.rs); при сбое современного — до конца игры совместимый
    engine: Engine,
    /// Номер запуска хоста: события от прежнего (убитого) отбрасываются
    host_generation: u32,
    /// Когда запущен хост, который ещё не прислал Ready
    host_started_at: Option<Instant>,
    /// Падения современного хоста (для «дважды за 2 минуты»)
    modern_crashes: Vec<Instant>,
    host_ready: bool,
    cef_running: bool,
    respawn_at: Option<Instant>,
    browsers: HashMap<u32, Remote>,
    ready_callbacks: HashMap<u32, Vec<BrowserReadyCallback>>,
    focused: Option<u32>,
    focused_queue: VecDeque<u32>,
    input_corrupted: bool,
    do_not_draw: bool,
    event_tx: Sender<Event>,
    mouse: (i32, i32),
    prev_fps: u64,
    surface_serial: u32,
    /// Забор кадров из общей памяти в текстуру: сколько раз и сколько стоит (lock/копия/unlock)
    uploads: ipc::stats::Stats,
    frames: ipc::stats::Stats,
    /// Вывод спрайтов: здесь D3D9 загружает изменённую текстуру на видеокарту
    draws: ipc::stats::Stats,
}

impl Manager {
    pub fn new(event_tx: Sender<Event>) -> Manager {
        Manager {
            host: None,
            engine: Engine::Legacy,
            host_generation: 0,
            host_started_at: None,
            modern_crashes: Vec::new(),
            host_ready: false,
            cef_running: false,
            respawn_at: None,
            browsers: HashMap::new(),
            ready_callbacks: HashMap::new(),
            focused: None,
            focused_queue: VecDeque::new(),
            input_corrupted: false,
            do_not_draw: false,
            event_tx,
            mouse: (0, 0),
            prev_fps: 0,
            surface_serial: 0,
            uploads: ipc::stats::Stats::default(),
            frames: ipc::stats::Stats::default(),
            draws: ipc::stats::Stats::default(),
        }
    }

    fn send(&self, message: ToHost) {
        if !self.host_ready {
            return; // всё нужное отправится заново, когда хост будет готов (on_host_ready)
        }
        if let Some(host) = self.host.as_ref() {
            host.send(&message);
        }
    }

    // ---- Хост ----------------------------------------------------------------------------

    pub fn initialize_cef(&mut self) {
        log::trace!("manager::initialize_cef() cef_running: {}", self.cef_running);
        if self.cef_running {
            return;
        }
        self.cef_running = true;
        self.engine = engine::choose();
        self.spawn_host();
    }

    fn spawn_host(&mut self) {
        self.host_generation = self.host_generation.wrapping_add(1);
        self.host = host::spawn(self.event_tx.clone(), &self.engine.dir(), self.host_generation);
        self.host_ready = false;
        self.host_started_at = Some(Instant::now());
        if self.host.is_none() {
            if self.engine == Engine::Modern {
                self.fall_back("cef-modern\\cef_host.exe не запустился");
                return;
            }
            self.respawn_at = Some(Instant::now() + RESPAWN_DELAY);
        }
    }

    /// Событие от текущего хоста, а не от прежнего.
    pub fn is_current_host(&self, generation: u32) -> bool {
        generation == self.host_generation
    }

    /// Современный хост не работает: запомнить (engine.rs) и сразу запустить совместимый.
    fn fall_back(&mut self, reason: &str) {
        if let Some(host) = self.host.as_ref() {
            log::error!("откат: хост ещё {} (код 0x{:X})", if host.exit_code() == 259 { "работает" } else { "завершён" }, host.exit_code());
        }
        engine::disable_modern(reason);
        if let Some(host) = self.host.take() {
            host.kill();
        }
        self.engine = Engine::Legacy;
        self.modern_crashes.clear();
        for remote in self.browsers.values_mut() {
            remote.loaded = false;
        }
        self.respawn_at = None;
        self.spawn_host();
    }

    pub fn shutdown_cef(&mut self) {
        // Канал закрывается — хост выходит сам
        self.host = None;
        self.host_ready = false;
        self.cef_running = false;
    }

    /// Хост запустил CEF. После перезапуска хоста — заново создаём браузеры, что были.
    pub fn on_host_ready(&mut self) {
        self.host_ready = true;
        if let Some(started) = self.host_started_at.take() {
            log::info!("хост интерфейса готов за {:?}: {}", started.elapsed(), self.engine.name());
        }
        self.prev_fps = 0;

        let creates: Vec<ToHost> = self
            .browsers
            .iter()
            .map(|(&id, remote)| ToHost::CreateBrowser {
                id,
                url: remote.url.clone(),
                surface: remote.surface_name.clone(),
                width: remote.view.size().0 as u32,
                height: remote.view.size().1 as u32,
                hidden: remote.hidden,
            })
            .collect();
        for message in creates {
            self.send(message);
        }
    }

    pub fn on_host_lost(&mut self, generation: u32) {
        if generation != self.host_generation {
            return; // прежний хост, его уже заменили
        }
        if let Some(host) = self.host.as_ref() {
            log::error!("хост завершился, код 0x{:X} ({})", host.exit_code(), self.engine.name());
        }
        if self.engine == Engine::Modern && self.cef_running {
            let now = Instant::now();
            self.modern_crashes.retain(|at| now.duration_since(*at) < engine::MODERN_CRASH_WINDOW);
            self.modern_crashes.push(now);
            if self.modern_crashes.len() >= 2 || !self.host_ready {
                let reason = if self.host_ready { "упал дважды за 2 минуты" } else { "упал при запуске" };
                self.fall_back(reason);
                return;
            }
        }
        log::error!("cef_host.exe завершился — перезапуск через {:?}", RESPAWN_DELAY);
        self.host = None;
        self.host_ready = false;
        for remote in self.browsers.values_mut() {
            remote.loaded = false;
        }
        if self.cef_running {
            self.respawn_at = Some(Instant::now() + RESPAWN_DELAY);
        }
    }

    /// Каждый кадр: перезапуск упавшего хоста.
    pub fn tick(&mut self) {
        if self.engine == Engine::Modern
            && !self.host_ready
            && self.host.is_some()
            && self.host_started_at.is_some_and(|at| at.elapsed() >= engine::MODERN_START_TIMEOUT)
        {
            self.fall_back("не ответил за 20 с");
        }
        if let Some(at) = self.respawn_at
            && Instant::now() >= at
            && self.cef_running
        {
            self.respawn_at = None;
            self.spawn_host();
        }
    }

    // ---- Браузеры ------------------------------------------------------------------------

    fn new_surface(&mut self, id: u32, width: usize, height: usize) -> (Option<Surface>, String) {
        self.surface_serial += 1;
        let name = surface::name(std::process::id(), id, self.surface_serial);
        let surface = Surface::create(&name, width as u32, height as u32);
        if surface.is_none() {
            log::error!("не создалась общая память {} ({}x{})", name, width, height);
        }
        (surface, name)
    }

    pub fn create_browser(&mut self, id: u32, url: &str) {
        let [width, height] = crate::utils::client_rect();
        let (width, height) = (width.max(1), height.max(1));
        log::trace!("manager::create_browser({}, {:?}) {}x{}", id, url, width, height);

        if self.browsers.contains_key(&id) {
            self.close_browser(id, true);
        }

        let (surface, surface_name) = self.new_surface(id, width, height);
        let mut view = View::new();
        view.make_display(width, height);

        self.browsers.insert(
            id,
            Remote {
                url: url.to_string(),
                hidden: false,
                prev_hidden: false,
                listen_keys: false,
                loaded: false,
                view,
                surface,
                surface_name: surface_name.clone(),
            },
        );

        self.send(ToHost::CreateBrowser {
            id,
            url: url.to_string(),
            surface: surface_name,
            width: width as u32,
            height: height as u32,
            hidden: false,
        });
    }

    pub fn create_browser_on_texture(&mut self, ext: &ExternalBrowser) {
        log::error!(
            "браузеры на текстурах объектов не поддерживаются (id {}, {})",
            ext.id,
            ext.url
        );
    }

    /// Окно игры поменяло размер (другое разрешение) — новая общая память и текстура.
    fn check_resize(&mut self) {
        let [width, height] = crate::utils::client_rect();
        if width == 0 || height == 0 {
            return; // свёрнуто — оставляем как было
        }

        let ids: Vec<u32> = self
            .browsers
            .iter()
            .filter(|(_, remote)| remote.view.is_active() && remote.view.size() != (width, height))
            .map(|(&id, _)| id)
            .collect();

        for id in ids {
            let (surface, surface_name) = self.new_surface(id, width, height);
            if let Some(remote) = self.browsers.get_mut(&id) {
                remote.view.make_display(width, height);
                remote.surface = surface;
                remote.surface_name = surface_name.clone();
            }
            self.send(ToHost::Resize {
                id,
                surface: surface_name,
                width: width as u32,
                height: height as u32,
            });
        }
    }

    pub fn draw(&mut self) {
        self.check_resize();

        for remote in self.browsers.values_mut() {
            if remote.hidden || !remote.view.is_active() {
                continue;
            }
            let Some(surface) = remote.surface.as_ref() else {
                continue;
            };
            // Хост как раз пишет кадр — заберём в следующем
            if let Some(guard) = surface.try_lock()
                && let Some(rect) = guard.take_dirty()
            {
                let started = Instant::now();
                remote.view.update_from(guard.pixels(), rect);
                self.uploads.add(started.elapsed(), (rect.width.max(0) * rect.height.max(0)) as u64);
            }
        }

        self.frames.add(std::time::Duration::ZERO, 0);
        if let Some(frames) = self.frames.report() {
            log::info!(
                "кадры игры {}; в текстуру: {}; вывод: {}; частота браузеров {}",
                frames,
                self.uploads.report().unwrap_or_else(|| "нет".into()),
                self.draws.report().unwrap_or_else(|| "нет".into()),
                self.prev_fps
            );
        }

        if self.do_not_draw {
            return;
        }
        let started = Instant::now();

        let focused = self.focused;
        for (id, remote) in self.browsers.iter_mut() {
            if !remote.hidden && Some(*id) != focused {
                remote.view.draw();
            }
        }
        if let Some(remote) = focused.and_then(|id| self.browsers.get_mut(&id))
            && !remote.hidden
        {
            remote.view.draw();
        }
        self.draws.add(started.elapsed(), 0);
    }

    pub fn close_browser(&mut self, id: u32, _force_close: bool) {
        if self.browsers.remove(&id).is_some() {
            self.send(ToHost::DestroyBrowser { id });
        }
        self.ready_callbacks.remove(&id);
        self.cleanup_focus_after_close(id);
    }

    pub fn close_all_browsers(&mut self) {
        let ids: Vec<u32> = self.browsers.keys().copied().collect();
        for id in ids {
            self.send(ToHost::DestroyBrowser { id });
        }
        self.browsers.clear();
        self.ready_callbacks.clear();
        self.focused = None;
        self.focused_queue.clear();
    }

    fn set_hidden(&mut self, id: u32, hide: bool) {
        let Some(remote) = self.browsers.get_mut(&id) else {
            return;
        };
        if remote.hidden == hide {
            return;
        }
        remote.hidden = hide;
        if hide {
            remote.view.clear();
        }
        self.send(ToHost::Hide { id, hide });
    }

    pub fn hide_browser(&mut self, id: u32, hide: bool) {
        if self.do_not_draw {
            // Окно неактивно: запомнить, что показать потом
            if let Some(remote) = self.browsers.get_mut(&id) {
                remote.prev_hidden = hide;
            }
            return;
        }
        self.set_hidden(id, hide);
    }

    pub fn browser_focus(&mut self, id: u32, focus: bool) {
        if !self.browsers.contains_key(&id) {
            return;
        }
        if focus {
            if let Some(&cur_id) = self.focused.as_ref() {
                if cur_id != id {
                    self.focused_queue.push_back(id);
                }
            } else {
                self.focused = Some(id);
            }
        } else if self.focused == Some(id) {
            self.focused = self.focused_queue.pop_front();
        } else if let Some(idx) = self.focused_queue.iter().position(|&queued| queued == id) {
            self.focused_queue.remove(idx);
        }
    }

    pub fn trigger_event(&self, name: &str, args: Vec<Value>) {
        self.send(ToHost::TriggerEvent {
            name: name.to_string(),
            args,
        });
    }

    pub fn on_browser_created(&mut self, id: u32) {
        if let Some(remote) = self.browsers.get_mut(&id) {
            remote.loaded = true;
        }
        if let Some(callbacks) = self.ready_callbacks.remove(&id) {
            callbacks.iter().for_each(|cb| cb(id));
        }
    }

    pub fn add_browser_ready(&mut self, browser_id: u32, callback: BrowserReadyCallback) {
        if self.browser_ready(browser_id) {
            callback(browser_id);
            return;
        }
        self.ready_callbacks.entry(browser_id).or_default().push(callback);
    }

    pub fn toggle_dev_tools(&self, browser_id: u32, enabled: bool) {
        self.send(ToHost::DevTools {
            id: browser_id,
            enabled,
        });
    }

    pub fn always_listen_keys(&mut self, browser_id: u32, listen: bool) {
        if let Some(remote) = self.browsers.get_mut(&browser_id) {
            remote.listen_keys = listen;
        }
    }

    pub fn load_url(&mut self, browser_id: u32, url: &str) {
        if let Some(remote) = self.browsers.get_mut(&browser_id) {
            remote.url = url.to_string();
            remote.loaded = false;
        }
        self.send(ToHost::LoadUrl {
            id: browser_id,
            url: url.to_string(),
        });
    }

    pub fn update_fps(&mut self, fps: u64) {
        if self.prev_fps != fps {
            self.prev_fps = fps;
            self.send(ToHost::FrameRate(std::cmp::max(15, fps as i32)));
        }
    }

    /// RenderWare выключается — текстур больше нет.
    pub fn remove_views(&mut self) {
        for remote in self.browsers.values_mut() {
            remote.view.make_inactive();
        }
    }

    // ---- Ввод ----------------------------------------------------------------------------

    pub fn send_mouse_move_event(&mut self, x: i32, y: i32) {
        self.mouse = (x, y);
        if self.input_corrupted {
            return;
        }
        if let Some(id) = self.focused {
            self.send(ToHost::MouseMove { id, x, y });
        }
    }

    pub fn send_mouse_click_event(&mut self, button: MouseKey, is_down: bool) {
        if self.input_corrupted {
            return;
        }
        if let Some(id) = self.focused {
            let (x, y) = self.mouse;
            self.send(ToHost::MouseClick {
                id,
                x,
                y,
                button,
                down: is_down,
            });
        }
    }

    pub fn send_mouse_wheel(&self, delta: i32) {
        if self.input_corrupted {
            return;
        }
        if let Some(id) = self.focused {
            let (x, y) = self.mouse;
            self.send(ToHost::MouseWheel { id, x, y, delta });
        }
    }

    pub fn send_keyboard_event(&self, event: KeyEvent) {
        if self.input_corrupted {
            return;
        }
        // Клавиши — только браузеру в фокусе; без фокуса — тем, кто попросил слушать всегда
        if let Some(id) = self.focused {
            self.send(ToHost::Key { id, event });
        } else {
            for (&id, _) in self.browsers.iter().filter(|(_, remote)| remote.listen_keys) {
                self.send(ToHost::Key { id, event });
            }
        }
    }

    #[inline(always)]
    pub fn is_input_blocked(&self) -> bool {
        self.focused.is_some()
    }

    #[inline(always)]
    pub fn is_input_available(&self, browser: u32) -> bool {
        if self.input_corrupted {
            return false;
        }
        if self.is_input_blocked() {
            self.focused == Some(browser)
        } else {
            true
        }
    }

    #[inline(always)]
    pub fn set_corrupted(&mut self, corrupted: bool) {
        self.input_corrupted = corrupted;
    }

    #[inline(always)]
    pub fn is_input_corrupted(&self) -> bool {
        self.input_corrupted
    }

    /// Окно игры неактивно / открыто меню GTA — браузеры прячем, потом возвращаем как было.
    pub fn do_not_draw(&mut self, donot: bool) {
        if self.do_not_draw == donot {
            return;
        }

        let ids: Vec<u32> = self.browsers.keys().copied().collect();
        for id in ids {
            if donot {
                if let Some(remote) = self.browsers.get_mut(&id) {
                    remote.prev_hidden = remote.hidden;
                }
                self.set_hidden(id, true);
            } else {
                let prev = self.browsers.get(&id).map(|r| r.prev_hidden).unwrap_or(true);
                self.set_hidden(id, prev);
            }
        }

        self.do_not_draw = donot;
    }

    #[inline(always)]
    pub fn browser_exists(&self, browser_id: u32) -> bool {
        self.browsers.contains_key(&browser_id)
    }

    #[inline(always)]
    pub fn browser_ready(&self, browser_id: u32) -> bool {
        self.browsers.get(&browser_id).map(|r| r.loaded).unwrap_or(false)
    }

    fn cleanup_focus_after_close(&mut self, closed_id: u32) {
        let browsers = &self.browsers;
        self.focused_queue
            .retain(|&id| id != closed_id && browsers.contains_key(&id));

        let focused_stale = self.focused.is_some_and(|id| !self.browsers.contains_key(&id));
        if self.focused == Some(closed_id) || focused_stale {
            self.focused = None;
            while let Some(next_id) = self.focused_queue.pop_front() {
                if self.browsers.contains_key(&next_id) {
                    self.focused = Some(next_id);
                    break;
                }
            }
        }

        if self.browsers.is_empty() {
            self.focused = None;
            self.focused_queue.clear();
        }
    }
}
