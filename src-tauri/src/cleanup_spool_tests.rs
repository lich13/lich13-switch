use crate::storage;
use std::{fs, path::Path};
use tempfile::tempdir;

fn private_root(parent: &Path, name: &str) -> std::path::PathBuf {
    let path = parent.join(name);
    storage::private_dir(&path).unwrap();
    path
}

fn private_file_named(parent: &Path, name: &str) -> std::path::PathBuf {
    let created = storage::private_tempfile(parent).unwrap();
    let (_, temporary_path) = created.keep().unwrap();
    let path = parent.join(name);
    fs::rename(temporary_path, &path).unwrap();
    path
}

fn old_spool(parent: &Path) -> storage::PrivateTempDir {
    storage::private_tempdir(parent, "gateway-spool-").unwrap()
}

#[test]
fn cleans_empty_and_private_file_spools_for_all_three_clients() {
    let fixture = tempdir().unwrap();
    let data = private_root(fixture.path(), "data");
    let claude = private_root(&data, "claude");
    let grok = private_root(&data, "grok");

    let codex_spool = old_spool(&data);
    let codex_file = private_file_named(codex_spool.path(), ".tmpA1b2C3");
    let claude_spool = old_spool(&claude);
    let grok_spool = old_spool(&grok);
    let grok_file = private_file_named(grok_spool.path(), ".tmpD4e5F6");

    assert_eq!(super::clean(&data), 3);
    assert!(!codex_spool.path().exists());
    assert!(!claude_spool.path().exists());
    assert!(!grok_spool.path().exists());
    assert!(!codex_file.exists());
    assert!(!grok_file.exists());
}

#[test]
fn unknown_spool_names_and_extra_entries_are_preserved() {
    let fixture = tempdir().unwrap();
    let data = private_root(fixture.path(), "data");
    let unknown_spool = private_root(&data, "gateway-spool-short");
    let unknown_spool_file = private_file_named(&unknown_spool, ".tmpG7h8I9");

    let extra_file_spool = old_spool(&data);
    let extra_file = private_file_named(extra_file_spool.path(), "keep-extra");
    let allocated_file = private_file_named(extra_file_spool.path(), ".tmpJ1k2L3");

    assert_eq!(super::clean(&data), 0);
    assert!(unknown_spool.exists());
    assert!(unknown_spool_file.exists());
    assert!(extra_file_spool.path().exists());
    assert!(extra_file.exists());
    assert!(allocated_file.exists());
}

#[test]
fn spool_with_a_child_directory_is_preserved_without_recursing() {
    let fixture = tempdir().unwrap();
    let data = private_root(fixture.path(), "data");
    let spool = old_spool(&data);
    let nested = private_root(spool.path(), "nested");
    let nested_file = private_file_named(&nested, ".tmpM4n5O6");

    assert_eq!(super::clean(&data), 0);
    assert!(spool.path().exists());
    assert!(nested.exists());
    assert!(nested_file.exists());
}

#[cfg(unix)]
#[test]
fn symlink_entries_are_preserved_without_touching_their_targets() {
    use std::os::unix::fs::symlink;

    let fixture = tempdir().unwrap();
    let data = private_root(fixture.path(), "data");
    let spool = old_spool(&data);
    let target = private_file_named(&data, "link-target");
    fs::write(&target, b"fixture link target").unwrap();
    let link = spool.path().join(".tmpP7q8R9");
    symlink(&target, &link).unwrap();

    assert_eq!(super::clean(&data), 0);
    assert!(spool.path().exists());
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read(&target).unwrap(), b"fixture link target");
}

#[cfg(unix)]
#[test]
fn symlink_data_root_is_not_followed() {
    use std::os::unix::fs::symlink;

    let fixture = tempdir().unwrap();
    let target = private_root(fixture.path(), "actual-data");
    let spool = old_spool(&target);
    let file = private_file_named(spool.path(), ".tmpH7i8J9");
    let data_link = fixture.path().join("data-link");
    symlink(&target, &data_link).unwrap();

    assert_eq!(super::clean(&data_link), 0);
    assert!(spool.path().exists());
    assert!(file.exists());
}

#[cfg(unix)]
#[test]
fn hard_link_entries_are_preserved() {
    let fixture = tempdir().unwrap();
    let data = private_root(fixture.path(), "data");
    let spool = old_spool(&data);
    let linked_path = private_file_named(spool.path(), ".tmpS1t2U3");
    fs::write(&linked_path, b"fixture hard-link target").unwrap();
    let outside_link = data.join("kept-hard-link");
    fs::hard_link(&linked_path, &outside_link).unwrap();

    assert_eq!(super::clean(&data), 0);
    assert!(spool.path().exists());
    assert_eq!(fs::read(&linked_path).unwrap(), b"fixture hard-link target");
    assert_eq!(
        fs::read(&outside_link).unwrap(),
        b"fixture hard-link target"
    );
}

#[cfg(unix)]
#[test]
fn non_private_spool_files_are_preserved() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = tempdir().unwrap();
    let data = private_root(fixture.path(), "data");
    let spool = old_spool(&data);
    let file = private_file_named(spool.path(), ".tmpV4w5X6");
    fs::write(&file, b"fixture public-mode file").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();

    assert_eq!(super::clean(&data), 0);
    assert!(spool.path().exists());
    assert_eq!(fs::read(&file).unwrap(), b"fixture public-mode file");
}

#[cfg(unix)]
#[test]
fn non_private_roots_are_not_cleaned() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = tempdir().unwrap();
    let data = private_root(fixture.path(), "data");
    let non_private_spool = old_spool(&data);
    let retained_file = private_file_named(non_private_spool.path(), ".tmpY7z8A9");
    fs::set_permissions(&data, fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(super::clean(&data), 0);
    assert!(non_private_spool.path().exists());
    assert!(retained_file.exists());
}

#[test]
fn spools_outside_the_data_root_are_not_cleaned() {
    let fixture = tempdir().unwrap();
    let data = private_root(fixture.path(), "data");
    let outside = private_root(fixture.path(), "outside");
    let outside_spool = old_spool(&outside);
    let outside_file = private_file_named(outside_spool.path(), ".tmpB1c2D3");
    let inside_spool = old_spool(&data);
    let inside_file = private_file_named(inside_spool.path(), ".tmpE4f5G6");

    assert_eq!(super::clean(&data), 1);
    assert!(!inside_spool.path().exists());
    assert!(!inside_file.exists());
    assert!(outside_spool.path().exists());
    assert!(outside_file.exists());
}

#[test]
fn gateway_api_write_and_recovery_journals_are_preserved() {
    let fixture = tempdir().unwrap();
    let data = private_root(fixture.path(), "data");
    let write_journal = data.join("gateway-api-write.json");
    let recovery_journal = data.join("gateway-recovery.json");
    fs::write(&write_journal, b"{\"fixture\":\"api-write\"}").unwrap();
    fs::write(&recovery_journal, b"{\"fixture\":\"recovery\"}").unwrap();
    let spool = old_spool(&data);

    assert_eq!(super::clean(&data), 1);
    assert_eq!(
        fs::read(&write_journal).unwrap(),
        b"{\"fixture\":\"api-write\"}"
    );
    assert_eq!(
        fs::read(&recovery_journal).unwrap(),
        b"{\"fixture\":\"recovery\"}"
    );
    assert!(!spool.path().exists());
}
