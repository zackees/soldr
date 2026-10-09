//! soldr#3567: which `home_origin` a Cargo child's build log records.

use super::*;

#[test]
fn a_host_binary_pinned_to_a_separate_dylint_home_reports_dylint() {
    let dylint = Path::new("/soldr/rustup");
    let caller = Path::new("/home/user/.rustup");
    assert_eq!(
        dylint_child_home_origin(HomeOrigin::Caller, Some(dylint), Some(caller)),
        HomeOrigin::Dylint
    );
    assert_eq!(
        dylint_child_home_origin(HomeOrigin::RepoLocal, Some(dylint), Some(caller)),
        HomeOrigin::Dylint
    );
}

#[test]
fn a_dylint_home_that_is_the_callers_own_keeps_the_binary_origin() {
    let caller = Path::new("/home/user/.rustup");
    assert_eq!(
        dylint_child_home_origin(HomeOrigin::Caller, Some(caller), Some(caller)),
        HomeOrigin::Caller
    );
    assert_eq!(
        dylint_child_home_origin(HomeOrigin::Caller, None, Some(caller)),
        HomeOrigin::Caller
    );
}

#[test]
fn a_managed_binary_stays_managed_and_a_host_binary_never_claims_it() {
    let dylint = Path::new("/soldr/rustup");
    let caller = Path::new("/home/user/.rustup");
    assert_eq!(
        dylint_child_home_origin(HomeOrigin::Managed, Some(dylint), Some(caller)),
        HomeOrigin::Managed
    );
    // soldr#1799's CI guard rejects `managed` for a binary outside the
    // managed root, so the Dylint pin must never be reported that way.
    for origin in [HomeOrigin::Caller, HomeOrigin::RepoLocal] {
        assert_ne!(
            dylint_child_home_origin(origin, Some(dylint), Some(caller)),
            HomeOrigin::Managed
        );
    }
}
