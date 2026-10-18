//! Свой `cef_list_value_t` для плагинов (cef/plugins/*.dll).
//!
//! CEF теперь живёт в cef_host.exe, а плагины по-прежнему получают и создают списки аргументов
//! событий в виде `cef_list_value_t*` и зовут его методы напрямую. Здесь — структура с той же
//! раскладкой (CEF 89: база с подсчётом ссылок + таблица методов, все `__stdcall`) поверх
//! `Vec<ipc::Value>`. Поддержаны значения, которые бывают в событиях: null, bool, int, double,
//! string. Бинарные данные, словари и вложенные списки — пустые.

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;

use ipc::Value;

// cef_value_type_t
const VTYPE_INVALID: i32 = 0;
const VTYPE_NULL: i32 = 1;
const VTYPE_BOOL: i32 = 2;
const VTYPE_INT: i32 = 3;
const VTYPE_DOUBLE: i32 = 4;
const VTYPE_STRING: i32 = 5;

/// cef_string_utf16_t
#[repr(C)]
pub struct CefString16 {
    str_: *mut u16,
    length: usize,
    dtor: Option<unsafe extern "C" fn(*mut u16)>,
}

#[repr(C)]
struct Base {
    size: usize,
    add_ref: unsafe extern "system" fn(*mut CefList),
    release: unsafe extern "system" fn(*mut CefList) -> i32,
    has_one_ref: unsafe extern "system" fn(*mut CefList) -> i32,
    has_at_least_one_ref: unsafe extern "system" fn(*mut CefList) -> i32,
}

/// Раскладка до `values` совпадает с `cef_list_value_t`, порядок полей менять нельзя.
#[repr(C)]
pub struct CefList {
    base: Base,
    is_valid: unsafe extern "system" fn(*mut CefList) -> i32,
    is_owned: unsafe extern "system" fn(*mut CefList) -> i32,
    is_read_only: unsafe extern "system" fn(*mut CefList) -> i32,
    is_same: unsafe extern "system" fn(*mut CefList, *mut CefList) -> i32,
    is_equal: unsafe extern "system" fn(*mut CefList, *mut CefList) -> i32,
    copy: unsafe extern "system" fn(*mut CefList) -> *mut CefList,
    set_size: unsafe extern "system" fn(*mut CefList, usize) -> i32,
    get_size: unsafe extern "system" fn(*mut CefList) -> usize,
    clear: unsafe extern "system" fn(*mut CefList) -> i32,
    remove: unsafe extern "system" fn(*mut CefList, usize) -> i32,
    get_type: unsafe extern "system" fn(*mut CefList, usize) -> i32,
    get_value: unsafe extern "system" fn(*mut CefList, usize) -> *mut c_void,
    get_bool: unsafe extern "system" fn(*mut CefList, usize) -> i32,
    get_int: unsafe extern "system" fn(*mut CefList, usize) -> i32,
    get_double: unsafe extern "system" fn(*mut CefList, usize) -> f64,
    get_string: unsafe extern "system" fn(*mut CefList, usize) -> *mut CefString16,
    get_binary: unsafe extern "system" fn(*mut CefList, usize) -> *mut c_void,
    get_dictionary: unsafe extern "system" fn(*mut CefList, usize) -> *mut c_void,
    get_list: unsafe extern "system" fn(*mut CefList, usize) -> *mut CefList,
    set_value: unsafe extern "system" fn(*mut CefList, usize, *mut c_void) -> i32,
    set_null: unsafe extern "system" fn(*mut CefList, usize) -> i32,
    set_bool: unsafe extern "system" fn(*mut CefList, usize, i32) -> i32,
    set_int: unsafe extern "system" fn(*mut CefList, usize, i32) -> i32,
    set_double: unsafe extern "system" fn(*mut CefList, usize, f64) -> i32,
    set_string: unsafe extern "system" fn(*mut CefList, usize, *const CefString16) -> i32,
    set_binary: unsafe extern "system" fn(*mut CefList, usize, *mut c_void) -> i32,
    set_dictionary: unsafe extern "system" fn(*mut CefList, usize, *mut c_void) -> i32,
    set_list: unsafe extern "system" fn(*mut CefList, usize, *mut CefList) -> i32,

    // Дальше — только наше, плагины сюда не смотрят
    refs: AtomicUsize,
    values: Mutex<Vec<Value>>,
}

/// Новый список с одной ссылкой.
pub fn create(values: Vec<Value>) -> *mut CefList {
    Box::into_raw(Box::new(CefList {
        base: Base {
            size: std::mem::size_of::<CefList>(),
            add_ref,
            release,
            has_one_ref,
            has_at_least_one_ref,
        },
        is_valid,
        is_owned,
        is_read_only,
        is_same,
        is_equal,
        copy,
        set_size,
        get_size,
        clear,
        remove,
        get_type,
        get_value,
        get_bool,
        get_int,
        get_double,
        get_string,
        get_binary,
        get_dictionary,
        get_list,
        set_value,
        set_null,
        set_bool,
        set_int,
        set_double,
        set_string,
        set_binary,
        set_dictionary,
        set_list,
        refs: AtomicUsize::new(1),
        values: Mutex::new(values),
    }))
}

/// Значения списка и освобождение своей ссылки — для списков, которые плагин отдал нам.
///
/// # Safety
/// `list` — null или список из [`create`], ссылка на который принадлежит вызывающему.
pub unsafe fn take(list: *mut CefList) -> Vec<Value> {
    if list.is_null() {
        return Vec::new();
    }
    let values = unsafe { (*list).values.lock().clone() };
    unsafe {
        release(list);
    }
    values
}

/// # Safety
/// `list` — список из [`create`].
pub unsafe fn release_list(list: *mut CefList) {
    if !list.is_null() {
        unsafe {
            release(list);
        }
    }
}

unsafe fn values<'a>(list: *mut CefList) -> parking_lot::MutexGuard<'a, Vec<Value>> {
    unsafe { (*list).values.lock() }
}

fn ensure(values: &mut Vec<Value>, index: usize) {
    if values.len() <= index {
        values.resize(index + 1, Value::Null);
    }
}

unsafe fn set(list: *mut CefList, index: usize, value: Value) -> i32 {
    let mut values = unsafe { values(list) };
    ensure(&mut values, index);
    values[index] = value;
    1
}

unsafe extern "system" fn add_ref(list: *mut CefList) {
    unsafe { (*list).refs.fetch_add(1, Ordering::Relaxed) };
}

unsafe extern "system" fn release(list: *mut CefList) -> i32 {
    if unsafe { (*list).refs.fetch_sub(1, Ordering::Release) } != 1 {
        return 0;
    }
    std::sync::atomic::fence(Ordering::Acquire);
    drop(unsafe { Box::from_raw(list) });
    1
}

unsafe extern "system" fn has_one_ref(list: *mut CefList) -> i32 {
    (unsafe { (*list).refs.load(Ordering::Relaxed) } == 1) as i32
}

unsafe extern "system" fn has_at_least_one_ref(list: *mut CefList) -> i32 {
    (unsafe { (*list).refs.load(Ordering::Relaxed) } >= 1) as i32
}

unsafe extern "system" fn is_valid(_: *mut CefList) -> i32 {
    1
}

unsafe extern "system" fn is_owned(_: *mut CefList) -> i32 {
    0
}

unsafe extern "system" fn is_read_only(_: *mut CefList) -> i32 {
    0
}

unsafe extern "system" fn is_same(list: *mut CefList, other: *mut CefList) -> i32 {
    (list == other) as i32
}

unsafe extern "system" fn is_equal(list: *mut CefList, other: *mut CefList) -> i32 {
    if other.is_null() {
        return 0;
    }
    if list == other {
        return 1;
    }
    let a = unsafe { values(list) }.clone();
    let b = unsafe { values(other) }.clone();
    (a == b) as i32
}

unsafe extern "system" fn copy(list: *mut CefList) -> *mut CefList {
    create(unsafe { values(list) }.clone())
}

unsafe extern "system" fn set_size(list: *mut CefList, size: usize) -> i32 {
    unsafe { values(list) }.resize(size, Value::Null);
    1
}

unsafe extern "system" fn get_size(list: *mut CefList) -> usize {
    unsafe { values(list) }.len()
}

unsafe extern "system" fn clear(list: *mut CefList) -> i32 {
    unsafe { values(list) }.clear();
    1
}

unsafe extern "system" fn remove(list: *mut CefList, index: usize) -> i32 {
    let mut values = unsafe { values(list) };
    if index < values.len() {
        values.remove(index);
        1
    } else {
        0
    }
}

unsafe extern "system" fn get_type(list: *mut CefList, index: usize) -> i32 {
    match unsafe { values(list) }.get(index) {
        None => VTYPE_INVALID,
        Some(Value::Null) => VTYPE_NULL,
        Some(Value::Bool(_)) => VTYPE_BOOL,
        Some(Value::Int(_)) => VTYPE_INT,
        Some(Value::Double(_)) => VTYPE_DOUBLE,
        Some(Value::String(_)) => VTYPE_STRING,
    }
}

unsafe extern "system" fn get_value(_: *mut CefList, _: usize) -> *mut c_void {
    std::ptr::null_mut()
}

unsafe extern "system" fn get_bool(list: *mut CefList, index: usize) -> i32 {
    match unsafe { values(list) }.get(index) {
        Some(Value::Bool(v)) => *v as i32,
        Some(Value::Int(v)) => (*v != 0) as i32,
        _ => 0,
    }
}

unsafe extern "system" fn get_int(list: *mut CefList, index: usize) -> i32 {
    match unsafe { values(list) }.get(index) {
        Some(Value::Int(v)) => *v,
        Some(Value::Bool(v)) => *v as i32,
        Some(Value::Double(v)) => *v as i32,
        _ => 0,
    }
}

unsafe extern "system" fn get_double(list: *mut CefList, index: usize) -> f64 {
    match unsafe { values(list) }.get(index) {
        Some(Value::Double(v)) => *v,
        Some(Value::Int(v)) => *v as f64,
        _ => 0.0,
    }
}

unsafe extern "C" fn free_string_buffer(buffer: *mut u16) {
    // Длину не знаем — буфер выделен как Box<[u16]> с нулём в конце
    if buffer.is_null() {
        return;
    }
    let mut len = 0;
    while unsafe { *buffer.add(len) } != 0 {
        len += 1;
    }
    drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(buffer, len + 1)) });
}

/// cef_string_userfree_t. Освобождать его плагину нечем (это функция libcef), поэтому
/// структура небольшая утечка; строка — через свой dtor.
unsafe extern "system" fn get_string(list: *mut CefList, index: usize) -> *mut CefString16 {
    let text = match unsafe { values(list) }.get(index) {
        Some(Value::String(s)) => s.clone(),
        _ => return std::ptr::null_mut(),
    };
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    let length = wide.len();
    wide.push(0);
    let buffer = Box::into_raw(wide.into_boxed_slice()) as *mut u16;
    Box::into_raw(Box::new(CefString16 {
        str_: buffer,
        length,
        dtor: Some(free_string_buffer),
    }))
}

unsafe extern "system" fn get_binary(_: *mut CefList, _: usize) -> *mut c_void {
    std::ptr::null_mut()
}

unsafe extern "system" fn get_dictionary(_: *mut CefList, _: usize) -> *mut c_void {
    std::ptr::null_mut()
}

unsafe extern "system" fn get_list(_: *mut CefList, _: usize) -> *mut CefList {
    std::ptr::null_mut()
}

unsafe extern "system" fn set_value(list: *mut CefList, index: usize, _: *mut c_void) -> i32 {
    unsafe { set(list, index, Value::Null) }
}

unsafe extern "system" fn set_null(list: *mut CefList, index: usize) -> i32 {
    unsafe { set(list, index, Value::Null) }
}

unsafe extern "system" fn set_bool(list: *mut CefList, index: usize, value: i32) -> i32 {
    unsafe { set(list, index, Value::Bool(value != 0)) }
}

unsafe extern "system" fn set_int(list: *mut CefList, index: usize, value: i32) -> i32 {
    unsafe { set(list, index, Value::Int(value)) }
}

unsafe extern "system" fn set_double(list: *mut CefList, index: usize, value: f64) -> i32 {
    unsafe { set(list, index, Value::Double(value)) }
}

unsafe extern "system" fn set_string(list: *mut CefList, index: usize, value: *const CefString16) -> i32 {
    let text = if value.is_null() || unsafe { (*value).str_.is_null() } {
        String::new()
    } else {
        let wide = unsafe { std::slice::from_raw_parts((*value).str_, (*value).length) };
        String::from_utf16_lossy(wide)
    };
    unsafe { set(list, index, Value::String(text)) }
}

unsafe extern "system" fn set_binary(list: *mut CefList, index: usize, _: *mut c_void) -> i32 {
    unsafe { set(list, index, Value::Null) }
}

unsafe extern "system" fn set_dictionary(list: *mut CefList, index: usize, _: *mut c_void) -> i32 {
    unsafe { set(list, index, Value::Null) }
}

unsafe extern "system" fn set_list(list: *mut CefList, index: usize, other: *mut CefList) -> i32 {
    // Вложенные списки в событиях не ходят; список-аргумент по правилам CEF забираем
    unsafe {
        release_list(other);
        set(list, index, Value::Null)
    }
}
