//! Windows console popup detection using the Win32 `EnumWindows` API.
//!
//! On non-Windows platforms every function is a no-op that returns an empty
//! `Vec`.

/// Metadata about a single visible window that appeared during monitoring.
#[derive(Debug, Clone)]
pub struct ConsoleWindowInfo {
    /// Process id that owns the visible window.
    pub pid: u32,
    /// Window title captured at enumeration time.
    pub title: String,
    /// Native window handle represented as an integer for stable transport.
    pub hwnd: u64,
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod imp {
    use super::ConsoleWindowInfo;
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    use winapi::shared::minwindef::{BOOL, LPARAM, TRUE};
    use winapi::shared::windef::HWND;
    use winapi::um::winuser::{
        EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    };

    /// Enumerate all currently visible windows and return their metadata.
    fn enumerate_visible_windows() -> Vec<ConsoleWindowInfo> {
        let mut results: Vec<ConsoleWindowInfo> = Vec::new();

        unsafe extern "system" fn enum_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
            // Only consider visible windows.
            if IsWindowVisible(hwnd) == 0 {
                return TRUE; // continue enumeration
            }

            // Obtain the owning PID.
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);

            // Read the window title into a wide‑char buffer.
            let mut title_buf: [u16; 512] = [0u16; 512];
            let len = GetWindowTextW(hwnd, title_buf.as_mut_ptr(), title_buf.len() as i32);
            let title = if len > 0 {
                String::from_utf16_lossy(&title_buf[..len as usize])
            } else {
                String::new()
            };

            let results = &mut *(lparam as *mut Vec<ConsoleWindowInfo>);
            results.push(ConsoleWindowInfo {
                pid,
                title,
                hwnd: hwnd as u64,
            });

            TRUE // continue enumeration
        }

        unsafe {
            EnumWindows(
                Some(enum_callback),
                &mut results as *mut Vec<ConsoleWindowInfo> as LPARAM,
            );
        }

        results
    }

    /// Monitor for **new** console windows that appear within `duration`.
    ///
    /// 1. Takes a snapshot of all currently visible window HWNDs.
    /// 2. Polls every 50 ms for the given duration.
    /// 3. Any HWND not present in the initial snapshot is collected.
    pub fn monitor_console_windows(duration: Duration) -> Vec<ConsoleWindowInfo> {
        // Initial snapshot — these windows already existed before monitoring.
        let baseline: HashSet<u64> = enumerate_visible_windows().iter().map(|w| w.hwnd).collect();

        let mut seen_new: HashSet<u64> = HashSet::new();
        let mut new_windows: Vec<ConsoleWindowInfo> = Vec::new();

        let start = Instant::now();
        let poll_interval = Duration::from_millis(50);

        while start.elapsed() < duration {
            // #199: intentional — Win32 exposes no "new console window"
            // event; enumerating top-level windows is the only way to
            // detect new ones. 50ms poll trades responsiveness for CPU.
            std::thread::sleep(poll_interval);

            for info in enumerate_visible_windows() {
                if !baseline.contains(&info.hwnd) && seen_new.insert(info.hwnd) {
                    new_windows.push(info);
                }
            }
        }

        new_windows
    }
}

#[cfg(windows)]
pub use imp::monitor_console_windows;

// ---------------------------------------------------------------------------
// Non-Windows stub
// ---------------------------------------------------------------------------
#[cfg(not(windows))]
/// Monitor for new console windows during `duration`.
///
/// Non-Windows platforms do not expose Win32 console popups, so this returns an
/// empty list.
pub fn monitor_console_windows(_duration: std::time::Duration) -> Vec<ConsoleWindowInfo> {
    Vec::new()
}
