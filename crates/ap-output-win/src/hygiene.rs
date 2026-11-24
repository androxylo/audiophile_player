//! Windows real-time playback hygiene helpers.
//!
//! - MMCSS `"Pro Audio"` thread elevation (`AvSetMmThreadCharacteristicsW` via `avrt.dll`).
//! - Audirvana-inspired scheduling optimization tiers (`Normal`, `High`, `Extreme`).
//! - RAII cleanup so thread characteristics are always reverted on drop.

#[cfg(windows)]
use std::collections::HashMap;
#[cfg(windows)]
use std::sync::{Mutex, OnceLock};

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

        // Record the thread's TRUE original priority BEFORE MMCSS elevates the thread so
        // Drop restores the real pre-playback priority rather than an MMCSS-boosted level.
        let desired_priority = match level {
            OptimizationLevel::Normal => None,
            OptimizationLevel::High => Some(THREAD_PRIORITY_HIGHEST),
            OptimizationLevel::Extreme => Some(THREAD_PRIORITY_TIME_CRITICAL),
        };
        let original_priority = unsafe {
            let prev = GetThreadPriority(GetCurrentThread());
            if prev != THREAD_PRIORITY_ERROR_RETURN {
                Some(prev)
            } else {
                None
            }
        };

        if let Some(api) = avrt_api() {
            let task: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
            let mut task_index: u32 = 0;
            handle = unsafe { (api.set_chars)(task.as_ptr(), &mut task_index) };
            if !handle.is_null() {
                revert_fn = Some(api.revert);
            }
        }

        // Apply Audirvana-style scheduling tier on top of MMCSS.
        let mut prior_priority = None;
        if let Some(target_prio) = desired_priority {
            unsafe {
                let thread = GetCurrentThread();
                if SetThreadPriority(thread, target_prio) != 0 {
                    prior_priority = original_priority;
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
            // Revert MMCSS registration FIRST, then restore the saved original thread priority
            // so neither MMCSS nor SetThreadPriority leaves the calling thread elevated.
            if !self.handle.is_null() {
                if let Some(rev) = self.revert_fn {
                    unsafe {
                        let _ = rev(self.handle);
                    }
                }
                self.handle = core::ptr::null_mut();
            }
            if let Some(prev) = self.prior_priority.take() {
                use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority};
                unsafe {
                    let _ = SetThreadPriority(GetCurrentThread(), prev);
                }
            }
        }
    }
}

#[cfg(windows)]
fn os_page_size() -> usize {
    static PAGE_SIZE: OnceLock<usize> = OnceLock::new();
    *PAGE_SIZE.get_or_init(|| {
        use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
        let mut info: SYSTEM_INFO = unsafe { core::mem::zeroed() };
        unsafe { GetSystemInfo(&mut info) };
        let ps = info.dwPageSize as usize;
        if ps.is_power_of_two() && ps >= 4096 {
            ps
        } else {
            4096
        }
    })
}

#[cfg(windows)]
#[derive(Default)]
struct MemoryLockTracker {
    /// Per-OS-page reference count so overlapping/shared-page guards never prematurely
    /// `VirtualUnlock` a page still pinned by another active guard.
    page_refcounts: HashMap<usize, usize>,
    /// Original process `(min_ws, max_ws)` recorded before our first quota growth, restored
    /// automatically when the last locked page is released.
    saved_working_set: Option<(usize, usize)>,
}

#[cfg(windows)]
fn lock_tracker() -> &'static Mutex<MemoryLockTracker> {
    static TRACKER: OnceLock<Mutex<MemoryLockTracker>> = OnceLock::new();
    TRACKER.get_or_init(|| Mutex::new(MemoryLockTracker::default()))
}

/// Best-effort RAII guard pinning an active RAM audio buffer with Win32 `VirtualLock`
/// so Windows does not page out playing audio frames mid-track.
///
/// - Tracks locked OS pages with an internal page-level reference count so concurrent or
///   adjacent slices sharing an OS page never prematurely unlock each other on drop.
/// - When `VirtualLock` hits `ERROR_WORKING_SET_QUOTA`, grows the process minimum working
///   set only by the actual shortfall (capped at 512 MiB above the original minimum) and
///   restores the original working-set bounds once the last active guard drops.
pub struct MemoryLockGuard {
    #[cfg(windows)]
    first_page: usize,
    #[cfg(windows)]
    page_count: usize,
    #[cfg(windows)]
    page_size: usize,
    locked: bool,
}

impl MemoryLockGuard {
    /// Attempt to lock `[ptr, ptr + byte_len)` in physical RAM (best-effort; never fails playback).
    #[cfg(windows)]
    pub fn lock_slice<T>(slice: &[T]) -> Self {
        use windows_sys::Win32::Foundation::{ERROR_WORKING_SET_QUOTA, GetLastError};
        use windows_sys::Win32::System::Memory::{VirtualLock, VirtualUnlock};
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, GetProcessWorkingSetSize, SetProcessWorkingSetSize,
        };

        const MAX_WORKING_SET_GROWTH_BYTES: usize = 512 * 1024 * 1024;
        const WORKING_SET_HEADROOM_BYTES: usize = 4 * 1024 * 1024;

        let byte_len = core::mem::size_of_val(slice);
        if byte_len == 0 {
            return Self {
                first_page: 0,
                page_count: 0,
                page_size: 4096,
                locked: false,
            };
        }

        let page_size = os_page_size();
        let start_addr = slice.as_ptr() as usize;
        let end_addr = start_addr.saturating_add(byte_len - 1);
        let first_page = start_addr / page_size;
        let last_page = end_addr / page_size;
        let page_count = last_page - first_page + 1;

        let mut tracker = lock_tracker().lock().unwrap_or_else(|e| e.into_inner());

        // Identify contiguous sub-runs of pages not yet locked by any active guard.
        let mut new_runs: Vec<(usize, usize)> = Vec::new();
        let mut run_start: Option<usize> = None;
        let mut new_pages: usize = 0;
        for page in first_page..=last_page {
            let already_locked = tracker.page_refcounts.get(&page).copied().unwrap_or(0) > 0;
            if !already_locked {
                new_pages += 1;
                if run_start.is_none() {
                    run_start = Some(page);
                }
            } else if let Some(start) = run_start.take() {
                new_runs.push((start, page - start));
            }
        }
        if let Some(start) = run_start {
            new_runs.push((start, last_page + 1 - start));
        }

        // Lock each newly acquired page run; on quota error grow only by the shortfall
        // relative to the saved original minimum working set and retry once.
        let mut locked_runs: Vec<(usize, usize)> = Vec::new();
        let mut all_ok = true;
        for &(run_first, run_len) in &new_runs {
            let run_ptr = (run_first * page_size) as *const core::ffi::c_void;
            let run_bytes = run_len * page_size;
            let mut ok = unsafe { VirtualLock(run_ptr, run_bytes) } != 0;

            if !ok {
                let err = unsafe { GetLastError() };
                let total_after_bytes = tracker
                    .page_refcounts
                    .len()
                    .saturating_add(new_pages)
                    .saturating_mul(page_size);
                if err == ERROR_WORKING_SET_QUOTA && total_after_bytes <= MAX_WORKING_SET_GROWTH_BYTES
                {
                    let proc = unsafe { GetCurrentProcess() };
                    let mut cur_min: usize = 0;
                    let mut cur_max: usize = 0;
                    if unsafe { GetProcessWorkingSetSize(proc, &mut cur_min, &mut cur_max) } != 0 {
                        let (orig_min, orig_max) =
                            *tracker.saved_working_set.get_or_insert((cur_min, cur_max));
                        let desired_min = orig_min
                            .saturating_add(total_after_bytes)
                            .saturating_add(WORKING_SET_HEADROOM_BYTES);
                        let capped_min =
                            desired_min.min(orig_min.saturating_add(MAX_WORKING_SET_GROWTH_BYTES));
                        if capped_min > cur_min {
                            let desired_max =
                                orig_max.max(cur_max).max(capped_min.saturating_add(64 * 1024 * 1024));
                            if unsafe { SetProcessWorkingSetSize(proc, capped_min, desired_max) }
                                != 0
                            {
                                ok = unsafe { VirtualLock(run_ptr, run_bytes) } != 0;
                            }
                        }
                    }
                }
            }

            if ok {
                locked_runs.push((run_first, run_len));
            } else {
                all_ok = false;
                break;
            }
        }

        if !all_ok {
            // Roll back any partial page runs we locked in this call.
            for (run_first, run_len) in locked_runs {
                let run_ptr = (run_first * page_size) as *const core::ffi::c_void;
                unsafe {
                    let _ = VirtualUnlock(run_ptr, run_len * page_size);
                }
            }
            if tracker.page_refcounts.is_empty() {
                restore_working_set_if_needed(&mut tracker);
            }
            return Self {
                first_page: 0,
                page_count: 0,
                page_size,
                locked: false,
            };
        }

        for page in first_page..=last_page {
            *tracker.page_refcounts.entry(page).or_insert(0) += 1;
        }

        Self {
            first_page,
            page_count,
            page_size,
            locked: true,
        }
    }

    #[cfg(not(windows))]
    pub fn lock_slice<T>(_slice: &[T]) -> Self {
        Self { locked: false }
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// Diagnostic helper for tests: total OS pages currently refcounted as locked.
    #[cfg(windows)]
    pub fn active_locked_page_count() -> usize {
        lock_tracker()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .page_refcounts
            .len()
    }
}

#[cfg(windows)]
fn restore_working_set_if_needed(tracker: &mut MemoryLockTracker) {
    if let Some((orig_min, orig_max)) = tracker.saved_working_set.take() {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
        unsafe {
            let _ = SetProcessWorkingSetSize(GetCurrentProcess(), orig_min, orig_max);
        }
    }
}

impl Drop for MemoryLockGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        if self.locked && self.page_count > 0 {
            use windows_sys::Win32::System::Memory::VirtualUnlock;
            let mut tracker = lock_tracker().lock().unwrap_or_else(|e| e.into_inner());
            let last_page = self.first_page + self.page_count - 1;
            let mut run_start: Option<usize> = None;

            for page in self.first_page..=last_page {
                let mut zeroed = false;
                if let Some(rc) = tracker.page_refcounts.get_mut(&page) {
                    *rc = rc.saturating_sub(1);
                    if *rc == 0 {
                        tracker.page_refcounts.remove(&page);
                        zeroed = true;
                    }
                }
                if zeroed {
                    if run_start.is_none() {
                        run_start = Some(page);
                    }
                } else if let Some(start) = run_start.take() {
                    let run_ptr = (start * self.page_size) as *const core::ffi::c_void;
                    unsafe {
                        let _ = VirtualUnlock(run_ptr, (page - start) * self.page_size);
                    }
                }
            }
            if let Some(start) = run_start {
                let run_ptr = (start * self.page_size) as *const core::ffi::c_void;
                unsafe {
                    let _ = VirtualUnlock(run_ptr, (last_page + 1 - start) * self.page_size);
                }
            }

            if tracker.page_refcounts.is_empty() {
                restore_working_set_if_needed(&mut tracker);
            }
            self.locked = false;
        }
    }
}
