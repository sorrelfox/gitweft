use std::os::unix::fs::PermissionsExt;

use gitweft_git::ops::{CommitOptions, commit, commit_context, head_message, identity};
use gitweft_git::{CancelToken, GitErrorKind};
use gitweft_testkit::Sandbox;

fn log(sb: &Sandbox, repo: &std::path::Path, format: &str) -> String {
    sb.run(repo, &["log", &format!("--format={format}")])
}

#[test]
fn commit_with_message_and_signoff() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    std::fs::write(repo.join("a.txt"), "1\n").unwrap();
    sb.run(&repo, &["add", "a.txt"]);

    let options = CommitOptions {
        signoff: true,
        ..CommitOptions::default()
    };
    commit(
        sb.git(),
        &repo,
        "Add a\n\nLonger explanation.",
        options,
        &CancelToken::new(),
    )
    .unwrap();

    let message = log(&sb, &repo, "%B");
    assert!(
        message.starts_with("Add a\n\nLonger explanation.\n"),
        "{message}"
    );
    assert!(
        message.contains("Signed-off-by: Test Author <author@example.invalid>"),
        "{message}"
    );
}

#[test]
fn amend_replaces_head_and_keeps_its_parent() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let first = sb.commit_file(&repo, "a.txt", "1\n", "first");
    sb.commit_file(&repo, "a.txt", "2\n", "second with typo");
    assert_eq!(
        head_message(sb.git(), &repo, &CancelToken::new()).unwrap(),
        "second with typo"
    );

    std::fs::write(repo.join("b.txt"), "extra\n").unwrap();
    sb.run(&repo, &["add", "b.txt"]);
    let options = CommitOptions {
        amend: true,
        ..CommitOptions::default()
    };
    commit(sb.git(), &repo, "second", options, &CancelToken::new()).unwrap();

    assert_eq!(log(&sb, &repo, "%s"), "second\nfirst\n");
    assert_eq!(sb.run(&repo, &["rev-parse", "HEAD~1"]).trim(), first);
    assert!(
        sb.run(&repo, &["show", "--name-only", "--format=", "HEAD"])
            .contains("b.txt")
    );
}

#[test]
fn failing_pre_commit_hook_blocks_with_its_output_unless_skipped() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let hook = repo.join(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\necho 'lint: trailing whitespace in a.txt' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(repo.join("a.txt"), "1 \n").unwrap();
    sb.run(&repo, &["add", "a.txt"]);

    let err = commit(
        sb.git(),
        &repo,
        "blocked",
        CommitOptions::default(),
        &CancelToken::new(),
    )
    .unwrap_err();
    assert!(
        err.stderr.contains("lint: trailing whitespace in a.txt"),
        "{}",
        err.stderr
    );
    // git says nothing of its own here: the runner finds the hook (#16).
    assert_eq!(err.kind, GitErrorKind::HookFailed, "{}", err.stderr);
    assert_eq!(err.summary(), "lint: trailing whitespace in a.txt");
    assert!(
        sb.run(&repo, &["rev-list", "--all"]).is_empty(),
        "no commit was made"
    );

    let options = CommitOptions {
        no_verify: true,
        ..CommitOptions::default()
    };
    commit(
        sb.git(),
        &repo,
        "skipped hooks",
        options,
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(log(&sb, &repo, "%s"), "skipped hooks\n");
}

#[test]
fn a_failing_commit_msg_hook_is_a_hook_failure_but_git_errors_stay_git_errors() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let hook = repo.join(".git/hooks/commit-msg");
    std::fs::write(
        &hook,
        "#!/bin/sh\necho 'message must start with a ticket' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(repo.join("a.txt"), "1\n").unwrap();
    sb.run(&repo, &["add", "a.txt"]);
    let attempt = || {
        commit(
            sb.git(),
            &repo,
            "no ticket",
            CommitOptions::default(),
            &CancelToken::new(),
        )
        .unwrap_err()
    };

    let err = attempt();
    assert_eq!(err.kind, GitErrorKind::HookFailed, "{}", err.stderr);
    assert_eq!(err.summary(), "message must start with a ticket");

    // With the hook still there, a failure git explains itself keeps its kind.
    sb.run(&repo, &["reset", "-q"]);
    assert_eq!(attempt().kind, GitErrorKind::NothingToCommit);
}

#[test]
fn nothing_staged_is_reported() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "first");
    let err = commit(
        sb.git(),
        &repo,
        "empty",
        CommitOptions::default(),
        &CancelToken::new(),
    )
    .unwrap_err();
    assert_eq!(err.kind, GitErrorKind::NothingToCommit);
}

#[test]
fn commit_context_reads_the_template_relative_to_the_worktree() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let cancel = CancelToken::new();
    // Unborn branch, no template.
    let context = commit_context(sb.git(), &repo, false, &cancel);
    assert_eq!((context.head_message, context.template), (None, None));

    std::fs::write(
        repo.join(".gitmessage"),
        "# Keep it short\nfix: \n\nDetails:\n",
    )
    .unwrap();
    sb.run(&repo, &["config", "commit.template", ".gitmessage"]);
    sb.commit_file(&repo, "a.txt", "1\n", "first");
    let context = commit_context(sb.git(), &repo, true, &cancel);
    assert_eq!(context.head_message.as_deref(), Some("first"));
    assert_eq!(
        context.template,
        Some(("fix:".to_owned(), "Details:".to_owned()))
    );

    // A missing template file is simply no template.
    sb.run(&repo, &["config", "commit.template", "missing.txt"]);
    assert_eq!(
        commit_context(sb.git(), &repo, true, &cancel).template,
        None
    );
}

#[test]
fn identity_comes_from_git_config() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    // Nothing configured: what git would use, here the sandbox's environment.
    let id = identity(sb.git(), &repo, &CancelToken::new());
    assert_eq!(id.name.as_deref(), Some("Test Author"));
    assert!(!id.guessed);
    sb.run(&repo, &["config", "user.name", "Ada Lovelace"]);
    sb.run(&repo, &["config", "user.email", "ada@example.invalid"]);
    let id = identity(sb.git(), &repo, &CancelToken::new());
    assert_eq!(id.name.as_deref(), Some("Ada Lovelace"));
    assert_eq!(id.email.as_deref(), Some("ada@example.invalid"));
}
