mod reqwest {
    pub struct Client;
    pub struct RequestBuilder;

    impl Client {
        pub fn new() -> Self {
            Self
        }

        pub fn get(&self, _url: &str) -> RequestBuilder {
            RequestBuilder
        }
    }

    impl RequestBuilder {
        pub fn send(self) {}
    }
}

fn main() {
    let client = reqwest::Client::new();
    let _request = client.get("https://example.invalid").send();
}
