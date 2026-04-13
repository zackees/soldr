//! Compilation caching.
//!
//! Wraps rustc invocations, hashes inputs, and stores/retrieves
//! compiled artifacts (.o, .rlib, .rmeta) for cache hits.
