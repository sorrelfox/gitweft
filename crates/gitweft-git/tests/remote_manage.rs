//! Adding, renaming, removing and re-pointing remotes, against real `file://` remotes.

use std::path::{Path, PathBuf};

use gitweft_git::remote::{
    FetchTarget, RemoteInfo, add_remote, fetch, is_valid_remote_name, remote_names, remotes,
    remove_remote, rename_remote, set_remote_push_url, set_remote_url,
};
use gitweft_git::{CancelToken, GitErrorKind};
use gitweft_testkit::Sandbox;

/// A bare copy of `source` at `<sandbox>/<name>`, as a `file://` URL.
fn bare_copy(sb: &Sandbox, source: &Path, name: &str) -> String {
    let bare = sb.path().join(name);
    sb.run(
        sb.path(),
        &[
            "clone",
            "-q",
            "--bare",
            source.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    format!("file://{}", bare.display())
}

/// A bare remote with one commit (as a `file://` URL) and a clone of it, `work`.
fn remote_and_clone(sb: &Sandbox) -> (String, PathBuf) {
    let seed = sb.init_repo("seed");
    sb.commit_file(&seed, "a.txt", "1\n", "first");
    let url = bare_copy(sb, &seed, "remote.git");
    sb.run(sb.path(), &["clone", "-q", &url, "work"]);
    (url, sb.path().join("work"))
}

fn rev(sb: &Sandbox, repo: &Path, rev: &str) -> String {
    sb.run(repo, &["rev-parse", rev]).trim().to_owned()
}

fn remote_refs(sb: &Sandbox, repo: &Path) -> String {
    sb.run(
        repo,
        &["for-each-ref", "--format=%(refname)", "refs/remotes"],
    )
}

fn fetch_from(sb: &Sandbox, repo: &Path, remote: &str) {
    fetch(
        sb.git(),
        repo,
        FetchTarget::Remote(remote),
        false,
        &CancelToken::new(),
        |_| {},
    )
    .unwrap();
}

fn info(name: &str, url: &str, push_url: Option<&str>) -> RemoteInfo {
    RemoteInfo {
        name: name.into(),
        url: url.into(),
        push_url: push_url.map(Into::into),
    }
}

#[test]
fn remotes_lists_urls_and_push_urls_in_order() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let cancel = CancelToken::new();
    assert_eq!(remotes(sb.git(), &repo, &cancel).unwrap(), []);

    sb.run(&repo, &["remote", "add", "origin", "file:///srv/app.git"]);
    sb.run(
        &repo,
        &[
            "remote",
            "set-url",
            "--push",
            "origin",
            "ssh://git@example.invalid/app.git",
        ],
    );
    // A space, and a URL that looks like the end of a `git remote -v` line.
    sb.run(
        &repo,
        &[
            "remote",
            "add",
            "my.remote",
            "file:///srv/My Work/app (fetch)",
        ],
    );
    sb.run(
        &repo,
        &["remote", "add", "corp/mirror", "file:///srv/mirror.git"],
    );
    // A push URL equal to the fetch URL isn't a separate one.
    sb.run(
        &repo,
        &[
            "config",
            "remote.corp/mirror.pushurl",
            "file:///srv/mirror.git",
        ],
    );
    // A remote with a refspec but no URL.
    sb.run(
        &repo,
        &[
            "config",
            "remote.bare.fetch",
            "+refs/heads/*:refs/remotes/bare/*",
        ],
    );
    // Rewrite rules don't leak in: these are the configured URLs.
    sb.run(
        &repo,
        &["config", "url.file:///elsewhere/.insteadOf", "file:///srv/"],
    );

    assert_eq!(
        remotes(sb.git(), &repo, &cancel).unwrap(),
        [
            info("bare", "", None),
            info("corp/mirror", "file:///srv/mirror.git", None),
            info("my.remote", "file:///srv/My Work/app (fetch)", None),
            info(
                "origin",
                "file:///srv/app.git",
                Some("ssh://git@example.invalid/app.git")
            ),
        ]
    );
}

#[test]
fn an_added_remote_can_be_fetched() {
    let sb = Sandbox::new();
    let (url, work) = remote_and_clone(&sb);
    let cancel = CancelToken::new();

    for name in ["corp/mirror", "my.remote"] {
        add_remote(sb.git(), &work, name, &url, &cancel).unwrap();
        assert!(!remote_refs(&sb, &work).contains(name), "not fetched yet");
        fetch_from(&sb, &work, name);
        assert_eq!(
            rev(&sb, &work, &format!("refs/remotes/{name}/main")),
            rev(&sb, &work, "origin/main")
        );
    }
    assert_eq!(
        remote_names(sb.git(), &work, &cancel).unwrap(),
        ["corp/mirror", "my.remote", "origin"]
    );
    assert_eq!(remotes(sb.git(), &work, &cancel).unwrap()[1].url, url);
}

#[test]
fn rename_moves_the_refs_and_the_upstream_follows() {
    let sb = Sandbox::new();
    let (_, work) = remote_and_clone(&sb);
    let cancel = CancelToken::new();
    let main = rev(&sb, &work, "origin/main");

    rename_remote(sb.git(), &work, "origin", "corp/mirror", &cancel).unwrap();
    let refs = remote_refs(&sb, &work);
    assert!(
        refs.lines()
            .all(|r| r.starts_with("refs/remotes/corp/mirror/")),
        "{refs}"
    );
    assert_eq!(rev(&sb, &work, "refs/remotes/corp/mirror/main"), main);
    assert_eq!(
        sb.run(&work, &["rev-parse", "--abbrev-ref", "main@{upstream}"]),
        "corp/mirror/main\n"
    );
    assert_eq!(
        remote_names(sb.git(), &work, &cancel).unwrap(),
        ["corp/mirror"]
    );
    fetch_from(&sb, &work, "corp/mirror");
}

#[test]
fn remove_deletes_the_refs_and_the_upstream() {
    let sb = Sandbox::new();
    let (_, work) = remote_and_clone(&sb);
    let cancel = CancelToken::new();
    assert!(!remote_refs(&sb, &work).is_empty());

    remove_remote(sb.git(), &work, "origin", &cancel).unwrap();
    assert_eq!(remote_refs(&sb, &work), "");
    let config = sb.run(&work, &["config", "--list", "--local"]);
    assert!(!config.contains("branch.main."), "{config}");
    assert!(!config.contains("remote.origin"), "{config}");
    assert_eq!(remotes(sb.git(), &work, &cancel).unwrap(), []);
    assert!(branch_has_no_upstream(&sb, &work));
}

fn branch_has_no_upstream(sb: &Sandbox, repo: &Path) -> bool {
    sb.git()
        .command(repo)
        .args(["rev-parse", "--abbrev-ref", "main@{upstream}"])
        .read_only()
        .run(&CancelToken::new())
        .is_err()
}

#[test]
fn set_url_changes_where_the_next_fetch_goes() {
    let sb = Sandbox::new();
    let (url, work) = remote_and_clone(&sb);
    let cancel = CancelToken::new();

    // The project moves elsewhere, and gains a commit there.
    sb.run(sb.path(), &["clone", "-q", &url, "other"]);
    let other = sb.path().join("other");
    let moved_on = sb.commit_file(&other, "b.txt", "2\n", "second");
    let moved = bare_copy(&sb, &other, "moved.git");

    set_remote_url(sb.git(), &work, "origin", &moved, &cancel).unwrap();
    assert_eq!(
        remotes(sb.git(), &work, &cancel).unwrap(),
        [info("origin", &moved, None)]
    );
    fetch_from(&sb, &work, "origin");
    assert_eq!(rev(&sb, &work, "origin/main"), moved_on);
}

#[test]
fn a_separate_push_url_is_set_replaced_and_dropped() {
    let sb = Sandbox::new();
    let (url, work) = remote_and_clone(&sb);
    let cancel = CancelToken::new();
    let first = rev(&sb, &work, "HEAD");
    let mirror = bare_copy(&sb, &work, "mirror.git");

    set_remote_push_url(sb.git(), &work, "origin", Some(&mirror), &cancel).unwrap();
    assert_eq!(
        remotes(sb.git(), &work, &cancel).unwrap(),
        [info("origin", &url, Some(&mirror))]
    );
    // A push goes to the mirror; the remote fetched from doesn't get it.
    let pushed = sb.commit_file(&work, "b.txt", "2\n", "second");
    sb.run(&work, &["push", "-q", "origin", "main"]);
    assert_eq!(rev(&sb, &sb.path().join("mirror.git"), "main"), pushed);
    assert_eq!(rev(&sb, &sb.path().join("remote.git"), "main"), first);

    // Set again, it's replaced (one push URL, not two).
    let other = bare_copy(&sb, &work, "other.git");
    set_remote_push_url(sb.git(), &work, "origin", Some(&other), &cancel).unwrap();
    assert_eq!(
        sb.run(&work, &["config", "--get-all", "remote.origin.pushurl"])
            .trim(),
        other
    );

    // Dropped, pushes go to the URL again; dropping it again is no error.
    set_remote_push_url(sb.git(), &work, "origin", None, &cancel).unwrap();
    set_remote_push_url(sb.git(), &work, "origin", None, &cancel).unwrap();
    assert_eq!(
        remotes(sb.git(), &work, &cancel).unwrap(),
        [info("origin", &url, None)]
    );
    let err = set_remote_push_url(sb.git(), &work, "nope", Some(&other), &cancel).unwrap_err();
    assert!(err.stderr.contains("No such remote"), "{}", err.stderr);
}

#[test]
fn bad_names_and_missing_remotes_are_errors() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let cancel = CancelToken::new();
    add_remote(sb.git(), &repo, "origin", "file:///a.git", &cancel).unwrap();

    let err = add_remote(sb.git(), &repo, "origin", "file:///b.git", &cancel).unwrap_err();
    assert!(
        err.stderr.contains("remote origin already exists"),
        "{}",
        err.stderr
    );
    assert_eq!(err.kind, GitErrorKind::Other);
    for err in [
        rename_remote(sb.git(), &repo, "nope", "other", &cancel).unwrap_err(),
        remove_remote(sb.git(), &repo, "nope", &cancel).unwrap_err(),
        set_remote_url(sb.git(), &repo, "nope", "file:///c.git", &cancel).unwrap_err(),
    ] {
        assert!(err.stderr.contains("No such remote"), "{}", err.stderr);
    }
    for err in [
        add_remote(sb.git(), &repo, "a..b", "file:///c.git", &cancel).unwrap_err(),
        rename_remote(sb.git(), &repo, "origin", "a b", &cancel).unwrap_err(),
    ] {
        assert!(
            err.stderr.contains("is not a valid remote name"),
            "{}",
            err.stderr
        );
    }
    assert_eq!(
        remotes(sb.git(), &repo, &cancel).unwrap(),
        [info("origin", "file:///a.git", None)],
        "nothing changed"
    );

    for good in ["origin", "corp/mirror", "my.remote"] {
        assert!(
            is_valid_remote_name(sb.git(), &repo, good, &cancel),
            "{good}"
        );
    }
    for bad in ["", "a b", "-x", "a..b", "a/"] {
        assert!(
            !is_valid_remote_name(sb.git(), &repo, bad, &cancel),
            "{bad:?}"
        );
    }

    // `--` keeps a leading dash from being read as an option (`-f` would mean "fetch
    // now"): git takes it as a name, which is why callers check `is_valid_remote_name`.
    add_remote(sb.git(), &repo, "-f", "file:///d.git", &cancel).unwrap();
    assert_eq!(
        remote_names(sb.git(), &repo, &cancel).unwrap(),
        ["-f", "origin"]
    );
}
