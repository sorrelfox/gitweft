//! Per-repository data keyed by `gitweft.id` (#19): assigned on the first load, the old
//! path-named folder moved under it, kept through a move, and a copy gets its own.

use std::path::Path;

use gitweft_core::paths::{legacy_repo_data_dir_in, repo_data_dir_in};
use gitweft_core::repo_id::{ensure_repo_id_in, moved_to_in, previous_roots_in};
use gitweft_git::CancelToken;
use gitweft_testkit::Sandbox;

/// `from` copied to `to`, `.git` included (as `cp -r` would).
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn config_id(sb: &Sandbox, repo: &Path) -> String {
    sb.run(repo, &["config", "gitweft.id"]).trim().to_owned()
}

#[test]
fn the_id_is_assigned_once_and_the_old_folder_moves_under_it() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("app");
    sb.commit_file(&repo, "a.txt", "x\n", "c1");
    let data = sb.path().join("data");
    let cancel = CancelToken::new();
    // Files from before ids: the folder named after the path.
    let legacy = legacy_repo_data_dir_in(&data, &repo);
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("oplog.jsonl"), "{}\n").unwrap();

    let key = ensure_repo_id_in(&data, sb.git(), &repo, &cancel).unwrap();
    let id = config_id(&sb, &repo);
    assert_eq!(key, id);
    assert!(id.starts_with("app-"), "{id}");
    let keyed = data.join("repos").join(&key);
    assert_eq!(repo_data_dir_in(&data, &repo), keyed);
    assert_eq!(
        std::fs::read_to_string(keyed.join("oplog.jsonl")).unwrap(),
        "{}\n"
    );
    assert!(!legacy.exists(), "moved, not copied");

    // A second load keeps the id.
    assert_eq!(
        ensure_repo_id_in(&data, sb.git(), &repo, &cancel),
        Some(key)
    );
    assert_eq!(config_id(&sb, &repo), id);
}

#[test]
fn a_moved_repository_keeps_its_data_and_a_copy_gets_its_own() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("app");
    sb.commit_file(&repo, "a.txt", "x\n", "c1");
    let data = sb.path().join("data");
    let cancel = CancelToken::new();
    let key = ensure_repo_id_in(&data, sb.git(), &repo, &cancel).unwrap();
    std::fs::write(data.join("repos").join(&key).join("settings.json"), "{}").unwrap();

    // Moved (renamed): the same id, so the same folder.
    let moved = sb.path().join("renamed-app");
    std::fs::rename(&repo, &moved).unwrap();
    assert_eq!(
        ensure_repo_id_in(&data, sb.git(), &moved, &cancel),
        Some(key.clone())
    );
    assert_eq!(
        repo_data_dir_in(&data, &moved),
        data.join("repos").join(&key)
    );

    // The old path is remembered: trash entries recorded there find the repository.
    assert_eq!(
        previous_roots_in(&data, &moved),
        std::slice::from_ref(&repo)
    );
    assert_eq!(moved_to_in(&data, &repo), Some(moved.clone()));
    assert_eq!(moved_to_in(&data, &sb.path().join("elsewhere")), None);

    // Copied, with the original still there: an id of its own, and no data of the
    // original's.
    let copy = sb.path().join("app-copy");
    copy_dir(&moved, &copy);
    let copy_key = ensure_repo_id_in(&data, sb.git(), &copy, &cancel).unwrap();
    assert_ne!(copy_key, key);
    assert_eq!(config_id(&sb, &copy), copy_key);
    assert!(
        !data
            .join("repos")
            .join(&copy_key)
            .join("settings.json")
            .exists()
    );
    // The original keeps its own.
    assert_eq!(
        ensure_repo_id_in(&data, sb.git(), &moved, &cancel),
        Some(key)
    );
}

#[test]
fn a_linked_worktree_has_a_folder_of_its_own() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("app");
    sb.commit_file(&repo, "a.txt", "x\n", "c1");
    let data = sb.path().join("data");
    let cancel = CancelToken::new();
    let key = ensure_repo_id_in(&data, sb.git(), &repo, &cancel).unwrap();
    let tree = sb.path().join("app-feature");
    sb.run(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            tree.to_str().unwrap(),
        ],
    );

    let tree_key = ensure_repo_id_in(&data, sb.git(), &tree, &cancel).unwrap();
    assert_eq!(tree_key, format!("{key}-wt-app-feature"));
    assert_eq!(config_id(&sb, &tree), key, "one id in the shared config");
}

#[test]
fn a_hostile_id_is_ignored() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("app");
    sb.run(&repo, &["config", "gitweft.id", "../../outside"]);
    let data = sb.path().join("data");
    assert_eq!(
        repo_data_dir_in(&data, &repo),
        legacy_repo_data_dir_in(&data, &repo)
    );
}
