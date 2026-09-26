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

use running_process::{SpawnStdio, StdioSource};

fn main() {
    let _stdio = SpawnStdio {
        stdin: StdioSource::Null,
        stdout: StdioSource::Pipe,
        stderr: StdioSource::Null,
    };
}
