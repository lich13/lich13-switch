use super::{io_error, Result};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

// Set access restrictions in the creation syscall, before a path is visible or
// any secret bytes are written. Existing objects are tightened by `protect`.
#[cfg(unix)]
fn create_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(windows)]
fn create_directory(path: &Path) -> io::Result<()> {
    windows::create_directory(path)
}

pub(super) fn create_tree(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        create_tree(parent)?;
    }
    match create_directory(path) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        result => result,
    }
}

pub fn private_tempfile(parent: &Path) -> Result<tempfile::NamedTempFile> {
    #[cfg(unix)]
    let file = tempfile::NamedTempFile::new_in(parent);
    #[cfg(windows)]
    let file = tempfile::Builder::new().make_in(parent, windows::create_file);
    file.map_err(io_error)
}

// tempfile does not accept SECURITY_ATTRIBUTES for directories on Windows.
// Retain its random-name allocation, but create the directory ourselves and
// give it the same owned cleanup lifetime as TempDir.
pub struct PrivateTempDir {
    path: Option<PathBuf>,
}
impl PrivateTempDir {
    pub fn path(&self) -> &Path {
        self.path.as_deref().expect("live temporary directory")
    }
    pub fn close(mut self) -> io::Result<()> {
        fs::remove_dir_all(self.path.take().expect("live temporary directory"))
    }
}
impl Drop for PrivateTempDir {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_dir_all(path);
        }
    }
}
pub fn private_tempdir(parent: &Path, prefix: &str) -> Result<PrivateTempDir> {
    let created = tempfile::Builder::new()
        .prefix(prefix)
        .make_in(parent, |path| {
            create_directory(path)?;
            Ok(path.to_path_buf())
        })
        .map_err(io_error)?;
    let (path, mut cleanup) = created.into_parts();
    cleanup.disable_cleanup(true);
    Ok(PrivateTempDir { path: Some(path) })
}

#[cfg(windows)]
pub(super) mod windows {
    use std::{
        fs::File,
        io,
        os::windows::{ffi::OsStrExt, io::FromRawHandle},
        path::Path,
        ptr,
    };
    use windows_sys::Win32::{
        Foundation::{CloseHandle, LocalFree, INVALID_HANDLE_VALUE},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            },
            GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        },
        Storage::FileSystem::{
            CreateDirectoryW, CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_TEMPORARY, FILE_GENERIC_READ,
            FILE_GENERIC_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    fn creation_sddl() -> io::Result<Vec<u16>> {
        unsafe {
            let mut token = ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut length = 0;
            GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut length);
            if length < std::mem::size_of::<TOKEN_USER>() as u32 || length > 65536 {
                CloseHandle(token);
                return Err(io::Error::other("invalid process user information"));
            }
            // TOKEN_USER and its SID require aligned storage until conversion ends.
            let mut buffer = vec![0usize; (length as usize).div_ceil(std::mem::size_of::<usize>())];
            let read = GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                length,
                &mut length,
            );
            let error = io::Error::last_os_error();
            CloseHandle(token);
            if read == 0 {
                return Err(error);
            }
            let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
            let mut sid = ptr::null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut sid) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut sddl: Vec<u16> = "O:".encode_utf16().collect();
            let mut cursor = sid;
            while *cursor != 0 {
                sddl.push(*cursor);
                cursor = cursor.add(1);
            }
            LocalFree(sid.cast());
            // Elevated tokens may default new objects to a group owner. Set the
            // actual user explicitly so OWNER RIGHTS never grants that group access.
            sddl.extend("D:P(A;OICI;FA;;;OW)\0".encode_utf16());
            Ok(sddl)
        }
    }

    pub(super) struct Descriptor(pub *mut std::ffi::c_void);
    impl Descriptor {
        pub(super) fn new() -> io::Result<Self> {
            let sddl = creation_sddl()?;
            let mut descriptor = ptr::null_mut();
            if unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    1,
                    &mut descriptor,
                    ptr::null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(descriptor))
        }
        fn attributes(&self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.0,
                bInheritHandle: 0,
            }
        }
    }
    impl Drop for Descriptor {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    fn name(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }
    pub(super) fn create_directory(path: &Path) -> io::Result<()> {
        let descriptor = Descriptor::new()?;
        if unsafe { CreateDirectoryW(name(path).as_ptr(), &descriptor.attributes()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    pub(super) fn create_file(path: &Path) -> io::Result<File> {
        let descriptor = Descriptor::new()?;
        let handle = unsafe {
            CreateFileW(
                name(path).as_ptr(),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                &descriptor.attributes(),
                CREATE_NEW,
                FILE_ATTRIBUTE_TEMPORARY,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_handle(handle) })
    }
}

#[cfg(test)]
#[path = "private_tests.rs"]
mod tests;
