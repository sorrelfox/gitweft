use std::path::{Path, PathBuf};

use gitweft_git::CancelToken;
use gitweft_git::read::{ChangeKind, LineCount, SignatureStatus, commit_details, open_repo};
use gitweft_testkit::Sandbox;

fn details(sb: &Sandbox, repo: &std::path::Path, rev: &str) -> gitweft_git::read::CommitDetails {
    let id = sb.run(repo, &["rev-parse", rev]).trim().parse().unwrap();
    let git_dir = open_repo(repo).unwrap().git_dir().to_owned();
    commit_details(sb.git(), &git_dir, id, &CancelToken::new()).unwrap()
}

#[cfg(unix)]
#[test]
fn a_path_that_isnt_utf8_keeps_its_bytes_and_its_count() {
    use std::os::unix::ffi::OsStrExt as _;
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "a\n", "base");
    let name: &[u8] = b"caf\xe9.txt";
    std::fs::write(
        repo.join(std::ffi::OsStr::from_bytes(name)),
        "hello\ncafe\n",
    )
    .unwrap();
    sb.run(&repo, &["add", "."]);
    sb.run(&repo, &["commit", "-q", "-m", "Add a Latin-1 name"]);

    // As git has it; before, the text "caf\u{fffd}.txt" named no file, and its
    // line count, matched by path, was lost.
    let d = details(&sb, &repo, "HEAD");
    assert_eq!(d.changes.len(), 1);
    assert_eq!(d.changes[0].path.as_slice(), name);
    assert_eq!(d.changes[0].kind, ChangeKind::Added);
    assert_eq!(
        d.changes[0].lines,
        Some(LineCount {
            added: 2,
            removed: 0
        })
    );
}

#[test]
fn root_commit_lists_all_files() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    std::fs::write(repo.join("b.txt"), "b\n").unwrap();
    sb.run(&repo, &["add", "."]);
    sb.run(
        &repo,
        &["commit", "-q", "-m", "Initial import\n\nWith a body line."],
    );

    let d = details(&sb, &repo, "HEAD");
    assert!(d.parents.is_empty());
    assert_eq!(d.message, "Initial import\n\nWith a body line.");
    assert_eq!(d.author.name, "Test Author");
    assert_eq!(d.author.email, "author@example.invalid");
    assert_eq!(d.author.time, 1_767_225_600); // FIXED_DATE
    let paths: Vec<_> = d
        .changes
        .iter()
        .map(|c| (c.kind, std::str::from_utf8(&c.path).unwrap()))
        .collect();
    assert_eq!(
        paths,
        [(ChangeKind::Added, "a.txt"), (ChangeKind::Added, "b.txt")]
    );
}

#[test]
fn modify_delete_and_rename() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let body = "line\n".repeat(20);
    std::fs::write(repo.join("keep.txt"), "1\n").unwrap();
    std::fs::write(repo.join("gone.txt"), "x\n").unwrap();
    std::fs::write(repo.join("old name.txt"), &body).unwrap();
    sb.run(&repo, &["add", "."]);
    sb.run(&repo, &["commit", "-q", "-m", "base"]);

    std::fs::write(repo.join("keep.txt"), "2\n").unwrap();
    sb.run(&repo, &["rm", "-q", "gone.txt"]);
    sb.run(&repo, &["mv", "old name.txt", "new name.txt"]);
    sb.run(&repo, &["commit", "-qam", "change"]);

    let d = details(&sb, &repo, "HEAD");
    assert_eq!(d.parents.len(), 1);
    let find = |path: &str| d.changes.iter().find(|c| c.path == path).unwrap();
    assert_eq!(find("keep.txt").kind, ChangeKind::Modified);
    assert_eq!(find("gone.txt").kind, ChangeKind::Deleted);
    let renamed = find("new name.txt");
    assert_eq!(renamed.kind, ChangeKind::Renamed);
    assert_eq!(
        renamed
            .old_path
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("old name.txt")
    );
}

#[test]
fn line_counts_per_file() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    std::fs::write(repo.join("keep.txt"), "1\n2\n3\n").unwrap();
    std::fs::write(repo.join("gone.txt"), "x\ny\n").unwrap();
    std::fs::write(repo.join("old name.txt"), "line\n".repeat(20)).unwrap();
    sb.run(&repo, &["add", "."]);
    sb.run(&repo, &["commit", "-q", "-m", "base"]);
    let root = details(&sb, &repo, "HEAD");
    let counted = |d: &gitweft_git::read::CommitDetails, path: &str| {
        d.changes.iter().find(|c| c.path == path).unwrap().lines
    };
    let count = |added, removed| Some(LineCount { added, removed });
    assert_eq!(counted(&root, "keep.txt"), count(3, 0));

    std::fs::write(repo.join("keep.txt"), "1\nzwei\n3\n4\n").unwrap();
    sb.run(&repo, &["rm", "-q", "gone.txt"]);
    sb.run(&repo, &["mv", "old name.txt", "new name.txt"]);
    std::fs::write(repo.join("image.bin"), [0u8, 1, 2, 0, 255]).unwrap();
    sb.run(&repo, &["add", "."]);
    sb.run(&repo, &["commit", "-q", "-m", "change"]);

    let d = details(&sb, &repo, "HEAD");
    assert_eq!(counted(&d, "keep.txt"), count(2, 1));
    assert_eq!(counted(&d, "gone.txt"), count(0, 2));
    // A pure rename is found under its new name.
    assert_eq!(counted(&d, "new name.txt"), count(0, 0));
    // Binary: no line count.
    assert_eq!(counted(&d, "image.bin"), None);
    assert_eq!(d.line_totals(), count(2, 3));
}

#[test]
fn merge_commit_is_compared_with_first_parent() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a", "1\n", "base");
    sb.run(&repo, &["checkout", "-q", "-b", "topic"]);
    sb.commit_file(&repo, "b", "topic\n", "topic");
    sb.run(&repo, &["checkout", "-q", "main"]);
    sb.commit_file(&repo, "c", "main\n", "main");
    sb.run(
        &repo,
        &["merge", "-q", "--no-ff", "-m", "merge topic", "topic"],
    );

    let d = details(&sb, &repo, "HEAD");
    assert_eq!(d.parents.len(), 2);
    // Relative to main (first parent), the merge brought in `b`.
    let paths: Vec<_> = d
        .changes
        .iter()
        .map(|c| std::str::from_utf8(&c.path).unwrap())
        .collect();
    assert_eq!(paths, ["b"]);
    assert_eq!(d.signature, None);
}

/// A passphrase-less SSH key for signing; returns the private key's path.
#[allow(clippy::disallowed_methods)] // ssh-keygen, only to make a test key
fn ssh_key(dir: &Path, name: &str) -> PathBuf {
    let key = dir.join(name);
    let status = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"])
        .arg(&key)
        .status()
        .expect("ssh-keygen must be installed to run this test");
    assert!(status.success());
    key
}

#[test]
fn ssh_signatures_are_checked_against_the_allowed_signers() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let key = ssh_key(sb.path(), "key");
    let public = format!("{}.pub", key.display());
    sb.run(&repo, &["config", "gpg.format", "ssh"]);
    sb.run(&repo, &["config", "user.signingkey", &public]);
    std::fs::write(repo.join("f"), "1\n").unwrap();
    sb.run(&repo, &["add", "f"]);
    sb.run(&repo, &["commit", "-q", "-S", "-m", "signed"]);
    sb.commit_file(&repo, "f", "2\n", "unsigned");

    assert_eq!(details(&sb, &repo, "HEAD").signature, None);
    // Without allowed signers git can't check an SSH signature, but it is still signed.
    let unchecked = details(&sb, &repo, "HEAD~1").signature.expect("signed");
    assert_eq!(unchecked.status, SignatureStatus::CannotCheck);

    let allowed = sb.path().join("allowed_signers");
    let set_allowed = |public_key: &str| {
        let line = format!(
            "author@example.invalid {}",
            std::fs::read_to_string(public_key).unwrap()
        );
        std::fs::write(&allowed, line).unwrap();
    };
    set_allowed(&public);
    sb.run(
        &repo,
        &[
            "config",
            "gpg.ssh.allowedSignersFile",
            allowed.to_str().unwrap(),
        ],
    );
    let good = details(&sb, &repo, "HEAD~1").signature.unwrap();
    assert_eq!(good.status, SignatureStatus::Good);
    assert_eq!(good.signer, "author@example.invalid");
    assert!(good.key.starts_with("SHA256:"), "{}", good.key);

    // Only another key is allowed: a good signature from an unknown key.
    let other = ssh_key(sb.path(), "other");
    set_allowed(&format!("{}.pub", other.display()));
    let untrusted = details(&sb, &repo, "HEAD~1").signature.unwrap();
    assert_eq!(untrusted.status, SignatureStatus::Untrusted);
}

#[test]
fn signature_marks_verify_only_the_signed_commits() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    let key = ssh_key(sb.path(), "key");
    let public = format!("{}.pub", key.display());
    sb.run(&repo, &["config", "gpg.format", "ssh"]);
    sb.run(&repo, &["config", "user.signingkey", &public]);
    std::fs::write(repo.join("f"), "1\n").unwrap();
    sb.run(&repo, &["add", "f"]);
    sb.run(&repo, &["commit", "-q", "-S", "-m", "signed"]);
    sb.commit_file(&repo, "f", "2\n", "unsigned");
    let allowed = sb.path().join("allowed_signers");
    let line = format!(
        "author@example.invalid {}",
        std::fs::read_to_string(&public).unwrap()
    );
    std::fs::write(&allowed, line).unwrap();
    let allowed = allowed.to_str().unwrap();
    sb.run(&repo, &["config", "gpg.ssh.allowedSignersFile", allowed]);
    let id = |rev: &str| {
        let hex = sb.run(&repo, &["rev-parse", rev]);
        gitweft_git::ObjectId::from_hex(hex.trim().as_bytes()).unwrap()
    };
    let (signed, unsigned) = (id("HEAD~1"), id("HEAD"));
    let gix_repo = gitweft_git::gix::open(&repo).unwrap();

    let marks = gitweft_git::read::signature_marks(
        sb.git(),
        &repo.join(".git"),
        &gix_repo,
        &[unsigned, signed],
        &gitweft_git::CancelToken::new(),
    )
    .unwrap();
    assert_eq!(marks.len(), 2);
    assert!(marks.contains(&(unsigned, None)), "{marks:?}");
    assert!(
        marks.contains(&(signed, Some(SignatureStatus::Good))),
        "{marks:?}"
    );
}
