use super::{private_tempdir, private_tempfile};
use crate::storage::{atomic_write, private_dir, protect};
use std::fs;
use tempfile::tempdir;

#[cfg(unix)]
fn assert_mode(path: &std::path::Path, expected: u32) {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        expected
    );
}

#[test]
fn tempfile_is_empty_and_private_when_created() {
    let parent = tempdir().unwrap();
    let file = private_tempfile(parent.path()).unwrap();

    assert_eq!(fs::metadata(file.path()).unwrap().len(), 0);
    #[cfg(unix)]
    assert_mode(file.path(), 0o600);
    #[cfg(windows)]
    windows_acl::assert_private(file.path());
}

#[test]
fn tempdir_is_empty_and_private_when_created_and_close_removes_it() {
    let parent = tempdir().unwrap();
    let directory = private_tempdir(parent.path(), "private-").unwrap();
    let path = directory.path().to_path_buf();

    assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
    #[cfg(unix)]
    assert_mode(&path, 0o700);
    #[cfg(windows)]
    windows_acl::assert_private(&path);

    directory.close().unwrap();
    assert!(!path.exists());
}

#[test]
fn dropping_tempdir_removes_it() {
    let parent = tempdir().unwrap();
    let path = {
        let directory = private_tempdir(parent.path(), "private-").unwrap();
        directory.path().to_path_buf()
    };

    assert!(!path.exists());
}

#[test]
fn recursively_created_directories_are_private() {
    let fixture = tempdir().unwrap();
    let first = fixture.path().join("new-parent");
    let second = first.join("nested");
    let target = second.join("private-data");

    private_dir(&target).unwrap();

    #[cfg(unix)]
    for path in [&first, &second, &target] {
        assert_mode(path, 0o700);
    }
    #[cfg(windows)]
    for path in [&first, &second, &target] {
        windows_acl::assert_private(path);
    }
}

#[test]
fn atomic_write_preserves_existing_export_directory_permissions() {
    let fixture = tempdir().unwrap();
    let export = fixture.path().join("ordinary-export");
    fs::create_dir(&export).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&export, fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[cfg(windows)]
    let before = windows_acl::snapshot(&export);

    atomic_write(&export.join("output.json"), b"fixture-output", None).unwrap();

    #[cfg(unix)]
    assert_mode(&export, 0o755);
    #[cfg(windows)]
    assert_eq!(windows_acl::snapshot(&export), before);
}

#[test]
fn creation_failure_does_not_write_to_the_blocking_file() {
    let fixture = tempdir().unwrap();
    let blocker = fixture.path().join("not-a-directory");
    fs::write(&blocker, b"fixture-before").unwrap();
    let target = blocker.join("output.json");

    assert!(atomic_write(&target, b"fixture-after", None).is_err());

    assert_eq!(fs::read(&blocker).unwrap(), b"fixture-before");
    assert!(!target.exists());
}

#[test]
fn atomic_replacement_leaves_a_private_final_file() {
    let fixture = tempdir().unwrap();
    let target = fixture.path().join("existing.json");
    fs::write(&target, b"fixture-old").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
    }

    atomic_write(&target, b"fixture-new", None).unwrap();

    assert_eq!(fs::read(&target).unwrap(), b"fixture-new");
    #[cfg(unix)]
    assert_mode(&target, 0o600);
    #[cfg(windows)]
    windows_acl::assert_private(&target);
}

#[test]
fn protecting_existing_sensitive_file_preserves_exact_bytes() {
    let fixture = tempdir().unwrap();
    let target = fixture.path().join("legacy-sensitive.json");
    let bytes = br#"{"access_token":"fixture-secret","future_field":{"kept":true}}"#;
    fs::write(&target, bytes).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
    }

    protect(&target, false).unwrap();

    assert_eq!(fs::read(&target).unwrap(), bytes);
    #[cfg(unix)]
    assert_mode(&target, 0o600);
    #[cfg(windows)]
    windows_acl::assert_private(&target);
}

#[cfg(windows)]
#[test]
fn protecting_current_user_owned_file_needs_no_write_owner_right() {
    use std::io::Write;

    let fixture = tempdir().unwrap();
    let bytes = b"sensitive fixture bytes";
    let mut file = private_tempfile(fixture.path()).unwrap();
    file.write_all(bytes).unwrap();
    let target = file.path().to_path_buf();
    assert!(windows_acl::owner_is_current_user(&target));
    windows_acl::set_owner_rights_read_control_and_write_dac(&target);

    protect(&target, false).unwrap();

    assert_eq!(fs::read(&target).unwrap(), bytes);
    windows_acl::assert_private(&target);
}

#[cfg(windows)]
#[test]
fn protecting_group_owned_file_restores_current_user_as_owner() {
    use windows_acl::SetOwnerResult;

    let fixture = tempdir().unwrap();
    let target = fixture.path().join("group-owner.fixture");
    let bytes = b"sensitive group-owner fixture";
    fs::write(&target, bytes).unwrap();

    match windows_acl::set_owner_to_primary_group(&target) {
        SetOwnerResult::Changed => (),
        SetOwnerResult::AlreadyUser => {
            eprintln!("skipping group-owner setup: primary group is the TokenUser SID");
            return;
        }
        SetOwnerResult::Unavailable(error) => {
            eprintln!("skipping group-owner setup: Windows rejected owner change ({error})");
            return;
        }
    }
    assert!(
        !windows_acl::owner_is_current_user(&target),
        "fixture was not made group-owned"
    );

    protect(&target, false).unwrap();

    assert_eq!(fs::read(&target).unwrap(), bytes);
    assert!(windows_acl::owner_is_current_user(&target));
    windows_acl::assert_private(&target);
}

#[cfg(windows)]
mod windows_acl {
    use std::{ffi::c_void, os::windows::ffi::OsStrExt, path::Path, ptr, slice};
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, LocalFree, ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER,
            ERROR_INVALID_OWNER, ERROR_PRIVILEGE_NOT_HELD,
        },
        Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW,
            GetNamedSecurityInfoW, SetNamedSecurityInfoW, SE_FILE_OBJECT,
        },
        Security::{
            EqualSid, GetAce, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
            GetTokenInformation, TokenPrimaryGroup, TokenUser, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
            DACL_SECURITY_INFORMATION, INHERITED_ACE, OWNER_SECURITY_INFORMATION,
            PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED, TOKEN_PRIMARY_GROUP,
            TOKEN_QUERY, TOKEN_USER,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

    struct LocalAllocation(*mut c_void);

    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }

    struct TokenHandle(windows_sys::Win32::Foundation::HANDLE);

    impl Drop for TokenHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn security_descriptor(
        path: &Path,
        security_information: u32,
    ) -> (LocalAllocation, *mut c_void, *mut ACL) {
        let name = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let mut owner = ptr::null_mut();
        let mut dacl = ptr::null_mut();
        let mut descriptor = ptr::null_mut();
        let result = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                security_information,
                &mut owner,
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(result, 0);
        (LocalAllocation(descriptor), owner, dacl)
    }

    fn current_token_user_sid() -> (*mut c_void, Vec<usize>) {
        let mut token = ptr::null_mut();
        assert_ne!(
            unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
            0,
            "OpenProcessToken failed"
        );
        let token = TokenHandle(token);

        let mut required = 0;
        assert_eq!(
            unsafe {
                windows_sys::Win32::Security::GetTokenInformation(
                    token.0,
                    TokenUser,
                    ptr::null_mut(),
                    0,
                    &mut required,
                )
            },
            0,
            "TokenUser size query unexpectedly succeeded"
        );
        let error = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        assert_eq!(
            error, ERROR_INSUFFICIENT_BUFFER,
            "TokenUser size query failed with an unexpected Win32 error"
        );
        assert!(
            required as usize >= std::mem::size_of::<TOKEN_USER>(),
            "TokenUser buffer size was invalid"
        );

        let word_count = (required as usize).div_ceil(std::mem::size_of::<usize>());
        let mut buffer = vec![0usize; word_count];
        assert_ne!(
            unsafe {
                windows_sys::Win32::Security::GetTokenInformation(
                    token.0,
                    TokenUser,
                    buffer.as_mut_ptr().cast(),
                    required,
                    &mut required,
                )
            },
            0,
            "GetTokenInformation(TokenUser) failed"
        );

        let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        assert!(!user.User.Sid.is_null(), "TokenUser SID is missing");
        (user.User.Sid, buffer)
    }

    fn primary_group_sid() -> (*mut c_void, Vec<usize>) {
        let mut token = ptr::null_mut();
        assert_ne!(
            unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
            0,
            "OpenProcessToken failed"
        );
        let token = TokenHandle(token);

        let mut required = 0;
        assert_eq!(
            unsafe {
                GetTokenInformation(
                    token.0,
                    TokenPrimaryGroup,
                    ptr::null_mut(),
                    0,
                    &mut required,
                )
            },
            0,
            "TokenPrimaryGroup size query unexpectedly succeeded"
        );
        assert_eq!(
            unsafe { windows_sys::Win32::Foundation::GetLastError() },
            ERROR_INSUFFICIENT_BUFFER,
            "TokenPrimaryGroup size query failed with an unexpected Win32 error"
        );
        assert!(
            required as usize >= std::mem::size_of::<TOKEN_PRIMARY_GROUP>(),
            "TokenPrimaryGroup buffer size was invalid"
        );

        let word_count = (required as usize).div_ceil(std::mem::size_of::<usize>());
        let mut buffer = vec![0usize; word_count];
        assert_ne!(
            unsafe {
                GetTokenInformation(
                    token.0,
                    TokenPrimaryGroup,
                    buffer.as_mut_ptr().cast(),
                    required,
                    &mut required,
                )
            },
            0,
            "GetTokenInformation(TokenPrimaryGroup) failed"
        );

        let group = unsafe { &*buffer.as_ptr().cast::<TOKEN_PRIMARY_GROUP>() };
        assert!(
            !group.PrimaryGroup.is_null(),
            "primary group SID is missing"
        );
        (group.PrimaryGroup, buffer)
    }

    pub(super) fn owner_is_current_user(path: &Path) -> bool {
        let (_descriptor, owner, _dacl) = security_descriptor(path, OWNER_SECURITY_INFORMATION);
        let (token_user, _token_user_buffer) = current_token_user_sid();
        !owner.is_null() && unsafe { EqualSid(owner, token_user) } != 0
    }

    pub(super) fn set_owner_rights_read_control_and_write_dac(path: &Path) {
        let sddl = "D:P(A;;RCWD;;;OW)\0".encode_utf16().collect::<Vec<_>>();
        let mut descriptor = ptr::null_mut();
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    1,
                    &mut descriptor,
                    ptr::null_mut(),
                )
            },
            0,
            "failed to build limited owner-rights DACL"
        );
        let descriptor = LocalAllocation(descriptor);

        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl = ptr::null_mut();
        assert_ne!(
            unsafe {
                GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted)
            },
            0
        );
        assert_ne!(present, 0);
        assert!(!dacl.is_null());

        let name = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        assert_eq!(
            unsafe {
                SetNamedSecurityInfoW(
                    name.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    dacl,
                    ptr::null_mut(),
                )
            },
            0,
            "failed to install limited owner-rights DACL"
        );
    }

    pub(super) enum SetOwnerResult {
        Changed,
        AlreadyUser,
        Unavailable(u32),
    }

    pub(super) fn set_owner_to_primary_group(path: &Path) -> SetOwnerResult {
        let (group, _group_buffer) = primary_group_sid();
        let (token_user, _user_buffer) = current_token_user_sid();
        if unsafe { EqualSid(group, token_user) } != 0 {
            return SetOwnerResult::AlreadyUser;
        }

        let name = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let result = unsafe {
            SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                group,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if result == 0 {
            SetOwnerResult::Changed
        } else if matches!(
            result,
            ERROR_ACCESS_DENIED | ERROR_INVALID_OWNER | ERROR_PRIVILEGE_NOT_HELD
        ) {
            SetOwnerResult::Unavailable(result)
        } else {
            panic!("SetNamedSecurityInfoW rejected primary group with error {result}");
        }
    }

    pub(super) fn snapshot(path: &Path) -> (u16, Vec<u8>) {
        let (descriptor, _owner, dacl) = security_descriptor(path, DACL_SECURITY_INFORMATION);
        assert!(!dacl.is_null());
        let mut control = 0;
        let mut revision = 0;
        assert_ne!(
            unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) },
            0
        );
        let acl = unsafe { &*dacl };
        let bytes = unsafe { slice::from_raw_parts(dacl.cast::<u8>(), acl.AclSize as usize) };
        (control, bytes.to_vec())
    }

    pub(super) fn assert_private(path: &Path) {
        let (descriptor, owner, dacl) =
            security_descriptor(path, DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION);
        assert!(!owner.is_null(), "security descriptor has no owner");
        let (token_user, _token_user_buffer) = current_token_user_sid();
        assert_ne!(
            unsafe { EqualSid(owner, token_user) },
            0,
            "created object owner does not match TokenUser"
        );
        assert!(!dacl.is_null());

        let mut control = 0;
        let mut revision = 0;
        assert_ne!(
            unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) },
            0
        );
        assert_ne!(control & SE_DACL_PROTECTED, 0);

        let acl = unsafe { &*dacl };
        assert_eq!(acl.AceCount, 1, "unexpected broad or inherited DACL ACE");

        let mut ace_pointer = ptr::null_mut();
        assert_ne!(unsafe { GetAce(dacl, 0, &mut ace_pointer) }, 0);
        let header = unsafe { &*(ace_pointer.cast::<ACE_HEADER>()) };
        assert_eq!(header.AceType, ACCESS_ALLOWED_ACE_TYPE);
        assert_eq!(header.AceFlags & INHERITED_ACE as u8, 0);

        let ace = unsafe { &*(ace_pointer.cast::<ACCESS_ALLOWED_ACE>()) };
        let owner_rights_sddl = "S-1-3-4\0".encode_utf16().collect::<Vec<_>>();
        let mut owner_rights = ptr::null_mut();
        assert_ne!(
            unsafe { ConvertStringSidToSidW(owner_rights_sddl.as_ptr(), &mut owner_rights) },
            0
        );
        let owner_rights = LocalAllocation(owner_rights);
        let ace_sid = ptr::addr_of!(ace.SidStart).cast_mut().cast::<c_void>();
        assert_ne!(unsafe { EqualSid(ace_sid, owner_rights.0) }, 0);
    }
}
