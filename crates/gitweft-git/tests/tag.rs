use gitweft_git::CancelToken;
use gitweft_git::tag::{create_tag, delete_remote_tag, delete_tag, is_valid_tag_name, push_tag};
use gitweft_testkit::Sandbox;

#[test]
fn lightweight_and_annotated_tags() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let first = sb.commit_file(&repo, "a.txt", "1\n", "first");
    sb.commit_file(&repo, "a.txt", "2\n", "second");
    let cancel = CancelToken::new();

    for good in ["v1.0", "release/2026-01", "v2.0-rc.1"] {
        assert!(is_valid_tag_name(sb.git(), &repo, good, &cancel), "{good}");
    }
    for bad in ["", "has space", "a..b", "-x", "v1^", "x.lock"] {
        assert!(!is_valid_tag_name(sb.git(), &repo, bad, &cancel), "{bad:?}");
    }

    create_tag(sb.git(), &repo, "v0.1", &first, None, &cancel).unwrap();
    create_tag(
        sb.git(),
        &repo,
        "v0.2",
        "HEAD",
        Some("Second release\n\nNotes."),
        &cancel,
    )
    .unwrap();
    assert_eq!(sb.run(&repo, &["cat-file", "-t", "v0.1"]).trim(), "commit");
    assert_eq!(sb.run(&repo, &["cat-file", "-t", "v0.2"]).trim(), "tag");
    assert_eq!(sb.run(&repo, &["rev-parse", "v0.1^{commit}"]).trim(), first);
    assert!(
        sb.run(&repo, &["tag", "-l", "--format=%(contents)", "v0.2"])
            .starts_with("Second release")
    );

    delete_tag(sb.git(), &repo, "v0.1", &cancel).unwrap();
    assert_eq!(sb.run(&repo, &["tag"]), "v0.2\n");
}

#[test]
fn tags_are_pushed_and_deleted_on_a_remote() {
    let sb = Sandbox::new();
    let seed = sb.init_repo("seed");
    sb.commit_file(&seed, "a.txt", "1\n", "first");
    let bare = sb.path().join("remote.git");
    sb.run(
        sb.path(),
        &[
            "clone",
            "-q",
            "--bare",
            seed.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let url = format!("file://{}", bare.display());
    sb.run(sb.path(), &["clone", "-q", &url, "work"]);
    let work = sb.path().join("work");
    let cancel = CancelToken::new();

    create_tag(sb.git(), &work, "v1.0", "HEAD", Some("One"), &cancel).unwrap();
    push_tag(sb.git(), &work, "origin", "v1.0", &cancel, |_| {}).unwrap();
    assert!(
        sb.run(&work, &["ls-remote", "--tags", "origin"])
            .contains("refs/tags/v1.0")
    );

    delete_remote_tag(sb.git(), &work, "origin", "v1.0", &cancel).unwrap();
    assert!(
        !sb.run(&work, &["ls-remote", "--tags", "origin"])
            .contains("v1.0")
    );
    assert_eq!(sb.run(&work, &["tag"]), "v1.0\n", "the local tag stays");
}
