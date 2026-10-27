//! `log.showSignature=true` in the user's config makes `git log` and `git show` print a
//! signature check on stdout before each signed commit. Gitweft parses their output, so
//! the runner turns the setting off for every command; a read that wants the
//! check asks for it (`--show-signature`, `%G?`).

use std::path::{Path, PathBuf};

use gitweft_git::CancelToken;
use gitweft_git::ops::head_message;
use gitweft_git::rebase::plan;
use gitweft_testkit::Sandbox;

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

/// A repository whose commits are all SSH-signed by a key git trusts, with
/// `log.showSignature` on.
fn signing_repo(sb: &Sandbox) -> PathBuf {
    let repo = sb.init_repo("r");
    let key = ssh_key(sb.path(), "key");
    let public = format!("{}.pub", key.display());
    let allowed = sb.path().join("allowed_signers");
    let line = format!(
        "author@example.invalid {}",
        std::fs::read_to_string(&public).unwrap()
    );
    std::fs::write(&allowed, line).unwrap();
    for (key, value) in [
        ("gpg.format", "ssh"),
        ("user.signingkey", public.as_str()),
        ("gpg.ssh.allowedSignersFile", allowed.to_str().unwrap()),
        ("commit.gpgSign", "true"),
        ("log.showSignature", "true"),
    ] {
        sb.run(&repo, &["config", key, value]);
    }
    repo
}

/// The commits are signed and git trusts them, so with the setting on `git log` would
/// print "Good "git" signature …" before each.
fn assert_signed(sb: &Sandbox, repo: &Path) {
    let shown = sb.run(repo, &["log", "-1", "--show-signature", "--format=%B"]);
    assert!(shown.contains("Good"), "{shown}");
}

#[test]
fn a_rebase_plan_keeps_every_signed_commit() {
    let sb = Sandbox::new();
    let repo = signing_repo(&sb);
    let base = sb.commit_file(&repo, "f", "1\n", "base");
    let second = sb.commit_file(&repo, "f", "2\n", "second");
    let third = sb.commit_file(&repo, "f", "3\n", "third\n\nwith a body");
    assert_signed(&sb, &repo);

    let plan = plan(sb.git(), &repo, &base, &CancelToken::new()).unwrap();
    let ids: Vec<String> = plan.items.iter().map(|i| i.commit.to_string()).collect();
    assert_eq!(ids, [second, third]);
    assert_eq!(plan.items[0].message, "second");
    assert_eq!(plan.items[1].message, "third\n\nwith a body");
}

#[test]
fn an_amends_message_is_only_the_commits() {
    let sb = Sandbox::new();
    let repo = signing_repo(&sb);
    sb.commit_file(&repo, "f", "1\n", "signed and shown\n\nthe body");
    assert_signed(&sb, &repo);

    let message = head_message(sb.git(), &repo, &CancelToken::new()).unwrap();
    assert_eq!(message, "signed and shown\n\nthe body");
}
