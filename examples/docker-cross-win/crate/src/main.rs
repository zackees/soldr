// Tiny test binary for the Linux→Windows cross-compile demo.
//
// Goals:
//   1. Print a recognizable signature so the build script can grep for
//      it to confirm the binary executed correctly on the Windows host.
//   2. Touch enough of std to ensure the cross-link actually linked the
//      Windows-flavored libstd (not just an empty `fn main() {}` that
//      could pass even if std weren't pulled in).
//   3. Exit with a non-zero code when ARG `--fail` is passed, so the
//      build script can also smoke-test failure propagation.

use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    let target_os = std::env::consts::OS;
    let target_arch = std::env::consts::ARCH;
    let host_pid = std::process::id();
    let exe_name = env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "<unknown>".to_string());

    println!("docker-cross-win-demo OK");
    println!("  target_os   = {target_os}");
    println!("  target_arch = {target_arch}");
    println!("  exe_name    = {exe_name}");
    println!("  pid         = {host_pid}");

    if env::args().any(|a| a == "--fail") {
        eprintln!("docker-cross-win-demo FAIL (requested via --fail)");
        return ExitCode::from(2);
    }
    ExitCode::SUCCESS
}
