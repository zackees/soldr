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
    pub fn send(&self) {}
}
