//! `commit_template`: the `commit.template` file found as `git commit` finds it, split
//! into summary and description (#50: only the splitting was tested).

use gitweft_git::CancelToken;
use gitweft_git::ops::commit_template;
use gitweft_testkit::Sandbox;

#[test]
fn the_template_is_found_as_git_commit_finds_it() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let cancel = CancelToken::new();
    let template = || commit_template(sb.git(), &repo, &cancel);
    let pair = |summary: &str, description: &str| Some((summary.into(), description.into()));

    assert_eq!(template(), None, "not set");

    // A relative path is the working tree's, as for `git commit` run there.
    std::fs::write(
        repo.join(".gitmessage"),
        "# How to write it\nSummary here\n\nWhy it changed.\n",
    )
    .unwrap();
    sb.run(&repo, &["config", "commit.template", ".gitmessage"]);
    assert_eq!(template(), pair("Summary here", "Why it changed."));

    // `~/` is the user's home (the sandbox's): `git config --path` expands it.
    std::fs::write(sb.path().join("template.txt"), "From home\n").unwrap();
    sb.run(&repo, &["config", "commit.template", "~/template.txt"]);
    assert_eq!(template(), pair("From home", ""));

    // Another comment character.
    std::fs::write(sb.path().join("template.txt"), "; a hint\nSemicolons\n").unwrap();
    sb.run(&repo, &["config", "core.commentChar", ";"]);
    assert_eq!(template(), pair("Semicolons", ""));

    sb.run(&repo, &["config", "commit.template", "missing.txt"]);
    assert_eq!(template(), None, "a file that isn't there");
}
