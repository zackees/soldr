//! The registration state machine.
//!
//! `ARMED` is the state in which the daemon will act on a process — capture its
//! stacks, read its memory, profile it. Reaching it therefore requires two
//! independent facts, and this module exists so there is exactly one code path
//! that can assert them:
//!
//! 1. **Identity verified** — the process is who it claimed to be (exe hash,
//!    boot id, still alive).
//! 2. **A live probe connection** — the registrant is still there.
//!
//! [`arm`] is the only way into `Armed`. There is deliberately no
//! `set_state(Armed)` back door: a second path would be a second place to get
//! the guard wrong.

/// Lifecycle of a single registration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegState {
    /// No registration exists.
    Unregistered,
    /// Registration accepted, identity not yet proven.
    Registering,
    /// Verified and live. The daemon will act on this process.
    Armed,
    /// Terminal. A re-registration mints a fresh entry rather than reviving
    /// this one — resurrection would let a dropped process inherit the trust
    /// its predecessor earned.
    Dropped,
}

/// Why a transition was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StateError {
    /// Arming attempted before identity verification succeeded.
    #[error("cannot arm: identity not verified")]
    IdentityUnverified,
    /// Arming attempted with no live probe connection.
    #[error("cannot arm: no live probe connection")]
    NoLiveProbe,
    /// The edge is not part of the machine.
    #[error("illegal transition {from:?} -> {to:?}")]
    Illegal {
        /// State being left.
        from: RegState,
        /// State that was requested.
        to: RegState,
    },
}

impl RegState {
    /// Whether `self -> to` is an edge of the machine.
    ///
    /// The full edge set, and nothing else:
    /// `Unregistered -> Registering -> {Armed, Dropped}`, `Armed -> Dropped`.
    pub fn can_transition_to(self, to: RegState) -> bool {
        matches!(
            (self, to),
            (RegState::Unregistered, RegState::Registering)
                | (RegState::Registering, RegState::Armed)
                | (RegState::Registering, RegState::Dropped)
                | (RegState::Armed, RegState::Dropped)
        )
    }
}

/// The only route to [`RegState::Armed`].
///
/// Both guards are checked before the edge is considered, so an illegal
/// starting state and a missing guard produce distinct errors.
pub fn arm(
    from: RegState,
    identity_verified: bool,
    connection_alive: bool,
) -> Result<RegState, StateError> {
    if !identity_verified {
        return Err(StateError::IdentityUnverified);
    }
    if !connection_alive {
        return Err(StateError::NoLiveProbe);
    }
    if !from.can_transition_to(RegState::Armed) {
        return Err(StateError::Illegal {
            from,
            to: RegState::Armed,
        });
    }
    Ok(RegState::Armed)
}

/// Move to [`RegState::Dropped`]. Idempotent: dropping an already-dropped
/// entry is a no-op rather than an error, because the connection-close and
/// heartbeat-reaper paths can both fire for the same entry.
pub fn drop_state(from: RegState) -> RegState {
    let _ = from;
    RegState::Dropped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legal_edges_are_exactly_the_documented_set() {
        let all = [
            RegState::Unregistered,
            RegState::Registering,
            RegState::Armed,
            RegState::Dropped,
        ];
        let legal: Vec<(RegState, RegState)> = all
            .iter()
            .flat_map(|&f| all.iter().map(move |&t| (f, t)))
            .filter(|&(f, t)| f.can_transition_to(t))
            .collect();

        assert_eq!(
            legal,
            vec![
                (RegState::Unregistered, RegState::Registering),
                (RegState::Registering, RegState::Armed),
                (RegState::Registering, RegState::Dropped),
                (RegState::Armed, RegState::Dropped),
            ]
        );
    }

    #[test]
    fn dropped_is_terminal() {
        for to in [
            RegState::Unregistered,
            RegState::Registering,
            RegState::Armed,
            RegState::Dropped,
        ] {
            assert!(
                !RegState::Dropped.can_transition_to(to),
                "Dropped must not transition to {to:?}"
            );
        }
    }

    #[test]
    fn arming_requires_verified_identity() {
        assert_eq!(
            arm(RegState::Registering, false, true),
            Err(StateError::IdentityUnverified)
        );
    }

    #[test]
    fn arming_requires_a_live_probe() {
        assert_eq!(
            arm(RegState::Registering, true, false),
            Err(StateError::NoLiveProbe)
        );
    }

    #[test]
    fn arming_from_a_non_registering_state_is_illegal() {
        // Even with both guards satisfied, the edge must exist.
        assert_eq!(
            arm(RegState::Unregistered, true, true),
            Err(StateError::Illegal {
                from: RegState::Unregistered,
                to: RegState::Armed
            })
        );
        assert!(arm(RegState::Dropped, true, true).is_err());
    }

    #[test]
    fn happy_path_arms() {
        assert_eq!(arm(RegState::Registering, true, true), Ok(RegState::Armed));
    }

    #[test]
    fn dropping_is_idempotent() {
        assert_eq!(drop_state(RegState::Armed), RegState::Dropped);
        assert_eq!(drop_state(RegState::Dropped), RegState::Dropped);
    }
}
