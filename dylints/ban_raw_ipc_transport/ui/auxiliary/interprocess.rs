pub mod local_socket {
    pub struct ListenerOptions;
    pub struct ConnectOptions;

    impl ListenerOptions {
        pub fn new() -> Self {
            Self
        }
    }

    impl ConnectOptions {
        pub fn new() -> Self {
            Self
        }
    }
}

pub mod os {
    pub mod windows {
        pub mod named_pipe {
            pub struct DuplexPipeStream;

            impl DuplexPipeStream {
                pub fn connect_by_path_with_wait_mode() {}
            }
        }
    }
}
