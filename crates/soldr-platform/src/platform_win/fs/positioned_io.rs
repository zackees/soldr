//! Windows fs implementation: positional I/O.

use std::io;

/// Write `buf` at `offset` without moving a shared file cursor.
pub fn write_at(file: &std::fs::File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_write(buf, offset)
}
