use std::process::{Command, Stdio};

fn main() {
    let mut command = Command::new("rustc");
    // reason: fixture proving the documented escape hatch.
    #[allow(ban_swallowed_child_stdio)]
    command.stdout(Stdio::null());
}
