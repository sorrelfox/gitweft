//! Moves between files, against git's own answer (#69).
//!
//! `moved_lines_across` looks for a block that left one file and arrived in another, as
//! `git diff --color-moved=blocks` does. Git says which lines it thinks moved by colouring
//! them (bold magenta on the way out, bold cyan on the way in, and alternating shades for
//! neighbouring blocks), while an ordinary change stays red or green. The tests below take
//! both from the same diff and compare what each one calls a move.

use std::collections::BTreeSet;

use gitweft_diff::{LineSide, MovedBetween, moved_lines_across};
use gitweft_testkit::Sandbox;

/// One file of a diff: its path, and every line of it as our detection takes them.
struct FileDiff {
    path: String,
    lines: Vec<(LineSide, String)>,
}

/// `git diff --no-color` split into files and lines.
fn parse(plain: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut in_hunk = false;
    for line in plain.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let path = rest.split(" b/").nth(1).unwrap_or_default().to_owned();
            files.push(FileDiff {
                path,
                lines: Vec::new(),
            });
            in_hunk = false;
            continue;
        }
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if !in_hunk || line.starts_with("\\ No newline") {
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        let (side, text) = match line.split_at_checked(1) {
            Some((" ", text)) => (LineSide::Context, text),
            Some(("-", text)) => (LineSide::Removed, text),
            Some(("+", text)) => (LineSide::Added, text),
            _ => continue,
        };
        file.lines.push((side, text.to_owned()));
    }
    files
}

/// The lines `git diff --color-moved=blocks` paints as moved: its move colours are 33 to 36
/// (magenta, cyan and the alternates), where a plain change is 31 or 32.
fn git_calls_moved(colored: &str) -> BTreeSet<(char, String)> {
    let mut moved = BTreeSet::new();
    for line in colored.lines() {
        let Some(rest) = line.strip_prefix("\u{1b}[") else {
            continue;
        };
        let Some((params, rest)) = rest.split_once('m') else {
            continue;
        };
        let move_colour = params
            .split(';')
            .any(|p| matches!(p, "33" | "34" | "35" | "36"));
        // Hunk headers are cyan too, and carry no line of their own.
        if !move_colour || rest.starts_with("@@") {
            continue;
        }
        let text = strip_colours(rest);
        let Some(mark) = text.chars().next().filter(|c| *c == '+' || *c == '-') else {
            continue;
        };
        moved.insert((mark, text[1..].to_owned()));
    }
    moved
}

fn strip_colours(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('\u{1b}') {
        out.push_str(&rest[..at]);
        rest = match rest[at..].split_once('m') {
            Some((_, after)) => after,
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// What our detection calls a move, in the same shape as git's answer.
fn we_call_moved(files: &[FileDiff]) -> (BTreeSet<(char, String)>, Vec<MovedBetween>) {
    let owned: Vec<Vec<(LineSide, &str)>> = files
        .iter()
        .map(|file| {
            file.lines
                .iter()
                .map(|(side, text)| (*side, text.as_str()))
                .collect()
        })
        .collect();
    let borrowed: Vec<&[(LineSide, &str)]> = owned.iter().map(Vec::as_slice).collect();
    let (per_file, ends) = moved_lines_across(&borrowed);
    let mut moved = BTreeSet::new();
    for (file, marks) in files.iter().zip(&per_file) {
        for ((side, text), mark) in file.lines.iter().zip(marks) {
            if mark.is_none() {
                continue;
            }
            let mark = match side {
                LineSide::Added => '+',
                LineSide::Removed => '-',
                LineSide::Context => continue,
            };
            moved.insert((mark, text.clone()));
        }
    }
    (moved, ends)
}

/// The repository's diff, plain and coloured, for the same revisions.
fn diffs(sb: &Sandbox, repo: &std::path::Path) -> (String, String) {
    let plain = sb.run(repo, &["diff", "--no-color", "-U3", "HEAD~1", "HEAD"]);
    let colored = sb.run(
        repo,
        &[
            "diff",
            "--color=always",
            "--color-moved=blocks",
            "-U3",
            "HEAD~1",
            "HEAD",
        ],
    );
    (plain, colored)
}

const HEADER_FN: &str = "\
fn parse_header(input: &str) -> Header {
    let (name, value) = input.split_once(':').unwrap();
    let value = value.trim().to_owned();
    Header { name: name.to_owned(), value }
}
";

#[test]
fn a_block_moved_to_another_file_is_the_move_git_sees() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("moves");
    std::fs::write(
        repo.join("a.rs"),
        format!("fn one() {{}}\n{HEADER_FN}fn two() {{}}\n"),
    )
    .unwrap();
    std::fs::write(repo.join("b.rs"), "fn three() {}\n").unwrap();
    sb.run(&repo, &["add", "."]);
    sb.commit_file(&repo, "keep.txt", "one\n", "before the move");

    // The block leaves a.rs for b.rs, and an unrelated line changes as well.
    std::fs::write(repo.join("a.rs"), "fn one() {}\nfn two() {}\n").unwrap();
    std::fs::write(repo.join("b.rs"), format!("fn three() {{}}\n{HEADER_FN}")).unwrap();
    std::fs::write(repo.join("keep.txt"), "two\n").unwrap();
    sb.run(&repo, &["add", "."]);
    sb.commit_file(&repo, "other.txt", "new file\n", "the move");

    let (plain, colored) = diffs(&sb, &repo);
    let files = parse(&plain);
    let (ours, ends) = we_call_moved(&files);
    let theirs = git_calls_moved(&colored);

    assert!(!theirs.is_empty(), "git found no move:\n{colored}");
    assert_eq!(ours, theirs, "we and git disagree about what moved");
    // One move, out of a.rs and into b.rs.
    let names: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(ends.len(), 1);
    assert_eq!(names[ends[0].from], "a.rs");
    assert_eq!(names[ends[0].to], "b.rs");
}

#[test]
fn an_ordinary_change_is_no_move_for_either_of_us() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("edits");
    std::fs::write(repo.join("a.rs"), format!("fn one() {{}}\n{HEADER_FN}")).unwrap();
    sb.run(&repo, &["add", "."]);
    sb.commit_file(&repo, "keep.txt", "one\n", "before the edit");

    std::fs::write(
        repo.join("a.rs"),
        format!("fn one() {{}}\n{}", HEADER_FN.replace("input", "text")),
    )
    .unwrap();
    sb.run(&repo, &["add", "."]);
    sb.commit_file(&repo, "keep.txt", "two\n", "the edit");

    let (plain, colored) = diffs(&sb, &repo);
    let (ours, ends) = we_call_moved(&parse(&plain));
    assert_eq!(git_calls_moved(&colored), BTreeSet::new());
    assert_eq!(ours, BTreeSet::new());
    assert!(ends.is_empty());
}
