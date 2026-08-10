//! Consumer-consumable test helpers (#415).
//!
//! Gated behind the off-by-default `test-support` cargo feature so that
//! the helpers ship in the published crate **but** are only compiled
//! when a consumer explicitly opts in as a `dev-dependency`:
//!
//! ```toml
//! [dev-dependencies]
//! running-process = { version = "3", features = ["test-support"] }
//! ```
//!
//! The kit's primary purpose is to give external consumer daemons
//! (zccache, soldr, fbuild, clud) a stable, well-documented surface for
//! conformance-testing their v1 broker integration (post-mortem §5.7)
//! without each crate re-implementing ~378 LOC of golden-bytes, probe,
//! and mixed-wire harness code.
//!
//! See [`conformance`] for the kit surface.

pub mod conformance;
