use super::Wrapper;
use crate::browser::Browser;
use crate::handlers::permission::PermissionHandler;
use crate::types::string::CefString;
use cef_sys::{
    cef_browser_t, cef_permission_handler_t, cef_permission_prompt_callback_t,
    cef_permission_request_result_t, cef_string_t,
};

unsafe extern "system" fn on_show_permission_prompt<I: PermissionHandler>(
    this: *mut cef_permission_handler_t, browser: *mut cef_browser_t, prompt_id: u64,
    requesting_origin: *const cef_string_t, requested_permissions: u32,
    callback: *mut cef_permission_prompt_callback_t,
) -> i32 {
    if browser.is_null() || requesting_origin.is_null() || callback.is_null() {
        return 0;
    }

    let obj: &mut Wrapper<_, I> = Wrapper::unwrap(this);
    let browser = Browser::from_raw_add_ref(browser);
    let requesting_origin = CefString::from(requesting_origin).to_string();

    let Some(result) = obj.interface.on_show_permission_prompt(
        browser,
        prompt_id,
        requesting_origin,
        requested_permissions,
    ) else {
        return 0;
    };

    let Some(continue_prompt) = (unsafe { (*callback).cont }) else {
        return 0;
    };

    unsafe {
        continue_prompt(callback, result.into());
    }
    1
}

unsafe extern "system" fn on_dismiss_permission_prompt<I: PermissionHandler>(
    this: *mut cef_permission_handler_t, browser: *mut cef_browser_t, prompt_id: u64,
    result: cef_permission_request_result_t::Type,
) {
    if browser.is_null() {
        return;
    }

    let obj: &mut Wrapper<_, I> = Wrapper::unwrap(this);
    let browser = Browser::from_raw_add_ref(browser);
    obj.interface
        .on_dismiss_permission_prompt(browser, prompt_id, result);
}

pub fn wrap<T: PermissionHandler>(handler: T) -> *mut cef_permission_handler_t {
    let mut object: cef_permission_handler_t = unsafe { std::mem::zeroed() };
    object.on_show_permission_prompt = Some(on_show_permission_prompt::<T>);
    object.on_dismiss_permission_prompt = Some(on_dismiss_permission_prompt::<T>);

    let wrapper = Wrapper::new(object, handler);
    Box::into_raw(Box::new(wrapper)) as *mut _
}
