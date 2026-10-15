//! A process-local cache for the polled auth file. A weak or failing metadata
//! probe always falls back to the existing bounded read, never to stale bytes.
use crate::storage::{self, Result};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Copy, PartialEq, Eq)]
struct Fingerprint([u64; 8]);

#[cfg(unix)]
fn fingerprint(path: &Path) -> Option<Fingerprint> {
    use std::os::unix::fs::MetadataExt;
    let m = fs::symlink_metadata(path).ok()?;
    if !m.is_file()
        || m.len() > 2 * 1024 * 1024
        || m.ino() == 0
        || (m.mtime_nsec() == 0 && m.ctime_nsec() == 0)
    {
        return None;
    }
    Some(Fingerprint([
        m.dev(),
        m.ino(),
        m.size(),
        m.mtime() as u64,
        m.mtime_nsec() as u64,
        m.ctime() as u64,
        m.ctime_nsec() as u64,
        m.mode() as u64,
    ]))
}

#[cfg(windows)]
fn fingerprint(path: &Path) -> Option<Fingerprint> {
    use std::os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FileBasicInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
        BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO, FILE_FLAG_OPEN_REPARSE_POINT,
    };
    let m = fs::symlink_metadata(path).ok()?;
    if !m.is_file() || m.len() > 2 * 1024 * 1024 || m.file_attributes() & 0x400 != 0 {
        return None;
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .ok()?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    let mut basic = FILE_BASIC_INFO::default();
    unsafe {
        if GetFileInformationByHandle(file.as_raw_handle(), &mut info) == 0
            || GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileBasicInfo,
                (&mut basic as *mut FILE_BASIC_INFO).cast(),
                std::mem::size_of_val(&basic) as u32,
            ) == 0
        {
            return None;
        }
    }
    let id = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    if id == 0
        || basic.ChangeTime == 0
        || basic.LastWriteTime == 0
        || basic.FileAttributes & 0x400 != 0
    {
        return None;
    }
    Some(Fingerprint([
        info.dwVolumeSerialNumber as u64,
        id,
        (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
        basic.LastWriteTime as u64,
        basic.ChangeTime as u64,
        basic.CreationTime as u64,
        basic.FileAttributes as u64,
        info.nNumberOfLinks as u64,
    ]))
}

pub struct Cache<T> {
    entry: Option<(PathBuf, Fingerprint, Arc<T>)>,
}
impl<T> Default for Cache<T> {
    fn default() -> Self {
        Self { entry: None }
    }
}
impl<T> Cache<T> {
    pub fn clear(&mut self) {
        self.entry = None;
    }
    pub fn read(
        &mut self,
        path: &Path,
        parse: impl FnOnce(Option<Vec<u8>>) -> T,
    ) -> Result<Arc<T>> {
        let before = fingerprint(path);
        if let (Some(stamp), Some((saved_path, saved_stamp, value))) = (before, self.entry.as_ref())
        {
            if path == saved_path && stamp == *saved_stamp {
                return Ok(value.clone());
            }
        }
        self.clear();
        let bytes = storage::read_optional(path)?;
        let after = fingerprint(path);
        let value = Arc::new(parse(bytes));
        if let Some(stamp) = before.filter(|v| Some(*v) == after) {
            self.entry = Some((path.to_owned(), stamp, value.clone()));
        }
        Ok(value)
    }
}

#[cfg(test)]
#[path = "auth_cache_tests.rs"]
mod tests;
