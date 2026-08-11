pub mod local_socket {
    pub struct Name;
    pub struct GenericNamespaced;
    pub struct GenericFilePath;

    pub trait ToNsName {
        fn to_ns_name<T>(&self) -> Result<Name, ()>;
    }

    pub trait ToFsName {
        fn to_fs_name<T>(&self) -> Result<Name, ()>;
    }

    impl ToNsName for str {
        fn to_ns_name<T>(&self) -> Result<Name, ()> {
            Ok(Name)
        }
    }

    impl ToFsName for str {
        fn to_fs_name<T>(&self) -> Result<Name, ()> {
            Ok(Name)
        }
    }
}
