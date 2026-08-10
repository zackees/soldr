use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;

#[derive(serde::Deserialize)]
struct Sidecar {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<String>,
    env: Option<HashMap<String, String>>,
}

fn sidecar_path(exe: &Path) -> PathBuf {
    // Replace extension with `.daemon.json`.
    // On Windows: foo.exe -> foo.daemon.json
    // On Unix:    foo     -> foo.daemon.json
    let stem = exe
        .file_stem()
        .expect("daemon-trampoline: cannot determine exe file stem");
    exe.with_file_name(format!("{}.daemon.json", stem.to_string_lossy()))
}

#[allow(clippy::needless_return)]
fn set_process_name(exe: &Path) {
    let stem = exe
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if stem.is_empty() {
        return;
    }

    #[cfg(target_os = "linux")]
    {
        // prctl(PR_SET_NAME, ...) — name truncated to 15 chars by the kernel
        let truncated: String = stem.chars().take(15).collect();
        let c_name = std::ffi::CString::new(truncated).unwrap_or_default();
        unsafe {
            libc::prctl(libc::PR_SET_NAME, c_name.as_ptr() as libc::c_ulong, 0, 0, 0);
        }
    }

    #[cfg(target_os = "macos")]
    {
        let c_name = std::ffi::CString::new(stem).unwrap_or_default();
        unsafe {
            libc::pthread_setname_np(c_name.as_ptr());
        }
    }
}

fn run() -> i32 {
    // 1. Determine our own exe path.
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("daemon-trampoline: failed to get current exe path: {e}");
            return 1;
        }
    };

    // 2. Derive sidecar path.
    let sidecar = sidecar_path(&exe);

    // 3. Read sidecar JSON.
    let json = match fs::read_to_string(&sidecar) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "daemon-trampoline: failed to read sidecar {}: {e}",
                sidecar.display()
            );
            return 1;
        }
    };

    let cfg: Sidecar = match serde_json::from_str(&json) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "daemon-trampoline: failed to parse sidecar {}: {e}",
                sidecar.display()
            );
            return 1;
        }
    };

    // 4. Set process name (Linux/macOS only).
    set_process_name(&exe);

    // 5. Build the command.
    let mut cmd = process::Command::new(&cfg.command);
    cmd.args(&cfg.args);

    // 6. Environment: replace if specified, otherwise inherit.
    if let Some(ref env) = cfg.env {
        cmd.env_clear();
        cmd.envs(env);
    }

    // 7. Working directory.
    if let Some(ref cwd) = cfg.cwd {
        cmd.current_dir(cwd);
    }

    // 8. Inherit stdin/stdout/stderr (default behavior).

    // 8b. On Windows, prevent the child from creating a visible console window.
    //     The trampoline itself was spawned with DETACHED_PROCESS (no console).
    //     Without explicit flags, Windows auto-creates a visible console for
    //     console applications spawned by a consoleless parent.
    //     CREATE_NO_WINDOW (0x0800_0000) gives the child a hidden console.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    // 9. Spawn, wait, and exit with child's status code.
    match cmd.status() {
        Ok(status) => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                // If killed by signal, map to 128 + signal (Unix convention).
                if let Some(sig) = status.signal() {
                    return 128 + sig;
                }
            }
            status.code().unwrap_or(1)
        }
        Err(e) => {
            eprintln!("daemon-trampoline: failed to spawn '{}': {e}", cfg.command);
            1
        }
    }
}

fn main() {
    process::exit(run());
}
