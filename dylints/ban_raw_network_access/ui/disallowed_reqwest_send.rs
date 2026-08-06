// aux-build:reqwest.rs

extern crate reqwest;

fn main() {
    let client = reqwest::Client::new();
    let _request = client.get("https://example.invalid").send();
}
