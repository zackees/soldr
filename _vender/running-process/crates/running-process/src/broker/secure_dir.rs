//! Private-directory helpers for broker-owned disk state.
//!
//! Broker manifests and service definitions both sit in per-user
//! directories that must not be writable by other users.

use std::fs;
use std::io;
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnsurePrivateDirOutcome {
    #[cfg(windows)]
    AlreadyPrivate,
    Hardened,
}

/// Create `path` and restrict it to the current user.
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    ensure_private_dir_with_outcome(path).map(|_| ())
}

fn ensure_private_dir_with_outcome(path: &Path) -> io::Result<EnsurePrivateDirOutcome> {
    fs::create_dir_all(path)?;

    // On Windows, applying a protected inheritable DACL re-propagates ACLs
    // through every existing descendant. Large cache roots can contain tens
    // of thousands of files, so repeating that operation on every manifest
    // write turns a constant-size write into a many-second tree walk. The
    // current DACL is already the complete policy: if it matches, there is
    // nothing to repair. The Windows predicate deliberately rejects the old
    // non-inheritable owner-only shape, so legacy roots still take the repair
    // path and heal descendants affected by that historical bug.
    #[cfg(windows)]
    if private_dir_permissions_are_private(path).unwrap_or(false) {
        return Ok(EnsurePrivateDirOutcome::AlreadyPrivate);
    }

    set_private_permissions(path)?;
    if private_dir_permissions_are_private(path)? {
        Ok(EnsurePrivateDirOutcome::Hardened)
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "private-directory permissions were not applied to {}",
                path.display()
            ),
        ))
    }
}

/// Return true when `path` has current-user-only permissions.
pub fn private_dir_permissions_are_private(path: &Path) -> io::Result<bool> {
    platform_private_dir_permissions_are_private(path)
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o700);
    fs::set_permissions(path, perms)
}

#[cfg(unix)]
fn platform_private_dir_permissions_are_private(path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::PermissionsExt;

    let mode = fs::metadata(path)?.permissions().mode();
    Ok(mode & 0o077 == 0)
}

/// Protected, owner-only DACL for private broker dirs.
///
/// The ACEs MUST carry `OICI` (container + object inherit). Applying a
/// protected DACL through `SetNamedSecurityInfoW` re-propagates
/// auto-inheritance to every existing descendant: each child's inherited
/// ACEs are recomputed from this DACL. With the non-inheritable
/// `D:P(A;;FA;;;OW)` this crate used before, that recomputation stripped
/// children to an EMPTY DACL — deny-everyone, including the owner — and
/// the damage escaped the tree through NTFS hardlinks, which share one
/// security descriptor per file (a hardlinked binary inside the private
/// dir bricked its sibling link in the caller's install dir; see
/// zackees/soldr#1513). `OICI` makes propagation grant owner + SYSTEM
/// full control down the tree instead, which also self-heals descendants
/// bricked by the old shape when the DACL is re-applied.
///
/// SYSTEM is granted alongside OWNER RIGHTS so AV, search indexing, and
/// backup agents keep working; it does not weaken the other-users
/// exclusion this hardening exists for.
#[cfg(windows)]
const PRIVATE_DIR_SDDL: &str = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)";

#[cfg(windows)]
fn set_private_permissions(path: &Path) -> io::Result<()> {
    apply_protected_dacl_sddl(path, PRIVATE_DIR_SDDL)
}

#[cfg(windows)]
fn apply_protected_dacl_sddl(path: &Path, sddl: &str) -> io::Result<()> {
    use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;

    apply_dacl_sddl(path, sddl, PROTECTED_DACL_SECURITY_INFORMATION)
}

#[cfg(windows)]
fn apply_dacl_sddl(
    path: &Path,
    sddl: &str,
    inheritance_control: windows_sys::Win32::Security::OBJECT_SECURITY_INFORMATION,
) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::{SetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{GetSecurityDescriptorDacl, DACL_SECURITY_INFORMATION};

    let sd = LocalSecurityDescriptor::from_sddl(sddl)?;
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl = std::ptr::null_mut();
    let ok = unsafe { GetSecurityDescriptorDacl(sd.0, &mut present, &mut dacl, &mut defaulted) };
    if ok == 0 || present == 0 || dacl.is_null() {
        return Err(io::Error::last_os_error());
    }

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let status = unsafe {
        SetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | inheritance_control,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null_mut(),
        )
    };
    if status != ERROR_SUCCESS {
        Err(io::Error::from_raw_os_error(status as i32))
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn platform_private_dir_permissions_are_private(path: &Path) -> io::Result<bool> {
    let actual = file_security_descriptor(path)?;
    if !security_descriptor_dacl_is_protected(actual.0)? {
        return Ok(false);
    }
    let actual_dacl = security_descriptor_dacl(actual.0)?;

    // Compare binary ACLs, not SDDL text. SDDL callback/object ACE payloads
    // can themselves contain strings that look like ordinary ACEs, so
    // substring matching can be spoofed even when the total ACE count is
    // correct. Binary equality validates the ACL revision plus every ACE's
    // type, flags, mask, SID, order, count, and callback/object payload.
    let expected = LocalSecurityDescriptor::from_sddl(PRIVATE_DIR_SDDL)?;
    let expected_dacl = security_descriptor_dacl(expected.0)?;
    Ok(acl_bytes(actual_dacl)? == acl_bytes(expected_dacl)?)
}

#[cfg(windows)]
fn file_security_descriptor(path: &Path) -> io::Result<LocalSecurityDescriptor> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    if sd.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "GetNamedSecurityInfoW returned a null security descriptor",
        ));
    }
    Ok(LocalSecurityDescriptor(sd))
}

#[cfg(windows)]
fn security_descriptor_dacl_is_protected(
    sd: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR,
) -> io::Result<bool> {
    use windows_sys::Win32::Security::{
        GetSecurityDescriptorControl, SECURITY_DESCRIPTOR_CONTROL, SE_DACL_PROTECTED,
    };

    let mut control: SECURITY_DESCRIPTOR_CONTROL = 0;
    let mut revision = 0;
    let ok = unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut revision) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(control & SE_DACL_PROTECTED != 0)
}

#[cfg(windows)]
fn security_descriptor_dacl(
    sd: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR,
) -> io::Result<*mut windows_sys::Win32::Security::ACL> {
    use windows_sys::Win32::Security::{GetSecurityDescriptorDacl, ACL};

    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let ok = unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if dacl.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "directory has no DACL",
        ));
    }
    Ok(dacl)
}

#[cfg(windows)]
fn acl_bytes(dacl: *const windows_sys::Win32::Security::ACL) -> io::Result<Vec<u8>> {
    use windows_sys::Win32::Security::{
        AclSizeInformation, GetAclInformation, ACL_SIZE_INFORMATION,
    };

    let mut acl_info = ACL_SIZE_INFORMATION {
        AceCount: 0,
        AclBytesInUse: 0,
        AclBytesFree: 0,
    };
    let ok = unsafe {
        GetAclInformation(
            dacl,
            (&mut acl_info as *mut ACL_SIZE_INFORMATION).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe {
        std::slice::from_raw_parts(dacl.cast::<u8>(), acl_info.AclBytesInUse as usize).to_vec()
    })
}

#[cfg(windows)]
struct LocalSecurityDescriptor(windows_sys::Win32::Security::PSECURITY_DESCRIPTOR);

#[cfg(windows)]
impl LocalSecurityDescriptor {
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };

        let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sd = std::ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 || sd.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(sd))
        }
    }
}

#[cfg(windows)]
impl Drop for LocalSecurityDescriptor {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(self.0.cast());
        }
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::fs::{self, File};

    use super::*;

    #[test]
    fn ensure_private_dir_passes_private_check() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("private");
        ensure_private_dir(&dir).unwrap();
        assert!(private_dir_permissions_are_private(&dir).unwrap());
    }

    /// Regression for zackees/running-process#624: re-applying a protected
    /// inheritable DACL asks Windows to propagate it through the full tree.
    /// A warm manifest publication must recognize the already-current root
    /// and skip that operation regardless of descendant count.
    #[test]
    fn ensure_private_dir_is_a_noop_for_an_already_private_populated_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("private");
        ensure_private_dir(&dir).unwrap();

        for index in 0..1_500 {
            let shard = dir.join(format!("shard-{index:04}"));
            fs::create_dir(&shard).unwrap();
            fs::write(shard.join("artifact.bin"), b"payload").unwrap();
        }

        assert_eq!(
            ensure_private_dir_with_outcome(&dir).unwrap(),
            EnsurePrivateDirOutcome::AlreadyPrivate,
            "a current root must not trigger recursive DACL propagation"
        );
        File::open(dir.join("shard-1499/artifact.bin"))
            .expect("the no-op path must preserve descendant access");
    }

    /// A two-ACE DACL can still be insecure when one grant uses an object or
    /// callback ACE instead of the expected ordinary owner grant. Binary ACL
    /// comparison must reject a same-count, nonstandard ACE without relying
    /// on text parsing.
    #[test]
    fn nonstandard_two_ace_policy_is_rejected_and_repaired() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("private");
        fs::create_dir_all(&dir).unwrap();
        apply_protected_dacl_sddl(&dir, "D:P(A;OICI;FA;;;SY)(OA;OICI;FA;;;OW)").unwrap();

        assert!(
            !private_dir_permissions_are_private(&dir).unwrap(),
            "same-count but structurally different ACEs must be rejected"
        );
        assert_eq!(
            ensure_private_dir_with_outcome(&dir).unwrap(),
            EnsurePrivateDirOutcome::Hardened
        );
        assert!(private_dir_permissions_are_private(&dir).unwrap());
    }

    /// ACL bytes do not carry the security descriptor's inheritance-control
    /// bits. An otherwise identical DACL must not take the no-op path when it
    /// is unprotected, because the parent may grant new inherited access
    /// later without changing this directory explicitly.
    #[test]
    fn unprotected_identical_acl_is_rejected_and_repaired() {
        use windows_sys::Win32::Security::UNPROTECTED_DACL_SECURITY_INFORMATION;

        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("parent");
        let dir = parent.join("private");
        fs::create_dir_all(&dir).unwrap();

        // Remove inheritable ACEs from the parent so clearing protection on
        // the child does not also change its ACL bytes. This isolates the
        // descriptor control bit as the only policy difference.
        apply_protected_dacl_sddl(&parent, "D:P(A;;FA;;;OW)(A;;FA;;;SY)").unwrap();
        apply_protected_dacl_sddl(&dir, PRIVATE_DIR_SDDL).unwrap();
        let protected = file_security_descriptor(&dir).unwrap();
        let protected_bytes = acl_bytes(security_descriptor_dacl(protected.0).unwrap()).unwrap();

        apply_dacl_sddl(
            &dir,
            PRIVATE_DIR_SDDL,
            UNPROTECTED_DACL_SECURITY_INFORMATION,
        )
        .unwrap();
        let unprotected = file_security_descriptor(&dir).unwrap();
        assert!(!security_descriptor_dacl_is_protected(unprotected.0).unwrap());
        assert_eq!(
            acl_bytes(security_descriptor_dacl(unprotected.0).unwrap()).unwrap(),
            protected_bytes,
            "the fixture must differ only in the descriptor protection bit"
        );
        assert!(!private_dir_permissions_are_private(&dir).unwrap());

        assert_eq!(
            ensure_private_dir_with_outcome(&dir).unwrap(),
            EnsurePrivateDirOutcome::Hardened
        );
        assert!(private_dir_permissions_are_private(&dir).unwrap());
    }

    /// Regression for zackees/soldr#1513: applying the protected DACL to
    /// a dir with existing contents must NOT strip the children's access.
    /// The old non-inheritable shape left every descendant with an empty
    /// DACL (deny-everyone).
    #[test]
    fn ensure_private_dir_keeps_existing_children_accessible() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("private");
        let sub = dir.join("v1.0.0");
        fs::create_dir_all(&sub).unwrap();
        let file = sub.join("service.bin");
        fs::write(&file, b"payload").unwrap();

        ensure_private_dir(&dir).unwrap();

        File::open(&file).expect("child file must stay readable by the owner");
        fs::write(&file, b"payload2").expect("child file must stay writable by the owner");
        fs::read_dir(&sub).expect("child dir must stay listable by the owner");
    }

    /// Regression for zackees/soldr#1513 (hardlink leak): NTFS hardlinks
    /// share one security descriptor per file, so stripping a link inside
    /// the private dir bricked the sibling link outside it (the
    /// pip-installed soldr.exe). The inheritable owner DACL must leave the
    /// outside link usable by the owner.
    #[test]
    fn ensure_private_dir_does_not_brick_hardlinked_files_outside() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside.bin");
        fs::write(&outside, b"binary").unwrap();
        let dir = tmp.path().join("private");
        fs::create_dir_all(&dir).unwrap();
        fs::hard_link(&outside, dir.join("inside.bin")).unwrap();

        ensure_private_dir(&dir).unwrap();

        File::open(&outside).expect("hardlinked sibling must stay readable by the owner");
        fs::write(&outside, b"binary2").expect("hardlinked sibling must stay writable");
    }

    /// The legacy non-inheritable shape is explicitly NOT considered
    /// private any more, so probe-and-repair callers re-apply the fixed
    /// DACL and heal descendants stripped by the old one.
    #[test]
    fn legacy_non_inheritable_dacl_is_rejected_and_healed_by_reapply() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("private");
        let file = dir.join("service.bin");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&file, b"payload").unwrap();

        apply_protected_dacl_sddl(&dir, "D:P(A;;FA;;;OW)").unwrap();
        assert!(
            !private_dir_permissions_are_private(&dir).unwrap(),
            "legacy shape must be treated as not-private"
        );
        // The old shape stripped the child's inherited ACEs to an empty
        // DACL — confirm the failure mode this fix exists for, then heal.
        assert!(
            File::open(&file).is_err(),
            "legacy shape bricks children; if this starts passing, the \
             propagation behavior changed and the fix should be revisited"
        );

        assert_eq!(
            ensure_private_dir_with_outcome(&dir).unwrap(),
            EnsurePrivateDirOutcome::Hardened,
            "the rejected legacy shape must still force DACL repair"
        );
        assert!(private_dir_permissions_are_private(&dir).unwrap());
        File::open(&file).expect("re-applying the inheritable DACL must heal the child");
    }
}
