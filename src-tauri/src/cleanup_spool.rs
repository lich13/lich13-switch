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

fn clean_directory(path: &Path) -> bool {
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
        if !allocated_name(&entry.file_name(), ".tmp") || !owned(&entry.path(), false) {
            return false;
        }
        files.push(entry.path());
    }
    // Do not recurse: unexpected subdirectories, links or newly created entries
    // remain intact. A failed unlink/removal leaves the remainder for a later run.
    for file in files {
        if !owned(&file, false) || fs::remove_file(&file).is_err() {
            return false;
        }
    }
    owned(path, true) && fs::remove_dir(path).is_ok()
}

pub fn clean(data: &Path) -> usize {
    // Reject an untrusted root before considering child client directories: a
    // symlink in an ancestor would otherwise be followed by child metadata reads.
    if !owned(data, true) {
        return 0;
    }
    let mut removed = 0;
    for root in [data.to_path_buf(), data.join("claude"), data.join("grok")] {
        if !owned(&root, true) {
            continue;
        }
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            if allocated_name(&entry.file_name(), "gateway-spool-")
                && clean_directory(&entry.path())
            {
                removed += 1;
            }
        }
    }
    removed
}

#[cfg(windows)]
fn owned_by_current_user(path: &Path) -> bool {
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, LocalFree},
        Security::{
            Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT},
            EqualSid, GetTokenInformation, TokenUser, OWNER_SECURITY_INFORMATION, TOKEN_QUERY,
            TOKEN_USER,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let mut token = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut length = 0;
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut length);
        if length == 0 || length > 65536 {
            CloseHandle(token);
            return false;
        }
        // Word storage provides the alignment required by TOKEN_USER and its SID.
        let mut buffer = vec![0usize; (length as usize).div_ceil(std::mem::size_of::<usize>())];
        let read = GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            length,
            &mut length,
        );
        CloseHandle(token);
        if read == 0 {
            return false;
        }
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
