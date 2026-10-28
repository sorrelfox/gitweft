use std::path::{Path, PathBuf};

use gitweft_git::submodules::{
    SubmoduleInfo, SubmoduleState, add_submodule, deinit_submodule, list_submodules,
    list_submodules_nested, remove_submodule, sync_submodules, update_submodules,
    update_submodules_with_events,
};
use gitweft_git::{CancelToken, GitErrorKind, ObjectId, StderrEvent};
use gitweft_testkit::Sandbox;

fn id(hex: &str) -> ObjectId {
    ObjectId::from_hex(hex.trim().as_bytes()).unwrap()
}

/// A sandbox that allows `file` transport for submodules, which git refuses by default
/// (`protocol.file.allow=user`, and submodule clones don't count as "user"). Set in the
/// sandbox's own global config file, never in the library.
fn sandbox() -> Sandbox {
    let sb = Sandbox::new();
    sb.run(
        sb.path(),
        &["config", "--global", "protocol.file.allow", "always"],
    );
    sb
}

/// A bare repository `<name>.git` with three commits (`1`, `2`, `3`); returns its path
/// and the commit ids, oldest first.
fn library(sb: &Sandbox, name: &str) -> (PathBuf, [ObjectId; 3]) {
    let work = sb.init_repo(&format!("{name}-work"));
    let ids = ["1", "2", "3"].map(|n| id(&sb.commit_file(&work, "lib.txt", n, n)));
    let bare = sb.path().join(format!("{name}.git"));
    sb.run(
        sb.path(),
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    (bare, ids)
}

struct Fixture {
    sb: Sandbox,
    superproject: PathBuf,
    /// `liba` is named `liba`; `vendor/lib b` is named `lib.b` (a name with a dot).
    a: (PathBuf, [ObjectId; 3]),
    b: (PathBuf, [ObjectId; 3]),
}

const A: &str = "liba";
const B: &str = "vendor/lib b";

/// A superproject with two submodules added from bare repositories, both at their
/// third commit.
fn fixture() -> Fixture {
    let sb = sandbox();
    let a = library(&sb, "a");
    let b = library(&sb, "b");
    let superproject = sb.init_repo("super");
    sb.commit_file(&superproject, "README", "super\n", "init");
    sb.run(
        &superproject,
        &["submodule", "add", "-q", a.0.to_str().unwrap(), A],
    );
    sb.run(
        &superproject,
        &[
            "submodule",
            "add",
            "-q",
            "--name",
            "lib.b",
            b.0.to_str().unwrap(),
            B,
        ],
    );
    sb.run(&superproject, &["commit", "-q", "-m", "add submodules"]);
    Fixture {
        sb,
        superproject,
        a,
        b,
    }
}

/// A clone of the superproject without `--recurse-submodules`.
fn fresh_clone(f: &Fixture) -> PathBuf {
    let clone = f.sb.path().join("clone");
    f.sb.run(
        f.sb.path(),
        &[
            "clone",
            "-q",
            f.superproject.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    clone
}

fn list(sb: &Sandbox, repo: &Path) -> Vec<SubmoduleInfo> {
    list_submodules(sb.git(), repo, &CancelToken::new()).unwrap()
}

fn states(sb: &Sandbox, repo: &Path) -> Vec<(PathBuf, SubmoduleState)> {
    list(sb, repo)
        .into_iter()
        .map(|s| (s.path, s.state))
        .collect()
}

fn update(sb: &Sandbox, repo: &Path, paths: &[&str]) {
    let paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
    update_submodules(sb.git(), repo, &paths, false, &CancelToken::new()).unwrap();
}

fn config(sb: &Sandbox, repo: &Path, key: &str) -> String {
    sb.run(repo, &["config", "--get", key]).trim().to_owned()
}

#[test]
fn lists_names_urls_and_recorded_commits() {
    let f = fixture();
    let a_url = f.a.0.to_str().unwrap().to_owned();
    let b_url = f.b.0.to_str().unwrap().to_owned();
    assert_eq!(
        list(&f.sb, &f.superproject),
        [
            SubmoduleInfo {
                path: A.into(),
                name: A.into(),
                url: Some(a_url),
                recorded: Some(f.a.1[2]),
                checked_out: Some(f.a.1[2]),
                superproject: PathBuf::new(),
                depth: 0,
                state: SubmoduleState::UpToDate,
            },
            SubmoduleInfo {
                path: B.into(),
                name: "lib.b".into(),
                url: Some(b_url),
                recorded: Some(f.b.1[2]),
                checked_out: Some(f.b.1[2]),
                superproject: PathBuf::new(),
                depth: 0,
                state: SubmoduleState::UpToDate,
            },
        ]
    );
}

#[test]
fn a_fresh_clone_has_uninitialized_submodules() {
    let f = fixture();
    let clone = fresh_clone(&f);
    // The folders exist, empty: not repositories.
    assert!(clone.join(A).is_dir() && clone.join(B).is_dir());

    let listed = list(&f.sb, &clone);
    assert_eq!(listed.len(), 2, "{listed:?}");
    for (info, recorded) in listed.iter().zip([f.a.1[2], f.b.1[2]]) {
        assert_eq!(info.state, SubmoduleState::Uninitialized, "{info:?}");
        assert_eq!(info.recorded, Some(recorded));
        assert_eq!(info.checked_out, None);
    }
    assert_eq!(listed[1].name, "lib.b");
    assert_eq!(listed[1].path, Path::new(B));
}

#[test]
fn update_one_then_all() {
    let f = fixture();
    let clone = fresh_clone(&f);

    // Only the path given, and a path with a space.
    update(&f.sb, &clone, &[B]);
    let listed = list(&f.sb, &clone);
    assert_eq!(listed[0].state, SubmoduleState::Uninitialized);
    assert_eq!(listed[1].state, SubmoduleState::UpToDate);
    assert_eq!(listed[1].checked_out, listed[1].recorded);
    assert_eq!(
        std::fs::read_to_string(clone.join(B).join("lib.txt")).unwrap(),
        "3"
    );

    // Move the submodule to another commit.
    f.sb.run(
        &clone.join(B),
        &["checkout", "-q", "--detach", &f.b.1[0].to_string()],
    );
    let listed = list(&f.sb, &clone);
    assert_eq!(listed[1].state, SubmoduleState::Changed);
    assert_eq!(listed[1].checked_out, Some(f.b.1[0]));
    assert_eq!(listed[1].recorded, Some(f.b.1[2]));

    update(&f.sb, &clone, &[]);
    assert_eq!(
        states(&f.sb, &clone),
        [
            (PathBuf::from(A), SubmoduleState::UpToDate),
            (PathBuf::from(B), SubmoduleState::UpToDate),
        ]
    );
    for info in list(&f.sb, &clone) {
        assert_eq!(info.checked_out, info.recorded, "{info:?}");
    }
}

#[test]
fn paths_are_literal() {
    let f = fixture();
    let clone = fresh_clone(&f);
    // A glob that would match `liba` is not a pathspec here: git finds no such submodule.
    let err = update_submodules(
        f.sb.git(),
        &clone,
        &[PathBuf::from("lib*")],
        false,
        &CancelToken::new(),
    )
    .unwrap_err();
    assert!(err.summary().contains("did not match"), "{err:?}");
    assert_eq!(
        states(&f.sb, &clone),
        [
            (PathBuf::from(A), SubmoduleState::Uninitialized),
            (PathBuf::from(B), SubmoduleState::Uninitialized),
        ]
    );
}

#[test]
fn a_folder_that_is_not_a_repository_is_uninitialized() {
    let f = fixture();
    let clone = fresh_clone(&f);
    std::fs::write(clone.join(A).join("stray.txt"), "x").unwrap();
    // A `.git` file that leads nowhere.
    std::fs::write(clone.join(B).join(".git"), "gitdir: /nonexistent\n").unwrap();
    assert_eq!(
        states(&f.sb, &clone),
        [
            (PathBuf::from(A), SubmoduleState::Uninitialized),
            (PathBuf::from(B), SubmoduleState::Uninitialized),
        ]
    );
}

#[test]
fn sync_copies_the_new_url() {
    let f = fixture();
    let clone = fresh_clone(&f);
    update(&f.sb, &clone, &[]);
    let moved = f.sb.path().join("moved b.git");
    std::fs::rename(&f.b.0, &moved).unwrap();
    let moved_url = moved.to_str().unwrap();

    f.sb.run(
        &clone,
        &[
            "config",
            "--file",
            ".gitmodules",
            "submodule.lib.b.url",
            moved_url,
        ],
    );
    assert_eq!(list(&f.sb, &clone)[1].url.as_deref(), Some(moved_url));
    // Not copied until synced.
    assert_eq!(
        config(&f.sb, &clone, "submodule.lib.b.url"),
        f.b.0.to_str().unwrap()
    );

    sync_submodules(
        f.sb.git(),
        &clone,
        &[PathBuf::from(B)],
        false,
        &CancelToken::new(),
    )
    .unwrap();
    assert_eq!(config(&f.sb, &clone, "submodule.lib.b.url"), moved_url);
    assert_eq!(
        config(&f.sb, &clone.join(B), "remote.origin.url"),
        moved_url
    );
    // The other submodule is untouched.
    assert_eq!(
        config(&f.sb, &clone, "submodule.liba.url"),
        f.a.0.to_str().unwrap()
    );
    // And the new URL works.
    f.sb.run(&clone.join(B), &["fetch", "-q", "origin"]);
}

/// A superproject whose submodule `mid` has a submodule `inner` of its own (at inner's
/// third commit), and a fresh clone of it with nothing updated.
struct Nested {
    sb: Sandbox,
    inner: (PathBuf, [ObjectId; 3]),
    clone: PathBuf,
}

fn nested_fixture() -> Nested {
    let sb = sandbox();
    let inner = library(&sb, "inner");
    let mid_work = sb.init_repo("mid-work");
    sb.commit_file(&mid_work, "mid.txt", "mid\n", "mid");
    sb.run(
        &mid_work,
        &["submodule", "add", "-q", inner.0.to_str().unwrap(), "inner"],
    );
    sb.run(&mid_work, &["commit", "-q", "-m", "add inner"]);
    let mid = sb.path().join("mid.git");
    sb.run(
        sb.path(),
        &[
            "clone",
            "-q",
            "--bare",
            mid_work.to_str().unwrap(),
            mid.to_str().unwrap(),
        ],
    );
    let superproject = sb.init_repo("super");
    sb.commit_file(&superproject, "README", "super\n", "init");
    sb.run(
        &superproject,
        &["submodule", "add", "-q", mid.to_str().unwrap(), "mid"],
    );
    sb.run(&superproject, &["commit", "-q", "-m", "add mid"]);
    let clone = sb.path().join("clone");
    sb.run(
        sb.path(),
        &[
            "clone",
            "-q",
            superproject.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    Nested { sb, inner, clone }
}

/// `(path, superproject, depth, state)` of each entry of [`list_submodules_nested`].
fn nested_states(sb: &Sandbox, repo: &Path) -> Vec<(PathBuf, PathBuf, usize, SubmoduleState)> {
    list_submodules_nested(sb.git(), repo, &CancelToken::new())
        .unwrap()
        .into_iter()
        .map(|s| (s.path, s.superproject, s.depth, s.state))
        .collect()
}

#[test]
fn recursive_update_and_sync_reach_nested_submodules() {
    let Nested { sb, inner, clone } = nested_fixture();
    let (inner_url, inner_ids) = (inner.0.to_str().unwrap(), inner.1);
    let nested = clone.join("mid");
    let cancel = CancelToken::new();

    // Not recursive: `mid` is checked out, its own submodule isn't.
    update_submodules(sb.git(), &clone, &[], false, &cancel).unwrap();
    assert_eq!(
        states(&sb, &clone),
        [(PathBuf::from("mid"), SubmoduleState::UpToDate)]
    );
    assert_eq!(
        states(&sb, &nested),
        [(PathBuf::from("inner"), SubmoduleState::Uninitialized)]
    );

    update_submodules(sb.git(), &clone, &[], true, &cancel).unwrap();
    let listed = list(&sb, &nested);
    assert_eq!(listed[0].state, SubmoduleState::UpToDate);
    assert_eq!(listed[0].checked_out, Some(inner_ids[2]));
    assert_eq!(
        std::fs::read_to_string(nested.join("inner/lib.txt")).unwrap(),
        "3"
    );

    // A stale URL in the nested submodule's config is fixed only by a recursive sync.
    sb.run(&nested, &["config", "submodule.inner.url", "/stale"]);
    sync_submodules(sb.git(), &clone, &[], false, &cancel).unwrap();
    assert_eq!(config(&sb, &nested, "submodule.inner.url"), "/stale");
    sync_submodules(sb.git(), &clone, &[], true, &cancel).unwrap();
    assert_eq!(config(&sb, &nested, "submodule.inner.url"), inner_url);
}

#[test]
fn a_gitlink_without_gitmodules_entry_is_listed_by_path() {
    let sb = sandbox();
    let repo = sb.init_repo("r");
    let first = sb.commit_file(&repo, "a.txt", "1\n", "first");
    sb.run(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{first},orphan"),
        ],
    );
    assert_eq!(
        list(&sb, &repo),
        [SubmoduleInfo {
            path: "orphan".into(),
            name: "orphan".into(),
            url: None,
            recorded: Some(id(&first)),
            checked_out: None,
            superproject: PathBuf::new(),
            depth: 0,
            state: SubmoduleState::Uninitialized,
        }]
    );
    // And a repository without submodules has none.
    assert!(list(&sb, &sb.init_repo("empty")).is_empty());
}

#[test]
fn a_conflicting_submodule_entry() {
    let f = fixture();
    let (sb, repo) = (&f.sb, &f.superproject);
    let move_a_to = |commit: ObjectId, message: &str| {
        sb.run(
            &repo.join(A),
            &["checkout", "-q", "--detach", &commit.to_string()],
        );
        sb.run(repo, &["add", "--", A]);
        sb.run(repo, &["commit", "-q", "-m", message]);
    };
    sb.run(repo, &["branch", "other"]);
    move_a_to(f.a.1[0], "a to 1");
    sb.run(repo, &["checkout", "-q", "other"]);
    move_a_to(f.a.1[1], "a to 2");
    sb.run(repo, &["checkout", "-q", "main"]);
    sb.run(
        &repo.join(A),
        &["checkout", "-q", "--detach", &f.a.1[0].to_string()],
    );

    // Neither side descends from the base (commit 3), so git can't pick one.
    let err = sb
        .git()
        .command(repo)
        .args(["merge", "--no-edit", "other"])
        .run(&CancelToken::new())
        .unwrap_err();
    assert_eq!(err.kind, GitErrorKind::Conflict, "{err:?}");

    let listed = list(sb, repo);
    assert_eq!(
        listed[0],
        SubmoduleInfo {
            path: A.into(),
            name: A.into(),
            url: Some(f.a.0.to_str().unwrap().into()),
            recorded: None,
            checked_out: None,
            superproject: PathBuf::new(),
            depth: 0,
            state: SubmoduleState::Conflict,
        }
    );
    assert_eq!(listed[1].state, SubmoduleState::UpToDate);
}

/// A `file://` URL for `path`, with `/` between its parts as git stores it (on Windows
/// `file://C:/…`).
fn file_url(path: &Path) -> String {
    format!("file://{}", path.display()).replace('\\', "/")
}

/// What `git diff --cached --name-status` says is staged.
fn staged(sb: &Sandbox, repo: &Path) -> String {
    sb.run(repo, &["diff", "--cached", "--name-status"])
}

/// `git config` exit status for a key regexp in the repository's own config: 0 found, 1 not.
fn config_has(sb: &Sandbox, repo: &Path, regexp: &str) -> bool {
    let output = sb
        .git()
        .command(repo)
        .args(["config", "--local", "--get-regexp", regexp])
        .allow_exit_codes(&[1])
        .run(&CancelToken::new())
        .unwrap();
    output.status == 0
}

#[test]
fn add_clones_and_stages_the_submodule() {
    let sb = sandbox();
    let (bare, ids) = library(&sb, "lib");
    let superproject = sb.init_repo("super");
    sb.commit_file(&superproject, "README", "super\n", "init");
    let url = file_url(&bare);
    let cancel = CancelToken::new();

    let mut phases = Vec::new();
    add_submodule(
        sb.git(),
        &superproject,
        &url,
        Path::new("vendor/my lib"),
        None,
        &cancel,
        |event| {
            if let StderrEvent::Progress(p) = event {
                phases.push(p.phase);
            }
        },
    )
    .unwrap_or_else(|e| panic!("{e:?}"));
    assert!(
        phases.iter().any(|p| p == "Receiving objects"),
        "{phases:?}"
    );
    assert_eq!(
        list(&sb, &superproject),
        [SubmoduleInfo {
            path: "vendor/my lib".into(),
            name: "vendor/my lib".into(),
            url: Some(url.clone()),
            recorded: Some(ids[2]),
            checked_out: Some(ids[2]),
            superproject: PathBuf::new(),
            depth: 0,
            state: SubmoduleState::UpToDate,
        }]
    );
    assert_eq!(
        staged(&sb, &superproject),
        "A\t.gitmodules\nA\tvendor/my lib\n"
    );

    // Another branch than the remote's HEAD, recorded in .gitmodules.
    sb.run(&bare, &["branch", "stable", &ids[0].to_string()]);
    add_submodule(
        sb.git(),
        &superproject,
        &url,
        Path::new("stable"),
        Some("stable"),
        &cancel,
        |_| {},
    )
    .unwrap_or_else(|e| panic!("{e:?}"));
    let listed = list(&sb, &superproject);
    assert_eq!(listed[0].path, Path::new("stable"));
    assert_eq!(listed[0].checked_out, Some(ids[0]));
    assert_eq!(
        sb.run(
            &superproject,
            &["config", "--file", ".gitmodules", "submodule.stable.branch"]
        )
        .trim(),
        "stable"
    );
    sb.run(&superproject, &["commit", "-q", "-m", "add submodules"]);

    // A path that is taken.
    let err = add_submodule(
        sb.git(),
        &superproject,
        &url,
        Path::new("README"),
        None,
        &cancel,
        |_| {},
    )
    .unwrap_err();
    assert!(err.summary().contains("already exists"), "{err:?}");
}

#[test]
fn deinit_empties_the_folder_and_forgets_the_url() {
    let f = fixture();
    let (sb, repo) = (&f.sb, &f.superproject);
    deinit_submodule(sb.git(), repo, Path::new(A), false, &CancelToken::new()).unwrap();
    assert_eq!(
        states(sb, repo),
        [
            (PathBuf::from(A), SubmoduleState::Uninitialized),
            (PathBuf::from(B), SubmoduleState::UpToDate),
        ]
    );
    // The folder stays, empty; the URL leaves .git/config but not .gitmodules.
    assert_eq!(std::fs::read_dir(repo.join(A)).unwrap().count(), 0);
    assert!(!config_has(sb, repo, r"^submodule\.liba\."));
    assert!(config_has(sb, repo, r"^submodule\.lib\.b\."));
    assert_eq!(
        list(sb, repo)[0].url.as_deref(),
        Some(f.a.0.to_str().unwrap())
    );
    assert_eq!(staged(sb, repo), "");

    // Deinitialising again is fine; updating brings it back.
    deinit_submodule(sb.git(), repo, Path::new(A), false, &CancelToken::new()).unwrap();
    update(sb, repo, &[A]);
    assert_eq!(list(sb, repo)[0].state, SubmoduleState::UpToDate);
}

#[test]
fn deinit_refuses_local_changes_unless_forced() {
    let f = fixture();
    let (sb, repo) = (&f.sb, &f.superproject);
    let cancel = CancelToken::new();
    // Merely an untracked file in one, a modified file in the other.
    std::fs::write(repo.join(A).join("scratch.txt"), "mine\n").unwrap();
    std::fs::write(repo.join(B).join("lib.txt"), "changed\n").unwrap();
    for path in [A, B] {
        let err = deinit_submodule(sb.git(), repo, Path::new(path), false, &cancel).unwrap_err();
        assert_eq!(err.kind, GitErrorKind::SubmoduleModified, "{err:?}");
    }
    assert_eq!(
        states(sb, repo),
        [
            (PathBuf::from(A), SubmoduleState::UpToDate),
            (PathBuf::from(B), SubmoduleState::UpToDate),
        ]
    );
    assert_eq!(
        std::fs::read_to_string(repo.join(B).join("lib.txt")).unwrap(),
        "changed\n"
    );

    deinit_submodule(sb.git(), repo, Path::new(B), true, &cancel).unwrap();
    assert_eq!(list(sb, repo)[1].state, SubmoduleState::Uninitialized);
    assert_eq!(std::fs::read_dir(repo.join(B)).unwrap().count(), 0);
    assert!(repo.join(A).join("scratch.txt").exists());
}

#[test]
fn remove_deinits_and_stages_the_removal() {
    let f = fixture();
    let (sb, repo) = (&f.sb, &f.superproject);
    let cancel = CancelToken::new();

    std::fs::write(repo.join(A).join("scratch.txt"), "mine\n").unwrap();
    let err = remove_submodule(sb.git(), repo, Path::new(A), false, &cancel).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::SubmoduleModified, "{err:?}");
    assert_eq!(list(sb, repo).len(), 2);
    assert_eq!(staged(sb, repo), "");

    remove_submodule(sb.git(), repo, Path::new(A), true, &cancel).unwrap();
    assert_eq!(
        states(sb, repo),
        [(PathBuf::from(B), SubmoduleState::UpToDate)]
    );
    assert!(!repo.join(A).exists());
    assert_eq!(staged(sb, repo), "M\t.gitmodules\nD\tliba\n");
    assert!(!config_has(sb, repo, r"^submodule\.liba\."));
    let modules = sb.run(repo, &["config", "--file", ".gitmodules", "--list"]);
    assert!(!modules.contains("liba"), "{modules}");
    // Git keeps the submodule's repository.
    assert!(repo.join(".git/modules/liba").is_dir());
    sb.run(repo, &["commit", "-q", "-m", "remove liba"]);
    assert_eq!(sb.run(repo, &["status", "--porcelain"]), "");
}

#[test]
fn nested_listing_follows_set_up_submodules() {
    let Nested { sb, inner, clone } = nested_fixture();
    let cancel = CancelToken::new();
    let mid = || (PathBuf::from("mid"), PathBuf::new(), 0);

    // Nothing set up: `mid` has no repository to look into.
    assert_eq!(
        nested_states(&sb, &clone),
        [(mid().0, mid().1, mid().2, SubmoduleState::Uninitialized)]
    );

    // `mid` set up, its own submodule not: listed, uninitialised.
    update_submodules(sb.git(), &clone, &[], false, &cancel).unwrap();
    assert_eq!(
        nested_states(&sb, &clone),
        [
            (mid().0, mid().1, mid().2, SubmoduleState::UpToDate),
            (
                PathBuf::from("mid/inner"),
                PathBuf::from("mid"),
                1,
                SubmoduleState::Uninitialized
            ),
        ]
    );

    update_submodules(sb.git(), &clone, &[], true, &cancel).unwrap();
    let listed = list_submodules_nested(sb.git(), &clone, &cancel).unwrap();
    assert_eq!(listed.len(), 2, "{listed:?}");
    assert_eq!(
        (&listed[0].path, &listed[0].superproject, listed[0].depth),
        (&PathBuf::from("mid"), &PathBuf::new(), 0)
    );
    assert_eq!(listed[0].state, SubmoduleState::UpToDate);
    assert_eq!(
        listed[1],
        SubmoduleInfo {
            path: "mid/inner".into(),
            superproject: "mid".into(),
            depth: 1,
            name: "inner".into(),
            url: Some(inner.0.to_str().unwrap().into()),
            recorded: Some(inner.1[2]),
            checked_out: Some(inner.1[2]),
            state: SubmoduleState::UpToDate,
        }
    );
    // The plain listing stays one level deep.
    assert_eq!(list(&sb, &clone).len(), 1);
}

#[test]
fn update_with_events_reports_the_clone() {
    let f = fixture();
    let clone = fresh_clone(&f);
    let mut events = Vec::new();
    update_submodules_with_events(
        f.sb.git(),
        &clone,
        &[PathBuf::from(A)],
        false,
        &CancelToken::new(),
        |event| events.push(event),
    )
    .unwrap_or_else(|e| panic!("{e:?}"));
    assert!(
        events.iter().any(|event| match event {
            StderrEvent::Line(line) => line.contains("Cloning into") && line.contains(A),
            StderrEvent::Progress(_) => false,
        }),
        "{events:?}"
    );
    assert_eq!(
        states(&f.sb, &clone),
        [
            (PathBuf::from(A), SubmoduleState::UpToDate),
            (PathBuf::from(B), SubmoduleState::Uninitialized),
        ]
    );
}
