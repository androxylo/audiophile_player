//! Windows real-time playback hygiene helpers.
//!
//! - MMCSS `"Pro Audio"` thread elevation (`AvSetMmThreadCharacteristicsW` via `avrt.dll`).
//! - Audirvana-inspired scheduling optimization tiers (`Normal`, `High`, `Extreme`).
//! - RAII cleanup so thread characteristics are always reverted on drop.

/// Thread/process scheduling priority tier inspired by Audirvana's KS optimization levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OptimizationLevel {
    Normal,
    #[default]
    High,
    Extreme,
}

/// RAII guard for Windows MMCSS `"Pro Audio"` thread registration.
pub struct MmcssGuard {
    #[cfg(windows)]
    handle: *mut core::ffi::c_void,
    #[cfg(windows)]
    revert_fn: Option<unsafe extern "system" fn(*mut core::ffi::c_void) -> i32>,
    active: bool,
}

impl MmcssGuard {
    /// Attempt to elevate the current thread to Windows MMCSS `"Pro Audio"` scheduling.
    ///
    /// Returns a guard whose [`MmcssGuard::is_active`] reports whether elevation succeeded.
    /// Reverts automatically on drop.
    #[cfg(windows)]
    pub fn enter_pro_audio(level: OptimizationLevel) -> Self {
        use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
            THREAD_PRIORITY_TIME_CRITICAL,
        };

        // Load avrt.dll dynamically so builds work cleanly across both MSVC and GNU toolchains.
        let dll_name: Vec<u16> = "avrt.dll\0".encode_utf16().collect();
        let module = unsafe { LoadLibraryW(dll_name.as_ptr()) };
        let mut handle: *mut core::ffi::c_void = core::ptr::null_mut();
        let mut revert_fn: Option<unsafe extern "system" fn(*mut core::ffi::c_void) -> i32> = None;

        if !module.is_null() {
            type SetCharsFn =
                unsafe extern "system" fn(*const u16, *mut u32) -> *mut core::ffi::c_void;
            type RevertFn = unsafe extern "system" fn(*mut core::ffi::c_void) -> i32;

            let set_sym = unsafe { GetProcAddress(module, c"AvSetMmThreadCharacteristicsW".as_ptr().cast()) };
            let rev_sym = unsafe { GetProcAddress(module, c"AvRevertMmThreadCharacteristics".as_ptr().cast()) };

            if let (Some(set_p), Some(rev_p)) = (set_sym, rev_sym) {
                let set_fn: SetCharsFn = unsafe { core::mem::transmute(set_p) };
                let rev: RevertFn = unsafe { core::mem::transmute(rev_p) };
                let task: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
                let mut task_index: u32 = 0;
                handle = unsafe { set_fn(task.as_ptr(), &mut task_index) };
                if !handle.is_null() {
                    revert_fn = Some(rev);
                }
            }
        }

        // Apply Audirvana-style scheduling tier on top of MMCSS.
        unsafe {
            match level {
                OptimizationLevel::Normal => {}
                OptimizationLevel::High => {
                    let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
                }
                OptimizationLevel::Extreme => {
                    let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
                }
            }
        }

        Self {
            active: !handle.is_null(),
            handle,
            revert_fn,
        }
    }

    #[cfg(not(windows))]
    pub fn enter_pro_audio(_level: OptimizationLevel) -> Self {
        Self { active: false }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }
}

impl Drop for MmcssGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        if !self.handle.is_null() {
            if let Some(rev) = self.revert_fn {
                unsafe {
                    let _ = rev(self.handle);
                }
            }
            self.handle = core::ptr::null_mut();
        }
    }
}
