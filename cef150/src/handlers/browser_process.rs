pub trait BrowserProcessHandler {
    fn on_context_initialized(&self) {}

    /// Перед запуском дочернего процесса CEF (renderer, gpu-process, utility — сеть и т. п.);
    /// `process_type` — значение --type.
    fn on_before_child_process_launch(&self, _process_type: String) {}
}
