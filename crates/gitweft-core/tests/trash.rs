use std::path::{Path, PathBuf};
use std::time::Duration;

use gitweft_core::trash::{self, INDEX_FILE, TrashAction, TrashError, TrashFileKind};
use gitweft_git::CancelToken;
use gitweft_git::gix::bstr::BString;
use gitweft_git::ops::{clean_paths, discard_paths};
use gitweft_platform::bytes::path_from_bytes;
use gitweft_testkit::Sandbox;

fn c() -> CancelToken {
    CancelToken::new()
}

fn path(base: &Path, name: &[u8]) -> PathBuf {
    base.join(path_from_bytes(name))
}

fn write(base: &Path, name: &[u8], content: &[u8]) {
    let file = path(base, name);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, content).unwrap();
}

fn read(base: &Path, name: &[u8]) -> Vec<u8> {
    std::fs::read(path(base, name)).unwrap()
}

fn bs(name: &[u8]) -> BString {
    BString::from(name)
}

/// A repository at `dir` (bytes, under the sandbox) with `keep.txt` committed.
fn repo_at(sb: &Sandbox, dir: &[u8]) -> PathBuf {
    let repo = path(sb.path(), dir);
    std::fs::create_dir_all(&repo).unwrap();
    sb.run(&repo, &["init", "-q", "-b", "main"]);
    write(&repo, b"keep.txt", b"committed\n");
    sb.run(&repo, &["add", "."]);
    sb.run(&repo, &["commit", "-qm", "base"]);
    repo
}

#[test]
fn a_discard_is_recorded_listed_and_restored_byte_for_byte() {
    let sb = Sandbox::new();
    let repo = repo_at(&sb, b"r");
    let trash_root = sb.path().join("trash");
    let edit = b"precious \xff edit\n";
    write(&repo, b"keep.txt", edit);
    // Only Unix has an executable bit to keep.
    #[cfg(unix)]
    gitweft_platform::fs::set_executable(&repo.join("keep.txt"), true).unwrap();
    let scratch = [0u8, 1, 2, 255, b'\n'];
    write(&repo, b"dir/scratch.bin", &scratch);

    let backup = discard_paths(
        sb.git(),
        &repo,
        &[bs(b"keep.txt")],
        &[bs(b"dir/scratch.bin")],
        &trash_root,
        &c(),
    )
    .unwrap();
    let dir = backup.trash_dir.clone().unwrap();
    assert!(dir.join(INDEX_FILE).is_file());
    assert_eq!(read(&dir, b"files/dir/scratch.bin"), scratch);
    assert_eq!(read(&repo, b"keep.txt"), b"committed\n");

    let entries = trash::list(&trash_root);
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.dir, dir);
    assert!(!entry.legacy);
    assert_eq!(entry.action, TrashAction::Discard);
    assert_eq!(entry.root.as_deref(), Some(repo.as_path()));
    assert!(entry.size_bytes > 0);
    let blob = backup.blobs[0].1;
    assert_eq!(entry.files.len(), 2);
    assert_eq!(entry.files[0].path, "keep.txt");
    assert!(matches!(
        entry.files[0].kind,
        TrashFileKind::Tracked { blob: b, left: Some(_), executable, symlink: false }
            if b == blob && executable == cfg!(unix)
    ));
    assert_eq!(entry.files[1].path, "dir/scratch.bin");
    assert_eq!(entry.files[1].kind, TrashFileKind::Untracked);
    assert_eq!(trash::list_for_repo(&trash_root, &repo).len(), 1);
    assert!(trash::list_for_repo(&trash_root, sb.path()).is_empty());

    // One file at a time: the record keeps the other.
    let report = trash::restore(sb.git(), entry, &[bs(b"keep.txt")], &c()).unwrap();
    assert_eq!(report.restored, [bs(b"keep.txt")]);
    assert!(report.conflicts.is_empty() && report.failed.is_empty());
    assert!(!report.emptied);
    assert_eq!(read(&repo, b"keep.txt"), edit);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = std::fs::metadata(repo.join("keep.txt"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111);
    }
    let entry = trash::list(&trash_root).remove(0);
    assert_eq!(entry.files.len(), 1);

    let report = trash::restore(sb.git(), &entry, &[bs(b"dir/scratch.bin")], &c()).unwrap();
    assert_eq!(report.restored, [bs(b"dir/scratch.bin")]);
    assert!(report.emptied);
    assert_eq!(read(&repo, b"dir/scratch.bin"), scratch);
    assert!(!dir.exists());
    assert!(trash::list(&trash_root).is_empty());
}

/// A tracked symlink's discarded version is its target text (as git stores a link), a
/// dangling one included, and a restore makes it a symlink again.
#[cfg(unix)]
#[test]
fn a_discarded_symlink_comes_back_as_a_symlink() {
    use std::os::unix::fs::symlink;
    let sb = Sandbox::new();
    let repo = repo_at(&sb, b"r");
    let trash_root = sb.path().join("trash");
    symlink("keep.txt", repo.join("link")).unwrap();
    symlink("missing", repo.join("dangling")).unwrap();
    sb.run(&repo, &["add", "link", "dangling"]);
    sb.run(&repo, &["commit", "-qm", "links"]);
    // Both retargeted: other.txt (untracked) exists, gone doesn't.
    write(&repo, b"other.txt", b"other\n");
    for (name, target) in [("link", "other.txt"), ("dangling", "gone")] {
        std::fs::remove_file(repo.join(name)).unwrap();
        symlink(target, repo.join(name)).unwrap();
    }

    let tracked = [bs(b"link"), bs(b"dangling")];
    let backup = discard_paths(sb.git(), &repo, &tracked, &[], &trash_root, &c()).unwrap();
    // The discard put the committed links back…
    assert_eq!(
        std::fs::read_link(repo.join("link")).unwrap(),
        Path::new("keep.txt")
    );
    assert_eq!(
        std::fs::read_link(repo.join("dangling")).unwrap(),
        Path::new("missing")
    );
    // …and saved each discarded link's target, as git stores a link.
    let saved: Vec<String> = backup
        .blobs
        .iter()
        .map(|(_, id)| sb.run(&repo, &["cat-file", "blob", &id.to_string()]))
        .collect();
    assert_eq!(saved, ["other.txt", "gone"]);
    let entry = trash::list(&trash_root).remove(0);
    assert_eq!(entry.files.len(), 2);
    for file in &entry.files {
        assert!(
            matches!(
                file.kind,
                TrashFileKind::Tracked {
                    left: Some(_),
                    executable: false,
                    symlink: true,
                    ..
                }
            ),
            "{file:?}"
        );
    }

    let report = trash::restore(sb.git(), &entry, &tracked, &c()).unwrap();
    assert_eq!(report.restored, tracked);
    assert!(
        report.conflicts.is_empty() && report.failed.is_empty(),
        "{report:?}"
    );
    assert!(report.emptied);
    for (name, target) in [("link", "other.txt"), ("dangling", "gone")] {
        let meta = std::fs::symlink_metadata(repo.join(name)).unwrap();
        assert!(meta.is_symlink(), "{name} is a symlink again");
        assert_eq!(
            std::fs::read_link(repo.join(name)).unwrap(),
            Path::new(target)
        );
    }
    // The links were replaced, never written through: their targets are untouched.
    assert_eq!(read(&repo, b"keep.txt"), b"committed\n");
    assert_eq!(read(&repo, b"other.txt"), b"other\n");
    assert_eq!(sb.run(&repo, &["status", "--porcelain"]).lines().count(), 3);
}

#[test]
fn a_restore_never_overwrites_a_different_file() {
    let sb = Sandbox::new();
    let repo = repo_at(&sb, b"r");
    let trash_root = sb.path().join("trash");
    write(&repo, b"keep.txt", b"discarded edit\n");
    write(&repo, b"new.txt", b"untracked\n");
    write(&repo, b"same.txt", b"same\n");
    let tracked = [bs(b"keep.txt")];
    let untracked = [bs(b"new.txt"), bs(b"same.txt")];
    discard_paths(sb.git(), &repo, &tracked, &untracked, &trash_root, &c()).unwrap();

    // Both paths hold something else now; `same.txt` holds the very same bytes.
    write(&repo, b"keep.txt", b"changed since\n");
    write(&repo, b"new.txt", b"someone else's\n");
    write(&repo, b"same.txt", b"same\n");
    let entry = trash::list(&trash_root).remove(0);
    let all = [bs(b"keep.txt"), bs(b"new.txt"), bs(b"same.txt")];
    let report = trash::restore(sb.git(), &entry, &all, &c()).unwrap();
    assert_eq!(report.conflicts, [bs(b"keep.txt"), bs(b"new.txt")]);
    assert_eq!(report.restored, [bs(b"same.txt")]);
    assert_eq!(read(&repo, b"keep.txt"), b"changed since\n");
    assert_eq!(read(&repo, b"new.txt"), b"someone else's\n");
    let entry = trash::list(&trash_root).remove(0);
    assert_eq!(entry.files.len(), 2);

    // Back as the discard left them: the restore goes through.
    sb.run(&repo, &["checkout", "--", "keep.txt"]);
    std::fs::remove_file(repo.join("new.txt")).unwrap();
    let report = trash::restore(sb.git(), &entry, &all, &c()).unwrap();
    assert_eq!(report.restored, [bs(b"keep.txt"), bs(b"new.txt")]);
    assert!(report.emptied);
    assert_eq!(read(&repo, b"keep.txt"), b"discarded edit\n");
    assert_eq!(read(&repo, b"new.txt"), b"untracked\n");
}

#[test]
fn cleans_and_tracked_only_discards_are_recorded() {
    let sb = Sandbox::new();
    let repo = repo_at(&sb, b"r");
    let trash_root = sb.path().join("trash");
    write(&repo, b"build/out.o", b"object\n");
    let backup = clean_paths(sb.git(), &repo, &[bs(b"build/")], &trash_root, &c()).unwrap();
    assert!(!repo.join("build").exists());
    let entry = trash::read_entry(&backup.trash_dir.unwrap());
    assert_eq!(entry.action, TrashAction::Clean);
    // A folder is one file of the record (without its trailing `/`).
    assert_eq!(entry.files.len(), 1);
    assert_eq!(entry.files[0].path, "build");

    write(&repo, b"keep.txt", b"edit\n");
    let backup =
        discard_paths(sb.git(), &repo, &[bs(b"keep.txt")], &[], &trash_root, &c()).unwrap();
    let entry = trash::read_entry(&backup.trash_dir.unwrap());
    assert_eq!(entry.action, TrashAction::Discard);
    assert_eq!(entry.files.len(), 1);
    assert!(matches!(entry.files[0].kind, TrashFileKind::Tracked { .. }));
    assert!(!entry.dir.join("files").exists());

    // Restoring the folder brings it back whole.
    let clean = trash::list(&trash_root)
        .into_iter()
        .find(|e| e.action == TrashAction::Clean)
        .unwrap();
    trash::restore(sb.git(), &clean, &[bs(b"build/")], &c()).unwrap();
    assert_eq!(read(&repo, b"build/out.o"), b"object\n");

    // Nothing to save (a deleted tracked file comes back from the index): no entry.
    std::fs::remove_file(repo.join("keep.txt")).unwrap();
    let backup =
        discard_paths(sb.git(), &repo, &[bs(b"keep.txt")], &[], &trash_root, &c()).unwrap();
    assert_eq!(backup.trash_dir, None);
    assert_eq!(trash::list(&trash_root).len(), 1);
}

#[test]
fn legacy_folders_are_listed_by_walking_them() {
    let sb = Sandbox::new();
    let repo = repo_at(&sb, b"r");
    let trash_root = sb.path().join("trash");
    let old = trash_root.join("1700000000000");
    write(&old, b"dir/a.txt", b"legacy\n");
    std::fs::create_dir_all(old.join("empty")).unwrap();
    write(
        &trash_root.join("1700000000001-reset"),
        b"t.txt",
        b"in the way\n",
    );

    let entries = trash::list(&trash_root);
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|e| e.legacy && e.root.is_none()));
    assert_eq!(entries[0].action, TrashAction::Reset);
    assert_eq!(entries[0].time_ms, 1_700_000_000_001);
    let paths: Vec<&BString> = entries[1].files.iter().map(|f| &f.path).collect();
    assert_eq!(paths, [&bs(b"dir/a.txt"), &bs(b"empty")]);
    assert_eq!(entries[1].action, TrashAction::Unknown);
    assert!(trash::list_for_repo(&trash_root, &repo).is_empty());

    let mut entry = entries[1].clone();
    let err = trash::restore(sb.git(), &entry, &[bs(b"dir/a.txt")], &c()).unwrap_err();
    assert!(matches!(err, TrashError::UnknownRepository));
    // The user picks the repository.
    entry.root = Some(repo.clone());
    let report = trash::restore(sb.git(), &entry, &[bs(b"dir/a.txt")], &c()).unwrap();
    assert_eq!(report.restored, [bs(b"dir/a.txt")]);
    assert!(!report.emptied);
    assert_eq!(read(&repo, b"dir/a.txt"), b"legacy\n");
    // `dir/` went with its last file; the empty folder that is itself trash stays.
    assert!(!old.join("dir").exists());
    assert!(old.join("empty").is_dir());
    let report = trash::restore(sb.git(), &entry, &[bs(b"empty")], &c()).unwrap();
    assert!(report.emptied);
    assert!(repo.join("empty").is_dir());
    assert!(!old.exists());
}

#[test]
fn delete_and_the_optional_limits() {
    let sb = Sandbox::new();
    let repo = repo_at(&sb, b"r");
    let trash_root = sb.path().join("trash");
    write(&trash_root.join("1000"), b"ancient.txt", b"old\n");
    write(&repo, b"fresh.txt", b"new\n");
    discard_paths(sb.git(), &repo, &[], &[bs(b"fresh.txt")], &trash_root, &c()).unwrap();
    assert_eq!(trash::list(&trash_root).len(), 2);

    // Off (the default): nothing goes.
    assert_eq!(
        trash::enforce_limits(sb.git(), &trash_root, None, None).unwrap(),
        0
    );
    // A day: the 1970 folder goes.
    let day = Some(Duration::from_secs(86_400));
    assert_eq!(
        trash::enforce_limits(sb.git(), &trash_root, day, None).unwrap(),
        1
    );
    let entries = trash::list(&trash_root);
    assert_eq!(entries.len(), 1);
    assert!(!entries[0].legacy);

    // A size limit below what's left: the oldest go until it fits (here all of it).
    write(&repo, b"second.txt", b"second\n");
    discard_paths(
        sb.git(),
        &repo,
        &[],
        &[bs(b"second.txt")],
        &trash_root,
        &c(),
    )
    .unwrap();
    let total: u64 = trash::list(&trash_root).iter().map(|e| e.size_bytes).sum();
    let newest = trash::list(&trash_root)[0].size_bytes;
    let removed = trash::enforce_limits(sb.git(), &trash_root, None, Some(total - 1)).unwrap();
    assert_eq!(removed, 1, "the older one only");
    let left = trash::list(&trash_root);
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].size_bytes, newest);

    trash::delete(sb.git(), &left[0]).unwrap();
    assert!(trash::list(&trash_root).is_empty());
    // A trash that was never created is just empty.
    assert!(trash::list(&sb.path().join("nowhere")).is_empty());
    assert_eq!(
        trash::enforce_limits(
            sb.git(),
            &sb.path().join("nowhere"),
            Some(Duration::ZERO),
            Some(0)
        )
        .unwrap(),
        0
    );
}

/// Whether the entry in `dir` has the ref that keeps its blobs.
fn protected(sb: &Sandbox, repo: &Path, dir: &Path) -> bool {
    gitweft_git::trash::is_protected(sb.git(), repo, dir, &c())
}

/// `git gc` pruning every unreachable object now.
fn gc_now(sb: &Sandbox, repo: &Path) {
    sb.run(
        repo,
        &[
            "-c",
            "gc.reflogExpire=now",
            "-c",
            "gc.reflogExpireUnreachable=now",
            "gc",
            "-q",
            "--prune=now",
        ],
    );
}

fn has_object(sb: &Sandbox, repo: &Path, id: gitweft_git::ObjectId) -> bool {
    let output = sb
        .git()
        .command(repo)
        .args(["cat-file", "--batch-check"])
        .stdin(format!("{id}\n"))
        .read_only()
        .run(&c())
        .unwrap();
    !output.stdout_str().contains("missing")
}

fn tracked_blob(entry: &trash::TrashEntry) -> gitweft_git::ObjectId {
    entry
        .files
        .iter()
        .find_map(|file| match file.kind {
            TrashFileKind::Tracked { blob, .. } => Some(blob),
            TrashFileKind::Untracked => None,
        })
        .unwrap()
}

#[test]
fn trashed_content_survives_git_gc_until_its_entry_goes() {
    let sb = Sandbox::new();
    let repo = repo_at(&sb, b"r");
    let trash_root = sb.path().join("trash");
    write(&repo, b"keep.txt", b"precious edit\n");
    discard_paths(sb.git(), &repo, &[bs(b"keep.txt")], &[], &trash_root, &c()).unwrap();
    let entry = trash::list_for_repo(&trash_root, &repo).remove(0);
    let blob = tracked_blob(&entry);
    assert!(protected(&sb, &repo, &entry.dir));
    gc_now(&sb, &repo);
    assert!(has_object(&sb, &repo, blob));

    // Restoring all of it removes the entry and its ref.
    let report = trash::restore(sb.git(), &entry, &[bs(b"keep.txt")], &c()).unwrap();
    assert_eq!(report.restored, [bs(b"keep.txt")]);
    assert!(report.emptied);
    assert_eq!(read(&repo, b"keep.txt"), b"precious edit\n");
    assert!(!protected(&sb, &repo, &entry.dir));

    // A deleted entry lets git have its blob again.
    write(&repo, b"keep.txt", b"second edit\n");
    discard_paths(sb.git(), &repo, &[bs(b"keep.txt")], &[], &trash_root, &c()).unwrap();
    let entry = trash::list_for_repo(&trash_root, &repo).remove(0);
    let blob = tracked_blob(&entry);
    assert!(protected(&sb, &repo, &entry.dir));
    trash::delete(sb.git(), &entry).unwrap();
    assert!(!protected(&sb, &repo, &entry.dir));
    gc_now(&sb, &repo);
    assert!(!has_object(&sb, &repo, blob));
}

#[test]
fn entries_from_before_refs_get_theirs_when_listed() {
    let sb = Sandbox::new();
    let repo = repo_at(&sb, b"r");
    let trash_root = sb.path().join("trash");
    write(&repo, b"keep.txt", b"old edit\n");
    discard_paths(sb.git(), &repo, &[bs(b"keep.txt")], &[], &trash_root, &c()).unwrap();
    let entries = trash::list_for_repo(&trash_root, &repo);
    let name = gitweft_git::trash::protect_ref(&entries[0].dir).unwrap();
    assert!(name.starts_with("refs/gitweft/trash/"));
    sb.run(&repo, &["update-ref", "-d", &name]);
    assert!(!protected(&sb, &repo, &entries[0].dir));

    trash::protect_all(sb.git(), &entries, &c());
    assert!(protected(&sb, &repo, &entries[0].dir));
    gc_now(&sb, &repo);
    assert!(has_object(&sb, &repo, tracked_blob(&entries[0])));
    // Neither a branch nor a tag: the refs git and the app list are unchanged.
    assert_eq!(sb.run(&repo, &["branch", "--list"]).trim(), "* main");
    assert!(sb.run(&repo, &["tag", "--list"]).trim().is_empty());
}

#[test]
fn a_file_where_a_folder_is_needed_blocks_a_restore_until_moved_aside() {
    let sb = Sandbox::new();
    let repo = repo_at(&sb, b"r");
    let trash_root = sb.path().join("trash");
    write(&repo, b"dir/notes.txt", b"notes\n");
    clean_paths(sb.git(), &repo, &[bs(b"dir/notes.txt")], &trash_root, &c()).unwrap();
    // `dir` is a file now.
    let _ = std::fs::remove_dir(repo.join("dir"));
    write(&repo, b"dir", b"a file now\n");
    assert_eq!(
        trash::blocker_in(&repo, Path::new("dir/notes.txt")),
        Some(PathBuf::from("dir"))
    );
    // Missing folders are no obstacle.
    assert_eq!(trash::blocker_in(&repo, Path::new("new/deep/file")), None);

    let entry = trash::list_for_repo(&trash_root, &repo).remove(0);
    let report = trash::restore(sb.git(), &entry, &[bs(b"dir/notes.txt")], &c()).unwrap();
    assert!(report.restored.is_empty() && report.failed.is_empty());
    assert_eq!(report.blocked, [(bs(b"dir/notes.txt"), bs(b"dir"))]);

    let aside = trash::move_aside(&trash_root, &repo, &[bs(b"dir")]).unwrap();
    let again = trash::read_entry(&entry.dir);
    let report = trash::restore(sb.git(), &again, &[bs(b"dir/notes.txt")], &c()).unwrap();
    assert_eq!(report.restored, [bs(b"dir/notes.txt")]);
    assert_eq!(read(&repo, b"dir/notes.txt"), b"notes\n");
    // What was in the way waits in the trash, to be put back like anything there.
    let moved = trash::read_entry(&aside);
    assert_eq!(moved.action, TrashAction::Aside);
    assert_eq!(moved.root.as_deref(), Some(repo.as_path()));
    assert_eq!(read(&moved.files_dir, b"dir"), b"a file now\n");
}

// macOS file systems refuse names that aren't UTF-8.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn non_utf8_paths_and_roots_round_trip() {
    let sb = Sandbox::new();
    let repo = repo_at(&sb, b"caf\xe9 repo");
    let trash_root = sb.path().join("trash");
    let tracked = b"\xff tracked.txt";
    write(&repo, tracked, b"committed\n");
    sb.git()
        .command(&repo)
        .args(["add", "-A"])
        .run(&c())
        .unwrap();
    sb.run(&repo, &["commit", "-qm", "non-utf8 name"]);
    write(&repo, tracked, b"edited\n");
    let untracked = b"d\xe9/\xfe.bin";
    write(&repo, untracked, b"\x00\xfe\n");

    discard_paths(
        sb.git(),
        &repo,
        &[bs(tracked)],
        &[bs(untracked)],
        &trash_root,
        &c(),
    )
    .unwrap();
    assert!(!path(&repo, untracked).exists());
    let entries = trash::list_for_repo(&trash_root, &repo);
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.root.as_deref(), Some(repo.as_path()));
    let paths: Vec<&BString> = entry.files.iter().map(|f| &f.path).collect();
    assert_eq!(paths, [&bs(tracked), &bs(untracked)]);

    let report = trash::restore(sb.git(), entry, &[bs(tracked), bs(untracked)], &c()).unwrap();
    assert!(report.emptied, "{report:?}");
    assert_eq!(read(&repo, tracked), b"edited\n");
    assert_eq!(read(&repo, untracked), b"\x00\xfe\n");
}

/// A saved copy that vanished from the trash folder is a failure the UI can word, not
/// English text from core.
#[test]
fn a_saved_copy_gone_from_the_trash_is_a_typed_failure() {
    let sb = Sandbox::new();
    let repo = repo_at(&sb, b"r");
    let trash_root = sb.path().join("trash");
    write(&repo, b"notes.txt", b"untracked\n");
    let backup = clean_paths(sb.git(), &repo, &[bs(b"notes.txt")], &trash_root, &c()).unwrap();
    let entry = trash::read_entry(&backup.trash_dir.unwrap());
    // Someone emptied the files by hand.
    std::fs::remove_dir_all(entry.dir.join("files")).unwrap();

    let report = trash::restore(sb.git(), &entry, &[bs(b"notes.txt")], &c()).unwrap();
    assert!(report.restored.is_empty());
    assert_eq!(
        report.failed,
        [(bs(b"notes.txt"), trash::RestoreFailure::GoneFromTrash)]
    );
}
