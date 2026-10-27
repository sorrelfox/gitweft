use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gitweft_git::bisect::{
    BisectMark, BisectProgress, BisectRunOutcome, BisectState, bisect_mark, bisect_reset,
    bisect_run, bisect_start, bisect_state,
};
use gitweft_git::{CancelToken, GitError, GitErrorKind, ObjectId};
use gitweft_testkit::Sandbox;

fn id(hex: &str) -> ObjectId {
    ObjectId::from_hex(hex.trim().as_bytes()).unwrap()
}

/// A repository `r` with a linear history of 16 commits `c1`…`c16` that each change
/// `app.txt`; from `c11` on, it contains "bug". Returns the repo and the ids, oldest first.
fn setup(sb: &Sandbox) -> (PathBuf, Vec<ObjectId>) {
    let repo = sb.init_repo("r");
    let ids = (1..=16)
        .map(|i| {
            let bug = if i >= 11 { "bug\n" } else { "" };
            let content = format!("version {i}\n{bug}");
            id(&sb.commit_file(&repo, "app.txt", &content, &format!("c{i}")))
        })
        .collect();
    (repo, ids)
}

fn has_bug(repo: &Path) -> bool {
    std::fs::read_to_string(repo.join("app.txt"))
        .unwrap()
        .contains("bug")
}

fn head(sb: &Sandbox, repo: &Path) -> ObjectId {
    id(&sb.run(repo, &["rev-parse", "HEAD"]))
}

/// The checked-out branch, or `HEAD` when detached.
fn branch(sb: &Sandbox, repo: &Path) -> String {
    sb.run(repo, &["rev-parse", "--abbrev-ref", "HEAD"])
        .trim()
        .to_owned()
}

fn state(sb: &Sandbox, repo: &Path) -> Option<BisectState> {
    bisect_state(sb.git(), repo, &CancelToken::new()).unwrap()
}

fn start(
    sb: &Sandbox,
    repo: &Path,
    bad: Option<&str>,
    good: &[String],
    paths: &[PathBuf],
) -> BisectState {
    let started = bisect_start(sb.git(), repo, bad, good, paths, &CancelToken::new())
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(state(sb, repo).as_ref(), Some(&started));
    started
}

/// Marks through the API; the returned state is what `bisect_state` reads afterwards.
fn mark(sb: &Sandbox, repo: &Path, mark: BisectMark, commit: Option<&str>) -> BisectState {
    let marked = bisect_mark(sb.git(), repo, mark, commit, &CancelToken::new())
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(state(sb, repo).as_ref(), Some(&marked));
    marked
}

fn reset(sb: &Sandbox, repo: &Path) {
    bisect_reset(sb.git(), repo, &CancelToken::new()).unwrap();
}

fn testing(commit: ObjectId, revisions_left: usize, steps_left: usize) -> BisectProgress {
    BisectProgress::Testing {
        commit,
        suggested: commit,
        revisions_left,
        steps_left,
    }
}

/// Git's bisect messages without quotes: newer git quotes the terms (`is the first
/// 'bad' commit`), older git doesn't (#79).
fn unquoted(text: &str) -> String {
    text.replace('\'', "")
}

/// Checks `bisect_state` against what `git bisect` printed on stdout for a command run in
/// a terminal: `Bisecting: N revisions left to test after this (roughly M steps)` and
/// `[<id>] <subject>`, or `<id> is the first '<term>' commit`.
fn assert_agrees_with_git(sb: &Sandbox, repo: &Path, printed: &str) -> BisectState {
    let current = state(sb, repo).expect("a bisect is running");
    if let Some(line) = printed
        .lines()
        .find(|line| unquoted(line).contains(" is the first "))
    {
        let first_bad = id(line.split(' ').next().unwrap());
        assert_eq!(current.progress, BisectProgress::Found { first_bad });
        return current;
    }
    let number = |text: &str| -> usize { text.split(' ').next().unwrap().parse().unwrap() };
    let counts = printed
        .lines()
        .find_map(|line| line.strip_prefix("Bisecting: "))
        .unwrap_or_else(|| panic!("unexpected output: {printed}"));
    let left = number(counts);
    let steps = number(&counts[counts.find("(roughly ").unwrap() + "(roughly ".len()..]);
    let chosen = printed
        .lines()
        .find_map(|line| line.strip_prefix('['))
        .map(|rest| id(&rest[..rest.find(']').unwrap()]))
        .unwrap();
    assert_eq!(current.progress, testing(chosen, left, steps), "{printed}");
    current
}

#[test]
fn start_with_bad_and_good_tests_the_middle() {
    let sb = Sandbox::new();
    let (repo, ids) = setup(&sb);
    assert_eq!(state(&sb, &repo), None);

    let started = start(&sb, &repo, Some("HEAD"), &[ids[0].to_string()], &[]);
    assert_eq!(
        started,
        BisectState {
            bad: Some(ids[15]),
            good: vec![ids[0]],
            skipped: vec![],
            paths: vec![],
            // As git prints it: 7 revisions left after this (roughly 3 steps), at c8.
            progress: testing(ids[7], 7, 3),
        }
    );
    assert_eq!(head(&sb, &repo), ids[7]);
    assert_eq!(branch(&sb, &repo), "HEAD");
}

#[test]
fn marking_by_the_test_result_finds_the_first_bad_commit() {
    let sb = Sandbox::new();
    let (repo, ids) = setup(&sb);
    let mut current = start(&sb, &repo, Some("HEAD"), &[ids[0].to_string()], &[]);

    let mut counts = Vec::new();
    let first_bad = loop {
        match current.progress {
            BisectProgress::Testing {
                commit,
                suggested,
                revisions_left,
                steps_left,
            } => {
                assert_eq!(commit, suggested);
                assert_eq!(head(&sb, &repo), commit);
                counts.push((revisions_left, steps_left));
                assert!(counts.len() <= 5, "too many steps: {counts:?}");
                let result = if has_bug(&repo) {
                    BisectMark::Bad
                } else {
                    BisectMark::Good
                };
                current = mark(&sb, &repo, result, None);
            }
            BisectProgress::Found { first_bad } => break first_bad,
            other => panic!("unexpected progress: {other:?}"),
        }
    };
    assert_eq!(first_bad, ids[10]);
    // c8 good, c12 bad, c10 good, c11 bad.
    assert_eq!(counts, [(7, 3), (3, 2), (1, 1), (0, 0)]);
    // The result is `refs/bisect/bad`; it stays until the reset.
    assert_eq!(current.bad, Some(ids[10]));
    assert_eq!(current.good, {
        let mut good = vec![ids[0], ids[7], ids[9]];
        good.sort();
        good
    });
    assert_eq!(state(&sb, &repo), Some(current));
}

#[test]
fn adjacent_marks_find_the_bad_commit_at_once() {
    let sb = Sandbox::new();
    let (repo, ids) = setup(&sb);
    let started = start(&sb, &repo, Some("HEAD"), &["HEAD~1".to_owned()], &[]);
    assert_eq!(
        started.progress,
        BisectProgress::Found { first_bad: ids[15] }
    );
    // Nothing to check out.
    assert_eq!(branch(&sb, &repo), "main");
}

#[test]
fn marks_can_come_later() {
    let sb = Sandbox::new();
    let (repo, ids) = setup(&sb);

    let started = start(&sb, &repo, None, &[], &[]);
    assert_eq!(
        started,
        BisectState {
            bad: None,
            good: vec![],
            skipped: vec![],
            paths: vec![],
            progress: BisectProgress::NeedsMarks {
                needs_bad: true,
                needs_good: true
            },
        }
    );
    assert_eq!(branch(&sb, &repo), "main");

    let marked = mark(&sb, &repo, BisectMark::Bad, None);
    assert_eq!(marked.bad, Some(ids[15]));
    assert_eq!(
        marked.progress,
        BisectProgress::NeedsMarks {
            needs_bad: false,
            needs_good: true
        }
    );

    let marked = mark(&sb, &repo, BisectMark::Good, Some(&ids[0].to_string()));
    assert_eq!(marked.good, [ids[0]]);
    assert_eq!(marked.progress, testing(ids[7], 7, 3));
    assert_eq!(head(&sb, &repo), ids[7]);
    reset(&sb, &repo);

    // Good commits without a bad one.
    let started = start(
        &sb,
        &repo,
        None,
        &[ids[0].to_string(), ids[1].to_string()],
        &[],
    );
    let mut good = vec![ids[0], ids[1]];
    good.sort();
    assert_eq!(started.good, good);
    assert_eq!(
        started.progress,
        BisectProgress::NeedsMarks {
            needs_bad: true,
            needs_good: false
        }
    );
    let marked = mark(&sb, &repo, BisectMark::Bad, Some("HEAD"));
    assert!(
        matches!(marked.progress, BisectProgress::Testing { commit, .. } if commit == head(&sb, &repo)),
        "{marked:?}"
    );
}

#[test]
fn skipping_everything_leaves_only_skipped_commits() {
    let sb = Sandbox::new();
    let (repo, ids) = setup(&sb);
    let mut current = start(&sb, &repo, Some("HEAD"), &[ids[0].to_string()], &[]);

    let mut skipped = Vec::new();
    let candidates = loop {
        match current.progress {
            BisectProgress::Testing { commit, .. } => {
                assert!(!skipped.contains(&commit), "{commit} again");
                assert_eq!(head(&sb, &repo), commit);
                skipped.push(commit);
                current = mark(&sb, &repo, BisectMark::Skip, None);
            }
            BisectProgress::OnlySkipped { candidates } => break candidates,
            other => panic!("unexpected progress: {other:?}"),
        }
    };
    // c2…c15 skipped: the first bad commit is any of c16…c2 (newest first).
    assert_eq!(skipped.len(), 14);
    let newest_first = |from: usize| ids[from..].iter().rev().copied().collect::<Vec<_>>();
    assert_eq!(candidates, newest_first(1));
    skipped.sort();
    assert_eq!(current.skipped, skipped);

    // Skipping again (git exits with 2) is still an answer.
    let again = mark(&sb, &repo, BisectMark::Skip, None);
    assert_eq!(again.progress, BisectProgress::OnlySkipped { candidates });

    // A good c10 narrows the candidates to c16…c11.
    let marked = mark(&sb, &repo, BisectMark::Good, Some(&ids[9].to_string()));
    assert_eq!(
        marked.progress,
        BisectProgress::OnlySkipped {
            candidates: newest_first(10)
        }
    );
}

#[test]
fn reset_returns_to_where_the_bisect_started() {
    let sb = Sandbox::new();
    let (repo, ids) = setup(&sb);
    start(&sb, &repo, Some("HEAD"), &[ids[0].to_string()], &[]);
    mark(&sb, &repo, BisectMark::Good, None);
    assert_ne!(head(&sb, &repo), ids[15]);

    reset(&sb, &repo);
    assert_eq!(branch(&sb, &repo), "main");
    assert_eq!(head(&sb, &repo), ids[15]);
    assert_eq!(state(&sb, &repo), None);
    // Nothing to reset is fine.
    reset(&sb, &repo);

    // From a detached HEAD, back to that commit.
    sb.run(&repo, &["checkout", "-q", "--detach", &ids[12].to_string()]);
    start(&sb, &repo, Some("main"), &[ids[0].to_string()], &[]);
    assert_eq!(head(&sb, &repo), ids[7]);
    reset(&sb, &repo);
    assert_eq!(branch(&sb, &repo), "HEAD");
    assert_eq!(head(&sb, &repo), ids[12]);
    assert_eq!(state(&sb, &repo), None);
}

#[test]
fn revisions_starting_with_a_dash_are_refused() {
    let sb = Sandbox::new();
    let (repo, _) = setup(&sb);
    let cancel = CancelToken::new();
    for (bad, good) in [
        (Some("-x"), vec![]),
        (Some("HEAD"), vec!["--foo".to_owned()]),
        (None, vec!["-h".to_owned()]),
    ] {
        let err = bisect_start(sb.git(), &repo, bad, &good, &[], &cancel).unwrap_err();
        assert!(err.summary().contains("is not a valid revision"), "{err:?}");
        // Refused before running git at all.
        assert_eq!(err.exit_code, None);
        assert_eq!(state(&sb, &repo), None);
    }
    // An unknown revision is git's to refuse; nothing is started.
    let err = bisect_start(sb.git(), &repo, Some("nope"), &[], &[], &cancel).unwrap_err();
    assert_eq!(
        err.summary(),
        "fatal: 'nope' does not appear to be a valid revision"
    );
    assert_eq!(state(&sb, &repo), None);

    // Marking without a bisect leaves nothing behind.
    let err = bisect_mark(sb.git(), &repo, BisectMark::Good, None, &cancel).unwrap_err();
    assert_eq!(err.summary(), "fatal: no bisect is running");
    assert!(!repo.join(".git/BISECT_TERMS").exists());

    let started = start(&sb, &repo, None, &[], &[]);
    for mark in [BisectMark::Good, BisectMark::Bad, BisectMark::Skip] {
        let err = bisect_mark(sb.git(), &repo, mark, Some("--foo"), &cancel).unwrap_err();
        assert!(err.summary().contains("is not a valid revision"), "{err:?}");
        assert_eq!(err.exit_code, None);
    }
    assert_eq!(state(&sb, &repo), Some(started));
}

#[test]
fn local_changes_in_the_way_of_a_checkout() {
    let sb = Sandbox::new();
    let (repo, ids) = setup(&sb);
    let cancel = CancelToken::new();
    let good = [ids[0].to_string()];

    // `app.txt` differs in every commit, so a local change blocks every checkout. Git then
    // doesn't start at all, and the change is kept.
    std::fs::write(repo.join("app.txt"), "local\n").unwrap();
    let err = bisect_start(sb.git(), &repo, Some("HEAD"), &good, &[], &cancel).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::WouldOverwrite, "{err:?}");
    assert_eq!(state(&sb, &repo), None);
    assert_eq!(branch(&sb, &repo), "main");
    assert_eq!(
        std::fs::read_to_string(repo.join("app.txt")).unwrap(),
        "local\n"
    );
    sb.run(&repo, &["checkout", "--", "app.txt"]);

    // An untracked file no commit has doesn't get in the way.
    std::fs::write(repo.join("notes.txt"), "mine\n").unwrap();
    start(&sb, &repo, Some("HEAD"), &good, &[]);
    assert_eq!(head(&sb, &repo), ids[7]);

    // Marking works, but checking out the next commit doesn't: the mark stays, HEAD stays,
    // and git's choice (c12) shows as the suggestion.
    std::fs::write(repo.join("app.txt"), "local\n").unwrap();
    let err = bisect_mark(sb.git(), &repo, BisectMark::Good, None, &cancel).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::WouldOverwrite, "{err:?}");
    let current = state(&sb, &repo).unwrap();
    assert!(current.good.contains(&ids[7]));
    assert_eq!(
        current.progress,
        BisectProgress::Testing {
            commit: ids[7],
            suggested: ids[11],
            revisions_left: 3,
            steps_left: 2,
        }
    );
    assert_eq!(head(&sb, &repo), ids[7]);

    // Ending the bisect is blocked too, and it keeps running.
    let err = bisect_reset(sb.git(), &repo, &cancel).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::WouldOverwrite, "{err:?}");
    assert_eq!(state(&sb, &repo), Some(current));

    sb.run(&repo, &["checkout", "--", "app.txt"]);
    reset(&sb, &repo);
    assert_eq!(state(&sb, &repo), None);
    assert_eq!(branch(&sb, &repo), "main");
    assert!(repo.join("notes.txt").exists());
}

#[test]
fn a_bisect_run_in_a_terminal_with_its_own_terms() {
    let sb = Sandbox::new();
    let (repo, ids) = setup(&sb);
    let root = ids[0].to_string();

    let printed = sb.run(
        &repo,
        &[
            "bisect",
            "start",
            "--term-new=broken",
            "--term-old=fine",
            "HEAD",
            &root,
        ],
    );
    let current = assert_agrees_with_git(&sb, &repo, &printed);
    assert_eq!((current.bad, current.good), (Some(ids[15]), vec![ids[0]]));

    // Our marks use the bisect's terms.
    let marked = mark(&sb, &repo, BisectMark::Good, None);
    assert_eq!(marked.good.len(), 2);
    let printed = sb.run(&repo, &["bisect", "broken"]);
    let current = assert_agrees_with_git(&sb, &repo, &printed);
    assert_ne!(current.bad, Some(ids[15]));
}

#[test]
fn a_bisect_run_in_a_terminal_without_checkout() {
    let sb = Sandbox::new();
    let (repo, ids) = setup(&sb);
    let root = ids[0].to_string();

    let printed = sb.run(&repo, &["bisect", "start", "--no-checkout", "HEAD", &root]);
    let current = assert_agrees_with_git(&sb, &repo, &printed);
    // Started without paths: the whole history.
    assert!(current.paths.is_empty());
    assert_eq!(head(&sb, &repo), ids[15]);

    // A mark without a revision is for BISECT_HEAD.
    let marked = mark(&sb, &repo, BisectMark::Good, None);
    assert!(marked.good.contains(&ids[7]));
    assert_eq!(marked.progress, testing(ids[11], 3, 2));
    assert_eq!(head(&sb, &repo), ids[15]);
}

#[test]
fn a_bisect_limited_to_a_path_counts_like_git() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    // Odd commits change the path we bisect, even ones another file.
    let path = "it's here!.txt";
    let ids: Vec<ObjectId> = (1..=16)
        .map(|i| {
            let file = if i % 2 == 1 { path } else { "other.txt" };
            id(&sb.commit_file(&repo, file, &format!("{i}\n"), &format!("c{i}")))
        })
        .collect();

    let root = ids[0].to_string();
    let mut printed = sb.run(&repo, &["bisect", "start", "HEAD", &root, "--", path]);
    for _ in 0..8 {
        let current = assert_agrees_with_git(&sb, &repo, &printed);
        let BisectProgress::Testing { commit, .. } = current.progress else {
            break;
        };
        // Bad from c9 on.
        let verb = if ids.iter().position(|id| *id == commit).unwrap() >= 8 {
            "bad"
        } else {
            "good"
        };
        printed = sb.run(&repo, &["bisect", verb]);
    }
    assert_eq!(
        state(&sb, &repo).unwrap().progress,
        BisectProgress::Found { first_bad: ids[8] }
    );
}

#[test]
fn a_good_commit_after_the_bad_one_is_contradictory() {
    let sb = Sandbox::new();
    let (repo, ids) = setup(&sb);
    let cancel = CancelToken::new();
    start(&sb, &repo, None, &[], &[]);
    mark(&sb, &repo, BisectMark::Bad, Some(&ids[3].to_string()));

    // Git keeps the mark but complains that it can't bisect this way.
    let err = bisect_mark(sb.git(), &repo, BisectMark::Good, Some("HEAD"), &cancel).unwrap_err();
    assert!(err.stderr.contains("Maybe you mistook"), "{err:?}");
    let current = state(&sb, &repo).unwrap();
    assert_eq!(current.good, [ids[15]]);
    assert_eq!(current.progress, BisectProgress::Contradictory);

    // Marking the good commit bad too: git says so on stdout, and keeps that mark as well.
    let err = bisect_mark(sb.git(), &repo, BisectMark::Bad, None, &cancel).unwrap_err();
    assert_eq!(
        unquoted(&err.summary()),
        format!("{} was both good and bad", ids[15])
    );
    let current = state(&sb, &repo).unwrap();
    assert_eq!((current.bad, current.good), (Some(ids[15]), vec![ids[15]]));
    assert_eq!(current.progress, BisectProgress::Contradictory);

    reset(&sb, &repo);
    assert_eq!(state(&sb, &repo), None);
}

/// A repository `v` with 16 commits `c1`…`c16` of `value.txt`; from `c11` on it says
/// "bug". Returns the repo and the ids, oldest first.
fn value_history(sb: &Sandbox) -> (PathBuf, Vec<ObjectId>) {
    let repo = sb.init_repo("v");
    let ids = (1..=16)
        .map(|i| {
            let bug = if i >= 11 { "bug\n" } else { "" };
            let content = format!("v{i}\n{bug}");
            id(&sb.commit_file(&repo, "value.txt", &content, &format!("c{i}")))
        })
        .collect();
    (repo, ids)
}

/// Runs `command` under `git bisect run`; returns the result and every line printed.
fn run(
    sb: &Sandbox,
    repo: &Path,
    command: &str,
) -> (Result<BisectRunOutcome, GitError>, Vec<String>) {
    let mut lines = Vec::new();
    let result = bisect_run(sb.git(), repo, command, &CancelToken::new(), |line| {
        lines.push(line.to_owned())
    });
    (result, lines)
}

#[test]
fn bisect_run_finds_the_first_bad_commit() {
    let sb = Sandbox::new();
    let (repo, ids) = value_history(&sb);
    start(&sb, &repo, Some("HEAD"), &[ids[0].to_string()], &[]);

    // Output on stdout and stderr; `Tests: 5` would parse as a progress record.
    let (result, lines) = run(
        &sb,
        &repo,
        "echo checking; echo 'Tests: 5' >&2; ! grep -q bug value.txt",
    );
    let outcome = result.unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(outcome.first_bad, Some(ids[10]));
    assert_eq!(
        outcome.state.progress,
        BisectProgress::Found { first_bad: ids[10] }
    );
    assert_eq!(state(&sb, &repo), Some(outcome.state));

    // c8, c12, c10, c11.
    let count = |text: &str| lines.iter().filter(|line| *line == text).count();
    assert_eq!(count("checking"), 4, "{lines:#?}");
    assert_eq!(count("Tests: 5"), 4, "{lines:#?}");
    assert_eq!(
        lines.iter().filter(|l| l.starts_with("running ")).count(),
        4
    );
    let found = format!("{} is the first bad commit", ids[10]);
    assert_eq!(lines.iter().filter(|l| unquoted(l) == found).count(), 1);

    // Still bisecting until the reset.
    reset(&sb, &repo);
    assert_eq!(branch(&sb, &repo), "main");
}

#[test]
fn bisect_run_with_the_bisect_s_own_terms() {
    let sb = Sandbox::new();
    let (repo, ids) = value_history(&sb);
    let root = ids[0].to_string();
    sb.run(
        &repo,
        &[
            "bisect",
            "start",
            "--term-new=broken",
            "--term-old=fine",
            "HEAD",
            &root,
        ],
    );
    let (result, lines) = run(&sb, &repo, "! grep -q bug value.txt");
    assert_eq!(result.unwrap().first_bad, Some(ids[10]));
    let found = format!("{} is the first broken commit", ids[10]);
    assert!(lines.iter().any(|l| unquoted(l) == found), "{lines:#?}");
}

#[test]
fn bisect_run_that_skips_everything_finds_nothing() {
    let sb = Sandbox::new();
    let (repo, ids) = value_history(&sb);
    start(&sb, &repo, Some("HEAD"), &[ids[0].to_string()], &[]);

    let (result, lines) = run(&sb, &repo, "exit 125");
    let outcome = result.unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(outcome.first_bad, None);
    // c16…c2, newest first.
    let candidates: Vec<ObjectId> = ids[1..].iter().rev().copied().collect();
    assert_eq!(
        outcome.state.progress,
        BisectProgress::OnlySkipped { candidates }
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("only 'skip'ped commits left")),
        "{lines:#?}"
    );
}

#[test]
fn bisect_run_failures() {
    let sb = Sandbox::new();
    let (repo, ids) = value_history(&sb);
    let good = [ids[0].to_string()];

    // Refused before git runs.
    let (result, lines) = run(&sb, &repo, "true");
    assert_eq!(result.unwrap_err().summary(), "fatal: no bisect is running");
    assert!(lines.is_empty());
    start(&sb, &repo, None, &[], &[]);
    let (result, _) = run(&sb, &repo, "true");
    assert!(
        result
            .unwrap_err()
            .summary()
            .contains("needs a bad and a good"),
        "needs marks"
    );
    start(&sb, &repo, Some("HEAD"), &good, &[]);
    let (result, _) = run(&sb, &repo, " ");
    assert_eq!(result.unwrap_err().summary(), "fatal: no command to run");

    // A crash (128 and up) stops the run; the bisect goes on.
    let (result, _) = run(&sb, &repo, "exit 200");
    let err = result.unwrap_err();
    assert!(
        err.summary().contains("bisect run failed: exit code 200"),
        "{err:?}"
    );
    assert!(state(&sb, &repo).is_some());
    reset(&sb, &repo);

    // A command that can't be found (127) fails on a good commit too.
    start(&sb, &repo, Some("HEAD"), &good, &[]);
    let (result, lines) = run(&sb, &repo, "no-such-command-in-gitweft-tests");
    let err = result.unwrap_err();
    assert_eq!(
        unquoted(&err.summary()),
        "error: bogus exit code 127 for good revision",
        "{err:?}"
    );
    // The shell's own words: bash's "command not found", dash's (Ubuntu's /bin/sh)
    // "sh: 1: …: not found".
    assert!(lines.iter().any(|l| l.contains("not found")), "{lines:#?}");
    reset(&sb, &repo);

    // Local changes in the way of the next checkout.
    start(&sb, &repo, Some("HEAD"), &good, &[]);
    std::fs::write(repo.join("value.txt"), "local\n").unwrap();
    let (result, _) = run(&sb, &repo, "true");
    assert_eq!(result.unwrap_err().kind, GitErrorKind::WouldOverwrite);
    sb.run(&repo, &["checkout", "--", "value.txt"]);
    reset(&sb, &repo);
}

#[test]
fn bisect_run_can_be_cancelled() {
    let sb = Sandbox::new();
    let (repo, ids) = value_history(&sb);
    start(&sb, &repo, Some("HEAD"), &[ids[0].to_string()], &[]);

    let cancel = CancelToken::new();
    let started = Instant::now();
    let err = bisect_run(sb.git(), &repo, "sleep 60", &cancel, |line| {
        if line.starts_with("running ") {
            cancel.cancel();
        }
    })
    .unwrap_err();
    assert_eq!(err.kind, GitErrorKind::Cancelled, "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(20));

    // The bisect is still on, at the commit being tested.
    let current = state(&sb, &repo).unwrap();
    assert_eq!(current.progress, testing(ids[7], 7, 3));
    reset(&sb, &repo);
    assert_eq!(branch(&sb, &repo), "main");
}

/// A repository `name` of 16 commits `c1`…`c16`: odd ones change `path`, even ones
/// `other`. Returns the repo and the ids, oldest first.
fn path_history(sb: &Sandbox, name: &str, path: &str, other: &str) -> (PathBuf, Vec<ObjectId>) {
    let repo = sb.init_repo(name);
    let ids = (1..=16)
        .map(|i| {
            let file = if i % 2 == 1 { path } else { other };
            id(&sb.commit_file(&repo, file, &format!("{i}\n"), &format!("c{i}")))
        })
        .collect();
    (repo, ids)
}

fn position(ids: &[ObjectId], commit: ObjectId) -> usize {
    ids.iter()
        .position(|id| *id == commit)
        .unwrap_or_else(|| panic!("{commit} is not in the history"))
}

#[test]
fn a_bisect_started_with_a_path_agrees_with_one_started_in_a_terminal() {
    let sb = Sandbox::new();
    let path = "it's here!.txt";
    let paths = vec![PathBuf::from(path)];
    let (ours, ids) = path_history(&sb, "ours", path, "other.txt");
    let (theirs, their_ids) = path_history(&sb, "theirs", path, "other.txt");
    // The sandbox's commits are deterministic: the same ids in both.
    assert_eq!(ids, their_ids);
    let root = ids[0].to_string();

    let mut current = start(
        &sb,
        &ours,
        Some("HEAD"),
        std::slice::from_ref(&root),
        &paths,
    );
    let first = current.clone();
    let mut printed = sb.run(&theirs, &["bisect", "start", "HEAD", &root, "--", path]);
    let mut steps = 0;
    loop {
        let twin = assert_agrees_with_git(&sb, &theirs, &printed);
        assert_eq!(current, twin);
        assert_eq!(current.paths, paths);
        let BisectProgress::Testing { commit, .. } = current.progress else {
            break;
        };
        assert_eq!(head(&sb, &ours), commit);
        // Only commits that change the path are tested.
        let at = position(&ids, commit);
        assert_eq!(at % 2, 0, "c{} doesn't change the path", at + 1);
        steps += 1;
        assert!(steps <= 8, "too many steps");
        // Bad from c9 on.
        let (result, verb) = if at >= 8 {
            (BisectMark::Bad, "bad")
        } else {
            (BisectMark::Good, "good")
        };
        current = mark(&sb, &ours, result, None);
        printed = sb.run(&theirs, &["bisect", verb]);
    }
    assert_eq!(
        current.progress,
        BisectProgress::Found { first_bad: ids[8] }
    );

    // Started with the path only: the marks come later, and the path still applies.
    reset(&sb, &ours);
    let started = start(&sb, &ours, None, &[], &paths);
    assert_eq!(
        started.progress,
        BisectProgress::NeedsMarks {
            needs_bad: true,
            needs_good: true
        }
    );
    assert_eq!(started.paths, paths);
    mark(&sb, &ours, BisectMark::Bad, None);
    let marked = mark(&sb, &ours, BisectMark::Good, Some(&root));
    assert_eq!(marked.progress, first.progress);
}

#[test]
fn a_path_with_glob_characters_is_taken_literally() {
    let sb = Sandbox::new();
    // Odd commits change `*.txt`, even ones `a.txt`, which the glob `*.txt` matches too.
    let (repo, ids) = path_history(&sb, "r", "*.txt", "a.txt");
    // The same history with a plain name: what a literal `*.txt` must count like.
    let (twin, twin_ids) = path_history(&sb, "t", "star.txt", "a.txt");
    let star = vec![PathBuf::from("*.txt")];

    let mut current = start(&sb, &repo, Some("HEAD"), &[ids[0].to_string()], &star);
    assert_eq!(current.paths, star);
    let first = current.progress.clone();
    let twin_root = twin_ids[0].to_string();
    let mut printed = sb.run(
        &twin,
        &["bisect", "start", "HEAD", &twin_root, "--", "star.txt"],
    );
    let first_bad = loop {
        let theirs = assert_agrees_with_git(&sb, &twin, &printed);
        match (current.progress.clone(), theirs.progress) {
            (
                BisectProgress::Testing {
                    commit,
                    revisions_left,
                    steps_left,
                    ..
                },
                BisectProgress::Testing {
                    commit: twin_commit,
                    revisions_left: twin_left,
                    steps_left: twin_steps,
                    ..
                },
            ) => {
                let at = position(&ids, commit);
                assert_eq!(at, position(&twin_ids, twin_commit));
                assert_eq!((revisions_left, steps_left), (twin_left, twin_steps));
                assert_eq!(at % 2, 0, "c{} changes a.txt only", at + 1);
                let (result, verb) = if at >= 8 {
                    (BisectMark::Bad, "bad")
                } else {
                    (BisectMark::Good, "good")
                };
                current = mark(&sb, &repo, result, None);
                printed = sb.run(&twin, &["bisect", verb]);
            }
            (
                BisectProgress::Found { first_bad },
                BisectProgress::Found {
                    first_bad: twin_first_bad,
                },
            ) => {
                assert_eq!(
                    position(&ids, first_bad),
                    position(&twin_ids, twin_first_bad)
                );
                break first_bad;
            }
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(first_bad, ids[8]);

    // Typed in a terminal, the pathspec is a glob that matches a.txt too, and git counts
    // otherwise; the state still agrees with git.
    reset(&sb, &repo);
    let root = ids[0].to_string();
    let printed = sb.run(&repo, &["bisect", "start", "HEAD", &root, "--", "*.txt"]);
    let globbed = assert_agrees_with_git(&sb, &repo, &printed);
    assert_eq!(globbed.paths, star);
    assert_ne!(globbed.progress, first);
    reset(&sb, &repo);

    // An empty path would be the whole tree: refused before git runs.
    let err = bisect_start(
        sb.git(),
        &repo,
        Some("HEAD"),
        &[],
        &[PathBuf::new()],
        &CancelToken::new(),
    )
    .unwrap_err();
    assert!(err.summary().contains("not a valid pathspec"), "{err:?}");
    assert_eq!(err.exit_code, None);
    assert_eq!(state(&sb, &repo), None);
}

/// `m1`…`m10` on `main` and `s1`…`s3` on `side`, a branch off `m4`; each commit changes
/// `f.txt`. Returns the repo and the ids of both, oldest first.
fn forked_history(sb: &Sandbox) -> (PathBuf, Vec<ObjectId>, Vec<ObjectId>) {
    let repo = sb.init_repo("r");
    let commit = |name: String| id(&sb.commit_file(&repo, "f.txt", &format!("{name}\n"), &name));
    let mut main: Vec<ObjectId> = (1..=4).map(|i| commit(format!("m{i}"))).collect();
    sb.run(&repo, &["checkout", "-q", "-b", "side"]);
    let side = (1..=3).map(|i| commit(format!("s{i}"))).collect();
    sb.run(&repo, &["checkout", "-q", "main"]);
    main.extend((5..=10).map(|i| commit(format!("m{i}"))));
    (repo, main, side)
}

#[test]
fn a_good_commit_on_another_branch_tests_the_merge_base_first() {
    let sb = Sandbox::new();
    let (repo, main, side) = forked_history(&sb);
    let (m4, m10, s3) = (main[3], main[9], side[2]);
    let good = [s3.to_string()];

    // m4 is the merge base of m10 and s3, outside the range m10 --not s3 git counts.
    let started = start(&sb, &repo, Some(&m10.to_string()), &good, &[]);
    assert_eq!(started.progress, BisectProgress::MergeBase { commit: m4 });
    assert_eq!((started.bad, started.good), (Some(m10), vec![s3]));
    assert!(started.paths.is_empty());
    assert_eq!(head(&sb, &repo), m4);

    // Good: git bisects m5…m10, 2 revisions left after this (roughly 2 steps).
    let marked = mark(&sb, &repo, BisectMark::Good, None);
    assert!(marked.good.contains(&m4));
    let BisectProgress::Testing {
        commit,
        revisions_left: 2,
        steps_left: 2,
        ..
    } = marked.progress
    else {
        panic!("{marked:?}");
    };
    assert_eq!(head(&sb, &repo), commit);
    reset(&sb, &repo);

    // The same in a terminal: git says so, and then counts as we do.
    let printed = sb.run(
        &repo,
        &["bisect", "start", &m10.to_string(), &s3.to_string(), "--"],
    );
    assert!(printed.contains("a merge base must be tested"), "{printed}");
    assert_eq!(
        state(&sb, &repo).unwrap().progress,
        BisectProgress::MergeBase { commit: m4 }
    );
    let printed = sb.run(&repo, &["bisect", "good"]);
    assert_eq!(assert_agrees_with_git(&sb, &repo, &printed), marked);
    reset(&sb, &repo);

    // Bad: the change came between m4 and s3, and git stops.
    start(&sb, &repo, Some(&m10.to_string()), &good, &[]);
    let err = bisect_mark(sb.git(), &repo, BisectMark::Bad, None, &CancelToken::new()).unwrap_err();
    assert!(
        err.stderr.contains(&format!("The merge base {m4} is bad")),
        "{err:?}"
    );
    let current = state(&sb, &repo).unwrap();
    assert_eq!(
        (current.bad, current.progress),
        (Some(m4), BisectProgress::Contradictory)
    );
    reset(&sb, &repo);

    // With nothing but the bad commit left to bisect, git still tests the merge base.
    let started = start(&sb, &repo, Some(&main[4].to_string()), &good, &[]);
    assert_eq!(started.progress, BisectProgress::MergeBase { commit: m4 });
    let marked = mark(&sb, &repo, BisectMark::Good, None);
    assert_eq!(
        marked.progress,
        BisectProgress::Found { first_bad: main[4] }
    );
}
