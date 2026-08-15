//! Linux running-image identity: the mapped GNU build ID.

/// The running executable's GNU build ID, read from the `PT_NOTE` segment
/// already mapped into this process. `None` when the image was linked
/// without one.
///
/// This exists so callers that only need to tell one linked image from
/// another do not have to read and hash the executable file: an
/// unoptimized Soldr image is 100+ MiB, and hashing it dominated broker
/// identity work (soldr#2517, soldr#2549). It is a linker-assigned
/// generation key, not an integrity measurement.
pub fn current_build_id() -> Option<Vec<u8>> {
    running_process::current_executable_build_id()
}
