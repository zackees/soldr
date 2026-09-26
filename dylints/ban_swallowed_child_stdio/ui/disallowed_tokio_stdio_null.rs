mod tokio {
    pub mod process {
        pub struct Command;

        impl Command {
            pub fn stderr(&mut self, _cfg: std::process::Stdio) -> &mut Self {
                self
            }
        }
    }
}

fn main() {
    let mut command = tokio::process::Command;
    command.stderr(std::process::Stdio::null());
}
