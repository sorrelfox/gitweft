//! `load_repo_in` without an explicit tip filter reads it from the repository's
//! settings, under the data dir it's given (the sandbox's: the user's is never read, #50).

use gitweft_core::repo::{LoadEnv, load_repo_in};
use gitweft_core::repo_settings::{self, RepoSettings};
use gitweft_git::read::{TipFilter, WalkOptions};
use gitweft_git::{CancelToken, ObjectId};
use gitweft_testkit::Sandbox;

#[test]
fn the_tip_filter_comes_from_the_repository_settings() {
    let sb = Sandbox::new();
    let env = LoadEnv::isolated(sb.path().join("data"));

    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "f.txt", "base\n", "base");
    sb.run(&repo, &["switch", "-q", "-c", "wip"]);
    sb.commit_file(&repo, "w.txt", "wip\n", "wip 1");
    sb.run(&repo, &["switch", "-q", "main"]);
    let wip: ObjectId = sb.run(&repo, &["rev-parse", "wip"]).trim().parse().unwrap();
    let cancel = CancelToken::new();
    let load = |walk, tips| load_repo_in(&repo, &env, walk, tips, &cancel).unwrap();

    // No settings file: nothing hidden.
    let loaded = load(None, None);
    assert!(loaded.tips.is_empty());
    assert!(loaded.graph.row_of(&wip).is_some());

    let root = repo.canonicalize().unwrap();
    let tips = TipFilter {
        hidden: vec!["refs/heads/wip".into()],
        solo: None,
    };
    let walk = WalkOptions {
        topo_order: true,
        ..WalkOptions::default()
    };
    let settings = RepoSettings {
        walk,
        tips: tips.clone(),
        ..RepoSettings::default()
    };
    let file = repo_settings::settings_file_in(&env.data_dir, &root);
    repo_settings::save_file(&file, &settings).unwrap();

    // Neither given (the workspace's call): the saved filter and options.
    let loaded = load(None, None);
    assert_eq!((loaded.walk, &loaded.tips), (walk, &tips));
    assert_eq!(loaded.graph.row_of(&wip), None);

    // Explicit walk options, the filter from the settings.
    let loaded = load(Some(WalkOptions::default()), None);
    assert_eq!(loaded.walk, WalkOptions::default());
    assert_eq!(loaded.tips, tips);
    assert_eq!(loaded.graph.rows(), 1);

    // An explicit filter, the walk options from the settings.
    let loaded = load(None, Some(TipFilter::default()));
    assert_eq!(loaded.walk, walk);
    assert!(loaded.tips.is_empty());
    assert_eq!(loaded.graph.rows(), 2);
}
