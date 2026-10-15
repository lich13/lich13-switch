use super::{private_tempdir, private_tempfile};
use crate::storage::{atomic_write, private_dir};
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

#[cfg(windows)]
mod windows_acl {
    use std::{ffi::c_void, os::windows::ffi::OsStrExt, path::Path, ptr, slice};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Authorization::{ConvertStringSidToSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT},
        Security::{
            EqualSid, GetAce, GetSecurityDescriptorControl, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
            DACL_SECURITY_INFORMATION, INHERITED_ACE, SE_DACL_PROTECTED,
        },
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

    fn security_descriptor(path: &Path) -> (LocalAllocation, *mut ACL) {
        let name = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let mut dacl = ptr::null_mut();
        let mut descriptor = ptr::null_mut();
        let result = unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        assert_eq!(result, 0);
        (LocalAllocation(descriptor), dacl)
    }

    pub(super) fn snapshot(path: &Path) -> (u16, Vec<u8>) {
        let (descriptor, dacl) = security_descriptor(path);
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
        let (descriptor, dacl) = security_descriptor(path);
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
