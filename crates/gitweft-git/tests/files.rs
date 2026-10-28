use std::path::{Path, PathBuf};

use gitweft_git::files::{
    ArchiveFormat, IgnoreScope, add_ignore_rule, apply_mailbox, apply_patch, archive,
    clean_candidates, format_patch, format_patches, ignore_pattern_for_extension,
    ignore_pattern_for_path, unmerged_count, write_patch,
};
use gitweft_git::gix::bstr::BString;
use gitweft_git::merge::{Operation, OperationAction, operation_state, run_operation_action};
use gitweft_git::ops::discard_paths;
use gitweft_git::{CancelToken, GitErrorKind, ObjectId};
use gitweft_platform::bytes::path_from_bytes;
use gitweft_testkit::Sandbox;

fn c() -> CancelToken {
    CancelToken::new()
}

fn id(hex: &str) -> ObjectId {
    ObjectId::from_hex(hex.trim().as_bytes()).unwrap()
}

fn path(repo: &Path, name: &[u8]) -> PathBuf {
    repo.join(path_from_bytes(name))
}

/// Writes `content` to `name` (bytes, relative to `repo`), creating folders.
fn write(repo: &Path, name: &[u8], content: &str) {
    let file = path(repo, name);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, content).unwrap();
}

fn head(sb: &Sandbox, repo: &Path) -> ObjectId {
    id(&sb.run(repo, &["rev-parse", "HEAD"]))
}

fn am_in_progress(repo: &Path) -> bool {
    repo.join(".git/rebase-apply").is_dir()
}

#[test]
fn format_patch_then_apply_mailbox_reproduces_the_commit() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let base = id(&sb.commit_file(&repo, "a.txt", "1\n2\n3\n", "base"));
    sb.run(&repo, &["branch", "other"]);
    write(&repo, b"a.txt", "1\ntwo\n3\n");
    write(&repo, b"dir/new file.txt", "new\n");
    sb.run(&repo, &["add", "-A"]);
    let message = "Change two\n\nA body line.\n\nRefs: #1";
    sb.run(&repo, &["commit", "-q", "-m", message]);
    let change = head(&sb, &repo);

    let folder = sb.path().join("patches");
    let file = format_patch(sb.git(), &repo, &change, &folder, &c()).unwrap();
    assert_eq!(file, folder.join("0001-Change-two.patch"));
    assert!(file.is_file());
    // A relative folder is relative to the repository, and may look like an option.
    let relative = format_patch(sb.git(), &repo, &change, Path::new("-out"), &c()).unwrap();
    assert!(relative.starts_with(repo.join("-out")), "{relative:?}");
    assert!(relative.is_file());

    sb.run(&repo, &["checkout", "-q", "other"]);
    std::fs::rename(&file, repo.join("-x.patch")).unwrap();
    apply_mailbox(sb.git(), &repo, &[PathBuf::from("-x.patch")], &c()).unwrap();

    // Same parent, tree, author, message, and (fixed sandbox dates) committer: the very
    // same commit.
    let applied = head(&sb, &repo);
    assert_eq!(applied, change);
    let show = |rev: &str, format: &str| sb.run(&repo, &["show", "-s", format, rev]);
    for format in ["--format=%T", "--format=%B", "--format=%an <%ae> %ad"] {
        assert_eq!(
            show(&applied.to_string(), format),
            show(&change.to_string(), format),
            "{format}"
        );
    }
    assert_eq!(id(&show("HEAD^", "--format=%H")), base);
    assert!(!am_in_progress(&repo));
    // Nothing to apply: nothing happens.
    apply_mailbox(sb.git(), &repo, &[], &c()).unwrap();
    assert_eq!(head(&sb, &repo), applied);
}

#[test]
fn format_patch_refuses_a_merge_commit() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "root");
    sb.run(&repo, &["checkout", "-q", "-b", "side"]);
    sb.commit_file(&repo, "b.txt", "b\n", "side");
    sb.run(&repo, &["checkout", "-q", "main"]);
    sb.commit_file(&repo, "a.txt", "2\n", "main");
    sb.run(&repo, &["merge", "-q", "--no-edit", "side"]);

    let folder = sb.path().join("patches");
    let err = format_patch(sb.git(), &repo, &head(&sb, &repo), &folder, &c()).unwrap_err();
    assert!(err.summary().contains("merge commit"), "{}", err.summary());
    assert!(!folder.exists());

    // The root commit is fine (format-patch `-1`, not `<root>^!`, which means "since root").
    let root = id(&sb.run(&repo, &["rev-list", "--max-parents=0", "HEAD"]));
    let file = format_patch(sb.git(), &repo, &root, &folder, &c()).unwrap();
    assert_eq!(file, folder.join("0001-root.patch"));
    assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 1);
}

#[test]
fn format_patches_writes_numbered_files_oldest_first() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let root = id(&sb.commit_file(&repo, "a.txt", "1\n", "root"));
    sb.run(&repo, &["branch", "other"]);
    let one = id(&sb.commit_file(&repo, "b.txt", "b\n", "one"));
    sb.commit_file(&repo, "c.txt", "c\n", "two");
    let three = id(&sb.commit_file(&repo, "d.txt", "d\n", "three"));

    // Not a range: "two" is left out, and the root commit needs nothing special.
    let folder = sb.path().join("patches");
    let files = format_patches(sb.git(), &repo, &[root, one, three], &folder, &c()).unwrap();
    let names = ["0001-root.patch", "0002-one.patch", "0003-three.patch"];
    assert_eq!(files, names.map(|name| folder.join(name)));
    assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 3);

    // A merge among them: refused before anything is written.
    sb.run(&repo, &["checkout", "-q", "-b", "side", "other"]);
    sb.commit_file(&repo, "s.txt", "s\n", "side");
    sb.run(&repo, &["checkout", "-q", "main"]);
    sb.run(&repo, &["merge", "-q", "--no-edit", "side"]);
    let merge = head(&sb, &repo);
    let refused = sb.path().join("refused");
    let err = format_patches(sb.git(), &repo, &[one, merge], &refused, &c()).unwrap_err();
    assert!(err.summary().contains("merge commit"), "{}", err.summary());
    assert!(!refused.exists());

    // The ones after the root apply in order onto a branch that has only the root.
    sb.run(&repo, &["checkout", "-q", "other"]);
    apply_mailbox(sb.git(), &repo, &files[1..], &c()).unwrap();
    assert_eq!(sb.run(&repo, &["log", "--format=%s"]), "three\none\nroot\n");
}

#[test]
fn write_patch_saves_one_mailbox_where_asked() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "base");
    sb.run(&repo, &["branch", "other"]);
    let change = id(&sb.commit_file(&repo, "a.txt", "2\n", "change"));

    let file = sb.path().join("r-change.patch");
    write_patch(sb.git(), &repo, &change, &file, &c()).unwrap();
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.starts_with(&format!("From {change} ")), "{text}");
    assert!(text.contains("Subject: [PATCH] change"), "{text}");

    // It's a mailbox: `git am` makes the very same commit of it.
    sb.run(&repo, &["checkout", "-q", "other"]);
    apply_mailbox(sb.git(), &repo, &[file], &c()).unwrap();
    assert_eq!(head(&sb, &repo), change);
}

#[test]
fn a_conflicting_mailbox_stops_with_am_in_progress() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "base");
    sb.run(&repo, &["branch", "other"]);
    let two = id(&sb.commit_file(&repo, "a.txt", "2\n", "two"));
    let file = format_patch(sb.git(), &repo, &two, &sb.path().join("p"), &c()).unwrap();
    sb.run(&repo, &["checkout", "-q", "other"]);
    let three = id(&sb.commit_file(&repo, "a.txt", "3\n", "three"));

    let err = apply_mailbox(sb.git(), &repo, std::slice::from_ref(&file), &c()).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::Conflict, "{err:?}");
    assert!(am_in_progress(&repo));
    let gix_repo = gitweft_git::gix::open(&repo).unwrap();
    assert_eq!(operation_state(&gix_repo), Some(Operation::ApplyMailbox));
    assert!(
        sb.run(&repo, &["status", "--porcelain"])
            .contains("UU a.txt")
    );

    run_operation_action(
        sb.git(),
        &repo,
        Operation::ApplyMailbox,
        OperationAction::Abort,
        &c(),
    )
    .unwrap();
    assert!(!am_in_progress(&repo));
    assert_eq!(head(&sb, &repo), three);
}

#[test]
fn staged_changes_are_refused_before_am_starts() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n", "base");
    sb.run(&repo, &["branch", "other"]);
    let two = id(&sb.commit_file(&repo, "b.txt", "b\n", "two"));
    let file = format_patch(sb.git(), &repo, &two, &sb.path().join("p"), &c()).unwrap();
    sb.run(&repo, &["checkout", "-q", "other"]);
    write(&repo, b"a.txt", "staged\n");
    sb.run(&repo, &["add", "a.txt"]);
    let before = head(&sb, &repo);

    // git itself would stop with "Dirty index" yet leave an am in progress behind.
    let err = apply_mailbox(sb.git(), &repo, std::slice::from_ref(&file), &c()).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::Other, "{err:?}");
    assert!(
        err.summary().contains("Staged changes"),
        "{}",
        err.summary()
    );
    assert!(!am_in_progress(&repo));
    assert_eq!(head(&sb, &repo), before);

    // Unstaged changes to other files don't stand in the way.
    sb.run(&repo, &["reset", "-q"]);
    apply_mailbox(sb.git(), &repo, std::slice::from_ref(&file), &c()).unwrap();
    assert!(repo.join("b.txt").is_file());
    assert!(!am_in_progress(&repo));
}

#[test]
fn a_mailbox_without_a_base_stops_too_and_garbage_leaves_nothing() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let base = id(&sb.commit_file(&repo, "a.txt", "1\n2\n3\n", "base"));
    // No `index` line, and context that doesn't match: a 3-way merge has nothing to go on.
    let no_base = sb.path().join("no-base.patch");
    std::fs::write(
        &no_base,
        "From: X <x@example.invalid>\nSubject: [PATCH] no base\n\n---\n\
         diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n\
         @@ -1,3 +1,3 @@\n 1\n-9\n+Z\n 3\n",
    )
    .unwrap();
    let err = apply_mailbox(sb.git(), &repo, &[no_base], &c()).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::Conflict, "{err:?}");
    assert!(am_in_progress(&repo));
    sb.run(&repo, &["am", "--abort"]);

    let garbage = sb.path().join("garbage.patch");
    std::fs::write(&garbage, "not a patch\n").unwrap();
    let err = apply_mailbox(sb.git(), &repo, &[garbage], &c()).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::Other, "{err:?}");
    assert!(
        err.summary().contains("Patch format detection failed"),
        "{err:?}"
    );
    assert!(!am_in_progress(&repo));
    assert_eq!(head(&sb, &repo), base);
}

#[test]
fn apply_patch_applies_a_diff_or_says_why_not() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "1\n2\n3\n", "base");
    write(&repo, b"a.txt", "1\nX\n3\n");
    let diff = sb.run(&repo, &["diff"]);
    // In the working tree and named like an option: passed after `--`.
    std::fs::write(repo.join("-x.diff"), &diff).unwrap();
    sb.run(&repo, &["checkout", "--", "a.txt"]);
    let patch = Path::new("-x.diff");
    let status = || sb.run(&repo, &["status", "--porcelain", "--untracked-files=no"]);

    apply_patch(sb.git(), &repo, patch, &c()).unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "1\nX\n3\n"
    );
    // Applied cleanly with `--3way`: staged as well.
    assert_eq!(status(), "M  a.txt\n");
    sb.run(&repo, &["reset", "-q", "--hard"]);

    // Unstaged changes in a file the patch touches: git's reason, nothing changed.
    write(&repo, b"a.txt", "0\n1\n2\n3\n");
    let err = apply_patch(sb.git(), &repo, patch, &c()).unwrap_err();
    assert!(
        err.summary().contains("a.txt: does not match index"),
        "{err:?}"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("a.txt")).unwrap(),
        "0\n1\n2\n3\n"
    );
    sb.run(&repo, &["reset", "-q", "--hard"]);

    // A file that isn't there.
    let missing = sb.path().join("missing.diff");
    std::fs::write(
        &missing,
        "diff --git a/zz b/zz\n--- a/zz\n+++ b/zz\n@@ -1 +1 @@\n-x\n+y\n",
    )
    .unwrap();
    let err = apply_patch(sb.git(), &repo, &missing, &c()).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::Other);
    assert!(
        err.summary().contains("zz: does not exist in index"),
        "{err:?}"
    );

    let garbage = sb.path().join("garbage.diff");
    std::fs::write(&garbage, "garbage\n").unwrap();
    let err = apply_patch(sb.git(), &repo, &garbage, &c()).unwrap_err();
    assert!(
        err.summary().contains("No valid patches in input"),
        "{err:?}"
    );
    assert_eq!(status(), "");

    // The context changed, but the blobs are there: a 3-way merge with conflicts.
    sb.commit_file(&repo, "a.txt", "1\nY\n3\n", "y");
    let err = apply_patch(sb.git(), &repo, patch, &c()).unwrap_err();
    assert_eq!(err.kind, GitErrorKind::Conflict, "{err:?}");
    assert_eq!(status(), "UU a.txt\n");
    let text = std::fs::read_to_string(repo.join("a.txt")).unwrap();
    assert!(text.contains("<<<<<<<") && text.contains("X") && text.contains("Y"));
    // No operation in progress, but one unmerged file, which is counted.
    assert_eq!(unmerged_count(sb.git(), &repo, &c()).unwrap(), 1);
    sb.run(&repo, &["reset", "-q", "--hard"]);
    assert_eq!(unmerged_count(sb.git(), &repo, &c()).unwrap(), 0);
}

/// The file names in a zip's central directory.
fn zip_names(zip: &[u8]) -> Vec<Vec<u8>> {
    let u16_at = |at: usize| u16::from_le_bytes([zip[at], zip[at + 1]]) as usize;
    let u32_at = |at: usize| u32::from_le_bytes(zip[at..at + 4].try_into().unwrap()) as usize;
    let end = (0..=zip.len() - 22)
        .rev()
        .find(|&at| zip[at..at + 4] == [0x50, 0x4b, 0x05, 0x06])
        .expect("end of central directory");
    let (count, mut at) = (u16_at(end + 10), u32_at(end + 16));
    let mut names = Vec::new();
    for _ in 0..count {
        assert_eq!(
            zip[at..at + 4],
            [0x50, 0x4b, 0x01, 0x02],
            "central directory entry"
        );
        let (name, extra, comment) = (u16_at(at + 28), u16_at(at + 30), u16_at(at + 32));
        names.push(zip[at + 46..at + 46 + name].to_vec());
        at += 46 + name + extra + comment;
    }
    names
}

/// The entry names in a gzipped tar, leaving out git's pax headers.
fn tar_gz_names(tgz: &[u8]) -> Vec<Vec<u8>> {
    use std::io::Read;

    let mut tar = Vec::new();
    flate2::read::GzDecoder::new(tgz)
        .read_to_end(&mut tar)
        .unwrap();
    let field = |block: &[u8]| -> Vec<u8> {
        let end = block.iter().position(|&b| b == 0).unwrap_or(block.len());
        block[..end].to_vec()
    };
    let mut names = Vec::new();
    let mut at = 0;
    // Two zero blocks end the archive.
    while at + 512 <= tar.len() && tar[at..at + 512].iter().any(|&b| b != 0) {
        let header = &tar[at..at + 512];
        let size = std::str::from_utf8(&field(&header[124..136]))
            .ok()
            .and_then(|octal| usize::from_str_radix(octal.trim(), 8).ok())
            .expect("entry size");
        if !matches!(header[156], b'g' | b'x') {
            let (prefix, name) = (field(&header[345..500]), field(&header[..100]));
            names.push(if prefix.is_empty() {
                name
            } else {
                [&prefix[..], b"/", &name[..]].concat()
            });
        }
        at += 512 + size.div_ceil(512) * 512;
    }
    names
}

/// Files only (archives list folders too), sorted.
fn files_only(names: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut names: Vec<_> = names
        .into_iter()
        .filter(|n| !n.is_empty() && !n.ends_with(b"/"))
        .collect();
    names.sort();
    names
}

#[test]
fn archive_writes_zip_and_tar_gz_with_the_commit_files() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let mut names = vec![&b"a.txt"[..], b"dir/sub file.txt", b"-dash.txt"];
    // A name that isn't UTF-8, where the system can hold one (macOS file systems can't).
    if cfg!(all(unix, not(target_os = "macos"))) {
        names.push(b"caf\xe9.txt");
    }
    for name in &names {
        write(&repo, name, "x\n");
    }
    sb.run(&repo, &["add", "-A"]);
    sb.run(&repo, &["commit", "-q", "-m", "files"]);
    let commit = head(&sb, &repo);
    // Untracked files aren't archived.
    write(&repo, b"untracked.txt", "u\n");
    let tree = sb
        .git()
        .command(&repo)
        .args(["ls-tree", "-r", "--name-only", "-z", "HEAD"])
        .run(&c())
        .unwrap()
        .stdout;
    let expected = files_only(tree.split(|&b| b == 0).map(<[u8]>::to_vec).collect());
    assert_eq!(expected.len(), names.len());

    // Relative to the repository, and named like an option.
    archive(
        sb.git(),
        &repo,
        &commit,
        ArchiveFormat::Zip,
        Path::new("-out.zip"),
        &c(),
    )
    .unwrap();
    let zip = std::fs::read(repo.join("-out.zip")).unwrap();
    assert_eq!(files_only(zip_names(&zip)), expected);

    let tgz = sb.path().join("out.tar.gz");
    archive(sb.git(), &repo, &commit, ArchiveFormat::TarGz, &tgz, &c()).unwrap();
    let bytes = std::fs::read(&tgz).unwrap();
    assert_eq!(bytes[..2], [0x1f, 0x8b], "gzip magic");
    assert_eq!(files_only(tar_gz_names(&bytes)), expected);

    // A failure leaves no empty file behind.
    let missing = ObjectId::from_hex(b"1c272101c5c26442cf2d04b6fbc29030cf072307").unwrap();
    let failed = sb.path().join("failed.zip");
    let err = archive(sb.git(), &repo, &missing, ArchiveFormat::Zip, &failed, &c()).unwrap_err();
    assert!(err.summary().contains("not a tree object"), "{err:?}");
    assert!(!failed.exists());
}

/// Untracked, ignored and nested-repository files of every kind `git clean` treats
/// differently.
fn clean_fixture(sb: &Sandbox) -> PathBuf {
    let repo = sb.init_repo("r");
    write(&repo, b".gitignore", "*.log\n/build/\n");
    write(&repo, b"t/tracked.txt", "t\n");
    sb.run(&repo, &["add", "-A"]);
    sb.run(&repo, &["commit", "-q", "-m", "init"]);
    for name in [
        &b"new.txt"[..],
        b"sp ace.txt",
        b"t/untracked.txt",
        b"u/deep/f.txt",
        b"mixed/keep.txt",
        b"mixed/x.log",
        b"logs/a.log",
        b"build/out.bin",
        b"top.log",
        b"u2/f.txt",
        b"nested/file.txt",
        b"u2/n/file.txt",
    ] {
        write(&repo, name, "x\n");
    }
    std::fs::create_dir(repo.join("empty")).unwrap();
    sb.run(&repo.join("nested"), &["init", "-q"]);
    sb.run(&repo.join("u2/n"), &["init", "-q"]);
    repo
}

fn strings(paths: &[&str]) -> Vec<BString> {
    paths.iter().map(|p| BString::from(*p)).collect()
}

/// `git clean -n -d [-x]`'s own list, for names that need no quoting.
fn git_clean_list(sb: &Sandbox, repo: &Path, with_ignored: bool) -> Vec<BString> {
    let mut args = vec!["clean", "-n", "-d"];
    if with_ignored {
        args.push("-x");
    }
    sb.run(repo, &args)
        .lines()
        .filter_map(|l| l.strip_prefix("Would remove "))
        .map(BString::from)
        .collect()
}

#[test]
fn clean_candidates_are_what_git_clean_would_remove() {
    let sb = Sandbox::new();
    let repo = clean_fixture(&sb);

    let untracked = clean_candidates(sb.git(), &repo, false, &c()).unwrap();
    // `mixed/` holds an ignored file, so only its untracked file goes; `logs/` holds only
    // ignored files; `nested/` and `u2/n` are repositories.
    assert_eq!(
        untracked,
        strings(&[
            "empty/",
            "mixed/keep.txt",
            "new.txt",
            "sp ace.txt",
            "t/untracked.txt",
            "u/",
            "u2/f.txt",
        ])
    );
    assert_eq!(untracked, git_clean_list(&sb, &repo, false));

    let all = clean_candidates(sb.git(), &repo, true, &c()).unwrap();
    assert_eq!(
        all,
        strings(&[
            "build/",
            "empty/",
            "logs/",
            "mixed/",
            "new.txt",
            "sp ace.txt",
            "t/untracked.txt",
            "top.log",
            "u/",
            "u2/f.txt",
        ])
    );
    assert_eq!(all, git_clean_list(&sb, &repo, true));
}

#[cfg(unix)]
#[test]
fn clean_candidates_keep_special_names_byte_exact() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "tracked.txt", "t\n", "init");
    let names: &[&[u8]] = &[
        b"-dash",
        b"back\\slash",
        #[cfg(all(unix, not(target_os = "macos")))]
        b"caf\xe9",
        b"nl\nx",
        b"tab\tx",
        b"we\"ird",
        b"\"leading quote",
    ];
    for name in names {
        write(&repo, name, "x\n");
    }
    let mut got = clean_candidates(sb.git(), &repo, false, &c()).unwrap();
    got.sort();
    let mut expected: Vec<BString> = names.iter().map(|n| BString::from(*n)).collect();
    expected.sort();
    assert_eq!(got, expected);
}

#[test]
fn discard_paths_moves_every_clean_candidate_to_the_trash() {
    let sb = Sandbox::new();
    let repo = clean_fixture(&sb);
    if cfg!(all(unix, not(target_os = "macos"))) {
        write(&repo, b"caf\xe9 \"q\".txt", "x\n");
    }
    let candidates = clean_candidates(sb.git(), &repo, true, &c()).unwrap();
    // Folders (with their trailing `/`), an empty one, ignored files and folders.
    for kind in ["u/", "empty/", "top.log", "build/", "logs/"] {
        assert!(candidates.contains(&BString::from(kind)), "{kind}");
    }

    let trash = sb.path().join("trash");
    let backup = discard_paths(sb.git(), &repo, &[], &candidates, &trash, &c()).unwrap();
    let dir = backup.trash_dir.unwrap().join("files");
    for candidate in &candidates {
        assert!(
            !path(&repo, candidate).exists(),
            "{candidate:?} is still there"
        );
        let moved = path(&dir, candidate);
        assert!(moved.exists(), "{candidate:?} isn't in the trash");
        assert_eq!(moved.is_dir(), candidate.ends_with(b"/"), "{candidate:?}");
    }
    assert!(path(&dir, b"u/deep/f.txt").is_file());
    assert!(path(&dir, b"mixed/x.log").is_file());
    assert!(
        clean_candidates(sb.git(), &repo, true, &c())
            .unwrap()
            .is_empty()
    );
    // Tracked files and nested repositories stay.
    assert!(repo.join("t/tracked.txt").is_file());
    assert!(repo.join("nested/.git").is_dir());
    assert!(repo.join("u2/n/file.txt").is_file());
}

/// `git check-ignore --no-index -v`: the file and line of the rule that ignores `name`.
fn ignored_by(sb: &Sandbox, repo: &Path, name: &[u8]) -> Option<(BString, usize)> {
    let mut stdin = name.to_vec();
    stdin.push(0);
    let output = sb
        .git()
        .command(repo)
        // (check-ignore matches the name as given; it refuses GIT_LITERAL_PATHSPECS.)
        .args(["check-ignore", "--no-index", "-v", "-z", "--stdin"])
        .stdin(stdin)
        .allow_exit_codes(&[1])
        .run(&c())
        .unwrap();
    let fields: Vec<&[u8]> = output.stdout.split(|&b| b == 0).collect();
    match fields.as_slice() {
        [source, line, _pattern, path, ..] if !source.is_empty() => {
            assert_eq!(*path, name);
            let line = std::str::from_utf8(line).unwrap().parse().unwrap();
            Some((BString::from(*source), line))
        }
        _ => None,
    }
}

#[cfg(unix)]
#[test]
fn path_patterns_ignore_exactly_that_path() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    // (target, a similar name that must stay visible)
    let cases: &[(&[u8], &[u8])] = &[
        (b"a*b.txt", b"aXb.txt"),
        (b"q?.txt", b"qa.txt"),
        (b"[ab].txt", b"a.txt"),
        (b"#notes", b"notes"),
        (b"!bang", b"bang"),
        (b"trailing space ", b"trailing space"),
        (b"back\\slash", b"backslash"),
        (b"dir/sub file.txt", b"other/dir/sub file.txt"),
        (b"x/n.txt", b"y/x/n.txt"),
        #[cfg(all(unix, not(target_os = "macos")))]
        (b"caf\xe9.txt", b"cafe.txt"),
        (b"cr\r", b"cr"),
    ];
    for (target, sibling) in cases {
        write(&repo, target, "x\n");
        write(&repo, sibling, "x\n");
    }
    for (target, _) in cases {
        let pattern = ignore_pattern_for_path(target, false);
        add_ignore_rule(&repo, &repo.join(".git"), IgnoreScope::Shared, &pattern).unwrap();
    }
    for (line, (target, sibling)) in cases.iter().enumerate() {
        assert_eq!(
            ignored_by(&sb, &repo, target),
            Some((BString::from(".gitignore"), line + 1)),
            "{:?}",
            BString::from(*target)
        );
        assert_eq!(
            ignored_by(&sb, &repo, sibling),
            None,
            "{:?}",
            BString::from(*sibling)
        );
    }
}

#[test]
fn folder_and_extension_patterns() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    for name in [
        &b"out dir/f"[..],
        b"out dirx/f",
        b"out dir2",
        b"a/b.gz",
        b"c.gzip",
    ] {
        write(&repo, name, "x\n");
    }
    let folder = ignore_pattern_for_path(b"out dir", true);
    assert_eq!(folder, "/out dir/");
    assert_eq!(ignore_pattern_for_path(b"out dir/", false), folder);
    add_ignore_rule(&repo, &repo.join(".git"), IgnoreScope::Shared, &folder).unwrap();
    let extension = ignore_pattern_for_extension(b"x/report.tar.gz").unwrap();
    assert_eq!(extension, "*.gz");
    add_ignore_rule(&repo, &repo.join(".git"), IgnoreScope::Shared, &extension).unwrap();

    let shared = |line| Some((BString::from(".gitignore"), line));
    assert_eq!(ignored_by(&sb, &repo, b"out dir"), shared(1));
    assert_eq!(ignored_by(&sb, &repo, b"out dirx"), None);
    assert_eq!(ignored_by(&sb, &repo, b"out dir2"), None);
    assert_eq!(ignored_by(&sb, &repo, b"a/b.gz"), shared(2));
    assert_eq!(ignored_by(&sb, &repo, b"c.gzip"), None);
    // Git agrees: the folder goes as a whole.
    let ignored = clean_candidates(sb.git(), &repo, true, &c()).unwrap();
    let untracked = clean_candidates(sb.git(), &repo, false, &c()).unwrap();
    assert!(ignored.contains(&BString::from("out dir/")));
    assert!(!untracked.contains(&BString::from("out dir/")));
}

#[test]
fn local_rules_go_to_info_exclude() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let git_dir = repo.join(".git");
    let exclude = git_dir.join("info/exclude");
    write(&repo, b"local.txt", "x\n");
    let before = std::fs::read(&exclude).unwrap();
    assert!(before.ends_with(b"\n"));

    add_ignore_rule(&repo, &git_dir, IgnoreScope::Local, b"/local.txt").unwrap();
    add_ignore_rule(&repo, &git_dir, IgnoreScope::Local, b"/local.txt").unwrap();
    let mut expected = before.clone();
    expected.extend_from_slice(b"/local.txt\n");
    assert_eq!(std::fs::read(&exclude).unwrap(), expected);
    let lines = before.iter().filter(|&&b| b == b'\n').count();
    assert_eq!(
        ignored_by(&sb, &repo, b"local.txt"),
        Some((BString::from(".git/info/exclude"), lines + 1))
    );
    assert!(!repo.join(".gitignore").exists());

    // `info/` is created when missing.
    std::fs::remove_dir_all(git_dir.join("info")).unwrap();
    add_ignore_rule(&repo, &git_dir, IgnoreScope::Local, b"/x").unwrap();
    assert_eq!(std::fs::read(&exclude).unwrap(), b"/x\n");
}

#[test]
fn rules_end_the_last_line_first_and_are_never_duplicated() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let git_dir = repo.join(".git");
    let gitignore = repo.join(".gitignore");
    let add = |pattern: &[u8]| add_ignore_rule(&repo, &git_dir, IgnoreScope::Shared, pattern);
    let read = || std::fs::read(&gitignore).unwrap();

    // Created when missing.
    add(b"/a.txt").unwrap();
    assert_eq!(read(), b"/a.txt\n");
    // A last line without a newline gets one first.
    std::fs::write(&gitignore, b"*.log").unwrap();
    add(b"/a.txt").unwrap();
    assert_eq!(read(), b"*.log\n/a.txt\n");
    add(b"*.log").unwrap();
    add(b"/a.txt").unwrap();
    assert_eq!(read(), b"*.log\n/a.txt\n");
    // CRLF lines count as the same line.
    std::fs::write(&gitignore, b"/a.txt\r\n/b\r\n").unwrap();
    add(b"/b").unwrap();
    assert_eq!(read(), b"/a.txt\r\n/b\r\n");
    // A longer line that contains the pattern isn't the same line.
    add(b"/a").unwrap();
    assert_eq!(read(), b"/a.txt\r\n/b\r\n/a\n");
    // Empty file: no leading newline.
    std::fs::write(&gitignore, b"").unwrap();
    add(b"/c").unwrap();
    assert_eq!(read(), b"/c\n");

    for bad in [&b""[..], b"/a\n/b"] {
        let err = add(bad).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }
    assert_eq!(read(), b"/c\n");
}
