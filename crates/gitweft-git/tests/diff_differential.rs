//! Differential test of the diff parser: the lines `file_diff` parses as added and
//! removed must be the ones `git diff --numstat` counts, on generated edits. The lines
//! include ones that look like diff syntax (`+`, `-`, `@@`, `\`, `---`, `+++`), and a file
//! may lack its final newline, which git marks with `\ No newline at end of file`.

use gitweft_git::gix::bstr::BString;
use gitweft_git::read::{DiffOptions, DiffSource, LineKind, file_diff};
use gitweft_git::{CancelToken, ObjectId};
use gitweft_testkit::Sandbox;
use proptest::prelude::*;

const LINES: &[&str] = &[
    "a",
    "b",
    "c",
    "",
    "+plus",
    "-minus",
    "@@ -1 +1 @@",
    "\\ backslash",
    " space",
    "tab\there",
    "--- a/f.txt",
    "+++ b/f.txt",
];

/// A file's text: up to 15 of the lines above, with or without a final newline.
fn content() -> impl Strategy<Value = String> {
    (
        prop::collection::vec(prop::sample::select(LINES), 0..16),
        any::<bool>(),
    )
        .prop_map(|(lines, final_newline)| {
            let mut text = lines.join("\n");
            if final_newline && !lines.is_empty() {
                text.push('\n');
            }
            text
        })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    #[test]
    fn parsed_lines_are_what_git_numstat_counts(
        old in content(),
        new in content(),
        context in 0u32..4,
    ) {
        prop_assume!(old != new);
        let sb = Sandbox::new();
        let repo = sb.init_repo("r");
        let parent: ObjectId = sb.commit_file(&repo, "f.txt", &old, "old").parse().unwrap();
        let commit: ObjectId = sb.commit_file(&repo, "f.txt", &new, "new").parse().unwrap();

        let source = DiffSource::Commit { parent: Some(parent), commit };
        let options = DiffOptions { context, ignore_whitespace: false };
        let path = BString::from("f.txt");
        let diff = file_diff(sb.git(), &repo, &source, &path, None, options, &CancelToken::new())
            .unwrap();
        let parsed = |kind| {
            diff.hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .filter(|line| line.kind == kind)
                .count()
        };

        let (parent, commit) = (parent.to_string(), commit.to_string());
        let numstat = sb.run(&repo, &["diff", "--numstat", "-M", &parent, &commit, "--", "f.txt"]);
        let mut counts = numstat.split('\t').map(|n| n.parse::<usize>().unwrap());
        let (added, removed) = (counts.next().unwrap(), counts.next().unwrap());
        prop_assert_eq!(
            (parsed(LineKind::Added), parsed(LineKind::Removed)),
            (added, removed),
            "old {:?}, new {:?}, -U{}", old, new, context
        );
    }
}
