//! Windows real-time playback hygiene helpers.
//!
//! - MMCSS `"Pro Audio"` thread elevation (`AvSetMmThreadCharacteristicsW` via `avrt.dll`).
//! - Audirvana-inspired scheduling optimization tiers (`Normal`, `High`, `Extreme`).
//! - RAII cleanup so thread characteristics are always reverted on drop.

#[cfg(windows)]
use std::sync::OnceLock;

/// Thread/process scheduling priority tier inspired by Audirvana's KS optimization levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OptimizationLevel {
    Normal,
    #[default]
    High,
    Extreme,
}

#[cfg(windows)]
type AvSetMmThreadCharacteristicsWFn =
    unsafe extern "system" fn(*const u16, *mut u32) -> *mut core::ffi::c_void;
#[cfg(windows)]
type AvRevertMmThreadCharacteristicsFn =
    unsafe extern "system" fn(*mut core::ffi::c_void) -> i32;

#[cfg(windows)]
struct AvrtApi {
    set_chars: AvSetMmThreadCharacteristicsWFn,
    revert: AvRevertMmThreadCharacteristicsFn,
}

#[cfg(windows)]
fn avrt_api() -> Option<&'static AvrtApi> {
    static AVRT: OnceLock<Option<AvrtApi>> = OnceLock::new();
    AVRT.get_or_init(|| {
        use windows_sys::Win32::System::LibraryLoader::{
            GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
        };
        let dll_name: Vec<u16> = "avrt.dll\0".encode_utf16().collect();
        // Load once strictly from System32 (never the application working/binary directory)
        // and keep the module handle resident for the lifetime of the process.
        let module = unsafe {
            LoadLibraryExW(
                dll_name.as_ptr(),
                core::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if module.is_null() {
            return None;
        }
        let set_sym = unsafe {
            GetProcAddress(module, c"AvSetMmThreadCharacteristicsW".as_ptr().cast())
        }?;
        let rev_sym = unsafe {
            GetProcAddress(module, c"AvRevertMmThreadCharacteristics".as_ptr().cast())
        }?;
        Some(AvrtApi {
            set_chars: unsafe { core::mem::transmute::<_, AvSetMmThreadCharacteristicsWFn>(set_sym) },
            revert: unsafe { core::mem::transmute::<_, AvRevertMmThreadCharacteristicsFn>(rev_sym) },
        })
    })
    .as_ref()
}

/// RAII guard for Windows MMCSS `"Pro Audio"` thread registration.
pub struct MmcssGuard {
    #[cfg(windows)]
    handle: *mut core::ffi::c_void,
    #[cfg(windows)]
    revert_fn: Option<AvRevertMmThreadCharacteristicsFn>,
    #[cfg(windows)]
    prior_priority: Option<i32>,
    active: bool,
}

impl MmcssGuard {
    /// Attempt to elevate the current thread to Windows MMCSS `"Pro Audio"` scheduling.
    ///
    /// Returns a guard whose [`MmcssGuard::is_active`] reports whether elevation succeeded.
    /// Both MMCSS registration and any `SetThreadPriority` change are reverted automatically
    /// on drop.
    #[cfg(windows)]
    pub fn enter_pro_audio(level: OptimizationLevel) -> Self {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, GetThreadPriority, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
            THREAD_PRIORITY_TIME_CRITICAL,
        };
        // Win32 `THREAD_PRIORITY_ERROR_RETURN` (`MAXLONG` in winbase.h).
        const THREAD_PRIORITY_ERROR_RETURN: i32 = i32::MAX;

        let mut handle: *mut core::ffi::c_void = core::ptr::null_mut();
        let mut revert_fn: Option<AvRevertMmThreadCharacteristicsFn> = None;

        if let Some(api) = avrt_api() {
            let task: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
            let mut task_index: u32 = 0;
            handle = unsafe { (api.set_chars)(task.as_ptr(), &mut task_index) };
            if !handle.is_null() {
                revert_fn = Some(api.revert);
            }
        }

        // Apply Audirvana-style scheduling tier on top of MMCSS, recording the prior priority
        // so Drop restores the calling thread back to its exact original priority.
        let desired_priority = match level {
            OptimizationLevel::Normal => None,
            OptimizationLevel::High => Some(THREAD_PRIORITY_HIGHEST),
            OptimizationLevel::Extreme => Some(THREAD_PRIORITY_TIME_CRITICAL),
        };
        let mut prior_priority = None;
        if let Some(target_prio) = desired_priority {
            unsafe {
                let thread = GetCurrentThread();
                let prev = GetThreadPriority(thread);
                if SetThreadPriority(thread, target_prio) != 0
                    && prev != THREAD_PRIORITY_ERROR_RETURN as i32
                {
                    prior_priority = Some(prev);
                }
            }
        }

        Self {
            active: !handle.is_null(),
            handle,
            revert_fn,
            prior_priority,
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
        {
            if let Some(prev) = self.prior_priority.take() {
                use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority};
                unsafe {
                    let _ = SetThreadPriority(GetCurrentThread(), prev);
                }
            }
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
}

/// Best-effort RAII guard pinning an active RAM audio buffer with Win32 `VirtualLock`
/// so Windows does not page out playing audio frames mid-track.
///
/// # Windows `VirtualLock` Caveats
/// - `VirtualLock` / `VirtualUnlock` operate at OS page granularity and are **not**
///   reference-counted: dropping a guard unlocks every page touched by its slice. Callers
///   must therefore use `MemoryLockGuard` only for a single dedicated full-track allocation
///   at a time (such as `play_full_memory`), never concurrently on multiple heap slices that
///   might share a memory page.
/// - Windows caps locked pages by the process minimum working-set quota. When `VirtualLock`
///   initially fails with `ERROR_WORKING_SET_QUOTA`, [`MemoryLockGuard::lock_slice`] makes a
///   single best-effort attempt to grow the process working-set limit (up to 512 MiB) and
///   retries once; failure is always non-fatal and never interrupts playback.
pub struct MemoryLockGuard {
    #[cfg(windows)]
    ptr: *const core::ffi::c_void,
    #[cfg(windows)]
    len: usize,
    locked: bool,
}

impl MemoryLockGuard {
    /// Attempt to lock `[ptr, ptr + byte_len)` in physical RAM (best-effort; never fails playback).
    #[cfg(windows)]
    pub fn lock_slice<T>(slice: &[T]) -> Self {
        use windows_sys::Win32::Foundation::{ERROR_WORKING_SET_QUOTA, GetLastError};
        use windows_sys::Win32::System::Memory::VirtualLock;
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, GetProcessWorkingSetSize, SetProcessWorkingSetSize,
        };

        const MAX_WORKING_SET_GROWTH_BYTES: usize = 512 * 1024 * 1024;

        let byte_len = core::mem::size_of_val(slice);
        if byte_len == 0 {
            return Self {
                ptr: core::ptr::null(),
                len: 0,
                locked: false,
            };
        }
        let ptr = slice.as_ptr().cast::<core::ffi::c_void>();
        let mut ok = unsafe { VirtualLock(ptr, byte_len) } != 0;

        if !ok && byte_len <= MAX_WORKING_SET_GROWTH_BYTES {
            let err = unsafe { GetLastError() };
            if err == ERROR_WORKING_SET_QUOTA {
                let proc = unsafe { GetCurrentProcess() };
                let mut min_ws: usize = 0;
                let mut max_ws: usize = 0;
                if unsafe { GetProcessWorkingSetSize(proc, &mut min_ws, &mut max_ws) } != 0 {
                    let needed_min = min_ws.saturating_add(byte_len);
                    let needed_max = max_ws.max(needed_min.saturating_add(64 * 1024 * 1024));
                    if unsafe { SetProcessWorkingSetSize(proc, needed_min, needed_max) } != 0 {
                        ok = unsafe { VirtualLock(ptr, byte_len) } != 0;
                    }
                }
            }
        }

        Self {
            ptr,
            len: byte_len,
            locked: ok,
        }
    }

    #[cfg(not(windows))]
    pub fn lock_slice<T>(_slice: &[T]) -> Self {
        Self { locked: false }
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }
}

impl Drop for MemoryLockGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        if self.locked && !self.ptr.is_null() && self.len > 0 {
            use windows_sys::Win32::System::Memory::VirtualUnlock;
            unsafe {
                let _ = VirtualUnlock(self.ptr, self.len);
            }
            self.locked = false;
        }
    }
}
