//! Запуск cef_host.exe и связь с ним.

use std::os::windows::ffi::OsStrExt;
use std::ptr::null_mut;
use std::sync::Arc;

use crossbeam_channel::Sender;

use winapi::shared::minwindef::{FALSE, TRUE};
use winapi::um::processthreadsapi::{CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess};
use winapi::um::winbase::{CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT};

use ipc::channel::{Handle, Writer, create_pipe};
use ipc::{ToGame, ToHost};

use crate::app::Event;

pub struct HostProcess {
    writer: Arc<Writer>,
    process: Handle,
}

impl HostProcess {
    /// false — хост завершился (канал закрыт).
    pub fn send(&self, message: &ToHost) -> bool {
        self.writer.send(message)
    }

    /// Завис при запуске (переход на совместимый хост): закрыть канал мало — CEF может не
    /// дойти до чтения.
    pub fn kill(&self) {
        unsafe {
            TerminateProcess(self.process.raw(), 1);
        }
    }
}

/// Окружение игры для хоста — без `__COMPAT_LAYER`: через неё режим совместимости gta_sa.exe
/// (Windows ставит ей, например, DWM8And16BitMitigation) переходит на дочерние процессы, и под
/// этой прослойкой CEF 150 не доходит до готовности. Блок UTF-16: `ИМЯ=значение\0...\0\0`.
fn host_environment() -> Vec<u16> {
    let mut block = Vec::new();
    for (name, value) in std::env::vars_os() {
        if name.to_string_lossy().eq_ignore_ascii_case("__COMPAT_LAYER") {
            continue;
        }
        block.extend(name.encode_wide());
        block.push('=' as u16);
        block.extend(value.encode_wide());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

/// Запускает cef_host.exe из `dir` (engine.rs: cef-modern\ или cef\). Сообщения хоста приходят в
/// `event_tx` как `Event::Host`, обрыв связи — `Event::HostLost`; `generation` — номер запуска:
/// события от прежнего (убитого) хоста менеджер отбрасывает.
pub fn spawn(event_tx: Sender<Event>, dir: &std::path::Path, generation: u32) -> Option<HostProcess> {
    let exe = dir.join("cef_host.exe");

    // Игра → хост: хост наследует конец для чтения. Хост → игра: конец для записи.
    let (child_in, to_host) = create_pipe(true)?;
    let (from_host, child_out) = create_pipe(false)?;

    let command = format!(
        "\"{}\" --in {} --out {}",
        exe.display(),
        child_in.as_arg(),
        child_out.as_arg()
    );
    let mut command: Vec<u16> = std::ffi::OsStr::new(&command).encode_wide().chain(Some(0)).collect();
    let exe_wide: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let dir: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();

    let mut environment = host_environment();
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    let ok = unsafe {
        CreateProcessW(
            exe_wide.as_ptr(),
            command.as_mut_ptr(),
            null_mut(),
            null_mut(),
            TRUE,
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            environment.as_mut_ptr() as *mut _,
            dir.as_ptr(),
            &mut startup,
            &mut info,
        )
    };
    if ok == FALSE {
        log::error!(
            "не запустился {}: ошибка Windows {}",
            exe.display(),
            std::io::Error::last_os_error()
        );
        return None;
    }
    drop(Handle(info.hThread));
    let process = Handle(info.hProcess);

    // Концы хоста у нас больше не нужны: иначе при его падении чтение не узнает об обрыве
    drop(child_in);
    drop(child_out);

    log::trace!("cef_host.exe запущен, pid {}", info.dwProcessId);

    std::thread::spawn(move || {
        ipc::channel::read_loop::<ToGame>(&from_host, |message| {
            let _ = event_tx.send(Event::Host(generation, message));
        });
        let _ = event_tx.send(Event::HostLost(generation));
    });

    Some(HostProcess {
        writer: Arc::new(Writer::new(to_host)),
        process,
    })
}
