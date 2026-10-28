#[cfg(unix)]
use std::ffi::OsStr;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use gitweft_git::gix::bstr::BString;
use gitweft_git::merge::{
    MergeMode, MergeOutcome, Operation, OperationAction, comment_prefix, merge, merge_preview,
    operation_state, prepared_conflicts, prepared_message, run_operation_action,
};
#[cfg(unix)]
use gitweft_git::ops::{CommitOptions, commit};
use gitweft_git::read::open_repo;
use gitweft_git::{CancelToken, GitErrorKind, GitVersion};
use gitweft_testkit::Sandbox;

fn state(repo: &Path) -> Option<Operation> {
    operation_state(&open_repo(repo).unwrap())
}

fn parents(sb: &Sandbox, repo: &Path) -> usize {
    sb.run(repo, &["rev-list", "--parents", "-n1", "HEAD"])
        .split_whitespace()
        .count()
        - 1
}

/// The repository as gix sees it with only its own configuration. The Sandbox isolates
/// the git processes it starts, but gix in this process would also read the real
/// `~/.gitconfig` and `/etc/gitconfig` (a `core.commentChar` there changes the results).
fn open_isolated(repo: &Path) -> gitweft_git::gix::Repository {
    gitweft_git::gix::open_opts(repo, gitweft_git::gix::open::Options::isolated()).unwrap()
}

/// `main` and `feature` both changed since `base`: `feature` touched b.txt, and with
/// `conflict`, both touched a.txt differently.
fn diverged(sb: &Sandbox, conflict: bool) -> PathBuf {
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "base\n", "base");
    sb.run(&repo, &["switch", "-q", "-c", "feature"]);
    sb.commit_file(&repo, "b.txt", "feature\n", "feature work");
    if conflict {
        sb.commit_file(&repo, "a.txt", "feature version\n", "feature edits a");
    }
    sb.run(&repo, &["switch", "-q", "main"]);
    let main_file = if conflict { "a.txt" } else { "c.txt" };
    sb.commit_file(&repo, main_file, "main version\n", "main work");
    repo
}

#[cfg(unix)]
#[test]
fn a_branch_whose_name_isnt_utf8_is_previewed_and_merged() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let cancel = CancelToken::new();
    sb.commit_file(&repo, "a.txt", "a\n", "base");
    sb.git()
        .command(&repo)
        .args(["switch", "-q", "-c"])
        .arg(OsStr::from_bytes(b"caf\xe9"))
        .run(&cancel)
        .unwrap();
    sb.commit_file(&repo, "b.txt", "b\n", "theirs");
    sb.run(&repo, &["switch", "-q", "main"]);
    sb.commit_file(&repo, "c.txt", "c\n", "ours");

    // By its full name's bytes: the lossy text names nothing to git.
    let full = OsStr::from_bytes(b"refs/heads/caf\xe9");
    assert!(
        merge_preview(sb.git(), &repo, "HEAD", full, &cancel)
            .unwrap()
            .is_clean()
    );
    merge(sb.git(), &repo, full, MergeMode::Default, &cancel).unwrap();
    let subject = sb
        .git()
        .command(&repo)
        .args(["log", "-1", "--format=%s"])
        .read_only()
        .run(&cancel)
        .unwrap()
        .stdout;
    // git names the ref as given, and writes the message in UTF-8: the Latin-1 byte
    // comes out as "é" (\xc3\xa9).
    assert_eq!(subject, "Merge branch 'refs/heads/café'\n".as_bytes());
}

#[test]
fn fast_forward_merge_commit_squash_and_up_to_date() {
    let sb = Sandbox::new();
    let cancel = CancelToken::new();

    // Fast-forward: main has nothing of its own.
    let repo = sb.init_repo("ff");
    sb.commit_file(&repo, "a.txt", "1\n", "base");
    sb.run(&repo, &["switch", "-q", "-c", "feature"]);
    sb.commit_file(&repo, "a.txt", "2\n", "ahead");
    sb.run(&repo, &["switch", "-q", "main"]);
    let outcome = merge(sb.git(), &repo, "feature", MergeMode::Default, &cancel).unwrap();
    assert_eq!(outcome, MergeOutcome::FastForwarded);
    assert_eq!(
        merge(sb.git(), &repo, "feature", MergeMode::Default, &cancel).unwrap(),
        MergeOutcome::UpToDate
    );

    // Diverged: a merge commit, or a squash that only stages.
    let sb = Sandbox::new();
    let repo = diverged(&sb, false);
    let err = merge(
        sb.git(),
        &repo,
        "feature",
        MergeMode::FastForwardOnly,
        &cancel,
    )
    .unwrap_err();
    assert_eq!(err.kind, GitErrorKind::DivergentBranches, "{}", err.stderr);
    let outcome = merge(
        sb.git(),
        &repo,
        "feature",
        MergeMode::NoFastForward,
        &cancel,
    )
    .unwrap();
    assert_eq!(outcome, MergeOutcome::Merged);
    assert_eq!(parents(&sb, &repo), 2);

    let sb = Sandbox::new();
    let repo = diverged(&sb, false);
    let outcome = merge(sb.git(), &repo, "feature", MergeMode::Squash, &cancel).unwrap();
    assert_eq!(outcome, MergeOutcome::Squashed);
    assert_eq!(parents(&sb, &repo), 1, "nothing committed yet");
    assert!(
        sb.run(&repo, &["diff", "--cached", "--name-only"])
            .contains("b.txt")
    );
    assert_eq!(state(&repo), None, "a squash leaves no merge in progress");
}

#[test]
fn preview_predicts_conflicts_without_touching_anything() {
    let sb = Sandbox::new();
    let cancel = CancelToken::new();
    let clean = diverged(&sb, false);
    let preview = merge_preview(sb.git(), &clean, "main", "feature", &cancel).unwrap();
    assert!(preview.is_clean(), "{preview:?}");

    let sb = Sandbox::new();
    let repo = diverged(&sb, true);
    let head = sb.run(&repo, &["rev-parse", "HEAD"]);
    let preview = merge_preview(sb.git(), &repo, "HEAD", "feature", &cancel).unwrap();
    assert_eq!(preview.conflicts, ["a.txt"]);
    assert_eq!(sb.run(&repo, &["rev-parse", "HEAD"]), head);
    assert!(sb.run(&repo, &["status", "--porcelain"]).is_empty());
    assert_eq!(state(&repo), None);
}

#[test]
fn a_conflicting_merge_is_continued_after_resolving_or_aborted() {
    let sb = Sandbox::new();
    let repo = diverged(&sb, true);
    let cancel = CancelToken::new();

    let err = merge(sb.git(), &repo, "feature", MergeMode::Default, &cancel).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::Conflict, "{}", err.stderr);
    assert_eq!(state(&repo), Some(Operation::Merge));

    let err = run_operation_action(
        sb.git(),
        &repo,
        Operation::Merge,
        OperationAction::Continue,
        &cancel,
    )
    .unwrap_err();
    assert_eq!(
        err.kind,
        GitErrorKind::UnresolvedConflicts,
        "{}",
        err.stderr
    );
    assert_eq!(state(&repo), Some(Operation::Merge), "still in progress");

    std::fs::write(repo.join("a.txt"), "both versions\n").unwrap();
    sb.run(&repo, &["add", "a.txt"]);
    run_operation_action(
        sb.git(),
        &repo,
        Operation::Merge,
        OperationAction::Continue,
        &cancel,
    )
    .unwrap();
    assert_eq!(state(&repo), None);
    assert_eq!(parents(&sb, &repo), 2);

    // The same merge again, aborted this time.
    let sb = Sandbox::new();
    let repo = diverged(&sb, true);
    let head = sb.run(&repo, &["rev-parse", "HEAD"]);
    merge(sb.git(), &repo, "feature", MergeMode::Default, &cancel).unwrap_err();
    run_operation_action(
        sb.git(),
        &repo,
        Operation::Merge,
        OperationAction::Abort,
        &cancel,
    )
    .unwrap();
    assert_eq!(state(&repo), None);
    assert_eq!(sb.run(&repo, &["rev-parse", "HEAD"]), head);
    assert!(sb.run(&repo, &["status", "--porcelain"]).is_empty());
    let err = run_operation_action(
        sb.git(),
        &repo,
        Operation::Merge,
        OperationAction::Skip,
        &cancel,
    )
    .unwrap_err();
    assert!(err.stderr.contains("not possible"), "{}", err.stderr);
}

#[test]
fn rebases_and_cherry_picks_started_in_a_terminal_are_detected() {
    let sb = Sandbox::new();
    let repo = diverged(&sb, true);
    let cancel = CancelToken::new();
    let head = sb.run(&repo, &["rev-parse", "HEAD"]);

    // Like a terminal would: a rebase stopping at a conflict.
    let _ = sb
        .git()
        .command(&repo)
        .args(["rebase", "feature"])
        .run(&cancel);
    assert_eq!(state(&repo), Some(Operation::Rebase));
    run_operation_action(
        sb.git(),
        &repo,
        Operation::Rebase,
        OperationAction::Abort,
        &cancel,
    )
    .unwrap();
    assert_eq!(state(&repo), None);
    assert_eq!(sb.run(&repo, &["rev-parse", "HEAD"]), head);

    let feature_edit = sb.run(&repo, &["rev-parse", "feature"]);
    let _ = sb
        .git()
        .command(&repo)
        .args(["cherry-pick", feature_edit.trim()])
        .run(&cancel);
    assert_eq!(state(&repo), Some(Operation::CherryPick));
    // Skipping the only pick ends the operation.
    run_operation_action(
        sb.git(),
        &repo,
        Operation::CherryPick,
        OperationAction::Skip,
        &cancel,
    )
    .unwrap();
    assert_eq!(state(&repo), None);
    assert_eq!(sb.run(&repo, &["rev-parse", "HEAD"]), head);
}

#[cfg(unix)]
#[test]
fn a_conflicting_merge_prepares_its_message_and_lists_its_conflicts() {
    let sb = Sandbox::new();
    let repo = diverged(&sb, true);
    // Two more conflicts: both sides add these, one with a name that isn't UTF-8.
    let latin1 = OsStr::from_bytes(b"caf\xe9.txt");
    for (branch, content) in [("feature", "feature\n"), ("main", "main\n")] {
        sb.run(&repo, &["switch", "-q", branch]);
        std::fs::write(repo.join("Z.txt"), content).unwrap();
        std::fs::write(repo.join(latin1), content).unwrap();
        sb.run(&repo, &["add", "-A"]);
        sb.run(&repo, &["commit", "-q", "-m", "add files"]);
    }
    let cancel = CancelToken::new();
    merge(sb.git(), &repo, "feature", MergeMode::Default, &cancel).unwrap_err();

    // Git lists the conflicted files as comments (which `git commit --file` would keep),
    // in index order and unquoted.
    let raw = std::fs::read(repo.join(".git/MERGE_MSG")).unwrap();
    assert!(
        raw.ends_with(b"\n\n# Conflicts:\n#\tZ.txt\n#\ta.txt\n#\tcaf\xe9.txt\n"),
        "{}",
        String::from_utf8_lossy(&raw)
    );
    assert_eq!(
        prepared_message(&open_isolated(&repo), sb.git().version()).as_deref(),
        Some("Merge branch 'feature'")
    );
    let conflicted = [
        BString::from("Z.txt"),
        BString::from("a.txt"),
        BString::from(&b"caf\xe9.txt"[..]),
    ];
    assert_eq!(
        prepared_conflicts(&open_isolated(&repo), sb.git().version()),
        conflicted
    );

    // Resolving a file doesn't take it off git's list.
    std::fs::write(repo.join("a.txt"), "both versions\n").unwrap();
    sb.run(&repo, &["add", "a.txt"]);
    assert_eq!(
        prepared_conflicts(&open_isolated(&repo), sb.git().version()),
        conflicted
    );

    std::fs::write(repo.join("Z.txt"), "both\n").unwrap();
    std::fs::write(repo.join(latin1), "both\n").unwrap();
    sb.run(&repo, &["add", "-A"]);
    let message = "Merge feature, keeping both\n\nBody line";
    commit(sb.git(), &repo, message, CommitOptions::default(), &cancel).unwrap();
    assert_eq!(parents(&sb, &repo), 2);
    assert_eq!(sb.run(&repo, &["log", "-1", "--format=%B"]).trim(), message);
    assert_eq!(state(&repo), None);
    let after = open_isolated(&repo);
    assert_eq!(
        prepared_message(&after, sb.git().version()),
        None,
        "git removed MERGE_MSG"
    );
    assert!(prepared_conflicts(&after, sb.git().version()).is_empty());
}

#[test]
fn the_comment_prefix_is_the_one_git_wrote_with() {
    let cancel = CancelToken::new();
    // Settings made in the repository, in this order, and the prefix git then uses: git
    // 2.45 and later, and older git, where `None` is a setting it refuses to run with.
    type Case<'a> = (&'a [(&'a str, &'a str)], &'a str, Option<&'a str>);
    let cases: [Case; 9] = [
        (&[], "#", Some("#")),
        (&[("core.commentChar", ";")], ";", Some(";")),
        // Older git doesn't know `commentString`.
        (&[("core.commentString", "//")], "//", Some("#")),
        // Git ≥ 2.45 takes a string in either key; older git only one character.
        (&[("core.commentChar", "//")], "//", None),
        // Quoted in the file: the space is part of the prefix.
        (&[("core.commentString", "// ")], "// ", Some("#")),
        // Deprecated: only `git commit` picks a free character; merges write `#`.
        (&[("core.commentChar", "AUTO")], "#", Some("#")),
        // Another section.
        (&[("core.x.commentChar", ";")], "#", Some("#")),
        // The two keys are one setting: the last one set wins.
        (
            &[("core.commentString", "//"), ("core.commentChar", ";")],
            ";",
            Some(";"),
        ),
        (
            &[("core.commentChar", ";"), ("core.commentString", "//")],
            "//",
            Some(";"),
        ),
    ];
    for (settings, newer, older) in cases {
        let sb = Sandbox::new();
        let version = sb.git().version();
        let Some(prefix) = (if version >= GitVersion::new(2, 45, 0) {
            Some(newer)
        } else {
            older
        }) else {
            continue;
        };
        let repo = diverged(&sb, true);
        for &(key, value) in settings {
            sb.run(&repo, &["config", key, value]);
        }
        merge(sb.git(), &repo, "feature", MergeMode::Default, &cancel).unwrap_err();
        let raw = std::fs::read_to_string(repo.join(".git/MERGE_MSG")).unwrap();
        assert!(
            raw.contains(&format!("\n{prefix}\ta.txt\n")),
            "{settings:?}: {raw}"
        );
        let gix_repo = open_isolated(&repo);
        assert_eq!(comment_prefix(&gix_repo, version), prefix, "{settings:?}");
        assert_eq!(
            prepared_message(&gix_repo, version).as_deref(),
            Some("Merge branch 'feature'"),
            "{settings:?}"
        );
        assert_eq!(
            prepared_conflicts(&gix_repo, version),
            ["a.txt"],
            "{settings:?}"
        );
    }

    // Git refuses to run with an empty value ("must have at least one character").
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.run(&repo, &["config", "core.commentString", "//"]);
    sb.run(&repo, &["config", "core.commentChar", ""]);
    assert_eq!(
        comment_prefix(&open_isolated(&repo), sb.git().version()),
        "#"
    );
}

#[test]
fn a_linked_worktree_prepares_its_own_message() {
    let sb = Sandbox::new();
    let repo = diverged(&sb, true);
    let cancel = CancelToken::new();
    sb.run(
        &repo,
        &["worktree", "add", "-q", "-b", "side", "../linked", "main"],
    );
    let linked = sb.path().join("linked");
    merge(sb.git(), &linked, "feature", MergeMode::Default, &cancel).unwrap_err();
    let linked_repo = open_isolated(&linked);
    assert!(linked_repo.git_dir().join("MERGE_MSG").is_file());
    assert_eq!(
        prepared_message(&linked_repo, sb.git().version()).as_deref(),
        Some("Merge branch 'feature' into side")
    );
    assert_eq!(
        prepared_message(&open_isolated(&repo), sb.git().version()),
        None,
        "not the main worktree's"
    );
}

#[test]
fn picks_reverts_and_rebases_prepare_the_commits_message_squashes_only_conflicts() {
    let cancel = CancelToken::new();

    let sb = Sandbox::new();
    let repo = diverged(&sb, true);
    let _ = sb
        .git()
        .command(&repo)
        .args(["cherry-pick", "feature"])
        .run(&cancel);
    assert_eq!(state(&repo), Some(Operation::CherryPick));
    assert_eq!(
        prepared_message(&open_isolated(&repo), sb.git().version()).as_deref(),
        Some("feature edits a")
    );
    assert_eq!(
        prepared_conflicts(&open_isolated(&repo), sb.git().version()),
        ["a.txt"]
    );

    let sb = Sandbox::new();
    let repo = diverged(&sb, true);
    let reverted = sb.run(&repo, &["rev-parse", "feature"]);
    let _ = sb
        .git()
        .command(&repo)
        .args(["revert", "--no-edit", "feature"])
        .run(&cancel);
    assert_eq!(state(&repo), Some(Operation::Revert));
    assert_eq!(
        prepared_message(&open_isolated(&repo), sb.git().version()),
        Some(format!(
            "Revert \"feature edits a\"\n\nThis reverts commit {}.",
            reverted.trim()
        ))
    );
    assert_eq!(
        prepared_conflicts(&open_isolated(&repo), sb.git().version()),
        ["a.txt"]
    );

    let sb = Sandbox::new();
    let repo = diverged(&sb, true);
    let _ = sb
        .git()
        .command(&repo)
        .args(["rebase", "feature"])
        .run(&cancel);
    assert_eq!(state(&repo), Some(Operation::Rebase));
    assert_eq!(
        prepared_message(&open_isolated(&repo), sb.git().version()).as_deref(),
        Some("main work")
    );
    // The merge backend writes the same block.
    assert_eq!(
        prepared_conflicts(&open_isolated(&repo), sb.git().version()),
        ["a.txt"]
    );

    // A squash's message is in SQUASH_MSG; its MERGE_MSG holds only the conflict comments.
    let sb = Sandbox::new();
    let repo = diverged(&sb, true);
    let err = merge(sb.git(), &repo, "feature", MergeMode::Squash, &cancel).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::Conflict, "{}", err.stderr);
    assert!(repo.join(".git/MERGE_MSG").is_file());
    assert_eq!(state(&repo), None);
    assert_eq!(
        prepared_message(&open_isolated(&repo), sb.git().version()),
        None
    );
    assert_eq!(
        prepared_conflicts(&open_isolated(&repo), sb.git().version()),
        ["a.txt"]
    );
}
