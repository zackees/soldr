mod running_process {
    pub enum StdioSource {
        Null,
        Pipe,
    }

    pub struct SpawnStdio {
        pub stdin: StdioSource,
        pub stdout: StdioSource,
        pub stderr: StdioSource,
    }
}

use std::process::{Command, Stdio};

fn main() {
    let mut command = Command::new("rustc");
    command.stdin(Stdio::null());
    let _stdio = running_process::SpawnStdio {
        stdin: running_process::StdioSource::Null,
        stdout: running_process::StdioSource::Pipe,
        stderr: running_process::StdioSource::Pipe,
    };
}
