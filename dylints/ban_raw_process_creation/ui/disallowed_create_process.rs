extern "system" {
    fn CreateProcessW();
}

fn main() {
    unsafe {
        CreateProcessW();
    }
}
