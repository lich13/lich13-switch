use super::*;
use std::fs;

#[test]
fn publish_batch_rolls_back_earlier_files_when_a_destination_exists() {
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first.fixture");
    let occupied = temp.path().join("occupied.fixture");
    fs::write(&occupied, b"existing bytes").unwrap();

    let staged = vec![
        (
            stage_write(temp.path(), b"first bytes", 64).unwrap(),
            first.clone(),
        ),
        (
            stage_write(temp.path(), b"replacement bytes", 64).unwrap(),
            occupied.clone(),
        ),
    ];

    let error = publish_batch(staged).unwrap_err();

    assert_eq!(error.code, code::ACCOUNT_EXCHANGE);
    assert!(
        !first.exists(),
        "the earlier batch write must be rolled back"
    );
    assert_eq!(fs::read(occupied).unwrap(), b"existing bytes");
}

#[test]
fn publish_batch_writes_every_staged_payload() {
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first.fixture");
    let second = temp.path().join("second.fixture");
    let staged = vec![
        (
            stage_write(temp.path(), b"first payload", 64).unwrap(),
            first.clone(),
        ),
        (
            stage_write(temp.path(), b"second payload", 64).unwrap(),
            second.clone(),
        ),
    ];

    publish_batch(staged).unwrap();

    assert_eq!(fs::read(first).unwrap(), b"first payload");
    assert_eq!(fs::read(second).unwrap(), b"second payload");
}

#[test]
fn stage_size_and_revision_conflicts_leave_existing_target_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target.fixture");
    fs::write(&target, b"original target").unwrap();

    let size_error = stage_write(temp.path(), b"too large", 2).unwrap_err();
    assert_eq!(size_error.code, code::FILE_SIZE);
    assert_eq!(fs::read(&target).unwrap(), b"original target");

    let conflict = atomic_write(&target, b"new target", Some("stale-version")).unwrap_err();
    assert_eq!(conflict.code, code::CONFLICT);
    assert_eq!(fs::read(&target).unwrap(), b"original target");
}

#[test]
fn preserve_corrupt_keeps_original_and_deduplicates_by_digest() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("settings.fixture");
    let bytes = b"synthetic invalid settings";
    fs::write(&source, bytes).unwrap();
    let backup = temp
        .path()
        .join(format!("settings.fixture.corrupt-{}", digest(bytes)));

    preserve_corrupt(&source, bytes).unwrap();
    preserve_corrupt(&source, bytes).unwrap();

    assert_eq!(fs::read(&source).unwrap(), bytes);
    assert_eq!(fs::read(&backup).unwrap(), bytes);
    assert_eq!(
        fs::read_dir(temp.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with("settings.fixture.corrupt-"))
            .count(),
        1
    );
}

#[test]
fn preserve_corrupt_reports_changed_backup_without_replacing_original() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("settings.fixture");
    let bytes = b"synthetic invalid settings";
    fs::write(&source, bytes).unwrap();
    let backup = temp
        .path()
        .join(format!("settings.fixture.corrupt-{}", digest(bytes)));
    fs::write(&backup, b"different fixture bytes").unwrap();

    let error = preserve_corrupt(&source, bytes).unwrap_err();

    assert_eq!(error.code, code::VERIFY);
    assert_eq!(fs::read(&source).unwrap(), bytes);
    assert_eq!(fs::read(backup).unwrap(), b"different fixture bytes");
}

#[cfg(unix)]
#[test]
fn preserve_corrupt_does_not_follow_backup_symlink() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("settings.fixture");
    let bytes = b"synthetic invalid settings";
    let target = temp.path().join("symlink-target.fixture");
    let backup = temp
        .path()
        .join(format!("settings.fixture.corrupt-{}", digest(bytes)));
    fs::write(&source, bytes).unwrap();
    fs::write(&target, b"symlink target fixture").unwrap();
    symlink(&target, &backup).unwrap();

    let error = preserve_corrupt(&source, bytes).unwrap_err();

    assert_eq!(error.code, code::SYMLINK);
    assert!(fs::symlink_metadata(&backup)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read(&target).unwrap(), b"symlink target fixture");
    assert_eq!(fs::read(&source).unwrap(), bytes);
}

#[test]
fn streamed_atomic_write_keeps_previous_bytes_on_staging_failure_and_private_permissions() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("journal.fixture");
    for point in [
        faults::Point::ShortWrite,
        faults::Point::FileSync,
        faults::Point::Replace,
    ] {
        fs::write(&path, b"previous").unwrap();
        let _fault = faults::inject(point);
        assert!(atomic_write_stream(&path, |writer| writer.write_all(b"replacement")).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"previous");
    }
    atomic_write_stream(&path, |writer| writer.write_all(b"replacement")).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"replacement");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let link = temp.path().join("link.fixture");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert_eq!(
            atomic_write_stream(&link, |writer| writer.write_all(b"bad"))
                .unwrap_err()
                .code,
            code::SYMLINK
        );
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
    }
}
