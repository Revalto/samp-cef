//! Сообщения по анонимным каналам: `u32` длина (LE) + bincode.

use std::ptr::null_mut;
use std::sync::Mutex;

use serde::Serialize;
use serde::de::DeserializeOwned;

use winapi::shared::minwindef::{DWORD, FALSE, TRUE};
use winapi::um::fileapi::{ReadFile, WriteFile};
use winapi::um::handleapi::{CloseHandle, SetHandleInformation};
use winapi::um::minwinbase::SECURITY_ATTRIBUTES;
use winapi::um::namedpipeapi::CreatePipe;
use winapi::um::winbase::HANDLE_FLAG_INHERIT;
use winapi::um::winnt::HANDLE;

/// Больше в одном сообщении не бывает (события — килобайты); защита от мусора в канале.
const MAX_MESSAGE: usize = 64 << 20;
const PIPE_BUFFER: DWORD = 1 << 20;

/// Дескриптор Windows, который можно передавать между потоками.
#[derive(Debug)]
pub struct Handle(pub HANDLE);

unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    pub fn raw(&self) -> HANDLE {
        self.0
    }

    /// Число для командной строки дочернего процесса.
    pub fn as_arg(&self) -> String {
        (self.0 as usize).to_string()
    }

    pub fn from_arg(arg: &str) -> Option<Handle> {
        arg.parse::<usize>().ok().filter(|&h| h != 0).map(|h| Handle(h as HANDLE))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// Анонимный канал, у которого наследуется только конец `inherit_read` (для чтения) или
/// конец для записи. Возвращает (чтение, запись).
pub fn create_pipe(inherit_read: bool) -> Option<(Handle, Handle)> {
    let mut sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as DWORD,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: TRUE,
    };
    let mut read = null_mut();
    let mut write = null_mut();

    unsafe {
        if CreatePipe(&mut read, &mut write, &mut sa, PIPE_BUFFER) == FALSE {
            return None;
        }
        // Свой конец не наследуется — иначе дочерний процесс держал бы канал открытым и чтение
        // не узнавало бы о его закрытии.
        let own = if inherit_read { write } else { read };
        SetHandleInformation(own, HANDLE_FLAG_INHERIT, 0);
    }

    Some((Handle(read), Handle(write)))
}

/// Отправка сообщений. Можно звать из любых потоков.
pub struct Writer {
    handle: Mutex<Handle>,
}

impl Writer {
    pub fn new(handle: Handle) -> Writer {
        Writer {
            handle: Mutex::new(handle),
        }
    }

    /// false — канал закрыт (другая сторона завершилась).
    pub fn send<T: Serialize>(&self, message: &T) -> bool {
        let Ok(body) = bincode::serialize(message) else {
            return false;
        };
        let mut frame = Vec::with_capacity(body.len() + 4);
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(&body);

        let handle = self.handle.lock().unwrap_or_else(|e| e.into_inner());
        write_all(handle.raw(), &frame)
    }
}

fn write_all(handle: HANDLE, mut bytes: &[u8]) -> bool {
    while !bytes.is_empty() {
        let mut written: DWORD = 0;
        let ok = unsafe {
            WriteFile(
                handle,
                bytes.as_ptr() as *const _,
                bytes.len() as DWORD,
                &mut written,
                null_mut(),
            )
        };
        if ok == FALSE || written == 0 {
            return false;
        }
        bytes = &bytes[written as usize..];
    }
    true
}

fn read_exact(handle: HANDLE, mut buffer: &mut [u8]) -> bool {
    while !buffer.is_empty() {
        let mut read: DWORD = 0;
        let ok = unsafe {
            ReadFile(
                handle,
                buffer.as_mut_ptr() as *mut _,
                buffer.len() as DWORD,
                &mut read,
                null_mut(),
            )
        };
        if ok == FALSE || read == 0 {
            return false;
        }
        buffer = &mut buffer[read as usize..];
    }
    true
}

/// Читает сообщения, пока канал открыт, и отдаёт каждое в `on_message`. Блокирует поток.
/// Нераспознанное сообщение пропускается (другая версия хоста), обрыв — выход.
pub fn read_loop<T: DeserializeOwned>(handle: &Handle, mut on_message: impl FnMut(T)) {
    let mut length = [0u8; 4];
    let mut body = Vec::new();

    loop {
        if !read_exact(handle.raw(), &mut length) {
            return;
        }
        let size = u32::from_le_bytes(length) as usize;
        if size > MAX_MESSAGE {
            return;
        }
        body.resize(size, 0);
        if !read_exact(handle.raw(), &mut body) {
            return;
        }
        if let Ok(message) = bincode::deserialize::<T>(&body) {
            on_message(message);
        }
    }
}
