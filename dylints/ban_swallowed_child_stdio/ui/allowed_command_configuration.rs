use std::process::{Command, Stdio};

fn main() {
    let mut command = Command::new("rustc");
    command.arg("-V").stdout(Stdio::piped()).stderr(Stdio::inherit());
}
