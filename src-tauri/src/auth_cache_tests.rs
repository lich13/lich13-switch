use super::{Cache, Result};
use std::{fs, path::Path, sync::Arc, time::Duration};

fn read_fixture(
    cache: &mut Cache<Option<String>>,
    path: &Path,
    parses: &mut usize,
) -> Result<Arc<Option<String>>> {
    cache.read(path, |bytes| {
        *parses += 1;
        bytes.map(|value| String::from_utf8(value).expect("fixture is UTF-8"))
    })
}

fn assert_fixture(value: &Arc<Option<String>>, expected: Option<&str>) {
    assert_eq!(value.as_ref().as_deref(), expected);
}

#[test]
fn unchanged_file_reuses_the_parsed_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.fixture");
    fs::write(&path, b"fixture-a").unwrap();
    let mut cache = Cache::default();
    let mut parses = 0;

    let first = read_fixture(&mut cache, &path, &mut parses).unwrap();
    let second = read_fixture(&mut cache, &path, &mut parses).unwrap();

    assert_fixture(&first, Some("fixture-a"));
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(parses, 1, "an unchanged file should be parsed once");
}

#[test]
fn same_size_rewrite_invalidates_the_cached_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.fixture");
    fs::write(&path, b"fixture-a").unwrap();
    let mut cache = Cache::default();
    let mut parses = 0;

    let first = read_fixture(&mut cache, &path, &mut parses).unwrap();
    let original_modified = fs::metadata(&path).unwrap().modified().unwrap();
    fs::write(&path, b"fixture-b").unwrap();
    assert_eq!(fs::metadata(&path).unwrap().len(), 9);
    let changed_modified = original_modified + Duration::from_secs(5);
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(changed_modified))
        .unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().modified().unwrap(),
        changed_modified
    );
    let second = read_fixture(&mut cache, &path, &mut parses).unwrap();

    assert_fixture(&first, Some("fixture-a"));
    assert_fixture(&second, Some("fixture-b"));
    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(parses, 2, "a same-size rewrite should be parsed again");
}

#[cfg(any(unix, windows))]
#[test]
fn changed_content_with_restored_mtime_is_detected_by_change_time() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.fixture");
    fs::write(&path, b"fixture-a").unwrap();
    let original_modified = fs::metadata(&path).unwrap().modified().unwrap();
    let original_fingerprint = super::fingerprint(&path).expect("fixture metadata is cacheable");
    let mut cache = Cache::default();
    let mut parses = 0;

    let first = read_fixture(&mut cache, &path, &mut parses).unwrap();
    assert!(
        cache.entry.is_some(),
        "the initial fixture should be cached"
    );
    std::thread::sleep(Duration::from_millis(1_100));
    fs::write(&path, b"fixture-b").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(original_modified))
        .unwrap();

    assert_eq!(
        fs::metadata(&path).unwrap().modified().unwrap(),
        original_modified,
        "the fixture modification time must be restored exactly"
    );
    assert!(
        super::fingerprint(&path) != Some(original_fingerprint),
        "the change-time fingerprint should notice the rewrite"
    );
    let second = read_fixture(&mut cache, &path, &mut parses).unwrap();

    assert_fixture(&first, Some("fixture-a"));
    assert_fixture(&second, Some("fixture-b"));
    assert_eq!(parses, 2, "restoring mtime must not hide a content change");
}

#[test]
fn atomic_replacement_invalidates_the_cached_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.fixture");
    fs::write(&path, b"fixture-a").unwrap();
    let mut cache = Cache::default();
    let mut parses = 0;

    let first = read_fixture(&mut cache, &path, &mut parses).unwrap();
    let replacement = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
    fs::write(replacement.path(), b"fixture-b").unwrap();
    replacement.persist(&path).unwrap();
    let second = read_fixture(&mut cache, &path, &mut parses).unwrap();

    assert_fixture(&first, Some("fixture-a"));
    assert_fixture(&second, Some("fixture-b"));
    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(parses, 2, "an atomic replacement should be parsed again");
}

#[test]
fn deleting_and_recreating_a_file_does_not_reuse_the_old_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.fixture");
    fs::write(&path, b"fixture-a").unwrap();
    let mut cache = Cache::default();
    let mut parses = 0;

    let first = read_fixture(&mut cache, &path, &mut parses).unwrap();
    fs::remove_file(&path).unwrap();
    let missing = read_fixture(&mut cache, &path, &mut parses).unwrap();
    fs::write(&path, b"fixture-b").unwrap();
    let recreated = read_fixture(&mut cache, &path, &mut parses).unwrap();

    assert_fixture(&first, Some("fixture-a"));
    assert_fixture(&missing, None);
    assert_fixture(&recreated, Some("fixture-b"));
    assert_eq!(
        parses, 3,
        "missing and recreated states should each be read"
    );
}

#[test]
fn switching_root_directories_does_not_reuse_the_other_path_value() {
    let dir = tempfile::tempdir().unwrap();
    let first_root = dir.path().join("root-a");
    let second_root = dir.path().join("root-b");
    fs::create_dir(&first_root).unwrap();
    fs::create_dir(&second_root).unwrap();
    let first_path = first_root.join("auth.fixture");
    let second_path = second_root.join("auth.fixture");
    fs::write(&first_path, b"fixture-a").unwrap();
    fs::write(&second_path, b"fixture-b").unwrap();
    let mut cache = Cache::default();
    let mut parses = 0;

    let first = read_fixture(&mut cache, &first_path, &mut parses).unwrap();
    let second = read_fixture(&mut cache, &second_path, &mut parses).unwrap();

    assert_fixture(&first, Some("fixture-a"));
    assert_fixture(&second, Some("fixture-b"));
    assert_eq!(
        parses, 2,
        "a different root path should be parsed separately"
    );
}

#[test]
fn missing_metadata_falls_back_to_a_read_without_caching_absence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.fixture");
    let mut cache = Cache::default();
    let mut parses = 0;

    let first_missing = read_fixture(&mut cache, &path, &mut parses).unwrap();
    let second_missing = read_fixture(&mut cache, &path, &mut parses).unwrap();
    fs::write(&path, b"fixture-a").unwrap();
    let created = read_fixture(&mut cache, &path, &mut parses).unwrap();

    assert_fixture(&first_missing, None);
    assert_fixture(&second_missing, None);
    assert_fixture(&created, Some("fixture-a"));
    assert_eq!(
        parses, 3,
        "an un-fingerprinted absence should not be cached"
    );
}

#[test]
fn non_regular_metadata_does_not_return_a_cached_file_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.fixture");
    fs::write(&path, b"fixture-a").unwrap();
    let mut cache = Cache::default();
    let mut parses = 0;

    let first = read_fixture(&mut cache, &path, &mut parses).unwrap();
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    let rejected = read_fixture(&mut cache, &path, &mut parses);
    fs::remove_dir(&path).unwrap();
    fs::write(&path, b"fixture-b").unwrap();
    let recreated = read_fixture(&mut cache, &path, &mut parses).unwrap();

    assert!(
        rejected.is_err(),
        "the bounded reader should reject a directory"
    );
    assert_fixture(&first, Some("fixture-a"));
    assert_fixture(&recreated, Some("fixture-b"));
    assert_eq!(parses, 2, "only regular-file reads should reach the parser");
}

#[cfg(unix)]
#[test]
fn symlink_metadata_falls_back_to_the_bounded_reader() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.fixture");
    let target = dir.path().join("target.fixture");
    fs::write(&path, b"fixture-a").unwrap();
    fs::write(&target, b"fixture-target").unwrap();
    let mut cache = Cache::default();
    let mut parses = 0;

    let first = read_fixture(&mut cache, &path, &mut parses).unwrap();
    fs::remove_file(&path).unwrap();
    symlink(&target, &path).unwrap();
    let rejected = read_fixture(&mut cache, &path, &mut parses);
    fs::remove_file(&path).unwrap();
    fs::write(&path, b"fixture-b").unwrap();
    let recreated = read_fixture(&mut cache, &path, &mut parses).unwrap();

    assert!(
        rejected.is_err(),
        "the bounded reader should reject a symlink"
    );
    assert_fixture(&first, Some("fixture-a"));
    assert_fixture(&recreated, Some("fixture-b"));
    assert_eq!(parses, 2, "a symlink must never reach the parser");
}

#[cfg(unix)]
#[test]
fn unreadable_file_error_does_not_return_a_cached_value() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.fixture");
    fs::write(&path, b"fixture-a").unwrap();
    let mut cache = Cache::default();
    let mut parses = 0;

    let first = read_fixture(&mut cache, &path, &mut parses).unwrap();
    let original_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&path).is_ok() {
        fs::set_permissions(&path, fs::Permissions::from_mode(original_mode)).unwrap();
        return;
    }

    let rejected = read_fixture(&mut cache, &path, &mut parses);
    fs::set_permissions(&path, fs::Permissions::from_mode(original_mode)).unwrap();
    fs::write(&path, b"fixture-b").unwrap();
    let recovered = read_fixture(&mut cache, &path, &mut parses).unwrap();

    assert!(
        rejected.is_err(),
        "the bounded reader should propagate read failure"
    );
    assert_fixture(&first, Some("fixture-a"));
    assert_fixture(&recovered, Some("fixture-b"));
    assert_eq!(parses, 2, "a failed read must not reach the parser");
}
