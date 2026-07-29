struct Command;

trait CommandExt {
    fn creation_flags(&mut self, flags: u32);
}

impl CommandExt for Command {
    fn creation_flags(&mut self, _flags: u32) {}
}

fn main() {
    let mut command = Command;
    command.creation_flags(0x0800_0000);
}
