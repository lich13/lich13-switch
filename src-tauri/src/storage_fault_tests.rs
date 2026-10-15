use super::*;
use std::fs;

fn assert_only_fixture_files_remain(directory: &Path, expected: &[&str]) {
    let mut names: Vec<String> = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();

    let mut expected: Vec<String> = expected.iter().map(|name| (*name).to_owned()).collect();
    expected.sort();
    assert_eq!(names, expected, "unexpected temporary files remain");
}

fn assert_atomic_write_failure_keeps_old_file(point: faults::Point) {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target.fixture");
    fs::write(&target, b"original fixture bytes").unwrap();

    let _fault = faults::inject(point);
    let error = atomic_write(&target, b"replacement fixture bytes", None).unwrap_err();

    assert_eq!(error.code, code::IO);
    assert_eq!(fs::read(&target).unwrap(), b"original fixture bytes");
    assert_only_fixture_files_remain(temp.path(), &["target.fixture"]);
}

#[test]
fn atomic_write_short_write_keeps_old_file_and_cleans_staging_file() {
    assert_atomic_write_failure_keeps_old_file(faults::Point::ShortWrite);
}

#[test]
fn atomic_write_file_sync_failure_keeps_old_file_and_cleans_staging_file() {
    assert_atomic_write_failure_keeps_old_file(faults::Point::FileSync);
}

#[test]
fn atomic_write_replace_failure_keeps_old_file_and_cleans_staging_file() {
    assert_atomic_write_failure_keeps_old_file(faults::Point::Replace);
}

#[test]
fn atomic_write_directory_sync_failure_reports_error_after_complete_publish() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target.fixture");
    let replacement = b"complete replacement fixture";
    fs::write(&target, b"original fixture bytes").unwrap();

    let _fault = faults::inject(faults::Point::DirectorySync);
    let error = atomic_write(&target, replacement, None).unwrap_err();

    assert_eq!(error.code, code::IO);
    assert_eq!(fs::read(&target).unwrap(), replacement);
    assert_only_fixture_files_remain(temp.path(), &["target.fixture"]);

    let current_revision = revision(Some(replacement));
    atomic_write(&target, replacement, Some(&current_revision)).unwrap();
    assert_eq!(fs::read(&target).unwrap(), replacement);
    assert_only_fixture_files_remain(temp.path(), &["target.fixture"]);
}

#[test]
fn publish_batch_directory_sync_failure_removes_batch_files_and_preserves_existing_file() {
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first.fixture");
    let second = temp.path().join("second.fixture");
    let existing = temp.path().join("existing.fixture");
    fs::write(&existing, b"existing fixture bytes").unwrap();
    let staged = vec![
        (
            stage_write(temp.path(), b"first fixture bytes", 64).unwrap(),
            first.clone(),
        ),
        (
            stage_write(temp.path(), b"second fixture bytes", 64).unwrap(),
            second.clone(),
        ),
    ];

    let _fault = faults::inject(faults::Point::DirectorySync);
    let error = publish_batch(staged).unwrap_err();

    assert_eq!(error.code, code::ACCOUNT_EXCHANGE);
    assert!(!first.exists());
    assert!(!second.exists());
    assert_eq!(fs::read(&existing).unwrap(), b"existing fixture bytes");
    assert_only_fixture_files_remain(temp.path(), &["existing.fixture"]);
}

#[test]
fn atomic_write_expected_revision_conflict_keeps_old_file_and_cleans_staging_file() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target.fixture");
    fs::write(&target, b"current fixture bytes").unwrap();

    let stale_revision = revision(Some(b"stale fixture bytes"));
    let error =
        atomic_write(&target, b"replacement fixture bytes", Some(&stale_revision)).unwrap_err();

    assert_eq!(error.code, code::CONFLICT);
    assert_eq!(fs::read(&target).unwrap(), b"current fixture bytes");
    assert_only_fixture_files_remain(temp.path(), &["target.fixture"]);
}

#[cfg(windows)]
#[test]
fn atomic_write_fails_when_target_is_open_without_windows_sharing() {
    use std::os::windows::fs::OpenOptionsExt;

    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target.fixture");
    fs::write(&target, b"locked fixture bytes").unwrap();
    let locked = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&target)
        .unwrap();

    let error = atomic_write(&target, b"replacement fixture bytes", None).unwrap_err();
    drop(locked);

    assert_eq!(error.code, code::IO);
    assert_eq!(fs::read(&target).unwrap(), b"locked fixture bytes");
    assert_only_fixture_files_remain(temp.path(), &["target.fixture"]);
}
