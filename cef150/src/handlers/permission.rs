use crate::browser::Browser;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionRequestResult {
    Accept,
    Deny,
    Dismiss,
    Ignore,
}

impl From<PermissionRequestResult> for cef_sys::cef_permission_request_result_t::Type {
    fn from(result: PermissionRequestResult) -> Self {
        match result {
            PermissionRequestResult::Accept => {
                cef_sys::cef_permission_request_result_t::CEF_PERMISSION_RESULT_ACCEPT
            }
            PermissionRequestResult::Deny => {
                cef_sys::cef_permission_request_result_t::CEF_PERMISSION_RESULT_DENY
            }
            PermissionRequestResult::Dismiss => {
                cef_sys::cef_permission_request_result_t::CEF_PERMISSION_RESULT_DISMISS
            }
            PermissionRequestResult::Ignore => {
                cef_sys::cef_permission_request_result_t::CEF_PERMISSION_RESULT_IGNORE
            }
        }
    }
}

pub trait PermissionHandler {
    fn on_show_permission_prompt(
        &self, _browser: Browser, _prompt_id: u64, _requesting_origin: String,
        _requested_permissions: u32,
    ) -> Option<PermissionRequestResult> {
        None
    }

    fn on_dismiss_permission_prompt(
        &self, _browser: Browser, _prompt_id: u64,
        _result: cef_sys::cef_permission_request_result_t::Type,
    ) {
    }
}
