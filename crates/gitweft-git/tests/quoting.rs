//! Paths git quotes, or reads as something else: a newline, a leading quote and a tab in
//! a name, and a name that looks like an index stage (`1:x`). Each goes through the reads and
//! writes that used to lose or misread it. Windows allows none of these names.

#![cfg(unix)]

use std::path::Path;

use gitweft_git::CancelToken;
use gitweft_git::gix::bstr::BString;
use gitweft_git::merge::merge_preview;
use gitweft_git::ops::hash_paths;
use gitweft_git::read::{DiffSource, file_versions};
use gitweft_testkit::Sandbox;

/// Names git C-quotes, or that don't survive one path per line.
const AWKWARD: [&str; 3] = ["new\nline.txt", "\"quoted.txt", "tab\there.txt"];

/// `git hash-object --no-filters` of one file, asked about that file alone.
fn blob_of(sb: &Sandbox, repo: &Path, name: &str) -> String {
    sb.run(repo, &["hash-object", "--no-filters", "--", name])
        .trim()
        .to_owned()
}

#[test]
fn awkward_names_hash_as_themselves() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    for (n, name) in AWKWARD.iter().enumerate() {
        std::fs::write(repo.join(name), format!("content {n}\n")).unwrap();
    }
    // A plain file too, whose id a stray unquote would shift onto its neighbour.
    std::fs::write(repo.join("plain.txt"), "plain\n").unwrap();

    let names: Vec<BString> = AWKWARD
        .iter()
        .chain(&["plain.txt"])
        .map(|name| BString::from(*name))
        .collect();
    let refs: Vec<&BString> = names.iter().collect();
    let ids = hash_paths(sb.git(), &repo, &refs, false, &CancelToken::new()).unwrap();

    let expected: Vec<String> = AWKWARD
        .iter()
        .chain(&["plain.txt"])
        .map(|name| blob_of(&sb, &repo, name))
        .collect();
    let got: Vec<String> = ids.iter().map(ToString::to_string).collect();
    assert_eq!(got, expected);
}

#[test]
fn a_merge_preview_names_awkward_conflicts_as_they_are() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    for name in AWKWARD {
        sb.commit_file(&repo, name, "base\n", "base");
    }
    sb.run(&repo, &["switch", "-q", "-c", "feature"]);
    for name in AWKWARD {
        sb.commit_file(&repo, name, "feature\n", "feature");
    }
    sb.run(&repo, &["switch", "-q", "main"]);
    for name in AWKWARD {
        sb.commit_file(&repo, name, "main\n", "main");
    }

    let preview = merge_preview(sb.git(), &repo, "HEAD", "feature", &CancelToken::new()).unwrap();
    let mut conflicts = preview.conflicts.clone();
    conflicts.sort();
    let mut expected: Vec<BString> = AWKWARD.iter().map(|name| BString::from(*name)).collect();
    expected.sort();
    assert_eq!(conflicts, expected);
}

#[test]
fn a_staged_file_named_like_an_index_stage_is_read_from_the_index() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "1:x", "committed\n", "base");
    std::fs::write(repo.join("1:x"), "staged\n").unwrap();
    sb.run(&repo, &["add", "--", "1:x"]);
    std::fs::write(repo.join("1:x"), "in the working tree\n").unwrap();

    let path = BString::from("1:x");
    let cancel = CancelToken::new();
    let staged = file_versions(
        sb.git(),
        &repo,
        &DiffSource::Staged,
        &path,
        None,
        1 << 20,
        &cancel,
    )
    .unwrap();
    assert_eq!(staged.old.as_deref(), Some(&b"committed\n"[..]));
    assert_eq!(
        staged.new.as_deref(),
        Some(&b"staged\n"[..]),
        "`:1:x` would be stage 1 of x"
    );

    let unstaged = file_versions(
        sb.git(),
        &repo,
        &DiffSource::Unstaged,
        &path,
        None,
        1 << 20,
        &cancel,
    )
    .unwrap();
    assert_eq!(unstaged.old.as_deref(), Some(&b"staged\n"[..]));
    assert_eq!(unstaged.new.as_deref(), Some(&b"in the working tree\n"[..]));
}
