use gitweft_git::CancelToken;
use gitweft_git::setup::{clone_repository, init_repository};
use gitweft_testkit::Sandbox;

#[test]
fn init_creates_missing_directories() {
    let sb = Sandbox::new();
    let dir = sb.path().join("projects/new-app");
    init_repository(sb.git(), &dir, &CancelToken::new()).unwrap();
    assert_eq!(sb.run(&dir, &["rev-parse", "--git-dir"]).trim(), ".git");
    // A second init leaves the repository alone.
    let head = sb.commit_file(&dir, "a.txt", "1\n", "first");
    init_repository(sb.git(), &dir, &CancelToken::new()).unwrap();
    assert_eq!(sb.run(&dir, &["rev-parse", "HEAD"]).trim(), head);
    assert_eq!(sb.run(&dir, &["status", "--porcelain"]), "");
}

#[test]
fn clone_can_check_out_submodules_too() {
    let sb = Sandbox::new();
    // Submodule clones over the file transport need this (see tests/submodules.rs).
    sb.run(
        sb.path(),
        &["config", "--global", "protocol.file.allow", "always"],
    );
    let lib = sb.init_repo("lib");
    let lib_head = sb.commit_file(&lib, "lib.txt", "lib\n", "lib");
    let origin = sb.init_repo("origin");
    sb.commit_file(&origin, "README", "hello\n", "first");
    sb.run(
        &origin,
        &["submodule", "add", "-q", lib.to_str().unwrap(), "lib"],
    );
    sb.run(&origin, &["commit", "-q", "-m", "add lib"]);
    let url = origin.to_str().unwrap();
    let cancel = CancelToken::new();

    let plain = sb.path().join("plain");
    clone_repository(sb.git(), url, &plain, false, &cancel, |_| {}).unwrap();
    assert!(plain.join("lib").is_dir());
    assert!(!plain.join("lib/lib.txt").exists(), "not checked out");

    let full = sb.path().join("full");
    clone_repository(sb.git(), url, &full, true, &cancel, |_| {}).unwrap();
    assert_eq!(
        std::fs::read_to_string(full.join("lib/lib.txt")).unwrap(),
        "lib\n"
    );
    assert_eq!(
        sb.run(&full.join("lib"), &["rev-parse", "HEAD"]).trim(),
        lib_head
    );
}

#[test]
fn clone_checks_out_the_default_branch_and_sets_origin() {
    let sb = Sandbox::new();
    let origin = sb.init_repo("origin");
    sb.commit_file(&origin, "README", "hello\n", "first");
    let dir = sb.path().join("work/copy");
    let url = origin.to_str().unwrap();

    clone_repository(sb.git(), url, &dir, false, &CancelToken::new(), |_| {}).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("README")).unwrap(),
        "hello\n"
    );
    assert_eq!(sb.run(&dir, &["remote", "get-url", "origin"]).trim(), url);
    assert_eq!(sb.run(&dir, &["branch", "--show-current"]).trim(), "main");

    // Not over an existing, non-empty directory.
    let err =
        clone_repository(sb.git(), url, &dir, false, &CancelToken::new(), |_| {}).unwrap_err();
    assert!(
        err.summary().contains("already exists"),
        "{}",
        err.summary()
    );
}

#[test]
fn clone_urls_are_never_options() {
    let sb = Sandbox::new();
    let dir = sb.path().join("x");
    let err = clone_repository(
        sb.git(),
        "--upload-pack=touch /tmp/pwned",
        &dir,
        false,
        &CancelToken::new(),
        |_| {},
    )
    .unwrap_err();
    // git treats it as a (missing) repository path, not as an option.
    assert!(
        err.summary().contains("does not exist"),
        "{}",
        err.summary()
    );
    let err =
        clone_repository(sb.git(), "  ", &dir, false, &CancelToken::new(), |_| {}).unwrap_err();
    assert!(err.summary().contains("no repository"), "{}", err.summary());
}
