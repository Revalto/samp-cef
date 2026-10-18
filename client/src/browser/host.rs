//! Запуск cef_host.exe и связь с ним.

use std::os::windows::ffi::OsStrExt;
use std::ptr::null_mut;
use std::sync::Arc;

use crossbeam_channel::Sender;

use winapi::shared::minwindef::{FALSE, TRUE};
use winapi::um::processthreadsapi::{CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW};
use winapi::um::winbase::CREATE_NO_WINDOW;

use ipc::channel::{Handle, Writer, create_pipe};
use ipc::{ToGame, ToHost};

use crate::app::Event;

pub struct HostProcess {
    writer: Arc<Writer>,
    _process: Handle,
}

impl HostProcess {
    /// false — хост завершился (канал закрыт).
    pub fn send(&self, message: &ToHost) -> bool {
        self.writer.send(message)
    }
}

/// Запускает cef/cef_host.exe. Сообщения хоста приходят в `event_tx` как `Event::Host`,
/// обрыв связи — `Event::HostLost`.
pub fn spawn(event_tx: Sender<Event>) -> Option<HostProcess> {
    let exe = crate::utils::cef_dir().join("cef_host.exe");

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
    let dir: Vec<u16> = crate::utils::cef_dir().as_os_str().encode_wide().chain(Some(0)).collect();

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
            CREATE_NO_WINDOW,
            null_mut(),
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
            let _ = event_tx.send(Event::Host(message));
        });
        let _ = event_tx.send(Event::HostLost);
    });

    Some(HostProcess {
        writer: Arc::new(Writer::new(to_host)),
        _process: process,
    })
}
