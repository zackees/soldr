//! Backend endpoint allocation for broker-managed daemon spawns.
//!
//! The v1 backend pipe name is frozen in `lifecycle::names`; this module owns
//! the runtime side: generating 128 bits of entropy, converting the derived
//! pipe name into an `Endpoint`, and avoiding duplicate allocations within one
//! broker process.

use std::collections::HashSet;

use crate::broker::lifecycle::names::{backend_pipe, PipePath, PipePathError};
use crate::broker::protocol::Endpoint;

/// Default number of random candidates tried before reporting exhaustion.
pub const DEFAULT_BACKEND_ENDPOINT_ATTEMPTS: usize = 16;

/// Allocates unguessable backend IPC endpoints for one broker namespace.
#[derive(Debug)]
pub struct BackendEndpointAllocator {
    user_sid_hash: String,
    namespace_id: String,
    max_attempts: usize,
    reserved_paths: HashSet<String>,
}

impl BackendEndpointAllocator {
    /// Create an allocator for one per-user broker namespace.
    pub fn new(user_sid_hash: impl Into<String>, namespace_id: impl Into<String>) -> Self {
        Self {
            user_sid_hash: user_sid_hash.into(),
            namespace_id: namespace_id.into(),
            max_attempts: DEFAULT_BACKEND_ENDPOINT_ATTEMPTS,
            reserved_paths: HashSet::new(),
        }
    }

    /// Override the collision retry bound.
    pub fn with_max_attempts(mut self, max_attempts: usize) -> Self {
        self.max_attempts = max_attempts.max(1);
        self
    }

    /// Reserve a path that should not be returned by future allocations.
    pub fn reserve_path(&mut self, path: impl Into<String>) {
        self.reserved_paths.insert(path.into());
    }

    /// Allocate one endpoint using operating-system randomness.
    pub fn allocate(&mut self) -> Result<Endpoint, BackendEndpointAllocatorError> {
        self.allocate_with_random128(|| {
            let mut bytes = [0_u8; 16];
            getrandom::fill(&mut bytes)?;
            Ok(bytes)
        })
    }

    /// Allocate one endpoint from a deterministic random source.
    ///
    /// Tests use this to force collisions without weakening the production
    /// randomness path.
    pub fn allocate_with_random128<F>(
        &mut self,
        mut next_random128: F,
    ) -> Result<Endpoint, BackendEndpointAllocatorError>
    where
        F: FnMut() -> Result<[u8; 16], BackendEndpointAllocatorError>,
    {
        for _ in 0..self.max_attempts {
            let random128 = next_random128()?;
            let path = endpoint_path(backend_pipe(&self.user_sid_hash, &random128)?)?;
            if self.reserved_paths.insert(path.clone()) {
                return Ok(Endpoint {
                    namespace_id: self.namespace_id.clone(),
                    path,
                });
            }
        }

        Err(BackendEndpointAllocatorError::CollisionExhausted {
            attempts: self.max_attempts,
        })
    }
}

/// Errors raised while allocating backend endpoints.
#[derive(Debug, thiserror::Error)]
pub enum BackendEndpointAllocatorError {
    /// Random byte generation failed.
    #[error("backend endpoint random generation failed: {0}")]
    Random(String),
    /// The frozen pipe-name derivation rejected its inputs.
    #[error(transparent)]
    PipePath(#[from] PipePathError),
    /// The platform path variant did not match the current platform.
    #[error("backend pipe path did not contain the current platform variant")]
    MissingPlatformPath,
    /// All random candidates collided with paths already reserved by this allocator.
    #[error("backend endpoint allocation exhausted after {attempts} collision attempts")]
    CollisionExhausted {
        /// Number of candidates attempted.
        attempts: usize,
    },
}

impl From<getrandom::Error> for BackendEndpointAllocatorError {
    fn from(value: getrandom::Error) -> Self {
        Self::Random(value.to_string())
    }
}

fn endpoint_path(pipe_path: PipePath) -> Result<String, BackendEndpointAllocatorError> {
    #[cfg(windows)]
    {
        pipe_path
            .windows
            .ok_or(BackendEndpointAllocatorError::MissingPlatformPath)
    }

    #[cfg(unix)]
    {
        pipe_path
            .unix
            .map(|path| path.to_string_lossy().into_owned())
            .ok_or(BackendEndpointAllocatorError::MissingPlatformPath)
    }
}
