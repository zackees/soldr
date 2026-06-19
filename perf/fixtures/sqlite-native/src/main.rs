fn main() {
    let version = unsafe { libsqlite3_sys::sqlite3_libversion_number() };
    println!("sqlite_version_number={version}");
}
