use std::path::{Path, PathBuf};

use gitweft_git::gix::bstr::BString;
use gitweft_git::read::{
    BlameChunk, BlameCommit, BlameOptions, BlameUpdate, LineMark, blame, blame_ignore_revs_file,
    commit_summaries, file_content,
};
use gitweft_git::{CancelToken, GitError, GitErrorKind, ObjectId};
use gitweft_platform::fs::canonicalize;
use gitweft_testkit::Sandbox;

fn run_blame(sb: &Sandbox, repo: &Path, rev: Option<ObjectId>, path: &str) -> Vec<BlameUpdate> {
    run_blame_with(sb, repo, rev, path, BlameOptions::default())
}

fn run_blame_with(
    sb: &Sandbox,
    repo: &Path,
    rev: Option<ObjectId>,
    path: &str,
    options: BlameOptions,
) -> Vec<BlameUpdate> {
    let mut all = Vec::new();
    blame(
        sb.git(),
        repo,
        rev,
        path.as_bytes(),
        options,
        &CancelToken::new(),
        |updates| all.extend(updates),
    )
    .unwrap();
    all
}

fn head(sb: &Sandbox, repo: &Path) -> ObjectId {
    sb.run(repo, &["rev-parse", "HEAD"]).trim().parse().unwrap()
}

fn commits(updates: &[BlameUpdate]) -> Vec<&BlameCommit> {
    updates
        .iter()
        .filter_map(|u| match u {
            BlameUpdate::Commit(c) => Some(c),
            _ => None,
        })
        .collect()
}

fn chunks(updates: &[BlameUpdate]) -> Vec<&BlameChunk> {
    updates
        .iter()
        .filter_map(|u| match u {
            BlameUpdate::Chunk(c) => Some(c),
            _ => None,
        })
        .collect()
}

/// The marks sent, if any; they come once, after everything else.
fn marks(updates: &[BlameUpdate]) -> Option<Vec<(u32, LineMark)>> {
    let at = updates
        .iter()
        .position(|u| matches!(u, BlameUpdate::Marks(_)))?;
    assert_eq!(at, updates.len() - 1, "marks before the end");
    match &updates[at] {
        BlameUpdate::Marks(marks) => Some(marks.clone()),
        _ => None,
    }
}

/// The commit owning each line of the file, in file order. Also checks that every chunk
/// comes after its commit and that the chunks cover each line exactly once.
fn line_owners(updates: &[BlameUpdate]) -> Vec<ObjectId> {
    let mut known = Vec::new();
    let mut owners: Vec<Option<ObjectId>> = Vec::new();
    for update in updates {
        match update {
            BlameUpdate::Commit(c) => known.push(c.id),
            BlameUpdate::Chunk(c) => {
                assert!(known.contains(&c.commit), "chunk before its commit");
                let end = (c.final_line + c.lines - 1) as usize;
                if owners.len() < end {
                    owners.resize(end, None);
                }
                for line in c.final_line as usize..=end {
                    assert!(owners[line - 1].is_none(), "line {line} blamed twice");
                    owners[line - 1] = Some(c.commit);
                }
            }
            BlameUpdate::Marks(_) => {}
        }
    }
    owners
        .into_iter()
        .map(|o| o.expect("every line blamed"))
        .collect()
}

#[test]
fn lines_belong_to_the_commit_that_last_changed_them() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let first: ObjectId = sb
        .commit_file(&repo, "f.txt", "one\ntwo\nthree\n", "First")
        .parse()
        .unwrap();
    let second: ObjectId = sb
        .commit_file(&repo, "f.txt", "one\nTWO\nthree\n", "Second")
        .parse()
        .unwrap();

    let updates = run_blame(&sb, &repo, Some(second), "f.txt");
    assert_eq!(line_owners(&updates), [first, second, first]);

    let commits = commits(&updates);
    assert_eq!(commits.len(), 2, "each commit is reported once");
    let info = commits.iter().find(|c| c.id == second).unwrap();
    assert_eq!(info.summary, "Second");
    assert_eq!(info.author.name, "Test Author");
    assert_eq!(info.author.email, "author@example.invalid");
    assert_eq!(info.author.time, 1_767_225_600); // FIXED_DATE
    assert_eq!(info.previous, Some((first, BString::from("f.txt"))));
    let root = commits.iter().find(|c| c.id == first).unwrap();
    assert!(!root.boundary, "the first commit owns its lines (--root)");
    assert_eq!(root.previous, None);
}

#[test]
fn working_tree_blame_marks_uncommitted_lines() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let first: ObjectId = sb
        .commit_file(&repo, "f.txt", "one\ntwo\nthree\n", "First")
        .parse()
        .unwrap();
    std::fs::write(repo.join("f.txt"), "one\ntwo\nTHREE\nfour\n").unwrap();

    let owners = line_owners(&run_blame(&sb, &repo, None, "f.txt"));
    assert_eq!(owners[..2], [first, first]);
    assert!(owners[2].is_null() && owners[3].is_null());

    let text = file_content(sb.git(), &repo, None, b"f.txt", &CancelToken::new()).unwrap();
    assert_eq!(text, b"one\ntwo\nTHREE\nfour\n");
}

#[test]
fn lines_keep_their_origin_across_a_rename() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let base = "alpha\nbeta\ngamma\ndelta\nepsilon\n";
    let first: ObjectId = sb
        .commit_file(&repo, "a.txt", base, "Add a")
        .parse()
        .unwrap();
    sb.run(&repo, &["mv", "a.txt", "b.txt"]);
    std::fs::write(repo.join("b.txt"), base.replace("alpha", "ALPHA")).unwrap();
    sb.run(&repo, &["commit", "-qam", "Rename to b"]);
    let second: ObjectId = sb
        .run(&repo, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    let updates = run_blame(&sb, &repo, Some(second), "b.txt");
    assert_eq!(line_owners(&updates), [second, first, first, first, first]);
    for chunk in chunks(&updates) {
        let expected = if chunk.commit == first {
            "a.txt"
        } else {
            "b.txt"
        };
        assert_eq!(chunk.orig_path, expected);
    }
    let renamed = commits(&updates)
        .into_iter()
        .find(|c| c.id == second)
        .unwrap();
    assert_eq!(renamed.previous, Some((first, BString::from("a.txt"))));

    let old = file_content(sb.git(), &repo, Some(first), b"a.txt", &CancelToken::new()).unwrap();
    assert_eq!(old, base.as_bytes());
}

#[test]
fn cancelled_blame_reports_cancellation() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "f.txt", "one\n", "First");
    let cancel = CancelToken::new();
    cancel.cancel();

    let err = blame(
        sb.git(),
        &repo,
        None,
        b"f.txt",
        BlameOptions::default(),
        &cancel,
        |_| {},
    )
    .unwrap_err();
    let err = err.downcast_ref::<GitError>().expect("a git error");
    assert_eq!(err.kind, GitErrorKind::Cancelled);
}

/// A repository whose second commit only reindents the first one's function body.
fn reindented(sb: &Sandbox) -> (PathBuf, ObjectId, ObjectId) {
    let repo = sb.init_repo("r");
    let first: ObjectId = sb
        .commit_file(&repo, "f.rs", "fn foo() {\n  a();\n  b();\n}\n", "Add foo")
        .parse()
        .unwrap();
    let reindent: ObjectId = sb
        .commit_file(
            &repo,
            "f.rs",
            "fn foo() {\n    a();\n    b();\n}\n",
            "Reindent",
        )
        .parse()
        .unwrap();
    (repo, first, reindent)
}

#[test]
fn an_ignored_commit_gives_its_lines_back_to_the_earlier_one() {
    let sb = Sandbox::new();
    let (repo, first, reindent) = reindented(&sb);
    assert_eq!(
        line_owners(&run_blame(&sb, &repo, None, "f.rs")),
        [first, reindent, reindent, first]
    );

    // An id that isn't in the repository is skipped (git would refuse it).
    let unknown = ObjectId::from_hex(&[b'1'; 40]).unwrap();
    let options = BlameOptions {
        ignore_revs: vec![unknown, reindent],
        ..BlameOptions::default()
    };
    let updates = run_blame_with(&sb, &repo, Some(reindent), "f.rs", options);
    assert_eq!(line_owners(&updates), [first; 4]);
    assert!(commits(&updates).iter().all(|c| c.id == first));
}

#[test]
fn lines_shaped_by_an_ignored_commit_are_marked() {
    use LineMark::*;
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let first: ObjectId = sb
        .commit_file(&repo, "f.rs", "fn foo() {\n  a();\n  b();\n}\n", "Add foo")
        .parse()
        .unwrap();
    let reindent: ObjectId = sb
        .commit_file(
            &repo,
            "f.rs",
            "fn foo() {\n    a();\n    b();\n    c();\n}\n",
            "Reindent, add c",
        )
        .parse()
        .unwrap();
    // Nothing ignored: no marks, and no second blame.
    assert_eq!(marks(&run_blame(&sb, &repo, None, "f.rs")), None);

    let options = BlameOptions {
        ignore_revs: vec![reindent],
        ..BlameOptions::default()
    };
    let updates = run_blame_with(&sb, &repo, None, "f.rs", options);
    assert_eq!(
        line_owners(&updates),
        [first, first, first, reindent, first]
    );
    // a() and b() passed through the reindent; c() has no older commit.
    let expected = [(2, Ignored), (3, Ignored), (4, Unblamable)];
    assert_eq!(marks(&updates).as_deref(), Some(&expected[..]));

    // The same through the repository's list, and at a commit.
    std::fs::write(repo.join(".git-blame-ignore-revs"), format!("{reindent}\n")).unwrap();
    sb.run(
        &repo,
        &["config", "blame.ignoreRevsFile", ".git-blame-ignore-revs"],
    );
    let updates = run_blame(&sb, &repo, Some(reindent), "f.rs");
    assert_eq!(marks(&updates).as_deref(), Some(&expected[..]));

    // The user's config can't switch the marks off.
    sb.run(&repo, &["config", "blame.markIgnoredLines", "false"]);
    let updates = run_blame(&sb, &repo, None, "f.rs");
    assert_eq!(marks(&updates).as_deref(), Some(&expected[..]));
}

#[test]
fn ignored_commits_are_listed_with_their_summaries() {
    let sb = Sandbox::new();
    let (repo, first, reindent) = reindented(&sb);
    let unknown = ObjectId::from_hex(&[b'1'; 40]).unwrap();
    let listed = commit_summaries(
        sb.git(),
        &repo,
        &[reindent, unknown, first],
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(
        listed,
        [
            (reindent, Some("Reindent".to_owned())),
            (unknown, None),
            (first, Some("Add foo".to_owned())),
        ]
    );
    let none = commit_summaries(sb.git(), &repo, &[], &CancelToken::new()).unwrap();
    assert!(none.is_empty());
}

#[test]
fn the_repository_ignore_revs_file_is_honoured() {
    let sb = Sandbox::new();
    let (repo, first, reindent) = reindented(&sb);
    std::fs::write(
        repo.join(".git-blame-ignore-revs"),
        format!("# Reformatting\n{reindent}\n"),
    )
    .unwrap();
    sb.run(
        &repo,
        &["config", "blame.ignoreRevsFile", ".git-blame-ignore-revs"],
    );

    assert_eq!(
        line_owners(&run_blame(&sb, &repo, None, "f.rs")),
        [first; 4]
    );
}

#[test]
fn a_missing_ignore_revs_file_does_not_break_blame() {
    let sb = Sandbox::new();
    let (repo, first, reindent) = reindented(&sb);
    sb.run(&repo, &["config", "blame.ignoreRevsFile", "missing-file"]);
    assert_eq!(
        line_owners(&run_blame(&sb, &repo, None, "f.rs")),
        [first, reindent, reindent, first]
    );

    // Configured files that do exist still count next to the missing one (and an empty
    // value, which git skips).
    let existing = sb.path().join("ignored");
    std::fs::write(&existing, format!("{reindent}\n")).unwrap();
    sb.run(
        &repo,
        &[
            "config",
            "--add",
            "blame.ignoreRevsFile",
            existing.to_str().unwrap(),
        ],
    );
    sb.run(&repo, &["config", "--add", "blame.ignoreRevsFile", ""]);
    assert_eq!(
        line_owners(&run_blame(&sb, &repo, None, "f.rs")),
        [first; 4]
    );
}

#[test]
fn the_ignore_revs_file_resolves_from_the_working_tree_root() {
    let sb = Sandbox::new();
    let (repo, _, _) = reindented(&sb);
    let cancel = CancelToken::new();
    assert_eq!(
        blame_ignore_revs_file(sb.git(), &repo, &cancel).unwrap(),
        None
    );

    sb.run(
        &repo,
        &["config", "blame.ignoreRevsFile", ".git-blame-ignore-revs"],
    );
    let expected = canonicalize(&repo).unwrap().join(".git-blame-ignore-revs");
    let sub = repo.join("sub");
    std::fs::create_dir(&sub).unwrap();
    for cwd in [&repo, &sub] {
        let found = blame_ignore_revs_file(sb.git(), cwd, &cancel)
            .unwrap()
            .unwrap();
        assert_eq!(canonicalize(&found).unwrap_or(found), expected);
    }

    // The last value wins, as with `git config --get`; an absolute path stays as it is.
    let absolute = sb.path().join("elsewhere");
    sb.run(
        &repo,
        &[
            "config",
            "--add",
            "blame.ignoreRevsFile",
            absolute.to_str().unwrap(),
        ],
    );
    assert_eq!(
        blame_ignore_revs_file(sb.git(), &repo, &cancel).unwrap(),
        Some(absolute)
    );
}

#[test]
fn a_commits_previous_blames_the_parent_across_a_rename() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let base = "alpha\nbeta\ngamma\ndelta\n";
    let first: ObjectId = sb
        .commit_file(&repo, "a.txt", base, "Add a")
        .parse()
        .unwrap();
    sb.run(&repo, &["mv", "a.txt", "b.txt"]);
    let renamed_text = base.replace("alpha", "ALPHA");
    std::fs::write(repo.join("b.txt"), &renamed_text).unwrap();
    sb.run(&repo, &["commit", "-qam", "Rename to b"]);
    let renamed = head(&sb, &repo);
    let last: ObjectId = sb
        .commit_file(
            &repo,
            "b.txt",
            &renamed_text.replace("beta", "BETA"),
            "Beta",
        )
        .parse()
        .unwrap();

    let updates = run_blame(&sb, &repo, Some(last), "b.txt");
    assert_eq!(line_owners(&updates), [renamed, last, first, first]);
    let previous = |id: ObjectId| {
        commits(&updates)
            .into_iter()
            .find(|c| c.id == id)
            .and_then(|c| c.previous.clone())
            .unwrap()
    };

    // "Blame the parent" of the last commit: the file as the rename left it.
    let (parent, path) = previous(last);
    assert_eq!((parent, path.as_slice()), (renamed, &b"b.txt"[..]));
    let before = run_blame(&sb, &repo, Some(parent), &path.to_string());
    assert_eq!(line_owners(&before), [renamed, first, first, first]);

    // And of the rename: the old path in the first commit.
    let (parent, path) = previous(renamed);
    assert_eq!((parent, path.as_slice()), (first, &b"a.txt"[..]));
    let before = run_blame(&sb, &repo, Some(parent), &path.to_string());
    assert_eq!(line_owners(&before), [first; 4]);
}
