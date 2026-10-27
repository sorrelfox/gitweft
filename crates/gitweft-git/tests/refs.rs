use gitweft_git::CancelToken;
use gitweft_git::read::{
    HeadInfo, open_repo, read_refs, read_refs_cli, read_refs_with, uses_reftable,
};
use gitweft_testkit::Sandbox;

#[test]
fn a_reftable_repository_is_read_through_git() {
    let sb = Sandbox::new();
    let Some(repo) = sb.init_reftable_repo("rt") else {
        return;
    };
    let first = sb.commit_file(&repo, "a.txt", "1\n", "one");
    sb.run(&repo, &["tag", "-a", "v1", "-m", "release"]);
    let second = sb.commit_file(&repo, "a.txt", "2\n", "two");
    sb.run(&repo, &["branch", "topic", first.trim()]);
    let opened = open_repo(&repo).unwrap();

    // gix 0.87 has no reftable backend: its HEAD is a placeholder.
    assert!(uses_reftable(&opened));
    assert!(read_refs(&opened).is_err());

    let refs = read_refs_with(&opened, sb.git(), &CancelToken::new()).unwrap();
    assert_eq!(
        refs.head,
        HeadInfo::Branch {
            name: "main".into(),
            oid: second.trim().parse().unwrap()
        }
    );
    let names = |list: &[gitweft_git::read::RefEntry]| {
        list.iter()
            .map(|r| (r.short_name.clone(), r.target.to_string()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(&refs.local_branches),
        [
            ("main".to_owned(), second.trim().to_owned()),
            ("topic".to_owned(), first.trim().to_owned())
        ]
    );
    // The annotated tag at the commit it tags, as gix gives it.
    assert_eq!(
        names(&refs.tags),
        [("v1".to_owned(), first.trim().to_owned())]
    );
}

#[test]
fn reading_through_git_gives_what_gix_gives() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let cancel = CancelToken::new();
    let same = |what: &str| {
        let opened = open_repo(&repo).unwrap();
        assert!(!uses_reftable(&opened));
        assert_eq!(
            read_refs_cli(sb.git(), &repo, &cancel).unwrap(),
            read_refs(&opened).unwrap(),
            "{what}"
        );
    };
    same("unborn");
    let first = sb.commit_file(&repo, "a.txt", "1\n", "one");
    sb.commit_file(&repo, "a.txt", "2\n", "two");
    sb.run(&repo, &["tag", "-a", "v1", "-m", "release", first.trim()]);
    sb.run(&repo, &["tag", "light"]);
    sb.run(&repo, &["branch", "feature/x", first.trim()]);
    sb.run(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    sb.run(
        &repo,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    same("branches, tags, a remote and its HEAD");
    sb.run(&repo, &["switch", "-q", "--detach", first.trim()]);
    same("detached");
}

#[test]
fn unborn_head_in_empty_repo() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("empty");
    let refs = read_refs(&open_repo(&repo).unwrap()).unwrap();
    assert_eq!(
        refs.head,
        HeadInfo::Unborn {
            name: "main".into()
        }
    );
    assert!(refs.local_branches.is_empty());
}

#[test]
fn branches_tags_and_remotes() {
    let sb = Sandbox::new();
    let origin = sb.init_repo("origin");
    let first = sb.commit_file(&origin, "a.txt", "1\n", "one");
    sb.run(&origin, &["tag", "-a", "v1.0", "-m", "release"]);
    sb.run(&origin, &["tag", "light"]);
    sb.commit_file(&origin, "a.txt", "2\n", "two");

    sb.run(sb.path(), &["clone", "-q", "origin", "work"]);
    let work = sb.path().join("work");
    sb.run(&work, &["branch", "feature/x", &first]);

    let refs = read_refs(&open_repo(&work).unwrap()).unwrap();
    let names = |v: &[gitweft_git::read::RefEntry]| {
        v.iter().map(|r| r.short_name.clone()).collect::<Vec<_>>()
    };

    assert!(matches!(&refs.head, HeadInfo::Branch { name, .. } if name == "main"));
    assert_eq!(names(&refs.local_branches), ["feature/x", "main"]);
    // origin/HEAD is a symbolic ref and must be skipped.
    assert_eq!(names(&refs.remote_branches), ["origin/main"]);
    assert_eq!(names(&refs.tags), ["light", "v1.0"]);

    // Annotated tags are peeled to the commit they point at.
    let v1 = refs.tags.iter().find(|t| t.short_name == "v1.0").unwrap();
    assert_eq!(v1.target.to_string(), first);
    assert_eq!(refs.local_branches[0].full_name, "refs/heads/feature/x");
}

#[test]
fn detached_head() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let first = sb.commit_file(&repo, "a.txt", "1\n", "one");
    sb.commit_file(&repo, "a.txt", "2\n", "two");
    sb.run(&repo, &["checkout", "-q", "--detach", &first]);

    let refs = read_refs(&open_repo(&repo).unwrap()).unwrap();
    assert_eq!(
        refs.head,
        HeadInfo::Detached {
            oid: first.parse().unwrap()
        }
    );
}
