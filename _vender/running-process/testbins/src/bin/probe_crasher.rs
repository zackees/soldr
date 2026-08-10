//! Real fault fixture for #636. Never invoke in-process from a test harness.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use running_process::probe::{self, Config};

static RUN_ALLOCATORS: AtomicBool = AtomicBool::new(true);

fn main() {
    let mut args = std::env::args().skip(1);
    let mut spool = None;
    let mut mode = "segv".to_string();
    let mut prior = None;
    let mut stress = false;
    let mut reselect = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--spool" => spool = args.next().map(PathBuf::from),
            "--mode" => mode = args.next().expect("--mode value"),
            "--prior" => prior = args.next().map(PathBuf::from),
            "--stress" => stress = true,
            "--reselect-metadata" => reselect = true,
            other => panic!("unknown argument: {other}"),
        }
    }
    let spool = spool.expect("--spool is required");
    std::env::set_var(probe::SPOOL_DIR_ENV, spool);

    if let Some(path) = prior {
        install_prior_handler(&path);
    }

    let guard = probe::install(
        Config::new(if reselect {
            "stale-first"
        } else {
            "crash-fixture"
        })
        .with_version("636")
        .with_instance("platform-real"),
    )
    .expect("install probe");

    let mut workers = Vec::new();
    if stress {
        for seed in 0..8usize {
            workers.push(std::thread::spawn(move || {
                let mut generation = seed;
                while RUN_ALLOCATORS.load(Ordering::Relaxed) {
                    let length = 1_024 + generation % 32_768;
                    let mut bytes = vec![0u8; length];
                    bytes[generation % length] = generation as u8;
                    std::hint::black_box(bytes);
                    generation = generation.wrapping_add(7919);
                }
            }));
        }
    } else {
        workers.push(std::thread::spawn(|| {
            while RUN_ALLOCATORS.load(Ordering::Relaxed) {
                std::hint::spin_loop();
            }
        }));
    }

    let required_threads = if stress { 8 } else { 1 };
    let deadline = Instant::now() + Duration::from_secs(10);
    while (!guard.crash_sample_ready() || guard.crash_sample_thread_count() < required_threads)
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(guard.crash_handler_armed(), "native handler was not armed");
    assert!(
        guard.crash_sample_ready(),
        "all-thread sample never became ready"
    );
    assert!(
        guard.crash_sample_thread_count() >= required_threads,
        "all-thread sample saw {} threads, expected at least {required_threads}",
        guard.crash_sample_thread_count()
    );
    println!("READY pid={}", std::process::id());
    std::io::stdout().flush().unwrap();

    let guard = if reselect {
        let replacement = probe::install(
            Config::new("crash-fixture")
                .with_version("636")
                .with_instance("platform-real"),
        )
        .expect("install replacement probe");
        drop(guard);
        replacement
    } else {
        guard
    };
    std::hint::black_box(&guard);

    match mode.as_str() {
        "segv" => {
            // SAFETY: this binary exists solely to fault in a child process.
            unsafe {
                std::ptr::read_volatile(std::ptr::dangling::<u8>());
            }
        }
        "abort" => {
            // Use the CRT signal path on Windows as well as Unix. Rust's
            // `process::abort` uses a fail-fast exception on Windows and never
            // exercises SIGABRT chaining.
            unsafe { libc::abort() }
        }
        _ => panic!("unknown mode: {mode}"),
    }

    RUN_ALLOCATORS.store(false, Ordering::Relaxed);
    for worker in workers {
        let _ = worker.join();
    }
}

#[cfg(unix)]
fn install_prior_handler(path: &std::path::Path) {
    use std::os::fd::IntoRawFd as _;

    static PRIOR_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

    extern "C" fn prior(_signal: libc::c_int) {
        let fd = PRIOR_FD.load(Ordering::Relaxed);
        if fd >= 0 {
            // SAFETY: pre-opened fd and static bytes; write/_exit are
            // async-signal-safe.
            unsafe {
                let marker = b"prior-handler-ran";
                libc::write(fd, marker.as_ptr().cast(), marker.len());
                libc::_exit(200);
            }
        }
    }

    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .expect("open prior sentinel");
    PRIOR_FD.store(file.into_raw_fd(), Ordering::Relaxed);
    // SAFETY: zeroed sigaction is initialized below before installation.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = prior as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = 0;
        libc::sigaction(libc::SIGSEGV, &action, std::ptr::null_mut());
        libc::sigaction(libc::SIGABRT, &action, std::ptr::null_mut());
    }
}

#[cfg(windows)]
fn install_prior_handler(path: &std::path::Path) {
    use std::os::windows::io::IntoRawHandle as _;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::WriteFile;
    use windows_sys::Win32::System::Diagnostics::Debug::{
        SetUnhandledExceptionFilter, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS,
    };

    static PRIOR_HANDLE: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(-1);

    unsafe extern "system" fn prior(_info: *const EXCEPTION_POINTERS) -> i32 {
        let handle = PRIOR_HANDLE.load(Ordering::Relaxed);
        if handle != -1 {
            let marker = b"prior-handler-ran";
            let mut written = 0u32;
            // SAFETY: pre-opened handle and static bytes.
            unsafe {
                WriteFile(
                    handle as HANDLE,
                    marker.as_ptr().cast(),
                    marker.len() as u32,
                    &raw mut written,
                    std::ptr::null_mut(),
                );
            }
        }
        EXCEPTION_CONTINUE_SEARCH
    }

    unsafe extern "C" fn prior_abort(_signal: i32) {
        let handle = PRIOR_HANDLE.load(Ordering::Relaxed);
        if handle != -1 {
            let marker = b"prior-handler-ran";
            let mut written = 0u32;
            unsafe {
                WriteFile(
                    handle as HANDLE,
                    marker.as_ptr().cast(),
                    marker.len() as u32,
                    &raw mut written,
                    std::ptr::null_mut(),
                );
            }
        }
    }

    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .expect("open prior sentinel");
    PRIOR_HANDLE.store(file.into_raw_handle() as isize, Ordering::Relaxed);
    // SAFETY: process-global callback has static lifetime.
    unsafe {
        SetUnhandledExceptionFilter(Some(prior));
        libc::signal(libc::SIGABRT, prior_abort as *const () as usize);
    }
}
