fn canonical_socket_name(path: &str) -> &str {
    path
}

fn main() {
    let _name = canonical_socket_name("resolved-endpoint");
}
