//! Best-effort cleanup after the application single-instance guard, before any
//! gateway creates its current spool. Recovery journals are never candidates.
use std::{fs, path::Path};

fn allocated_name(name: &std::ffi::OsStr, prefix: &str) -> bool {
    name.to_str()
        .and_then(|s| s.strip_prefix(prefix))
        .is_some_and(|suffix| {
            suffix.len() == 6 && suffix.bytes().all(|c| c.is_ascii_alphanumeric())
        })
}

fn owned(path: &Path, directory: bool) -> bool {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    if meta.file_type().is_symlink()
        || if directory {
            !meta.is_dir()
        } else {
            !meta.is_file()
        }
    {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o077 == 0
            && (directory || meta.nlink() == 1)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 == 0 && owned_by_current_user(path)
    }
}

fn bill_entry_name(name: &std::ffi::OsStr) -> bool {
    matches!(
        name.to_str(),
        Some("spool.sqlite3" | "spool.sqlite3-journal" | "spool.sqlite3-wal" | "spool.sqlite3-shm")
    ) || allocated_name(name, ".tmp")
}

fn clean_directory_with(path: &Path, allowed_file: fn(&std::ffi::OsStr) -> bool) -> bool {
    if !owned(path, true) {
        return false;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    let mut files = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        if !allowed_file(&entry.file_name()) || !owned(&entry.path(), false) {
            return false;
        }
        files.push(entry.path());
    }
    // Do not recurse: unexpected subdirectories, links or newly created entries
    // remain intact. A failed unlink/removal leaves the remainder for a later run.
    for file in files {
        if fs::remove_file(&file).is_err() {
            return false;
        }
    }
    fs::remove_dir(path).is_ok()
}

pub fn clean(data: &Path) -> usize {
    // Reject an untrusted root before considering child client directories: a
    // symlink in an ancestor would otherwise be followed by child metadata reads.
    if !owned(data, true) {
        return 0;
    }
    let mut removed = 0;
    for root in [
        data.to_path_buf(),
        data.join("claude"),
        data.join("grok"),
        data.join("dsh"),
        data.join("agy"),
    ] {
        if root != data && !owned(&root, true) {
            continue;
        }
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            if allocated_name(&entry.file_name(), "gateway-spool-")
                && clean_directory_with(&entry.path(), |name| allocated_name(name, ".tmp"))
            {
                removed += 1;
            }
        }
    }
    let usage = data.join("usage");
    if owned(&usage, true) {
        if let Ok(entries) = fs::read_dir(usage) {
            for entry in entries.flatten() {
                if allocated_name(&entry.file_name(), "bill-")
                    && clean_directory_with(&entry.path(), bill_entry_name)
                {
                    removed += 1;
                }
            }
        }
    }
    removed
}

#[cfg(windows)]
fn owned_by_current_user(path: &Path) -> bool {
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT},
            EqualSid, OWNER_SECURITY_INFORMATION, TOKEN_USER,
        },
    };
    let Ok(buffer) = crate::storage::current_user_token() else {
        return false;
    };
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
        let mut owner = ptr::null_mut();
        let mut descriptor = ptr::null_mut();
        let result = GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        );
        let matches = result == 0 && !owner.is_null() && EqualSid(owner, user.User.Sid) != 0;
        if !descriptor.is_null() {
            LocalFree(descriptor);
        }
        matches
    }
}

#[cfg(test)]
#[path = "cleanup_spool_tests.rs"]
mod tests;
