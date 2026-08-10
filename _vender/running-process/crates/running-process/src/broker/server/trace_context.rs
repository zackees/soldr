//! Trace context captured from the v1 broker frame.

use crate::broker::protocol::Frame;

/// W3C trace context plus the broker frame request id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TraceContext {
    /// Broker frame request id.
    pub request_id: u64,
    /// W3C traceparent header value.
    pub traceparent: String,
    /// W3C tracestate header value.
    pub tracestate: String,
}

impl TraceContext {
    /// Capture trace metadata from a broker frame.
    pub fn from_frame(frame: &Frame) -> Self {
        Self {
            request_id: frame.request_id,
            traceparent: frame.traceparent.clone(),
            tracestate: frame.tracestate.clone(),
        }
    }

    /// Return the non-empty W3C headers in backend-forwarding order.
    pub fn backend_headers(&self) -> Vec<(&'static str, String)> {
        let mut headers = Vec::new();
        if !self.traceparent.is_empty() {
            headers.push(("traceparent", self.traceparent.clone()));
        }
        if !self.tracestate.is_empty() {
            headers.push(("tracestate", self.tracestate.clone()));
        }
        headers
    }
}
