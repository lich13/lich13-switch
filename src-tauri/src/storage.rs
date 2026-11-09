use crate::errors::code;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};
mod private;
pub use private::{private_tempdir, private_tempfile, PrivateTempDir};

pub type Result<T> = std::result::Result<T, AppError>;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppError {
    pub code: String,
    pub message: String,
    pub line: Option<usize>,
    pub column: Option<usize>,
}
impl AppError {
    pub fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            line: None,
            column: None,
        }
    }
}
impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for AppError {}
pub fn io_error(_: std::io::Error) -> AppError {
    AppError::new(code::IO, "文件操作失败，请检查路径、权限和磁盘空间")
}
pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    read_bounded(path, 2 * 1024 * 1024)
}
pub fn read_bounded(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            return Err(AppError::new(
                code::SYMLINK,
                "目标文件是符号链接，请选择实际文件所在目录",
            ))
        }
        Ok(m) if !m.is_file() || m.len() > limit => {
            return Err(AppError::new(
                code::FILE_TYPE,
                "文件不是普通文件或超过允许大小",
            ))
        }
        Ok(_) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_error(e)),
    }
    read_file_bounded(fs::File::open(path).map_err(io_error)?, limit).map(Some)
}
fn read_file_bounded(file: fs::File, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > limit {
        return Err(AppError::new(
            code::FILE_TYPE,
            "文件不是普通文件或超过允许大小",
        ));
    }
    Ok(bytes)
}
pub fn revision(bytes: Option<&[u8]>) -> String {
    bytes.map(digest).unwrap_or_else(|| "missing".into())
}
pub fn private_dir(path: &Path) -> Result<()> {
    private::create_tree(path).map_err(io_error)?;
    protect(path, true)
}
#[cfg(unix)]
pub fn protect(path: &Path, directory: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if directory { 0o700 } else { 0o600 }),
    )
    .map_err(io_error)
}
#[cfg(windows)]
pub fn protect(path: &Path, _directory: bool) -> Result<()> {
    private::windows::protect(path)
        .map_err(|_| AppError::new(code::PERMISSIONS, "无法限制文件访问权限"))
}

pub fn atomic_write(path: &Path, bytes: &[u8], expected: Option<&str>) -> Result<()> {
    atomic_write_bounded(path, bytes, expected, 2 * 1024 * 1024)
}
/// Fully write and sync private bytes before either replacement or batch publish.
pub fn stage_write(parent: &Path, bytes: &[u8], limit: u64) -> Result<tempfile::NamedTempFile> {
    if bytes.len() as u64 > limit {
        return Err(AppError::new(code::FILE_SIZE, "文件超过允许大小"));
    }
    let mut temp = private_tempfile(parent)?;
    temp.write_all(bytes).map_err(io_error)?;
    temp.as_file().sync_all().map_err(io_error)?;
    Ok(temp)
}
pub fn sync_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .ok_or_else(|| AppError::new(code::PATH, "目标路径无效"))?;
        fs::File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(io_error)?;
    }
    #[cfg(windows)]
    let _ = path;
    Ok(())
}

/// Publish an already staged batch without replacing any destination. Keep track
/// of the current destination before directory sync, which can fail after publish.
pub fn publish_batch(staged: Vec<(tempfile::NamedTempFile, PathBuf)>) -> Result<()> {
    let mut published = Vec::new();
    for (file, destination) in staged {
        let result = file
            .persist_noclobber(&destination)
            .map_err(|e| io_error(e.error))
            .and_then(|_| {
                published.push(destination.clone());
                sync_parent(&destination)
            });
        if result.is_err() {
            let mut rolled_back = true;
            for owned in published {
                // All names were created by this batch; existing exports are never removed.
                rolled_back &= fs::remove_file(&owned).is_ok();
                rolled_back &= sync_parent(&owned).is_ok();
            }
            return Err(AppError::new(
                code::ACCOUNT_EXCHANGE,
                if rolled_back {
                    "导出保存失败，已撤回本次写入"
                } else {
                    "导出保存失败，部分文件无法撤回，请保留现场"
                },
            ));
        }
    }
    Ok(())
}

/// Keep corrupt source bytes unchanged. The digest name makes retries idempotent
/// and no-clobber publication protects an existing diagnostic copy.
pub fn preserve_corrupt(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::new(code::PATH, "目标路径无效"))?;
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or_else(|| AppError::new(code::PATH, "目标路径无效"))?;
    let backup = parent.join(format!("{name}.corrupt-{}", digest(bytes)));
    let existing = || match fs::symlink_metadata(&backup) {
        Ok(m) if m.file_type().is_symlink() => Err(AppError::new(
            code::SYMLINK,
            "目标文件是符号链接，请选择实际文件所在目录",
        )),
        Ok(m) if !m.is_file() => Err(AppError::new(
            code::FILE_TYPE,
            "文件不是普通文件或超过允许大小",
        )),
        Ok(_) => protect(&backup, false).map(|_| true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io_error(e)),
    };
    if existing()? {
        return Ok(());
    }
    let temp = stage_write(parent, bytes, 2 * 1024 * 1024)?;
    match temp.persist_noclobber(&backup) {
        Ok(_) => sync_parent(&backup),
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => existing().map(|_| ()),
        Err(e) => Err(io_error(e.error)),
    }
}
pub fn atomic_write_bounded(
    path: &Path,
    bytes: &[u8],
    expected: Option<&str>,
    limit: u64,
) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::new(code::PATH, "目标路径无效"))?;
    if !parent.exists() {
        private_dir(parent)?;
    }
    if let Some(expected) = expected {
        let current = read_bounded(path, limit)?;
        if expected != revision(current.as_deref()) {
            return Err(AppError::new(
                code::CONFLICT,
                "文件已被其他程序修改，请重新读取后再操作",
            ));
        }
    } else {
        match fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(AppError::new(
                    code::SYMLINK,
                    "目标文件是符号链接，请选择实际文件所在目录",
                ))
            }
            Ok(m) if !m.is_file() => {
                return Err(AppError::new(
                    code::FILE_TYPE,
                    "文件不是普通文件或超过允许大小",
                ))
            }
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(io_error(e)),
        }
    }
    let temp = stage_write(parent, bytes, limit)?;
    temp.persist(path).map_err(|e| io_error(e.error))?;
    sync_parent(path)
}

/// Replace an unbounded journal without holding its serialized contents in RAM.
/// Configuration transactions continue to use the bounded CAS writer above.
pub fn atomic_write_stream(
    path: &Path,
    write: impl FnOnce(&mut dyn Write) -> std::io::Result<()>,
) -> Result<()> {
    use std::io::BufWriter;
    let parent = path
        .parent()
        .ok_or_else(|| AppError::new(code::PATH, "目标路径无效"))?;
    if !parent.exists() {
        private_dir(parent)?;
    }
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Err(AppError::new(
            code::SYMLINK,
            "目标文件是符号链接，请选择实际文件所在目录",
        )),
        Ok(m) if !m.is_file() => Err(AppError::new(
            code::FILE_TYPE,
            "文件不是普通文件或超过允许大小",
        )),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_error(e)),
    }?;
    let mut temp = private_tempfile(parent)?;
    {
        let mut writer = BufWriter::new(temp.as_file_mut());
        write(&mut writer).map_err(io_error)?;
        writer.flush().map_err(io_error)?;
    }
    temp.as_file().sync_all().map_err(io_error)?;
    temp.persist(path).map_err(|e| io_error(e.error))?;
    sync_parent(path)
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
