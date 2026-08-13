//! Host-neutral filesystem facade.
//!
//! Owns stable file identity and same-file comparison, link/reparse
//! classification, symlink creation/removal, owner-only/writable/executable
//! permissions, atomic replacement and open-running-image retirement,
//! volume identity and free-space probes, positional I/O, and the
//! normalization of OS-specific lock/contention errors. Callers retain
//! archive traversal, cache/hash policy, retry policy, and authorization
//! to delete or replace data.

pub mod contention;
pub mod identity;
pub mod links;
pub mod permissions;
pub mod positioned_io;
pub mod replace;
pub mod volume;
