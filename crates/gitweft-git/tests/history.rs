#[cfg(unix)]
use std::ffi::OsStr;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

use gitweft_git::read::{ChangeKind, FileRevision, file_history};
use gitweft_git::{CancelToken, ObjectId};
use gitweft_testkit::Sandbox;

fn history(
    sb: &Sandbox,
    repo: &std::path::Path,
    path: &[u8],
    limit: Option<usize>,
) -> Vec<FileRevision> {
    file_history(sb.git(), repo, None, path, limit, &CancelToken::new()).unwrap()
}

fn head(sb: &Sandbox, repo: &std::path::Path) -> ObjectId {
    sb.run(repo, &["rev-parse", "HEAD"]).trim().parse().unwrap()
}

#[test]
fn follows_the_file_across_a_rename() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let body = "line\n".repeat(20);
    let added: ObjectId = sb
        .commit_file(&repo, "a.txt", &body, "Add a")
        .parse()
        .unwrap();
    let edited: ObjectId = sb
        .commit_file(&repo, "a.txt", &format!("{body}more\n"), "Edit a")
        .parse()
        .unwrap();
    sb.commit_file(&repo, "other.txt", "x\n", "Unrelated");
    sb.run(&repo, &["mv", "a.txt", "b.txt"]);
    sb.run(&repo, &["commit", "-qm", "Rename to b"]);
    let renamed = head(&sb, &repo);
    let last: ObjectId = sb
        .commit_file(&repo, "b.txt", &format!("{body}more\nlast\n"), "Edit b")
        .parse()
        .unwrap();

    let revisions = history(&sb, &repo, b"b.txt", None);
    let summary: Vec<_> = revisions
        .iter()
        .map(|r| {
            (
                r.id,
                r.kind,
                r.path.to_string(),
                r.old_path.as_ref().map(|p| p.to_string()),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (last, ChangeKind::Modified, "b.txt".into(), None),
            (
                renamed,
                ChangeKind::Renamed,
                "b.txt".into(),
                Some("a.txt".into())
            ),
            (edited, ChangeKind::Modified, "a.txt".into(), None),
            (added, ChangeKind::Added, "a.txt".into(), None),
        ]
    );
    assert_eq!(revisions[0].summary, "Edit b");
    assert_eq!(revisions[0].parents, [renamed]);
    assert_eq!(revisions[0].author.name, "Test Author");
    assert_eq!(revisions[0].author.time, 1_767_225_600);
    assert!(
        revisions[3].parents.is_empty(),
        "the file was added by the root commit"
    );
}

#[test]
fn limit_caps_the_number_of_commits() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    for n in 0..5 {
        sb.commit_file(&repo, "f.txt", &format!("{n}\n"), &format!("Commit {n}"));
    }
    let revisions = history(&sb, &repo, b"f.txt", Some(2));
    let summaries: Vec<_> = revisions.iter().map(|r| r.summary.as_str()).collect();
    assert_eq!(summaries, ["Commit 4", "Commit 3"]);
}

#[cfg(unix)]
#[test]
fn non_utf8_and_pattern_like_names_work() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let name: &[u8] = b"caf\xe9 *.txt";
    std::fs::write(repo.join(OsStr::from_bytes(name)), "1\n").unwrap();
    // A file whose name the pattern would also match must not be listed.
    std::fs::write(repo.join("cafe x.txt"), "1\n").unwrap();
    sb.run(&repo, &["add", "-A"]);
    sb.run(&repo, &["commit", "-qm", "Add both"]);
    std::fs::write(repo.join("cafe x.txt"), "2\n").unwrap();
    sb.run(&repo, &["commit", "-qam", "Edit the other"]);

    let revisions = history(&sb, &repo, name, None);
    assert_eq!(revisions.len(), 1);
    assert_eq!(revisions[0].summary, "Add both");
    assert_eq!(revisions[0].path, name);
}
