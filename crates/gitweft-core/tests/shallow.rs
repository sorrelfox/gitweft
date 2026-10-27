//! Loading a shallow clone: the history ends at the boundary instead of failing on the
//! parents that were never fetched.

use gitweft_core::repo::{LoadEnv, load_repo_in};
use gitweft_git::CancelToken;
use gitweft_testkit::Sandbox;

#[test]
fn a_shallow_clone_loads_and_ends_at_the_boundary() {
    let sb = Sandbox::new();
    let seed = sb.init_repo("seed");
    for n in 1..=5 {
        sb.commit_file(&seed, "f.txt", &format!("{n}\n"), &format!("c{n}"));
    }
    // `--depth` needs a URL; git ignores it for a plain local path.
    let url = format!("file://{}", seed.display());
    sb.run(sb.path(), &["clone", "-q", "--depth", "2", &url, "shallow"]);
    let repo = sb.path().join("shallow");
    // git doesn't write a commit-graph in a shallow repository (it exits 0 and writes
    // nothing), so loading takes the `git rev-list` walk.
    sb.run(&repo, &["commit-graph", "write"]);

    // The sandbox's data dir: the user's settings for a same-named repository are never
    // read (#50).
    let env = LoadEnv::isolated(sb.path().join("data"));
    let loaded = load_repo_in(&repo, &env, None, None, &CancelToken::new()).unwrap();
    assert!(loaded.timings.walk_via_git);
    // So the app doesn't offer a commit-graph here (#14).
    assert!(loaded.shallow);
    assert!(
        !load_repo_in(&seed, &env, None, None, &CancelToken::new())
            .unwrap()
            .shallow
    );
    assert_eq!(loaded.graph.rows(), 2);
    let head: gitweft_git::ObjectId = sb
        .run(&repo, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();
    assert_eq!(loaded.graph.commits.id(0), head);
    assert_eq!(loaded.graph.merges, [false, false]);
    assert_eq!(&*loaded.metas[1].summary, "c4");
}
