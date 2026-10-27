//! Whole-file conflict resolution: reading a conflicted path's stages and taking one side,
//! with real merges, cherry-picks and rebases in a sandbox.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use gitweft_git::conflict_files::{
    ConflictSides, Side, binary_merge_paths, conflict_marker_size, conflict_sides, take_side,
};
use gitweft_git::gix::bstr::{BStr, BString};
use gitweft_git::{CancelToken, GitErrorKind, ObjectId};
use gitweft_testkit::Sandbox;

fn b(path: &str) -> &BStr {
    path.into()
}

/// A change to one file: new content, or `None` to delete it.
type Change<'a> = (&'a str, Option<&'a [u8]>);

fn apply(repo: &Path, changes: &[Change<'_>]) {
    for (path, content) in changes {
        let file = repo.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        match content {
            Some(content) => std::fs::write(&file, content).unwrap(),
            None => std::fs::remove_file(&file).unwrap(),
        }
    }
}

/// Commits everything (`add -A`: no pathspec, so names like `*.txt` need no care).
fn commit_all(sb: &Sandbox, repo: &Path, message: &str) {
    sb.run(repo, &["add", "-A"]);
    sb.run(repo, &["commit", "-q", "-m", message]);
}

/// `base` committed on `main`; `topic` branches from it with its own changes, `main`
/// makes its changes, and nothing is merged yet (`main` checked out).
fn branches(
    sb: &Sandbox,
    name: &str,
    base: &[Change<'_>],
    main: &[Change<'_>],
    topic: &[Change<'_>],
) -> PathBuf {
    let repo = sb.init_repo(name);
    apply(&repo, base);
    commit_all(sb, &repo, "base");
    sb.run(&repo, &["switch", "-q", "-c", "topic"]);
    apply(&repo, topic);
    commit_all(sb, &repo, "topic");
    sb.run(&repo, &["switch", "-q", "main"]);
    apply(&repo, main);
    commit_all(sb, &repo, "main");
    repo
}

/// [`branches`], then `topic` merged into `main`, stopping on conflicts.
fn merged(
    sb: &Sandbox,
    name: &str,
    base: &[Change<'_>],
    main: &[Change<'_>],
    topic: &[Change<'_>],
) -> PathBuf {
    let repo = branches(sb, name, base, main, topic);
    let err = sb
        .git()
        .command(&repo)
        .args(["merge", "--no-edit", "topic"])
        .run(&CancelToken::new())
        .unwrap_err();
    assert_eq!(err.kind, GitErrorKind::Conflict, "{}", err.stderr);
    repo
}

fn sides(sb: &Sandbox, repo: &Path, path: &str) -> ConflictSides {
    conflict_sides(sb.git(), repo, b(path), &CancelToken::new()).unwrap()
}

fn take(sb: &Sandbox, repo: &Path, path: &str, side: Side) {
    take_side(sb.git(), repo, b(path), side, &CancelToken::new()).unwrap();
}

fn stdout(sb: &Sandbox, repo: &Path, args: &[&str]) -> Vec<u8> {
    sb.git()
        .command(repo)
        .args(args)
        .env("GIT_LITERAL_PATHSPECS", "1")
        .run(&CancelToken::new())
        .unwrap()
        .stdout
}

/// The blob content of `oid`.
fn blob(sb: &Sandbox, repo: &Path, oid: ObjectId) -> Vec<u8> {
    stdout(sb, repo, &["cat-file", "blob", &oid.to_string()])
}

/// `git ls-files -s` for exactly `path`: `(mode, oid, stage)` per entry.
fn index_entries(sb: &Sandbox, repo: &Path, path: &str) -> Vec<(String, String, String)> {
    stdout(sb, repo, &["ls-files", "-s", "-z", "--", path])
        .split(|&b| b == 0)
        .filter(|r| !r.is_empty())
        .map(|record| {
            let text = String::from_utf8_lossy(record);
            let (meta, name) = text.split_once('\t').unwrap();
            assert_eq!(name, path, "only the path itself");
            let mut fields = meta.split(' ').map(str::to_owned);
            (
                fields.next().unwrap(),
                fields.next().unwrap(),
                fields.next().unwrap(),
            )
        })
        .collect()
}

/// The staged content of `path` (stage 0), asserting there is exactly that entry.
fn staged(sb: &Sandbox, repo: &Path, path: &str) -> (String, Vec<u8>) {
    let entries = index_entries(sb, repo, path);
    assert_eq!(entries.len(), 1, "{path}: {entries:?}");
    let (mode, oid, stage) = &entries[0];
    assert_eq!(stage, "0", "{path}");
    let oid = ObjectId::from_hex(oid.as_bytes()).unwrap();
    (mode.clone(), blob(sb, repo, oid))
}

/// `git status --porcelain -z` entries for exactly `path`: `(XY, path)`.
fn status_of(sb: &Sandbox, repo: &Path, path: &str) -> Vec<String> {
    stdout(sb, repo, &["status", "--porcelain", "-z"])
        .split(|&b| b == 0)
        .filter(|r| r.len() > 3 && &r[3..] == path.as_bytes())
        .map(|r| String::from_utf8_lossy(&r[..2]).into_owned())
        .collect()
}

const UNMERGED: [&str; 7] = ["DD", "AU", "UD", "UA", "DU", "AA", "UU"];

fn assert_resolved(sb: &Sandbox, repo: &Path, path: &str) {
    let status = status_of(sb, repo, path);
    assert!(
        !status.iter().any(|xy| UNMERGED.contains(&xy.as_str())),
        "{path} still unmerged: {status:?}"
    );
    assert!(sides(sb, repo, path) == ConflictSides::default());
}

fn assert_removed(sb: &Sandbox, repo: &Path, path: &str) {
    assert!(
        index_entries(sb, repo, path).is_empty(),
        "{path} in the index"
    );
    assert!(
        std::fs::symlink_metadata(repo.join(path)).is_err(),
        "{path} on disk"
    );
    assert_resolved(sb, repo, path);
}

fn assert_kept(sb: &Sandbox, repo: &Path, path: &str, content: &[u8]) {
    assert_eq!(
        std::fs::read(repo.join(path)).unwrap(),
        content,
        "{path} on disk"
    );
    assert_eq!(staged(sb, repo, path).1, content, "{path} staged");
    assert_resolved(sb, repo, path);
}

fn commit_no_edit(sb: &Sandbox, repo: &Path) {
    sb.run(repo, &["commit", "-q", "--no-edit"]);
    let parents = sb.run(repo, &["rev-list", "--parents", "-n1", "HEAD"]);
    assert_eq!(parents.split_whitespace().count(), 3, "a merge commit");
}

#[test]
fn modify_delete_takes_either_side_and_the_merge_commits() {
    let modify_delete = |sb: &Sandbox, name: &str| {
        merged(
            sb,
            name,
            &[("a.txt", Some(b"base\n"))],
            &[("a.txt", Some(b"main version\n"))],
            &[("a.txt", None)],
        )
    };
    let sb = Sandbox::new();
    let repo = modify_delete(&sb, "keep");
    let conflict = sides(&sb, &repo, "a.txt");
    let ours = conflict.ours.clone().expect("main modified it");
    assert_eq!((ours.size, ours.binary, ours.mode), (13, false, 0o100644));
    assert_eq!(blob(&sb, &repo, ours.oid), b"main version\n");
    let base = conflict.base.clone().expect("the base had it");
    assert_eq!(base.size, 5);
    assert_eq!(conflict.theirs, None, "topic deleted it");
    assert!(conflict.whole_file());
    assert_eq!(status_of(&sb, &repo, "a.txt"), ["UD"]);

    take(&sb, &repo, "a.txt", Side::Ours);
    assert_kept(&sb, &repo, "a.txt", b"main version\n");
    commit_no_edit(&sb, &repo);
    assert_eq!(sb.run(&repo, &["show", "HEAD:a.txt"]), "main version\n");

    // A fresh copy, taking the deletion.
    let repo = modify_delete(&sb, "delete");
    take(&sb, &repo, "a.txt", Side::Theirs);
    assert_removed(&sb, &repo, "a.txt");
    commit_no_edit(&sb, &repo);
    assert!(
        sb.run(&repo, &["ls-tree", "--name-only", "HEAD"])
            .is_empty()
    );

    // The deletion again, with the working-tree file already gone.
    let repo = modify_delete(&sb, "gone");
    std::fs::remove_file(repo.join("a.txt")).unwrap();
    take(&sb, &repo, "a.txt", Side::Theirs);
    assert_removed(&sb, &repo, "a.txt");
}

#[test]
fn delete_modify_with_a_space_in_the_name() {
    let name = "a b.txt";
    let delete_modify = |sb: &Sandbox, repo: &str| {
        merged(
            sb,
            repo,
            &[(name, Some(b"base\n"))],
            &[(name, None)],
            &[(name, Some(b"topic version\n"))],
        )
    };
    let sb = Sandbox::new();
    let repo = delete_modify(&sb, "theirs");
    let conflict = sides(&sb, &repo, name);
    assert_eq!(conflict.ours, None, "main deleted it");
    assert!(conflict.base.is_some());
    let theirs = conflict.theirs.clone().unwrap();
    assert_eq!((theirs.size, theirs.binary), (14, false));
    assert!(conflict.whole_file());
    assert_eq!(status_of(&sb, &repo, name), ["DU"]);
    take(&sb, &repo, name, Side::Theirs);
    assert_kept(&sb, &repo, name, b"topic version\n");
    commit_no_edit(&sb, &repo);

    let repo = delete_modify(&sb, "ours");
    // Git leaves the surviving side's file on disk.
    assert_eq!(std::fs::read(repo.join(name)).unwrap(), b"topic version\n");
    take(&sb, &repo, name, Side::Ours);
    assert_removed(&sb, &repo, name);
    commit_no_edit(&sb, &repo);
}

#[test]
fn pathspec_characters_in_a_name_mean_only_that_file() {
    // Both files would match the glob `*.txt`.
    let glob = "*.txt";
    let sb = Sandbox::new();
    let repo = merged(
        &sb,
        "r",
        &[(glob, Some(b"base\n")), ("x.txt", Some(b"base\n"))],
        &[(glob, Some(b"main *\n")), ("x.txt", Some(b"main x\n"))],
        &[(glob, None), ("x.txt", None)],
    );
    let conflict = sides(&sb, &repo, glob);
    assert_eq!(blob(&sb, &repo, conflict.ours.unwrap().oid), b"main *\n");
    take(&sb, &repo, glob, Side::Theirs);
    assert_removed(&sb, &repo, glob);
    assert_eq!(status_of(&sb, &repo, "x.txt"), ["UD"], "untouched");
    assert_eq!(std::fs::read(repo.join("x.txt")).unwrap(), b"main x\n");

    take(&sb, &repo, "x.txt", Side::Ours);
    assert_kept(&sb, &repo, "x.txt", b"main x\n");
    commit_no_edit(&sb, &repo);
}

#[test]
fn binary_files_changed_on_both_sides() {
    let base: &[u8] = b"\0\x01base";
    let main: &[u8] = b"\0\x02main side\xff";
    let topic: &[u8] = b"\0\x03topic\xfe\0";
    // Git's own rule: a NUL within the first 8000 bytes. One past them is text to git,
    // so the merge writes conflict markers into that file.
    let mut late_base = vec![b'a'; 8000];
    late_base.extend_from_slice(b"\0base\n");
    let late_main = [&late_base[..], b"main\n"].concat();
    let late_topic = [&late_base[..], b"topic\n"].concat();
    let binary = |sb: &Sandbox, name: &str| {
        merged(
            sb,
            name,
            &[("data.bin", Some(base)), ("late.dat", Some(&late_base))],
            &[("data.bin", Some(main)), ("late.dat", Some(&late_main))],
            &[("data.bin", Some(topic)), ("late.dat", Some(&late_topic))],
        )
    };
    let sb = Sandbox::new();
    let repo = binary(&sb, "theirs");
    let conflict = sides(&sb, &repo, "data.bin");
    let (ours, theirs) = (
        conflict.ours.clone().unwrap(),
        conflict.theirs.clone().unwrap(),
    );
    assert!(ours.binary && theirs.binary && conflict.base.as_ref().unwrap().binary);
    assert_eq!(
        (ours.size, theirs.size),
        (main.len() as u64, topic.len() as u64)
    );
    assert!(conflict.whole_file());
    // Git keeps ours on disk for a binary conflict, without markers.
    assert_eq!(std::fs::read(repo.join("data.bin")).unwrap(), main);

    let late = sides(&sb, &repo, "late.dat");
    assert!(!late.ours.as_ref().unwrap().binary);
    assert_eq!(late.theirs.as_ref().unwrap().size, late_topic.len() as u64);
    assert!(!late.whole_file());
    let on_disk = std::fs::read(repo.join("late.dat")).unwrap();
    assert!(
        on_disk.windows(7).any(|w| w == b"<<<<<<<"),
        "git merged it as text"
    );

    take(&sb, &repo, "data.bin", Side::Theirs);
    assert_kept(&sb, &repo, "data.bin", topic);
    // Taking a side works for a text conflict too.
    take(&sb, &repo, "late.dat", Side::Theirs);
    assert_kept(&sb, &repo, "late.dat", &late_topic);

    let repo = binary(&sb, "ours");
    take(&sb, &repo, "data.bin", Side::Ours);
    assert_kept(&sb, &repo, "data.bin", main);
}

#[test]
fn a_text_conflict_is_for_the_line_tool_an_add_add_has_no_base() {
    let sb = Sandbox::new();
    let repo = merged(
        &sb,
        "r",
        &[("a.txt", Some(b"base\n"))],
        &[
            ("a.txt", Some(b"main\n")),
            ("new.txt", Some(b"main's new\n")),
        ],
        &[
            ("a.txt", Some(b"topic\n")),
            ("new.txt", Some(b"topic's new\n")),
        ],
    );
    let text = sides(&sb, &repo, "a.txt");
    assert!(text.base.is_some() && text.ours.is_some() && text.theirs.is_some());
    assert!(!text.ours.as_ref().unwrap().binary);
    assert!(!text.whole_file());

    let added = sides(&sb, &repo, "new.txt");
    assert_eq!(added.base, None);
    assert_eq!(added.ours.as_ref().unwrap().size, 11);
    assert_eq!(added.theirs.as_ref().unwrap().size, 12);
    assert!(!added.whole_file());
    assert_eq!(status_of(&sb, &repo, "new.txt"), ["AA"]);

    take(&sb, &repo, "new.txt", Side::Ours);
    assert_kept(&sb, &repo, "new.txt", b"main's new\n");
    take(&sb, &repo, "a.txt", Side::Theirs);
    assert_kept(&sb, &repo, "a.txt", b"topic\n");
    commit_no_edit(&sb, &repo);
}

#[test]
fn modes_are_taken_exactly_executable_bits_and_symlinks() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    std::fs::write(repo.join("run.sh"), "echo base\n").unwrap();
    std::os::unix::fs::symlink("target-base", repo.join("link")).unwrap();
    commit_all(&sb, &repo, "base");
    sb.run(&repo, &["switch", "-q", "-c", "topic"]);
    std::fs::write(repo.join("run.sh"), "echo topic\n").unwrap();
    std::fs::set_permissions(repo.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::remove_file(repo.join("link")).unwrap();
    std::os::unix::fs::symlink("target-topic", repo.join("link")).unwrap();
    commit_all(&sb, &repo, "topic");
    sb.run(&repo, &["switch", "-q", "main"]);
    std::fs::write(repo.join("run.sh"), "echo main\n").unwrap();
    std::fs::remove_file(repo.join("link")).unwrap();
    std::os::unix::fs::symlink("target-main", repo.join("link")).unwrap();
    commit_all(&sb, &repo, "main");
    // Even when git mustn't trust the working tree's executable bit.
    sb.run(&repo, &["config", "core.fileMode", "false"]);
    sb.git()
        .command(&repo)
        .args(["merge", "--no-edit", "topic"])
        .run(&CancelToken::new())
        .unwrap_err();

    let script = sides(&sb, &repo, "run.sh");
    assert_eq!(script.ours.as_ref().unwrap().mode, 0o100644);
    assert_eq!(script.theirs.as_ref().unwrap().mode, 0o100755);
    assert!(!script.whole_file(), "a mode change alone is still text");
    take(&sb, &repo, "run.sh", Side::Theirs);
    assert_kept(&sb, &repo, "run.sh", b"echo topic\n");
    assert_eq!(staged(&sb, &repo, "run.sh").0, "100755");
    let mode = std::fs::metadata(repo.join("run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0o111, "executable on disk");

    let link = sides(&sb, &repo, "link");
    let theirs = link.theirs.clone().unwrap();
    assert_eq!((theirs.mode, theirs.size), (0o120000, 12));
    assert!(link.whole_file(), "no conflict markers in a symlink");
    take(&sb, &repo, "link", Side::Theirs);
    assert_eq!(
        std::fs::read_link(repo.join("link")).unwrap(),
        Path::new("target-topic")
    );
    assert_eq!(
        staged(&sb, &repo, "link"),
        ("120000".to_owned(), b"target-topic".to_vec())
    );
    assert_resolved(&sb, &repo, "link");
    commit_no_edit(&sb, &repo);
}

fn relink(repo: &Path, path: &str, target: &[u8]) {
    let link = repo.join(path);
    if std::fs::symlink_metadata(&link).is_ok() {
        std::fs::remove_file(&link).unwrap();
    }
    std::os::unix::fs::symlink(OsStr::from_bytes(target), link).unwrap();
}

#[test]
fn stages_know_symlink_targets_and_executable_bits() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let main_target: &[u8] = b"caf\xe9-main";
    std::fs::write(repo.join("run.sh"), "echo base\n").unwrap();
    relink(&repo, "link", b"target-base");
    commit_all(&sb, &repo, "base");
    sb.run(&repo, &["switch", "-q", "-c", "topic"]);
    std::fs::write(repo.join("run.sh"), "echo topic\n").unwrap();
    std::fs::set_permissions(repo.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    relink(&repo, "link", b"dir/target-topic");
    commit_all(&sb, &repo, "topic");
    sb.run(&repo, &["switch", "-q", "main"]);
    std::fs::write(repo.join("run.sh"), "echo main\n").unwrap();
    relink(&repo, "link", main_target);
    commit_all(&sb, &repo, "main");
    let err = sb
        .git()
        .command(&repo)
        .args(["merge", "--no-edit", "topic"])
        .run(&CancelToken::new())
        .unwrap_err();
    assert_eq!(err.kind, GitErrorKind::Conflict, "{}", err.stderr);

    let link = sides(&sb, &repo, "link");
    let stages = [&link.base, &link.ours, &link.theirs].map(|s| s.clone().unwrap());
    for stage in &stages {
        assert!(stage.is_symlink() && !stage.is_submodule() && !stage.is_executable());
    }
    let targets = stages.map(|s| s.symlink_target);
    assert_eq!(
        targets,
        [
            Some(BString::from("target-base")),
            Some(BString::from(main_target)),
            Some(BString::from("dir/target-topic")),
        ]
    );
    assert!(!link.binary_by_attributes);
    assert!(link.whole_file());

    let script = sides(&sb, &repo, "run.sh");
    let [base, ours, theirs] =
        [&script.base, &script.ours, &script.theirs].map(|s| s.clone().unwrap());
    assert!(
        !base.is_executable() && !ours.is_executable(),
        "only topic set +x"
    );
    assert!(theirs.is_executable());
    for stage in [&base, &ours, &theirs] {
        assert!(!stage.is_symlink() && !stage.is_submodule());
        assert_eq!(stage.symlink_target, None);
    }
    assert!(
        !script.whole_file(),
        "an executable bit alone is still text"
    );
}

/// `a.txt` changed on both sides, with `a.txt <attributes>` committed in the base.
fn attribute_conflict(sb: &Sandbox, name: &str, attributes: &str) -> PathBuf {
    let rule = format!("a.txt {attributes}\n");
    merged(
        sb,
        name,
        &[
            (".gitattributes", Some(rule.as_bytes())),
            ("a.txt", Some(b"base\n")),
        ],
        &[("a.txt", Some(b"main\n"))],
        &[("a.txt", Some(b"topic\n"))],
    )
}

fn has_markers(repo: &Path, path: &str) -> bool {
    std::fs::read(repo.join(path))
        .unwrap()
        .windows(7)
        .any(|w| w == b"<<<<<<<")
}

#[test]
fn merge_attributes_make_git_merge_text_as_binary() {
    let sb = Sandbox::new();
    for (i, attributes) in ["merge=binary", "-merge", "binary"].into_iter().enumerate() {
        let repo = attribute_conflict(&sb, &format!("r{i}"), attributes);
        // Git's binary driver: ours left in place, no markers, still unmerged.
        assert_eq!(
            std::fs::read(repo.join("a.txt")).unwrap(),
            b"main\n",
            "{attributes}"
        );
        assert_eq!(status_of(&sb, &repo, "a.txt"), ["UU"], "{attributes}");
        let conflict = sides(&sb, &repo, "a.txt");
        assert!(conflict.binary_by_attributes, "{attributes}");
        let (ours, theirs) = (
            conflict.ours.as_ref().unwrap(),
            conflict.theirs.as_ref().unwrap(),
        );
        assert!(
            !ours.binary && !theirs.binary,
            "{attributes}: the content is text"
        );
        assert!(conflict.whole_file(), "{attributes}");

        take(&sb, &repo, "a.txt", Side::Theirs);
        assert_kept(&sb, &repo, "a.txt", b"topic\n");
        commit_no_edit(&sb, &repo);
    }
}

#[test]
fn text_or_diff_attributes_alone_still_merge_with_markers() {
    let sb = Sandbox::new();
    for (i, attributes) in ["-text", "-diff"].into_iter().enumerate() {
        let repo = attribute_conflict(&sb, &format!("r{i}"), attributes);
        assert!(has_markers(&repo, "a.txt"), "{attributes}");
        let conflict = sides(&sb, &repo, "a.txt");
        assert!(!conflict.binary_by_attributes, "{attributes}");
        assert!(!conflict.whole_file(), "{attributes}");
    }
}

#[test]
fn the_merge_attribute_is_read_from_our_side() {
    let sb = Sandbox::new();
    let rule: Option<&[u8]> = Some(b"a.txt merge=binary\n");
    let base: &[(&str, Option<&[u8]>)] = &[("a.txt", Some(&b"base\n"[..]))];
    let main: Option<&[u8]> = Some(b"main\n");
    let topic: Option<&[u8]> = Some(b"topic\n");
    // Only topic adds the rule: git merged with main's attributes and wrote markers,
    // although the merged .gitattributes now has it.
    let theirs_rule = merged(
        &sb,
        "theirs-rule",
        base,
        &[("a.txt", main)],
        &[("a.txt", topic), (".gitattributes", rule)],
    );
    assert!(has_markers(&theirs_rule, "a.txt"));
    let conflict = sides(&sb, &theirs_rule, "a.txt");
    // Git before 2.40 can't read HEAD's attributes (`check-attr --source`): the working
    // tree's merged .gitattributes counts there, the known fallback.
    let head_attributes = sb.git().version() >= gitweft_git::GitVersion::new(2, 40, 0);
    assert_eq!(conflict.binary_by_attributes, !head_attributes);
    assert_eq!(conflict.whole_file(), !head_attributes);
    // Only main adds it: git kept ours.
    let ours_rule = merged(
        &sb,
        "ours-rule",
        base,
        &[("a.txt", main), (".gitattributes", rule)],
        &[("a.txt", topic)],
    );
    assert!(!has_markers(&ours_rule, "a.txt"));
    assert!(sides(&sb, &ours_rule, "a.txt").binary_by_attributes);
}

#[test]
fn the_marker_size_comes_from_the_conflict_marker_size_attribute() {
    let sb = Sandbox::new();
    let cancel = CancelToken::new();
    let size = |repo: &Path, path: &str| {
        conflict_marker_size(sb.git(), repo, &BString::from(path), &cancel)
    };
    let attributes = "*.txt conflict-marker-size=10\n";

    // Committed: read from HEAD's attributes, as git did when it wrote the markers.
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, ".gitattributes", attributes, "attributes");
    assert_eq!(size(&repo, "a.txt"), 10);
    assert_eq!(size(&repo, "dir/b.txt"), 10);
    assert_eq!(size(&repo, "c.md"), 7, "no attribute: git's default");

    // No commit yet: the working tree's attributes.
    let unborn = sb.init_repo("unborn");
    std::fs::write(unborn.join(".gitattributes"), attributes).unwrap();
    assert_eq!(size(&unborn, "a.txt"), 10);

    // Not a repository at all: the default rather than an error.
    assert_eq!(size(Path::new("/nonexistent"), "a.txt"), 7);
}

#[test]
fn binary_merge_paths_checks_many_paths_in_one_run() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    std::fs::write(
        repo.join(".gitattributes"),
        b"*.bin merge=binary\n*.raw -merge\n*.pic binary\n*.txt -text -diff\n\
          *.union merge=union\n\xff* merge=binary\n",
    )
    .unwrap();
    let affected: [&[u8]; 5] = [
        b"dir/a b.bin",
        b"caf\xe9.raw",
        b"x.pic",
        b"new\nline.bin",
        b"\xffdata",
    ];
    let unaffected: [&[u8]; 3] = [b"plain.txt", b"c.union", b"other"];
    let paths: Vec<BString> = unaffected
        .iter()
        .chain(&affected)
        .map(|p| BString::from(*p))
        .collect();
    let found = binary_merge_paths(sb.git(), &repo, &paths, &CancelToken::new()).unwrap();
    let expected: HashSet<BString> = affected.iter().map(|p| BString::from(*p)).collect();
    assert_eq!(found, expected);

    // No paths: git isn't run at all (this directory isn't even a repository).
    let none = binary_merge_paths(
        sb.git(),
        Path::new("/nonexistent"),
        &[],
        &CancelToken::new(),
    );
    assert!(none.unwrap().is_empty());
}

#[test]
fn cherry_picks_and_rebases_take_sides_too() {
    let sb = Sandbox::new();
    let cancel = CancelToken::new();
    let setup = |name: &str| {
        branches(
            &sb,
            name,
            &[("a.txt", Some(b"base\n"))],
            &[("a.txt", Some(b"main version\n"))],
            &[("a.txt", None)],
        )
    };

    // Picking topic's deletion onto main: ours is HEAD (main), theirs the picked commit.
    let repo = setup("pick");
    sb.git()
        .command(&repo)
        .args(["cherry-pick", "topic"])
        .run(&cancel)
        .unwrap_err();
    let conflict = sides(&sb, &repo, "a.txt");
    assert!(conflict.ours.is_some() && conflict.theirs.is_none());
    take(&sb, &repo, "a.txt", Side::Theirs);
    assert_removed(&sb, &repo, "a.txt");
    sb.run(&repo, &["cherry-pick", "--continue"]);
    assert!(
        sb.run(&repo, &["ls-tree", "--name-only", "HEAD"])
            .is_empty()
    );

    // Rebasing main onto topic: ours is topic (the branch rebased onto, which deleted the
    // file), theirs main's commit being replayed.
    let repo = setup("rebase");
    sb.git()
        .command(&repo)
        .args(["rebase", "topic"])
        .run(&cancel)
        .unwrap_err();
    let conflict = sides(&sb, &repo, "a.txt");
    assert_eq!(conflict.ours, None, "the branch rebased onto deleted it");
    let theirs = conflict.theirs.unwrap();
    assert_eq!(blob(&sb, &repo, theirs.oid), b"main version\n");
    take(&sb, &repo, "a.txt", Side::Theirs);
    assert_kept(&sb, &repo, "a.txt", b"main version\n");
    sb.run(&repo, &["rebase", "--continue"]);
    assert_eq!(sb.run(&repo, &["show", "HEAD:a.txt"]), "main version\n");
    assert_eq!(sb.run(&repo, &["log", "-1", "--format=%s"]).trim(), "main");
}

#[test]
fn a_path_that_isnt_conflicted_has_no_sides() {
    let sb = Sandbox::new();
    let repo = merged(
        &sb,
        "r",
        &[("a.txt", Some(b"base\n")), ("dir/b.txt", Some(b"b\n"))],
        &[("a.txt", Some(b"main\n"))],
        &[
            ("a.txt", Some(b"topic\n")),
            ("dir/b.txt", Some(b"topic b\n")),
        ],
    );
    for path in ["dir/b.txt", "missing.txt", "dir", "a.tx"] {
        assert_eq!(sides(&sb, &repo, path), ConflictSides::default(), "{path}");
        assert!(!sides(&sb, &repo, path).is_conflicted());
    }
    // Taking a side of it changes nothing.
    let before = stdout(&sb, &repo, &["ls-files", "-s"]);
    take(&sb, &repo, "dir/b.txt", Side::Theirs);
    take(&sb, &repo, "missing.txt", Side::Ours);
    assert_eq!(stdout(&sb, &repo, &["ls-files", "-s"]), before);
    assert!(sides(&sb, &repo, "a.txt").is_conflicted());
}
