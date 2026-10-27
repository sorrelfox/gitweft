use std::collections::HashSet;
use std::path::Path;

use gitweft_git::read::{Pickaxe, pickaxe};
use gitweft_git::{CancelToken, GitError, GitErrorKind, ObjectId};
use gitweft_testkit::Sandbox;

fn search(sb: &Sandbox, repo: &Path, query: &str) -> HashSet<ObjectId> {
    let query = Pickaxe::parse(query).unwrap();
    let mut found = HashSet::new();
    pickaxe(sb.git(), repo, &query, &CancelToken::new(), |ids| {
        found.extend(ids)
    })
    .unwrap();
    found
}

fn commit(sb: &Sandbox, repo: &Path, file: &str, content: &str, message: &str) -> ObjectId {
    sb.commit_file(repo, file, content, message)
        .parse()
        .unwrap()
}

#[test]
fn text_finds_count_changes_and_regex_finds_touched_lines() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let add = commit(&sb, &repo, "f.txt", "alpha\nneedle one\n", "Add the needle");
    commit(&sb, &repo, "g.txt", "unrelated\n", "Unrelated");
    // The needle stays once in the file: -S ignores this commit, -G sees the line change.
    let edit = commit(
        &sb,
        &repo,
        "f.txt",
        "alpha\nneedle two\n",
        "Edit the needle's line",
    );
    let remove = commit(&sb, &repo, "f.txt", "alpha\n", "Remove the needle");
    // A branch off an older commit is searched too (`--all`).
    sb.run(&repo, &["switch", "-q", "-c", "side", &add.to_string()]);
    let side = commit(
        &sb,
        &repo,
        "h.txt",
        "needle on a side branch\n",
        "Side needle",
    );

    assert_eq!(
        search(&sb, &repo, "needle"),
        HashSet::from([add, remove, side])
    );
    assert_eq!(
        search(&sb, &repo, "/nee+dle (one|two)/"),
        HashSet::from([add, edit, remove])
    );
    assert!(search(&sb, &repo, "no such text").is_empty());
}

#[test]
fn a_query_starting_with_a_dash_is_text() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let add = commit(&sb, &repo, "f.txt", "--output=x\n", "Add a flag");
    assert_eq!(search(&sb, &repo, "--output=x"), HashSet::from([add]));
    assert!(!repo.join("x").exists());
}

#[test]
fn cancelled_search_reports_cancellation() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    commit(&sb, &repo, "f.txt", "needle\n", "Add");
    let cancel = CancelToken::new();
    cancel.cancel();
    let query = Pickaxe::parse("needle").unwrap();
    let err = pickaxe(sb.git(), &repo, &query, &cancel, |_| {}).unwrap_err();
    let err = err.downcast_ref::<GitError>().expect("a git error");
    assert_eq!(err.kind, GitErrorKind::Cancelled);
}
