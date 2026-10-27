//! `trash::move_path` across file systems, where a rename can't: it copies, then removes
//! the original (#50). Needs the temp dir and the target dir on different devices (on
//! Fedora `/tmp` is a tmpfs); where they aren't, it's skipped with a note.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use gitweft_git::trash::move_path;

#[test]
fn a_folder_and_a_file_move_to_another_file_system() {
    let here = tempfile::tempdir().unwrap();
    let there = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let device = |path: &Path| std::fs::metadata(path).unwrap().dev();
    if device(here.path()) == device(there.path()) {
        eprintln!("note: the temp and target dirs share a file system; skipping this test");
        return;
    }

    let from = here.path().join("folder");
    std::fs::create_dir_all(from.join("sub")).unwrap();
    std::fs::write(from.join("sub/file.txt"), "content\n").unwrap();
    std::fs::write(from.join("run.sh"), "#!/bin/sh\n").unwrap();
    let executable = std::fs::Permissions::from_mode(0o755);
    std::fs::set_permissions(from.join("run.sh"), executable).unwrap();
    std::os::unix::fs::symlink("sub/file.txt", from.join("link")).unwrap();

    let to = there.path().join("nested/folder");
    move_path(&from, &to).unwrap();
    assert!(!from.exists(), "the original is gone");
    assert_eq!(
        std::fs::read_to_string(to.join("sub/file.txt")).unwrap(),
        "content\n"
    );
    let mode = std::fs::metadata(to.join("run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0o111, "still executable");
    assert_eq!(
        std::fs::read_link(to.join("link")).unwrap(),
        Path::new("sub/file.txt"),
        "still a symlink"
    );

    let file = here.path().join("one.txt");
    std::fs::write(&file, "1\n").unwrap();
    move_path(&file, &there.path().join("one.txt")).unwrap();
    assert!(!file.exists());
    assert_eq!(
        std::fs::read_to_string(there.path().join("one.txt")).unwrap(),
        "1\n"
    );
}
