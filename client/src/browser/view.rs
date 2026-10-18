use ipc::surface::Rect;
use client_api::gta::matrix::CRect;
use client_api::gta::rw;
use client_api::gta::rw::rwcore::{RwRaster, RwTexture};
use client_api::gta::rw::rwplcore::{self, RwRGBA};
use client_api::gta::sprite::Sprite;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

pub struct RwLockGuard<'a> {
    bytes: &'a mut [u8],
    pub pitch: usize,
    raster: NonNull<RwRaster>,
}

impl RwLockGuard<'_> {
    #[inline(always)]
    pub fn bytes_as_mut_ptr(&mut self) -> *mut u8 {
        self.bytes.as_mut_ptr()
    }
}

impl Deref for RwLockGuard<'_> {
    type Target = [u8];

    #[inline(always)]
    fn deref(&self) -> &[u8] {
        self.bytes
    }
}

impl DerefMut for RwLockGuard<'_> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut [u8] {
        self.bytes
    }
}

impl Drop for RwLockGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            self.raster.as_mut().unlock();
        }
    }
}

pub struct RwContainer {
    texture: Option<NonNull<RwTexture>>,
    raster: Option<NonNull<RwRaster>>,
}

impl RwContainer {
    pub fn new(width: usize, height: usize) -> RwContainer {
        let raster = RwRaster::new(width as i32, height as i32);
        let texture = RwTexture::new(raster);

        RwContainer {
            texture: NonNull::new(texture),
            raster: NonNull::new(raster),
        }
    }

    #[inline]
    pub fn bytes(&mut self) -> Option<RwLockGuard<'_>> {
        unsafe {
            self.raster.as_mut().map(|raster| {
                let bytes = raster.as_mut().lock(0);
                // Строка растра может быть длиннее width * 4 (выравнивание) — берём pitch
                let size = {
                    let raster = raster.as_mut();
                    raster.height * raster.stride
                };

                RwLockGuard {
                    bytes: std::slice::from_raw_parts_mut(bytes, size as usize),
                    pitch: raster.as_mut().stride as usize,
                    raster: *raster,
                }
            })
        }
    }
}

impl Drop for RwContainer {
    fn drop(&mut self) {
        unsafe {
            if let Some(mut texture) = self.texture.take() {
                texture.as_mut().destroy();
            }

            if let Some(mut raster) = self.raster.take() {
                raster.as_mut().destroy();
            }
        }
    }
}

pub struct SpriteContainer {
    sprite: Sprite,
    rw: RwContainer,
}

impl SpriteContainer {
    pub fn new(width: usize, height: usize) -> SpriteContainer {
        let rw = RwContainer::new(width, height);
        let mut sprite = Sprite::new();

        if let Some(texture) = rw.texture {
            sprite.set_texture(texture.as_ptr());
        }

        SpriteContainer { sprite, rw }
    }

    #[inline]
    pub fn draw(&mut self) {
        let client = crate::utils::client_rect();
        let rect = CRect {
            top: 0.0,
            left: 0.0,
            right: client[0] as f32,
            bottom: client[1] as f32,
        };

        let color = RwRGBA {
            red: 0xFF,
            green: 0xFF,
            blue: 0xFF,
            alpha: 0xFF,
        };

        let prev = rw::render_state(rwplcore::RENDERSTATETEXTUREFILTER);

        rw::set_render_state(rwplcore::RENDERSTATETEXTUREFILTER, rwplcore::FILTERNEAREST);
        self.sprite.draw(rect, color);

        rw::set_render_state(rwplcore::RENDERSTATETEXTUREFILTER, prev);
    }

    #[inline]
    pub fn bytes(&mut self) -> Option<RwLockGuard<'_>> {
        self.rw.bytes()
    }
}

/// Текстура браузера в игре: спрайт на весь экран, пиксели — из общей памяти хоста.
pub struct View {
    container: Option<SpriteContainer>,
    width: usize,
    height: usize,
}

impl View {
    pub fn new() -> View {
        View {
            container: None,
            width: 0,
            height: 0,
        }
    }

    pub fn make_display(&mut self, width: usize, height: usize) {
        let width = std::cmp::max(1, width);
        let height = std::cmp::max(1, height);

        self.container = None;
        self.container = Some(SpriteContainer::new(width, height));
        self.width = width;
        self.height = height;
    }

    /// Текстуры больше нет (RenderWare выключается) — до следующего make_display.
    pub fn make_inactive(&mut self) {
        self.container = None;
    }

    pub fn is_active(&self) -> bool {
        self.container.is_some()
    }

    pub fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    #[inline]
    pub fn draw(&mut self) {
        if let Some(sprite) = self.container.as_mut() {
            sprite.draw();
        }
    }

    /// Изменившаяся область кадра из общей памяти (строка источника — `width * 4`).
    pub fn update_from(&mut self, source: &[u8], rect: Rect) {
        let width = self.width as i32;
        let height = self.height as i32;
        let left = rect.x.clamp(0, width);
        let top = rect.y.clamp(0, height);
        let right = (rect.x + rect.width).clamp(0, width);
        let bottom = (rect.y + rect.height).clamp(0, height);
        if left >= right || top >= bottom || source.len() < self.width * self.height * 4 {
            return;
        }

        let Some(mut dest) = self.container.as_mut().and_then(|sprite| sprite.bytes()) else {
            return;
        };
        let pitch = dest.pitch;
        let source_stride = self.width * 4;
        let bytes = (right - left) as usize * 4;
        let dest = dest.bytes_as_mut_ptr();

        for y in top as usize..bottom as usize {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    source.as_ptr().add(y * source_stride + left as usize * 4),
                    dest.add(y * pitch + left as usize * 4),
                    bytes,
                );
            }
        }
    }

    /// Пустой кадр — чтобы после показа не мелькнуло старое.
    pub fn clear(&mut self) {
        if let Some(mut dest) = self.container.as_mut().and_then(|sprite| sprite.bytes()) {
            dest.fill(0);
        }
    }
}

impl Default for View {
    fn default() -> Self {
        Self::new()
    }
}
