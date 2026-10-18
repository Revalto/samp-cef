use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;

use crate::browser::manager::Manager;
use crate::static_cell::StaticCell;

use client_api::gta::menu_manager::CMenuManager;

use retour::GenericDetour;

static RENDER: StaticCell<Render> = StaticCell::new();

const REFERENCE_FRAMES: u64 = 10;

const DRAWING_EVENT: usize = 0x58FAE0;
const SHUTDOWN_RW_EVENT: usize = 0x53BB80;

type DrawingEventFn = extern "C" fn();
type ShutdownRwEventFn = extern "C" fn();

struct FrameCounter {
    start_at: Instant,
    frames: u64,
    last_fps: u64,
}

struct Render {
    manager: Arc<Mutex<Manager>>,
    drawing_event: GenericDetour<DrawingEventFn>,
    shutdown_event: GenericDetour<ShutdownRwEventFn>,
    counter: FrameCounter,
}

impl Render {
    fn get<'a>() -> Option<&'a mut Render> {
        unsafe { RENDER.get_mut() }
    }

    fn calc_frames(&mut self) -> Option<u64> {
        let counter = &mut self.counter;

        counter.frames += 1;

        if counter.frames == REFERENCE_FRAMES {
            let elapsed = counter.start_at.elapsed().as_millis() as u64;

            let fps = (REFERENCE_FRAMES * 1000) / elapsed;

            counter.last_fps = fps;
            counter.frames = 0;
            counter.start_at = Instant::now();

            return Some(fps);
        }

        None
    }
}

pub fn initialize(manager: Arc<Mutex<Manager>>) {
    let drawing_event = unsafe {
        let func: DrawingEventFn = std::mem::transmute(DRAWING_EVENT);
        let hook = GenericDetour::new(func, drawing_event).unwrap();

        hook.enable().unwrap();
        hook
    };

    let shutdown_event = unsafe {
        let func: ShutdownRwEventFn = std::mem::transmute(SHUTDOWN_RW_EVENT);
        let hook = GenericDetour::new(func, shutdown_event).unwrap();

        hook.enable().unwrap();
        hook
    };

    log::trace!("hooking ok ...");

    let counter = FrameCounter {
        start_at: Instant::now(),
        frames: 0,
        last_fps: 0,
    };

    let render = Render {
        manager,
        drawing_event,
        shutdown_event,
        counter,
    };

    unsafe {
        RENDER.set(render);
    }
}

pub fn uninitialize() {
    unsafe {
        RENDER.take();
    }
}

fn on_render() {
    crate::app::mainloop();
}

pub fn render() {
    if let Some(render) = Render::get() {
        let fps = render.calc_frames();

        {
            let mut manager = render.manager.lock();

            if let Some(&fps) = fps.as_ref() {
                manager.update_fps(fps);
            }

            manager.do_not_draw(CMenuManager::is_menu_active());
            manager.draw();
        }
    }
}

fn on_destroy() {
    log::trace!("rwshutdown ...");

    if let Some(render) = Render::get() {
        let mut manager = render.manager.lock();
        manager.remove_views();
    }
}



extern "C" fn drawing_event() {
    if let Some(render) = Render::get() {
        render.drawing_event.call();
    }

    on_render();
}

extern "C" fn shutdown_event() {
    on_destroy();

    if let Some(render) = Render::get() {
        render.shutdown_event.call();
    }
}
