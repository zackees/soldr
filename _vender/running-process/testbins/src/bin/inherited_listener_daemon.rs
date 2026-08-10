//! Fixture: a daemon that serves the listener the broker handed it (#500).
//!
//! The unit tests for broker-owned bind prove the broker's half — it binds,
//! clears `FD_CLOEXEC`, and publishes the descriptor. They cannot prove the
//! half that matters to a client: that a *separate process*, after `exec`,
//! adopts that descriptor and answers on it. Only a real child can show that,
//! which is what this binary is for.
//!
//! It goes through [`bootstrap`] rather than calling `recover_from_env`
//! directly, so the test exercises the entry point a real brokered daemon
//! uses — including the branch that prefers an inherited listener over
//! binding its own. Calling the primitive here would leave that branch
//! covered only by inspection.
//!
//! Protocol, kept deliberately small so a failure points at the handover
//! rather than at the fixture:
//!
//! - Adopted a listener: accept one connection, write `SERVED`, exit `0`.
//! - No listener was passed: exit `2`.
//! - A listener was advertised but could not be adopted: exit `3`.
//!
//! The outcomes get distinct exit codes because they fail for different
//! reasons, and a test that cannot tell them apart would pass for the wrong
//! one — "the child inherited nothing" and "the child rejected what it
//! inherited" are indistinguishable from the parent otherwise.

use running_process::broker::brokered_backend::{
    bootstrap, BindError, BrokeredBackend, IpcListener, Never,
};

/// A backend whose `bind` always refuses.
///
/// The fixture exists to prove the *inherited* path works, so binding for
/// itself would defeat the point: a green test could then mean "the handover
/// worked" or "the handover was skipped and it bound its own socket". Failing
/// the self-bind makes those two outcomes distinguishable — exit 2 means
/// nothing was inherited.
struct InheritOnly;

impl BrokeredBackend for InheritOnly {
    type State = ();

    fn bind(_endpoint: &str) -> Result<IpcListener, BindError> {
        Err(BindError::Other(
            "fixture refuses to self-bind; it exists to prove the handover".to_string(),
        ))
    }

    fn serve(listener: IpcListener) -> Never {
        match serve_once(listener) {
            Ok(()) => std::process::exit(0),
            Err(error) => {
                eprintln!("inherited-listener-daemon: serving failed: {error}");
                std::process::exit(4);
            }
        }
    }
}

fn main() {
    // The endpoint argument is unused on the inherited path — the broker
    // already bound it — and the self-bind path refuses regardless.
    match bootstrap::<InheritOnly>("inherited-listener-fixture") {
        // `serve` diverges via `process::exit`, so reaching here means the
        // listener was never obtained.
        Ok(()) => {
            eprintln!("inherited-listener-daemon: bootstrap returned without serving");
            std::process::exit(5);
        }
        Err(BindError::Other(reason)) => {
            // The self-bind refusal above: nothing was handed over.
            eprintln!("inherited-listener-daemon: no listener was passed ({reason})");
            std::process::exit(2);
        }
        Err(error) => {
            // A descriptor was advertised and could not be adopted.
            eprintln!("inherited-listener-daemon: listener not adoptable: {error:?}");
            std::process::exit(3);
        }
    }
}

/// Accept exactly one connection and answer it.
fn serve_once(listener: IpcListener) -> std::io::Result<()> {
    use interprocess::local_socket::traits::Listener as _;
    use std::io::Write as _;

    let mut stream = listener.accept()?;
    // The marker proves the bytes came from this process rather than from a
    // broker that happened to still be holding the socket open.
    stream.write_all(b"SERVED\n")?;
    stream.flush()?;
    Ok(())
}
