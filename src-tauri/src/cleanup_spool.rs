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
    fs::symlink_metadata(path).is_ok_and(|meta| {
        !meta.file_type().is_symlink()
            && if directory {
                meta.is_dir()
            } else {
                meta.is_file()
            }
    })
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
    // A symlinked root is not followed.
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

#[cfg(test)]
#[path = "cleanup_spool_tests.rs"]
mod tests;
