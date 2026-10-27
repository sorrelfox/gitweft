//! Git LFS against a local LFS store: git-lfs 3.x serves `file://` remotes itself (its
//! standalone file transfer adapter keeps objects in `<remote>/lfs/objects`), so nothing
//! touches the network. Skipped, with a note, where git-lfs isn't installed.

use std::path::{Path, PathBuf};

use gitweft_git::gix::bstr::BString;
use gitweft_git::lfs::{
    LfsPointer, LfsStatus, LfsTypeSize, MigrateDirection, lfs_fetch, lfs_fetch_all, lfs_files,
    lfs_install_local, lfs_lock, lfs_locks, lfs_migrate, lfs_migrate_info, lfs_prunable, lfs_prune,
    lfs_pull, lfs_status, lfs_unlock, local_object, migrate_patterns, parse_pointer, pointer_sides,
};
use gitweft_git::read::{DiffOptions, DiffSource, file_diff};
use gitweft_git::{CancelToken, GitErrorKind, StderrEvent};
use gitweft_testkit::Sandbox;

const HELLO: &str = "hello lfs\n";

fn has_git_lfs(sb: &Sandbox) -> bool {
    let found = sb
        .git()
        .command(sb.path())
        .args(["lfs", "version"])
        .run(&CancelToken::new())
        .is_ok();
    if !found {
        eprintln!("note: git-lfs is not installed; skipping this test");
    }
    found
}

/// The sandbox's global config is empty, so each repository gets the filter itself.
fn install(sb: &Sandbox, repo: &Path) {
    sb.run(repo, &["lfs", "install", "--local"]);
}

/// `seed` with two LFS files (`*.bin`) pushed to a bare `remote.git`, whose LFS store is
/// `remote.git/lfs/objects`. Returns the seed and the remote's `file://` URL.
fn seed_and_remote(sb: &Sandbox) -> (PathBuf, String) {
    let seed = sb.init_repo("seed");
    install(sb, &seed);
    sb.run(&seed, &["lfs", "track", "*.bin"]);
    std::fs::write(seed.join("a.bin"), HELLO).unwrap();
    std::fs::write(seed.join("b.bin"), "second\n").unwrap();
    sb.run(&seed, &["add", ".gitattributes", "a.bin", "b.bin"]);
    sb.run(&seed, &["commit", "-q", "-m", "lfs files"]);
    sb.run(
        sb.path(),
        &["init", "-q", "--bare", "-b", "main", "remote.git"],
    );
    let url = format!("file://{}", sb.path().join("remote.git").display());
    sb.run(&seed, &["remote", "add", "origin", &url]);
    // The pre-push hook uploads the objects.
    sb.run(&seed, &["push", "-q", "origin", "main"]);
    (seed, url)
}

/// A clone that checked out the pointers (the sandbox has no global LFS filter; skipping
/// the smudge makes that explicit), then got the filter.
fn pointer_clone(sb: &Sandbox, url: &str, name: &str, with_filter: bool) -> PathBuf {
    sb.git()
        .command(sb.path())
        .args(["clone", "-q", url, name])
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .run(&CancelToken::new())
        .unwrap();
    let repo = sb.path().join(name);
    if with_filter {
        install(sb, &repo);
    }
    repo
}

fn pointer_in(repo: &Path, file: &str) -> Option<LfsPointer> {
    parse_pointer(&std::fs::read(repo.join(file)).unwrap())
}

#[test]
fn status_tells_installed_configured_and_used_apart() {
    let sb = Sandbox::new();
    if !has_git_lfs(&sb) {
        return;
    }
    let cancel = CancelToken::new();
    let (seed, _) = seed_and_remote(&sb);
    let status = lfs_status(sb.git(), &seed, &cancel).unwrap();
    assert!(
        status
            .version
            .as_deref()
            .is_some_and(|v| v.starts_with('3')),
        "{status:?}"
    );
    assert!(status.filter_configured && status.used, "{status:?}");

    let plain = sb.init_repo("plain");
    sb.commit_file(&plain, "a.txt", "a\n", "plain");
    let status = lfs_status(sb.git(), &plain, &cancel).unwrap();
    assert!(!status.filter_configured && !status.used, "{status:?}");
    install(&sb, &plain);
    let status = lfs_status(sb.git(), &plain, &cancel).unwrap();
    assert!(status.filter_configured && !status.used, "{status:?}");
}

/// Runs whether or not git-lfs is installed: an empty `PATH` hides it from git (the
/// runner starts git by its full path, and git-lfs isn't in git's exec path).
#[test]
fn without_git_lfs_status_says_so_and_other_calls_fail() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    std::fs::write(
        repo.join(".gitattributes"),
        "*.bin filter=lfs diff=lfs merge=lfs -text\n",
    )
    .unwrap();
    std::fs::write(repo.join("a.bin"), "not a pointer\n").unwrap();
    sb.run(&repo, &["add", "."]);
    sb.run(&repo, &["commit", "-q", "-m", "lfs attributes"]);
    let empty = sb.path().join("empty-path");
    std::fs::create_dir(&empty).unwrap();
    let mut git = sb.git().clone();
    git.set_env("PATH", &empty);
    let cancel = CancelToken::new();

    assert_eq!(
        lfs_status(&git, &repo, &cancel).unwrap(),
        LfsStatus {
            version: None,
            filter_configured: false,
            used: true,
        }
    );
    let err = lfs_files(&git, &repo, &cancel).unwrap_err();
    assert!(err.stderr.contains("is not a git command"), "{err:?}");
}

#[test]
fn pull_downloads_and_checks_out_with_progress() {
    let sb = Sandbox::new();
    if !has_git_lfs(&sb) {
        return;
    }
    let cancel = CancelToken::new();
    let (_, url) = seed_and_remote(&sb);
    let work = pointer_clone(&sb, &url, "work", true);
    let pointer = pointer_in(&work, "a.bin").expect("a pointer is checked out");
    assert_eq!(pointer.size, HELLO.len() as u64);

    let files = lfs_files(sb.git(), &work, &cancel).unwrap();
    let names: Vec<_> = files.iter().map(|file| file.path.clone()).collect();
    assert_eq!(names, [BString::from("a.bin"), BString::from("b.bin")]);
    assert!(
        files
            .iter()
            .all(|file| !file.downloaded && !file.checked_out)
    );
    assert_eq!(
        (files[0].oid.as_str(), files[0].size),
        (pointer.oid.as_str(), 10)
    );

    let mut progress = Vec::new();
    lfs_pull(sb.git(), &work, &cancel, |event| {
        if let StderrEvent::Progress(p) = event {
            progress.push(p);
        }
    })
    .unwrap();
    assert_eq!(std::fs::read_to_string(work.join("a.bin")).unwrap(), HELLO);
    assert!(
        progress
            .iter()
            .any(|p| p.phase == "Downloading LFS objects" && p.percent.is_some()),
        "{progress:?}"
    );
    let files = lfs_files(sb.git(), &work, &cancel).unwrap();
    assert!(files.iter().all(|file| file.downloaded && file.checked_out));
    assert_eq!(sb.run(&work, &["status", "--porcelain"]), "");
}

#[test]
fn fetch_fills_the_store_but_leaves_the_work_tree() {
    let sb = Sandbox::new();
    if !has_git_lfs(&sb) {
        return;
    }
    let cancel = CancelToken::new();
    let (_, url) = seed_and_remote(&sb);
    let work = pointer_clone(&sb, &url, "work", true);
    let oid = pointer_in(&work, "a.bin").unwrap().oid;
    lfs_fetch(sb.git(), &work, &cancel, |_| {}).unwrap();

    let object = work
        .join(".git/lfs/objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(&oid);
    assert_eq!(std::fs::read_to_string(object).unwrap(), HELLO);
    assert!(pointer_in(&work, "a.bin").is_some(), "still the pointer");
    let files = lfs_files(sb.git(), &work, &cancel).unwrap();
    assert!(
        files
            .iter()
            .all(|file| file.downloaded && !file.checked_out)
    );
}

/// Whether `repo`'s local LFS store has `oid`.
fn stored(repo: &Path, oid: &str) -> bool {
    repo.join(".git/lfs/objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(oid)
        .exists()
}

/// Commits `content` as `a.bin` in `repo`; returns its LFS oid.
fn commit_a(sb: &Sandbox, repo: &Path, content: &str) -> String {
    std::fs::write(repo.join("a.bin"), content).unwrap();
    sb.run(repo, &["add", "a.bin"]);
    sb.run(repo, &["commit", "-q", "-m", content.trim()]);
    let pointer = sb.run(repo, &["cat-file", "blob", "HEAD:a.bin"]);
    parse_pointer(pointer.as_bytes()).unwrap().oid
}

#[test]
fn fetch_all_gets_older_versions_too() {
    let sb = Sandbox::new();
    if !has_git_lfs(&sb) {
        return;
    }
    let cancel = CancelToken::new();
    let (seed, url) = seed_and_remote(&sb);
    let old = parse_pointer(
        sb.run(&seed, &["cat-file", "blob", "HEAD:a.bin"])
            .as_bytes(),
    )
    .unwrap()
    .oid;
    let new = commit_a(&sb, &seed, "second version\n");
    sb.run(&seed, &["push", "-q", "origin", "main"]);

    let work = pointer_clone(&sb, &url, "work", true);
    lfs_fetch(sb.git(), &work, &cancel, |_| {}).unwrap();
    assert!(
        stored(&work, &new) && !stored(&work, &old),
        "fetch: the checkout's only"
    );
    let mut progress = false;
    lfs_fetch_all(sb.git(), &work, &cancel, |event| {
        progress |= matches!(event, StderrEvent::Progress(_));
    })
    .unwrap();
    assert!(
        stored(&work, &old),
        "fetch --all: the older version as well"
    );
    assert!(progress);
    assert!(
        pointer_in(&work, "a.bin").is_some(),
        "the work tree is unchanged"
    );
}

#[test]
fn prune_deletes_old_pushed_copies_and_keeps_unpushed_ones() {
    let sb = Sandbox::new();
    if !has_git_lfs(&sb) {
        return;
    }
    let cancel = CancelToken::new();
    // v1 (HELLO) pushed by seed_and_remote, v2 pushed, v3 committed only here, v4 checked
    // out. The sandbox's commits are dated 2026-01-01, long past git-lfs's "recent".
    let (seed, _) = seed_and_remote(&sb);
    // The seed has the filter: its a.bin is the content, the pointer is in the commit.
    let v1 = parse_pointer(
        sb.run(&seed, &["cat-file", "blob", "HEAD:a.bin"])
            .as_bytes(),
    )
    .unwrap()
    .oid;
    let v2 = commit_a(&sb, &seed, "v2 pushed\n");
    sb.run(&seed, &["push", "-q", "origin", "main"]);
    let v3 = commit_a(&sb, &seed, "v3 not pushed\n");
    let v4 = commit_a(&sb, &seed, "v4 checked out\n");

    let prunable = lfs_prunable(sb.git(), &seed, &cancel).unwrap();
    let mut oids = prunable.oids.clone();
    oids.sort();
    let mut want = vec![v1.clone(), v2.clone()];
    want.sort();
    assert_eq!(oids, want);
    assert_eq!(prunable.bytes, (HELLO.len() + "v2 pushed\n".len()) as u64);
    assert!(stored(&seed, &v1), "a dry run deletes nothing");

    let pruned = lfs_prune(sb.git(), &seed, &cancel).unwrap();
    assert_eq!(pruned.bytes, prunable.bytes);
    assert_eq!(pruned.oids.len(), 2);
    assert!(!stored(&seed, &v1) && !stored(&seed, &v2));
    assert!(stored(&seed, &v3), "an unpushed commit's copy stays");
    assert!(stored(&seed, &v4), "the checkout's copy stays");
    assert_eq!(
        lfs_prunable(sb.git(), &seed, &cancel).unwrap(),
        Default::default()
    );
}

#[test]
fn pull_without_the_filter_or_the_objects_fails() {
    let sb = Sandbox::new();
    if !has_git_lfs(&sb) {
        return;
    }
    let cancel = CancelToken::new();
    let (_, url) = seed_and_remote(&sb);

    // git-lfs downloads, skips the checkout and exits 0; that must not look like success.
    let unfiltered = pointer_clone(&sb, &url, "unfiltered", false);
    let err = lfs_pull(sb.git(), &unfiltered, &cancel, |_| {}).unwrap_err();
    assert!(err.summary().contains("Git LFS is not installed"), "{err}");
    assert!(pointer_in(&unfiltered, "a.bin").is_some());

    // The store lost its objects: exit 2, one line per missing object.
    std::fs::rename(sb.path().join("remote.git/lfs"), sb.path().join("lfs-gone")).unwrap();
    let work = pointer_clone(&sb, &url, "work", true);
    let err = lfs_pull(sb.git(), &work, &cancel, |_| {}).unwrap_err();
    assert_eq!(err.exit_code, Some(2), "{}", err.stderr);
    assert!(
        err.stderr.contains("remote missing object"),
        "{}",
        err.stderr
    );
    assert!(
        err.summary().contains("Failed to fetch some objects"),
        "{err}"
    );
}

#[test]
fn pointer_sides_of_real_diffs() {
    let sb = Sandbox::new();
    if !has_git_lfs(&sb) {
        return;
    }
    let cancel = CancelToken::new();
    let repo = sb.init_repo("r");
    install(&sb, &repo);
    sb.run(&repo, &["lfs", "track", "*.bin"]);
    sb.run(&repo, &["add", ".gitattributes"]);
    let first = sb.commit_file(&repo, "a.bin", "one\n", "one");
    let second = sb.commit_file(&repo, "a.bin", "two\n", "two");
    let sides = |source: DiffSource, path: &str| {
        let diff = file_diff(
            sb.git(),
            &repo,
            &source,
            &BString::from(path),
            None,
            DiffOptions::default(),
            &cancel,
        )
        .unwrap();
        pointer_sides(&diff)
    };
    let size = |pointer: Option<LfsPointer>| pointer.map(|p| p.size);

    // Between commits.
    let (old, new) = sides(
        DiffSource::Commit {
            parent: Some(first.parse().unwrap()),
            commit: second.parse().unwrap(),
        },
        "a.bin",
    )
    .unwrap();
    assert_eq!((size(old), size(new)), (Some(4), Some(4)));
    // Nothing staged: no hunks, no answer.
    assert_eq!(sides(DiffSource::Staged, "a.bin"), None);

    // Unstaged: the clean filter turns the work-tree file into a pointer for the diff.
    std::fs::write(repo.join("a.bin"), "three!\n").unwrap();
    let (old, new) = sides(DiffSource::Unstaged, "a.bin").unwrap();
    assert_eq!((size(old), size(new)), (Some(4), Some(7)));
    sb.run(&repo, &["add", "a.bin"]);
    let (old, new) = sides(DiffSource::Staged, "a.bin").unwrap();
    assert_eq!((size(old), size(new)), (Some(4), Some(7)));
    sb.run(&repo, &["commit", "-q", "-m", "three"]);

    // Added and deleted.
    std::fs::write(repo.join("n.bin"), "new\n").unwrap();
    sb.run(&repo, &["add", "n.bin"]);
    let (old, new) = sides(DiffSource::Staged, "n.bin").unwrap();
    assert_eq!((size(old), size(new)), (None, Some(4)));
    sb.run(&repo, &["commit", "-q", "-m", "n"]);
    sb.run(&repo, &["rm", "-q", "a.bin"]);
    let (old, new) = sides(DiffSource::Staged, "a.bin").unwrap();
    assert_eq!((size(old), size(new)), (Some(7), None));

    // A text file that starts like a pointer isn't one.
    std::fs::write(
        repo.join("p.txt"),
        "version https://git-lfs.github.com/spec/v1\nhello\n",
    )
    .unwrap();
    sb.run(&repo, &["add", "p.txt"]);
    assert_eq!(sides(DiffSource::Staged, "p.txt"), None);
}

/// The LFS image preview reads a pointer's content from the local store only: the
/// repository that committed it has it; a clone that skipped the download doesn't, and
/// nothing is fetched to get it.
#[test]
fn local_object_reads_the_store_and_never_fetches() {
    let sb = Sandbox::new();
    if !has_git_lfs(&sb) {
        return;
    }
    let cancel = CancelToken::new();
    let (seed, url) = seed_and_remote(&sb);
    let pointer = pointer_in(&pointer_clone(&sb, &url, "p", false), "a.bin").unwrap();
    assert_eq!(
        local_object(sb.git(), &seed, &pointer, 1 << 20, &cancel).unwrap(),
        Some(HELLO.as_bytes().to_vec())
    );
    // Over the size limit: not read.
    assert_eq!(
        local_object(sb.git(), &seed, &pointer, 3, &cancel).unwrap(),
        None
    );
    // A clone without the objects: none, and its store stays empty (nothing fetched).
    let clone = pointer_clone(&sb, &url, "c", true);
    assert_eq!(
        local_object(sb.git(), &clone, &pointer, 1 << 20, &cancel).unwrap(),
        None
    );
    assert!(
        !clone
            .join(".git/lfs/objects")
            .join(&pointer.oid[..2])
            .exists()
    );
}

#[test]
fn install_local_sets_up_this_repository_only() {
    let sb = Sandbox::new();
    if !has_git_lfs(&sb) {
        return;
    }
    let cancel = CancelToken::new();
    let configured = |repo: &Path| {
        lfs_status(sb.git(), repo, &cancel)
            .unwrap()
            .filter_configured
    };
    let repo = sb.init_repo("r");
    let other = sb.init_repo("other");
    assert!(!configured(&repo));

    lfs_install_local(sb.git(), &repo, &cancel).unwrap();
    assert!(configured(&repo));
    assert!(repo.join(".git/hooks/pre-push").is_file());
    assert!(!configured(&other));
    // Again is fine.
    lfs_install_local(sb.git(), &repo, &cancel).unwrap();

    // A hook of the repository's own stays: git-lfs fails, but has set the filter.
    let hooked = sb.init_repo("hooked");
    let hook = hooked.join(".git/hooks/pre-push");
    std::fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();
    let err = lfs_install_local(sb.git(), &hooked, &cancel).unwrap_err();
    assert_eq!(err.exit_code, Some(2), "{err:?}");
    assert!(
        err.stderr.contains("Hook already exists: pre-push"),
        "{err:?}"
    );
    assert!(configured(&hooked));
    assert_eq!(
        std::fs::read_to_string(&hook).unwrap(),
        "#!/bin/sh\nexit 0\n"
    );
}

/// A `file://` LFS store has no locking API; nothing here reaches a network.
#[test]
fn locking_needs_a_server_with_a_locking_api() {
    let sb = Sandbox::new();
    if !has_git_lfs(&sb) {
        return;
    }
    let cancel = CancelToken::new();
    let (seed, _) = seed_and_remote(&sb);

    // git-lfs lists no locks, exit 0, with a hint on stderr: that must not read as "none".
    let err = lfs_locks(sb.git(), &seed, &cancel).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::LfsLockingUnsupported, "{err:?}");
    assert_eq!(
        err.summary(),
        "error: this LFS remote has no file-locking API"
    );
    assert!(
        err.stderr.contains("The remote resolves to a file:// URL"),
        "{err:?}"
    );

    let lock = lfs_lock(sb.git(), &seed, b"a.bin", &cancel).unwrap_err();
    assert!(lock.stderr.contains("missing protocol"), "{lock:?}");
    let unlock = lfs_unlock(sb.git(), &seed, b"a.bin", false, &cancel).unwrap_err();
    let forced = lfs_unlock(sb.git(), &seed, b"a.bin", true, &cancel).unwrap_err();
    for err in [lock, unlock, forced] {
        assert_eq!(err.kind, GitErrorKind::LfsLockingUnsupported, "{err:?}");
        assert_eq!(err.exit_code, Some(2), "{err:?}");
    }

    // Without a remote there is no server to ask: none.
    let plain = sb.init_repo("plain");
    install(&sb, &plain);
    assert_eq!(lfs_locks(sb.git(), &plain, &cancel).unwrap(), []);
}

/// A git-lfs file-locking API for these tests, on 127.0.0.1 only (nothing leaves the
/// machine): locks live in memory, owned by "Tester" unless seeded otherwise. With
/// `not_found`, every request is answered 404, as by a server without the API. Returns the
/// LFS URL to set as `lfs.url`.
fn lock_server(not_found: bool, seeded: &[(&str, &str)]) -> String {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::{Arc, Mutex};

    type Locks = Arc<Mutex<Vec<(String, String, String)>>>; // id, path, owner
    let locks: Locks = Arc::new(Mutex::new(
        seeded
            .iter()
            .enumerate()
            .map(|(n, (path, owner))| {
                (
                    (100 + n).to_string(),
                    (*path).to_owned(),
                    (*owner).to_owned(),
                )
            })
            .collect(),
    ));
    // The loopback interface only: a stand-in for an LFS server in tests.
    #[allow(clippy::disallowed_methods)]
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let json = |lock: &(String, String, String)| {
        serde_json::json!({
            "id": lock.0, "path": lock.1, "owner": {"name": lock.2},
            "locked_at": "2026-09-13T10:00:00Z"
        })
    };
    let handle = move |method: &str, target: &str, body: &serde_json::Value, locks: &Locks| {
        let mut locks = locks.lock().unwrap();
        let route = target.split('?').next().unwrap_or("");
        let route = route.strip_prefix("/repo.git/info/lfs").unwrap_or(route);
        let query = target.split_once('?').map_or("", |(_, q)| q);
        match (method, route) {
            _ if not_found => (404, serde_json::json!({"message": "Not Found"})),
            ("GET", "/locks") => {
                let path = query
                    .split('&')
                    .find_map(|pair| pair.strip_prefix("path="))
                    .map(|p| p.replace("%2F", "/"));
                let found: Vec<_> = locks
                    .iter()
                    .filter(|lock| path.as_ref().is_none_or(|p| *p == lock.1))
                    .map(json)
                    .collect();
                (200, serde_json::json!({"locks": found, "next_cursor": ""}))
            }
            ("POST", "/locks/verify") => {
                let (ours, theirs): (Vec<_>, Vec<_>) =
                    locks.iter().partition(|lock| lock.2 == "Tester");
                let ours: Vec<_> = ours.into_iter().map(json).collect();
                let theirs: Vec<_> = theirs.into_iter().map(json).collect();
                (
                    200,
                    serde_json::json!({"ours": ours, "theirs": theirs, "next_cursor": ""}),
                )
            }
            ("POST", "/locks") => {
                let path = body["path"].as_str().unwrap_or_default().to_owned();
                if let Some(lock) = locks.iter().find(|lock| lock.1 == path) {
                    let lock = json(lock);
                    return (
                        409,
                        serde_json::json!({"lock": lock, "message": "already created lock"}),
                    );
                }
                let lock = ((locks.len() + 1).to_string(), path, "Tester".to_owned());
                let reply = serde_json::json!({"lock": json(&lock)});
                locks.push(lock);
                (201, reply)
            }
            ("POST", route) if route.starts_with("/locks/") && route.ends_with("/unlock") => {
                let id = &route["/locks/".len()..route.len() - "/unlock".len()];
                let force = body["force"].as_bool().unwrap_or(false);
                match locks.iter().position(|lock| lock.0 == id) {
                    None => (404, serde_json::json!({"message": "no such lock"})),
                    Some(ix) if locks[ix].2 != "Tester" && !force => {
                        let owner = locks[ix].2.clone();
                        (
                            403,
                            serde_json::json!({"message": format!("lock is owned by {owner}")}),
                        )
                    }
                    Some(ix) => {
                        let lock = locks.remove(ix);
                        (200, serde_json::json!({"lock": json(&lock)}))
                    }
                }
            }
            _ => (404, serde_json::json!({"message": "Not Found"})),
        }
    };
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let locks = locks.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut stream = stream;
                loop {
                    let mut request = String::new();
                    if reader.read_line(&mut request).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut parts = request.split_whitespace();
                    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                    let mut length = 0;
                    loop {
                        let mut header = String::new();
                        reader.read_line(&mut header).unwrap();
                        if header.trim().is_empty() {
                            break;
                        }
                        if let Some((name, value)) = header.split_once(':')
                            && name.eq_ignore_ascii_case("content-length")
                        {
                            length = value.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    let body = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                    let (status, reply) = handle(method, target, &body, &locks);
                    let reply = reply.to_string();
                    let head = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/vnd.git-lfs+json\r\nContent-Length: {}\r\n\r\n",
                        reply.len()
                    );
                    if stream
                        .write_all(head.as_bytes())
                        .and_then(|()| stream.write_all(reply.as_bytes()))
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
    });
    format!("http://127.0.0.1:{port}/repo.git/info/lfs")
}

#[test]
fn locks_against_a_lock_server() {
    let sb = Sandbox::new();
    if !has_git_lfs(&sb) {
        return;
    }
    let cancel = CancelToken::new();
    let (seed, _) = seed_and_remote(&sb);
    let url = lock_server(false, &[("b.bin", "Someone Else")]);
    sb.run(&seed, &["config", "lfs.url", &url]);

    let listed = lfs_locks(sb.git(), &seed, &cancel).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (listed[0].path.as_slice(), listed[0].owner.as_str()),
        (&b"b.bin"[..], "Someone Else")
    );
    assert_eq!(listed[0].locked_at, Some(1_789_293_600));

    // git-lfs 3.x writes the new lock as a list of one.
    let lock = lfs_lock(sb.git(), &seed, b"a.bin", &cancel).unwrap();
    assert_eq!(
        (lock.path.as_slice(), lock.owner.as_str()),
        (&b"a.bin"[..], "Tester")
    );
    let again = lfs_lock(sb.git(), &seed, b"a.bin", &cancel).unwrap_err();
    assert_eq!(again.kind, GitErrorKind::Other, "{again:?}");
    assert!(again.stderr.contains("already created lock"), "{again:?}");

    // Someone else's lock: refused, unless forced.
    let theirs = lfs_unlock(sb.git(), &seed, b"b.bin", false, &cancel).unwrap_err();
    assert!(
        theirs.stderr.contains("owned by Someone Else"),
        "{theirs:?}"
    );
    lfs_unlock(sb.git(), &seed, b"b.bin", true, &cancel).unwrap();
    lfs_unlock(sb.git(), &seed, b"a.bin", false, &cancel).unwrap();
    assert_eq!(lfs_locks(sb.git(), &seed, &cancel).unwrap(), []);
}

#[test]
fn a_server_without_the_locking_api() {
    let sb = Sandbox::new();
    if !has_git_lfs(&sb) {
        return;
    }
    let cancel = CancelToken::new();
    let (seed, _) = seed_and_remote(&sb);
    sb.run(&seed, &["config", "lfs.url", &lock_server(true, &[])]);
    // git-lfs lists none, exit 0, silently: its trace shows the 404.
    let listed = lfs_locks(sb.git(), &seed, &cancel).unwrap_err();
    assert_eq!(
        listed.kind,
        GitErrorKind::LfsLockingUnsupported,
        "{listed:?}"
    );
    assert!(
        listed
            .summary()
            .starts_with("error: this LFS remote has no file-locking API")
    );
    let lock = lfs_lock(sb.git(), &seed, b"a.bin", &cancel).unwrap_err();
    assert_eq!(lock.kind, GitErrorKind::LfsLockingUnsupported, "{lock:?}");
    let unlock = lfs_unlock(sb.git(), &seed, b"a.bin", true, &cancel).unwrap_err();
    assert_eq!(
        unlock.kind,
        GitErrorKind::LfsLockingUnsupported,
        "{unlock:?}"
    );
    // No trace file is left behind.
    let left = std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .flatten()
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.starts_with(&format!("gitweft-lfs-trace-{}-", std::process::id()))
        })
        .count();
    assert_eq!(left, 0);
}

/// `lfs migrate info`: the kinds of files in plain git, largest first, over this
/// branch or every branch; files already in LFS aren't counted. Nothing is rewritten.
#[test]
fn migrate_info_reports_the_largest_kinds_of_files() {
    let sb = Sandbox::new();
    if !has_git_lfs(&sb) {
        return;
    }
    let repo = sb.init_repo("r");
    install(&sb, &repo);
    sb.run(&repo, &["lfs", "track", "*.bin"]);
    std::fs::write(repo.join("in-lfs.bin"), "x".repeat(50_000)).unwrap();
    std::fs::write(repo.join("big.dat"), "d".repeat(20_000)).unwrap();
    std::fs::write(repo.join("small.txt"), "t".repeat(300)).unwrap();
    sb.run(&repo, &["add", "."]);
    sb.run(&repo, &["commit", "-qm", "files"]);
    std::fs::write(repo.join("big.dat"), "e".repeat(10_000)).unwrap();
    sb.run(&repo, &["commit", "-qam", "a second version"]);
    sb.run(&repo, &["switch", "-qc", "side"]);
    std::fs::write(repo.join("photo.png"), "p".repeat(40_000)).unwrap();
    sb.run(&repo, &["add", "photo.png"]);
    sb.run(&repo, &["commit", "-qm", "a picture"]);
    sb.run(&repo, &["switch", "-q", "main"]);
    let head = sb.run(&repo, &["rev-parse", "HEAD"]);
    let cancel = CancelToken::new();

    let branch = lfs_migrate_info(sb.git(), &repo, false, 10, &cancel).unwrap();
    let kinds: Vec<(&str, u64, u64)> = branch
        .iter()
        .map(
            |LfsTypeSize {
                 pattern,
                 bytes,
                 files,
             }| (pattern.as_str(), *bytes, *files),
        )
        .collect();
    // .gitattributes is a plain file too; the LFS file isn't counted.
    assert_eq!(kinds[0], ("*.dat", 30_000, 2), "{kinds:?}");
    assert!(
        kinds.iter().any(|k| k.0 == "*.txt" && k.1 == 300),
        "{kinds:?}"
    );
    assert!(
        !kinds.iter().any(|k| k.0 == "*.bin" || k.0 == "*.png"),
        "{kinds:?}"
    );

    let all = lfs_migrate_info(sb.git(), &repo, true, 10, &cancel).unwrap();
    assert_eq!(all[0].pattern, "*.png", "{all:?}");
    assert_eq!(all[0].bytes, 40_000);
    assert_eq!(
        lfs_migrate_info(sb.git(), &repo, true, 1, &cancel)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        sb.run(&repo, &["rev-parse", "HEAD"]),
        head,
        "nothing rewritten"
    );
}

/// `lfs migrate` on the checked-out branch only: into LFS and back out, the side
/// branch untouched, the file's bytes the same at the end.
#[test]
fn migrate_rewrites_the_branch_into_lfs_and_back() {
    let sb = Sandbox::new();
    if !has_git_lfs(&sb) {
        return;
    }
    let repo = sb.init_repo("r");
    install(&sb, &repo);
    let data: Vec<u8> = (0..4000u32).map(|n| (n * 7 % 251) as u8).collect();
    std::fs::write(repo.join("a.bin"), &data).unwrap();
    sb.run(&repo, &["add", "a.bin"]);
    sb.run(&repo, &["commit", "-qm", "data"]);
    sb.run(&repo, &["switch", "-qc", "side"]);
    std::fs::write(repo.join("s.bin"), "side\n").unwrap();
    sb.run(&repo, &["add", "s.bin"]);
    sb.run(&repo, &["commit", "-qm", "side"]);
    sb.run(&repo, &["switch", "-q", "main"]);
    let (main, side) = (
        sb.run(&repo, &["rev-parse", "main"]),
        sb.run(&repo, &["rev-parse", "side"]),
    );
    let cancel = CancelToken::new();
    let patterns = migrate_patterns(" *.bin,  *.psd  ");
    assert_eq!(patterns, ["*.bin", "*.psd"]);

    lfs_migrate(
        sb.git(),
        &repo,
        MigrateDirection::Import,
        &patterns,
        "refs/heads/main",
        &cancel,
    )
    .unwrap();
    assert_ne!(sb.run(&repo, &["rev-parse", "main"]), main, "rewritten");
    assert_eq!(
        sb.run(&repo, &["rev-parse", "side"]),
        side,
        "other branches left alone"
    );
    assert!(sb.run(&repo, &["lfs", "ls-files"]).contains("a.bin"));
    assert!(parse_pointer(sb.run(&repo, &["cat-file", "-p", "HEAD:a.bin"]).as_bytes()).is_some());

    lfs_migrate(
        sb.git(),
        &repo,
        MigrateDirection::Export,
        &patterns,
        "refs/heads/main",
        &cancel,
    )
    .unwrap();
    assert!(!sb.run(&repo, &["lfs", "ls-files"]).contains("a.bin"));
    assert_eq!(std::fs::read(repo.join("a.bin")).unwrap(), data);
    assert_eq!(sb.run(&repo, &["rev-parse", "side"]), side);
}
