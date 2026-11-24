//! Graceful Windows interrupt & termination signal handling.
//!
//! Uses Win32 `SetConsoleCtrlHandler` directly (zero external crates) to intercept:
//! - `CTRL_C_EVENT` (`Ctrl+C` / `SIGINT`)
//! - `CTRL_BREAK_EVENT` (`Ctrl+Break`)
//! - `CTRL_CLOSE_EVENT` (console window close)
//! - `CTRL_LOGOFF_EVENT` (user logoff)
//! - `CTRL_SHUTDOWN_EVENT` (system shutdown)
//!
//! On close/logoff/shutdown events, the handler waits briefly for `main()` to finish
//! flushing output and releasing resources (future exclusive DAC pins) before returning.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

const CTRL_C_EVENT: u32 = 0;
const CTRL_BREAK_EVENT: u32 = 1;
const CTRL_CLOSE_EVENT: u32 = 2;
const CTRL_LOGOFF_EVENT: u32 = 5;
const CTRL_SHUTDOWN_EVENT: u32 = 6;

/// Recognized graceful termination signals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownSignal {
    CtrlC,
    CtrlBreak,
    ConsoleClose,
    Logoff,
    Shutdown,
    Other(u32),
}

impl ShutdownSignal {
    pub fn from_win32_ctrl_type(ctrl_type: u32) -> Self {
        match ctrl_type {
            CTRL_C_EVENT => Self::CtrlC,
            CTRL_BREAK_EVENT => Self::CtrlBreak,
            CTRL_CLOSE_EVENT => Self::ConsoleClose,
            CTRL_LOGOFF_EVENT => Self::Logoff,
            CTRL_SHUTDOWN_EVENT => Self::Shutdown,
            other => Self::Other(other),
        }
    }

    /// True if Windows will terminate the process as soon as the control handler returns.
    pub fn requires_handler_wait(self) -> bool {
        matches!(self, Self::ConsoleClose | Self::Logoff | Self::Shutdown)
    }
}

impl fmt::Display for ShutdownSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CtrlC => write!(f, "CTRL_C_EVENT"),
            Self::CtrlBreak => write!(f, "CTRL_BREAK_EVENT"),
            Self::ConsoleClose => write!(f, "CTRL_CLOSE_EVENT"),
            Self::Logoff => write!(f, "CTRL_LOGOFF_EVENT"),
            Self::Shutdown => write!(f, "CTRL_SHUTDOWN_EVENT"),
            Self::Other(code) => write!(f, "CTRL_EVENT({code})"),
        }
    }
}

use ap_control::state::SharedControl;

/// Thread-safe shutdown coordinator that parks the main thread at 0% CPU until signaled,
/// and propagates interrupts immediately to the shared control hub & active audio sink.
#[derive(Default)]
pub struct ShutdownController {
    signal: Mutex<Option<ShutdownSignal>>,
    signal_cv: Condvar,
    fast_teardown: AtomicBool,
    cleanup_done: Mutex<bool>,
    cleanup_cv: Condvar,
    active_stop_flag: Mutex<Option<Arc<AtomicBool>>>,
    shared_control: Mutex<Option<Arc<SharedControl>>>,
}

impl ShutdownController {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach an audio sink's `stop_handle()` so any interrupt signal triggers an immediate
    /// click-free fade-out stop without waiting for the current track to finish.
    pub fn attach_stop_flag(&self, flag: Arc<AtomicBool>) {
        let already_signaled = self
            .signal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        if already_signaled {
            flag.store(true, Ordering::SeqCst);
        }
        *self
            .active_stop_flag
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(flag);
    }

    /// Attach the shared HTTP control hub so interrupts also wake a paused engine immediately.
    pub fn attach_shared_control(&self, shared: Arc<SharedControl>) {
        let already_signaled = self
            .signal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some();
        if already_signaled {
            shared.request_shutdown();
        }
        *self
            .shared_control
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(shared);
    }

    /// Check non-blockingly whether a shutdown signal has already been received.
    pub fn current_signal(&self) -> Option<ShutdownSignal> {
        *self.signal.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// True if any received signal (even a subsequent `CTRL_CLOSE_EVENT` after an earlier
    /// `CTRL_C_EVENT`) requires fast bounded teardown before OS termination.
    pub fn fast_teardown_requested(&self) -> bool {
        self.fast_teardown.load(Ordering::SeqCst)
    }

    /// Record the first shutdown signal, trip any attached sink stop flag, and wake waiters.
    pub fn trigger(&self, sig: ShutdownSignal) {
        if sig.requires_handler_wait() {
            self.fast_teardown.store(true, Ordering::SeqCst);
        }
        let mut guard = self.signal.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            *guard = Some(sig);
            self.signal_cv.notify_all();
        }
        if let Some(flag) = self
            .active_stop_flag
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            flag.store(true, Ordering::SeqCst);
        }
        if let Some(shared) = self
            .shared_control
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            shared.request_shutdown();
        }
    }

    /// Block with zero busy-wait until a shutdown signal arrives.
    pub fn wait_for_shutdown(&self) -> ShutdownSignal {
        let mut guard = self.signal.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(sig) = *guard {
                return sig;
            }
            guard = self.signal_cv.wait(guard).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Notify waiting OS shutdown handlers that graceful teardown has completed.
    pub fn mark_cleanup_complete(&self) {
        let mut done = self.cleanup_done.lock().unwrap_or_else(|e| e.into_inner());
        *done = true;
        self.cleanup_cv.notify_all();
    }

    /// Wait up to `timeout` for `main()` to complete teardown.
    pub fn wait_for_cleanup_complete(&self, timeout: Duration) -> bool {
        let done = self.cleanup_done.lock().unwrap_or_else(|e| e.into_inner());
        let (done, _) = self
            .cleanup_cv
            .wait_timeout_while(done, timeout, |completed| !*completed)
            .unwrap_or_else(|e| e.into_inner());
        *done
    }
}

static GLOBAL_CONTROLLER: OnceLock<ShutdownController> = OnceLock::new();

pub fn global_controller() -> &'static ShutdownController {
    GLOBAL_CONTROLLER.get_or_init(ShutdownController::new)
}

#[cfg(windows)]
unsafe extern "system" {
    fn SetConsoleCtrlHandler(
        handler_routine: Option<unsafe extern "system" fn(u32) -> i32>,
        add: i32,
    ) -> i32;
}

#[cfg(windows)]
unsafe extern "system" fn win32_ctrl_handler(ctrl_type: u32) -> i32 {
    let sig = ShutdownSignal::from_win32_ctrl_type(ctrl_type);
    let controller = global_controller();
    controller.trigger(sig);

    // For console close / logoff / shutdown, keep the handler alive briefly so main()
    // can flush logs and cleanly release hardware resources before OS termination.
    if sig.requires_handler_wait() {
        let _ = controller.wait_for_cleanup_complete(Duration::from_secs(3));
    }
    1 // TRUE: signal handled gracefully
}

/// Install Windows console control handlers covering Ctrl+C, Ctrl+Break, Close, Logoff, Shutdown.
#[cfg(windows)]
pub fn install_signal_handlers() -> Result<&'static ShutdownController, String> {
    let controller = global_controller();
    let ok = unsafe { SetConsoleCtrlHandler(Some(win32_ctrl_handler), 1) };
    if ok == 0 {
        return Err("Failed to register Win32 SetConsoleCtrlHandler".to_string());
    }
    Ok(controller)
}

#[cfg(not(windows))]
pub fn install_signal_handlers() -> Result<&'static ShutdownController, String> {
    Ok(global_controller())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn maps_all_standard_windows_ctrl_events() {
        assert_eq!(ShutdownSignal::from_win32_ctrl_type(0), ShutdownSignal::CtrlC);
        assert_eq!(ShutdownSignal::from_win32_ctrl_type(1), ShutdownSignal::CtrlBreak);
        assert_eq!(ShutdownSignal::from_win32_ctrl_type(2), ShutdownSignal::ConsoleClose);
        assert_eq!(ShutdownSignal::from_win32_ctrl_type(5), ShutdownSignal::Logoff);
        assert_eq!(ShutdownSignal::from_win32_ctrl_type(6), ShutdownSignal::Shutdown);
        assert_eq!(ShutdownSignal::from_win32_ctrl_type(99), ShutdownSignal::Other(99));

        assert!(!ShutdownSignal::CtrlC.requires_handler_wait());
        assert!(!ShutdownSignal::CtrlBreak.requires_handler_wait());
        assert!(ShutdownSignal::ConsoleClose.requires_handler_wait());
        assert!(ShutdownSignal::Logoff.requires_handler_wait());
        assert!(ShutdownSignal::Shutdown.requires_handler_wait());
    }

    #[test]
    fn controller_wakes_waiter_and_preserves_first_signal() {
        let ctrl = Arc::new(ShutdownController::new());
        let c2 = Arc::clone(&ctrl);

        let handle = thread::spawn(move || c2.wait_for_shutdown());
        ctrl.trigger(ShutdownSignal::CtrlBreak);
        ctrl.trigger(ShutdownSignal::CtrlC); // second signal must not overwrite first

        assert_eq!(handle.join().unwrap(), ShutdownSignal::CtrlBreak);

        ctrl.mark_cleanup_complete();
        assert!(ctrl.wait_for_cleanup_complete(Duration::from_millis(50)));
    }
}
