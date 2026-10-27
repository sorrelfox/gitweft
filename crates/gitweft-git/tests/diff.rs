use gitweft_git::CancelToken;
use gitweft_git::gix::bstr::BString;
use gitweft_git::read::{
    DiffOptions, DiffSource, FULL_CONTEXT, FileDiff, LineKind, file_diff, whole_diff,
};
use gitweft_testkit::Sandbox;

fn diff(sb: &Sandbox, repo: &std::path::Path, source: DiffSource, path: &str) -> FileDiff {
    file_diff(
        sb.git(),
        repo,
        &source,
        &BString::from(path),
        None,
        DiffOptions::default(),
        &CancelToken::new(),
    )
    .unwrap()
}

fn kinds(diff: &FileDiff) -> Vec<LineKind> {
    diff.hunks
        .iter()
        .flat_map(|h| h.lines.iter().map(|l| l.kind))
        .collect()
}

#[test]
fn unstaged_and_staged_changes_are_separate() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    std::fs::write(repo.join("f.txt"), "one\ntwo\nthree\n").unwrap();
    sb.run(&repo, &["add", "."]);
    sb.run(&repo, &["commit", "-qm", "base"]);

    std::fs::write(repo.join("f.txt"), "one\nTWO\nthree\n").unwrap();
    sb.run(&repo, &["add", "f.txt"]);
    std::fs::write(repo.join("f.txt"), "one\nTWO\nthree\nfour").unwrap();

    use LineKind::*;
    let staged = diff(&sb, &repo, DiffSource::Staged, "f.txt");
    assert_eq!(kinds(&staged), [Context, Removed, Added, Context]);
    assert_eq!(staged.hunks[0].lines[2].text, "TWO");

    let unstaged = diff(&sb, &repo, DiffSource::Unstaged, "f.txt");
    // "three" gains no newline marker change; "four" is added without a trailing newline.
    let last = unstaged.hunks[0].lines.last().unwrap();
    assert_eq!(last.text, "four");
    assert_eq!(last.kind, Added);
    assert!(last.no_newline);
    assert_eq!(last.new_line, Some(4));
}

#[test]
fn context_size_and_ignoring_whitespace() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let base: String = (1..=30).map(|i| format!("line {i}\n")).collect();
    sb.commit_file(&repo, "f.txt", &base, "base");
    // Line 5 changes content, line 20 only its indentation.
    let changed = base
        .replace("line 5\n", "line five\n")
        .replace("line 20\n", "    line 20\n");
    std::fs::write(repo.join("f.txt"), &changed).unwrap();
    let with = |options: DiffOptions| {
        file_diff(
            sb.git(),
            &repo,
            &DiffSource::Unstaged,
            &BString::from("f.txt"),
            None,
            options,
            &CancelToken::new(),
        )
        .unwrap()
    };

    let default = with(DiffOptions::default());
    assert_eq!(default.hunks.len(), 2);
    assert_eq!(default.hunks[0].lines.len(), 3 + 2 + 3);

    // The hunks are 8 lines apart: ten lines of context join them.
    let wide = with(DiffOptions {
        context: 10,
        ..DiffOptions::default()
    });
    assert_eq!(wide.hunks.len(), 1);

    let full = with(DiffOptions {
        context: FULL_CONTEXT,
        ..DiffOptions::default()
    });
    assert_eq!(full.hunks.len(), 1);
    assert_eq!(
        full.line_count(),
        30 + 2,
        "every line, plus two replacements"
    );

    let no_whitespace = with(DiffOptions {
        ignore_whitespace: true,
        ..DiffOptions::default()
    });
    assert_eq!(no_whitespace.hunks.len(), 1);
    let changed_texts: Vec<String> = no_whitespace.hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind != LineKind::Context)
        .map(|l| l.text.to_string())
        .collect();
    assert_eq!(changed_texts, ["line 5", "line five"]);
}

#[test]
fn a_commits_diff_of_a_path_that_isnt_utf8() {
    use std::os::unix::ffi::OsStrExt as _;
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "a\n", "base");
    let base = sb
        .run(&repo, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    let name: &[u8] = b"caf\xe9.txt";
    std::fs::write(repo.join(std::ffi::OsStr::from_bytes(name)), "hello cafe\n").unwrap();
    sb.run(&repo, &["add", "."]);
    sb.run(&repo, &["commit", "-q", "-m", "Add a Latin-1 name"]);
    let head = sb
        .run(&repo, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    // The path as git has it, as the commit details give it now.
    let source = DiffSource::Commit {
        parent: Some(base),
        commit: head,
    };
    let diff = file_diff(
        sb.git(),
        &repo,
        &source,
        &BString::from(name),
        None,
        DiffOptions::default(),
        &CancelToken::new(),
    )
    .unwrap();
    let lines: Vec<String> = diff
        .hunks
        .iter()
        .flat_map(|hunk| hunk.lines.iter().map(|line| line.text.to_string()))
        .collect();
    assert_eq!(lines, ["hello cafe"], "{diff:?}");
    assert_eq!(diff.path.as_slice(), name);
}

#[test]
fn a_mode_change_is_reported_with_or_without_content_changes() {
    use std::os::unix::fs::PermissionsExt as _;
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.sh", "a\n", "a");
    sb.commit_file(&repo, "b.sh", "b\n", "b");
    for name in ["a.sh", "b.sh"] {
        let file = repo.join(name);
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(repo.join("b.sh"), "b\nmore\n").unwrap();
    let executable = Some(("100644".to_owned(), "100755".to_owned()));

    // A chmod alone: git prints only `old mode`/`new mode`, no hunk. An earlier version
    // returned an empty diff here.
    let only = diff(&sb, &repo, DiffSource::Unstaged, "a.sh");
    assert_eq!(only.mode_change, executable);
    assert!(only.hunks.is_empty());
    // A chmod and an edit: both.
    let both = diff(&sb, &repo, DiffSource::Unstaged, "b.sh");
    assert_eq!(both.mode_change, executable);
    assert_eq!(kinds(&both), [LineKind::Context, LineKind::Added]);
}

#[test]
fn untracked_binary_and_clean_files() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "clean.txt", "x\n", "base");
    std::fs::write(repo.join("new.txt"), "a\nb\n").unwrap();
    std::fs::write(repo.join("blob.bin"), [0u8, 1, 2, 3]).unwrap();

    let new = diff(&sb, &repo, DiffSource::Untracked, "new.txt");
    assert_eq!(kinds(&new), [LineKind::Added, LineKind::Added]);
    assert_eq!(new.hunks[0].header(), "@@ -0,0 +1,2 @@");

    assert!(diff(&sb, &repo, DiffSource::Untracked, "blob.bin").binary);
    // No changes: an empty diff, not an error.
    assert!(
        diff(&sb, &repo, DiffSource::Unstaged, "clean.txt")
            .hunks
            .is_empty()
    );
}

/// A root commit is compared with the empty tree of its own hash: SHA-1's empty tree
/// doesn't exist in a SHA-256 repository ("unknown revision").
#[test]
fn a_root_commit_in_a_sha256_repository() {
    let sb = Sandbox::new();
    let repo = sb.init_sha256_repo("r");
    let root = sb.commit_file(&repo, "f.txt", "one\ntwo\n", "root");
    assert_eq!(root.len(), 64);

    let root_diff = diff(
        &sb,
        &repo,
        DiffSource::Commit {
            parent: None,
            commit: root.parse().unwrap(),
        },
        "f.txt",
    );
    assert_eq!(kinds(&root_diff), [LineKind::Added, LineKind::Added]);
}

#[test]
fn object_format_names_the_repositorys_hash() {
    use gitweft_git::gix::hash::Kind;
    use gitweft_git::read::object_format;
    let sb = Sandbox::new();
    let cancel = CancelToken::new();
    let sha1 = sb.init_repo("sha1");
    let sha256 = sb.init_sha256_repo("sha256");
    assert_eq!(object_format(sb.git(), &sha1, &cancel).unwrap(), Kind::Sha1);
    assert_eq!(
        object_format(sb.git(), &sha256, &cancel).unwrap(),
        Kind::Sha256
    );
}

#[test]
fn commit_diffs_including_root_and_rename() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let root = sb.commit_file(&repo, "old name.txt", &"line\n".repeat(20), "root");
    sb.run(&repo, &["mv", "old name.txt", "new name.txt"]);
    std::fs::write(repo.join("new name.txt"), "line\n".repeat(19) + "changed\n").unwrap();
    sb.run(&repo, &["commit", "-qam", "rename and edit"]);
    let head = sb.run(&repo, &["rev-parse", "HEAD"]).trim().to_owned();

    let root_diff = diff(
        &sb,
        &repo,
        DiffSource::Commit {
            parent: None,
            commit: root.parse().unwrap(),
        },
        "old name.txt",
    );
    assert_eq!(root_diff.line_count(), 20);
    assert!(kinds(&root_diff).iter().all(|k| *k == LineKind::Added));

    let renamed = file_diff(
        sb.git(),
        &repo,
        &DiffSource::Commit {
            parent: Some(root.parse().unwrap()),
            commit: head.parse().unwrap(),
        },
        &BString::from("new name.txt"),
        Some(&BString::from("old name.txt")),
        DiffOptions::default(),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(renamed.old_path.as_ref().unwrap(), "old name.txt");
    assert_eq!(renamed.path, "new name.txt");
    let changed: Vec<_> = renamed.hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind != LineKind::Context)
        .map(|l| (l.kind, l.text.to_string()))
        .collect();
    assert_eq!(
        changed,
        [
            (LineKind::Removed, "line".to_owned()),
            (LineKind::Added, "changed".to_owned())
        ]
    );
}

/// `whole_diff` reads every file of the same comparison, so the moved-line detection can
/// follow a block from one file into another. It asks for no context lines.
#[test]
fn the_whole_diff_has_every_file_and_no_context_lines() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    std::fs::write(repo.join("a.txt"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
    std::fs::write(repo.join("b.txt"), "alpha\nbeta\n").unwrap();
    sb.run(&repo, &["add", "."]);
    sb.run(&repo, &["commit", "-qm", "base"]);

    std::fs::write(repo.join("a.txt"), "one\nTWO\nthree\nfour\nfive\n").unwrap();
    std::fs::write(repo.join("b.txt"), "alpha\nBETA\n").unwrap();
    let files = whole_diff(
        sb.git(),
        &repo,
        &DiffSource::Unstaged,
        DiffOptions::default(),
        &CancelToken::new(),
    )
    .unwrap();

    let paths: Vec<String> = files.iter().map(|f| f.path.to_string()).collect();
    assert_eq!(paths, ["a.txt", "b.txt"]);
    // -U0: only the changed lines, although DiffOptions asks for three of context.
    for file in &files {
        assert_eq!(
            kinds(file),
            [LineKind::Removed, LineKind::Added],
            "{}",
            file.path
        );
    }

    // An untracked file belongs to no such diff.
    let none = whole_diff(
        sb.git(),
        &repo,
        &DiffSource::Untracked,
        DiffOptions::default(),
        &CancelToken::new(),
    )
    .unwrap();
    assert!(none.is_empty());
}
