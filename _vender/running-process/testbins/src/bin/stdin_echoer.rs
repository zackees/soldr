//! Test fixture for the MITM stdin substrate tests (#448, #449).
//!
//! Reads stdin in 4 KB chunks and writes each chunk to stdout
//! immediately, flushing after every write. Useful for verifying
//! that bytes the host writes to a PTY's master input pipe transit
//! byte-exact to the child.
//!
//! Flags:
//!
//! * `--no-echo` — consume stdin but do not echo. Used to prove the
//!   host's master output pipe does NOT receive an echo of the host's
//!   own input in `PSEUDOCONSOLE_PASSTHROUGH_MODE` (test 9 of #448).
//! * `--advertise-paste` — emit `\x1b[?2004h` (bracketed-paste enable)
//!   on stdout at startup. Used to verify the child's enable sequence
//!   reaches the host via the output pipe (test 8 of #449).
//! * `--tick-ms <n>` — emit `"T\n"` on a side thread every `n`
//!   milliseconds. Used by test 6 of #448 to verify the host's
//!   input pipe can interleave a write while the child is producing
//!   continuous output.
//! * `--exit-on <hex>` — read until a byte matching the given hex
//!   value is seen, write it back, then exit cleanly. Used by tests
//!   that need deterministic teardown.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// On POSIX hosts the default PTY line discipline echoes input back
/// to the master and renders control characters in caret form
/// (`\x1b` → `^[`). Both behaviors break the byte-exact MITM
/// guarantee the #448 / #449 tests assert. Put stdin into raw mode
/// so the host master pipe sees only what we explicitly write back.
///
/// On Windows ConPTY in PASSTHROUGH_MODE handles this for us — no
/// action needed.
#[cfg(unix)]
fn enter_raw_mode() {
    use libc::{cfmakeraw, tcgetattr, tcsetattr, termios, TCSANOW};
    unsafe {
        let fd = 0; // STDIN_FILENO
        let mut t: termios = std::mem::zeroed();
        if tcgetattr(fd, &mut t) != 0 {
            return;
        }
        cfmakeraw(&mut t);
        let _ = tcsetattr(fd, TCSANOW, &t);
    }
}

#[cfg(not(unix))]
fn enter_raw_mode() {}

fn main() {
    enter_raw_mode();

    let mut advertise_paste = false;
    let mut no_echo = false;
    let mut exit_on: Option<u8> = None;
    let mut tick_ms: Option<u64> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--advertise-paste" => advertise_paste = true,
            "--no-echo" => no_echo = true,
            "--exit-on" => {
                let hex = args.next().expect("--exit-on requires hex byte");
                exit_on = Some(
                    u8::from_str_radix(hex.trim_start_matches("0x"), 16)
                        .expect("--exit-on argument must be a hex byte (e.g. 0x04 or 04)"),
                );
            }
            "--tick-ms" => {
                let n = args.next().expect("--tick-ms requires an integer");
                tick_ms = Some(n.parse().expect("--tick-ms argument must be an integer"));
            }
            other => {
                eprintln!("stdin_echoer: unknown flag {other}");
                std::process::exit(2);
            }
        }
    }

    // Shared stdout, so the tick thread and the echo loop don't tear
    // each other's writes. We hold the lock as a Mutex<Stdout> rather
    // than locking std's stdout because std's StdoutLock is !Send.
    let stdout = Arc::new(Mutex::new(std::io::stdout()));

    // Startup handshake: emit printable ASCII `R` (0x52) immediately.
    // The host-side helper (`EchoerSession::spawn`) drains until this
    // byte before returning, which guarantees that the POSIX
    // `cfmakeraw` above has been applied before the host issues its
    // first `write_stdin`. Without this fence, the kernel's line
    // discipline (still in cooked mode at host-write time) would echo
    // control characters back as `^X` form, breaking byte-exact MITM
    // assertions.
    //
    // #452: we used `\x06` (ASCII ACK) here originally, but on
    // Windows Server 2025 (build 26100) ConPTY in virtual-screen mode
    // silently filtered the C0 control byte from the master pipe.
    // A printable byte survives the renderer round-trip.
    //
    // We deliberately do NOT emit any extra debug bytes here: under a
    // PTY both stdout and stderr route through the same slave, so an
    // eprintln would interleave with the master pipe and pollute the
    // byte-exact assertions. The investigation hooks for #452 live in
    // the helper's panic-message renderer instead, where they only
    // fire when the handshake actually fails.
    {
        let mut guard = stdout.lock().expect("stdout mutex poisoned");
        guard.write_all(b"R").expect("handshake write");
        guard.flush().expect("handshake flush");
    }

    if advertise_paste {
        // Bracketed-paste enable sequence per xterm DEC mode 2004.
        let mut guard = stdout.lock().expect("stdout mutex poisoned");
        guard.write_all(b"\x1b[?2004h").expect("advertise paste");
        guard.flush().expect("flush startup");
    }

    if let Some(period) = tick_ms {
        let stdout = Arc::clone(&stdout);
        std::thread::spawn(move || {
            let interval = Duration::from_millis(period);
            loop {
                std::thread::sleep(interval);
                let mut guard = stdout.lock().expect("stdout mutex poisoned");
                if guard.write_all(b"T\n").is_err() {
                    return;
                }
                if guard.flush().is_err() {
                    return;
                }
            }
        });
    }

    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let mut buf = [0u8; 4096];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => {
                let chunk = &buf[..n];
                if !no_echo {
                    let mut guard = stdout.lock().expect("stdout mutex poisoned");
                    guard.write_all(chunk).expect("echo write");
                    guard.flush().expect("echo flush");
                }
                if let Some(needle) = exit_on {
                    if chunk.contains(&needle) {
                        return;
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("stdin_echoer: read error: {e}");
                std::process::exit(1);
            }
        }
    }
}
