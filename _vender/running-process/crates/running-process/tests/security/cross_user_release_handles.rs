use running_process::maintenance::release_handles::{
    authorize_release_handles_request, ReleaseHandlesAuthorization,
    ReleaseHandlesAuthorizationError,
};

#[test]
fn cross_user_release_handles_refuses_different_daemon_owner() {
    let err = authorize_release_handles_request(ReleaseHandlesAuthorization {
        requester_account_id: account_id("1000"),
        daemon_owner_account_id: account_id("2000"),
        requester_can_write_target_path: true,
    })
    .unwrap_err();

    assert_eq!(err, ReleaseHandlesAuthorizationError::OwnerMismatch);
}

#[test]
fn cross_user_release_handles_refuses_same_owner_without_target_write_access() {
    let owner = account_id("1000");

    let err = authorize_release_handles_request(ReleaseHandlesAuthorization {
        requester_account_id: owner,
        daemon_owner_account_id: owner,
        requester_can_write_target_path: false,
    })
    .unwrap_err();

    assert_eq!(err, ReleaseHandlesAuthorizationError::TargetPathWriteDenied);
}

#[test]
fn cross_user_release_handles_allows_same_owner_with_target_write_access() {
    let owner = account_id("1000");

    authorize_release_handles_request(ReleaseHandlesAuthorization {
        requester_account_id: owner,
        daemon_owner_account_id: owner,
        requester_can_write_target_path: true,
    })
    .expect("same-owner requester with target write access should be authorized");
}

#[test]
fn cross_user_release_handles_rejects_empty_identity_inputs() {
    let requester_err = authorize_release_handles_request(ReleaseHandlesAuthorization {
        requester_account_id: " ",
        daemon_owner_account_id: account_id("1000"),
        requester_can_write_target_path: true,
    })
    .unwrap_err();
    assert_eq!(
        requester_err,
        ReleaseHandlesAuthorizationError::EmptyRequesterIdentity
    );

    let daemon_err = authorize_release_handles_request(ReleaseHandlesAuthorization {
        requester_account_id: account_id("1000"),
        daemon_owner_account_id: "\t",
        requester_can_write_target_path: true,
    })
    .unwrap_err();
    assert_eq!(
        daemon_err,
        ReleaseHandlesAuthorizationError::EmptyDaemonOwnerIdentity
    );
}

fn account_id(local_id: &'static str) -> &'static str {
    if cfg!(windows) {
        match local_id {
            "1000" => "S-1-5-21-1000",
            "2000" => "S-1-5-21-2000",
            _ => "S-1-5-21-9999",
        }
    } else {
        match local_id {
            "1000" => "uid:1000",
            "2000" => "uid:2000",
            _ => "uid:9999",
        }
    }
}
