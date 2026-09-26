use std::process::{Command, Stdio};

fn main() {
    let mut command = Command::new("rustc");
    command.stderr(Stdio::null());
}
