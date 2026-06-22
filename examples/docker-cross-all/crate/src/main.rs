fn main() {
    println!(
        "docker-cross-all-demo OK target_os={} target_arch={} target_env={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::env::consts::FAMILY,
    );
}
