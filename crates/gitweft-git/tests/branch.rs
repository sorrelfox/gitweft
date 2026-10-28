use std::path::{Path, PathBuf};

use gitweft_git::branch::{
    create_branch, delete_branch, is_valid_branch_name, rename_branch, switch_branch,
    switch_detached, switch_to_remote_branch,
};
use gitweft_git::read::branch_tracking;
use gitweft_git::{CancelToken, GitErrorKind};
use gitweft_testkit::Sandbox;

fn head(sb: &Sandbox, repo: &Path) -> String {
    sb.run(repo, &["rev-parse", "--abbrev-ref", "HEAD"])
        .trim()
        .to_owned()
}

fn branches(sb: &Sandbox, repo: &Path) -> Vec<String> {
    sb.run(repo, &["branch", "--format=%(refname:short)"])
        .lines()
        .map(str::to_owned)
        .collect()
}

/// A repository with two commits on `main`.
fn repo(sb: &Sandbox) -> (PathBuf, String) {
    let repo = sb.init_repo("r");
    let first = sb.commit_file(&repo, "a.txt", "1\n", "first");
    sb.commit_file(&repo, "a.txt", "2\n", "second");
    (repo, first)
}

// macOS file systems refuse names that aren't UTF-8.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn a_branch_whose_name_isnt_utf8_is_switched_renamed_and_deleted() {
    use std::os::unix::ffi::OsStrExt as _;
    let sb = Sandbox::new();
    let (repo, first) = repo(&sb);
    let cancel = CancelToken::new();
    let name = std::ffi::OsStr::from_bytes(b"caf\xe9");
    let other = std::ffi::OsStr::from_bytes(b"th\xe9");
    for branch in [name, other] {
        sb.git()
            .command(&repo)
            .arg("branch")
            .arg(branch)
            .arg(first.trim())
            .run(&cancel)
            .unwrap();
    }
    let head_ref = || {
        sb.git()
            .command(&repo)
            .args(["symbolic-ref", "HEAD"])
            .read_only()
            .run(&cancel)
            .unwrap()
            .stdout
    };

    // git refuses the lossy name ("invalid reference: caf\u{fffd}"); its bytes work.
    let lossy = name.to_string_lossy().into_owned();
    assert!(switch_branch(sb.git(), &repo, &lossy, &cancel).is_err());
    switch_branch(sb.git(), &repo, name, &cancel).unwrap();
    assert_eq!(head_ref(), b"refs/heads/caf\xe9\n");

    switch_branch(sb.git(), &repo, "main", &cancel).unwrap();
    rename_branch(sb.git(), &repo, name, "cafe", &cancel).unwrap();
    delete_branch(sb.git(), &repo, other, true, &cancel).unwrap();
    assert_eq!(branches(&sb, &repo), ["cafe", "main"]);
}

#[test]
fn branch_names_are_checked_like_git_does() {
    let sb = Sandbox::new();
    let (repo, _) = repo(&sb);
    let cancel = CancelToken::new();
    for good in ["feature", "feature/login-form", "fix-1.2"] {
        assert!(
            is_valid_branch_name(sb.git(), &repo, good, &cancel),
            "{good}"
        );
    }
    // After a switch, `@{-1}` names the previous branch: git accepts it, but it isn't a
    // new name.
    sb.run(&repo, &["switch", "-q", "-c", "other"]);
    sb.run(&repo, &["switch", "-q", "main"]);
    for bad in ["", "has space", "a..b", "-x", "ends.lock", "x~1", "@{-1}"] {
        assert!(
            !is_valid_branch_name(sb.git(), &repo, bad, &cancel),
            "{bad:?}"
        );
    }
}

#[test]
fn create_at_a_commit_rename_and_switch() {
    let sb = Sandbox::new();
    let (repo, first) = repo(&sb);
    let cancel = CancelToken::new();

    create_branch(sb.git(), &repo, "old-idea", Some(&first), false, &cancel).unwrap();
    assert_eq!(head(&sb, &repo), "main", "creating alone doesn't switch");
    assert_eq!(sb.run(&repo, &["rev-parse", "old-idea"]).trim(), first);

    rename_branch(sb.git(), &repo, "old-idea", "idea", &cancel).unwrap();
    assert_eq!(branches(&sb, &repo), ["idea", "main"]);

    switch_branch(sb.git(), &repo, "idea", &cancel).unwrap();
    assert_eq!(head(&sb, &repo), "idea");
    create_branch(sb.git(), &repo, "next", None, true, &cancel).unwrap();
    assert_eq!(head(&sb, &repo), "next");

    let err = create_branch(sb.git(), &repo, "main", None, false, &cancel).unwrap_err();
    assert!(err.stderr.contains("already exists"), "{}", err.stderr);
}

#[test]
fn deleting_unmerged_work_needs_force() {
    let sb = Sandbox::new();
    let (repo, _) = repo(&sb);
    let cancel = CancelToken::new();
    create_branch(sb.git(), &repo, "merged", None, false, &cancel).unwrap();
    delete_branch(sb.git(), &repo, "merged", false, &cancel).unwrap();

    create_branch(sb.git(), &repo, "wip", None, true, &cancel).unwrap();
    sb.commit_file(&repo, "b.txt", "b\n", "unmerged work");
    switch_branch(sb.git(), &repo, "main", &cancel).unwrap();
    let err = delete_branch(sb.git(), &repo, "wip", false, &cancel).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::NotFullyMerged, "{}", err.stderr);
    assert!(branches(&sb, &repo).contains(&"wip".to_owned()));

    delete_branch(sb.git(), &repo, "wip", true, &cancel).unwrap();
    assert_eq!(branches(&sb, &repo), ["main"]);
}

#[test]
fn switching_refuses_to_overwrite_local_changes_and_keeps_the_rest() {
    let sb = Sandbox::new();
    let (repo, first) = repo(&sb);
    let cancel = CancelToken::new();
    create_branch(sb.git(), &repo, "old", Some(&first), false, &cancel).unwrap();

    // a.txt differs between the branches, so a local edit of it blocks the switch.
    std::fs::write(repo.join("a.txt"), "local edit\n").unwrap();
    let err = switch_branch(sb.git(), &repo, "old", &cancel).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::WouldOverwrite, "{}", err.stderr);
    assert_eq!(head(&sb, &repo), "main");
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "local edit\n"
    );

    // An unrelated new file just comes along.
    sb.run(&repo, &["checkout", "--", "a.txt"]);
    std::fs::write(repo.join("notes.txt"), "keep me\n").unwrap();
    switch_branch(sb.git(), &repo, "old", &cancel).unwrap();
    assert_eq!(head(&sb, &repo), "old");
    assert!(repo.join("notes.txt").exists());

    switch_detached(sb.git(), &repo, "main", &cancel).unwrap();
    assert_eq!(head(&sb, &repo), "HEAD", "detached");
}

#[test]
fn a_remote_branch_becomes_a_local_tracking_branch() {
    let sb = Sandbox::new();
    let (seed, _) = repo(&sb);
    sb.run(&seed, &["branch", "feature"]);
    let url = format!("file://{}", seed.display());
    sb.run(sb.path(), &["clone", "-q", &url, "work"]);
    let work = sb.path().join("work");
    let cancel = CancelToken::new();

    let local =
        switch_to_remote_branch(sb.git(), &work, "origin", "feature", false, &cancel).unwrap();
    assert_eq!(local, "feature");
    assert_eq!(head(&sb, &work), "feature");
    let tracking = branch_tracking(sb.git(), &work, &cancel).unwrap();
    let feature = tracking.iter().find(|t| t.branch == "feature").unwrap();
    assert_eq!(feature.upstream.as_deref(), Some("origin/feature"));

    // Already there: just switch.
    switch_branch(sb.git(), &work, "main", &cancel).unwrap();
    switch_to_remote_branch(sb.git(), &work, "origin", "feature", true, &cancel).unwrap();
    assert_eq!(head(&sb, &work), "feature");
}

/// A remote branch whose name isn't UTF-8 is checked out by its bytes, tracks its upstream,
/// and pushes there without setting the upstream again.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn a_remote_branch_that_isnt_utf8_tracks_and_pushes_to_its_upstream() {
    use gitweft_git::remote::push_target;
    use gitweft_git::undo::{ref_name_bytes, ref_name_text};
    use std::os::unix::ffi::OsStrExt as _;
    let sb = Sandbox::new();
    let (seed, first) = repo(&sb);
    let name = std::ffi::OsStr::from_bytes(b"caf\xe9");
    let cancel = CancelToken::new();
    sb.git()
        .command(&seed)
        .arg("branch")
        .arg(name)
        .arg(first.trim())
        .run(&cancel)
        .unwrap();
    let url = format!("file://{}", seed.display());
    sb.run(sb.path(), &["clone", "-q", &url, "work"]);
    let work = sb.path().join("work");

    let local = switch_to_remote_branch(sb.git(), &work, "origin", name, false, &cancel).unwrap();
    assert_eq!(local, "caf\u{fffd}");
    let tracking = branch_tracking(sb.git(), &work, &cancel).unwrap();
    let key = ref_name_text(b"caf\xe9");
    let entry = tracking
        .iter()
        .find(|t| t.branch == key)
        .expect("tracked by its bytes");
    assert_eq!(
        entry.upstream.as_deref().map(ref_name_bytes),
        Some(b"origin/caf\xe9".to_vec())
    );
    let request = push_target(&key, &tracking, &["origin".to_owned()]).unwrap();
    assert!(!request.set_upstream, "{request:?}");
    assert_eq!(ref_name_bytes(&request.remote_branch), b"caf\xe9");
}
