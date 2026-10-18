//! Проверка cef_host.exe без игры: запускает хост, открывает страницу, ждёт кадр и события.
//!   host_smoke.exe <папка cef с cef_host.exe>
//! Пишет результат в консоль и кадр — в host_smoke.bmp.

use std::os::windows::ffi::OsStrExt;
use std::ptr::null_mut;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ipc::channel::{Handle, Writer, create_pipe};
use ipc::surface::Surface;
use ipc::{MouseButton, ToGame, ToHost, Value};

use winapi::um::processthreadsapi::{CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW};

const W: u32 = 640;
const H: u32 = 360;

const PAGE: &str = "data:text/html,<html><body style='margin:0;background:rgb(255,0,0)'>\
<div id='b' style='position:absolute;left:0;top:0;width:100px;height:100px;background:rgb(0,0,255)'></div>\
<script>\
cef.on('ping', function(a){ cef.emit('pong', a); });\
document.addEventListener('dblclick', function(){ cef.emit('dbl'); });\
document.addEventListener('mousemove', function(e){ if (e.buttons & 1) cef.emit('drag', e.clientX); });\
setTimeout(function(){ cef.emit('smoke', 1, 'two', 3.5); }, 300);\
var g = document.createElement('canvas').getContext('webgl'); var r = 'WebGL недоступен';\
if (g) { var x = g.getExtension('WEBGL_debug_renderer_info'); r = x ? g.getParameter(x.UNMASKED_RENDERER_WEBGL) : g.getParameter(g.RENDERER); }\
setTimeout(function(){ cef.emit('gpu', r); }, 400);\
</script></body></html>";

fn wait(rx: &mpsc::Receiver<ToGame>, what: &str, secs: u64, mut ok: impl FnMut(&ToGame) -> bool) -> bool {
    let until = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < until {
        if let Ok(m) = rx.recv_timeout(Duration::from_millis(100)) {
            println!("  <- {:?}", m);
            if ok(&m) {
                println!("OK   {}", what);
                return true;
            }
        }
    }
    println!("FAIL {} (нет за {} с)", what, secs);
    false
}

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let exe = std::path::Path::new(&dir).join("cef_host.exe");

    let (child_in, to_host) = create_pipe(true).unwrap();
    let (from_host, child_out) = create_pipe(false).unwrap();
    let cmd = format!("\"{}\" --in {} --out {}", exe.display(), child_in.as_arg(), child_out.as_arg());
    let mut cmd: Vec<u16> = std::ffi::OsStr::new(&cmd).encode_wide().chain(Some(0)).collect();
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let started = unsafe {
        CreateProcessW(null_mut(), cmd.as_mut_ptr(), null_mut(), null_mut(), 1, 0, null_mut(), null_mut(), &mut si, &mut pi)
    };
    if started == 0 {
        println!("FAIL запуск {}: {}", exe.display(), std::io::Error::last_os_error());
        std::process::exit(1);
    }
    drop(Handle(pi.hThread));
    drop(child_in);
    drop(child_out);

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        ipc::channel::read_loop::<ToGame>(&from_host, |m| {
            let _ = tx.send(m);
        });
        println!("  (канал от хоста закрыт)");
    });
    let writer = Writer::new(to_host);
    let mut pass = true;

    pass &= wait(&rx, "Ready", 30, |m| matches!(m, ToGame::Ready));

    let name = ipc::surface::name(std::process::id(), 1, 1);
    let surface = Surface::create(&name, W, H).expect("общая память");
    writer.send(&ToHost::CreateBrowser { id: 1, url: PAGE.into(), surface: name, width: W, height: H, hidden: false });
    pass &= wait(&rx, "BrowserCreated", 20, |m| matches!(m, ToGame::BrowserCreated { id: 1, .. }));
    pass &= wait(&rx, "JsEvent smoke(1,'two',3.5)", 10, |m| {
        matches!(m, ToGame::JsEvent { name, args, .. } if name == "smoke"
            && args == &vec![Value::Int(1), Value::String("two".into()), Value::Double(3.5)])
    });

    // Чем рисует Chromium: SwiftShader — процессор, иначе — видеокарта (cef_host.ini, render)
    wait(&rx, "отрисовка", 10, |m| {
        if let ToGame::JsEvent { name, args, .. } = m
            && name == "gpu"
        {
            let renderer = format!("{:?}", args);
            let kind = if renderer.contains("SwiftShader") { "ПРОЦЕССОР" } else { "ВИДЕОКАРТА" };
            println!("  отрисовка: {} — {}", kind, renderer);
            return true;
        }
        false
    });

    writer.send(&ToHost::TriggerEvent { name: "ping".into(), args: vec![Value::Int(42)] });
    pass &= wait(&rx, "игра->JS->игра: pong(42)", 10, |m| {
        matches!(m, ToGame::JsEvent { name, args, .. } if name == "pong" && args == &vec![Value::Int(42)])
    });

    // Двойной клик и перетаскивание с зажатой кнопкой
    for down in [true, false, true, false] {
        writer.send(&ToHost::MouseClick { id: 1, x: 300, y: 200, button: MouseButton::Left, down });
    }
    pass &= wait(&rx, "dblclick", 5, |m| matches!(m, ToGame::JsEvent { name, .. } if name == "dbl"));
    writer.send(&ToHost::MouseClick { id: 1, x: 300, y: 200, button: MouseButton::Left, down: true });
    writer.send(&ToHost::MouseMove { id: 1, x: 350, y: 200 });
    pass &= wait(&rx, "перетаскивание (buttons=1)", 5, |m| matches!(m, ToGame::JsEvent { name, .. } if name == "drag"));
    writer.send(&ToHost::MouseClick { id: 1, x: 350, y: 200, button: MouseButton::Left, down: false });

    // Кадр: слева сверху синий квадрат, остальное красное (BGRA)
    std::thread::sleep(Duration::from_millis(500));
    let guard = surface.lock();
    let dirty = guard.take_dirty();
    let px = guard.pixels();
    let at = |x: usize, y: usize| { let i = (y * W as usize + x) * 4; [px[i], px[i + 1], px[i + 2]] };
    let (blue, red) = (at(50, 50), at(400, 300));
    println!("  грязная область {:?}, пиксель (50,50) BGR {:?}, (400,300) BGR {:?}", dirty, blue, red);
    let frame_ok = blue[0] > 200 && blue[2] < 50 && red[2] > 200 && red[0] < 50;
    println!("{} кадр в общей памяти", if frame_ok { "OK  " } else { "FAIL" });
    pass &= frame_ok;

    let mut bmp = Vec::new();
    let size = (W * H * 4) as u32;
    bmp.extend_from_slice(b"BM");
    for v in [54 + size, 0, 54, 40, W, (-(H as i32)) as u32] { bmp.extend_from_slice(&v.to_le_bytes()); }
    bmp.extend_from_slice(&1u16.to_le_bytes());
    bmp.extend_from_slice(&32u16.to_le_bytes());
    for v in [0u32, size, 2835, 2835, 0, 0] { bmp.extend_from_slice(&v.to_le_bytes()); }
    bmp.extend_from_slice(px);
    drop(guard);
    let _ = std::fs::write("host_smoke.bmp", bmp);

    // Частота кадров интерфейса (в data:-адресе нельзя «%»: %36 раскодируется в «6» и ломает
    // скрипт — поэтому остаток от деления через Math.floor): вторая страница во весь «экран» 1920×1080 с анимацией на каждом
    // кадре (как перетаскивание карты — меняется весь кадр). Забираем кадры, как игра, 4 с.
    const AW: u32 = 1920;
    const AH: u32 = 1080;
    const ANIM: &str = "data:text/html,<html><body style='margin:0;overflow:hidden'>\
<div id='a' style='position:absolute;inset:0'></div><script>var t=0;function f(){t++;\
document.getElementById('a').style.background='linear-gradient('+(t-Math.floor(t/360)*360)+'deg,rgb(255,0,0),rgb(0,0,255))';\
requestAnimationFrame(f);}requestAnimationFrame(f);</script></body></html>";
    writer.send(&ToHost::FrameRate(240));
    let anim_name = ipc::surface::name(std::process::id(), 2, 1);
    let anim = Surface::create(&anim_name, AW, AH).expect("общая память 2");
    writer.send(&ToHost::CreateBrowser { id: 2, url: ANIM.into(), surface: anim_name, width: AW, height: AH, hidden: false });
    wait(&rx, "анимированная страница", 20, |m| matches!(m, ToGame::BrowserCreated { id: 2, .. }));
    std::thread::sleep(Duration::from_millis(1000));
    let (mut frames, mut pixels) = (0u64, 0u64);
    let measure = Instant::now();
    while measure.elapsed() < Duration::from_secs(4) {
        if let Some(guard) = anim.try_lock() {
            if let Some(rect) = guard.take_dirty() {
                frames += 1;
                pixels += (rect.width.max(0) * rect.height.max(0)) as u64;
            }
        }
        std::thread::sleep(Duration::from_micros(500));
    }
    let fps = frames as f64 / measure.elapsed().as_secs_f64();
    println!("  кадров интерфейса 1920x1080 с анимацией: {:.1} к/с (в среднем {:.0}% кадра за раз)", fps,
        if frames > 0 { 100.0 * pixels as f64 / frames as f64 / (AW * AH) as f64 } else { 0.0 });
    writer.send(&ToHost::DestroyBrowser { id: 2 });

    // Закрыли канал — хост должен выйти сам
    drop(writer);
    let process = Handle(pi.hProcess);
    let exited = unsafe { winapi::um::synchapi::WaitForSingleObject(process.raw(), 10_000) } == 0;
    println!("{} хост вышел после закрытия канала", if exited { "OK  " } else { "FAIL" });
    pass &= exited;

    println!("{}", if pass { "ИТОГ: всё прошло" } else { "ИТОГ: есть ошибки" });
    std::process::exit(if pass { 0 } else { 1 });
}
