//! Partial clones (#82): reads never fetch missing blobs by themselves; `fetch_missing`
//! gets exactly those a view needs, on request. A `file://` remote serves the filter.

use std::path::PathBuf;

use gitweft_git::gix::bstr::BString;
use gitweft_git::partial::{
    fetch_history, fetch_missing, history_blobs, is_missing_objects, missing_blobs, promisor_remote,
};
use gitweft_git::read::{
    BlameOptions, DiffOptions, DiffSource, blame, commit_details, file_content, file_diff,
};
use gitweft_git::{CancelToken, GitVersion, ObjectId};
use gitweft_testkit::Sandbox;

/// A source with three versions of a.txt, and a blobless clone of it (only HEAD's blobs,
/// which the checkout needed, are there).
fn blobless(sb: &Sandbox) -> (PathBuf, PathBuf) {
    let source = sb.init_repo("source");
    sb.run(&source, &["config", "uploadpack.allowFilter", "true"]);
    for text in ["one\n", "two\n", "three\n"] {
        sb.commit_file(&source, "a.txt", text, text.trim());
    }
    let url = format!("file://{}", source.display());
    sb.run(
        sb.path(),
        &["clone", "-q", "--filter=blob:none", &url, "clone"],
    );
    (source, sb.path().join("clone"))
}

/// `GIT_NO_LAZY_FETCH` is honoured from git 2.44: older git fetches by itself, which
/// nothing in the app can stop (OFFLINE.md says so).
fn honours_no_lazy_fetch(sb: &Sandbox) -> bool {
    let yes = sb.git().version() >= GitVersion::new(2, 44, 0);
    if !yes {
        eprintln!("note: git before 2.44 ignores GIT_NO_LAZY_FETCH; skipping this test");
    }
    yes
}

fn id(sb: &Sandbox, repo: &std::path::Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(sb.run(repo, &["rev-parse", rev]).trim().as_bytes()).unwrap()
}

#[test]
fn a_read_stops_at_missing_blobs_and_fetch_missing_gets_them() {
    let sb = Sandbox::new();
    if !honours_no_lazy_fetch(&sb) {
        return;
    }
    let (_, clone) = blobless(&sb);
    let cancel = CancelToken::new();
    let source = DiffSource::Commit {
        parent: Some(id(&sb, &clone, "HEAD~2")),
        commit: id(&sb, &clone, "HEAD~1"),
    };
    let path = BString::from("a.txt");
    let diff = || {
        file_diff(
            sb.git(),
            &clone,
            &source,
            &path,
            None,
            DiffOptions::default(),
            &cancel,
        )
    };

    // "one" and "two" were never downloaded, and the diff doesn't download them.
    let err = diff().unwrap_err();
    assert!(is_missing_objects(err.as_ref()), "{err}");
    let files = [
        ("HEAD~1".to_owned(), path.clone()),
        ("HEAD~2".to_owned(), path.clone()),
        ("HEAD".to_owned(), path.clone()),
        // Not in that commit: left out.
        ("HEAD".to_owned(), BString::from("nothing.txt")),
    ];
    assert_eq!(
        missing_blobs(sb.git(), &clone, &files, &cancel)
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        missing_blobs(sb.git(), &clone, &files, &cancel)
            .unwrap()
            .len(),
        2,
        "checking fetched nothing"
    );
    assert_eq!(
        promisor_remote(sb.git(), &clone, &cancel)
            .unwrap()
            .as_deref(),
        Some("origin")
    );

    // On request: exactly those two, then the diff reads.
    assert_eq!(fetch_missing(sb.git(), &clone, &files, &cancel).unwrap(), 2);
    let diff = diff().unwrap();
    assert_eq!(diff.hunks.len(), 1);
    assert!(
        missing_blobs(sb.git(), &clone, &files, &cancel)
            .unwrap()
            .is_empty()
    );
    assert_eq!(fetch_missing(sb.git(), &clone, &files, &cancel).unwrap(), 0);
}

/// A commit's details in a blobless clone: its files, without line counts, rather than an
/// error or a download (#82).
#[test]
fn details_list_the_files_without_counts_when_the_contents_are_missing() {
    let sb = Sandbox::new();
    if !honours_no_lazy_fetch(&sb) {
        return;
    }
    let (_, clone) = blobless(&sb);
    let cancel = CancelToken::new();
    let before = missing_blobs(
        sb.git(),
        &clone,
        &[("HEAD~1".to_owned(), BString::from("a.txt"))],
        &cancel,
    )
    .unwrap();
    assert_eq!(before.len(), 1);

    let details = commit_details(
        sb.git(),
        &clone.join(".git"),
        id(&sb, &clone, "HEAD"),
        &cancel,
    )
    .unwrap();
    assert_eq!(details.changes.len(), 1);
    assert_eq!(details.changes[0].path, "a.txt");
    assert_eq!(details.changes[0].lines, None);
    assert_eq!(
        missing_blobs(
            sb.git(),
            &clone,
            &[("HEAD~1".to_owned(), BString::from("a.txt"))],
            &cancel
        )
        .unwrap(),
        before,
        "nothing downloaded"
    );
}

/// Blame and an old version of a file in a blobless clone (#87): both say the contents are
/// missing rather than fetching them; `fetch_history` gets every version, then both read.
#[test]
fn blame_and_old_versions_fetch_the_file_history_on_request() {
    let sb = Sandbox::new();
    if !honours_no_lazy_fetch(&sb) {
        return;
    }
    let (_, clone) = blobless(&sb);
    let cancel = CancelToken::new();
    let path = BString::from("a.txt");
    let old = id(&sb, &clone, "HEAD~1");

    let err = file_content(sb.git(), &clone, Some(old), &path, &cancel).unwrap_err();
    assert!(is_missing_objects(err.as_ref()), "{err}");
    let run_blame = || {
        blame(
            sb.git(),
            &clone,
            None,
            &path,
            BlameOptions::default(),
            &cancel,
            |_| {},
        )
    };
    let err = run_blame().unwrap_err();
    assert!(is_missing_objects(err.as_ref()), "{err}");
    assert_eq!(
        history_blobs(sb.git(), &clone, "HEAD", path.as_ref(), &cancel)
            .unwrap()
            .len(),
        3
    );

    assert_eq!(
        fetch_history(sb.git(), &clone, "HEAD", path.as_ref(), &cancel).unwrap(),
        2
    );
    assert_eq!(
        file_content(sb.git(), &clone, Some(old), &path, &cancel).unwrap(),
        b"two\n"
    );
    run_blame().unwrap();
}
