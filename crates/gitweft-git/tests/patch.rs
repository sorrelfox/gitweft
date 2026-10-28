//! Partial staging against real git: the patch builder plus `git apply` must produce
//! exactly the content an independent model predicts.

use std::path::Path;

use gitweft_git::CancelToken;
use gitweft_git::gix::bstr::BString;
use gitweft_git::ops::apply_patch;
use gitweft_git::patch::{
    LineSelection, PatchTarget, build_patch, is_creation, select_hunks, supports_partial,
};
use gitweft_git::read::{DiffOptions, DiffSource, FileDiff, LineKind, file_diff};
use gitweft_testkit::Sandbox;
use proptest::prelude::*;

const FILE: &str = "file.txt";

/// Repository with `base` committed as FILE and `new` in the working tree.
fn repo_with_change(sb: &Sandbox, base: &str, new: &str, file: &str) -> std::path::PathBuf {
    let repo = sb.init_repo("r");
    std::fs::write(repo.join(file), base).unwrap();
    sb.run(&repo, &["add", "--", file]);
    sb.run(&repo, &["commit", "-qm", "base"]);
    std::fs::write(repo.join(file), new).unwrap();
    repo
}

fn diff_of(sb: &Sandbox, repo: &Path, source: DiffSource, file: &str) -> FileDiff {
    file_diff(
        sb.git(),
        repo,
        &source,
        &BString::from(file),
        None,
        DiffOptions::default(),
        &CancelToken::new(),
    )
    .unwrap()
}

fn index_content(sb: &Sandbox, repo: &Path, file: &str) -> String {
    sb.run(repo, &["show", &format!(":{file}")])
}

/// A repository with one commit and `content` as the untracked file `file`.
fn repo_with_untracked(sb: &Sandbox, file: &str, content: &str) -> std::path::PathBuf {
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "base.txt", "base\n", "base");
    std::fs::write(repo.join(file), content).unwrap();
    repo
}

fn added_texts(diff: &FileDiff) -> Vec<String> {
    diff.hunks
        .iter()
        .flat_map(|h| &h.lines)
        .filter(|l| l.kind == LineKind::Added)
        .map(|l| l.text.to_string())
        .collect()
}

#[test]
fn stage_some_lines_of_an_untracked_file() {
    let sb = Sandbox::new();
    let repo = repo_with_untracked(&sb, "new.txt", "a\nb\nc\n");
    let diff = diff_of(&sb, &repo, DiffSource::Untracked, "new.txt");
    assert!(supports_partial(&diff));

    let selection = select(&diff, |_, text| text != "b");
    let patch = build_patch(&diff, &selection, PatchTarget::Forward).unwrap();
    apply_patch(sb.git(), &repo, patch, true, false, &CancelToken::new()).unwrap();
    assert_eq!(index_content(&sb, &repo, "new.txt"), "a\nc\n");
    // What is left is an ordinary unstaged change.
    let rest = diff_of(&sb, &repo, DiffSource::Unstaged, "new.txt");
    assert_eq!(added_texts(&rest), ["b"]);
}

#[test]
fn unstage_a_line_of_a_staged_new_file() {
    let sb = Sandbox::new();
    let repo = repo_with_untracked(&sb, "new.txt", "a\nb\nc\n");
    sb.run(&repo, &["add", "new.txt"]);
    let diff = diff_of(&sb, &repo, DiffSource::Staged, "new.txt");
    assert_eq!(diff.new_file_mode.as_deref(), Some("100644"));

    let selection = select(&diff, |_, text| text == "c");
    let patch = build_patch(&diff, &selection, PatchTarget::Reverse).unwrap();
    apply_patch(sb.git(), &repo, patch, true, true, &CancelToken::new()).unwrap();
    assert_eq!(index_content(&sb, &repo, "new.txt"), "a\nb\n");
    assert_eq!(
        std::fs::read_to_string(repo.join("new.txt")).unwrap(),
        "a\nb\nc\n"
    );
}

#[test]
fn discard_a_line_of_an_untracked_file_keeps_it_untracked() {
    let sb = Sandbox::new();
    let repo = repo_with_untracked(&sb, "new.txt", "x\ny\nz\n");
    let diff = diff_of(&sb, &repo, DiffSource::Untracked, "new.txt");

    let selection = select(&diff, |_, text| text == "y");
    let patch = build_patch(&diff, &selection, PatchTarget::Reverse).unwrap();
    apply_patch(sb.git(), &repo, patch, false, true, &CancelToken::new()).unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.join("new.txt")).unwrap(),
        "x\nz\n"
    );
    assert!(sb.run(&repo, &["ls-files", "new.txt"]).is_empty());
}

#[cfg(unix)]
#[test]
fn partially_staged_executable_keeps_its_mode() {
    use std::os::unix::fs::PermissionsExt;
    let sb = Sandbox::new();
    let repo = repo_with_untracked(&sb, "run.sh", "#!/bin/sh\necho one\necho two\n");
    std::fs::set_permissions(repo.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let diff = diff_of(&sb, &repo, DiffSource::Untracked, "run.sh");
    assert_eq!(diff.new_file_mode.as_deref(), Some("100755"));

    let selection = select(&diff, |_, text| text != "echo two");
    let patch = build_patch(&diff, &selection, PatchTarget::Forward).unwrap();
    apply_patch(sb.git(), &repo, patch, true, false, &CancelToken::new()).unwrap();
    assert!(
        sb.run(&repo, &["ls-files", "-s", "run.sh"])
            .starts_with("100755")
    );
    assert_eq!(index_content(&sb, &repo, "run.sh"), "#!/bin/sh\necho one\n");
}

// Git quotes a name with a quote, a backslash or a control character, none of which
// Windows allows in one.
#[cfg(unix)]
#[test]
fn lines_of_a_file_whose_name_git_has_to_quote() {
    let sb = Sandbox::new();
    let file = "we\"ird\tname.txt";
    let repo = repo_with_change(&sb, "a\nb\nc\n", "a\nB\nc\nd\n", file);
    let diff = diff_of(&sb, &repo, DiffSource::Unstaged, file);
    assert_eq!(diff.path, file);

    let selection = select(&diff, |kind, text| kind == LineKind::Added && text == "d");
    let patch = build_patch(&diff, &selection, PatchTarget::Forward).unwrap();
    apply_patch(sb.git(), &repo, patch, true, false, &CancelToken::new()).unwrap();
    assert_eq!(index_content(&sb, &repo, file), "a\nb\nc\nd\n");
}

/// Indices of changed lines whose text satisfies `pred`.
fn select(diff: &FileDiff, pred: impl Fn(LineKind, &str) -> bool) -> LineSelection {
    let mut selection = LineSelection::new();
    for (h, hunk) in diff.hunks.iter().enumerate() {
        for (l, line) in hunk.lines.iter().enumerate() {
            if line.kind != LineKind::Context && pred(line.kind, &line.text.to_string()) {
                selection.insert((h, l));
            }
        }
    }
    selection
}

fn stage(sb: &Sandbox, repo: &Path, diff: &FileDiff, selection: &LineSelection) {
    let patch = build_patch(diff, selection, PatchTarget::Forward).expect("something selected");
    apply_patch(sb.git(), repo, patch, true, false, &CancelToken::new()).unwrap();
}

fn numbered(lines: impl IntoIterator<Item = String>) -> String {
    lines.into_iter().map(|l| l + "\n").collect()
}

#[test]
fn stage_one_hunk_of_two() {
    let sb = Sandbox::new();
    let base = numbered((1..=30).map(|i| format!("line {i}")));
    let new = base
        .replace("line 3\n", "line three\n")
        .replace("line 27\n", "line twenty-seven\n");
    let repo = repo_with_change(&sb, &base, &new, FILE);

    let diff = diff_of(&sb, &repo, DiffSource::Unstaged, FILE);
    assert_eq!(diff.hunks.len(), 2);
    assert!(supports_partial(&diff));
    stage(&sb, &repo, &diff, &select_hunks(&diff, [1]));

    assert_eq!(
        index_content(&sb, &repo, FILE),
        base.replace("line 27\n", "line twenty-seven\n")
    );
    // The other hunk is still unstaged.
    let rest = diff_of(&sb, &repo, DiffSource::Unstaged, FILE);
    assert_eq!(rest.hunks.len(), 1);
    assert!(rest.hunks[0].lines.iter().any(|l| l.text == "line three"));
}

#[test]
fn stage_single_lines_inside_a_hunk() {
    let sb = Sandbox::new();
    let base = "a\nb\nc\nd\ne\n";
    let new = "a\nB\nc\nD\nnew\ne\n";
    let repo = repo_with_change(&sb, base, new, FILE);
    let diff = diff_of(&sb, &repo, DiffSource::Unstaged, FILE);

    // Stage the "d" → "D" replacement and the added "new" line, but not "b" → "B".
    let selection = select(&diff, |_, text| matches!(text, "d" | "D" | "new"));
    stage(&sb, &repo, &diff, &selection);
    assert_eq!(index_content(&sb, &repo, FILE), "a\nb\nc\nD\nnew\ne\n");

    // Now unstage just the added "new" line from the index.
    let staged = diff_of(&sb, &repo, DiffSource::Staged, FILE);
    let selection = select(&staged, |kind, text| {
        kind == LineKind::Added && text == "new"
    });
    let patch = build_patch(&staged, &selection, PatchTarget::Reverse).unwrap();
    apply_patch(sb.git(), &repo, patch, true, true, &CancelToken::new()).unwrap();
    assert_eq!(index_content(&sb, &repo, FILE), "a\nb\nc\nD\ne\n");
    // The working tree was never touched.
    assert_eq!(std::fs::read_to_string(repo.join(FILE)).unwrap(), new);
}

#[test]
fn discard_a_single_line_from_the_working_tree() {
    let sb = Sandbox::new();
    let repo = repo_with_change(&sb, "a\nb\nc\n", "a\nB\nc\nextra\n", FILE);
    let diff = diff_of(&sb, &repo, DiffSource::Unstaged, FILE);
    let selection = select(&diff, |_, text| text == "extra");
    let patch = build_patch(&diff, &selection, PatchTarget::Reverse).unwrap();
    apply_patch(sb.git(), &repo, patch, false, true, &CancelToken::new()).unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.join(FILE)).unwrap(),
        "a\nB\nc\n"
    );
}

#[test]
fn crlf_lines_and_paths_with_spaces() {
    let sb = Sandbox::new();
    let file = "dir with space/win file.txt";
    std::fs::create_dir_all(sb.path().join("r/dir with space")).unwrap();
    let base = "one\r\ntwo\r\nthree\r\n";
    let new = "one\r\nTWO\r\nthree\r\nfour\r\n";
    let repo = repo_with_change(&sb, base, new, file);
    let diff = diff_of(&sb, &repo, DiffSource::Unstaged, file);
    let selection = select(&diff, |_, text| text.starts_with("four"));
    stage(&sb, &repo, &diff, &selection);
    assert_eq!(
        index_content(&sb, &repo, file),
        "one\r\ntwo\r\nthree\r\nfour\r\n"
    );
}

#[test]
fn no_newline_at_end_of_file() {
    let sb = Sandbox::new();
    // The last line gains a newline and a new line is added after it.
    let repo = repo_with_change(&sb, "a\nb", "a\nb\nc\n", FILE);
    let diff = diff_of(&sb, &repo, DiffSource::Unstaged, FILE);
    stage(&sb, &repo, &diff, &select_hunks(&diff, [0]));
    assert_eq!(index_content(&sb, &repo, FILE), "a\nb\nc\n");
}

#[test]
fn is_creation_only_for_files_that_do_not_exist_yet() {
    let sb = Sandbox::new();
    let repo = repo_with_untracked(&sb, "new.txt", "a\nb\n");
    assert!(is_creation(&diff_of(
        &sb,
        &repo,
        DiffSource::Untracked,
        "new.txt"
    )));
    sb.run(&repo, &["add", "new.txt"]);
    assert!(is_creation(&diff_of(
        &sb,
        &repo,
        DiffSource::Staged,
        "new.txt"
    )));
    sb.run(&repo, &["commit", "-qm", "new"]);
    let rev = |rev: &str| sb.run(&repo, &["rev-parse", rev]).trim().parse().unwrap();
    let commit = DiffSource::Commit {
        parent: Some(rev("HEAD^")),
        commit: rev("HEAD"),
    };
    assert!(is_creation(&diff_of(&sb, &repo, commit, "new.txt")));

    // A change to a file that exists.
    std::fs::write(repo.join("new.txt"), "a\nB\n").unwrap();
    assert!(!is_creation(&diff_of(
        &sb,
        &repo,
        DiffSource::Unstaged,
        "new.txt"
    )));
}

#[test]
fn an_empty_tracked_file_that_gains_lines_is_not_a_creation() {
    let sb = Sandbox::new();
    let repo = repo_with_change(&sb, "", "a\nb\nc\n", FILE);
    let diff = diff_of(&sb, &repo, DiffSource::Unstaged, FILE);
    // git shows it all added (`@@ -0,0 +1,3 @@`), like a new file, but it is in the index.
    assert_eq!((diff.hunks[0].old_start, diff.hunks[0].old_len), (0, 0));
    assert_eq!(diff.new_file_mode, None);
    assert!(!is_creation(&diff));
    assert!(supports_partial(&diff));
    // So staging some of its lines is an ordinary patch, not a "new file" one.
    stage(&sb, &repo, &diff, &select(&diff, |_, text| text != "b"));
    assert_eq!(index_content(&sb, &repo, FILE), "a\nc\n");
}

#[test]
fn whole_file_changes_do_not_support_partial_staging() {
    let sb = Sandbox::new();
    let repo = repo_with_change(&sb, "a\nb\n", "a\nb\n", FILE);
    let partial =
        |source: DiffSource, path: &str| supports_partial(&diff_of(&sb, &repo, source, path));

    // Nothing changed: no hunks.
    assert!(!partial(DiffSource::Unstaged, FILE));

    // A deletion.
    std::fs::remove_file(repo.join(FILE)).unwrap();
    let deleted = diff_of(&sb, &repo, DiffSource::Unstaged, FILE);
    assert!(!deleted.hunks.is_empty());
    assert!(!supports_partial(&deleted));
    sb.run(&repo, &["checkout", "--", FILE]);

    // A rename.
    sb.run(&repo, &["mv", FILE, "moved.txt"]);
    let renamed = file_diff(
        sb.git(),
        &repo,
        &DiffSource::Staged,
        &BString::from("moved.txt"),
        Some(&BString::from(FILE)),
        DiffOptions::default(),
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(renamed.old_path, Some(BString::from(FILE)));
    assert!(!supports_partial(&renamed));
    sb.run(&repo, &["reset", "-q", "--hard"]);

    // Binary, a symlink, and an empty file (no hunks).
    std::fs::write(repo.join("bin.dat"), b"\0\x01\x02\n").unwrap();
    assert!(!partial(DiffSource::Untracked, "bin.dat"));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(FILE, repo.join("link")).unwrap();
        let link = diff_of(&sb, &repo, DiffSource::Untracked, "link");
        assert_eq!(link.new_file_mode.as_deref(), Some("120000"));
        assert!(!supports_partial(&link));
    }
    std::fs::write(repo.join("empty.txt"), "").unwrap();
    assert!(!partial(DiffSource::Untracked, "empty.txt"));

    // A plain change does.
    std::fs::write(repo.join(FILE), "a\nB\n").unwrap();
    assert!(partial(DiffSource::Unstaged, FILE));
}

/// The content the target file must have after applying `selection`: the model the
/// patch builder is checked against. `forward`: old side + selected changes;
/// otherwise: new side with selected changes reverted.
fn expected(
    old: &[&str],
    new: &[&str],
    diff: &FileDiff,
    selection: &LineSelection,
    forward: bool,
) -> String {
    let side = if forward { old } else { new };
    let mut out: Vec<String> = Vec::new();
    let mut pos = 0usize; // next unconsumed line of `side` (0-based)
    for (h, hunk) in diff.hunks.iter().enumerate() {
        let (start, len) = if forward {
            (hunk.old_start, hunk.old_len)
        } else {
            (hunk.new_start, hunk.new_len)
        };
        let first = if len == 0 {
            start as usize
        } else {
            start as usize - 1
        };
        out.extend(side[pos..first].iter().map(|s| s.to_string()));
        for (l, line) in hunk.lines.iter().enumerate() {
            let chosen = selection.contains(&(h, l));
            let keep = match (line.kind, forward) {
                (LineKind::Context, _) => true,
                (LineKind::Added, true) => chosen,
                (LineKind::Removed, true) => !chosen,
                (LineKind::Added, false) => !chosen,
                (LineKind::Removed, false) => chosen,
            };
            if keep {
                out.push(line.text.to_string());
            }
        }
        pos = first + len as usize;
    }
    out.extend(side[pos..].iter().map(|s| s.to_string()));
    numbered(out)
}

fn arb_file() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(
        prop::sample::select(vec!["a", "b", "c", "fn x() {", "}", "let y = 1;"]),
        1..25,
    )
    .prop_map(|lines| lines.into_iter().map(String::from).collect())
}

/// Random edits (replace / delete / insert at random positions).
fn arb_edit(base: Vec<String>) -> impl Strategy<Value = (Vec<String>, Vec<String>)> {
    let len = base.len();
    prop::collection::vec(
        (
            0..3u8,
            0..len.max(1),
            prop::sample::select(vec!["X", "Y", "new line", "b"]),
        ),
        1..6,
    )
    .prop_map(move |edits| {
        let mut new = base.clone();
        for (op, at, text) in edits {
            let at = at.min(new.len());
            match op {
                0 if at < new.len() => new[at] = text.to_owned(),
                1 if at < new.len() && new.len() > 1 => {
                    new.remove(at);
                }
                _ => new.insert(at, text.to_owned()),
            }
        }
        (base.clone(), new)
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    #[test]
    fn random_line_selections_stage_and_discard_exactly(
        (old, new) in arb_file().prop_flat_map(arb_edit),
        picks in prop::collection::vec(any::<bool>(), 64),
    ) {
        prop_assume!(old != new);
        let sb = Sandbox::new();
        let repo = repo_with_change(&sb, &numbered(old.clone()), &numbered(new.clone()), FILE);
        let diff = diff_of(&sb, &repo, DiffSource::Unstaged, FILE);
        prop_assume!(supports_partial(&diff));

        let changed: Vec<(usize, usize)> = diff.hunks.iter().enumerate()
            .flat_map(|(h, hunk)| hunk.lines.iter().enumerate()
                .filter(|(_, l)| l.kind != LineKind::Context).map(move |(l, _)| (h, l)))
            .collect();
        let selection: LineSelection = changed.iter().zip(picks.iter().cycle())
            .filter(|(_, pick)| **pick).map(|(line, _)| *line).collect();
        prop_assume!(!selection.is_empty());

        let old_refs: Vec<&str> = old.iter().map(String::as_str).collect();
        let new_refs: Vec<&str> = new.iter().map(String::as_str).collect();

        // Stage the selection into the index.
        let patch = build_patch(&diff, &selection, PatchTarget::Forward).unwrap();
        apply_patch(sb.git(), &repo, patch, true, false, &CancelToken::new()).unwrap();
        prop_assert_eq!(index_content(&sb, &repo, FILE), expected(&old_refs, &new_refs, &diff, &selection, true));

        // Independently: discarding the same selection from the working tree.
        sb.run(&repo, &["reset", "-q"]);
        let patch = build_patch(&diff, &selection, PatchTarget::Reverse).unwrap();
        apply_patch(sb.git(), &repo, patch, false, true, &CancelToken::new()).unwrap();
        prop_assert_eq!(
            std::fs::read_to_string(repo.join(FILE)).unwrap(),
            expected(&old_refs, &new_refs, &diff, &selection, false)
        );
    }
}
