//! Браузер в хосте: рисует кадры в общую память игры, пересылает игре события страницы.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use cef::ProcessId;
use cef::browser::{Browser, ContextMenuParams, Frame, MenuModel};
use cef::client::Client;
use cef::handlers::audio::AudioHandler;
use cef::handlers::context_menu::ContextMenuHandler;
use cef::handlers::lifespan::LifespanHandler;
use cef::handlers::load::LoadHandler;
use cef::handlers::render::{DirtyRects, PaintElement, RenderHandler};
use cef::process_message::ProcessMessage;
use cef::types::list::{List, ValueType};
use cef::types::string::CefString;
use cef_sys::{
    cef_event_flags_t, cef_key_event_t, cef_key_event_type_t, cef_mouse_button_type_t,
    cef_mouse_event_t, cef_rect_t,
};

use ipc::channel::Writer;
use ipc::surface::{Rect, Surface, SurfaceGuard};
use ipc::{KeyEvent, KeyEventKind, MouseButton, ToGame, Value};

use crate::gpu::{Device, Readback};

/// Двойной клик — как в Windows по умолчанию
const DOUBLE_CLICK_TIME: Duration = Duration::from_millis(500);
const DOUBLE_CLICK_DISTANCE: i32 = 4;

struct Popup {
    show: bool,
    rect: cef_rect_t,
    buffer: Vec<u8>,
}

#[derive(Default)]
struct Clicks {
    at: Option<Instant>,
    x: i32,
    y: i32,
    button: Option<MouseButton>,
    count: i32,
}

pub struct View {
    id: u32,
    writer: Arc<Writer>,
    browser: Mutex<Option<Browser>>,
    surface: Mutex<Option<Surface>>,
    hidden: AtomicBool,
    frame_rate: AtomicI32,
    popup: Mutex<Popup>,
    /// Зажатые кнопки мыши — флаги EVENTFLAG_*_MOUSE_BUTTON для событий движения. Без них
    /// страница не видит перетаскивания (ползунки, выделение текста).
    buttons: AtomicU32,
    clicks: Mutex<Clicks>,
    /// OnPaint: сколько кадров приходит от CEF и сколько стоит копия в общую память
    paints: Mutex<ipc::stats::Stats>,
    /// render = shared: кадры текстурой на видеокарте (gpu.rs)
    readback: Mutex<Option<Readback>>,
    /// Кадры по запросу хоста (pacing.rs): включено при создании браузера
    external_begin_frame: AtomicBool,
    /// Запрос кадра стоит в очереди UI-потока CEF — новый не ставим
    #[cfg_attr(not(feature = "modern"), allow(dead_code))] // внешний BeginFrame — только CEF 150
    begin_frame_queued: AtomicBool,
    /// Когда последний раз просили кадр
    #[cfg_attr(not(feature = "modern"), allow(dead_code))] // внешний BeginFrame — только CEF 150
    last_begin_frame: Mutex<Instant>,
}

#[derive(Clone)]
pub struct ViewRef(pub Arc<View>);

fn button_flag(button: MouseButton) -> u32 {
    (match button {
        MouseButton::Left => cef_event_flags_t::EVENTFLAG_LEFT_MOUSE_BUTTON,
        MouseButton::Middle => cef_event_flags_t::EVENTFLAG_MIDDLE_MOUSE_BUTTON,
        MouseButton::Right => cef_event_flags_t::EVENTFLAG_RIGHT_MOUSE_BUTTON,
    }) as u32
}

fn button_type(button: MouseButton) -> cef_mouse_button_type_t::Type {
    match button {
        MouseButton::Left => cef_mouse_button_type_t::MBT_LEFT,
        MouseButton::Middle => cef_mouse_button_type_t::MBT_MIDDLE,
        MouseButton::Right => cef_mouse_button_type_t::MBT_RIGHT,
    }
}

/// Копия прямоугольника из буфера CEF (строка `width * 4`) в общую память того же размера.
fn copy_rect(source: &[u8], guard: &SurfaceGuard, rect: &cef_rect_t, width: i32, height: i32) {
    let left = rect.x.clamp(0, width);
    let top = rect.y.clamp(0, height);
    let right = (rect.x + rect.width).clamp(0, width);
    let bottom = (rect.y + rect.height).clamp(0, height);
    if left >= right || top >= bottom {
        return;
    }

    let stride = guard.stride();
    let pixels = guard.pixels();
    let bytes = (right - left) as usize * 4;
    for y in top..bottom {
        let at = y as usize * stride + left as usize * 4;
        pixels[at..at + bytes].copy_from_slice(&source[at..at + bytes]);
    }
    guard.mark_dirty(Rect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    });
}

/// Выпадающий список (<select>) CEF рисует отдельно — накладываем его поверх кадра.
fn draw_popup(popup: &Popup, guard: &SurfaceGuard, width: i32, height: i32) {
    let rect = popup.rect;
    if !popup.show || popup.buffer.len() < (rect.width * rect.height * 4).max(0) as usize {
        return;
    }
    let left = rect.x.clamp(0, width);
    let top = rect.y.clamp(0, height);
    let right = (rect.x + rect.width).clamp(0, width);
    let bottom = (rect.y + rect.height).clamp(0, height);
    if left >= right || top >= bottom {
        return;
    }

    let stride = guard.stride();
    let pixels = guard.pixels();
    let bytes = (right - left) as usize * 4;
    let popup_stride = rect.width as usize * 4;
    for y in top..bottom {
        let from = (y - rect.y) as usize * popup_stride + (left - rect.x) as usize * 4;
        let to = y as usize * stride + left as usize * 4;
        pixels[to..to + bytes].copy_from_slice(&popup.buffer[from..from + bytes]);
    }
    guard.mark_dirty(Rect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    });
}

pub fn values_from_list(list: &List, from: usize) -> Vec<Value> {
    (from..list.len())
        .map(|idx| match list.get_type(idx) {
            ValueType::String => Value::String(list.string(idx).to_string()),
            ValueType::Bool => Value::Bool(list.bool(idx)),
            ValueType::Integer => Value::Int(list.integer(idx)),
            ValueType::Double => Value::Double(list.double(idx)),
            _ => Value::Null,
        })
        .collect()
}

pub fn list_from_values(values: &[Value]) -> List {
    let list = List::new();
    for (idx, value) in values.iter().enumerate() {
        match value {
            Value::Null => list.set_null(idx),
            Value::Bool(v) => list.set_bool(idx, *v),
            Value::Int(v) => list.set_integer(idx, *v),
            Value::Double(v) => list.set_double(idx, *v),
            Value::String(v) => list.set_string(idx, &CefString::new(v)),
        }
    }
    list
}

impl View {
    pub fn new(id: u32, writer: Arc<Writer>, surface: Option<Surface>, hidden: bool, frame_rate: i32) -> Arc<View> {
        Arc::new(View {
            id,
            writer,
            browser: Mutex::new(None),
            surface: Mutex::new(surface),
            hidden: AtomicBool::new(hidden),
            frame_rate: AtomicI32::new(frame_rate),
            popup: Mutex::new(Popup {
                show: false,
                rect: cef_rect_t {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                },
                buffer: Vec::new(),
            }),
            buttons: AtomicU32::new(0),
            clicks: Mutex::new(Clicks::default()),
            paints: Mutex::new(ipc::stats::Stats::default()),
            readback: Mutex::new(None),
            external_begin_frame: AtomicBool::new(false),
            begin_frame_queued: AtomicBool::new(false),
            last_begin_frame: Mutex::new(Instant::now()),
        })
    }

    pub fn set_external_begin_frame(&self, enabled: bool) {
        self.external_begin_frame.store(enabled, Ordering::SeqCst);
    }

    /// Пора просить у Chromium новый кадр (pacing.rs): браузер есть и виден, запрос не стоит в
    /// очереди, а игра забрала прошлый кадр — или давно не просили (не застрять, если игра не
    /// забирает, например свёрнута, — тогда раз в `idle`).
    #[cfg_attr(not(feature = "modern"), allow(dead_code))] // внешний BeginFrame — только CEF 150
    pub fn wants_begin_frame(&self, idle: Duration) -> bool {
        if !self.external_begin_frame.load(Ordering::SeqCst)
            || self.hidden.load(Ordering::SeqCst)
            || self.begin_frame_queued.load(Ordering::SeqCst)
            || self.browser.lock().is_none()
        {
            return false;
        }
        let pending = self.surface.lock().as_ref().is_some_and(|s| s.pending());
        !pending || self.last_begin_frame.lock().elapsed() >= idle
    }

    /// Пометить, что запрос кадра поставлен в очередь UI-потока.
    #[cfg_attr(not(feature = "modern"), allow(dead_code))] // внешний BeginFrame — только CEF 150
    pub fn queue_begin_frame(&self) -> bool {
        !self.begin_frame_queued.swap(true, Ordering::SeqCst)
    }

    /// Запрос в очередь не встал.
    #[cfg_attr(not(feature = "modern"), allow(dead_code))] // внешний BeginFrame — только CEF 150
    pub fn cancel_begin_frame(&self) {
        self.begin_frame_queued.store(false, Ordering::SeqCst);
    }

    /// В UI-потоке CEF: запросить кадр.
    #[cfg_attr(not(feature = "modern"), allow(dead_code))] // внешний BeginFrame — только CEF 150
    pub fn begin_frame(&self) {
        *self.last_begin_frame.lock() = Instant::now();
        if let Some(host) = self.browser().map(|b| b.host()) {
            #[cfg(feature = "modern")]
            host.send_external_begin_frame();
            #[cfg(not(feature = "modern"))]
            let _ = host;
        }
        self.begin_frame_queued.store(false, Ordering::SeqCst);
    }

    /// Кадры текстурой на видеокарте (render = shared). До создания браузера.
    pub fn enable_shared(self: &Arc<View>, device: Arc<Device>) {
        *self.readback.lock() = Some(Readback::new(device, Arc::downgrade(self)));
    }

    /// Кадр из промежуточной текстуры (поток чтения в gpu.rs): строки `pitch`, область `rect`
    /// на тех же местах, что в кадре `width × height`.
    pub fn write_frame(&self, data: *const u8, pitch: usize, rect: Rect, width: u32, height: u32) {
        let started = Instant::now();
        let surface = self.surface.lock();
        let Some(surface) = surface.as_ref() else {
            return;
        };
        if surface.width() != width || surface.height() != height {
            return; // кадр старого размера
        }

        let guard = surface.lock();
        let stride = guard.stride();
        let pixels = guard.pixels();
        let bytes = rect.width as usize * 4;
        for y in rect.y as usize..(rect.y + rect.height) as usize {
            let at = y * stride + rect.x as usize * 4;
            unsafe {
                std::ptr::copy_nonoverlapping(data.add(y * pitch + rect.x as usize * 4), pixels.as_mut_ptr().add(at), bytes);
            }
        }
        guard.mark_dirty(rect);
        draw_popup(&self.popup.lock(), &guard, width as i32, height as i32);
        drop(guard);

        let mut paints = self.paints.lock();
        paints.add(started.elapsed(), (rect.width * rect.height) as u64);
        if let Some(line) = paints.report() {
            log::info!("браузер {}: кадры от CEF (видеокарта, без ожидания) {}", self.id, line);
        }
    }

    pub fn browser(&self) -> Option<Browser> {
        self.browser.lock().as_ref().cloned()
    }

    pub fn set_surface(&self, surface: Option<Surface>) {
        *self.surface.lock() = surface;
        if let Some(host) = self.browser().map(|b| b.host()) {
            host.was_resized();
            host.invalidate(PaintElement::View);
        }
    }

    pub fn hide(&self, hide: bool) {
        self.hidden.store(hide, Ordering::SeqCst);
        if let Some(host) = self.browser().map(|b| b.host()) {
            host.was_hidden(hide);
            host.set_audio_muted(hide);
            if !hide {
                host.invalidate(PaintElement::View);
            }
        }
    }

    pub fn set_frame_rate(&self, fps: i32) {
        self.frame_rate.store(fps, Ordering::SeqCst);
        if let Some(host) = self.browser().map(|b| b.host()) {
            host.set_windowless_frame_rate(fps);
        }
    }

    pub fn mouse_move(&self, x: i32, y: i32) {
        if let Some(host) = self.browser().map(|b| b.host()) {
            let modifiers = self.buttons.load(Ordering::SeqCst);
            host.send_mouse_move(cef_mouse_event_t { x, y, modifiers });
        }
    }

    pub fn mouse_click(&self, x: i32, y: i32, button: MouseButton, down: bool) {
        let flag = button_flag(button);
        let count = {
            let mut clicks = self.clicks.lock();
            if down {
                let now = Instant::now();
                let repeat = clicks.button == Some(button)
                    && clicks.at.is_some_and(|at| now.duration_since(at) <= DOUBLE_CLICK_TIME)
                    && (clicks.x - x).abs() <= DOUBLE_CLICK_DISTANCE
                    && (clicks.y - y).abs() <= DOUBLE_CLICK_DISTANCE;
                clicks.count = if repeat { (clicks.count % 3) + 1 } else { 1 };
                clicks.at = Some(now);
                clicks.x = x;
                clicks.y = y;
                clicks.button = Some(button);
            }
            clicks.count.max(1)
        };

        // Флаг кнопки — в событии нажатия есть, в событии отпускания уже нет
        let modifiers = if down {
            self.buttons.fetch_or(flag, Ordering::SeqCst) | flag
        } else {
            self.buttons.fetch_and(!flag, Ordering::SeqCst) & !flag
        };

        if let Some(host) = self.browser().map(|b| b.host()) {
            host.send_mouse_click_count(button_type(button), cef_mouse_event_t { x, y, modifiers }, down, count);
        }
    }

    pub fn mouse_wheel(&self, x: i32, y: i32, delta: i32) {
        if let Some(host) = self.browser().map(|b| b.host()) {
            let modifiers = self.buttons.load(Ordering::SeqCst);
            host.send_mouse_wheel_event(cef_mouse_event_t { x, y, modifiers }, delta);
        }
    }

    pub fn key(&self, event: KeyEvent) {
        let Some(host) = self.browser().map(|b| b.host()) else {
            return;
        };
        let mut cef_event: cef_key_event_t = unsafe { std::mem::zeroed() };
        cef_event.type_ = match event.kind {
            KeyEventKind::RawKeyDown => cef_key_event_type_t::KEYEVENT_RAWKEYDOWN,
            KeyEventKind::KeyUp => cef_key_event_type_t::KEYEVENT_KEYUP,
            KeyEventKind::Char => cef_key_event_type_t::KEYEVENT_CHAR,
        };
        cef_event.modifiers = event.modifiers;
        cef_event.windows_key_code = event.windows_key_code;
        cef_event.native_key_code = event.native_key_code;
        cef_event.is_system_key = event.is_system_key as i32;
        host.send_keyboard_event(cef_event);
    }

    pub fn trigger(&self, name: &str, args: &[Value]) {
        if let Some(frame) = self.browser().map(|b| b.main_frame()) {
            let msg = ProcessMessage::create("trigger_event");
            let list = msg.argument_list();
            list.set_string(0, &CefString::new(name));
            list.set_list(1, list_from_values(args));
            frame.send_process_message(ProcessId::Renderer, msg);
        }
    }

    pub fn load_url(&self, url: &str) {
        if let Some(browser) = self.browser() {
            let url = crate::assets_scheme::resolve_browser_url(url);
            browser.main_frame().load_url(&url);
        }
    }

    pub fn dev_tools(&self, enabled: bool) {
        use winapi::um::winuser::*;

        let Some(host) = self.browser().map(|b| b.host()) else {
            return;
        };
        if !enabled {
            host.close_dev_tools();
            return;
        }

        let caption = format!("Dev Tools for {} browser", self.id);
        let window_name = CefString::new(&caption);
        let mut window_info = unsafe { std::mem::zeroed::<cef_sys::cef_window_info_t>() };
        window_info.style = WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN | WS_CLIPSIBLINGS | WS_VISIBLE;
        // CEF 150: положение окна — в bounds, у структуры есть поле size
        #[cfg(feature = "modern")]
        {
            window_info.size = std::mem::size_of::<cef_sys::cef_window_info_t>();
            window_info.bounds = cef_sys::cef_rect_t {
                x: CW_USEDEFAULT,
                y: CW_USEDEFAULT,
                width: CW_USEDEFAULT,
                height: CW_USEDEFAULT,
            };
        }
        #[cfg(not(feature = "modern"))]
        {
            window_info.x = CW_USEDEFAULT;
            window_info.y = CW_USEDEFAULT;
            window_info.width = CW_USEDEFAULT;
            window_info.height = CW_USEDEFAULT;
        }
        window_info.window_name = window_name.to_cef_string();
        window_info.windowless_rendering_enabled = 0;

        let mut settings = unsafe { std::mem::zeroed::<cef_sys::cef_browser_settings_t>() };
        settings.size = std::mem::size_of::<cef_sys::cef_browser_settings_t>();
        host.open_dev_tools(&window_info, &settings);
    }

    pub fn close(&self) {
        if let Some(host) = self.browser().map(|b| b.host()) {
            host.close_browser(true);
        }
        *self.surface.lock() = None;
    }

    fn send(&self, message: ToGame) {
        self.writer.send(&message);
    }
}

impl LifespanHandler for ViewRef {
    fn on_after_created(&self, browser: Browser) {
        *self.0.browser.lock() = Some(browser);
        log::info!("browser {} created", self.0.id);

        let view = &self.0;
        view.set_frame_rate(view.frame_rate.load(Ordering::SeqCst));
        if view.hidden.load(Ordering::SeqCst) {
            view.hide(true);
        }
    }

    fn on_before_close(&self, _: Browser) {
        if let Some(browser) = self.0.browser.lock().take() {
            browser.host().close_dev_tools();
        }
    }
}

impl RenderHandler for ViewRef {
    fn view_rect(&self, _: Browser, rect: &mut cef_rect_t) {
        let surface = self.0.surface.lock();
        let (width, height) = surface.as_ref().map(|s| (s.width(), s.height())).unwrap_or((1, 1));
        *rect = cef_rect_t {
            x: 0,
            y: 0,
            width: width.max(1) as i32,
            height: height.max(1) as i32,
        };
    }

    fn on_popup_show(&self, browser: Browser, show: bool) {
        {
            let mut popup = self.0.popup.lock();
            popup.show = show;
            if !show {
                popup.buffer.clear();
            }
        }
        if !show {
            // Под закрытым списком — то, что было: перерисовать кадр целиком
            browser.host().invalidate(PaintElement::View);
        }
    }

    fn on_popup_size(&self, _: Browser, rect: &cef_rect_t) {
        let mut popup = self.0.popup.lock();
        popup.rect = *rect;
        popup.buffer.resize((rect.width.max(0) * rect.height.max(0) * 4) as usize, 0);
    }

    fn on_paint(
        &self, _: Browser, paint_type: PaintElement, dirty_rects: DirtyRects, buffer: &[u8],
        width: usize, height: usize,
    ) {
        let surface = self.0.surface.lock();
        let Some(surface) = surface.as_ref() else {
            return;
        };
        // Кадр старого размера, пока игра не прислала новую память, — пропускаем
        let (w, h) = (surface.width() as i32, surface.height() as i32);

        match paint_type {
            PaintElement::View => {
                if width as i32 != w || height as i32 != h || buffer.len() < width * height * 4 {
                    return;
                }
                let started = std::time::Instant::now();
                let guard = surface.lock();
                for rect in dirty_rects.as_slice() {
                    copy_rect(buffer, &guard, rect, w, h);
                }
                draw_popup(&self.0.popup.lock(), &guard, w, h);
                drop(guard);

                let pixels: u64 = dirty_rects.as_slice().iter().map(|r| (r.width.max(0) * r.height.max(0)) as u64).sum();
                let mut paints = self.0.paints.lock();
                paints.add(started.elapsed(), pixels);
                if let Some(line) = paints.report() {
                    log::info!("браузер {}: кадры от CEF {}", self.0.id, line);
                }
            }

            PaintElement::Popup => {
                let mut popup = self.0.popup.lock();
                if popup.buffer.len() != buffer.len() || width as i32 != popup.rect.width || height as i32 != popup.rect.height {
                    return;
                }
                popup.buffer.copy_from_slice(buffer);
                let guard = surface.lock();
                draw_popup(&popup, &guard, w, h);
            }
        }
    }

    fn on_accelerated_paint(&self, _: Browser, paint_type: PaintElement, dirty_rects: DirtyRects, handle: *mut std::ffi::c_void) {
        self.accelerated_paint(paint_type, dirty_rects, handle);
    }
}

impl ViewRef {
    fn accelerated_paint(&self, paint_type: PaintElement, dirty_rects: DirtyRects, handle: *mut std::ffi::c_void) {
        let readback = self.0.readback.lock();
        let Some(readback) = readback.as_ref() else {
            return;
        };
        match paint_type {
            PaintElement::View => {
                if let Some(rect) = crate::gpu::union(dirty_rects.as_slice()) {
                    readback.on_paint(handle, rect);
                }
            }
            PaintElement::Popup => {
                let Some((pixels, width, height)) = readback.read_popup(handle) else {
                    return;
                };
                let mut popup = self.0.popup.lock();
                if width as i32 != popup.rect.width || height as i32 != popup.rect.height {
                    return;
                }
                popup.buffer = pixels;
                let surface = self.0.surface.lock();
                if let Some(surface) = surface.as_ref() {
                    let guard = surface.lock();
                    draw_popup(&popup, &guard, surface.width() as i32, surface.height() as i32);
                }
            }
        }
    }
}

impl LoadHandler for ViewRef {
    fn on_load_end(&self, _browser: Browser, frame: Frame, status_code: i32) {
        log::info!("browser {} load end: {}", self.0.id, status_code);
        if frame.is_main() {
            self.0.send(ToGame::BrowserCreated {
                id: self.0.id,
                status: status_code,
            });
        }
    }
}

impl ContextMenuHandler for ViewRef {
    fn on_before_context_menu(&self, _: Browser, _: Frame, _: ContextMenuParams, model: MenuModel) {
        model.clear();
    }
}

impl AudioHandler for ViewRef {}

#[cfg(feature = "modern")]
impl cef::handlers::permission::PermissionHandler for ViewRef {}

fn flag_arg(list: &List) -> bool {
    match list.get_type(0) {
        ValueType::Integer => list.integer(0) == 1,
        ValueType::Bool => list.bool(0),
        _ => false,
    }
}

impl Client for ViewRef {
    type LifespanHandler = Self;
    type RenderHandler = Self;
    type ContextMenuHandler = Self;
    type LoadHandler = Self;
    type AudioHandler = Self;
    #[cfg(feature = "modern")]
    type PermissionHandler = Self;

    fn lifespan_handler(&self) -> Option<Self> {
        Some(self.clone())
    }

    fn render_handler(&self) -> Option<Self> {
        Some(self.clone())
    }

    fn context_menu_handler(&self) -> Option<Self> {
        Some(self.clone())
    }

    fn load_handler(&self) -> Option<Self> {
        Some(self.clone())
    }

    fn on_process_message(&self, _browser: Browser, _frame: Frame, _source: ProcessId, msg: ProcessMessage) -> bool {
        let id = self.0.id;
        match msg.name().to_string().as_str() {
            "set_focus" => {
                let focus = flag_arg(&msg.argument_list());
                self.0.send(ToGame::Focus { id, focus });
                true
            }

            "hide" => {
                let hide = flag_arg(&msg.argument_list());
                self.0.send(ToGame::Hide { id, hide });
                true
            }

            "emit_event" => {
                let args = msg.argument_list();
                if args.get_type(0) != ValueType::String {
                    return true;
                }
                let name = args.string(0).to_string();
                self.0.send(ToGame::JsEvent {
                    id,
                    name,
                    args: values_from_list(&args, 1),
                });
                true
            }

            _ => false,
        }
    }
}
