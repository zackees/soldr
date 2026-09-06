//! Source-only fixture for the embedded zccache Wasm-output regression.

fn main() {
    // Keep the command artifact real while avoiding dependencies or a checked-in
    // Wasm binary: the test must exercise rustc output materialization itself.
    let worker_count = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    println!("wasm thread fixture: {worker_count}");
}
