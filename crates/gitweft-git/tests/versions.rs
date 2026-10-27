use std::path::Path;

use gitweft_git::gix::bstr::BString;
use gitweft_git::read::{DiffSource, file_versions};
use gitweft_git::{CancelToken, ObjectId};
use gitweft_testkit::Sandbox;

const MAX: u64 = 1024 * 1024;

fn versions(
    sb: &Sandbox,
    repo: &Path,
    source: &DiffSource,
    path: &str,
) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let path = BString::from(path);
    let versions = file_versions(
        sb.git(),
        repo,
        source,
        &path,
        None,
        MAX,
        &CancelToken::new(),
    )
    .unwrap();
    (versions.old, versions.new)
}

fn id(sb: &Sandbox, repo: &Path, rev: &str) -> ObjectId {
    sb.run(repo, &["rev-parse", rev]).trim().parse().unwrap()
}

/// Binary content, as an image would be.
fn bytes(tag: u8) -> Vec<u8> {
    vec![0x89, b'P', b'N', b'G', 0, tag, 0, tag]
}

#[test]
fn both_sides_of_every_kind_of_diff() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    std::fs::write(repo.join("a.png"), bytes(1)).unwrap();
    sb.run(&repo, &["add", "a.png"]);
    sb.run(&repo, &["commit", "-qm", "Add"]);
    let added = id(&sb, &repo, "HEAD");
    std::fs::write(repo.join("a.png"), bytes(2)).unwrap();
    sb.run(&repo, &["commit", "-qam", "Change"]);
    let changed = id(&sb, &repo, "HEAD");

    // A commit against its parent, and the root commit (no old side).
    let source = DiffSource::Commit {
        parent: Some(added),
        commit: changed,
    };
    assert_eq!(
        versions(&sb, &repo, &source, "a.png"),
        (Some(bytes(1)), Some(bytes(2)))
    );
    let root = DiffSource::Commit {
        parent: None,
        commit: added,
    };
    assert_eq!(versions(&sb, &repo, &root, "a.png"), (None, Some(bytes(1))));

    // Staged: HEAD → index; unstaged: index → working tree.
    std::fs::write(repo.join("a.png"), bytes(3)).unwrap();
    sb.run(&repo, &["add", "a.png"]);
    std::fs::write(repo.join("a.png"), bytes(4)).unwrap();
    assert_eq!(
        versions(&sb, &repo, &DiffSource::Staged, "a.png"),
        (Some(bytes(2)), Some(bytes(3)))
    );
    assert_eq!(
        versions(&sb, &repo, &DiffSource::Unstaged, "a.png"),
        (Some(bytes(3)), Some(bytes(4)))
    );

    // Untracked: only the new side; a deleted file: only the old side.
    std::fs::write(repo.join("new.png"), bytes(5)).unwrap();
    assert_eq!(
        versions(&sb, &repo, &DiffSource::Untracked, "new.png"),
        (None, Some(bytes(5)))
    );
    std::fs::remove_file(repo.join("a.png")).unwrap();
    assert_eq!(
        versions(&sb, &repo, &DiffSource::Unstaged, "a.png"),
        (Some(bytes(3)), None)
    );
}

#[test]
fn a_binary_diff_names_its_file() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    std::fs::write(repo.join("pic.png"), bytes(1)).unwrap();
    sb.run(&repo, &["add", "pic.png"]);
    let diff = gitweft_git::read::file_diff(
        sb.git(),
        &repo,
        &DiffSource::Staged,
        &BString::from("pic.png"),
        None,
        gitweft_git::read::DiffOptions::default(),
        &CancelToken::new(),
    )
    .unwrap();
    assert!(diff.binary);
    assert_eq!(diff.path, "pic.png");
}

#[test]
fn versions_over_the_limit_are_refused() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    std::fs::write(repo.join("big.bin"), vec![0u8; 4096]).unwrap();
    sb.run(&repo, &["add", "big.bin"]);
    sb.run(&repo, &["commit", "-qm", "Big"]);
    let head = id(&sb, &repo, "HEAD");
    let source = DiffSource::Commit {
        parent: None,
        commit: head,
    };
    let path = BString::from("big.bin");
    let result = file_versions(
        sb.git(),
        &repo,
        &source,
        &path,
        None,
        1000,
        &CancelToken::new(),
    );
    assert!(result.is_err());
}
