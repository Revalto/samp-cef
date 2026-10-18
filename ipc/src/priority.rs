//! Приоритет процессов браузера на видеокарте и процессоре.
//!
//! Игра без ограничения FPS загружает видеокарту почти на 100%. Chromium рисует интерфейс на той же
//! видеокарте и на каждом кадре ждёт её — анимации идут рывками. Планировщик видеокарты Windows
//! (WDDM) раздаёт ей время по классу приоритета процесса: хосту и GPU-процессу Chromium ставим
//! класс выше обычного, тогда интерфейс получает видеокарту первым, а игре достаётся остальное.

use std::ffi::c_void;

use winapi::um::libloaderapi::{GetModuleHandleA, GetProcAddress, LoadLibraryA};
use winapi::um::processthreadsapi::{GetCurrentProcess, SetPriorityClass};
use winapi::um::winbase::ABOVE_NORMAL_PRIORITY_CLASS;

/// В объекте задания (Job) ли процесс — для логов: из Job без права выхода дочерние процессы
/// Chromium (сеть, GPU) могут не запуститься.
pub fn job_status() -> String {
    let mut in_job = 0;
    let ok = unsafe { winapi::um::jobapi::IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_job) };
    if ok == 0 {
        "Job: не удалось узнать".into()
    } else if in_job != 0 {
        "процесс в объекте задания (Job)".into()
    } else {
        "процесс не в объекте задания (Job)".into()
    }
}

/// D3DKMT_SCHEDULINGPRIORITYCLASS
const PRIORITY_ABOVE_NORMAL: i32 = 3;
const PRIORITY_HIGH: i32 = 4;

type SetSchedulingPriorityFn = unsafe extern "system" fn(process: *mut c_void, priority: i32) -> i32;

/// Поднять приоритет текущего процесса: на видеокарте (HIGH, если Windows не даёт — ABOVE_NORMAL)
/// и, если `cpu`, на процессоре (ABOVE_NORMAL). Возвращает строку для лога.
pub fn raise_current(cpu: bool) -> String {
    let mut result = String::new();
    unsafe {
        let mut gdi = GetModuleHandleA(b"gdi32.dll\0".as_ptr() as _);
        if gdi.is_null() {
            gdi = LoadLibraryA(b"gdi32.dll\0".as_ptr() as _);
        }
        let proc_ = if gdi.is_null() {
            std::ptr::null_mut()
        } else {
            GetProcAddress(gdi, b"D3DKMTSetProcessSchedulingPriorityClass\0".as_ptr() as _)
        };
        if proc_.is_null() {
            result.push_str("видеокарта: функции нет (Windows старше 8?)");
        } else {
            let set: SetSchedulingPriorityFn = std::mem::transmute(proc_);
            let process = GetCurrentProcess() as *mut c_void;
            let status = set(process, PRIORITY_HIGH);
            if status >= 0 {
                result.push_str("видеокарта: высокий");
            } else {
                let fallback = set(process, PRIORITY_ABOVE_NORMAL);
                if fallback >= 0 {
                    result.push_str("видеокарта: выше обычного");
                } else {
                    result.push_str(&format!("видеокарта: не изменён (0x{:08X})", fallback as u32));
                }
            }
        }
        if cpu {
            let ok = SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS) != 0;
            result.push_str(if ok { ", процессор: выше обычного" } else { ", процессор: не изменён" });
        }
    }
    result
}

/// Дочерние процессы текущего (для логов): «renderer.exe 1234, renderer.exe 5678».
pub fn children_text() -> String {
    use winapi::um::tlhelp32::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
    let me = std::process::id();
    let mut list = Vec::new();
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot.is_null() || snapshot == winapi::um::handleapi::INVALID_HANDLE_VALUE {
            return "снимок процессов не получен".into();
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snapshot, &mut entry);
        while ok != 0 {
            if entry.th32ParentProcessID == me {
                let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
                list.push(format!("{} {}", String::from_utf16_lossy(&entry.szExeFile[..len]), entry.th32ProcessID));
            }
            ok = Process32NextW(snapshot, &mut entry);
        }
        winapi::um::handleapi::CloseHandle(snapshot);
    }
    if list.is_empty() { "нет".into() } else { list.join(", ") }
}
