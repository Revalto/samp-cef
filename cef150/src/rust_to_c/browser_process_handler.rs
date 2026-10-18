use crate::handlers::browser_process::BrowserProcessHandler;
use crate::rust_to_c::Wrapper;

use cef_sys::{cef_browser_process_handler_t, cef_command_line_t};

extern "system" fn on_context_initialized<I: BrowserProcessHandler>(
    this: *mut cef_browser_process_handler_t,
) {
    let obj: &mut Wrapper<_, I> = Wrapper::unwrap(this);
    obj.interface.on_context_initialized();
}

extern "system" fn on_before_child_process_launch<I: BrowserProcessHandler>(
    this: *mut cef_browser_process_handler_t, command_line: *mut cef_command_line_t,
) {
    if command_line.is_null() {
        return;
    }
    let obj: &mut Wrapper<_, I> = Wrapper::unwrap(this);
    let command_line = crate::command_line::CommandLine::from_raw_borrowed(command_line);
    obj.interface.on_before_child_process_launch(command_line.switch_value("type"));
}

pub fn wrap<T: BrowserProcessHandler>(handler: T) -> *mut cef_browser_process_handler_t {
    let mut object: cef_browser_process_handler_t = unsafe { std::mem::zeroed() };

    object.on_context_initialized = Some(on_context_initialized::<T>);
    object.on_before_child_process_launch = Some(on_before_child_process_launch::<T>);

    let wrapper = Wrapper::new(object, handler);

    Box::into_raw(Box::new(wrapper)) as *mut _
}
