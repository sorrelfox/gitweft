//! `load_repo_in` without explicit walk options reads them from the repository's settings,
//! under the data dir it's given (the sandbox's: the user's is never read, #50).

use gitweft_core::repo::{LoadEnv, load_repo_in};
use gitweft_core::repo_settings::{self, RepoSettings};
use gitweft_git::read::WalkOptions;
use gitweft_git::{CancelToken, ObjectId};
use gitweft_testkit::Sandbox;

#[test]
fn walk_options_come_from_the_repository_settings() {
    let sb = Sandbox::new();
    let env = LoadEnv::isolated(sb.path().join("data"));

    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "f.txt", "base\n", "base");
    sb.run(&repo, &["switch", "-q", "-c", "other"]);
    sb.commit_file(&repo, "o.txt", "other\n", "other 1");
    sb.run(&repo, &["switch", "-q", "main"]);
    let other: ObjectId = sb
        .run(&repo, &["rev-parse", "other"])
        .trim()
        .parse()
        .unwrap();
    let cancel = CancelToken::new();
    let load = |walk| load_repo_in(&repo, &env, walk, None, &cancel).unwrap();

    // No settings file: the defaults, every branch.
    let loaded = load(None);
    assert_eq!(loaded.walk, WalkOptions::default());
    assert!(loaded.graph.row_of(&other).is_some());

    // The root the app keeps the settings under: the (canonical) working tree.
    let root = repo.canonicalize().unwrap();
    let walk = WalkOptions {
        current_branch: true,
        topo_order: true,
        ..WalkOptions::default()
    };
    let settings = RepoSettings {
        walk,
        ..RepoSettings::default()
    };
    let file = repo_settings::settings_file_in(&env.data_dir, &root);
    repo_settings::save_file(&file, &settings).unwrap();
    assert!(file.starts_with(sb.path()));

    let loaded = load(None);
    assert_eq!(loaded.walk, walk);
    assert_eq!(loaded.graph.row_of(&other), None);
    assert_eq!(loaded.graph.rows(), 1);

    // Explicit options win over the settings.
    let loaded = load(Some(WalkOptions::default()));
    assert_eq!(loaded.graph.rows(), 2);
}
