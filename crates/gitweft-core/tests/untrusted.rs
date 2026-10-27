//! Opening a folder another user owns. The load succeeds: gix reads the repository
//! and doesn't refuse it. git refuses it ("dubious ownership") from the first command the
//! window runs next, `git status`, so the refusal is met on a loaded repository, where the
//! banner offers Trust; the start screen never sees it. The folder is found in git's
//! message, and once it is in `safe.directory` the status reads.
//!
//! git's own test hook `GIT_TEST_ASSUME_DIFFERENT_OWNER` makes every repository someone
//! else's. It is set for the whole process, so this file holds one test: a test binary of
//! its own, where it can't reach another test.

use gitweft_core::repo::{LoadEnv, load_repo_in};
use gitweft_git::repair::{trust_directory, untrusted_folder};
use gitweft_git::{CancelToken, GitCli, GitErrorKind};
use gitweft_testkit::Sandbox;

#[test]
fn a_folder_another_user_owns_loads_but_git_refuses_it_until_trusted() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("theirs");
    sb.commit_file(&repo, "a.txt", "one\n", "first");
    let env = LoadEnv::isolated(sb.path().join("data"));

    // SAFETY: this binary runs this one test, and nothing else reads the environment
    // while it changes.
    unsafe {
        // The app's own git reads these: a global config of the sandbox's, never the user's.
        std::env::set_var("GIT_CONFIG_GLOBAL", sb.path().join("global.gitconfig"));
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1");
    }
    let git = GitCli::discover().unwrap();
    let status = |cancel: &CancelToken| {
        git.command(&repo)
            .args(["status", "--porcelain=v2", "-z"])
            .read_only()
            .run(cancel)
    };

    // The load goes through: the start screen has nothing to offer Trust for.
    let loaded = load_repo_in(&repo, &env, None, None, &CancelToken::new())
        .expect("gix reads a folder another user owns");
    assert_eq!(loaded.graph.rows(), 1);

    // git's refusal comes with the status, classified, and names the folder.
    let refused = status(&CancelToken::new()).expect_err("git refuses the folder");
    assert_eq!(refused.kind, GitErrorKind::DubiousOwnership, "{refused:?}");
    let folder =
        untrusted_folder(&refused.stderr, Some(&repo)).expect("the folder, from git's message");
    assert_eq!(folder, repo.canonicalize().unwrap());

    // What the banner's Trust does, then the same status.
    trust_directory(&git, &folder, &CancelToken::new()).unwrap();
    status(&CancelToken::new()).expect("a trusted folder's status reads");
}
