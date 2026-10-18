//! Кадр браузера в общей памяти.
//!
//! Раскладка: [`Header`] (64 байта), за ним пиксели BGRA построчно, строка — `width * 4` байт,
//! как в буфере `OnPaint` CEF. Создаёт игра под размер окна и передаёт имя хосту
//! (`ToHost::CreateBrowser` / `ToHost::Resize`). Хост в `OnPaint` копирует изменившиеся области и
//! расширяет «грязный» прямоугольник, игра раз в кадр забирает его в текстуру и сбрасывает.
//! Доступ — под спин-блокировкой в заголовке: она лежит в общей памяти и работает между
//! процессами. Игра её только пробует (`try_lock`): занято — заберёт в следующем кадре.

use std::os::windows::ffi::OsStrExt;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicU32, Ordering};

use winapi::shared::minwindef::FALSE;
use winapi::um::handleapi::INVALID_HANDLE_VALUE;
use winapi::um::memoryapi::{
    CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile,
};
use winapi::um::winnt::PAGE_READWRITE;

use crate::channel::Handle;

pub const HEADER_SIZE: usize = 64;

#[repr(C)]
pub struct Header {
    lock: AtomicU32,
    pub width: u32,
    pub height: u32,
    /// 1 — с прошлого забора что-то изменилось, область — [left, right) × [top, bottom)
    pub dirty: u32,
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    _reserved: [u32; 8],
}

const _: () = assert!(std::mem::size_of::<Header>() == HEADER_SIZE);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub struct Surface {
    _mapping: Handle,
    view: *mut u8,
    width: u32,
    height: u32,
}

unsafe impl Send for Surface {}
unsafe impl Sync for Surface {}

fn wide(name: &str) -> Vec<u16> {
    std::ffi::OsStr::new(name).encode_wide().chain(Some(0)).collect()
}

/// Имя общей памяти: процесс игры + браузер + номер (при смене размера — новая).
pub fn name(game_pid: u32, browser: u32, serial: u32) -> String {
    format!("Local\\samp-cef-{}-{}-{}", game_pid, browser, serial)
}

impl Surface {
    /// Игра: новая общая память под кадр `width × height`.
    pub fn create(name: &str, width: u32, height: u32) -> Option<Surface> {
        let size = HEADER_SIZE as u64 + width as u64 * height as u64 * 4;
        let name = wide(name);

        unsafe {
            let mapping = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                null_mut(),
                PAGE_READWRITE,
                (size >> 32) as u32,
                size as u32,
                name.as_ptr(),
            );
            if mapping.is_null() {
                return None;
            }
            let mapping = Handle(mapping);
            let view = MapViewOfFile(mapping.raw(), FILE_MAP_ALL_ACCESS, 0, 0, 0) as *mut u8;
            if view.is_null() {
                return None;
            }

            // Память новая — нули, остаётся заполнить размер
            let header = &mut *(view as *mut Header);
            header.width = width;
            header.height = height;

            Some(Surface {
                _mapping: mapping,
                view,
                width,
                height,
            })
        }
    }

    /// Хост: открыть созданную игрой.
    pub fn open(name: &str) -> Option<Surface> {
        let name = wide(name);

        unsafe {
            let mapping = OpenFileMappingW(FILE_MAP_ALL_ACCESS, FALSE, name.as_ptr());
            if mapping.is_null() {
                return None;
            }
            let mapping = Handle(mapping);
            let view = MapViewOfFile(mapping.raw(), FILE_MAP_ALL_ACCESS, 0, 0, 0) as *mut u8;
            if view.is_null() {
                return None;
            }

            let header = &*(view as *const Header);
            let (width, height) = (header.width, header.height);

            Some(Surface {
                _mapping: mapping,
                view,
                width,
                height,
            })
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    fn header(&self) -> &Header {
        unsafe { &*(self.view as *const Header) }
    }

    /// Ждёт блокировку (хост: копирование кадра короткое, игра держит её только на копию).
    pub fn lock(&self) -> SurfaceGuard<'_> {
        let lock = &self.header().lock;
        let mut spins = 0u32;
        while lock
            .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spins += 1;
            if spins < 64 {
                std::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }
        SurfaceGuard { surface: self }
    }

    /// Хост: игра ещё не забрала прошлый кадр (рукопожатие «кадр готов → кадр забран»: пока
    /// забрать не успела, новый кадр у Chromium не просим — лишняя работа и рывки).
    pub fn pending(&self) -> bool {
        let header = self.view as *const Header;
        unsafe { std::ptr::read_volatile(std::ptr::addr_of!((*header).dirty)) != 0 }
    }

    /// Игра: занято — не ждём, заберём в следующем кадре.
    pub fn try_lock(&self) -> Option<SurfaceGuard<'_>> {
        self.header()
            .lock
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| SurfaceGuard { surface: self })
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(self.view as *const _);
        }
    }
}

pub struct SurfaceGuard<'a> {
    surface: &'a Surface,
}

impl SurfaceGuard<'_> {
    #[allow(clippy::mut_from_ref)]
    fn header(&self) -> &mut Header {
        unsafe { &mut *(self.surface.view as *mut Header) }
    }

    /// Пиксели, строка — `width * 4` байт.
    #[allow(clippy::mut_from_ref)]
    pub fn pixels(&self) -> &mut [u8] {
        let size = self.surface.width as usize * self.surface.height as usize * 4;
        unsafe { std::slice::from_raw_parts_mut(self.surface.view.add(HEADER_SIZE), size) }
    }

    pub fn stride(&self) -> usize {
        self.surface.width as usize * 4
    }

    /// Хост: отметить изменившуюся область (обрезается по размеру кадра).
    pub fn mark_dirty(&self, rect: Rect) {
        let width = self.surface.width as i32;
        let height = self.surface.height as i32;
        let left = rect.x.clamp(0, width);
        let top = rect.y.clamp(0, height);
        let right = (rect.x + rect.width).clamp(0, width);
        let bottom = (rect.y + rect.height).clamp(0, height);
        if left >= right || top >= bottom {
            return;
        }

        let header = self.header();
        if header.dirty == 0 {
            header.left = left;
            header.top = top;
            header.right = right;
            header.bottom = bottom;
            header.dirty = 1;
        } else {
            header.left = header.left.min(left);
            header.top = header.top.min(top);
            header.right = header.right.max(right);
            header.bottom = header.bottom.max(bottom);
        }
    }

    /// Игра: изменившаяся с прошлого раза область, и сразу сбросить.
    pub fn take_dirty(&self) -> Option<Rect> {
        let header = self.header();
        if header.dirty == 0 {
            return None;
        }
        header.dirty = 0;
        Some(Rect {
            x: header.left,
            y: header.top,
            width: header.right - header.left,
            height: header.bottom - header.top,
        })
    }
}

impl Drop for SurfaceGuard<'_> {
    fn drop(&mut self) {
        self.surface.header().lock.store(0, Ordering::Release);
    }
}
