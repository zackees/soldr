//! Host identity values stored in v1 CacheManifest files.
//!
//! Phase 2 of #228 (#231). The cleanup tool uses this identity to skip
//! manifests restored from another machine or from a prior boot.

use std::path::Path;

use crate::broker::protocol::HostIdentity;

/// Return the current host identity using the current directory as the
/// filesystem-device probe.
pub fn current() -> HostIdentity {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
    current_for_path(&cwd)
}

/// Return the current host identity, including the filesystem device id
/// for `path` when the platform exposes it.
pub fn current_for_path(path: &Path) -> HostIdentity {
    HostIdentity {
        hostname: hostname(),
        machine_id: machine_id(),
        boot_id: boot_id(),
        fs_dev_id: fs_dev_id(path),
        namespace_id: namespace_id(),
    }
}

fn hostname() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".to_string())
    }
    #[cfg(unix)]
    {
        unix_hostname()
    }
}

fn machine_id() -> String {
    #[cfg(target_os = "linux")]
    {
        read_trimmed("/etc/machine-id")
            .or_else(|| read_trimmed("/var/lib/dbus/machine-id"))
            .unwrap_or_else(|| "unknown".to_string())
    }
    #[cfg(target_os = "macos")]
    {
        // The final broker platform module will use IOPlatformUUID.
        // Phase 2 avoids spawning `ioreg` so the cleanup-only client
        // path passes the repo's spawn-path guard.
        format!("macos-{}", unix_hostname())
    }
    #[cfg(windows)]
    {
        windows_machine_guid()
            .or_else(|| std::env::var("COMPUTERNAME").ok())
            .unwrap_or_else(|| "unknown".to_string())
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        "unknown".to_string()
    }
}

fn boot_id() -> String {
    #[cfg(target_os = "linux")]
    {
        read_trimmed("/proc/sys/kernel/random/boot_id").unwrap_or_else(|| "unknown".to_string())
    }
    #[cfg(target_os = "macos")]
    {
        macos_boot_time()
    }
    #[cfg(windows)]
    {
        windows_boot_counter()
            .map(|counter| format!("windows-boot-{counter}"))
            .unwrap_or_else(windows_unavailable_boot_id)
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        "unknown".to_string()
    }
}

fn namespace_id() -> String {
    #[cfg(target_os = "linux")]
    {
        let mnt = std::fs::read_link("/proc/self/ns/mnt")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "mntns:unknown".to_string());
        let pid = std::fs::read_link("/proc/self/ns/pid")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "pidns:unknown".to_string());
        format!("{mnt}:{pid}")
    }
    #[cfg(not(target_os = "linux"))]
    {
        String::new()
    }
}

#[cfg(unix)]
fn fs_dev_id(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;

    std::fs::metadata(path).map(|m| m.dev()).unwrap_or(0)
}

#[cfg(windows)]
fn fs_dev_id(path: &Path) -> u64 {
    windows_volume_serial(path).unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(unix)]
fn unix_hostname() -> String {
    let mut buf = [0_u8; 256];
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if ok != 0 {
        return "unknown".to_string();
    }
    let nul = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..nul]).into_owned()
}

#[cfg(target_os = "macos")]
fn macos_boot_time() -> String {
    use std::ffi::CString;

    let name = CString::new("kern.boottime").expect("static sysctl name");
    let mut boot: libc::timeval = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::timeval>();
    let ok = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut boot as *mut libc::timeval).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if ok == 0 {
        format!("macos-boot-{}-{}", boot.tv_sec, boot.tv_usec)
    } else {
        "unknown".to_string()
    }
}

#[cfg(windows)]
fn windows_boot_counter() -> Option<u32> {
    select_windows_boot_counter(
        windows_registry_boot_counter(),
        windows_process_boot_counter,
    )
}

#[cfg(windows)]
fn select_windows_boot_counter(
    registry: Option<u32>,
    process: impl FnOnce() -> Option<u32>,
) -> Option<u32> {
    registry.or_else(process)
}

#[cfg(windows)]
fn windows_registry_boot_counter() -> Option<u32> {
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD,
    };

    let subkey = wide_str(
        "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Memory Management\\PrefetchParameters",
    );
    let value = wide_str("BootId");
    let mut ty = 0_u32;
    let mut counter = 0_u32;
    let mut bytes = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            &mut ty,
            (&mut counter as *mut u32).cast(),
            &mut bytes,
        )
    };
    registry_dword(status, ty, bytes, counter)
}

#[cfg(windows)]
fn registry_dword(status: u32, ty: u32, bytes: u32, value: u32) -> Option<u32> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::REG_DWORD;

    (status == ERROR_SUCCESS && ty == REG_DWORD && bytes as usize == std::mem::size_of::<u32>())
        .then_some(value)
}

/// Read a stable kernel boot counter through process telemetry when the
/// registry value is missing, inaccessible, or has an unexpected type.
#[cfg(windows)]
fn windows_process_boot_counter() -> Option<u32> {
    use windows_sys::Wdk::System::Threading::{
        NtQueryInformationProcess, ProcessTelemetryIdInformation,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    #[repr(C)]
    struct ProcessTelemetryInfo {
        header_size: u32,
        process_id: u32,
        process_start_key: u64,
        create_time: u64,
        create_interrupt_time: u64,
        create_unbiased_interrupt_time: u64,
        process_sequence_number: u64,
        session_create_time: u64,
        session_id: u32,
        boot_id: u32,
        image_checksum: u32,
        image_time_date_stamp: u32,
        user_sid_offset: u32,
        image_path_offset: u32,
        package_name_offset: u32,
        relative_app_name_offset: u32,
        command_line_offset: u32,
    }

    let boot_id_offset = std::mem::offset_of!(ProcessTelemetryInfo, boot_id);
    let boot_id_end = boot_id_offset + std::mem::size_of::<u32>();
    const MAX_PROCESS_TELEMETRY_BYTES: usize = 1024 * 1024;

    // SAFETY: GetCurrentProcess returns a pseudo-handle owned by Windows; it
    // must not and will not be closed by this process.
    let process = unsafe { GetCurrentProcess() };
    let mut needed = 0_u32;
    // SAFETY: a zero-length probe with a null output buffer asks the kernel for
    // the required allocation size. `needed` is valid writable storage.
    unsafe {
        NtQueryInformationProcess(
            process,
            ProcessTelemetryIdInformation,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
    };
    if (needed as usize) < boot_id_end || needed as usize > MAX_PROCESS_TELEMETRY_BYTES {
        return None;
    }

    // The telemetry header is followed by variable-length strings. Size the
    // buffer from the kernel's first response and retry once if it grows.
    for _ in 0..2 {
        let words = (needed as usize).div_ceil(std::mem::size_of::<u64>());
        let mut buffer = vec![0_u64; words];
        let capacity = buffer.len() * std::mem::size_of::<u64>();
        let mut returned = needed;
        // SAFETY: `buffer` is writable for `capacity` bytes and is aligned more
        // strictly than the telemetry header. The current-process pseudo-handle
        // remains valid for the lifetime of this call.
        let status = unsafe {
            NtQueryInformationProcess(
                process,
                ProcessTelemetryIdInformation,
                buffer.as_mut_ptr().cast(),
                capacity as u32,
                &mut returned,
            )
        };
        if status >= 0 && returned as usize >= boot_id_end {
            // SAFETY: the successful query reported at least `boot_id_end`
            // initialized bytes. `read_unaligned` avoids relying on the buffer's
            // alignment for the field read.
            let boot_id = unsafe {
                std::ptr::read_unaligned(
                    buffer
                        .as_ptr()
                        .cast::<u8>()
                        .add(boot_id_offset)
                        .cast::<u32>(),
                )
            };
            return Some(boot_id);
        }
        if returned as usize <= capacity || returned as usize > MAX_PROCESS_TELEMETRY_BYTES {
            return None;
        }
        needed = returned;
    }
    None
}

/// Fail closed if both OS boot-counter sources are unavailable. The token is
/// stable for this process, but deliberately differs between processes so an
/// identity probe cannot accidentally accept a daemon from an unknown boot.
#[cfg(windows)]
fn windows_unavailable_boot_id() -> String {
    use std::sync::OnceLock;
    use std::time::{SystemTime, UNIX_EPOCH};

    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN
        .get_or_init(|| {
            let created = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default();
            format!("windows-boot-unavailable-{}-{created}", std::process::id())
        })
        .clone()
}

#[cfg(windows)]
fn windows_machine_guid() -> Option<String> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, HKEY_LOCAL_MACHINE, REG_SZ, RRF_RT_REG_SZ,
    };

    let subkey = wide_str("SOFTWARE\\Microsoft\\Cryptography");
    let value = wide_str("MachineGuid");
    let mut ty = 0_u32;
    let mut buf = [0_u16; 128];
    let mut bytes = (buf.len() * std::mem::size_of::<u16>()) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            &mut ty,
            buf.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if status != ERROR_SUCCESS || ty != REG_SZ {
        return None;
    }

    let len = (bytes as usize / std::mem::size_of::<u16>()).min(buf.len());
    let nul = buf[..len].iter().position(|ch| *ch == 0).unwrap_or(len);
    let guid = String::from_utf16_lossy(&buf[..nul]).trim().to_string();
    if guid.is_empty() {
        None
    } else {
        Some(guid)
    }
}

#[cfg(windows)]
fn windows_volume_serial(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetVolumeInformationByHandleW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let probe = existing_volume_probe_path(path)?;
    let wide: Vec<u16> = probe
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }

    let mut serial = 0_u32;
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            handle,
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    unsafe {
        CloseHandle(handle);
    }
    if ok == 0 {
        None
    } else {
        Some(serial as u64)
    }
}

#[cfg(windows)]
fn existing_volume_probe_path(path: &Path) -> Option<std::path::PathBuf> {
    path.ancestors()
        .find(|candidate| !candidate.as_os_str().is_empty() && candidate.exists())
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
}

#[cfg(windows)]
fn wide_str(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_identity_has_required_strings() {
        let id = current();
        assert!(!id.hostname.is_empty());
        assert!(!id.machine_id.is_empty());
        assert!(!id.boot_id.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_identity_uses_machine_and_volume_ids() {
        let cwd = std::env::current_dir().unwrap();
        let id = current_for_path(&cwd);
        assert_ne!(id.machine_id, id.hostname);
        assert_ne!(id.fs_dev_id, 0);
    }

    #[cfg(windows)]
    #[test]
    fn windows_boot_id_is_the_stable_os_boot_counter() {
        use windows_sys::Win32::Foundation::ERROR_SUCCESS;
        use windows_sys::Win32::System::Registry::{
            RegGetValueW, HKEY_LOCAL_MACHINE, REG_DWORD, RRF_RT_REG_DWORD,
        };

        let subkey = wide_str(
            "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Memory Management\\PrefetchParameters",
        );
        let value = wide_str("BootId");
        let mut ty = 0_u32;
        let mut counter = 0_u32;
        let mut bytes = std::mem::size_of::<u32>() as u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                subkey.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_DWORD,
                &mut ty,
                (&mut counter as *mut u32).cast(),
                &mut bytes,
            )
        };
        assert_eq!(status, ERROR_SUCCESS, "Windows must expose its BootId");
        assert_eq!(ty, REG_DWORD);
        assert_eq!(bytes as usize, std::mem::size_of::<u32>());

        let expected = format!("windows-boot-{counter}");
        for _ in 0..1_000 {
            assert_eq!(boot_id(), expected);
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_process_telemetry_boot_counter_is_stable() {
        let expected = windows_process_boot_counter().expect("Windows process telemetry BootId");
        assert_ne!(expected, 0);
        for _ in 0..1_000 {
            assert_eq!(windows_process_boot_counter(), Some(expected));
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_boot_counter_falls_back_for_missing_or_wrong_registry_value() {
        use windows_sys::Win32::Foundation::ERROR_SUCCESS;
        use windows_sys::Win32::System::Registry::REG_SZ;

        let process_counter = Some(42);
        assert_eq!(
            select_windows_boot_counter(registry_dword(2, 0, 0, 0), || process_counter),
            process_counter
        );
        assert_eq!(
            select_windows_boot_counter(
                registry_dword(ERROR_SUCCESS, REG_SZ, std::mem::size_of::<u32>() as u32, 7),
                || process_counter
            ),
            process_counter
        );
    }

    #[cfg(windows)]
    #[test]
    fn unavailable_windows_boot_id_is_stable_and_fail_closed() {
        let first = windows_unavailable_boot_id();
        assert_eq!(windows_unavailable_boot_id(), first);
        assert!(first.starts_with(&format!("windows-boot-unavailable-{}-", std::process::id())));
        assert_ne!(first, "unknown");
    }
}
