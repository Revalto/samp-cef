//! Темп кадров интерфейса (cef-modern: общая текстура + внешний BeginFrame).
//!
//! Таймер Chromium в режиме без окна отдаёт не больше 60 кадров/с, а у половины игроков FPS не
//! ограничен. Здесь кадры просит сам хост: с частотой игры (`ToHost::FrameRate`), но не чаще
//! `max_fps` (cef_host.ini, по умолчанию 144) и только когда игра забрала прошлый кадр
//! (`Surface::pending`). Интерфейс идёт вровень с игрой, а кадров, которые игра всё равно не
//! покажет, Chromium не рисует. Ничего не меняется на странице — Chromium кадр и не отдаёт.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::view::View;

const MIN_FPS: i32 = 30;
const DEFAULT_MAX_FPS: i32 = 144;
/// Игра не забирает кадры (свёрнута, загрузка) — всё равно просить, но не чаще
const IDLE: Duration = Duration::from_millis(100);

/// Для лога раз в 5 с: сколько запросов кадра поставлено в UI-поток CEF и сколько выполнено
pub static POSTED: AtomicU32 = AtomicU32::new(0);
pub static EXECUTED: AtomicU32 = AtomicU32::new(0);
pub static PAINTS: AtomicU32 = AtomicU32::new(0);

pub fn start(views: Arc<Mutex<HashMap<u32, Arc<View>>>>, game_fps: Arc<AtomicI32>) {
    let max = crate::app::config_value("max_fps")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_FPS)
        .clamp(MIN_FPS, 360);
    log::info!("кадры интерфейса: по запросу хоста, с частотой игры, не больше {} к/с (max_fps в cef_host.ini)", max);

    let spawned = std::thread::Builder::new().name("pacing".into()).spawn(move || {
        // Sleep с точностью до 1 мс, а не 15,6
        unsafe {
            winapi::um::timeapi::timeBeginPeriod(1);
        }
        let mut next = Instant::now();
        let mut report = Instant::now();
        loop {
            if report.elapsed() >= Duration::from_secs(5) {
                let secs = report.elapsed().as_secs_f64();
                log::info!(
                    "запросы кадров: поставлено {:.1}/с, выполнено {:.1}/с, кадров от Chromium {:.1}/с (частота игры {})",
                    POSTED.swap(0, Ordering::Relaxed) as f64 / secs,
                    EXECUTED.swap(0, Ordering::Relaxed) as f64 / secs,
                    PAINTS.swap(0, Ordering::Relaxed) as f64 / secs,
                    game_fps.load(Ordering::SeqCst)
                );
                report = Instant::now();
            }
            let fps = game_fps.load(Ordering::SeqCst).clamp(MIN_FPS, max);
            next += Duration::from_secs_f64(1.0 / fps as f64);
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                next = now; // отстали (хост был занят) — не догоняем пачкой
            }

            let list: Vec<Arc<View>> = views.lock().values().cloned().collect();
            for view in list {
                if !view.wants_begin_frame(IDLE) || !view.queue_begin_frame() {
                    continue;
                }
                let target = view.clone();
                if cef::task::post_ui_task(move || target.begin_frame()) {
                    POSTED.fetch_add(1, Ordering::Relaxed);
                } else {
                    view.cancel_begin_frame(); // CEF ещё не запущен
                }
            }
        }
    });
    if let Err(error) = spawned {
        log::error!("поток кадров интерфейса не запустился: {}", error);
    }
}
