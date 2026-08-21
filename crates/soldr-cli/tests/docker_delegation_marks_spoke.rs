//! The Docker-delegated build must not be labelled an internal soldr fault
//! (soldr#2718).
//!
//! `exit_guard` annotates a non-zero exit with "soldr emitted no diagnostic
//! and ran no child process ... this is a fault in soldr itself ... please
//! report it" whenever nothing called `mark_spoke()`. Its module docs ask
//! callers to mark "wherever it spawns a child that inherits stdio", because
//! that child may have done the explaining.
//!
//! `docker_cross::run` hands stdio to `docker run`, and the container's soldr
//! is what reports compile errors, a missing `rust-toolchain.toml`, or an
//! unresolved dependency. Before soldr#2718 it never marked, so on a Windows
//! host *every* failing Windows -> Linux-GNU build printed a correct,
//! actionable diagnostic and then told the user to file a soldr bug for it.
//!
//! Why a source guard rather than a behavioural test: reaching the real spawn
//! needs a Windows host with a running Docker Linux engine, which is the
//! combination soldr#2693 records as unavailable on hosted runners. And
//! `mark_spoke()` sets a process-global one-way latch, so a same-binary test
//! that asserts on `spoke()` would depend on which other tests ran first.
//! A source guard is portable, order-independent, and catches the actual
//! regression: someone deleting the call without knowing why it is there.

mod common;

/// The file whose spawn must stay marked.
const DELEGATION: &str = "crates/soldr-cli/src/docker_cross.rs";

#[test]
fn the_docker_delegation_marks_the_invocation_as_having_spoken() {
    // Runtime resolution, not `CARGO_MANIFEST_DIR` -- the nextest-archive
    // replay remaps the workspace onto a different host.
    let path = common::workspace_root().join(DELEGATION);
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    // The rationale comment names `mark_spoke` too, so only non-comment
    // lines count as the call.
    let code: String = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        code.contains("exit_guard::mark_spoke()"),
        "{DELEGATION} spawns `docker run` with inherited stdio, so it must \
         call `exit_guard::mark_spoke()`. Without it, every failing delegated \
         build prints the container's diagnostic and is then annotated \
         \"soldr emitted no diagnostic and ran no child process ... this is a \
         fault in soldr itself\" (soldr#2718). If the delegation moved, move \
         this guard with it rather than deleting it."
    );
}
