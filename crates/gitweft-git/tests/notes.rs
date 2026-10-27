use std::path::{Path, PathBuf};

use gitweft_git::notes::{REVIEW_NOTES_REF, add_note, read_note, remove_note};
use gitweft_git::{CancelToken, GitErrorKind, ObjectId};
use gitweft_testkit::Sandbox;

fn id(hex: &str) -> ObjectId {
    ObjectId::from_hex(hex.trim().as_bytes()).unwrap()
}

/// A repository with two commits; returns it and their ids, oldest first.
fn setup(sb: &Sandbox) -> (PathBuf, [ObjectId; 2]) {
    let repo = sb.init_repo("r");
    let first = id(&sb.commit_file(&repo, "a.txt", "1\n", "first"));
    let second = id(&sb.commit_file(&repo, "a.txt", "2\n", "second"));
    (repo, [first, second])
}

fn add(sb: &Sandbox, repo: &Path, notes_ref: &str, commit: ObjectId, text: &str) {
    add_note(sb.git(), repo, notes_ref, commit, text, &CancelToken::new())
        .unwrap_or_else(|e| panic!("{e:?}"));
}

fn note(sb: &Sandbox, repo: &Path, notes_ref: &str, commit: ObjectId) -> Option<String> {
    read_note(sb.git(), repo, notes_ref, commit, &CancelToken::new())
        .unwrap_or_else(|e| panic!("{e:?}"))
}

fn refs(sb: &Sandbox, repo: &Path) -> String {
    sb.run(repo, &["for-each-ref", "--format=%(refname)"])
}

/// Markdown that `git notes add -m/-F` would clean up: trailing spaces (a line break in
/// Markdown), runs of blank lines, a CR, no final newline. And `#` headings, which stay
/// either way.
const REVIEW: &str = "# Review of `feature`\n\n\n## a.rs  \nline one  \nline two\n\n\n\
                      - [x] done\r\nno final newline";

#[test]
fn add_overwrite_and_read_back() {
    let sb = Sandbox::new();
    let (repo, [first, head]) = setup(&sb);
    // No notes ref at all yet.
    assert_eq!(note(&sb, &repo, REVIEW_NOTES_REF, head), None);

    add(&sb, &repo, REVIEW_NOTES_REF, head, REVIEW);
    assert_eq!(
        note(&sb, &repo, REVIEW_NOTES_REF, head).as_deref(),
        Some(REVIEW)
    );
    // The short name is the same ref.
    assert_eq!(note(&sb, &repo, "review", head).as_deref(), Some(REVIEW));
    assert_eq!(note(&sb, &repo, REVIEW_NOTES_REF, first), None);
    // Not in the default notes ref (`git log` shows those).
    assert_eq!(note(&sb, &repo, "commits", head), None);

    // Overwriting makes a second commit on the notes ref; the reviewed commit is untouched.
    add(&sb, &repo, "review", head, "second pass\n");
    assert_eq!(
        note(&sb, &repo, REVIEW_NOTES_REF, head).as_deref(),
        Some("second pass\n")
    );
    assert_eq!(
        sb.run(&repo, &["rev-list", "--count", REVIEW_NOTES_REF])
            .trim(),
        "2"
    );
    assert_eq!(id(&sb.run(&repo, &["rev-parse", "HEAD"])), head);
    assert_eq!(
        sb.run(&repo, &["status", "--porcelain"]),
        "",
        "the work tree and index are untouched"
    );
}

#[test]
fn empty_text_removes_the_note() {
    let sb = Sandbox::new();
    let (repo, [first, head]) = setup(&sb);
    add(&sb, &repo, "review", head, "keep me\n");
    add(&sb, &repo, "review", first, "\n");
    // Whitespace is text: stored as it is.
    assert_eq!(note(&sb, &repo, "review", first).as_deref(), Some("\n"));

    add(&sb, &repo, "review", first, "");
    assert_eq!(note(&sb, &repo, "review", first), None);
    assert_eq!(
        note(&sb, &repo, "review", head).as_deref(),
        Some("keep me\n")
    );

    let cancel = CancelToken::new();
    remove_note(sb.git(), &repo, "review", head, &cancel).unwrap();
    assert_eq!(note(&sb, &repo, "review", head), None);
    // Removing what isn't there, even from a notes ref that doesn't exist, is fine.
    remove_note(sb.git(), &repo, "review", head, &cancel).unwrap();
    remove_note(sb.git(), &repo, "never", head, &cancel).unwrap();
}

#[test]
fn notes_refs_stay_under_refs_notes() {
    let sb = Sandbox::new();
    let (repo, [_, head]) = setup(&sb);
    add(&sb, &repo, "review", head, "a\n");
    add(&sb, &repo, "notes/other", head, "b\n");
    // A branch's full name is a name under refs/notes/, not the branch.
    add(&sb, &repo, "refs/heads/main", head, "c\n");
    add(&sb, &repo, "-x", head, "d\n");
    assert_eq!(
        refs(&sb, &repo),
        "refs/heads/main\nrefs/notes/-x\nrefs/notes/other\nrefs/notes/refs/heads/main\n\
         refs/notes/review\n"
    );
    assert_eq!(id(&sb.run(&repo, &["rev-parse", "refs/heads/main"])), head);
    assert_eq!(
        note(&sb, &repo, "refs/notes/refs/heads/main", head).as_deref(),
        Some("c\n")
    );
    assert_eq!(note(&sb, &repo, "-x", head).as_deref(), Some("d\n"));

    // A name git can't store is git's to refuse.
    let err = add_note(sb.git(), &repo, "../x", head, "e\n", &CancelToken::new()).unwrap_err();
    assert!(err.summary().contains("bad name"), "{err:?}");
}

#[test]
fn missing_commits_and_missing_repositories() {
    let sb = Sandbox::new();
    let (repo, [_, head]) = setup(&sb);
    add(&sb, &repo, "review", head, "a\n");
    // A full id isn't resolved: an object that doesn't exist just has no note.
    let missing = ObjectId::from_hex(b"0123456789012345678901234567890123456789").unwrap();
    assert_eq!(note(&sb, &repo, "review", missing), None);

    let elsewhere = sb.path().join("not-a-repo");
    std::fs::create_dir(&elsewhere).unwrap();
    let err = read_note(sb.git(), &elsewhere, "review", head, &CancelToken::new()).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::NotARepo, "{err:?}");
}
