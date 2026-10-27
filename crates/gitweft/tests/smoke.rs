//! `--headless-smoke` on a repository with a working tree and on a bare one.

use std::process::Output;

use gitweft_testkit::Sandbox;

const GITWEFT: &str = env!("CARGO_BIN_EXE_gitweft");

fn smoke(sb: &Sandbox, path: &std::path::Path) -> Output {
    // The test runs the app binary itself, not git, so it can't go through GitCli.
    #[allow(clippy::disallowed_methods)]
    std::process::Command::new(GITWEFT)
        .arg("--headless-smoke")
        .arg(path)
        .env("HOME", sb.path())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run gitweft")
}

#[test]
fn smoke_reports_a_working_tree_and_a_bare_repository() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("work");
    sb.commit_file(&repo, "a.txt", "one\n", "First");
    std::fs::write(repo.join("a.txt"), "two\n").unwrap();
    let bare = sb.path().join("bare.git");
    sb.run(sb.path(), &["clone", "-q", "--bare", "work", "bare.git"]);

    let work = smoke(&sb, &repo);
    let stdout = String::from_utf8_lossy(&work.stdout);
    assert!(
        work.status.success(),
        "{}",
        String::from_utf8_lossy(&work.stderr)
    );
    assert!(stdout.contains("changed paths: 1"), "{stdout}");

    // A bare repository has no working tree; the smoke test used to fail on it.
    let bare = smoke(&sb, &bare);
    let stdout = String::from_utf8_lossy(&bare.stdout);
    assert!(
        bare.status.success(),
        "{}",
        String::from_utf8_lossy(&bare.stderr)
    );
    assert!(
        stdout.contains("changed paths: none (bare repository)"),
        "{stdout}"
    );
    assert!(stdout.contains("commits: 1"), "{stdout}");
}
