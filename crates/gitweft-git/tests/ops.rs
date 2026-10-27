use gitweft_git::CancelToken;
use gitweft_git::ops::write_commit_graph;
use gitweft_git::read::open_repo;
use gitweft_testkit::Sandbox;

#[test]
fn writes_commit_graph_file() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a", "1\n", "one");
    sb.commit_file(&repo, "a", "2\n", "two");

    let gix_repo = open_repo(&repo).unwrap();
    assert!(matches!(gix_repo.commit_graph_if_enabled(), Ok(None)));

    write_commit_graph(sb.git(), gix_repo.git_dir(), &CancelToken::new()).unwrap();

    let gix_repo = open_repo(&repo).unwrap();
    let graph = gix_repo
        .commit_graph_if_enabled()
        .unwrap()
        .expect("commit-graph written");
    assert_eq!(graph.num_commits(), 2);
}
