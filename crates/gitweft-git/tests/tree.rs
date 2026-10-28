#![cfg(unix)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;

use gitweft_git::ObjectId;
use gitweft_git::read::{TreeEntryKind, open_repo, tree_entries};
use gitweft_testkit::Sandbox;

#[test]
fn lists_files_directories_links_and_submodules() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let base = sb.commit_file(&repo, "README.md", "hello\n", "Add readme");
    for (path, content) in [("src/lib.rs", "fn a() {}\n"), ("src/deep/mod.rs", "x\n")] {
        std::fs::create_dir_all(repo.join(path).parent().unwrap()).unwrap();
        std::fs::write(repo.join(path), content).unwrap();
    }
    std::fs::write(repo.join("run.sh"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(repo.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("README.md", repo.join("link")).unwrap();
    sb.run(&repo, &["add", "-A"]);
    // A gitlink, as `git submodule add` would record it.
    let gitlink = format!("160000,{base},vendor/sub");
    sb.run(&repo, &["update-index", "--add", "--cacheinfo", &gitlink]);
    sb.run(&repo, &["commit", "-qm", "Everything"]);
    let head: ObjectId = sb
        .run(&repo, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .unwrap();

    let entries = tree_entries(&open_repo(&repo).unwrap(), head).unwrap();
    let kinds: BTreeMap<String, TreeEntryKind> = entries
        .iter()
        .map(|e| (e.path.to_string(), e.kind))
        .collect();
    use TreeEntryKind::*;
    let expected: BTreeMap<String, TreeEntryKind> = [
        ("README.md", File),
        ("link", Symlink),
        ("run.sh", Executable),
        ("src", Directory),
        ("src/deep", Directory),
        ("src/deep/mod.rs", File),
        ("src/lib.rs", File),
        ("vendor", Directory),
        ("vendor/sub", Submodule),
    ]
    .into_iter()
    .map(|(path, kind)| (path.to_owned(), kind))
    .collect();
    assert_eq!(kinds, expected);

    let sub = entries.iter().find(|e| e.path == "vendor/sub").unwrap();
    assert_eq!(
        sub.id.to_string(),
        base,
        "a gitlink records the submodule's commit"
    );

    // Every entry's directory comes before it.
    for (ix, entry) in entries.iter().enumerate() {
        if let Some(slash) = entry.path.iter().rposition(|b| *b == b'/') {
            let parent = &entry.path[..slash];
            assert!(
                entries[..ix].iter().any(|e| e.path == parent),
                "{} before its directory",
                entry.path
            );
        }
    }
}
