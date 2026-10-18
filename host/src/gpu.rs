//! Режим `render = shared`: кадр от Chromium — текстурой на видеокарте (OnAcceleratedPaint).
//!
//! В обычном режиме с видеокартой Chromium сам читает каждый кадр в память и ждёт, пока чтение
//! закончится, — на 3440×1440 это ~40 кадров/с. Здесь он отдаёт общую текстуру D3D11, а мы:
//!   1. в вызове копируем изменившуюся область в свою промежуточную текстуру (на видеокарте,
//!      доли миллисекунды) и ждём только эту копию — текстура Chromium действительна лишь
//!      внутри вызова;
//!   2. отпускаем Chromium, а чтение в память и запись в общую память игры делает отдельный
//!      поток. Промежуточных текстур три — пока одна читается, в другие уже копируются новые.

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::mpsc;
use std::sync::{Arc, Weak};

use parking_lot::{Condvar, Mutex};

use winapi::Interface;
use winapi::shared::dxgiformat::DXGI_FORMAT_B8G8R8A8_UNORM;
use winapi::shared::dxgitype::DXGI_SAMPLE_DESC;
use winapi::shared::minwindef::BOOL;
use winapi::shared::winerror::{S_OK, SUCCEEDED};
use winapi::um::d3d11::{
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_QUERY_DESC,
    D3D11_QUERY_EVENT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Asynchronous, ID3D11Device, ID3D11DeviceContext, ID3D11Query,
    ID3D11Resource, ID3D11Texture2D,
};
use winapi::um::d3dcommon::D3D_DRIVER_TYPE_HARDWARE;
use winapi::um::unknwnbase::IUnknown;

use ipc::surface::Rect;

use crate::view::View;

const STAGING_COUNT: usize = 3;

/// Указатель COM, который отпускается сам.
struct Com<T: Interface>(*mut T);

unsafe impl<T: Interface> Send for Com<T> {}
unsafe impl<T: Interface> Sync for Com<T> {}

impl<T: Interface> Com<T> {
    fn ptr(&self) -> *mut T {
        self.0
    }
}

impl<T: Interface> Drop for Com<T> {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                (*(self.0 as *mut IUnknown)).Release();
            }
        }
    }
}

/// Своё устройство D3D11 на весь хост. Немедленный контекст однопоточный — под замком.
pub struct Device {
    device: Com<ID3D11Device>,
    /// CEF 150 отдаёт NT-дескриптор — открывает его только ID3D11Device1::OpenSharedResource1
    #[cfg(feature = "modern")]
    device1: Option<Com<winapi::um::d3d11_1::ID3D11Device1>>,
    context: Mutex<Com<ID3D11DeviceContext>>,
    query: Com<ID3D11Query>,
}

impl Device {
    pub fn create() -> Option<Arc<Device>> {
        let mut device = null_mut();
        let mut context = null_mut();
        let mut level = 0;
        let hr = unsafe {
            D3D11CreateDevice(
                null_mut(),
                D3D_DRIVER_TYPE_HARDWARE,
                null_mut(),
                0,
                null_mut(),
                0,
                D3D11_SDK_VERSION,
                &mut device,
                &mut level,
                &mut context,
            )
        };
        if !SUCCEEDED(hr) || device.is_null() || context.is_null() {
            log::error!("D3D11CreateDevice: 0x{:08X}", hr as u32);
            return None;
        }
        log::info!("D3D11 для общей текстуры: уровень 0x{:X}", level);
        let device = Com(device);
        let context = Com(context);

        let desc = D3D11_QUERY_DESC {
            Query: D3D11_QUERY_EVENT,
            MiscFlags: 0,
        };
        let mut query = null_mut();
        let hr = unsafe { (*device.ptr()).CreateQuery(&desc, &mut query) };
        if !SUCCEEDED(hr) {
            log::error!("CreateQuery: 0x{:08X}", hr as u32);
            return None;
        }

        #[cfg(feature = "modern")]
        let device1 = {
            let mut device1: *mut winapi::ctypes::c_void = null_mut();
            let hr = unsafe {
                (*(device.ptr() as *mut IUnknown))
                    .QueryInterface(&winapi::um::d3d11_1::ID3D11Device1::uuidof(), &mut device1)
            };
            if !SUCCEEDED(hr) || device1.is_null() {
                log::error!("ID3D11Device1 нет (0x{:08X}) — общая текстура CEF 150 не откроется", hr as u32);
                return None;
            }
            Some(Com(device1 as *mut winapi::um::d3d11_1::ID3D11Device1))
        };

        Some(Arc::new(Device {
            device,
            #[cfg(feature = "modern")]
            device1,
            context: Mutex::new(context),
            query: Com(query),
        }))
    }

    fn open(&self, handle: *mut c_void) -> Option<Com<ID3D11Texture2D>> {
        let mut texture: *mut winapi::ctypes::c_void = null_mut();
        #[cfg(feature = "modern")]
        let hr = match self.device1.as_ref() {
            Some(device1) => unsafe {
                (*device1.ptr()).OpenSharedResource1(handle as _, &ID3D11Texture2D::uuidof(), &mut texture)
            },
            None => return None,
        };
        #[cfg(not(feature = "modern"))]
        let hr = unsafe {
            (*self.device.ptr()).OpenSharedResource(handle as _, &ID3D11Texture2D::uuidof(), &mut texture)
        };
        if !SUCCEEDED(hr) || texture.is_null() {
            log::error!("OpenSharedResource: 0x{:08X}", hr as u32);
            return None;
        }
        Some(Com(texture as *mut ID3D11Texture2D))
    }

    fn staging(&self, width: u32, height: u32) -> Option<Com<ID3D11Texture2D>> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ,
            MiscFlags: 0,
        };
        let mut texture = null_mut();
        let hr = unsafe { (*self.device.ptr()).CreateTexture2D(&desc, null_mut(), &mut texture) };
        if !SUCCEEDED(hr) {
            log::error!("CreateTexture2D {}x{}: 0x{:08X}", width, height, hr as u32);
            return None;
        }
        Some(Com(texture))
    }

    /// Копия области `rect` из `source` в то же место `target` и ожидание, пока видеокарта её
    /// сделает (после этого текстура Chromium нам больше не нужна).
    fn copy_and_wait(&self, target: &Com<ID3D11Texture2D>, source: &Com<ID3D11Texture2D>, rect: Rect) {
        let context = self.context.lock();
        let ctx = context.ptr();
        let source_box = D3D11_BOX {
            left: rect.x as u32,
            top: rect.y as u32,
            front: 0,
            right: (rect.x + rect.width) as u32,
            bottom: (rect.y + rect.height) as u32,
            back: 1,
        };
        unsafe {
            (*ctx).CopySubresourceRegion(
                target.ptr() as *mut ID3D11Resource,
                0,
                rect.x as u32,
                rect.y as u32,
                0,
                source.ptr() as *mut ID3D11Resource,
                0,
                &source_box,
            );
            let query = self.query.ptr() as *mut ID3D11Asynchronous;
            (*ctx).End(query);
            let mut done: BOOL = 0;
            loop {
                let hr = (*ctx).GetData(query, &mut done as *mut BOOL as *mut winapi::ctypes::c_void, std::mem::size_of::<BOOL>() as u32, 0);
                if hr == S_OK && done != 0 {
                    break;
                }
                if !SUCCEEDED(hr) {
                    break; // устройство потеряно — дальше Map вернёт ошибку
                }
                std::thread::yield_now();
            }
        }
    }

    /// Содержимое промежуточной текстуры: (указатель, длина строки). Держать до unmap.
    fn map(&self, texture: &Com<ID3D11Texture2D>) -> Option<(*const u8, usize)> {
        let context = self.context.lock();
        let mut mapped: D3D11_MAPPED_SUBRESOURCE = unsafe { std::mem::zeroed() };
        let hr = unsafe {
            (*context.ptr()).Map(texture.ptr() as *mut ID3D11Resource, 0, D3D11_MAP_READ, 0, &mut mapped)
        };
        if !SUCCEEDED(hr) || mapped.pData.is_null() {
            log::error!("Map: 0x{:08X}", hr as u32);
            return None;
        }
        Some((mapped.pData as *const u8, mapped.RowPitch as usize))
    }

    fn unmap(&self, texture: &Com<ID3D11Texture2D>) {
        let context = self.context.lock();
        unsafe {
            (*context.ptr()).Unmap(texture.ptr() as *mut ID3D11Resource, 0);
        }
    }
}

fn texture_size(texture: &Com<ID3D11Texture2D>) -> (u32, u32) {
    let mut desc: D3D11_TEXTURE2D_DESC = unsafe { std::mem::zeroed() };
    unsafe {
        (*texture.ptr()).GetDesc(&mut desc);
    }
    (desc.Width, desc.Height)
}

struct Slot {
    texture: Arc<Com<ID3D11Texture2D>>,
    busy: bool,
}

struct State {
    width: u32,
    height: u32,
    slots: Vec<Slot>,
    /// Текстуры Chromium по общему дескриптору: их немного, и они повторяются
    opened: HashMap<usize, Com<ID3D11Texture2D>>,
}

struct Job {
    slot: usize,
    texture: Arc<Com<ID3D11Texture2D>>,
    rect: Rect,
    width: u32,
    height: u32,
}

/// Чтение кадров одного браузера.
pub struct Readback {
    device: Arc<Device>,
    state: Arc<(Mutex<State>, Condvar)>,
    jobs: mpsc::Sender<Job>,
}

impl Readback {
    pub fn new(device: Arc<Device>, view: Weak<View>) -> Readback {
        let state = Arc::new((
            Mutex::new(State {
                width: 0,
                height: 0,
                slots: Vec::new(),
                opened: HashMap::new(),
            }),
            Condvar::new(),
        ));
        let (jobs, rx) = mpsc::channel::<Job>();

        let worker_device = device.clone();
        let worker_state = state.clone();
        std::thread::spawn(move || {
            // Канал закрыт — браузер закрыт, выходим
            while let Ok(job) = rx.recv() {
                if let Some((data, pitch)) = worker_device.map(&job.texture) {
                    if let Some(view) = view.upgrade() {
                        view.write_frame(data, pitch, job.rect, job.width, job.height);
                    }
                    worker_device.unmap(&job.texture);
                }
                let (lock, ready) = &*worker_state;
                if let Some(slot) = lock.lock().slots.get_mut(job.slot) {
                    // Слоты могли пересоздать под новый размер — тогда это уже другой
                    if Arc::ptr_eq(&slot.texture, &job.texture) {
                        slot.busy = false;
                    }
                }
                ready.notify_all();
            }
        });

        Readback { device, state, jobs }
    }

    /// Вызов из OnAcceleratedPaint (вид, не всплывающий список).
    pub fn on_paint(&self, handle: *mut c_void, rect: Rect) {
        let (lock, ready) = &*self.state;
        let mut state = lock.lock();

        let key = handle as usize;
        // CEF 150: NT-дескриптор живёт только внутри вызова, его номер потом достаётся другой
        // текстуре — по номеру не кэшируем, открываем каждый кадр (это десятки микросекунд)
        if cfg!(feature = "modern") {
            state.opened.remove(&key);
        }
        if !state.opened.contains_key(&key) {
            let Some(texture) = self.device.open(handle) else {
                return;
            };
            if state.opened.len() >= 8 {
                state.opened.clear();
            }
            state.opened.insert(key, texture);
        }
        let (width, height) = texture_size(&state.opened[&key]);

        if state.width != width || state.height != height || state.slots.is_empty() {
            // Новый размер: ждём, пока поток дочитает старые, и создаём новые
            while state.slots.iter().any(|slot| slot.busy) {
                ready.wait(&mut state);
            }
            state.slots.clear();
            for _ in 0..STAGING_COUNT {
                match self.device.staging(width, height) {
                    Some(texture) => state.slots.push(Slot {
                        texture: Arc::new(texture),
                        busy: false,
                    }),
                    None => return,
                }
            }
            state.width = width;
            state.height = height;
        }

        let rect = clip(rect, width, height);
        if rect.width <= 0 || rect.height <= 0 {
            return;
        }

        // Свободная промежуточная текстура; все заняты — ждём поток (иначе кадры терялись бы)
        let slot = loop {
            if let Some(index) = state.slots.iter().position(|slot| !slot.busy) {
                break index;
            }
            ready.wait(&mut state);
        };
        state.slots[slot].busy = true;
        let target = state.slots[slot].texture.clone();
        self.device.copy_and_wait(&target, &state.opened[&key], rect);
        drop(state);

        let _ = self.jobs.send(Job {
            slot,
            texture: target,
            rect,
            width,
            height,
        });
    }

    /// Всплывающий список (<select>): редко и маленький — читаем сразу.
    pub fn read_popup(&self, handle: *mut c_void) -> Option<(Vec<u8>, u32, u32)> {
        let source = self.device.open(handle)?;
        let (width, height) = texture_size(&source);
        let staging = self.device.staging(width, height)?;
        self.device.copy_and_wait(
            &staging,
            &source,
            Rect {
                x: 0,
                y: 0,
                width: width as i32,
                height: height as i32,
            },
        );
        let (data, pitch) = self.device.map(&staging)?;
        let row = width as usize * 4;
        let mut pixels = vec![0u8; row * height as usize];
        for y in 0..height as usize {
            unsafe {
                std::ptr::copy_nonoverlapping(data.add(y * pitch), pixels.as_mut_ptr().add(y * row), row);
            }
        }
        self.device.unmap(&staging);
        Some((pixels, width, height))
    }
}

/// Объединение прямоугольников CEF.
pub fn union(rects: &[cef_sys::cef_rect_t]) -> Option<Rect> {
    let mut iter = rects.iter().filter(|r| r.width > 0 && r.height > 0);
    let first = iter.next()?;
    let (mut left, mut top) = (first.x, first.y);
    let (mut right, mut bottom) = (first.x + first.width, first.y + first.height);
    for r in iter {
        left = left.min(r.x);
        top = top.min(r.y);
        right = right.max(r.x + r.width);
        bottom = bottom.max(r.y + r.height);
    }
    Some(Rect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    })
}

fn clip(rect: Rect, width: u32, height: u32) -> Rect {
    let left = rect.x.clamp(0, width as i32);
    let top = rect.y.clamp(0, height as i32);
    let right = (rect.x + rect.width).clamp(0, width as i32);
    let bottom = (rect.y + rect.height).clamp(0, height as i32);
    Rect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    }
}
