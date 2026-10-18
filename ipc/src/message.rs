use serde::{Deserialize, Serialize};

/// Аргумент события: то, что умеют передавать JS, сервер и плагины.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i32),
    Double(f64),
    String(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

/// Тип клавиатурного события — как `cef_key_event_type_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyEventKind {
    RawKeyDown,
    KeyUp,
    Char,
}

/// Поля `cef_key_event_t`, которые заполняет игра.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEvent {
    pub kind: KeyEventKind,
    pub modifiers: u32,
    pub windows_key_code: i32,
    pub native_key_code: i32,
    pub is_system_key: bool,
}

/// Игра → хост.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToHost {
    /// Браузер на весь экран. `surface` — имя общей памяти под кадр, см. [`crate::surface`].
    CreateBrowser {
        id: u32,
        url: String,
        surface: String,
        width: u32,
        height: u32,
        hidden: bool,
    },
    /// Новый размер — и новая общая память под него.
    Resize {
        id: u32,
        surface: String,
        width: u32,
        height: u32,
    },
    DestroyBrowser {
        id: u32,
    },
    /// Скрыть (перестать рисовать, выключить звук) или показать.
    Hide {
        id: u32,
        hide: bool,
    },
    MouseMove {
        id: u32,
        x: i32,
        y: i32,
    },
    MouseClick {
        id: u32,
        x: i32,
        y: i32,
        button: MouseButton,
        down: bool,
    },
    MouseWheel {
        id: u32,
        x: i32,
        y: i32,
        delta: i32,
    },
    Key {
        id: u32,
        event: KeyEvent,
    },
    /// Событие в JS всех браузеров (`cef.on`): от сервера или плагина.
    TriggerEvent {
        name: String,
        args: Vec<Value>,
    },
    DevTools {
        id: u32,
        enabled: bool,
    },
    LoadUrl {
        id: u32,
        url: String,
    },
    /// Частота кадров браузеров — как у игры, не меньше 15.
    FrameRate(i32),
}

/// Хост → игра.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToGame {
    /// CEF запущен, можно создавать браузеры.
    Ready,
    /// Главная страница загрузилась (`on_load_end` главного фрейма).
    BrowserCreated { id: u32, status: i32 },
    /// `cef.emit(name, ...args)` из JS.
    JsEvent {
        id: u32,
        name: String,
        args: Vec<Value>,
    },
    /// `cef.set_focus(bool)` из JS.
    Focus { id: u32, focus: bool },
    /// `cef.hide(bool)` из JS.
    Hide { id: u32, hide: bool },
}
